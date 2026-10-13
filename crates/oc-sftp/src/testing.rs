//! An in-process SSH server with an SFTP subsystem serving a local folder as
//! its root (like the Python tests' asyncssh server with `chroot`), for this
//! crate's tests and for UI tests elsewhere. Enabled by the `testing` feature.
//!
//! ```ignore
//! let server = oc_sftp::testing::start_sftp_server(dir.path());
//! // connect to server.host:server.port as USER / PASSWORD
//! ```

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{Auth, ChannelOpenHandle, Msg, Response, Server as _, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use russh_sftp::protocol::{
    Attrs, Data, ExtendedReply, File, FileAttributes, Handle, Name, OpenFlags, Packet, Status, StatusCode, Version,
};
use russh_sftp::server::StatusReply;
use tokio::runtime::Runtime;

use crate::known_hosts;
use crate::paths;

pub const USER: &str = "tester";
pub const PASSWORD: &str = "s3cret";

/// How the test server behaves.
#[derive(Clone, Debug)]
pub struct ServerOptions {
    /// The accepted password for [`USER`]; None: no password logins.
    pub password: Option<String>,
    /// Keys [`USER`] may log in with.
    pub authorized_keys: Vec<PublicKey>,
    /// Take the password only through keyboard-interactive (like many PAM setups).
    pub keyboard_interactive_only: bool,
    /// Disconnect after this many failed attempts (OpenSSH's MaxAuthTries).
    pub max_auth_attempts: usize,
    /// Answer reads with at most this many bytes (short reads).
    pub max_read: Option<u32>,
    /// Advertise `limits@openssh.com` with these (read, write) lengths.
    pub limits: Option<(u64, u64)>,
    pub posix_rename: bool,
    /// The host key; None: a fresh Ed25519 key.
    pub host_key: Option<PrivateKey>,
    /// Refuse channels beyond this many open at once (OpenSSH's MaxSessions).
    pub max_sessions: Option<usize>,
    /// Extra directory entries every listing returns (a broken or hostile
    /// server: "", "../x", "a/b"…), listed as folders.
    pub bogus_names: Vec<String>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions {
            password: Some(PASSWORD.into()),
            authorized_keys: Vec::new(),
            keyboard_interactive_only: false,
            max_auth_attempts: 6,
            max_read: None,
            limits: None,
            posix_rename: true,
            host_key: None,
            max_sessions: None,
            bogus_names: Vec::new(),
        }
    }
}

/// What clients tried, for assertions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthLog {
    pub publickey_offers: usize,
    pub password_attempts: usize,
    pub keyboard_interactive_attempts: usize,
}

struct Shared {
    root: PathBuf,
    opts: ServerOptions,
    log: Mutex<AuthLog>,
}

/// A running server; stopped when dropped.
pub struct SftpServer {
    pub host: String,
    pub port: u16,
    pub root: PathBuf,
    pub host_key: PublicKey,
    /// "SHA256:…"
    pub host_key_fingerprint: String,
    shared: Arc<Shared>,
    runtime: Option<Runtime>,
}

impl SftpServer {
    pub fn auth_log(&self) -> AuthLog {
        self.shared.log.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

impl Drop for SftpServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A random Ed25519 key (e.g. a client key, or another host key).
pub fn random_key() -> PrivateKey {
    PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap_or_else(|e| panic!("key generation: {e}"))
}

pub fn start_sftp_server(root: &Path) -> SftpServer {
    start_sftp_server_with(root, ServerOptions::default())
}

pub fn start_sftp_server_with(root: &Path, opts: ServerOptions) -> SftpServer {
    let key = opts.host_key.clone().unwrap_or_else(random_key);
    let host_key = key.public_key().clone();
    let mut methods = vec![MethodKind::PublicKey];
    if opts.keyboard_interactive_only {
        methods.push(MethodKind::KeyboardInteractive);
    } else if opts.password.is_some() {
        methods.push(MethodKind::Password);
    }
    let config = Arc::new(russh::server::Config {
        methods: MethodSet::from(&methods[..]),
        auth_rejection_time: Duration::from_millis(1),
        auth_rejection_time_initial: Some(Duration::ZERO),
        max_auth_attempts: opts.max_auth_attempts,
        keys: vec![key],
        inactivity_timeout: Some(Duration::from_secs(600)),
        ..Default::default()
    });
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let shared = Arc::new(Shared { root: root.clone(), opts, log: Mutex::default() });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("oc-sftp-test-server")
        .enable_all()
        .build()
        .unwrap_or_else(|e| panic!("test server runtime: {e}"));
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap_or_else(|e| panic!("test server bind: {e}"));
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let mut server = Server { shared: shared.clone() };
    runtime.spawn(async move {
        while let Ok((socket, peer)) = listener.accept().await {
            let _ = socket.set_nodelay(true);
            let handler = server.new_client(Some(peer));
            let config = config.clone();
            tokio::spawn(async move {
                if let Ok(session) = russh::server::run_stream(config, QueuedWrites::new(socket), handler).await {
                    let _ = session.await;
                }
            });
        }
    });
    SftpServer {
        host: "127.0.0.1".into(),
        port,
        root,
        host_key_fingerprint: known_hosts::fingerprint(&host_key),
        host_key,
        shared,
        runtime: Some(runtime),
    }
}

/// The server's end of a connection, with writes queued (unbounded) and sent
/// by a task of their own. russh's session loop doesn't read while it
/// writes: with russh at both ends (the client under test), both sides can
/// block writing into full socket buffers at once and never read again
/// (Windows' small loopback buffers make that easy to hit). OpenSSH keeps
/// reading while it writes; so does this. What is queued is bounded by the
/// SSH channel windows.
struct QueuedWrites {
    read: tokio::net::tcp::OwnedReadHalf,
    queue: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
}

impl QueuedWrites {
    fn new(socket: tokio::net::TcpStream) -> Self {
        use tokio::io::AsyncWriteExt;
        let (read, mut write) = socket.into_split();
        let (queue, mut chunks) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(chunk) = chunks.recv().await {
                if write.write_all(&chunk).await.is_err() {
                    return;
                }
            }
            let _ = write.shutdown().await;
        });
        QueuedWrites { read, queue: Some(queue) }
    }
}

impl tokio::io::AsyncRead for QueuedWrites {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.read).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for QueuedWrites {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let sent = self.queue.as_ref().is_some_and(|q| q.send(buf.to_vec()).is_ok());
        std::task::Poll::Ready(if sent { Ok(buf.len()) } else { Err(io::ErrorKind::BrokenPipe.into()) })
    }

    fn poll_flush(self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        self.queue = None; // the writer task sends what is queued, then shuts down
        std::task::Poll::Ready(Ok(()))
    }
}

/// An SSH agent holding some keys, on a Unix socket; stopped when dropped.
#[cfg(unix)]
pub struct TestAgent {
    pub socket: PathBuf,
    _dir: PathBuf,
    runtime: Option<Runtime>,
}

#[cfg(unix)]
impl Drop for TestAgent {
    fn drop(&mut self) {
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
        let _ = fs::remove_dir_all(&self._dir);
    }
}

/// Start an SSH agent holding `keys`.
#[cfg(unix)]
pub fn start_agent(keys: &[PrivateKey]) -> TestAgent {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    // Unix socket paths are short (104 bytes on macOS): keep it in /tmp.
    let dir =
        PathBuf::from("/tmp").join(format!("oc-agent-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("agent dir: {e}"));
    let socket = dir.join("agent.sock");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap_or_else(|e| panic!("agent runtime: {e}"));
    let listener = runtime
        .block_on(async { tokio::net::UnixListener::bind(&socket) })
        .unwrap_or_else(|e| panic!("agent bind: {e}"));
    let incoming = futures::stream::unfold(listener, |l| async move {
        let next = l.accept().await.map(|(s, _)| s);
        Some((next, l))
    });
    runtime.spawn(async move {
        let _ = russh::keys::agent::server::serve(Box::pin(incoming), ()).await;
    });
    let sock = socket.clone();
    let keys = keys.to_vec();
    runtime.block_on(async move {
        let mut client = russh::keys::agent::client::AgentClient::connect_uds(&sock)
            .await
            .unwrap_or_else(|e| panic!("agent connect: {e}"));
        for k in &keys {
            client.add_identity(k, &[]).await.unwrap_or_else(|e| panic!("agent add: {e}"));
        }
    });
    TestAgent { socket, _dir: dir, runtime: Some(runtime) }
}

// ---------------------------------------------------------------------------
// SSH
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Server {
    shared: Arc<Shared>,
}

impl russh::server::Server for Server {
    type Handler = SshSession;

    fn new_client(&mut self, _: Option<SocketAddr>) -> SshSession {
        SshSession { shared: self.shared.clone(), channels: HashMap::new(), open_channels: 0 }
    }
}

struct SshSession {
    shared: Arc<Shared>,
    channels: HashMap<ChannelId, Channel<Msg>>,
    open_channels: usize,
}

impl SshSession {
    fn log(&self, f: impl FnOnce(&mut AuthLog)) {
        f(&mut self.shared.log.lock().unwrap_or_else(|e| e.into_inner()));
    }

    fn key_ok(&self, user: &str, key: &PublicKey) -> bool {
        user == USER && self.shared.opts.authorized_keys.iter().any(|k| k.key_data() == key.key_data())
    }
}

impl russh::server::Handler for SshSession {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        self.log(|l| l.password_attempts += 1);
        let opts = &self.shared.opts;
        let ok = !opts.keyboard_interactive_only && user == USER && opts.password.as_deref() == Some(password);
        Ok(if ok { Auth::Accept } else { Auth::reject() })
    }

    async fn auth_publickey_offered(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        self.log(|l| l.publickey_offers += 1);
        Ok(if self.key_ok(user, key) { Auth::Accept } else { Auth::reject() })
    }

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        Ok(if self.key_ok(user, key) { Auth::Accept } else { Auth::reject() })
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _submethods: &str,
        response: Option<Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        if !self.shared.opts.keyboard_interactive_only {
            return Ok(Auth::reject());
        }
        let Some(mut response) = response else {
            return Ok(Auth::Partial {
                name: "".into(),
                instructions: "".into(),
                prompts: vec![("Password: ".into(), false)].into(),
            });
        };
        self.log(|l| l.keyboard_interactive_attempts += 1);
        let answer = response.next().map(|b| String::from_utf8_lossy(&b).into_owned());
        let ok = user == USER && answer.is_some() && answer.as_deref() == self.shared.opts.password.as_deref();
        Ok(if ok { Auth::Accept } else { Auth::reject() })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.shared.opts.max_sessions.is_some_and(|max| self.open_channels >= max) {
            drop(reply); // refused (administratively prohibited)
            return Ok(());
        }
        self.open_channels += 1;
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn channel_close(&mut self, _channel: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
        self.open_channels = self.open_channels.saturating_sub(1);
        Ok(())
    }

    async fn channel_eof(&mut self, channel: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
        session.close(channel)?;
        Ok(())
    }

    async fn subsystem_request(&mut self, id: ChannelId, name: &str, session: &mut Session) -> Result<(), Self::Error> {
        match self.channels.remove(&id) {
            Some(channel) if name == "sftp" => {
                session.channel_success(id)?;
                let handler = SftpHandler { shared: self.shared.clone(), handles: HashMap::new(), next: 0 };
                russh_sftp::server::run(channel.into_stream(), handler).await;
            }
            _ => session.channel_failure(id)?,
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SFTP
// ---------------------------------------------------------------------------

enum Open {
    File(fs::File),
    Dir(Option<Vec<File>>),
}

struct SftpHandler {
    shared: Arc<Shared>,
    handles: HashMap<String, Open>,
    next: u64,
}

fn ok(id: u32) -> Status {
    Status { id, status_code: StatusCode::Ok, error_message: "Ok".into(), language_tag: "en-US".into() }
}

fn fail(e: io::Error) -> StatusReply {
    let code = match e.kind() {
        io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    };
    let message = match code {
        StatusCode::NoSuchFile => "No such file".to_string(),
        StatusCode::PermissionDenied => "Permission denied".to_string(),
        _ => e.to_string(),
    };
    code.with_message(message)
}

fn secs(t: io::Result<SystemTime>) -> Option<u32> {
    t.ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs().min(u64::from(u32::MAX)) as u32)
}

fn attrs(meta: &fs::Metadata) -> FileAttributes {
    #[cfg(unix)]
    let (mode, uid, gid) = {
        use std::os::unix::fs::MetadataExt;
        (meta.mode(), meta.uid(), meta.gid())
    };
    #[cfg(not(unix))]
    let (mode, uid, gid) = {
        let kind = if meta.is_dir() {
            0o040_000
        } else if meta.is_symlink() {
            0o120_000
        } else {
            0o100_000
        };
        (kind | if meta.permissions().readonly() { 0o444 } else { 0o644 }, 0, 0)
    };
    FileAttributes {
        size: Some(meta.len()),
        uid: Some(uid),
        gid: Some(gid),
        permissions: Some(mode),
        atime: secs(meta.accessed()),
        mtime: secs(meta.modified()),
        ..Default::default()
    }
}

/// Read an SSH string from `data`.
fn take_string(data: &mut &[u8]) -> Option<String> {
    let len = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize;
    let s = data.get(4..4 + len)?;
    let out = String::from_utf8_lossy(s).into_owned();
    *data = &data[4 + len..];
    Some(out)
}

impl SftpHandler {
    /// The local path for a client path, confined to the root.
    fn local(&self, path: &str) -> PathBuf {
        let virt = paths::normpath(&paths::join("/", &[path]));
        let rel = virt.trim_start_matches('/');
        if rel.is_empty() { self.shared.root.clone() } else { self.shared.root.join(rel) }
    }

    fn add(&mut self, open: Open) -> String {
        self.next += 1;
        let h = format!("h{}", self.next);
        self.handles.insert(h.clone(), open);
        h
    }

    fn file(&mut self, handle: &str) -> Result<&mut fs::File, StatusReply> {
        match self.handles.get_mut(handle) {
            Some(Open::File(f)) => Ok(f),
            _ => Err(StatusCode::Failure.with_message("bad handle")),
        }
    }
}

fn set_attrs(path: &Path, file: Option<&fs::File>, a: &FileAttributes) -> io::Result<()> {
    if let Some(size) = a.size {
        fs::OpenOptions::new().write(true).open(path)?.set_len(size)?;
    }
    #[cfg(unix)]
    if let Some(mode) = a.permissions {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))?;
    }
    if let Some(mtime) = a.mtime {
        let t = |s: u32| UNIX_EPOCH + Duration::from_secs(u64::from(s));
        let times = fs::FileTimes::new().set_modified(t(mtime)).set_accessed(t(a.atime.unwrap_or(mtime)));
        match file {
            Some(f) => f.set_times(times)?,
            None => {
                let mut options = fs::OpenOptions::new();
                options.read(true);
                // Windows: setting times needs FILE_WRITE_ATTRIBUTES (a
                // read-only handle is refused), and folders open only with
                // FILE_FLAG_BACKUP_SEMANTICS.
                #[cfg(windows)]
                {
                    use std::os::windows::fs::OpenOptionsExt;
                    options.access_mode(0x0100).custom_flags(0x0200_0000);
                }
                options.open(path)?.set_times(times)?
            }
        }
    }
    Ok(())
}

impl russh_sftp::server::Handler for SftpHandler {
    type Error = StatusReply;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported.into()
    }

    async fn init(&mut self, _version: u32, _ext: HashMap<String, String>) -> Result<Version, Self::Error> {
        let mut v = Version::new();
        if self.shared.opts.posix_rename {
            v.extensions.insert("posix-rename@openssh.com".into(), "1".into());
        }
        if self.shared.opts.limits.is_some() {
            v.extensions.insert("limits@openssh.com".into(), "1".into());
        }
        Ok(v)
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _a: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let path = self.local(&filename);
        let file = fs::OpenOptions::from(pflags).open(&path).map_err(fail)?;
        Ok(Handle { id, handle: self.add(Open::File(file)) })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        match self.handles.remove(&handle) {
            Some(_) => Ok(ok(id)),
            None => Err(StatusCode::Failure.with_message("bad handle")),
        }
    }

    async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<Data, Self::Error> {
        let len = self.shared.opts.max_read.map_or(len, |m| len.min(m)) as usize;
        let file = self.file(&handle)?;
        file.seek(SeekFrom::Start(offset)).map_err(fail)?;
        let mut data = vec![0u8; len];
        let mut n = 0;
        while n < len {
            match file.read(&mut data[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(fail(e)),
            }
        }
        if n == 0 {
            return Err(StatusCode::Eof.with_message("End of file"));
        }
        data.truncate(n);
        Ok(Data { id, data })
    }

    async fn write(&mut self, id: u32, handle: String, offset: u64, data: Vec<u8>) -> Result<Status, Self::Error> {
        let file = self.file(&handle)?;
        file.seek(SeekFrom::Start(offset)).map_err(fail)?;
        file.write_all(&data).map_err(fail)?;
        Ok(ok(id))
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let meta = fs::symlink_metadata(self.local(&path)).map_err(fail)?;
        Ok(Attrs { id, attrs: attrs(&meta) })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let meta = fs::metadata(self.local(&path)).map_err(fail)?;
        Ok(Attrs { id, attrs: attrs(&meta) })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let meta = self.file(&handle)?.metadata().map_err(fail)?;
        Ok(Attrs { id, attrs: attrs(&meta) })
    }

    async fn setstat(&mut self, id: u32, path: String, a: FileAttributes) -> Result<Status, Self::Error> {
        set_attrs(&self.local(&path), None, &a).map_err(fail)?;
        Ok(ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let dir = self.local(&path);
        let mut files = Vec::new();
        for name in [".", ".."] {
            if let Ok(meta) = fs::metadata(&dir) {
                files.push(File::new(name, attrs(&meta)));
            }
        }
        for item in fs::read_dir(&dir).map_err(fail)? {
            let item = item.map_err(fail)?;
            let meta = fs::symlink_metadata(item.path()).map_err(fail)?;
            files.push(File::new(item.file_name().to_string_lossy(), attrs(&meta)));
        }
        if let Ok(meta) = fs::metadata(&dir) {
            for name in &self.shared.opts.bogus_names {
                files.push(File::new(name.as_str(), attrs(&meta)));
            }
        }
        Ok(Handle { id, handle: self.add(Open::Dir(Some(files))) })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        match self.handles.get_mut(&handle) {
            Some(Open::Dir(files)) => match files.take() {
                Some(files) => Ok(Name { id, files }),
                None => Err(StatusCode::Eof.with_message("End of directory")),
            },
            _ => Err(StatusCode::Failure.with_message("bad handle")),
        }
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        fs::remove_file(self.local(&filename)).map_err(fail)?;
        Ok(ok(id))
    }

    async fn mkdir(&mut self, id: u32, path: String, _a: FileAttributes) -> Result<Status, Self::Error> {
        fs::create_dir(self.local(&path)).map_err(fail)?;
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        fs::remove_dir(self.local(&path)).map_err(fail)?;
        Ok(ok(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let virt = paths::normpath(&paths::join("/", &[&path]));
        Ok(Name { id, files: vec![File::dummy(virt)] })
    }

    async fn rename(&mut self, id: u32, oldpath: String, newpath: String) -> Result<Status, Self::Error> {
        let (old, new) = (self.local(&oldpath), self.local(&newpath));
        if fs::symlink_metadata(&new).is_ok() {
            return Err(StatusCode::Failure.with_message("File exists")); // SFTP v3 rename doesn't replace
        }
        fs::rename(old, new).map_err(fail)?;
        Ok(ok(id))
    }

    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let target = fs::read_link(self.local(&path)).map_err(fail)?;
        Ok(Name { id, files: vec![File::dummy(target.to_string_lossy())] })
    }

    async fn symlink(&mut self, id: u32, linkpath: String, targetpath: String) -> Result<Status, Self::Error> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(targetpath, self.local(&linkpath)).map_err(fail)?;
            Ok(ok(id))
        }
        #[cfg(not(unix))]
        {
            let _ = (id, linkpath, targetpath);
            Err(StatusCode::OpUnsupported.into())
        }
    }

    async fn extended(&mut self, id: u32, request: String, data: Vec<u8>) -> Result<Packet, Self::Error> {
        match request.as_str() {
            "posix-rename@openssh.com" if self.shared.opts.posix_rename => {
                let mut rest = &data[..];
                let (Some(old), Some(new)) = (take_string(&mut rest), take_string(&mut rest)) else {
                    return Err(StatusCode::BadMessage.into());
                };
                fs::rename(self.local(&old), self.local(&new)).map_err(fail)?;
                Ok(Packet::Status(ok(id)))
            }
            "limits@openssh.com" => {
                let Some((read, write)) = self.shared.opts.limits else {
                    return Err(StatusCode::OpUnsupported.into());
                };
                let mut out = Vec::new();
                for v in [256 * 1024u64, read, write, 0] {
                    out.extend_from_slice(&v.to_be_bytes());
                }
                Ok(Packet::ExtendedReply(ExtendedReply { id, data: out }))
            }
            _ => Err(StatusCode::OpUnsupported.into()),
        }
    }
}
