//! The local disk: [`LocalFS`] plus the native operations behind it (port of
//! `fs.py` and the `LocalFS` part of `vfs.py`).
//!
//! Paths are absolute OS paths as strings. Names that aren't valid UTF-8
//! (possible on Linux) are shown lossily and can't be opened by name — the
//! shared [`Entry`] stores names as `String`.
//!
//! [`copy`] is the entry point for copy/move between any two file systems:
//! local-to-local transfers use the native routines here (renames, symlinks,
//! OS fast copies), everything else streams through `oc_vfs::ops`.

mod cloud;
mod error;
mod listing;
mod ops;
mod system;

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;

use oc_vfs::{
    Capabilities, Entry, FileSystem, ListOptions, ReadStream, Result, TransferCallbacks, VPath, VfsError, WalkIter,
    WriteStream,
};

pub use listing::{
    SF_DATALESS, UF_HIDDEN, WINDOWS_CLOUD_ATTRIBUTES, WINDOWS_HIDDEN_ATTRIBUTES, list_dir, parent_of, read_entries,
    stat_entry,
};
pub use oc_vfs::human_size;
pub use ops::{copy_paths, measure, move_paths, resolve_target, trash_paths};
pub use system::{disk_free, home_dir, locations, open_with_default_app};

use error::IoContext;

/// What every [`LocalFS`] reports as its identity: there is one local disk,
/// so all instances (and VPaths on them) compare equal.
pub const LOCAL_IDENTITY: &str = "local";

static CLOUD_FETCHES: LazyLock<cloud::CloudFetches> = LazyLock::new(cloud::CloudFetches::default);

/// Replaces the OS trash (tests, or a sandbox without one).
pub type Trasher = dyn Fn(&[PathBuf]) -> Result<()> + Send + Sync;

/// The real file system; paths are absolute OS paths.
#[derive(Default)]
pub struct LocalFS {
    trasher: Option<Box<Trasher>>,
}

impl LocalFS {
    pub fn new() -> Self {
        LocalFS::default()
    }

    /// A LocalFS whose `trash` calls `trasher` instead of the OS trash.
    pub fn with_trasher(trasher: impl Fn(&[PathBuf]) -> Result<()> + Send + Sync + 'static) -> Self {
        LocalFS { trasher: Some(Box::new(trasher)) }
    }
}

impl FileSystem for LocalFS {
    fn type_name(&self) -> String {
        "local disk".to_string()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { writable: true, can_trash: true, can_rename: true, watchable: true }
    }

    fn identity(&self) -> String {
        LOCAL_IDENTITY.to_string()
    }

    fn is_local(&self) -> bool {
        true
    }

    fn list_dir(&self, path: &str, opts: ListOptions) -> Result<Vec<Entry>> {
        let p = Path::new(path);
        list_dir(p, opts).at(p)
    }

    fn stat(&self, path: &str) -> Result<Entry> {
        let p = Path::new(path);
        stat_entry(p).at(p)
    }

    /// True for dangling links too (they can be deleted, renamed, overwritten).
    fn exists(&self, path: &str) -> bool {
        fs::symlink_metadata(path).is_ok()
    }

    fn is_dir(&self, path: &str) -> bool {
        Path::new(path).is_dir()
    }

    fn parent_path(&self, path: &str) -> Option<String> {
        parent_of(Path::new(path)).map(path_string)
    }

    fn join(&self, path: &str, names: &[&str]) -> String {
        let mut p = PathBuf::from(path);
        for n in names {
            p.push(n);
        }
        path_string(p)
    }

    fn basename(&self, path: &str) -> String {
        basename(path).to_string()
    }

    fn display(&self, path: &str) -> String {
        path.to_string()
    }

    fn open_read(&self, path: &str) -> Result<Box<dyn ReadStream>> {
        let p = Path::new(path);
        Ok(Box::new(File::open(p).at(p)?))
    }

    fn walk<'a>(&'a self, path: &str) -> Result<WalkIter<'a>> {
        let p = Path::new(path);
        Ok(Box::new(listing::Walk::new(p).at(p)?))
    }

    fn local_path(&self, path: &str) -> Option<PathBuf> {
        Some(PathBuf::from(path))
    }

    fn fetch_online_only(&self, path: &str, timeout: Duration, is_cancelled: &dyn Fn() -> bool) -> Result<()> {
        CLOUD_FETCHES.fetch(Path::new(path), timeout, is_cancelled)
    }

    fn free_space(&self, path: &str) -> Option<u64> {
        disk_free(Path::new(path))
    }

    /// Creates missing parents. Without `exist_ok`, an existing path is an error.
    fn mkdir(&self, path: &str, exist_ok: bool) -> Result<()> {
        let p = Path::new(path);
        if exist_ok {
            return fs::create_dir_all(p).at(p);
        }
        if let Some(parent) = p.parent().filter(|q| !q.as_os_str().is_empty()) {
            fs::create_dir_all(parent).at(parent)?;
        }
        fs::create_dir(p).at(p)
    }

    fn remove(&self, path: &str) -> Result<()> {
        ops::remove_any(Path::new(path))
    }

    fn trash(&self, paths: &[String]) -> Result<()> {
        let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        match &self.trasher {
            Some(t) => t(&paths),
            None => trash_paths(&paths),
        }
    }

    fn rename(&self, src: &str, dst: &str) -> Result<()> {
        fs::rename(src, dst).at2(Path::new(src), Path::new(dst))
    }

    /// Writes in place (truncating), like Python's `open(path, "wb")`; callers
    /// that need atomic saves (the editor) write a temp file and rename.
    fn open_write(&self, path: &str) -> Result<Box<dyn WriteStream>> {
        let p = Path::new(path);
        let file = File::create(p).at(p)?;
        Ok(Box::new(LocalWriter { out: BufWriter::new(file), path: p.to_path_buf() }))
    }

    /// Sets access and modification time, like `os.utime(path, (mtime, mtime))`.
    fn set_mtime(&self, path: &str, mtime: f64) -> Result<()> {
        let t = filetime_from_secs(mtime);
        let p = Path::new(path);
        filetime::set_file_times(p, t, t).at(p)
    }
}

/// [`LocalFS::open_write`]'s stream; `finish` flushes and reports errors that
/// a plain drop would swallow.
struct LocalWriter {
    out: BufWriter<File>,
    path: PathBuf,
}

impl Write for LocalWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.out.write(buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.out.write_all(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

impl WriteStream for LocalWriter {
    fn finish(self: Box<Self>) -> Result<()> {
        let LocalWriter { out, path } = *self;
        out.into_inner().map_err(|e| e.into_error()).at(&path)?;
        Ok(())
    }
}

fn filetime_from_secs(secs: f64) -> filetime::FileTime {
    let whole = secs.floor();
    let nanos = (((secs - whole) * 1e9).round() as u32).min(999_999_999);
    filetime::FileTime::from_unix_time(whole as i64, nanos)
}

fn path_string(p: PathBuf) -> String {
    p.into_os_string().into_string().unwrap_or_else(|s| s.to_string_lossy().into_owned())
}

fn is_sep(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

/// Last component, ignoring trailing separators; the path itself for roots
/// ("/" → "/"), like Python's `os.path.basename(path.rstrip(os.sep)) or path`.
pub fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches(is_sep);
    let name = match trimmed.rfind(|c| is_sep(c) || (cfg!(windows) && c == ':')) {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    };
    if name.is_empty() { path } else { name }
}

/// Absolute and lexically normalized ("a/../b" → "b"), like `os.path.abspath`.
pub fn absolute(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // Never above the root.
                if out.file_name().is_some() {
                    out.pop();
                }
            }
            other => out.push(other),
        }
    }
    out
}

static LOCAL: OnceLock<Arc<dyn FileSystem>> = OnceLock::new();

/// The shared local file system.
pub fn local_fs() -> Arc<dyn FileSystem> {
    LOCAL.get_or_init(|| Arc::new(LocalFS::new())).clone()
}

/// A VPath on the local disk, made absolute.
pub fn local(path: impl AsRef<Path>) -> VPath {
    VPath::new(local_fs(), path_string(absolute(path.as_ref())))
}

/// Whether `v` is on the local disk (any [`LocalFS`] instance); same as
/// [`VPath::is_local`].
pub fn is_local(v: &VPath) -> bool {
    v.fs.is_local()
}

/// Copy (or move) `sources` to `target` across any file systems. Target
/// semantics as in [`copy_paths`]: into `target` if it is a directory,
/// otherwise a single source is written under that name. Local-to-local uses
/// the native routines; the rest streams through `oc_vfs::ops`.
pub fn copy(sources: &[VPath], target: &VPath, mv: bool, callbacks: TransferCallbacks<'_>) -> Result<()> {
    if !target.fs.capabilities().writable {
        return Err(VfsError::ReadOnly(target.fs.read_only_reason()));
    }
    if mv && let Some(s) = sources.iter().find(|s| !s.fs.capabilities().writable) {
        return Err(VfsError::ReadOnly(s.fs.read_only_reason()));
    }
    if is_local(target) && sources.iter().all(is_local) {
        let paths: Vec<&Path> = sources.iter().map(|s| Path::new(&s.path)).collect();
        let target = Path::new(&target.path);
        return if mv { move_paths(&paths, target, callbacks) } else { copy_paths(&paths, target, callbacks) };
    }
    oc_vfs::ops::copy_or_move(sources, target, mv, callbacks)
}
