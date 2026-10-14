//! [`SmbFileSystem`]: one share as an `oc_vfs::FileSystem`.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use oc_vfs::{
    Capabilities, Entry, FileSystem, ListOptions, ReadStream, RelPath, Result, VfsError, WalkIter, WriteStream,
    sort_entries,
};
use smb2::pack::FileTime;
use smb2::{ClientConfig, FileAttributes, FileTimes, SmbClient, Tree};
use tokio::sync::{MappedMutexGuard, Mutex, MutexGuard};

use crate::error::{Failure, SmbResult, describe_connect_error};
use crate::runtime::run;
use crate::stream::{SmbReader, SmbWriter};

pub const DEFAULT_PORT: u16 = 445;
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Ceiling for one request including the wait for the connection lock. The
/// library's own per-request timeouts (30 s response, 20 s send) normally
/// fire long before; this only guarantees nothing ever waits forever.
pub(crate) const OP_TIMEOUT: Duration = Duration::from_secs(180);
/// For closing handles and the connection: best effort, never long.
pub(crate) const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

/// How to connect; see [`connect`].
#[derive(Clone, Debug)]
pub struct ConnectOptions {
    pub port: u16,
    /// None: guest. `DOMAIN\user` logs on to that domain (NTLM), as smbprotocol did.
    pub username: Option<String>,
    pub password: Option<String>,
    /// For establishing the TCP connection (Python: `connection_timeout`).
    pub timeout: Duration,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        ConnectOptions { port: DEFAULT_PORT, username: None, password: None, timeout: Duration::from_secs(15) }
    }
}

/// Why [`connect`] failed.
#[derive(Debug, thiserror::Error)]
pub enum SmbConnectError {
    /// The server refused the credentials (or the account may not use the share).
    #[error("{0}")]
    AuthenticationRequired(String),
    #[error("{0}")]
    Connection(String),
}

impl From<SmbConnectError> for oc_provider::ConnectError {
    fn from(e: SmbConnectError) -> Self {
        match e {
            SmbConnectError::AuthenticationRequired(m) => oc_provider::ConnectError::AuthenticationRequired(m),
            SmbConnectError::Connection(m) => oc_provider::ConnectError::Connection(m),
        }
    }
}

/// `DOMAIN\user` -> ("DOMAIN", "user"); anything else has no domain.
pub(crate) fn split_domain(username: &str) -> (&str, &str) {
    match username.split_once('\\') {
        Some((domain, user)) => (domain, user),
        None => ("", username),
    }
}

fn socket_addr(server: &str, port: u16) -> String {
    if server.contains(':') && !server.starts_with('[') {
        format!("[{server}]:{port}")
    } else {
        format!("{server}:{port}")
    }
}

/// The whole handshake (TCP, NEGOTIATE, two SESSION_SETUPs, TREE_CONNECT,
/// the first listing) gets twice the connect timeout: the library bounds the
/// TCP connect by it but waits its 30 s response timeout for each answer,
/// whereas Python's `connection_timeout` bounded the handshake too.
fn connect_limit(timeout: Duration) -> Duration {
    (timeout * 2).max(Duration::from_secs(5))
}

/// Connect to `\\server\share`. Errors with AuthenticationRequired or a
/// connection message (port of `smb.connect`).
pub fn connect(
    server: &str,
    share: &str,
    opts: &ConnectOptions,
) -> std::result::Result<SmbFileSystem, SmbConnectError> {
    let share = share.trim_matches(['/', '\\']).to_string();
    let username = opts.username.clone().filter(|u| !u.is_empty());
    let (domain, user) = split_domain(username.as_deref().unwrap_or(""));
    let config = ClientConfig {
        addr: socket_addr(server, opts.port),
        timeout: opts.timeout,
        username: user.to_string(),
        password: opts.password.clone().unwrap_or_default(),
        domain: domain.to_string(),
        // A NAS that went to sleep or a Wi-Fi roam: the library re-dials within
        // its bounded policy and replays only idempotent requests.
        auto_reconnect: true,
        ..ClientConfig::default()
    };
    let share_name = share.clone();
    let result = run(connect_limit(opts.timeout), async move {
        let mut client = SmbClient::connect(config).await?;
        let mut tree = client.connect_share(&share_name).await?;
        client.list_directory(&mut tree, "").await?; // proves the share exists and is readable
        Ok::<_, Failure>(Session { client, tree })
    })
    .unwrap_or(Err(Failure::TimedOut));
    match result {
        Ok(session) => Ok(SmbFileSystem::new(server, &share, opts.port, session)),
        Err(f) if f.is_auth() => Err(SmbConnectError::AuthenticationRequired(format!(
            "The server {server} didn't accept the user name or password for {}",
            username.as_deref().unwrap_or("guest")
        ))),
        Err(f) => Err(SmbConnectError::Connection(describe_connect_error(server, opts.port, &f))),
    }
}

/// The share names a server offers (disk shares, without admin `$` shares).
/// Not used by the provider (Python required a share), but handy for a
/// share picker.
pub fn list_shares(server: &str, opts: &ConnectOptions) -> std::result::Result<Vec<String>, SmbConnectError> {
    let username = opts.username.clone().filter(|u| !u.is_empty());
    let (domain, user) = split_domain(username.as_deref().unwrap_or(""));
    let config = ClientConfig {
        addr: socket_addr(server, opts.port),
        timeout: opts.timeout,
        username: user.to_string(),
        password: opts.password.clone().unwrap_or_default(),
        domain: domain.to_string(),
        ..ClientConfig::default()
    };
    let result = run(connect_limit(opts.timeout), async move {
        let mut client = SmbClient::connect(config).await?;
        let shares = client.list_shares().await?;
        Ok::<_, Failure>(shares.into_iter().map(|s| s.name).collect::<Vec<_>>())
    })
    .unwrap_or(Err(Failure::TimedOut));
    result.map_err(|f| {
        if f.is_auth() {
            SmbConnectError::AuthenticationRequired(format!(
                "The server {server} didn't accept the user name or password for {}",
                username.as_deref().unwrap_or("guest")
            ))
        } else {
            SmbConnectError::Connection(describe_connect_error(server, opts.port, &f))
        }
    })
}

/// An authenticated session with the share connected.
pub(crate) struct Session {
    pub(crate) client: SmbClient,
    pub(crate) tree: Tree,
}

/// What a file system and its open files share: the connection, behind the
/// per-connection request lock.
pub(crate) struct Shared {
    server: String,
    share: String,
    pub(crate) type_name: String,
    /// One request in flight per connection, as in the Python version: a
    /// listing waits while a preview's read is answered. `None` once closed.
    state: Mutex<Option<Session>>,
}

impl Shared {
    /// The connection, for one request. If it is known to have dropped (a
    /// NAS reboot, a Wi-Fi roam), it is revived first, while nothing of this
    /// request has been sent yet: the library replays only idempotent
    /// requests by itself, so without this a mkdir, rename, delete or upload
    /// would keep failing with "Disconnected" until something listed.
    pub(crate) async fn session(&self) -> SmbResult<MappedMutexGuard<'_, Session>> {
        let mut guard = MutexGuard::try_map(self.state.lock().await, |s| s.as_mut()).map_err(|_| Failure::Closed)?;
        if guard.client.is_disconnected() {
            let s = &mut *guard;
            // A stat replays after reviving the session and its tree.
            if let Err(e) = s.client.stat(&mut s.tree, "").await
                && s.client.is_disconnected()
            {
                return Err(e.into());
            }
        }
        Ok(guard)
    }

    /// `\\server\share\a\b`, for error messages (as smbclient showed them).
    pub(crate) fn unc(&self, path: &str) -> String {
        let inner = path.trim_matches('/').replace('/', "\\");
        let base = format!("\\\\{}\\{}", self.server, self.share);
        if inner.is_empty() { base } else { format!("{base}\\{inner}") }
    }

    pub(crate) fn vfs_error(&self, failure: Failure, path: &str) -> VfsError {
        failure.into_vfs(&self.type_name, &self.unc(path))
    }
}

/// Run one request (holding the connection lock inside `fut`), bounded.
pub(crate) fn exec<T: Send + 'static>(fut: impl Future<Output = SmbResult<T>> + Send + 'static) -> SmbResult<T> {
    run(OP_TIMEOUT, fut).unwrap_or(Err(Failure::TimedOut))
}

/// Path inside the share as smb2 wants it: `/`-separated, no leading slash.
pub(crate) fn rel(path: &str) -> String {
    path.trim_matches('/').to_string()
}

/// `posixpath.dirname`, None at the root.
fn parent_path(path: &str) -> Option<String> {
    if path.is_empty() || path == "/" {
        return None;
    }
    let trimmed = path.trim_end_matches('/');
    Some(match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
    })
}

/// `posixpath.join(path or "/", *names)`.
fn join(path: &str, names: &[&str]) -> String {
    let mut out = if path.is_empty() { "/".to_string() } else { path.to_string() };
    for name in names {
        if name.starts_with('/') {
            out = (*name).to_string();
        } else {
            if !out.ends_with('/') {
                out.push('/');
            }
            out.push_str(name);
        }
    }
    out
}

/// A name that is one entry of its directory: not empty, not "." or "..",
/// no separator or NUL.
fn is_plain_name(name: &str) -> bool {
    !matches!(name, "" | "." | "..") && !name.contains(['/', '\0'])
}

/// `posixpath.basename` ("" at the root).
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or("")
}

fn unix_time(t: FileTime) -> f64 {
    const EPOCH_DIFF: i128 = 116_444_736_000_000_000; // 1601 -> 1970 in 100 ns ticks
    if t.0 == 0 { 0.0 } else { (i128::from(t.0) - EPOCH_DIFF) as f64 / 1e7 }
}

/// A Unix time as an SMB FILETIME, or None if it has none (NaN, infinite,
/// or outside what FILETIME can hold). 0 and -1 mean "leave unchanged" /
/// "stop updating" on the wire, so the result is kept clear of both.
fn file_time(mtime: f64) -> Option<FileTime> {
    const EPOCH_DIFF: f64 = 116_444_736_000_000_000.0;
    let ticks = (mtime * 1e7).round() + EPOCH_DIFF;
    // i64::MAX as f64 rounds up to 2^63, so `<` keeps the cast in range.
    (ticks.is_finite() && ticks >= 1.0 && ticks < i64::MAX as f64).then_some(FileTime(ticks as u64))
}

fn entry(name: String, is_dir: bool, size: u64, modified: FileTime, attributes: FileAttributes) -> Entry {
    let attrs = attributes.bits();
    Entry {
        is_hidden: name.starts_with('.') || attrs & FILE_ATTRIBUTE_HIDDEN != 0,
        name,
        is_dir,
        // A directory that is a reparse point is a symlink or junction: walks
        // don't follow it (loops). Files that are reparse points (dedup, cloud
        // tiering) are ordinary files to us.
        is_link: is_dir && attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0,
        is_executable: false,
        size: if is_dir { 0 } else { size },
        mtime: unix_time(modified),
        is_online_only: false,
    }
}

/// One share on an SMB server. Paths are POSIX-style absolute paths inside
/// the share ("/" is the share root).
pub struct SmbFileSystem {
    shared: Arc<Shared>,
}

impl SmbFileSystem {
    fn new(server: &str, share: &str, port: u16, session: Session) -> Self {
        let host = if port == DEFAULT_PORT { server.to_string() } else { format!("{server}:{port}") };
        SmbFileSystem {
            shared: Arc::new(Shared {
                server: server.to_string(),
                share: share.to_string(),
                type_name: format!("smb://{host}/{share}"),
                state: Mutex::new(Some(session)),
            }),
        }
    }

    pub fn server(&self) -> &str {
        &self.shared.server
    }

    pub fn share(&self) -> &str {
        &self.shared.share
    }

    /// Whether the connection is still usable (else reconnect).
    pub fn is_connected(&self) -> bool {
        let shared = self.shared.clone();
        run(
            CLOSE_TIMEOUT,
            async move { shared.state.lock().await.as_ref().is_some_and(|s| !s.client.is_disconnected()) },
        )
        .unwrap_or(false)
    }

    fn err(&self, failure: Failure, path: &str) -> VfsError {
        self.shared.vfs_error(failure, path)
    }

    fn list(&self, path: &str) -> SmbResult<Vec<Entry>> {
        let shared = self.shared.clone();
        let p = rel(path);
        exec(async move {
            let mut guard = shared.session().await?;
            let s = &mut *guard;
            let listing = s.client.list_directory(&mut s.tree, &p).await?;
            Ok(listing
                .into_iter()
                // "." and ".." (and, from a hostile server, names that would
                // climb out of a copy's destination) are not entries.
                .filter(|e| is_plain_name(&e.name))
                .map(|e| entry(e.name, e.is_directory, e.size, e.modified, e.attributes))
                .collect())
        })
    }

    fn stat_raw(&self, path: &str) -> SmbResult<Entry> {
        let shared = self.shared.clone();
        let p = rel(path);
        let name = self.basename(path);
        exec(async move {
            let mut guard = shared.session().await?;
            let s = &mut *guard;
            let info = s.client.stat(&mut s.tree, &p).await?;
            Ok(entry(name, info.is_directory, info.size, info.modified, info.attributes))
        })
    }

    fn create_dir(&self, path: &str) -> SmbResult<()> {
        let shared = self.shared.clone();
        let p = rel(path);
        exec(async move {
            let mut guard = shared.session().await?;
            let s = &mut *guard;
            s.client.create_directory(&s.tree, &p).await?;
            Ok(())
        })
    }

    fn delete(&self, path: &str, dir: bool) -> SmbResult<()> {
        let shared = self.shared.clone();
        let p = rel(path);
        exec(async move {
            let mut guard = shared.session().await?;
            let s = &mut *guard;
            if dir {
                s.client.delete_directory(&s.tree, &p).await?;
            } else {
                s.client.delete_file(&s.tree, &p).await?;
            }
            Ok(())
        })
    }

    /// Delete a directory tree, one request at a time (other requests may
    /// run in between).
    fn rmtree(&self, path: &str) -> Result<()> {
        for e in self.list(path).map_err(|f| self.err(f, path))? {
            let child = self.join(path, &[&e.name]);
            if e.is_dir && !e.is_link {
                self.rmtree(&child)?;
            } else {
                self.delete(&child, e.is_dir).map_err(|f| self.err(f, &child))?;
            }
        }
        self.delete(path, true).map_err(|f| self.err(f, path))
    }
}

impl Drop for SmbFileSystem {
    fn drop(&mut self) {
        // The last handle on the share: log off instead of leaving the
        // session to time out on the server. Open streams keep `Shared` alive.
        if Arc::strong_count(&self.shared) == 1 {
            self.close();
        }
    }
}

impl FileSystem for SmbFileSystem {
    fn type_name(&self) -> String {
        self.shared.type_name.clone()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { writable: true, can_trash: false, can_rename: true, watchable: false }
    }

    fn read_only_reason(&self) -> String {
        format!("{} refused the operation", self.shared.type_name)
    }

    fn list_dir(&self, path: &str, opts: ListOptions) -> Result<Vec<Entry>> {
        // The listing already carries size/times/attributes: no stat per entry.
        let entries = self.list(path).map_err(|f| self.err(f, path))?;
        Ok(sort_entries(entries, opts))
    }

    fn stat(&self, path: &str) -> Result<Entry> {
        self.stat_raw(path).map_err(|f| self.err(f, path))
    }

    fn parent_path(&self, path: &str) -> Option<String> {
        parent_path(path)
    }

    fn join(&self, path: &str, names: &[&str]) -> String {
        join(path, names)
    }

    fn basename(&self, path: &str) -> String {
        let name = basename(path);
        if name.is_empty() { self.shared.share.clone() } else { name.to_string() }
    }

    fn display(&self, path: &str) -> String {
        if path == "/" { self.shared.type_name.clone() } else { format!("{}{path}", self.shared.type_name) }
    }

    fn open_read(&self, path: &str) -> Result<Box<dyn ReadStream>> {
        SmbReader::open(self.shared.clone(), path).map(|r| Box::new(r) as Box<dyn ReadStream>)
    }

    fn walk<'a>(&'a self, path: &str) -> Result<WalkIter<'a>> {
        let root = self.stat(path)?;
        let frames = if root.is_dir {
            vec![Frame { path: path.to_string(), rel: Vec::new(), entries: None }]
        } else {
            Vec::new()
        };
        Ok(Box::new(Walk { fs: self, first: Some(root), frames }))
    }

    fn exists(&self, path: &str) -> bool {
        self.stat_raw(path).is_ok()
    }

    // -- writing ---------------------------------------------------------------

    fn mkdir(&self, path: &str, exist_ok: bool) -> Result<()> {
        // Like os.makedirs: missing parents are created too.
        let exists_error =
            |f: Failure| -> Result<()> { if exist_ok && self.is_dir(path) { Ok(()) } else { Err(self.err(f, path)) } };
        match self.create_dir(path) {
            Ok(()) => Ok(()),
            Err(f) if f.is_already_exists() => exists_error(f),
            Err(f) if f.is_not_found() => {
                match self.parent_path(path) {
                    Some(parent) if parent != path && !self.exists(&parent) => self.mkdir(&parent, true)?,
                    _ => return Err(self.err(f, path)),
                }
                match self.create_dir(path) {
                    Ok(()) => Ok(()),
                    Err(f) if f.is_already_exists() => exists_error(f),
                    Err(f) => Err(self.err(f, path)),
                }
            }
            Err(f) => {
                // The share root can't be created; it always exists.
                if rel(path).is_empty() {
                    return exists_error(f);
                }
                Err(self.err(f, path))
            }
        }
    }

    fn remove(&self, path: &str) -> Result<()> {
        let e = self.stat(path)?;
        if e.is_dir && !e.is_link {
            self.rmtree(path)
        } else {
            self.delete(path, e.is_dir).map_err(|f| self.err(f, path))
        }
    }

    fn rename(&self, src: &str, dst: &str) -> Result<()> {
        let shared = self.shared.clone();
        let (from, to) = (rel(src), rel(dst));
        exec(async move {
            let mut guard = shared.session().await?;
            let s = &mut *guard;
            s.client.rename(&s.tree, &from, &to).await?;
            Ok(())
        })
        .map_err(|f| self.err(f, src))
    }

    fn open_write(&self, path: &str) -> Result<Box<dyn WriteStream>> {
        SmbWriter::create(self.shared.clone(), path).map(|w| Box::new(w) as Box<dyn WriteStream>)
    }

    fn set_mtime(&self, path: &str, mtime: f64) -> Result<()> {
        let shared = self.shared.clone();
        let p = rel(path);
        let Some(t) = file_time(mtime) else {
            return Err(VfsError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Can't set the modification time of {}: {mtime} is out of range", self.shared.unc(path)),
            )));
        };
        exec(async move {
            let mut guard = shared.session().await?;
            let s = &mut *guard;
            // Python: utime(times=(mtime, mtime)).
            s.client.set_times(&s.tree, &p, FileTimes::new().set_modified(t).set_accessed(t)).await?;
            Ok(())
        })
        .map_err(|f| self.err(f, path))
    }

    fn close(&self) {
        let shared = self.shared.clone();
        let _ = run(CLOSE_TIMEOUT, async move {
            let session = shared.state.lock().await.take();
            if let Some(mut s) = session {
                let _ = s.client.disconnect_share(&s.tree).await;
            } // dropping the client closes the socket
        });
    }
}

/// One directory being walked; `entries` is listed when first needed.
struct Frame {
    path: String,
    rel: RelPath,
    entries: Option<std::vec::IntoIter<Entry>>,
}

/// Depth-first walk, entries sorted by name, directories before their
/// contents, symlinked directories not followed.
struct Walk<'a> {
    fs: &'a SmbFileSystem,
    first: Option<Entry>,
    frames: Vec<Frame>,
}

impl Iterator for Walk<'_> {
    type Item = Result<(RelPath, Entry)>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(root) = self.first.take() {
            return Some(Ok((Vec::new(), root)));
        }
        loop {
            let frame = self.frames.last_mut()?;
            if frame.entries.is_none() {
                match self.fs.list(&frame.path) {
                    Ok(mut list) => {
                        list.sort_by(|a, b| a.name.cmp(&b.name));
                        frame.entries = Some(list.into_iter());
                    }
                    Err(f) => {
                        let path = frame.path.clone();
                        self.frames.pop();
                        return Some(Err(self.fs.err(f, &path)));
                    }
                }
            }
            let Some(e) = frame.entries.as_mut().and_then(Iterator::next) else {
                self.frames.pop();
                continue;
            };
            let mut rel = frame.rel.clone();
            rel.push(e.name.clone());
            if e.is_dir && !e.is_link {
                let path = self.fs.join(&frame.path, &[&e.name]);
                self.frames.push(Frame { path, rel: rel.clone(), entries: None });
            }
            return Some(Ok((rel, e)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn times() {
        assert_eq!(unix_time(FileTime(0)), 0.0);
        assert_eq!(unix_time(FileTime(116_444_736_000_000_000 + 15_000_000)), 1.5);
        assert_eq!(unix_time(file_time(1_600_000_000.25).unwrap()), 1_600_000_000.25);
        assert_eq!(
            file_time(1_600_000_000.25),
            Some(FileTime::from_system_time(UNIX_EPOCH + Duration::from_millis(1_600_000_000_250)))
        );
        assert_eq!(unix_time(file_time(-86_400.0).unwrap()), -86_400.0); // before 1970
        // Garbage from a source file system never panics or sends 0 / -1.
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e300, -1e300, -11_644_473_600.0] {
            assert_eq!(file_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn hostile_names_are_not_entries() {
        for name in ["a", "a b", ".hidden", "...", "x\\y"] {
            assert!(is_plain_name(name), "{name}");
        }
        for name in ["", ".", "..", "../etc", "a/b", "nul\0"] {
            assert!(!is_plain_name(name), "{name:?}");
        }
    }

    #[test]
    fn posix_paths() {
        assert_eq!(parent_path("/"), None);
        assert_eq!(parent_path(""), None);
        assert_eq!(parent_path("/a").as_deref(), Some("/"));
        assert_eq!(parent_path("/a/b").as_deref(), Some("/a"));
        assert_eq!(join("/", &["a", "b"]), "/a/b");
        assert_eq!(join("", &["a"]), "/a");
        assert_eq!(join("/x", &["/abs", "c"]), "/abs/c");
        assert_eq!(basename("/a/b"), "b");
        assert_eq!(basename("/"), "");
        assert_eq!(rel("/a/b/"), "a/b");
        assert_eq!(rel("/"), "");
    }

    #[test]
    fn domains_and_addresses() {
        assert_eq!(split_domain(r"CORP\bob"), ("CORP", "bob"));
        assert_eq!(split_domain("bob@corp"), ("", "bob@corp"));
        assert_eq!(socket_addr("nas", 445), "nas:445");
        assert_eq!(socket_addr("fe80::1", 139), "[fe80::1]:139");
    }
}
