//! Virtual file system: one interface over local directories, archives,
//! servers and virtual folders (search results).
//!
//! Everything above this layer (panels, previews, file operations) works with
//! [`VPath`] values — a file system plus a path in it — and asks the file
//! system what it can do through capability flags instead of checking what it
//! is. Port of `vfs.py` / the listing half of `fs.py` in the Python version.

use std::fmt;
use std::io::{self, Read, Seek, Write};
use std::path::PathBuf;
use std::sync::Arc;

/// Path below a walk's starting point, as parts.
pub type RelPath = Vec<String>;

/// Chunk size for streaming copies.
pub const CHUNK: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum VfsError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("{0}")]
    ReadOnly(String),
    #[error("cancelled")]
    Cancelled,
    /// An online-only (cloud placeholder) file wasn't delivered in time.
    #[error("timed out waiting for {0}")]
    FetchTimeout(String),
    /// A user-facing operation error (e.g. "Target directory does not exist").
    #[error("{0}")]
    Operation(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl VfsError {
    pub fn is_not_found(&self) -> bool {
        match self {
            VfsError::NotFound(_) => true,
            VfsError::Io(e) => e.kind() == io::ErrorKind::NotFound,
            _ => false,
        }
    }
}

pub type Result<T> = std::result::Result<T, VfsError>;

// ---------------------------------------------------------------------------
// Entries and sorting
// ---------------------------------------------------------------------------

/// A single item in a directory listing (on any file system).
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub is_link: bool,
    pub is_hidden: bool,
    pub is_executable: bool,
    /// 0 for directories.
    pub size: u64,
    /// Seconds since the epoch; 0.0 if unknown.
    pub mtime: f64,
    /// A cloud placeholder (Dropbox, iCloud, OneDrive): reading downloads it.
    pub is_online_only: bool,
}

impl Entry {
    /// A plain file entry; handy for tests and virtual file systems.
    pub fn file(name: impl Into<String>, size: u64, mtime: f64) -> Self {
        let name = name.into();
        Entry {
            is_hidden: name.starts_with('.'),
            name,
            is_dir: false,
            is_link: false,
            is_executable: false,
            size,
            mtime,
            is_online_only: false,
        }
    }

    pub fn dir(name: impl Into<String>, mtime: f64) -> Self {
        Entry { is_dir: true, ..Entry::file(name, 0, mtime) }
    }

    /// Lower-case extension without the dot; "" for directories, dotfiles and
    /// names without one.
    pub fn ext(&self) -> String {
        if self.is_dir {
            return String::new();
        }
        ext_of(&self.name)
    }
}

/// Lower-case extension of a file name without the dot ("" if none; ".bashrc" has none).
pub fn ext_of(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => name[i + 1..].to_lowercase(),
        _ => String::new(),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SortKey {
    #[default]
    Name,
    Ext,
    Size,
    Mtime,
}

impl SortKey {
    pub fn as_str(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Ext => "ext",
            SortKey::Size => "size",
            SortKey::Mtime => "mtime",
        }
    }

    pub fn parse(s: &str) -> Option<SortKey> {
        Some(match s {
            "name" => SortKey::Name,
            "ext" => SortKey::Ext,
            "size" => SortKey::Size,
            "mtime" => SortKey::Mtime,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListOptions {
    pub show_hidden: bool,
    pub sort_key: SortKey,
    pub reverse: bool,
}

/// Filter hidden entries and sort: directories first, then files, each group
/// by `sort_key` (ties broken by case-insensitive name). `reverse` reverses
/// each group, not the dirs-first order.
pub fn sort_entries(mut entries: Vec<Entry>, opts: ListOptions) -> Vec<Entry> {
    if !opts.show_hidden {
        entries.retain(|e| !e.is_hidden);
    }
    // Precompute keys once: lowering names per comparison dominates for big folders.
    let mut keyed: Vec<(String, String, Entry)> = entries
        .into_iter()
        .map(|e| {
            let ext = if opts.sort_key == SortKey::Ext { e.ext() } else { String::new() };
            (e.name.to_lowercase(), ext, e)
        })
        .collect();
    keyed.sort_by(|(na, xa, a), (nb, xb, b)| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            let ord = match opts.sort_key {
                SortKey::Name => na.cmp(nb),
                SortKey::Ext => xa.cmp(xb).then_with(|| na.cmp(nb)),
                SortKey::Size => a.size.cmp(&b.size).then_with(|| na.cmp(nb)),
                SortKey::Mtime => a.mtime.total_cmp(&b.mtime).then_with(|| na.cmp(nb)),
            };
            if opts.reverse { ord.reverse() } else { ord }
        })
    });
    keyed.into_iter().map(|(_, _, e)| e).collect()
}

/// "1.2 K", "247 B", "412 G" — the panel's size column.
pub fn human_size(n: u64) -> String {
    if n < 1024 {
        return format!("{n} B");
    }
    let mut size = n as f64;
    for unit in ["K", "M", "G", "T", "P"] {
        size /= 1024.0;
        if size < 1024.0 || unit == "P" {
            return if size < 100.0 { format!("{size:.1} {unit}") } else { format!("{size:.0} {unit}") };
        }
    }
    unreachable!()
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

/// A readable stream. `seek` may fail with `ErrorKind::Unsupported` when the
/// source can't seek (some archive members, tool pipes); check `seekable()`.
pub trait ReadStream: Read + Seek + Send {
    fn seekable(&self) -> bool {
        true
    }
}

/// A writable stream. `finish` commits the file (flush, close the upload,
/// atomically rename a temp file…); dropping without `finish` should discard
/// or at least not report success.
pub trait WriteStream: Write + Send {
    fn finish(self: Box<Self>) -> Result<()>;
}

impl ReadStream for std::fs::File {}
impl ReadStream for io::Cursor<Vec<u8>> {}

// ---------------------------------------------------------------------------
// The file system interface
// ---------------------------------------------------------------------------

/// Capability flags; the UI checks these instead of the concrete type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// mkdir, remove, open_write
    pub writable: bool,
    /// trash() moves to a recycle bin; otherwise deletion is permanent.
    pub can_trash: bool,
    /// rename() within this file system (fast moves).
    pub can_rename: bool,
    /// A local path that a directory watcher can observe.
    pub watchable: bool,
}

/// A source of directories and files. Paths are strings in the file system's
/// own syntax (absolute OS paths for local disks, POSIX paths inside archives
/// with "" as the root, …). Implementations must be thread-safe: listings,
/// previews and copies run on worker threads.
pub trait FileSystem: Send + Sync {
    /// "local disk", "ZIP archive", "SFTP server", …
    fn type_name(&self) -> String;

    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    fn read_only_reason(&self) -> String {
        format!("{} is read-only", self.type_name())
    }

    /// For nested file systems (archives): the file they live in.
    fn container(&self) -> Option<VPath> {
        None
    }

    /// Two handles to the same file system compare equal through this
    /// (e.g. two ArchiveFS opened on the same file). Default: object identity.
    fn identity(&self) -> String {
        format!("{:p}", self as *const Self as *const ())
    }

    // -- reading -------------------------------------------------------------

    /// Entries of `path`, filtered and sorted per `opts` (see [`sort_entries`]).
    fn list_dir(&self, path: &str, opts: ListOptions) -> Result<Vec<Entry>>;

    /// Errors with `NotFound` if `path` doesn't exist.
    fn stat(&self, path: &str) -> Result<Entry>;

    /// Parent within this file system, or None at its root.
    fn parent_path(&self, path: &str) -> Option<String>;

    fn join(&self, path: &str, names: &[&str]) -> String;

    fn basename(&self, path: &str) -> String;

    /// What the user sees in titles and messages.
    fn display(&self, path: &str) -> String;

    fn open_read(&self, path: &str) -> Result<Box<dyn ReadStream>>;

    /// `path` itself (as an empty RelPath) and everything below it, directories
    /// before their contents.
    fn walk<'a>(&'a self, path: &str) -> Result<Box<dyn Iterator<Item = Result<(RelPath, Entry)>> + 'a>>;

    fn exists(&self, path: &str) -> bool {
        self.stat(path).is_ok()
    }

    fn is_dir(&self, path: &str) -> bool {
        self.stat(path).map(|e| e.is_dir).unwrap_or(false)
    }

    /// A real path for this item if one exists without copying.
    fn local_path(&self, _path: &str) -> Option<PathBuf> {
        None
    }

    /// If `path` only stands for a location elsewhere (a search hit), that
    /// location; operations and previews then act on it.
    fn real_location(&self, _path: &str) -> Option<VPath> {
        None
    }

    /// Where ".." leads from this file system's root: for an archive, the
    /// folder holding it; for search results, where the search started.
    fn exit_to(&self) -> Option<VPath> {
        self.container().and_then(|c| c.parent())
    }

    /// A short status for the panel's bottom line (e.g. "searching…").
    fn status_text(&self) -> Option<String> {
        None
    }

    /// Make sure an online-only file is available for reading without
    /// blocking longer than `timeout`. Errors with FetchTimeout / Cancelled;
    /// the fetch may go on in the background. Default: nothing to do.
    fn fetch_online_only(
        &self,
        _path: &str,
        _timeout: std::time::Duration,
        _is_cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        Ok(())
    }

    fn free_space(&self, _path: &str) -> Option<u64> {
        None
    }

    // -- writing (only for writable file systems) ------------------------------

    fn mkdir(&self, _path: &str, _exist_ok: bool) -> Result<()> {
        Err(VfsError::ReadOnly(self.read_only_reason()))
    }

    /// Delete permanently (files or whole directories).
    fn remove(&self, _path: &str) -> Result<()> {
        Err(VfsError::ReadOnly(self.read_only_reason()))
    }

    fn trash(&self, _paths: &[String]) -> Result<()> {
        Err(VfsError::ReadOnly(self.read_only_reason()))
    }

    fn rename(&self, _src: &str, _dst: &str) -> Result<()> {
        Err(VfsError::ReadOnly(self.read_only_reason()))
    }

    fn open_write(&self, _path: &str) -> Result<Box<dyn WriteStream>> {
        Err(VfsError::ReadOnly(self.read_only_reason()))
    }

    fn set_mtime(&self, _path: &str, _mtime: f64) -> Result<()> {
        Ok(())
    }

    /// Release connections etc.
    fn close(&self) {}
}

// ---------------------------------------------------------------------------
// VPath
// ---------------------------------------------------------------------------

/// A file system plus a path in it — the only path type above this layer.
#[derive(Clone)]
pub struct VPath {
    pub fs: Arc<dyn FileSystem>,
    pub path: String,
}

impl VPath {
    pub fn new(fs: Arc<dyn FileSystem>, path: impl Into<String>) -> Self {
        VPath { fs, path: path.into() }
    }

    pub fn same_fs(&self, other: &VPath) -> bool {
        Arc::ptr_eq(&self.fs, &other.fs) || self.fs.identity() == other.fs.identity()
    }

    /// An archive's root is named after the archive.
    pub fn name(&self) -> String {
        if self.fs.parent_path(&self.path).is_none() {
            if let Some(c) = self.fs.container() {
                return c.name();
            }
        }
        self.fs.basename(&self.path)
    }

    /// The containing directory; from an archive's root, the directory holding the archive.
    pub fn parent(&self) -> Option<VPath> {
        match self.fs.parent_path(&self.path) {
            Some(p) => Some(VPath::new(self.fs.clone(), p)),
            None => self.fs.exit_to(),
        }
    }

    /// The real location this stands for (see [`FileSystem::real_location`]), or itself.
    pub fn resolved(&self) -> VPath {
        self.fs.real_location(&self.path).unwrap_or_else(|| self.clone())
    }

    pub fn child(&self, names: &[&str]) -> VPath {
        VPath::new(self.fs.clone(), self.fs.join(&self.path, names))
    }

    pub fn display(&self) -> String {
        self.fs.display(&self.path)
    }

    pub fn stat(&self) -> Result<Entry> {
        self.fs.stat(&self.path)
    }

    pub fn exists(&self) -> bool {
        self.fs.exists(&self.path)
    }

    pub fn is_dir(&self) -> bool {
        self.fs.is_dir(&self.path)
    }

    pub fn list_dir(&self, opts: ListOptions) -> Result<Vec<Entry>> {
        self.fs.list_dir(&self.path, opts)
    }

    pub fn local_path(&self) -> Option<PathBuf> {
        self.fs.local_path(&self.path)
    }

    pub fn open_read(&self) -> Result<Box<dyn ReadStream>> {
        self.fs.open_read(&self.path)
    }
}

impl PartialEq for VPath {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.same_fs(other)
    }
}

impl Eq for VPath {}

impl std::hash::Hash for VPath {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.fs.identity().hash(state);
        self.path.hash(state);
    }
}

impl fmt::Debug for VPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VPath({}: {})", self.fs.type_name(), self.path)
    }
}

impl fmt::Display for VPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_rules() {
        assert_eq!(ext_of("a.TXT"), "txt");
        assert_eq!(ext_of(".bashrc"), "");
        assert_eq!(ext_of("noext"), "");
        assert_eq!(ext_of("a.tar.gz"), "gz");
        assert_eq!(Entry::dir("x.d", 0.0).ext(), "");
    }

    #[test]
    fn sorting_dirs_first_and_reverse_per_group() {
        let es = vec![
            Entry::file("b.txt", 3, 0.0),
            Entry::dir("Zdir", 0.0),
            Entry::file("A.md", 10, 0.0),
            Entry::dir("adir", 0.0),
            Entry::file(".hidden", 1, 0.0),
        ];
        let names = |v: Vec<Entry>| v.into_iter().map(|e| e.name).collect::<Vec<_>>();
        assert_eq!(names(sort_entries(es.clone(), ListOptions::default())), ["adir", "Zdir", "A.md", "b.txt"]);
        let rev = ListOptions { reverse: true, sort_key: SortKey::Size, show_hidden: true };
        assert_eq!(names(sort_entries(es, rev)), ["Zdir", "adir", "A.md", "b.txt", ".hidden"]);
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_size(247), "247 B");
        assert_eq!(human_size(1229), "1.2 K");
        assert_eq!(human_size(412 * 1024 * 1024 * 1024), "412 G");
    }
}
