//! Operations across file systems: copy/move by streaming, delete, trash,
//! mkdir, measure and materialize. Port of the "Operations across file
//! systems" half of `vfs.py`.
//!
//! These only use the generic [`FileSystem`](crate::FileSystem) interface, so they work between
//! any two backends. When both sides are the local disk the app should use
//! `oc-local`'s native routines instead (symlinks, permissions, copy-on-write
//! clones); this is the fallback that always works.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{CHUNK, Entry, RelPath, Result, Transfer, TransferCallbacks, VPath, VfsError};

/// One source's planned walk: every item below it, collected up front so
/// totals are known before the first byte moves.
pub type WalkItems = Vec<(RelPath, Entry)>;

// ---------------------------------------------------------------------------
// Copy / move
// ---------------------------------------------------------------------------

/// Map each source to its destination: into `target` if it is a directory,
/// otherwise a single source is written under that name.
pub fn resolve_target(sources: &[VPath], target: &VPath) -> Result<Vec<(VPath, VPath)>> {
    if target.is_dir() {
        return Ok(sources.iter().map(|s| (s.clone(), target.child(&[&s.name()]))).collect());
    }
    if let [only] = sources {
        return Ok(vec![(only.clone(), target.clone())]);
    }
    Err(VfsError::Operation(format!("Target directory does not exist: {}", target.display())))
}

/// Copy `sources` to `target` across any file systems.
pub fn copy(sources: &[VPath], target: &VPath, callbacks: TransferCallbacks<'_>) -> Result<()> {
    copy_or_move(sources, target, false, callbacks)
}

/// Move `sources` to `target`: a rename where the file system can, else
/// copy + delete. Sources with skipped items are kept (only what was copied
/// is removed), so a "skip" never loses data.
pub fn move_to(sources: &[VPath], target: &VPath, callbacks: TransferCallbacks<'_>) -> Result<()> {
    copy_or_move(sources, target, true, callbacks)
}

/// The shared implementation of [`copy`] and [`move_to`].
pub fn copy_or_move(sources: &[VPath], target: &VPath, mv: bool, callbacks: TransferCallbacks<'_>) -> Result<()> {
    if !target.fs.capabilities().writable {
        return Err(VfsError::ReadOnly(target.fs.read_only_reason()));
    }
    if mv && let Some(s) = sources.iter().find(|s| !s.fs.capabilities().writable) {
        return Err(VfsError::ReadOnly(s.fs.read_only_reason()));
    }
    let mut pairs = resolve_target(sources, target)?;
    for (src, dst) in &pairs {
        check_not_into_self(src, dst, mv)?;
    }
    if mv {
        // Same file system: a server-side rename is instant. Only possible when
        // nothing is in the way; otherwise fall through to copy + delete.
        pairs.retain(|(src, dst)| {
            !(src.same_fs(dst)
                && src.fs.capabilities().can_rename
                && !dst.exists()
                // Some servers refuse renames; copy + delete instead.
                && src.fs.rename(&src.path, &dst.path).is_ok())
        });
    }
    let mut plans = Vec::with_capacity(pairs.len());
    let (mut files, mut bytes) = (0u64, 0u64);
    for (src, dst) in pairs {
        let items: WalkItems = src.fs.walk(&src.path)?.collect::<Result<_>>()?;
        for (_, e) in items.iter().filter(|(_, e)| !e.is_dir) {
            files += 1;
            bytes += e.size;
        }
        plans.push((src, dst, items));
    }
    let mut runner = Transfer::new(callbacks, files, bytes);
    let mut buf = vec![0u8; CHUNK];
    for (src, dst, items) in &plans {
        let skipped = stream_tree_with(&mut runner, src, dst, items, &mut buf)?;
        if mv {
            remove_moved(src, items, &skipped)?;
        }
    }
    Ok(())
}

/// Refuse to copy a directory into itself (a move would then delete the
/// copy along with the source) and to overwrite a file with itself (the
/// conflict handler would delete the only copy first). Paths are compared
/// within one file system; when both sides are real files, the files
/// themselves are compared too, so two handles on the same disk, symlinked
/// folders or a case-only rename on a case-insensitive disk are caught.
fn check_not_into_self(src: &VPath, dst: &VPath, mv: bool) -> Result<()> {
    let into_self = || VfsError::Operation(format!("Cannot copy or move '{}' into itself", src.name()));
    let same = |exact: bool| {
        let what = if mv && exact { "Source and destination are the same" } else { "Source and destination are the same file" };
        VfsError::Operation(format!("{what}: {}", dst.display()))
    };
    let src_is_dir = src.is_dir();
    if src.same_fs(dst) {
        if src_is_dir {
            let mut p = Some(dst.path.clone());
            while let Some(path) = p {
                if path == src.path {
                    return Err(into_self());
                }
                p = dst.fs.parent_path(&path);
            }
        } else if src.path == dst.path {
            return Err(same(true));
        }
    }
    if let (Some(s), Some(d)) = (src.local_path(), dst.local_path()) {
        if src_is_dir {
            // A symlinked folder is copied as the link (Python skips it too).
            let is_link = std::fs::symlink_metadata(&s).is_ok_and(|m| m.file_type().is_symlink());
            if !is_link
                && let (Ok(s), Some(d)) = (std::fs::canonicalize(&s), canonical_dest(&d))
                && d.starts_with(&s)
            {
                return Err(into_self());
            }
        } else if same_local_file(&s, &d) {
            return Err(same(s == d));
        }
    }
    Ok(())
}

/// `dst` with symlinks resolved; it may not exist yet, so fall back to its
/// resolved parent plus its name.
fn canonical_dest(dst: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(dst)
        .ok()
        .or_else(|| Some(std::fs::canonicalize(dst.parent()?).ok()?.join(dst.file_name()?)))
}

/// `os.path.samefile`: both exist and are the same file on disk.
fn same_local_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        matches!((std::fs::canonicalize(a), std::fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
    }
}

/// After a move's copy: drop the source. If the user skipped something,
/// remove only what was copied and the directories that became empty.
fn remove_moved(src: &VPath, items: &WalkItems, skipped: &[RelPath]) -> Result<()> {
    if skipped.is_empty() {
        return src.fs.remove(&src.path);
    }
    let is_skipped = |rel: &RelPath| skipped.iter().any(|d| rel.starts_with(d));
    for (rel, _) in items.iter().filter(|(rel, e)| !e.is_dir && !is_skipped(rel)) {
        src.fs.remove(&child(src, rel).path)?;
    }
    let all = crate::ListOptions { show_hidden: true, ..Default::default() };
    for (rel, _) in items.iter().rev().filter(|(rel, e)| e.is_dir && !is_skipped(rel)) {
        let dir = child(src, rel);
        if dir.list_dir(all).is_ok_and(|v| v.is_empty()) {
            src.fs.remove(&dir.path)?;
        }
    }
    Ok(())
}

fn child(root: &VPath, rel: &RelPath) -> VPath {
    if rel.is_empty() {
        return root.clone();
    }
    let names: Vec<&str> = rel.iter().map(String::as_str).collect();
    root.child(&names)
}

/// Stream one walked tree (`items` from `src_root.fs.walk`) to `dst_root`.
/// Directories merge; anything else in the way is asked about. Returns the
/// relative paths the user chose to skip (their contents are skipped too).
pub fn stream_tree(runner: &mut Transfer<'_>, src_root: &VPath, dst_root: &VPath, items: &WalkItems) -> Result<Vec<RelPath>> {
    stream_tree_with(runner, src_root, dst_root, items, &mut vec![0u8; CHUNK])
}

fn stream_tree_with(
    runner: &mut Transfer<'_>,
    src_root: &VPath,
    dst_root: &VPath,
    items: &WalkItems,
    buf: &mut [u8],
) -> Result<Vec<RelPath>> {
    let mut skipped: Vec<RelPath> = Vec::new();
    let skip = |runner: &mut Transfer<'_>, entry: &Entry| {
        if !entry.is_dir {
            runner.progress.files_done += 1;
            runner.progress.bytes_done += entry.size;
            // Report it, or a skip at the very end leaves the bar short.
            runner.report(None);
        }
    };
    for (rel, entry) in items {
        if skipped.iter().any(|d| rel.starts_with(d)) {
            skip(runner, entry);
            continue;
        }
        let (src, dst) = (child(src_root, rel), child(dst_root, rel));
        runner.check_cancel()?;
        runner.report(Some(&src.display()));
        let in_the_way = dst.stat().ok().is_some_and(|existing| !(entry.is_dir && existing.is_dir));
        if in_the_way && !confirm_overwrite(runner, &src, &dst)? {
            skipped.push(rel.clone());
            skip(runner, entry);
            continue;
        }
        if entry.is_dir {
            dst.fs.mkdir(&dst.path, true)?;
            continue;
        }
        stream_file_with(runner, &src, &dst, buf)?;
        if entry.mtime != 0.0 {
            dst.fs.set_mtime(&dst.path, entry.mtime)?;
        }
        runner.progress.files_done += 1;
        runner.report(None);
    }
    Ok(skipped)
}

/// Ask whether `dst` may be replaced; if so, remove it.
fn confirm_overwrite(runner: &mut Transfer<'_>, src: &VPath, dst: &VPath) -> Result<bool> {
    let dst_shown = match dst.local_path() {
        Some(p) => p.display().to_string(),
        None => dst.display(),
    };
    if !runner.ask_conflict(&src.display(), &dst_shown)? {
        return Ok(false);
    }
    dst.fs.remove(&dst.path)?;
    Ok(true)
}

/// Copy one file's bytes, reporting progress per chunk and checking for
/// cancellation. On any failure the partial destination is removed.
pub fn stream_file(runner: &mut Transfer<'_>, src: &VPath, dst: &VPath) -> Result<()> {
    stream_file_with(runner, src, dst, &mut vec![0u8; CHUNK])
}

fn stream_file_with(runner: &mut Transfer<'_>, src: &VPath, dst: &VPath, buf: &mut [u8]) -> Result<()> {
    // Opened first: if the source can't be read, nothing was written, so
    // there's nothing at `dst` of ours to clean up.
    let mut reader = src.fs.open_read(&src.path)?;
    let result = (|| {
        let mut writer = dst.fs.open_write(&dst.path)?;
        loop {
            let n = match reader.read(buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            writer.write_all(&buf[..n])?;
            runner.progress.bytes_done += n as u64;
            runner.report(None);
            runner.check_cancel()?;
        }
        writer.finish()
    })();
    if result.is_err() && dst.exists() {
        // Don't leave a partial file behind; the original error matters more
        // than a failed cleanup.
        let _ = dst.fs.remove(&dst.path);
    }
    result
}

// ---------------------------------------------------------------------------
// Delete / trash / mkdir
// ---------------------------------------------------------------------------

/// Delete permanently (for file systems without a trash, e.g. servers).
/// Every path is checked for writability before anything is deleted.
pub fn delete(paths: &[VPath]) -> Result<()> {
    if let Some(p) = paths.iter().find(|p| !p.fs.capabilities().writable) {
        return Err(VfsError::ReadOnly(p.fs.read_only_reason()));
    }
    for p in paths {
        p.fs.remove(&p.path)?;
    }
    Ok(())
}

/// Move to the recycle bin, one `trash` call per file system (in order of
/// first appearance) so backends can batch.
pub fn trash(paths: &[VPath]) -> Result<()> {
    let mut by_fs: Vec<(&VPath, Vec<String>)> = Vec::new();
    for p in paths {
        if !p.fs.capabilities().can_trash {
            return Err(VfsError::ReadOnly(p.fs.read_only_reason()));
        }
        match by_fs.iter_mut().find(|(first, _)| first.same_fs(p)) {
            Some((_, items)) => items.push(p.path.clone()),
            None => by_fs.push((p, vec![p.path.clone()])),
        }
    }
    for (first, items) in by_fs {
        first.fs.trash(&items)?;
    }
    Ok(())
}

/// Create directory `name` in `parent`; errors if it already exists.
pub fn mkdir(parent: &VPath, name: &str) -> Result<VPath> {
    let path = parent.child(&[name]);
    parent.fs.mkdir(&path.path, false)?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// Measure
// ---------------------------------------------------------------------------

/// (file count, total bytes) for everything below `paths`. Unreadable parts
/// are skipped: this feeds progress totals and size displays, which should
/// not fail over one bad folder.
pub fn measure(paths: &[VPath]) -> (u64, u64) {
    measure_cancellable(paths, &|| false).unwrap_or_default()
}

/// [`measure`] that stops with `Cancelled` when `is_cancelled` says so
/// (big remote trees take a while).
pub fn measure_cancellable(paths: &[VPath], is_cancelled: &dyn Fn() -> bool) -> Result<(u64, u64)> {
    let (mut files, mut bytes) = (0u64, 0u64);
    for p in paths {
        let Ok(walk) = p.fs.walk(&p.path) else { continue };
        for (_, entry) in walk.flatten() {
            if is_cancelled() {
                return Err(VfsError::Cancelled);
            }
            if !entry.is_dir {
                files += 1;
                bytes += entry.size;
            }
        }
    }
    Ok((files, bytes))
}

// ---------------------------------------------------------------------------
// Materialize
// ---------------------------------------------------------------------------

/// A real file with this item's contents: its local path if it has one,
/// else whatever the file system makes itself ([`FileSystem::materialize`](crate::FileSystem::materialize),
/// e.g. an archive's extraction cache), otherwise a copy cached under `cache_root/vfs/<key>/<name>`. The key
/// covers display path, size and mtime, so a changed file gets a fresh copy
/// while repeated previews of the same one are free.
pub fn materialize(vpath: &VPath, cache_root: &Path) -> Result<PathBuf> {
    materialize_cancellable(vpath, cache_root, &|| false)
}

/// [`materialize`] that gives up with `Cancelled` between chunks.
pub fn materialize_cancellable(vpath: &VPath, cache_root: &Path, is_cancelled: &dyn Fn() -> bool) -> Result<PathBuf> {
    if let Some(local) = vpath.local_path() {
        return Ok(local);
    }
    if let Some(own) = vpath.fs.materialize(&vpath.path, cache_root) {
        return own;
    }
    let entry = vpath.stat()?;
    if entry.is_dir {
        // Checked before creating the cache folder, so a failure leaves nothing.
        return Err(VfsError::Io(io::Error::new(io::ErrorKind::IsADirectory, format!("is a directory: {}", vpath.display()))));
    }
    let target = cache_root.join("vfs").join(cache_key(vpath, &entry)).join(cache_name(vpath));
    if target.exists() {
        return Ok(target);
    }
    let dir = target.parent().expect("joined above");
    std::fs::create_dir_all(dir)?;
    // A unique .part per call: two previews of the same file may race; the
    // rename makes whichever finishes last win with identical contents.
    static N: AtomicU64 = AtomicU64::new(0);
    let partial = dir.join(format!(
        "{}.{}-{}.part",
        target.file_name().expect("named").to_string_lossy(),
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut src = vpath.open_read()?;
        let mut dst = std::fs::File::create(&partial)?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            if is_cancelled() {
                return Err(VfsError::Cancelled);
            }
            let n = match src.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            dst.write_all(&buf[..n])?;
        }
        dst.flush()?;
        drop(dst);
        std::fs::rename(&partial, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    result.map(|()| target)
}

fn cache_key(vpath: &VPath, entry: &Entry) -> String {
    let mut h = sha1_smol::Sha1::new();
    h.update(format!("{}|{}|{}", vpath.display(), entry.size, entry.mtime).as_bytes());
    let mut hex = h.digest().to_string();
    hex.truncate(16);
    hex
}

/// The item's name, made safe as a single local file name.
fn cache_name(vpath: &VPath) -> String {
    let name = vpath.name().replace(['/', '\\', '\0'], "_");
    match name.as_str() {
        "" | "." | ".." => "item".to_string(),
        _ => name,
    }
}

