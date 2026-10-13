//! Test file systems: a minimal local disk, an in-memory one (standing in for
//! servers and cloud folders) and a fake archive format.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use oc_search::{ArchiveOpener, Hit, Query, SearchProgress};
use oc_vfs::{Entry, FileSystem, ListOptions, ReadStream, RelPath, Result, VPath, VfsError, sort_entries};

// ---------------------------------------------------------------------------
// Local disk
// ---------------------------------------------------------------------------

/// A plain local file system. With `expose_local_path` false the search can't
/// see that it's local and has to use the generic walkers.
pub struct LocalFs {
    pub expose_local_path: bool,
}

pub fn local_entry(path: &Path) -> io::Result<Entry> {
    let lmeta = fs::symlink_metadata(path)?;
    let is_link = lmeta.file_type().is_symlink();
    let meta = fs::metadata(path).unwrap_or(lmeta);
    let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64());
    let mut e = if meta.is_dir() { Entry::dir(name, mtime) } else { Entry::file(name, meta.len(), mtime) };
    e.is_link = is_link;
    Ok(e)
}

impl FileSystem for LocalFs {
    fn type_name(&self) -> String {
        "local disk".into()
    }

    fn identity(&self) -> String {
        format!("local:{}", self.expose_local_path)
    }

    fn list_dir(&self, path: &str, opts: ListOptions) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        for de in fs::read_dir(path)? {
            out.push(local_entry(&de?.path())?);
        }
        Ok(sort_entries(out, opts))
    }

    fn stat(&self, path: &str) -> Result<Entry> {
        local_entry(Path::new(path)).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound { VfsError::NotFound(path.into()) } else { e.into() }
        })
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
        Path::new(path).file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    }

    fn display(&self, path: &str) -> String {
        path.to_string()
    }

    fn open_read(&self, path: &str) -> Result<Box<dyn ReadStream>> {
        Ok(Box::new(fs::File::open(path)?))
    }

    fn walk<'a>(&'a self, path: &str) -> Result<Box<dyn Iterator<Item = Result<(RelPath, Entry)>> + 'a>> {
        let mut out = vec![Ok((Vec::new(), self.stat(path)?))];
        fn rec(dir: &Path, rel: &RelPath, out: &mut Vec<Result<(RelPath, Entry)>>) {
            let rd = match fs::read_dir(dir) {
                Ok(rd) => rd,
                Err(e) => return out.push(Err(e.into())),
            };
            let mut paths: Vec<PathBuf> = rd.filter_map(|d| d.ok().map(|d| d.path())).collect();
            paths.sort();
            for p in paths {
                let Ok(e) = local_entry(&p) else { continue };
                let mut r = rel.clone();
                r.push(e.name.clone());
                let descend = e.is_dir && !e.is_link;
                out.push(Ok((r.clone(), e)));
                if descend {
                    rec(&p, &r, out);
                }
            }
        }
        rec(Path::new(path), &Vec::new(), &mut out);
        Ok(Box::new(out.into_iter()))
    }

    fn local_path(&self, path: &str) -> Option<PathBuf> {
        self.expose_local_path.then(|| PathBuf::from(path))
    }
}

pub fn local(path: &Path) -> VPath {
    VPath::new(Arc::new(LocalFs { expose_local_path: true }), path.to_string_lossy().into_owned())
}

/// The same tree, but searched through the generic (non-local) walkers.
pub fn opaque(path: &Path) -> VPath {
    VPath::new(Arc::new(LocalFs { expose_local_path: false }), path.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// An in-memory file system ("" is the root, POSIX paths). With
/// `online_only` every file is a cloud placeholder; `bytes_read` counts
/// what reading would have downloaded.
#[derive(Default)]
pub struct MemFs {
    pub files: Mutex<BTreeMap<String, Vec<u8>>>,
    pub dirs: Mutex<BTreeSet<String>>,
    pub online_only: bool,
    pub bytes_read: Arc<AtomicU64>,
    pub container: Option<VPath>,
    pub list_calls: AtomicU64,
}

struct Counting {
    inner: Cursor<Vec<u8>>,
    counter: Arc<AtomicU64>,
}

impl Read for Counting {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.counter.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

impl Seek for Counting {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl ReadStream for Counting {}

fn parent_of(p: &str) -> &str {
    p.rfind('/').map_or("", |i| &p[..i])
}

impl MemFs {
    pub fn new() -> Self {
        let m = MemFs::default();
        m.dirs.lock().unwrap().insert(String::new());
        m
    }

    pub fn mkdir_p(&self, path: &str) {
        let mut dirs = self.dirs.lock().unwrap();
        let mut p = path.to_string();
        while !p.is_empty() {
            dirs.insert(p.clone());
            p = parent_of(&p).to_string();
        }
    }

    pub fn put(&self, path: &str, data: &[u8]) {
        self.mkdir_p(parent_of(path));
        self.files.lock().unwrap().insert(path.to_string(), data.to_vec());
    }

    fn all_paths(&self) -> BTreeSet<String> {
        let mut all: BTreeSet<String> = self.files.lock().unwrap().keys().cloned().collect();
        all.extend(self.dirs.lock().unwrap().iter().cloned());
        all
    }
}

impl FileSystem for MemFs {
    fn type_name(&self) -> String {
        if self.container.is_some() { "fake archive".into() } else { "memory".into() }
    }

    fn container(&self) -> Option<VPath> {
        self.container.clone()
    }

    fn list_dir(&self, path: &str, opts: ListOptions) -> Result<Vec<Entry>> {
        self.list_calls.fetch_add(1, Ordering::Relaxed);
        if !self.dirs.lock().unwrap().contains(path) {
            return Err(VfsError::NotFound(path.into()));
        }
        let entries = self
            .all_paths()
            .into_iter()
            .filter(|p| !p.is_empty() && parent_of(p) == path)
            .map(|p| self.stat(&p))
            .collect::<Result<Vec<_>>>()?;
        Ok(sort_entries(entries, opts))
    }

    fn stat(&self, path: &str) -> Result<Entry> {
        let name = self.basename(path);
        if self.dirs.lock().unwrap().contains(path) {
            return Ok(Entry::dir(name, 0.0));
        }
        match self.files.lock().unwrap().get(path) {
            Some(data) => Ok(Entry { is_online_only: self.online_only, ..Entry::file(name, data.len() as u64, 0.0) }),
            None => Err(VfsError::NotFound(path.into())),
        }
    }

    fn parent_path(&self, path: &str) -> Option<String> {
        if path.is_empty() { None } else { Some(parent_of(path).to_string()) }
    }

    fn join(&self, path: &str, names: &[&str]) -> String {
        std::iter::once(path).chain(names.iter().copied()).filter(|p| !p.is_empty()).collect::<Vec<_>>().join("/")
    }

    fn basename(&self, path: &str) -> String {
        path.rsplit('/').next().unwrap_or("").to_string()
    }

    fn display(&self, path: &str) -> String {
        format!("mem:/{path}")
    }

    fn open_read(&self, path: &str) -> Result<Box<dyn ReadStream>> {
        let data = self.files.lock().unwrap().get(path).cloned().ok_or_else(|| VfsError::NotFound(path.into()))?;
        Ok(Box::new(Counting { inner: Cursor::new(data), counter: self.bytes_read.clone() }))
    }

    fn walk<'a>(&'a self, path: &str) -> Result<Box<dyn Iterator<Item = Result<(RelPath, Entry)>> + 'a>> {
        let mut out = vec![Ok((Vec::new(), self.stat(path)?))];
        let prefix = if path.is_empty() { String::new() } else { format!("{path}/") };
        for p in self.all_paths() {
            if p != path && !p.is_empty() && p.starts_with(&prefix) {
                let rel = p[prefix.len()..].split('/').map(str::to_string).collect();
                out.push(self.stat(&p).map(|e| (rel, e)));
            }
        }
        Ok(Box::new(out.into_iter()))
    }

    fn remove(&self, path: &str) -> Result<()> {
        let prefix = format!("{path}/");
        self.files.lock().unwrap().retain(|p, _| p != path && !p.starts_with(&prefix));
        self.dirs.lock().unwrap().retain(|p| p != path && !p.starts_with(&prefix));
        Ok(())
    }

    fn rename(&self, src: &str, dst: &str) -> Result<()> {
        let prefix = format!("{src}/");
        let moved = |p: &str| -> Option<String> {
            if p == src {
                Some(dst.to_string())
            } else {
                p.strip_prefix(&prefix).map(|rest| format!("{dst}/{rest}"))
            }
        };
        let mut files = self.files.lock().unwrap();
        *files = std::mem::take(&mut *files).into_iter().map(|(p, d)| (moved(&p).unwrap_or(p), d)).collect();
        let mut dirs = self.dirs.lock().unwrap();
        *dirs = std::mem::take(&mut *dirs).into_iter().map(|p| moved(&p).unwrap_or(p)).collect();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fake archives
// ---------------------------------------------------------------------------

/// "*.fakezip" files hold hex-encoded "member\tcontent" lines (hex, so the
/// members' text isn't in the raw bytes, like a compressed ZIP).
pub struct FakeZip;

pub fn fakezip(members: &[(&str, &str)]) -> Vec<u8> {
    let mut plain = String::new();
    for (name, content) in members {
        plain.push_str(&format!("{name}\t{content}\n"));
    }
    plain.bytes().map(|b| format!("{b:02x}")).collect::<String>().into_bytes()
}

impl ArchiveOpener for FakeZip {
    fn is_archive(&self, name: &str) -> bool {
        name.to_lowercase().ends_with(".fakezip")
    }

    fn open(&self, file: &VPath) -> Result<VPath> {
        let mut hex = String::new();
        file.open_read()?.read_to_string(&mut hex)?;
        let bytes: Option<Vec<u8>> = (0..hex.len())
            .step_by(2)
            .map(|i| hex.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
            .collect();
        let plain = bytes
            .and_then(|b| String::from_utf8(b).ok())
            .ok_or_else(|| VfsError::Operation("not a fake archive".into()))?;
        let mem = MemFs { container: Some(file.clone()), ..MemFs::new() };
        for line in plain.lines() {
            let (name, content) = line.split_once('\t').unwrap_or((line, ""));
            mem.put(name, content.as_bytes());
        }
        Ok(VPath::new(Arc::new(mem), ""))
    }
}

// ---------------------------------------------------------------------------
// Running searches
// ---------------------------------------------------------------------------

pub fn collect(query: &Query) -> (Vec<Hit>, SearchProgress) {
    let progress = SearchProgress::new();
    let mut hits = Vec::new();
    oc_search::run(query, |h| hits.push(h), &progress, || false).unwrap();
    (hits, progress)
}

/// Sorted relative paths with "/" separators.
pub fn rels(hits: &[Hit]) -> Vec<String> {
    let mut v: Vec<String> = hits.iter().map(|h| h.relative.replace(std::path::MAIN_SEPARATOR, "/")).collect();
    v.sort();
    v
}

pub fn find(root: VPath, configure: impl FnOnce(&mut Query)) -> Vec<String> {
    let mut q = Query::new(vec![root]);
    q.archive_opener = Some(Arc::new(FakeZip));
    configure(&mut q);
    rels(&collect(&q).0)
}
