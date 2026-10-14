//! Native copy / move / trash on the local disk (the operations half of
//! `fs.py`). Cross-file-system transfers stream through `oc_vfs`; these work
//! on real paths so they can rename instead of copying, reproduce symlinks,
//! keep permissions and timestamps, and use the OS's fast copy paths.

use std::ffi::OsString;
use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use oc_vfs::{Result, Transfer, TransferCallbacks, VfsError};

use crate::error::IoContext;

/// Files up to this size are copied in one OS call (no progress or cancel
/// inside them): per-file overhead dominates small files, and the OS copy
/// routines are the fastest path.
const SMALL_FILE: u64 = 8 * 1024 * 1024;

/// Chunk between progress reports / cancel checks for large files.
const CHUNK: usize = oc_vfs::CHUNK;

/// Knobs the tests turn to reach the chunked path on a cloning file system,
/// and the cross-device move without a second file system.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tuning {
    pub small_file: u64,
    /// Try a copy-on-write clone first (APFS); instant, so no progress needed.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub clone: bool,
    /// Treat every rename as crossing devices (tests).
    pub fake_cross_device: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning { small_file: SMALL_FILE, clone: true, fake_cross_device: false }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn op_err(msg: String) -> VfsError {
    VfsError::Operation(msg)
}

/// A directory that isn't a symlink (Python: `is_dir() and not is_symlink()`).
fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

fn lsize(p: &Path) -> u64 {
    fs::symlink_metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn display(p: &Path) -> String {
    p.display().to_string()
}

/// Delete a file, link or whole directory tree (a link is removed, never followed).
pub(crate) fn remove_any(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).at(path)?;
    remove_with_meta(path, &meta)
}

fn remove_with_meta(path: &Path, meta: &Metadata) -> Result<()> {
    if meta.is_dir() {
        return fs::remove_dir_all(path).at(path);
    }
    #[cfg(windows)]
    {
        // Directory links and junctions (even dangling ones) must be removed
        // as directories on Windows; the link's own attributes say which.
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
        if meta.is_symlink() && meta.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 {
            return fs::remove_dir(path).at(path);
        }
    }
    fs::remove_file(path).at(path)
}

/// Same file (device + inode), following links; false if either is missing.
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (fs::metadata(a), fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        match (fs::canonicalize(a), fs::canonicalize(b)) {
            (Ok(x), Ok(y)) => x.to_string_lossy().to_lowercase() == y.to_string_lossy().to_lowercase(),
            _ => false,
        }
    }
}

/// Like Python's non-strict `Path.resolve()`: resolve links as far as the
/// path exists, append the rest.
fn resolve_lenient(p: &Path) -> PathBuf {
    if let Ok(c) = fs::canonicalize(p) {
        return c;
    }
    let abs = crate::absolute(p);
    let mut rest: Vec<OsString> = Vec::new();
    let mut cur = abs.as_path();
    loop {
        if let Ok(c) = fs::canonicalize(cur) {
            let mut out = c;
            out.extend(rest.iter().rev());
            return out;
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                cur = parent;
            }
            _ => return abs,
        }
    }
}

/// Python's `os.path.normcase`: case-folds only on Windows.
fn normcase(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(p.to_string_lossy().to_lowercase().replace('/', "\\"))
    }
    #[cfg(not(windows))]
    {
        p
    }
}

fn check_not_into_self(src: &Path, dst: &Path) -> Result<()> {
    if !is_real_dir(src) {
        return Ok(());
    }
    let s = normcase(resolve_lenient(src));
    let d = normcase(resolve_lenient(dst));
    if d.starts_with(&s) {
        let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        return Err(op_err(format!("Can't copy or move '{name}' into itself")));
    }
    Ok(())
}

/// Map each source to its destination. If `target` is an existing directory,
/// sources go inside it; otherwise a single source is renamed to `target`,
/// and several sources need a directory.
pub fn resolve_target<P: AsRef<Path>>(sources: &[P], target: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    if target.is_dir() {
        return Ok(sources
            .iter()
            .map(|s| {
                let s = s.as_ref();
                (s.to_path_buf(), target.join(s.file_name().unwrap_or_default()))
            })
            .collect());
    }
    if let [only] = sources {
        return Ok(vec![(only.as_ref().to_path_buf(), target.to_path_buf())]);
    }
    Err(op_err(format!("Destination folder {} doesn't exist", target.display())))
}

/// (file count, total bytes) for everything below `paths`. Directories don't
/// count as files; links (also to directories) count with their own size.
pub fn measure<P: AsRef<Path>>(paths: &[P]) -> (u64, u64) {
    paths.iter().map(|p| measure_one(p.as_ref())).fold((0, 0), |(a, b), (c, d)| (a + c, b + d))
}

fn measure_one(path: &Path) -> (u64, u64) {
    let mut acc = (0, 0);
    if is_real_dir(path) {
        measure_dir(path, &mut acc);
    } else {
        acc = (1, lsize(path));
    }
    acc
}

fn measure_dir(dir: &Path, acc: &mut (u64, u64)) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for de in rd.flatten() {
        if de.file_type().is_ok_and(|t| t.is_dir()) {
            measure_dir(&de.path(), acc);
        } else {
            acc.0 += 1;
            acc.1 += de.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
}

/// Children of a directory, sorted by name (deterministic order for copies).
fn sorted_children(dir: &Path) -> Result<Vec<OsString>> {
    let mut names = fs::read_dir(dir).at(dir)?.map(|de| de.map(|d| d.file_name())).collect::<io::Result<Vec<_>>>().at(dir)?;
    names.sort_unstable();
    Ok(names)
}

/// Recreate the link at `src` (same target text) at `dst`.
fn copy_link(src: &Path, dst: &Path) -> Result<()> {
    let target = fs::read_link(src).at(src)?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, dst).at2(&target, dst)
    }
    #[cfg(windows)]
    {
        // The link's own attributes say whether it is a directory link, even
        // when its target is gone (Python asked the target, losing that).
        use std::os::windows::fs::MetadataExt;
        if fs::symlink_metadata(src).is_ok_and(|m| m.file_attributes() & 0x10 != 0) {
            std::os::windows::fs::symlink_dir(&target, dst).at2(&target, dst)
        } else {
            std::os::windows::fs::symlink_file(&target, dst).at2(&target, dst)
        }
    }
}

/// Copy permissions, timestamps (and on macOS the user flags such as
/// "hidden") like `shutil.copystat`. Best effort for flags and times; a
/// failing chmod is reported. Flags go last, as in Python: a "Locked" (uchg)
/// item refuses chmod and utimes once the flag is set.
fn copy_stat_path(meta: &Metadata, dst: &Path) -> Result<()> {
    let atime = filetime::FileTime::from_last_access_time(meta);
    let mtime = filetime::FileTime::from_last_modification_time(meta);
    let _ = filetime::set_file_times(dst, atime, mtime);
    fs::set_permissions(dst, meta.permissions()).at(dst)?;
    #[cfg(target_os = "macos")]
    if let Ok(c) = crate::system::c_path(dst) {
        // SAFETY: c is NUL-terminated.
        unsafe { libc::chflags(c.as_ptr(), user_flags(meta)) };
    }
    Ok(())
}

fn copy_stat_file(meta: &Metadata, dst: &File, dst_path: &Path) -> Result<()> {
    let atime = filetime::FileTime::from_last_access_time(meta);
    let mtime = filetime::FileTime::from_last_modification_time(meta);
    let _ = filetime::set_file_handle_times(dst, Some(atime), Some(mtime));
    dst.set_permissions(meta.permissions()).at(dst_path)?;
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: the fd is open for the lifetime of `dst`.
        unsafe { libc::fchflags(dst.as_raw_fd(), user_flags(meta)) };
    }
    Ok(())
}

/// UF_* flags only: the SF_* (system) ones, like SF_DATALESS, can't be set by users.
#[cfg(target_os = "macos")]
fn user_flags(meta: &Metadata) -> u32 {
    use std::os::macos::fs::MetadataExt;
    meta.st_flags() & 0xffff
}

/// A copy-on-write clone (APFS): instant whatever the size, keeps metadata.
#[cfg(target_os = "macos")]
fn try_clone(src: &Path, dst: &Path) -> bool {
    let (Ok(s), Ok(d)) = (crate::system::c_path(src), crate::system::c_path(dst)) else { return false };
    // SAFETY: both are NUL-terminated paths. CLONE_NOFOLLOW = 1.
    unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 1) == 0 }
}

/// Copy up to one chunk; 0 at end of file. On Linux the kernel moves the data
/// (`io::copy` specializes to copy_file_range/sendfile), elsewhere a reused buffer.
fn copy_chunk(from: &mut File, to: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let _ = buf;
        io::copy(&mut Read::by_ref(from).take(CHUNK as u64), to).map(|n| n as usize)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let n = loop {
            match from.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                r => break r?,
            }
        };
        io::Write::write_all(to, &buf[..n])?;
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// The runner
// ---------------------------------------------------------------------------

/// One native copy/move: progress, conflicts and cancellation via [`Transfer`].
struct Native<'a> {
    t: Transfer<'a>,
    tuning: Tuning,
    /// Reused for every chunked copy (allocated on first use).
    buf: Vec<u8>,
}

impl<'a> Native<'a> {
    fn new(callbacks: TransferCallbacks<'a>, totals: (u64, u64), tuning: Tuning) -> Self {
        Native { t: Transfer::new(callbacks, totals.0, totals.1), tuning, buf: Vec::new() }
    }

    /// Count skipped (or renamed-at-once) items as done and tell the UI.
    fn skip(&mut self, (files, bytes): (u64, u64)) {
        self.t.progress.files_done += files;
        self.t.progress.bytes_done += bytes;
        self.t.report(None);
    }

    /// Ok(true) if `dst` may be written: it's absent, a directory to merge
    /// into, or the user chose to overwrite (then the old item is removed).
    fn resolve_conflict(&mut self, src: &Path, dst: &Path) -> Result<bool> {
        let Ok(dmeta) = fs::symlink_metadata(dst) else { return Ok(true) };
        if dmeta.is_dir() && is_real_dir(src) {
            return Ok(true); // directories merge
        }
        if same_file(src, dst) {
            return Err(op_err(format!("Source and destination are the same file: {}", dst.display())));
        }
        if !self.t.ask_conflict(&display(src), &display(dst))? {
            return Ok(false);
        }
        remove_with_meta(dst, &dmeta)?;
        Ok(true)
    }

    /// Ok(false) if the user skipped it.
    fn copy_file(&mut self, src: &Path, dst: &Path) -> Result<bool> {
        self.t.check_cancel()?;
        self.t.report(Some(&display(src)));
        if !self.resolve_conflict(src, dst)? {
            self.skip((1, lsize(src)));
            return Ok(false);
        }
        let meta = fs::symlink_metadata(src).at(src)?;
        if meta.is_symlink() {
            copy_link(src, dst)?;
            self.t.progress.bytes_done += meta.len();
        } else {
            let before = self.t.progress.bytes_done;
            if let Err(e) = self.copy_contents(src, dst, &meta) {
                // Don't leave a truncated file behind (dst was absent before).
                let _ = fs::remove_file(dst);
                self.t.progress.bytes_done = before;
                return Err(e);
            }
        }
        self.t.progress.files_done += 1;
        self.t.report(None);
        Ok(true)
    }

    fn copy_contents(&mut self, src: &Path, dst: &Path, meta: &Metadata) -> Result<()> {
        let size = meta.len();
        #[cfg(target_os = "macos")]
        if self.tuning.clone && try_clone(src, dst) {
            self.t.progress.bytes_done += size;
            return Ok(());
        }
        if size <= self.tuning.small_file {
            // clonefile/fcopyfile on macOS, copy_file_range on Linux, CopyFileExW on Windows.
            fs::copy(src, dst).at2(src, dst)?;
            self.t.progress.bytes_done += size;
            // macOS (clone or COPYFILE_ALL) and Windows (CopyFileExW) already
            // copied mode, times and flags; Linux copied only the mode.
            #[cfg(not(any(windows, target_os = "macos")))]
            copy_stat_path(meta, dst)?;
            return Ok(());
        }
        let mut from = File::open(src).at(src)?;
        let mut to = File::create(dst).at(dst)?;
        if self.buf.len() < CHUNK {
            self.buf = vec![0; CHUNK];
        }
        loop {
            let n = copy_chunk(&mut from, &mut to, &mut self.buf).at2(src, dst)?;
            if n == 0 {
                break;
            }
            self.t.progress.bytes_done += n as u64;
            self.t.report(None);
            self.t.check_cancel()?;
        }
        copy_stat_file(meta, &to, dst)
    }

    fn copy_tree(&mut self, src: &Path, dst: &Path) -> Result<()> {
        let meta = match fs::symlink_metadata(src) {
            Ok(m) if m.is_dir() => m,
            _ => return self.copy_file(src, dst).map(drop),
        };
        self.t.check_cancel()?;
        if !self.resolve_conflict(src, dst)? {
            self.skip(measure_one(src));
            return Ok(());
        }
        create_dir_or_merge(dst)?;
        for name in sorted_children(src)? {
            self.copy_tree(&src.join(&name), &dst.join(&name))?;
        }
        copy_stat_path(&meta, dst)
    }

    /// `known`: the source's measure if already taken (top-level items).
    fn mv(&mut self, src: &Path, dst: &Path, known: Option<(u64, u64)>) -> Result<()> {
        self.t.check_cancel()?;
        self.t.report(Some(&display(src)));
        if let Ok(dmeta) = fs::symlink_metadata(dst) {
            if dmeta.is_dir() && is_real_dir(src) {
                // Merge: move children one by one, then drop the emptied source.
                for name in sorted_children(src)? {
                    self.mv(&src.join(&name), &dst.join(&name), None)?;
                }
                let _ = fs::remove_dir(src); // fails if something was skipped
                return Ok(());
            }
            if !self.resolve_conflict(src, dst)? {
                self.skip(known.unwrap_or_else(|| measure_one(src)));
                return Ok(());
            }
        }
        let renamed = if self.tuning.fake_cross_device {
            Err(io::Error::from(io::ErrorKind::CrossesDevices))
        } else {
            fs::rename(src, dst)
        };
        match renamed {
            Ok(()) => {
                self.skip(known.unwrap_or_else(|| measure_one(dst)));
                Ok(())
            }
            Err(e) if is_cross_device(&e) => self.move_across(src, dst),
            Err(e) => Err(crate::error::with_paths(e, src, dst)),
        }
    }

    /// A move to another file system: copy, then delete what was copied.
    /// Item by item rather than copy-all-then-delete-all (as Python did), so
    /// anything skipped on the way — e.g. "README" and "readme" colliding on a
    /// case-insensitive target — stays at the source instead of being deleted.
    fn move_across(&mut self, src: &Path, dst: &Path) -> Result<()> {
        let meta = fs::symlink_metadata(src).at(src)?;
        if !meta.is_dir() {
            if self.copy_file(src, dst)? {
                remove_with_meta(src, &meta)?;
            }
            return Ok(());
        }
        self.t.check_cancel()?;
        if !self.resolve_conflict(src, dst)? {
            self.skip(measure_one(src));
            return Ok(());
        }
        create_dir_or_merge(dst)?;
        for name in sorted_children(src)? {
            self.move_across(&src.join(&name), &dst.join(&name))?;
        }
        copy_stat_path(&meta, dst)?;
        let _ = fs::remove_dir(src); // fails if something was skipped
        Ok(())
    }
}

/// Create `dst`, or use it if it already is a directory (merge).
fn create_dir_or_merge(dst: &Path) -> Result<()> {
    match fs::create_dir(dst) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && is_real_dir(dst) => Ok(()),
        Err(e) => Err(crate::error::with_path(e, dst)),
    }
}

fn is_cross_device(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::CrossesDevices {
        return true;
    }
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(libc::EXDEV)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Copy `sources` to `target` (see [`resolve_target`] for the semantics).
/// Directories merge into existing ones; other existing items go through
/// `on_conflict`. A cancelled or failed file is removed, not left truncated.
pub fn copy_paths<P: AsRef<Path>>(sources: &[P], target: &Path, callbacks: TransferCallbacks<'_>) -> Result<()> {
    copy_paths_tuned(sources, target, callbacks, Tuning::default())
}

pub(crate) fn copy_paths_tuned<P: AsRef<Path>>(
    sources: &[P],
    target: &Path,
    callbacks: TransferCallbacks<'_>,
    tuning: Tuning,
) -> Result<()> {
    let pairs = resolve_target(sources, target)?;
    for (src, dst) in &pairs {
        check_not_into_self(src, dst)?;
    }
    let mut runner = Native::new(callbacks, measure(sources), tuning);
    for (src, dst) in &pairs {
        runner.copy_tree(src, dst)?;
    }
    Ok(())
}

/// Move / rename `sources` to `target` (see [`resolve_target`]). A rename
/// where possible; across devices, copy then delete.
pub fn move_paths<P: AsRef<Path>>(sources: &[P], target: &Path, callbacks: TransferCallbacks<'_>) -> Result<()> {
    move_paths_tuned(sources, target, callbacks, Tuning::default())
}

pub(crate) fn move_paths_tuned<P: AsRef<Path>>(
    sources: &[P],
    target: &Path,
    callbacks: TransferCallbacks<'_>,
    tuning: Tuning,
) -> Result<()> {
    let pairs = resolve_target(sources, target)?;
    for (src, dst) in &pairs {
        check_not_into_self(src, dst)?;
        if src == dst {
            return Err(op_err(format!("Source and destination are the same: {}", dst.display())));
        }
    }
    // Measured per source so a plain rename doesn't walk the tree again.
    let sizes: Vec<(u64, u64)> = pairs.iter().map(|(s, _)| measure_one(s)).collect();
    let totals = sizes.iter().fold((0, 0), |(a, b), (c, d)| (a + c, b + d));
    let mut runner = Native::new(callbacks, totals, tuning);
    for ((src, dst), size) in pairs.iter().zip(sizes) {
        runner.mv(src, dst, Some(size))?;
    }
    Ok(())
}

/// Move `paths` to the OS recycle bin / trash, all in one OS operation.
///
/// This can be slow, and nothing here can change that: on Windows it is the
/// shell's file operation (one per call, set up on the calling thread),
/// which visits every file inside a folder to make it restorable — big
/// trees, folders kept in sync by cloud storage clients and network drives
/// take seconds to minutes. The UI runs it in the background
/// (`oc_ui::features::fileops::Removals`).
pub fn trash_paths<P: AsRef<Path>>(paths: &[P]) -> Result<()> {
    #[allow(unused_mut)]
    let mut ctx = trash::TrashContext::default();
    #[cfg(target_os = "macos")]
    {
        // The default (Finder via AppleScript) asks for Automation permission
        // and plays a sound; NSFileManager does neither.
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        ctx.set_delete_method(DeleteMethod::NsFileManager);
    }
    ctx.delete_all(paths.iter().map(AsRef::as_ref)).map_err(trash_error)
}

/// The trash library's error in words: its own text is debug output.
fn trash_error(e: trash::Error) -> VfsError {
    const CANT: &str = "Can't move to the trash";
    op_err(match e {
        trash::Error::TargetedRoot => "Can't move the root folder to the trash".into(),
        trash::Error::CanonicalizePath { original } => format!("{CANT}: {} not found", original.display()),
        trash::Error::CouldNotAccess { target } => format!("{CANT}: can't access {target}"),
        trash::Error::ConvertOsString { original } => {
            format!("{CANT}: the name {} isn't valid text", original.to_string_lossy())
        }
        #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
        trash::Error::FileSystem { path, source } => {
            format!("Can't move {} to the trash: {}", path.display(), oc_vfs::io_reason(&source))
        }
        trash::Error::Os { description, .. } | trash::Error::Unknown { description } => {
            // macOS: "While deleting '…', `trashItemAtURL` failed: <the system's sentence>";
            // Windows: "windows error: <the system's sentence>".
            let reason = description
                .rsplit_once("failed: ")
                .map(|(_, r)| r)
                .or_else(|| description.strip_prefix("windows error: "))
                .unwrap_or(&description)
                .trim()
                .trim_end_matches('.');
            format!("{CANT}: {reason}")
        }
        _ => CANT.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_vfs::{Conflict, Progress};
    use std::cell::Cell;

    /// Forces the chunked path (no clone, no one-shot OS copy).
    const CHUNKED: Tuning = Tuning { small_file: 0, clone: false, fake_cross_device: false };
    const ACROSS: Tuning = Tuning { fake_cross_device: true, ..CHUNKED };

    fn big_file(dir: &Path) -> (PathBuf, Vec<u8>) {
        let data: Vec<u8> = (0..(CHUNK * 3 + 123)).map(|i| (i % 251) as u8).collect();
        let p = dir.join("big.bin");
        fs::write(&p, &data).unwrap();
        (p, data)
    }

    #[test]
    fn chunked_copy_reports_progress_per_chunk() {
        let t = tempfile::tempdir().unwrap();
        let (src, data) = big_file(t.path());
        let dst = t.path().join("out.bin");
        let mut snaps: Vec<Progress> = Vec::new();
        let mut on_conflict = |_: &str, _: &str| Conflict::Cancel;
        let mut on_progress = |p: &Progress| snaps.push(p.clone());
        let cb = TransferCallbacks { on_conflict: &mut on_conflict, on_progress: Some(&mut on_progress), is_cancelled: &|| false };
        copy_paths_tuned(&[&src], &dst, cb, CHUNKED).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), data);
        let bytes: Vec<u64> = snaps.iter().map(|p| p.bytes_done).collect();
        assert!(bytes.windows(2).all(|w| w[0] <= w[1]));
        assert!(bytes.iter().filter(|b| **b > 0 && **b < data.len() as u64).count() >= 3, "{bytes:?}");
        let last = snaps.last().unwrap();
        assert_eq!((last.bytes_done, last.bytes_total, last.files_done), (data.len() as u64, data.len() as u64, 1));
        assert_eq!(last.current, src.display().to_string());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode();
            assert_eq!(mode(&src), mode(&dst));
        }
    }

    #[test]
    fn cancel_mid_file_removes_the_partial_copy() {
        let t = tempfile::tempdir().unwrap();
        let (src, _) = big_file(t.path());
        let dst = t.path().join("out.bin");
        let reports = Cell::new(0);
        let mut on_conflict = |_: &str, _: &str| Conflict::Cancel;
        let mut on_progress = |_: &Progress| reports.set(reports.get() + 1);
        // Cancel once the first chunk is through.
        let cancelled = || reports.get() >= 2;
        let cb = TransferCallbacks { on_conflict: &mut on_conflict, on_progress: Some(&mut on_progress), is_cancelled: &cancelled };
        let r = copy_paths_tuned(&[&src], &dst, cb, CHUNKED);
        assert!(matches!(r, Err(VfsError::Cancelled)), "{r:?}");
        assert!(!dst.exists());
        assert!(src.exists());
    }

    /// Finder's "Locked" (uchg) files and folders copy, and stay locked; the
    /// flags must be set after chmod and times, as `shutil.copystat` does.
    #[cfg(target_os = "macos")]
    #[test]
    fn locked_items_copy_on_every_path() {
        use std::os::macos::fs::MetadataExt;
        use std::os::unix::fs::MetadataExt as _;
        const UF_IMMUTABLE: u32 = 0x2;
        let chflags = |flag: &str, p: &Path| {
            assert!(std::process::Command::new("chflags").args(["-R", flag]).arg(p).status().unwrap().success());
        };
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("small"), "s").unwrap();
        big_file(&src);
        filetime::set_file_mtime(src.join("small"), filetime::FileTime::from_unix_time(1_500_000_000, 0)).unwrap();
        chflags("uchg", &src);
        let tunings = [
            ("clone", Tuning::default()),
            ("os copy", Tuning { clone: false, ..Tuning::default() }),
            ("chunked", CHUNKED),
        ];
        let mut results = Vec::new();
        for (label, tuning) in tunings {
            let dst = t.path().join(label);
            let mut on_conflict = |_: &str, _: &str| Conflict::Cancel;
            let cb = TransferCallbacks { on_conflict: &mut on_conflict, on_progress: None, is_cancelled: &|| false };
            let r = copy_paths_tuned(&[&src], &dst, cb, tuning);
            let flags = |name: &str| fs::symlink_metadata(dst.join(name)).map(|m| m.st_flags() & UF_IMMUTABLE != 0);
            let mtime = fs::metadata(dst.join("small")).map(|m| m.mtime());
            results.push((label, r.map_err(|e| e.to_string()), flags("."), flags("small"), flags("big.bin"), mtime));
        }
        chflags("nouchg", t.path());
        for (label, r, dir, small, big, mtime) in results {
            assert!(r.is_ok(), "{label}: {r:?}");
            assert!(dir.unwrap() && small.unwrap() && big.unwrap(), "{label}: lock lost");
            assert_eq!(mtime.unwrap(), 1_500_000_000, "{label}");
        }
    }

    /// src/{a.txt, b.bin, sub/c.txt} (+ links on Unix)
    fn move_tree(dir: &Path) -> PathBuf {
        let src = dir.join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.txt"), "alpha").unwrap();
        fs::write(src.join("b.bin"), vec![7u8; 3000]).unwrap();
        fs::write(src.join("sub/c.txt"), "gamma").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("sub", src.join("sublink")).unwrap();
            std::os::unix::fs::symlink("gone", src.join("dangling")).unwrap();
        }
        src
    }

    #[test]
    fn move_across_devices_copies_then_deletes() {
        let t = tempfile::tempdir().unwrap();
        let src = move_tree(t.path());
        filetime::set_file_mtime(src.join("a.txt"), filetime::FileTime::from_unix_time(1_500_000_000, 0)).unwrap();
        let totals = measure(&[&src]);
        let dst = t.path().join("moved");
        let mut last = Progress::default();
        let mut on_conflict = |s: &str, d: &str| panic!("unexpected conflict {s} -> {d}");
        let mut on_progress = |p: &Progress| last = p.clone();
        let cb = TransferCallbacks { on_conflict: &mut on_conflict, on_progress: Some(&mut on_progress), is_cancelled: &|| false };
        move_paths_tuned(&[&src], &dst, cb, ACROSS).unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "alpha");
        assert_eq!(fs::read_to_string(dst.join("sub/c.txt")).unwrap(), "gamma");
        assert_eq!(fs::read(dst.join("b.bin")).unwrap(), vec![7u8; 3000]);
        let mtime = filetime::FileTime::from_last_modification_time(&fs::metadata(dst.join("a.txt")).unwrap());
        assert_eq!(mtime.unix_seconds(), 1_500_000_000);
        #[cfg(unix)]
        for name in ["sublink", "dangling"] {
            assert!(fs::symlink_metadata(dst.join(name)).unwrap().file_type().is_symlink(), "{name}");
        }
        assert_eq!((last.files_done, last.bytes_done), totals);
        assert_eq!((last.files_total, last.bytes_total), totals);
    }

    /// Something skipped during a cross-device move must stay at the source
    /// (the Python version copied everything, then deleted the whole source).
    #[test]
    fn move_across_devices_keeps_what_was_skipped() {
        let t = tempfile::tempdir().unwrap();
        let src = move_tree(t.path());
        let dst = t.path().join("moved");
        let intruder = dst.join("sub/c.txt");
        let mut asked = Vec::new();
        let mut on_conflict = |s: &str, d: &str| {
            asked.push((s.to_string(), d.to_string()));
            Conflict::Skip
        };
        // A file shows up at the destination just before c.txt is copied
        // (as a case-insensitive name collision would).
        let mut on_progress = |p: &Progress| {
            if p.current.ends_with("c.txt") && !intruder.exists() && intruder.parent().unwrap().exists() {
                fs::write(&intruder, "intruder").unwrap();
            }
        };
        let cb = TransferCallbacks { on_conflict: &mut on_conflict, on_progress: Some(&mut on_progress), is_cancelled: &|| false };
        move_paths_tuned(&[&src], &dst, cb, ACROSS).unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(fs::read_to_string(src.join("sub/c.txt")).unwrap(), "gamma", "skipped file was deleted");
        assert_eq!(fs::read_to_string(&intruder).unwrap(), "intruder");
        assert!(!src.join("a.txt").exists() && !src.join("b.bin").exists());
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "alpha");
    }

    #[test]
    fn move_across_devices_cancel_keeps_the_rest() {
        let t = tempfile::tempdir().unwrap();
        let src = move_tree(t.path());
        let dst = t.path().join("moved");
        let done = Cell::new(0u64);
        let mut on_conflict = |_: &str, _: &str| Conflict::Cancel;
        let mut on_progress = |p: &Progress| done.set(p.files_done);
        let cancelled = || done.get() >= 1;
        let cb = TransferCallbacks { on_conflict: &mut on_conflict, on_progress: Some(&mut on_progress), is_cancelled: &cancelled };
        let r = move_paths_tuned(&[&src], &dst, cb, ACROSS);
        assert!(matches!(r, Err(VfsError::Cancelled)), "{r:?}");
        // a.txt (first by name) moved; everything else still at the source.
        assert!(!src.join("a.txt").exists() && dst.join("a.txt").exists());
        assert!(src.join("b.bin").exists() && src.join("sub/c.txt").exists());
    }

    #[test]
    fn lenient_resolve_appends_missing_parts() {
        let t = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(t.path()).unwrap();
        assert_eq!(resolve_lenient(&t.path().join("a/b")), base.join("a").join("b"));
        assert_eq!(resolve_lenient(t.path()), base);
    }

    #[test]
    fn measure_counts_files_not_dirs() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("d/e")).unwrap();
        fs::write(t.path().join("d/e/f"), "12345").unwrap();
        fs::write(t.path().join("g"), "12").unwrap();
        assert_eq!(measure(&[t.path().join("d"), t.path().join("g")]), (2, 7));
        // A missing path counts as one empty file (the copy will report it).
        assert_eq!(measure(&[t.path().join("missing")]), (1, 0));
    }
}
