//! [`ArchiveFS`]: an archive as a read-only [`FileSystem`], living in a file
//! on any other file system (so archives nest).

use std::io::Write;
use std::path::{MAIN_SEPARATOR, PathBuf};
use std::sync::Arc;

use oc_vfs::{Entry, FileSystem, ListOptions, ReadStream, RelPath, Result, VPath, VfsError};

use crate::archive::Archive;
use crate::{
    ArchiveError, basename, cache_dir, cache_key, dirname, extract_cached, is_archive_name,
    open_archive_file,
};

/// Read-only view of an archive. Two instances on the same archive file in
/// the same container are the same file system ([`FileSystem::identity`]).
pub struct ArchiveFS {
    archive: Arc<Archive>,
    container: VPath,
    identity: String,
}

impl ArchiveFS {
    pub fn new(archive: Arc<Archive>, container: VPath) -> Self {
        // Keyed by what the archive *is* (container location, and the local
        // file's mtime/size) rather than by object, so a reopened archive
        // after cache eviction still counts as the same file system.
        let identity = format!(
            "archive\0{}\0{}\0{}\0{}",
            container.fs.identity(),
            container.path,
            archive.path().display(),
            archive.mtime().to_bits()
        );
        ArchiveFS {
            archive,
            container,
            identity,
        }
    }

    pub fn archive(&self) -> &Arc<Archive> {
        &self.archive
    }

    /// A real file with this member's contents, extracted to the temp cache
    /// once. `oc_vfs::ops::materialize` gets here through
    /// [`FileSystem::materialize`].
    pub fn materialize(&self, path: &str) -> Result<PathBuf> {
        extract_cached(&self.archive, path)
    }
}

impl FileSystem for ArchiveFS {
    fn type_name(&self) -> String {
        format!("{} archive", self.archive.type_name())
    }

    fn read_only_reason(&self) -> String {
        format!("{} is an archive (read-only)", self.container.name())
    }

    fn container(&self) -> Option<VPath> {
        Some(self.container.clone())
    }

    fn identity(&self) -> String {
        self.identity.clone()
    }

    /// Members come from the extraction cache (one extraction per member,
    /// shared by previews and "open with"), not `cache_root`.
    fn materialize(&self, path: &str, _cache_root: &std::path::Path) -> Option<Result<PathBuf>> {
        if self.archive.is_dir(path) {
            return None; // ops::materialize reports directories itself
        }
        Some(extract_cached(&self.archive, path))
    }

    fn list_dir(&self, path: &str, opts: ListOptions) -> Result<Vec<Entry>> {
        self.archive.list_dir(path, opts)
    }

    fn stat(&self, path: &str) -> Result<Entry> {
        if path.is_empty() {
            return Ok(Entry {
                is_hidden: false,
                ..Entry::dir(self.container.name(), self.archive.mtime())
            });
        }
        match self.archive.member(path) {
            Some(m) => Ok(self.archive.entry(m)),
            None => Err(VfsError::NotFound(path.to_owned())),
        }
    }

    fn exists(&self, path: &str) -> bool {
        path.is_empty() || self.archive.member(path).is_some()
    }

    fn is_dir(&self, path: &str) -> bool {
        self.archive.is_dir(path)
    }

    fn parent_path(&self, path: &str) -> Option<String> {
        (!path.is_empty()).then(|| dirname(path).to_owned())
    }

    fn join(&self, path: &str, names: &[&str]) -> String {
        let mut out =
            String::with_capacity(path.len() + names.iter().map(|n| n.len() + 1).sum::<usize>());
        for p in std::iter::once(&path)
            .chain(names)
            .filter(|p| !p.is_empty())
        {
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(p);
        }
        out
    }

    fn basename(&self, path: &str) -> String {
        basename(path).to_owned()
    }

    fn display(&self, path: &str) -> String {
        let mut out = self.container.display();
        if path.is_empty() {
            return out;
        }
        for part in path.split('/') {
            if !out.ends_with(MAIN_SEPARATOR) {
                out.push(MAIN_SEPARATOR);
            }
            out.push_str(part);
        }
        out
    }

    fn open_read(&self, path: &str) -> Result<Box<dyn ReadStream>> {
        self.archive.open_member(path)
    }

    fn walk<'a>(
        &'a self,
        path: &str,
    ) -> Result<Box<dyn Iterator<Item = Result<(RelPath, Entry)>> + 'a>> {
        let root = self.stat(path)?;
        let skip = if path.is_empty() { 0 } else { path.len() + 1 };
        let path = path.to_owned();
        let below = self
            .archive
            .walk(&path)
            .filter(move |m| m.name != path)
            .map(move |m| {
                let rel = m.name[skip..].split('/').map(str::to_owned).collect();
                Ok((rel, self.archive.entry(m)))
            });
        Ok(Box::new(
            std::iter::once(Ok((Vec::new(), root))).chain(below),
        ))
    }
}

/// Enter archive `file` (on any file system); returns the archive's root.
///
/// A file without a local path (inside another archive, on a server) is
/// copied to the temp cache first.
pub fn open_archive(file: &VPath) -> std::result::Result<VPath, ArchiveError> {
    let local = match file.local_path() {
        Some(p) => p,
        None => materialize(file).map_err(|e| ArchiveError(e.to_string()))?,
    };
    let archive = open_archive_file(&local)?;
    Ok(VPath::new(
        Arc::new(ArchiveFS::new(archive, file.clone())),
        "",
    ))
}

/// Whether `path` names an archive we can enter (by name; directories called
/// "x.zip" don't count).
pub fn is_archive(path: &VPath) -> bool {
    is_archive_name(&path.name()) && !path.is_dir()
}

/// A local copy of `file` in the temp cache, keyed by where it is and its
/// size/mtime, and named like the original so its suffix still identifies it.
fn materialize(file: &VPath) -> Result<PathBuf> {
    let entry = file.stat()?;
    let key = cache_key((file.display(), entry.size, entry.mtime.to_bits()));
    let target = cache_dir()?.join("vfs").join(key).join(file.name());
    if target.exists() {
        return Ok(target);
    }
    let dir = target.parent().expect("has a parent");
    std::fs::create_dir_all(dir)?;
    let mut part = tempfile::NamedTempFile::new_in(dir)?;
    let mut src = file.open_read()?;
    crate::archive::copy_chunked(&mut src, part.as_file_mut(), None)?;
    part.as_file_mut().flush()?;
    part.persist(&target).map_err(|e| VfsError::Io(e.error))?;
    Ok(target)
}
