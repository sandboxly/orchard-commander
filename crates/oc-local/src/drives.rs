//! The folder above the drive roots on Windows: one entry per drive ("C:",
//! "D:", …), so ".." from a drive's root lists the other drives. Its root is
//! "" and each entry's path is its name; the entries stand for the drive
//! roots on the local disk (`real_location`), so entering one lists the
//! drive and operations act on it.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use oc_vfs::{Entry, FileSystem, ListOptions, ReadStream, Result, VPath, VfsError, WalkIter, sort_entries};

/// What the root is called in titles and messages.
pub const DRIVES_TITLE: &str = "Drives";

/// Every [`DrivesFS`]'s identity.
const DRIVES_IDENTITY: &str = "drives";

/// Lists the drive roots it is given ("C:\\"), by default the system's.
pub struct DrivesFS {
    roots: Box<dyn Fn() -> Vec<PathBuf> + Send + Sync>,
}

impl DrivesFS {
    /// The system's drives (none outside Windows).
    pub fn new() -> Self {
        DrivesFS { roots: Box::new(crate::system::drive_roots) }
    }

    /// These folders as the drives (tests); each is named after its path.
    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        DrivesFS { roots: Box::new(move || roots.clone()) }
    }

    /// The drives as (entry name, root): "C:" for "C:\\".
    fn drives(&self) -> Vec<(String, PathBuf)> {
        (self.roots)().into_iter().map(|r| (drive_name(&r), r)).collect()
    }

    fn root_of(&self, path: &str) -> Option<PathBuf> {
        self.drives().into_iter().find(|(n, _)| n.eq_ignore_ascii_case(path)).map(|(_, r)| r)
    }

    fn not_found(path: &str) -> VfsError {
        VfsError::NotFound(path.to_string())
    }
}

impl Default for DrivesFS {
    fn default() -> Self {
        DrivesFS::new()
    }
}

/// "C:\\" → "C:"; other folders (tests) keep their path.
fn drive_name(root: &std::path::Path) -> String {
    let s = root.to_string_lossy();
    let trimmed = s.trim_end_matches(['\\', '/']);
    if trimmed.len() == 2 && trimmed.ends_with(':') { trimmed.to_string() } else { s.into_owned() }
}

impl FileSystem for DrivesFS {
    fn type_name(&self) -> String {
        "drives".to_string()
    }

    fn read_only_reason(&self) -> String {
        "the drives list is read-only — open a drive first".to_string()
    }

    fn identity(&self) -> String {
        DRIVES_IDENTITY.to_string()
    }

    /// Drives only: their stat could hang (an empty card reader).
    fn list_dir(&self, path: &str, opts: ListOptions) -> Result<Vec<Entry>> {
        if !path.is_empty() {
            return Err(Self::not_found(path));
        }
        let entries = self.drives().into_iter().map(|(name, _)| Entry::dir(name, 0.0)).collect();
        Ok(sort_entries(entries, opts))
    }

    fn stat(&self, path: &str) -> Result<Entry> {
        if path.is_empty() {
            return Ok(Entry::dir(DRIVES_TITLE, 0.0));
        }
        self.root_of(path).map(|_| Entry::dir(path, 0.0)).ok_or_else(|| Self::not_found(path))
    }

    fn exists(&self, path: &str) -> bool {
        path.is_empty() || self.root_of(path).is_some()
    }

    fn is_dir(&self, path: &str) -> bool {
        self.exists(path)
    }

    fn parent_path(&self, path: &str) -> Option<String> {
        if path.is_empty() { None } else { Some(String::new()) }
    }

    fn join(&self, path: &str, names: &[&str]) -> String {
        names.last().map_or_else(|| path.to_string(), |n| n.to_string())
    }

    fn basename(&self, path: &str) -> String {
        if path.is_empty() { DRIVES_TITLE.to_string() } else { path.to_string() }
    }

    fn display(&self, path: &str) -> String {
        match self.root_of(path) {
            Some(root) => root.display().to_string(),
            None if path.is_empty() => DRIVES_TITLE.to_string(),
            None => path.to_string(),
        }
    }

    fn open_read(&self, path: &str) -> Result<Box<dyn ReadStream>> {
        Err(Self::not_found(path))
    }

    /// A drive is walked on the local disk; the list itself isn't walked
    /// (a search would cover every drive).
    fn walk<'a>(&'a self, path: &str) -> Result<WalkIter<'a>> {
        let Some(root) = self.root_of(path) else {
            return Err(VfsError::Operation(format!("Can't go through all drives at once — {}", self.read_only_reason())));
        };
        let local = crate::local(root);
        let items: Vec<_> = local.fs.walk(&local.path)?.collect();
        Ok(Box::new(items.into_iter()))
    }

    fn local_path(&self, path: &str) -> Option<PathBuf> {
        self.root_of(path)
    }

    fn real_location(&self, path: &str) -> Option<VPath> {
        self.root_of(path).map(crate::local)
    }

    fn status_text(&self) -> Option<String> {
        Some(String::new())
    }
}

static DRIVES: OnceLock<Arc<dyn FileSystem>> = OnceLock::new();

/// The folder above the drive roots ([`DrivesFS`] on the system's drives).
pub fn drives() -> VPath {
    VPath::new(DRIVES.get_or_init(|| Arc::new(DrivesFS::new())).clone(), "")
}

/// Whether `v` is in a drives list (its entries are whole drives).
pub fn is_drives(v: &VPath) -> bool {
    v.fs.identity() == DRIVES_IDENTITY
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_drives_that_stand_for_their_roots() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("x.txt"), b"x").unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(DrivesFS::with_roots(vec![a.path().into(), b.path().into()]));
        let root = VPath::new(fs, "");
        assert_eq!(root.name(), "Drives");
        assert_eq!(root.display(), "Drives");
        assert!(root.parent().is_none() && root.is_dir() && !root.is_local());
        let entries = root.list_dir(ListOptions::default()).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.is_dir));
        let name = drive_name(a.path());
        let drive = root.child(&[&name]);
        assert_eq!(drive.parent().unwrap(), root);
        assert_eq!(drive.resolved(), crate::local(a.path()));
        assert_eq!(drive.local_path().unwrap(), a.path());
        assert_eq!(drive.fs.walk(&drive.path).unwrap().count(), 2);
        assert!(root.fs.walk("").is_err());
        assert!(root.child(&["Q:"]).stat().unwrap_err().is_not_found());
        assert!(!root.fs.capabilities().writable);
        assert!(is_drives(&root) && !is_drives(&crate::local(a.path())));
    }

    #[test]
    fn drive_names() {
        assert_eq!(drive_name(std::path::Path::new("C:\\")), "C:");
        assert_eq!(drive_name(std::path::Path::new("d:")), "d:");
        assert_eq!(drive_name(std::path::Path::new("/tmp/x")), "/tmp/x");
    }
}
