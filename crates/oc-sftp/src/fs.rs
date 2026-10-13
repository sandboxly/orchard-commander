//! The SFTP file system. Port of `SFTPFileSystem` in `remote/sftp.py`.
//!
//! Paths are absolute POSIX paths on the server. Every method blocks the
//! calling thread (they run on workers) but never forever: each request has
//! a timeout, and a dead connection is noticed by keep-alives.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};

use futures::stream::{self, StreamExt};
use oc_vfs::{
    Capabilities, Entry, FileSystem, ListOptions, ReadStream, RelPath, Result, VfsError, WalkIter, WriteStream,
    sort_entries,
};
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::{FileAttributes, OpenFlags};

use crate::paths;
use crate::runtime::block_on;
use crate::session::{Conn, Sftp, is_eof, posix_rename};
use crate::stream::{SftpReader, SftpWriter};

/// Requests in flight at once for batches (link targets, deleting files).
const PIPELINE: usize = 64;

const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;

fn is_dir_mode(mode: u32) -> bool {
    mode & S_IFMT == S_IFDIR
}

fn is_link_mode(mode: u32) -> bool {
    mode & S_IFMT == S_IFLNK
}

/// Whether a name from a listing names a child: not ".", "..", empty or a
/// path ("a/b", "../x"), which a broken or hostile server could send to make
/// a download write outside its target folder or a recursive remove loop.
fn is_plain_name(name: &str) -> bool {
    !matches!(name, "" | "." | "..") && !name.contains('/') && !name.contains('\0')
}

/// An entry from a listing's (lstat) attributes and, for a symlink, its
/// target's attributes (None if dangling). Port of `SFTPFileSystem._entry`.
fn make_entry(name: &str, attrs: &FileAttributes, target: Option<&FileAttributes>) -> Entry {
    let lmode = attrs.permissions.unwrap_or(0);
    let is_link = is_link_mode(lmode);
    let shown = if is_link { target.unwrap_or(attrs) } else { attrs };
    let mode = shown.permissions.unwrap_or(0);
    let is_dir = is_dir_mode(mode);
    Entry {
        name: name.to_string(),
        is_dir,
        is_link,
        is_hidden: name.starts_with('.'),
        is_executable: !is_dir && mode & 0o111 != 0,
        size: if is_dir { 0 } else { shown.size.unwrap_or(0) },
        mtime: f64::from(shown.mtime.unwrap_or(0)),
        is_online_only: false,
    }
}

/// Live file systems, so the provider can find ours behind an `Arc<dyn FileSystem>`.
fn live() -> &'static Mutex<Vec<Weak<SftpFileSystem>>> {
    static LIVE: Mutex<Vec<Weak<SftpFileSystem>>> = Mutex::new(Vec::new());
    &LIVE
}

pub struct SftpFileSystem {
    conn: Arc<Conn>,
    home: String,
}

impl SftpFileSystem {
    /// Wrap a connection whose first SFTP channel is open: ask for the home folder.
    pub(crate) fn start(conn: Arc<Conn>, sftp: Arc<Sftp>) -> std::result::Result<Self, String> {
        let s = sftp.clone();
        let home = block_on(async move {
            match s.raw.realpath(".").await {
                Ok(name) => name.files.into_iter().next().map(|f| f.filename),
                Err(_) => None,
            }
        })
        .map_err(|e| e.to_string())?
        .filter(|h| h.starts_with('/'))
        .unwrap_or_else(|| "/".into());
        conn.set_control(sftp);
        Ok(SftpFileSystem { conn, home })
    }

    /// Register `fs` so [`SftpFileSystem::find`] can recognise it.
    pub(crate) fn register(fs: &Arc<SftpFileSystem>) {
        let mut live = live().lock().unwrap_or_else(|e| e.into_inner());
        live.retain(|w| w.strong_count() > 0);
        live.push(Arc::downgrade(fs));
    }

    /// The SFTP file system behind `fs`, if it is one opened by this crate.
    pub fn find(fs: &Arc<dyn FileSystem>) -> Option<Arc<SftpFileSystem>> {
        let ptr = Arc::as_ptr(fs) as *const ();
        let live = live().lock().unwrap_or_else(|e| e.into_inner());
        live.iter().filter_map(Weak::upgrade).find(|s| Arc::as_ptr(s) as *const () == ptr)
    }

    /// The login's home folder (where relative start folders begin).
    pub fn home(&self) -> &str {
        &self.home
    }

    /// "user@host[:port]"
    pub fn label(&self) -> &str {
        &self.conn.label
    }

    /// False once the connection dropped (keep-alives unanswered, server gone…).
    pub fn is_connected(&self) -> bool {
        self.conn.is_connected()
    }

    fn control(&self) -> Result<Arc<Sftp>> {
        self.conn.control()
    }

    /// Run a request on the control channel; errors are reported for `path`.
    fn request<T, F, Fut>(&self, path: &str, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Sftp>) -> Fut,
        Fut: Future<Output = std::result::Result<T, SftpError>> + Send + 'static,
    {
        let sftp = self.control()?;
        let fut = f(sftp.clone());
        match block_on(fut)? {
            Ok(v) => Ok(v),
            Err(e) => {
                sftp.note(&e);
                Err(self.conn.error(e, &self.display(path)))
            }
        }
    }

    /// lstat, then (for a link) stat of the target.
    fn stat_entry(&self, path: &str) -> Result<Entry> {
        let p = path.to_string();
        let (attrs, target) = self.request(path, move |s| async move {
            let attrs = s.raw.lstat(p.clone()).await?.attrs;
            let target = if is_link_mode(attrs.permissions.unwrap_or(0)) {
                s.raw.stat(p).await.ok().map(|a| a.attrs)
            } else {
                None
            };
            Ok((attrs, target))
        })?;
        let name = paths::basename(path);
        let name = if name.is_empty() { path } else { name };
        Ok(make_entry(name, &attrs, target.as_ref()))
    }

    /// Every entry of a directory (unsorted, hidden ones included).
    fn read_dir(&self, path: &str) -> Result<Vec<Entry>> {
        let dir = path.to_string();
        let items = self.request(path, move |s| async move {
            let handle = s.raw.opendir(dir.clone()).await?.handle;
            let mut files = Vec::new();
            let listed = loop {
                match s.raw.readdir(handle.clone()).await {
                    // A server that never says EOF but runs out of names is done too.
                    Ok(name) if name.files.is_empty() => break Ok(()),
                    Ok(name) => files.extend(name.files.into_iter().filter(|f| is_plain_name(&f.filename))),
                    Err(e) if is_eof(&e) => break Ok(()),
                    Err(e) => break Err(e),
                }
            };
            // Closed in the background: one round trip less for a listing.
            let closer = s.clone();
            tokio::spawn(async move {
                let _ = closer.raw.close(handle).await;
            });
            listed?;
            // Follow symlinks, pipelined rather than one round trip each.
            let lookups: Vec<_> = files
                .iter()
                .map(|f| {
                    let link = is_link_mode(f.attrs.permissions.unwrap_or(0));
                    let (s, p) = (s.clone(), paths::join(&dir, &[&f.filename]));
                    async move { if link { s.raw.stat(p).await.ok().map(|a| a.attrs) } else { None } }
                })
                .collect();
            let targets: Vec<_> = stream::iter(lookups).buffered(PIPELINE).collect().await;
            Ok(files.into_iter().zip(targets).collect::<Vec<_>>())
        })?;
        Ok(items.iter().map(|(f, target)| make_entry(&f.filename, &f.attrs, target.as_ref())).collect())
    }

    fn sftp_mkdir(&self, path: &str) -> Result<()> {
        let p = path.to_string();
        self.request(path, move |s| async move {
            let attrs = FileAttributes { permissions: Some(0o777), ..Default::default() };
            s.raw.mkdir(p, attrs).await.map(|_| ())
        })
    }
}

/// Depth-first walk below a directory; symlinked directories are yielded as
/// file-like links and not followed (Python's `_walk`).
struct Walk<'a> {
    fs: &'a SftpFileSystem,
    root: Option<(RelPath, Entry)>,
    stack: Vec<(String, RelPath, VecDeque<Entry>)>,
    pending_dir: Option<(String, RelPath)>,
}

impl Iterator for Walk<'_> {
    type Item = Result<(RelPath, Entry)>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(root) = self.root.take() {
            return Some(Ok(root));
        }
        if let Some((path, rel)) = self.pending_dir.take() {
            match self.fs.read_dir(&path) {
                Ok(mut entries) => {
                    entries.sort_by(|a, b| a.name.cmp(&b.name));
                    self.stack.push((path, rel, entries.into()));
                }
                Err(e) => return Some(Err(e)),
            }
        }
        loop {
            let (dir, rel, entries) = self.stack.last_mut()?;
            let Some(mut entry) = entries.pop_front() else {
                self.stack.pop();
                continue;
            };
            if entry.is_link && entry.is_dir {
                entry = Entry { is_dir: false, is_executable: false, size: 0, ..entry };
            }
            let mut child_rel = rel.clone();
            child_rel.push(entry.name.clone());
            if entry.is_dir {
                self.pending_dir = Some((paths::join(dir, &[&entry.name]), child_rel.clone()));
            }
            return Some(Ok((child_rel, entry)));
        }
    }
}

impl FileSystem for SftpFileSystem {
    fn type_name(&self) -> String {
        self.conn.type_name.clone()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { writable: true, can_trash: false, can_rename: true, watchable: false }
    }

    fn read_only_reason(&self) -> String {
        format!("{} refused the operation", self.conn.type_name)
    }

    fn list_dir(&self, path: &str, opts: ListOptions) -> Result<Vec<Entry>> {
        Ok(sort_entries(self.read_dir(path)?, opts))
    }

    fn stat(&self, path: &str) -> Result<Entry> {
        self.stat_entry(path)
    }

    fn is_dir(&self, path: &str) -> bool {
        let p = path.to_string();
        self.request(path, move |s| async move { s.raw.stat(p).await })
            .map(|a| is_dir_mode(a.attrs.permissions.unwrap_or(0)))
            .unwrap_or(false)
    }

    fn parent_path(&self, path: &str) -> Option<String> {
        if path == "/" { None } else { Some(paths::dirname(path).to_string()) }
    }

    fn join(&self, path: &str, names: &[&str]) -> String {
        paths::join(path, names)
    }

    fn basename(&self, path: &str) -> String {
        let name = paths::basename(path);
        if name.is_empty() { path.to_string() } else { name.to_string() }
    }

    fn display(&self, path: &str) -> String {
        format!("{}{path}", self.conn.type_name)
    }

    fn open_read(&self, path: &str) -> Result<Box<dyn ReadStream>> {
        Ok(Box::new(SftpReader::open(self.conn.clone(), path, self.display(path))?))
    }

    fn walk<'a>(&'a self, path: &str) -> Result<WalkIter<'a>> {
        let root = self.stat(path)?;
        let pending_dir = (root.is_dir && !root.is_link).then(|| (path.to_string(), RelPath::new()));
        Ok(Box::new(Walk { fs: self, root: Some((RelPath::new(), root)), stack: Vec::new(), pending_dir }))
    }

    fn mkdir(&self, path: &str, exist_ok: bool) -> Result<()> {
        let mut missing = Vec::new();
        let mut p = path.to_string();
        while !p.is_empty() && p != "/" && !self.exists(&p) {
            let parent = paths::dirname(&p).to_string();
            missing.push(p);
            p = parent;
        }
        if missing.is_empty() && !exist_ok {
            return Err(VfsError::AlreadyExists(self.display(path)));
        }
        for dir in missing.iter().rev() {
            self.sftp_mkdir(dir)?;
        }
        Ok(())
    }

    fn remove(&self, path: &str) -> Result<()> {
        let p = path.to_string();
        let is_dir = self
            .request(path, move |s| async move { s.raw.lstat(p).await })
            .map(|a| is_dir_mode(a.attrs.permissions.unwrap_or(0)))?;
        if !is_dir {
            let p = path.to_string();
            return self.request(path, move |s| async move { s.raw.remove(p).await.map(|_| ()) });
        }
        // Subdirectories one by one; the files of a directory all at once.
        let entries = self.read_dir(path)?;
        let mut files = Vec::new();
        for e in entries {
            let child = paths::join(path, &[&e.name]);
            if e.is_dir && !e.is_link {
                self.remove(&child)?;
            } else {
                files.push(child);
            }
        }
        if !files.is_empty() {
            let failed = self.request(path, move |s| async move {
                let removals: Vec<_> = files
                    .iter()
                    .map(|f| {
                        let (s, f) = (s.clone(), f.clone());
                        async move { s.raw.remove(f).await }
                    })
                    .collect();
                let results: Vec<_> = stream::iter(removals).buffered(PIPELINE).collect().await;
                let first_error = files.into_iter().zip(results).find_map(|(f, r)| r.err().map(|e| (f, e)));
                Ok(first_error)
            })?;
            if let Some((file, err)) = failed {
                return Err(self.conn.error(err, &self.display(&file)));
            }
        }
        let p = path.to_string();
        self.request(path, move |s| async move { s.raw.rmdir(p).await.map(|_| ()) })
    }

    fn rename(&self, src: &str, dst: &str) -> Result<()> {
        let (s1, d1) = (src.to_string(), dst.to_string());
        self.request(src, move |s| async move {
            if s.posix_rename && posix_rename(&s, &s1, &d1).await.is_ok() {
                return Ok(());
            }
            // Servers without the posix-rename extension.
            s.raw.rename(s1, d1).await.map(|_| ())
        })
    }

    fn open_write(&self, path: &str) -> Result<Box<dyn WriteStream>> {
        let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE;
        Ok(Box::new(SftpWriter::open(self.conn.clone(), path, self.display(path), flags)?))
    }

    fn set_mtime(&self, path: &str, mtime: f64) -> Result<()> {
        let t = mtime.clamp(0.0, f64::from(u32::MAX)) as u32;
        let p = path.to_string();
        self.request(path, move |s| async move {
            let attrs = FileAttributes { atime: Some(t), mtime: Some(t), ..Default::default() };
            s.raw.setstat(p, attrs).await.map(|_| ())
        })
    }

    fn close(&self) {
        self.conn.close();
    }
}
