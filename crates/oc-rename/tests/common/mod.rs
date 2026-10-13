//! Test file systems: a minimal local disk (so case-insensitive disks are
//! exercised for real) and a wrapper whose renames fail on demand.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use oc_vfs::{
    Capabilities, Entry, FileSystem, ListOptions, ReadStream, RelPath, Result, VPath, VfsError, sort_entries,
};

/// Just enough of a local file system for renaming.
pub struct LocalFs;

fn entry(path: &Path) -> Result<Entry> {
    let meta = std::fs::symlink_metadata(path)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0.0, |d| d.as_secs_f64());
    Ok(if meta.is_dir() { Entry::dir(name, mtime) } else { Entry::file(name, meta.len(), mtime) })
}

impl FileSystem for LocalFs {
    fn type_name(&self) -> String {
        "local disk".into()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { writable: true, can_trash: false, can_rename: true, watchable: true }
    }

    fn identity(&self) -> String {
        "local".into()
    }

    fn list_dir(&self, path: &str, opts: ListOptions) -> Result<Vec<Entry>> {
        let entries = std::fs::read_dir(path)?.map(|e| entry(&e?.path())).collect::<Result<Vec<_>>>()?;
        Ok(sort_entries(entries, opts))
    }

    fn stat(&self, path: &str) -> Result<Entry> {
        entry(Path::new(path)).map_err(|e| if e.is_not_found() { VfsError::NotFound(path.into()) } else { e })
    }

    fn parent_path(&self, path: &str) -> Option<String> {
        Path::new(path).parent().map(|p| p.to_string_lossy().into_owned())
    }

    fn join(&self, path: &str, names: &[&str]) -> String {
        let mut p = PathBuf::from(path);
        for n in names {
            p.push(n);
        }
        p.to_string_lossy().into_owned()
    }

    fn basename(&self, path: &str) -> String {
        Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    fn display(&self, path: &str) -> String {
        path.into()
    }

    fn open_read(&self, path: &str) -> Result<Box<dyn ReadStream>> {
        Ok(Box::new(std::fs::File::open(path)?))
    }

    fn walk<'a>(&'a self, _path: &str) -> Result<Box<dyn Iterator<Item = Result<(RelPath, Entry)>> + 'a>> {
        Err(VfsError::Operation("not needed for these tests".into()))
    }

    fn local_path(&self, path: &str) -> Option<PathBuf> {
        Some(PathBuf::from(path))
    }

    /// Like the real local file system: POSIX `rename`, which replaces `dst`.
    fn rename(&self, src: &str, dst: &str) -> Result<()> {
        Ok(std::fs::rename(src, dst)?)
    }
}

pub fn local(path: &Path) -> VPath {
    VPath::new(Arc::new(LocalFs), path.to_string_lossy().into_owned())
}

/// Delegates to `inner`, but the `fail_on`-th rename (1-based) fails.
pub struct Flaky {
    pub inner: Arc<dyn FileSystem>,
    pub fail_on: usize,
    pub calls: AtomicUsize,
}

impl FileSystem for Flaky {
    fn type_name(&self) -> String {
        self.inner.type_name()
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn list_dir(&self, path: &str, opts: ListOptions) -> Result<Vec<Entry>> {
        self.inner.list_dir(path, opts)
    }
    fn stat(&self, path: &str) -> Result<Entry> {
        self.inner.stat(path)
    }
    fn parent_path(&self, path: &str) -> Option<String> {
        self.inner.parent_path(path)
    }
    fn join(&self, path: &str, names: &[&str]) -> String {
        self.inner.join(path, names)
    }
    fn basename(&self, path: &str) -> String {
        self.inner.basename(path)
    }
    fn display(&self, path: &str) -> String {
        self.inner.display(path)
    }
    fn open_read(&self, path: &str) -> Result<Box<dyn ReadStream>> {
        self.inner.open_read(path)
    }
    fn walk<'a>(&'a self, path: &str) -> Result<Box<dyn Iterator<Item = Result<(RelPath, Entry)>> + 'a>> {
        self.inner.walk(path)
    }
    fn local_path(&self, path: &str) -> Option<PathBuf> {
        self.inner.local_path(path)
    }
    fn rename(&self, src: &str, dst: &str) -> Result<()> {
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.fail_on {
            return Err(VfsError::Io(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope")));
        }
        self.inner.rename(src, dst)
    }
}

/// Names in a local folder, sorted.
pub fn names_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

pub fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}
