//! The format-independent part of an opened archive: the member index
//! (with implicit directories), listing, walking, and extraction.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use oc_vfs::{Entry, ListOptions, ReadStream, Result, VfsError, sort_entries};

use crate::io::TempFileStream;
use crate::{ArchiveError, ArchiveKind, basename, cache_dir, dirname, file_mtime};

/// One file or directory in an archive.
#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    /// Normalized POSIX path, no leading/trailing slash.
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: f64,
    pub is_link: bool,
}

impl Member {
    pub(crate) fn new(name: String, is_dir: bool, size: u64, mtime: f64) -> Self {
        Member {
            name,
            is_dir,
            size,
            mtime,
            is_link: false,
        }
    }
}

/// Where a member's data lives, in format terms.
#[derive(Clone, Debug)]
pub(crate) enum Source {
    /// Implicit directories (and anything else without data).
    None,
    /// Index in the zip central directory.
    Zip(usize),
    Tar(crate::tar::TarSource),
    /// The one member of a single compressed file.
    Single,
    /// Index into the 7z file list.
    SevenZ(usize),
    /// Matched by normalized name while scanning the RAR headers.
    #[cfg(feature = "rar")]
    Rar,
}

/// What a format module hands back after listing an archive.
pub(crate) struct Listing {
    pub(crate) format: Box<dyn Format>,
    pub(crate) members: Vec<(Member, Source)>,
}

/// Format-specific reading. Implementations are thread-safe and give every
/// call its own stream.
pub(crate) trait Format: Send + Sync {
    /// Stream file member `idx`.
    fn open(&self, archive: &Archive, idx: usize) -> Result<Box<dyn ReadStream>>;

    /// Write file member `idx` to `out` (a fresh file). Formats that can't
    /// stream a member (7z, rar) override this and [`Format::open`] goes
    /// through a temp file instead.
    fn decode_to(
        &self,
        archive: &Archive,
        idx: usize,
        out: &Path,
        on_chunk: OnChunk<'_>,
    ) -> Result<()> {
        let mut src = self.open(archive, idx)?;
        let mut dst = File::create(out)?;
        copy_chunked(&mut src, &mut dst, on_chunk)
    }

    /// Release the file handle (the archive stays usable and reopens it).
    fn close(&self) {}
}

/// Progress hook for extraction: called with each chunk's size; an error
/// (e.g. [`VfsError::Cancelled`]) aborts.
pub type OnChunk<'a> = Option<&'a mut dyn FnMut(u64) -> Result<()>>;

pub(crate) fn copy_chunked(
    src: &mut dyn Read,
    dst: &mut dyn Write,
    mut on_chunk: OnChunk<'_>,
) -> Result<()> {
    let mut buf = vec![0u8; oc_vfs::CHUNK];
    loop {
        let n = match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        dst.write_all(&buf[..n])?;
        if let Some(cb) = on_chunk.as_mut() {
            cb(n as u64)?;
        }
    }
    dst.flush()?;
    Ok(())
}

/// An opened archive with an index of its members.
pub struct Archive {
    path: PathBuf,
    kind: ArchiveKind,
    mtime: f64,
    members: Vec<Member>,
    sources: Vec<Source>,
    index: HashMap<String, usize>,
    /// Children of each member (by index), in archive order.
    children: Vec<Vec<usize>>,
    root: Vec<usize>,
    format: Box<dyn Format>,
}

impl std::fmt::Debug for Archive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Archive")
            .field("path", &self.path)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl Archive {
    /// Open an archive (uncached; see [`crate::open_archive_file`]). The kind
    /// comes from the file name.
    pub fn open(path: &Path) -> std::result::Result<Archive, ArchiveError> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        let kind = crate::archive_type(&name)
            .ok_or_else(|| ArchiveError(format!("Not a supported archive: {name}")))?;
        Self::open_kind(path, kind)
    }

    pub(crate) fn open_kind(
        path: &Path,
        kind: ArchiveKind,
    ) -> std::result::Result<Archive, ArchiveError> {
        let display_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let meta = std::fs::metadata(path)
            .map_err(|e| ArchiveError(format!("Cannot read {display_name}: {e}")))?;
        let listing = match kind {
            ArchiveKind::Zip => crate::zip::list(path),
            ArchiveKind::Tar => crate::tar::list(path),
            ArchiveKind::SevenZ => crate::sevenz::list(path),
            #[cfg(feature = "rar")]
            ArchiveKind::Rar => crate::rar::list(path),
            #[cfg(not(feature = "rar"))]
            ArchiveKind::Rar => Err(ListError::User(format!(
                "RAR support is not included in this build: {display_name}"
            ))),
            ArchiveKind::Gz | ArchiveKind::Bz2 | ArchiveKind::Xz => {
                crate::single::list(path, kind, &meta)
            }
        };
        // Corrupt/encrypted/unsupported: one error type for the UI. Messages
        // that already are user-facing ("Encrypted … not supported yet") pass as-is.
        let listing = listing.map_err(|e| match e {
            ListError::User(msg) => ArchiveError(msg),
            ListError::Other(msg) => ArchiveError(format!("Cannot read {display_name}: {msg}")),
        })?;
        let mut archive = Archive {
            path: path.to_path_buf(),
            kind,
            mtime: file_mtime(&meta),
            members: Vec::new(),
            sources: Vec::new(),
            index: HashMap::new(),
            children: Vec::new(),
            root: Vec::new(),
            format: listing.format,
        };
        archive.build_index(listing.members);
        Ok(archive)
    }

    fn build_index(&mut self, raw: Vec<(Member, Source)>) {
        // Children are keyed by name while building: a child can come before
        // its (implicit or explicit) parent.
        let mut children: HashMap<String, Vec<usize>> = HashMap::new();
        let mtime = self.mtime;
        for (m, src) in raw {
            let mut parent = dirname(&m.name).to_owned();
            self.add(m, src, &mut children);
            while !parent.is_empty() && !self.index.contains_key(&parent) {
                let up = dirname(&parent).to_owned();
                self.add(
                    Member::new(parent, true, 0, mtime),
                    Source::None,
                    &mut children,
                );
                parent = up;
            }
        }
        self.children = self
            .members
            .iter()
            .map(|m| children.remove(&m.name).unwrap_or_default())
            .collect();
        self.root = children.remove("").unwrap_or_default();
    }

    fn add(&mut self, m: Member, src: Source, children: &mut HashMap<String, Vec<usize>>) {
        if m.name.is_empty() {
            return;
        }
        if let Some(&i) = self.index.get(&m.name) {
            // The first file wins; a directory entry replaces what's there
            // (e.g. an implicit directory, with the real mtime).
            if m.is_dir {
                self.members[i] = m;
                self.sources[i] = src;
            }
            return;
        }
        let i = self.members.len();
        children
            .entry(dirname(&m.name).to_owned())
            .or_default()
            .push(i);
        self.index.insert(m.name.clone(), i);
        self.members.push(m);
        self.sources.push(src);
    }

    // -- accessors -------------------------------------------------------------

    /// The archive file on the local disk.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn kind(&self) -> ArchiveKind {
        self.kind
    }

    /// "zip", "tar", "gz", "7z", …
    pub fn type_name(&self) -> &'static str {
        self.kind.as_str()
    }

    /// The archive file's mtime when it was opened.
    pub fn mtime(&self) -> f64 {
        self.mtime
    }

    pub fn member(&self, name: &str) -> Option<&Member> {
        self.index.get(name).map(|&i| &self.members[i])
    }

    /// All members (explicit and implicit), in archive order.
    pub fn members(&self) -> impl Iterator<Item = &Member> {
        self.members.iter()
    }

    pub(crate) fn source(&self, idx: usize) -> &Source {
        &self.sources[idx]
    }

    pub(crate) fn member_at(&self, idx: usize) -> &Member {
        &self.members[idx]
    }

    pub(crate) fn member_name(&self, idx: usize) -> &str {
        &self.members[idx].name
    }

    pub(crate) fn index_of(&self, name: &str) -> Option<usize> {
        self.index.get(name).copied()
    }

    pub fn is_dir(&self, inner: &str) -> bool {
        inner.is_empty() || self.member(inner).is_some_and(|m| m.is_dir)
    }

    pub fn entry(&self, m: &Member) -> Entry {
        let name = basename(&m.name);
        Entry {
            name: name.to_owned(),
            is_dir: m.is_dir,
            is_link: m.is_link,
            is_hidden: name.starts_with('.'),
            is_executable: false,
            size: m.size,
            mtime: m.mtime,
            is_online_only: false,
        }
    }

    fn children_of(&self, inner: &str) -> &[usize] {
        if inner.is_empty() {
            return &self.root;
        }
        self.index_of(inner).map_or(&[], |i| &self.children[i])
    }

    /// Entries of directory `inner`, filtered and sorted. Errors with
    /// `NotADirectory` for files and missing paths.
    pub fn list_dir(&self, inner: &str, opts: ListOptions) -> Result<Vec<Entry>> {
        if !self.is_dir(inner) {
            let name = self
                .path
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            return Err(VfsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                format!("Not a directory in {name}: {inner}"),
            )));
        }
        let entries = self
            .children_of(inner)
            .iter()
            .map(|&i| self.entry(&self.members[i]))
            .collect();
        Ok(sort_entries(entries, opts))
    }

    /// `inner` itself (unless it's the root) and everything below it,
    /// parents before children, in archive order.
    pub fn walk<'a>(&'a self, inner: &str) -> Walk<'a> {
        let (first, start) = if inner.is_empty() {
            (None, &self.root[..])
        } else {
            match self.index_of(inner) {
                Some(i) => (Some(i), &self.children[i][..]),
                None => (None, &[][..]),
            }
        };
        Walk {
            archive: self,
            first,
            stack: vec![start.iter()],
        }
    }

    // -- reading ---------------------------------------------------------------

    fn file_index(&self, name: &str) -> Result<usize> {
        let i = self
            .index_of(name)
            .ok_or_else(|| VfsError::NotFound(name.to_owned()))?;
        if self.members[i].is_dir {
            return Err(VfsError::Io(std::io::Error::new(
                std::io::ErrorKind::IsADirectory,
                format!("Is a directory: {name}"),
            )));
        }
        Ok(i)
    }

    /// Stream file member `name`. Each call gets an independent stream, so
    /// any number of threads can read at once.
    pub fn open_member(&self, name: &str) -> Result<Box<dyn ReadStream>> {
        let i = self.file_index(name)?;
        self.format.open(self, i)
    }

    /// Write file member `name` to `dst` (replacing it). `on_chunk` gets each
    /// chunk's size and may return an error (e.g. Cancelled) to abort; no
    /// partial file is left behind either way.
    pub fn extract_file(&self, name: &str, dst: &Path, on_chunk: OnChunk<'_>) -> Result<()> {
        let i = self.file_index(name)?;
        let parent = match dst.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        // Decode next to the target and rename into place, so readers never
        // see a half-written file. A private directory (not "dst.part") keeps
        // concurrent extractions of the same member apart.
        let tmp = tempfile::Builder::new()
            .prefix(".oc-extract-")
            .tempdir_in(parent)?;
        let part = tmp.path().join("part");
        self.format.decode_to(self, i, &part, on_chunk)?;
        // Best effort, like a copy's metadata: an mtime the platform can't
        // represent (tar allows huge ones) must not fail the extraction.
        if let Some(t) = system_time(self.members[i].mtime) {
            let _ = File::options()
                .write(true)
                .open(&part)
                .and_then(|f| f.set_modified(t));
        }
        std::fs::rename(&part, dst)?;
        Ok(())
    }

    /// Decode member `idx` into a temp file and stream that; for formats that
    /// can only extract whole members.
    pub(crate) fn open_via_temp(&self, idx: usize) -> Result<Box<dyn ReadStream>> {
        let dir = tempfile::Builder::new()
            .prefix("member-")
            .tempdir_in(cache_dir()?)?;
        let path = dir.path().join("data");
        self.format.decode_to(self, idx, &path, None)?;
        let file = File::open(&path)?;
        Ok(Box::new(TempFileStream::new(file, Some(dir))?))
    }

    /// Release the file handle; the archive stays usable and reopens it when
    /// read. Streams already open keep working.
    pub fn close(&self) {
        self.format.close();
    }
}

/// An mtime as a `SystemTime`; None for "unknown" (0) and for values out of
/// range (tar headers can hold anything; `UNIX_EPOCH + d` would panic).
fn system_time(mtime: f64) -> Option<std::time::SystemTime> {
    if mtime == 0.0 || !mtime.is_finite() {
        return None;
    }
    let d = std::time::Duration::try_from_secs_f64(mtime.abs()).ok()?;
    if mtime > 0.0 {
        std::time::UNIX_EPOCH.checked_add(d)
    } else {
        std::time::UNIX_EPOCH.checked_sub(d)
    }
}

/// Iterator returned by [`Archive::walk`]: depth-first without recursion.
pub struct Walk<'a> {
    archive: &'a Archive,
    first: Option<usize>,
    stack: Vec<std::slice::Iter<'a, usize>>,
}

impl<'a> Iterator for Walk<'a> {
    type Item = &'a Member;

    fn next(&mut self) -> Option<&'a Member> {
        if let Some(i) = self.first.take() {
            return Some(&self.archive.members[i]);
        }
        loop {
            let top = self.stack.last_mut()?;
            match top.next() {
                Some(&i) => {
                    let m = &self.archive.members[i];
                    if m.is_dir {
                        self.stack.push(self.archive.children[i].iter());
                    }
                    return Some(m);
                }
                None => {
                    self.stack.pop();
                }
            }
        }
    }
}

/// Listing failures: `User` messages are shown as they are, `Other` ones get
/// "Cannot read <archive>: " in front.
#[derive(Debug)]
pub(crate) enum ListError {
    User(String),
    Other(String),
}

impl<E: std::fmt::Display> From<E> for ListError {
    fn from(e: E) -> Self {
        ListError::Other(e.to_string())
    }
}
