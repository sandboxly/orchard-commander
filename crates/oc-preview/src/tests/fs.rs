//! Test file systems: in-memory (non-local, counts bytes read, optional cloud
//! placeholders) and a minimal local one over a temp dir.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oc_vfs::{Entry, FileSystem, ListOptions, ReadStream, RelPath, Result, VPath, VfsError, sort_entries};

/// Contents of a memory file: real bytes, or `size` bytes of `fill` generated on read.
#[derive(Clone)]
pub enum Content {
    Bytes(Arc<Vec<u8>>),
    Generated { size: u64, fill: u8 },
}

impl Content {
    fn len(&self) -> u64 {
        match self {
            Content::Bytes(b) => b.len() as u64,
            Content::Generated { size, .. } => *size,
        }
    }
}

#[derive(Default)]
pub struct MemFs {
    pub files: Mutex<BTreeMap<String, Content>>,
    pub dirs: Mutex<BTreeSet<String>>,
    pub bytes_read: Arc<AtomicU64>,
    /// Files are cloud placeholders (`is_online_only`).
    pub online_only: bool,
    /// With `online_only`: fetches wait for this (a stuck cloud app until set).
    pub deliver: Option<Arc<AtomicBool>>,
    pub fetches: AtomicUsize,
    /// Streams claim not to seek (like a pipe or compressed archive member).
    pub unseekable: bool,
}

impl MemFs {
    pub fn new() -> Self {
        MemFs::default()
    }

    pub fn add(&self, name: &str, data: impl Into<Vec<u8>>) {
        self.files.lock().unwrap().insert(name.to_string(), Content::Bytes(Arc::new(data.into())));
    }

    pub fn add_generated(&self, name: &str, size: u64, fill: u8) {
        self.files.lock().unwrap().insert(name.to_string(), Content::Generated { size, fill });
    }

    pub fn add_dir(&self, name: &str) {
        self.dirs.lock().unwrap().insert(name.to_string());
    }

    pub fn read(&self) -> u64 {
        self.bytes_read.load(Ordering::SeqCst)
    }

    pub fn vpath(self: &Arc<Self>, path: &str) -> VPath {
        VPath::new(self.clone(), path)
    }

    fn parent_of(path: &str) -> &str {
        path.rsplit_once('/').map_or("", |(p, _)| p)
    }
}

pub struct MemReader {
    content: Content,
    pos: u64,
    counter: Arc<AtomicU64>,
    seekable: bool,
}

impl Read for MemReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = self.content.len();
        if self.pos >= len {
            return Ok(0);
        }
        let n = buf.len().min((len - self.pos) as usize);
        match &self.content {
            Content::Bytes(b) => buf[..n].copy_from_slice(&b[self.pos as usize..self.pos as usize + n]),
            Content::Generated { fill, .. } => buf[..n].fill(*fill),
        }
        self.pos += n as u64;
        self.counter.fetch_add(n as u64, Ordering::SeqCst);
        Ok(n)
    }
}

impl Seek for MemReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        if !self.seekable {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "not seekable"));
        }
        let len = self.content.len() as i64;
        let new = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(d) => len + d,
            SeekFrom::Current(d) => self.pos as i64 + d,
        };
        if new < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "negative seek"));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

impl ReadStream for MemReader {
    fn seekable(&self) -> bool {
        self.seekable
    }
}

impl FileSystem for MemFs {
    fn type_name(&self) -> String {
        "test server".into()
    }

    fn list_dir(&self, path: &str, opts: ListOptions) -> Result<Vec<Entry>> {
        if !path.is_empty() && !self.dirs.lock().unwrap().contains(path) {
            return Err(VfsError::NotFound(path.into()));
        }
        let mut out = Vec::new();
        for d in self.dirs.lock().unwrap().iter() {
            if Self::parent_of(d) == path {
                out.push(Entry::dir(self.basename(d), 0.0));
            }
        }
        for (f, c) in self.files.lock().unwrap().iter() {
            if Self::parent_of(f) == path {
                out.push(Entry::file(self.basename(f), c.len(), 1_700_000_000.0));
            }
        }
        Ok(sort_entries(out, opts))
    }

    fn stat(&self, path: &str) -> Result<Entry> {
        if path.is_empty() || self.dirs.lock().unwrap().contains(path) {
            return Ok(Entry::dir(self.basename(path), 0.0));
        }
        let files = self.files.lock().unwrap();
        let c = files.get(path).ok_or_else(|| VfsError::NotFound(path.into()))?;
        let mut e = Entry::file(self.basename(path), c.len(), 1_700_000_000.0);
        e.is_online_only = self.online_only;
        Ok(e)
    }

    fn parent_path(&self, path: &str) -> Option<String> {
        if path.is_empty() { None } else { Some(Self::parent_of(path).to_string()) }
    }

    fn join(&self, path: &str, names: &[&str]) -> String {
        let mut out = path.to_string();
        for n in names {
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(n);
        }
        out
    }

    fn basename(&self, path: &str) -> String {
        path.rsplit('/').next().unwrap_or("").to_string()
    }

    fn display(&self, path: &str) -> String {
        format!("mem:/{path}")
    }

    fn open_read(&self, path: &str) -> Result<Box<dyn ReadStream>> {
        let content = self.files.lock().unwrap().get(path).cloned().ok_or_else(|| VfsError::NotFound(path.into()))?;
        Ok(Box::new(MemReader { content, pos: 0, counter: self.bytes_read.clone(), seekable: !self.unseekable }))
    }

    fn walk<'a>(&'a self, _path: &str) -> Result<Box<dyn Iterator<Item = Result<(RelPath, Entry)>> + 'a>> {
        Ok(Box::new(std::iter::empty()))
    }

    fn fetch_online_only(&self, path: &str, timeout: Duration, is_cancelled: &dyn Fn() -> bool) -> Result<()> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        let Some(deliver) = &self.deliver else { return Ok(()) };
        let deadline = Instant::now() + timeout;
        while !deliver.load(Ordering::SeqCst) {
            if is_cancelled() {
                return Err(VfsError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(VfsError::FetchTimeout(path.into()));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}

/// Absolute OS paths; just enough of a local disk for previews.
pub struct LocalFs;

impl LocalFs {
    pub fn vpath(path: &Path) -> VPath {
        VPath::new(Arc::new(LocalFs), path.to_string_lossy().into_owned())
    }
}

fn entry_of(path: &Path) -> Result<Entry> {
    let meta = std::fs::metadata(path)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64());
    Ok(if meta.is_dir() { Entry::dir(name, mtime) } else { Entry::file(name, meta.len(), mtime) })
}

impl FileSystem for LocalFs {
    fn type_name(&self) -> String {
        "local disk".into()
    }

    fn list_dir(&self, path: &str, opts: ListOptions) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        for item in std::fs::read_dir(path)? {
            out.push(entry_of(&item?.path())?);
        }
        Ok(sort_entries(out, opts))
    }

    fn stat(&self, path: &str) -> Result<Entry> {
        entry_of(Path::new(path)).map_err(|e| if e.is_not_found() { VfsError::NotFound(path.into()) } else { e })
    }

    fn parent_path(&self, path: &str) -> Option<String> {
        Path::new(path).parent().map(|p| p.to_string_lossy().into_owned())
    }

    fn join(&self, path: &str, names: &[&str]) -> String {
        let mut p = PathBuf::from(path);
        p.extend(names);
        p.to_string_lossy().into_owned()
    }

    fn basename(&self, path: &str) -> String {
        Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    fn display(&self, path: &str) -> String {
        path.to_string()
    }

    fn open_read(&self, path: &str) -> Result<Box<dyn ReadStream>> {
        Ok(Box::new(std::fs::File::open(path)?))
    }

    fn walk<'a>(&'a self, _path: &str) -> Result<Box<dyn Iterator<Item = Result<(RelPath, Entry)>> + 'a>> {
        Ok(Box::new(std::iter::empty()))
    }

    fn local_path(&self, path: &str) -> Option<PathBuf> {
        Some(PathBuf::from(path))
    }
}
