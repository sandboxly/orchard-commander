//! One SSH connection and its SFTP channels.
//!
//! Metadata requests (listings, stat, rename…) share one "control" channel:
//! russh-sftp multiplexes requests by id, so threads don't wait for each
//! other's round trips. Streams (downloads, uploads) take a channel of their
//! own from a small pool, so a long transfer never delays a listing behind
//! megabytes of queued file data. When the server allows no more channels
//! (OpenSSH: `MaxSessions`), streams share one.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use oc_vfs::VfsError;
use russh::client::Handle;
use russh_sftp::client::RawSftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::rawsession::Limits;
use russh_sftp::protocol::{Packet, StatusCode};

use crate::connect::ClientHandler;
use crate::runtime::block_on;

/// Chunk size when the server doesn't say (the SFTP spec's guaranteed minimum).
const SAFE_CHUNK: u32 = 32 * 1024;
/// Largest read/write per request even when a server allows more.
const MAX_CHUNK: u32 = 255 * 1024;
/// Per-request timeout on the metadata channel.
pub(crate) const REQUEST_TIMEOUT_SECS: u64 = 60;
/// Stream channels queue megabytes of requests, so a single request may wait
/// long behind the others on a slow link; streams bound their waiting by
/// [`STALL_TIMEOUT`] (no progress at all) instead.
const STREAM_REQUEST_TIMEOUT_SECS: u64 = 3600;
/// How long a stream waits for the next reply before giving up.
pub(crate) const STALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Idle stream channels kept for reuse.
const POOL_MAX: usize = 4;

pub(crate) type Ssh = Arc<Handle<ClientHandler>>;

/// An SFTP channel.
pub(crate) struct Sftp {
    pub raw: RawSftpSession,
    pub posix_rename: bool,
    pub read_chunk: u32,
    pub write_chunk: u32,
    /// A stream channel (pooled when idle), not the metadata channel.
    stream: bool,
    /// Streams using this channel (more than one when channels ran out).
    users: AtomicUsize,
    dead: AtomicBool,
}

impl Sftp {
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    /// Note a failure: anything but a status reply means the channel is gone.
    pub fn note(&self, err: &SftpError) {
        if !matches!(err, SftpError::Status(_) | SftpError::Limited(_)) {
            self.mark_dead();
        }
    }

    pub fn mark_dead(&self) {
        self.dead.store(true, Ordering::Relaxed);
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Open an SFTP channel on `ssh` and negotiate the protocol: the metadata
/// channel, or (`stream`) one for a download or upload.
pub(crate) async fn open_sftp(ssh: &Ssh, timeout: Duration, stream: bool) -> Result<Sftp, String> {
    let request_timeout_secs = if stream { STREAM_REQUEST_TIMEOUT_SECS } else { REQUEST_TIMEOUT_SECS };
    let channel = tokio::time::timeout(timeout, ssh.channel_open_session())
        .await
        .map_err(|_| "timed out opening an SFTP channel".to_string())?
        .map_err(|e| format!("cannot open an SFTP channel: {e}"))?;
    tokio::time::timeout(timeout, channel.request_subsystem(true, "sftp"))
        .await
        .map_err(|_| "timed out starting SFTP".to_string())?
        .map_err(|e| format!("cannot start SFTP: {e}"))?;
    let config = russh_sftp::client::Config {
        max_packet_len: 512 * 1024,
        request_timeout_secs: timeout.as_secs().max(1),
        ..Default::default()
    };
    let mut raw = RawSftpSession::new_with_config(channel.into_stream(), config);
    let version = raw.init().await.map_err(|e| format!("the server has no SFTP service ({e})"))?;
    let has = |name: &str| version.extensions.contains_key(name);
    let (mut read_chunk, mut write_chunk) = (SAFE_CHUNK, SAFE_CHUNK);
    if has("limits@openssh.com")
        && let Ok(reply) = raw.limits().await
    {
        let limits = Limits::from(reply);
        // Leave room for the packet header around the data.
        let packet = limits.packet_len.map_or(u64::MAX, |p| p.saturating_sub(1024));
        let pick =
            |len: Option<u64>| len.unwrap_or(u64::from(MAX_CHUNK)).min(packet).clamp(1, u64::from(MAX_CHUNK)) as u32;
        read_chunk = pick(limits.read_len);
        write_chunk = pick(limits.write_len);
        raw.set_limits(limits);
    }
    raw.set_timeout(request_timeout_secs);
    Ok(Sftp {
        raw,
        posix_rename: has("posix-rename@openssh.com"),
        read_chunk,
        write_chunk,
        stream,
        users: AtomicUsize::new(0),
        dead: AtomicBool::new(false),
    })
}

/// The SSH connection with its SFTP channels.
pub(crate) struct Conn {
    pub ssh: Ssh,
    pub label: String,
    pub type_name: String,
    timeout: Duration,
    control: Mutex<Option<Arc<Sftp>>>,
    pool: Mutex<Vec<Arc<Sftp>>>,
    /// Every stream channel opened, to share one when the server allows no more.
    streams: Mutex<Vec<Weak<Sftp>>>,
    closed: AtomicBool,
}

impl Conn {
    pub fn new(ssh: Ssh, label: String, timeout: Duration) -> Self {
        Conn {
            ssh,
            type_name: format!("sftp://{label}"),
            label,
            timeout,
            control: Mutex::new(None),
            pool: Mutex::new(Vec::new()),
            streams: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        }
    }

    pub fn set_control(&self, sftp: Arc<Sftp>) {
        *lock(&self.control) = Some(sftp);
    }

    pub fn is_connected(&self) -> bool {
        !self.closed.load(Ordering::Relaxed) && !self.ssh.is_closed()
    }

    pub fn lost(&self) -> VfsError {
        VfsError::Io(io::Error::new(io::ErrorKind::NotConnected, format!("Connection to {} was lost", self.label)))
    }

    fn open(&self, stream: bool) -> Result<Arc<Sftp>, VfsError> {
        if !self.is_connected() {
            return Err(self.lost());
        }
        let (ssh, timeout) = (self.ssh.clone(), self.timeout);
        match block_on(async move { open_sftp(&ssh, timeout, stream).await }) {
            Ok(Ok(sftp)) => Ok(Arc::new(sftp)),
            _ if !self.is_connected() => Err(self.lost()),
            Ok(Err(e)) => Err(VfsError::Io(io::Error::other(format!("{}: {e}", self.type_name)))),
            Err(e) => Err(VfsError::Io(e)),
        }
    }

    /// The channel for metadata requests (reopened if it broke).
    pub fn control(&self) -> Result<Arc<Sftp>, VfsError> {
        let mut control = lock(&self.control);
        if let Some(s) = control.as_ref().filter(|s| !s.is_dead()) {
            return Ok(s.clone());
        }
        let fresh = self.open(false)?;
        *control = Some(fresh.clone());
        Ok(fresh)
    }

    /// A channel for a stream; give it back with [`Conn::checkin`].
    ///
    /// Servers limit the channels per connection (OpenSSH: `MaxSessions`,
    /// 10 by default), so when no further one can be opened, the stream
    /// shares the least busy stream channel (requests are multiplexed by id).
    pub fn checkout(&self) -> Result<Arc<Sftp>, VfsError> {
        let sftp = self.find_channel()?;
        sftp.users.fetch_add(1, Ordering::Relaxed);
        Ok(sftp)
    }

    fn find_channel(&self) -> Result<Arc<Sftp>, VfsError> {
        while let Some(s) = lock(&self.pool).pop() {
            if !s.is_dead() {
                return Ok(s);
            }
        }
        let error = match self.open(true) {
            Ok(sftp) => {
                let mut streams = lock(&self.streams);
                streams.retain(|w| w.strong_count() > 0);
                streams.push(Arc::downgrade(&sftp));
                return Ok(sftp);
            }
            Err(e) => e,
        };
        if !self.is_connected() {
            return Err(error);
        }
        let shared = lock(&self.streams)
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|s| !s.is_dead())
            .min_by_key(|s| s.users.load(Ordering::Relaxed));
        match shared {
            Some(sftp) => Ok(sftp),
            None => {
                log::debug!("{}: a stream shares the metadata channel ({error})", self.label);
                self.control().map_err(|_| error)
            }
        }
    }

    /// Give back a stream's channel: kept for reuse unless it broke or
    /// another stream still uses it.
    pub fn checkin(&self, sftp: Arc<Sftp>) {
        let before = sftp.users.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| Some(n.saturating_sub(1)));
        let left = before.unwrap_or(0).saturating_sub(1);
        if left > 0 || !sftp.stream || sftp.is_dead() || !self.is_connected() {
            return;
        }
        let mut pool = lock(&self.pool);
        if pool.len() < POOL_MAX && !pool.iter().any(|p| Arc::ptr_eq(p, &sftp)) {
            pool.push(sftp);
        }
    }

    /// Close the connection (idempotent).
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::Relaxed) {
            return;
        }
        lock(&self.pool).clear();
        lock(&self.control).take();
        let ssh = self.ssh.clone();
        let _ = block_on(async move {
            let _ = tokio::time::timeout(
                Duration::from_secs(3),
                ssh.disconnect(russh::Disconnect::ByApplication, "", "en"),
            )
            .await;
        });
    }

    /// A user-facing error for a failed request on `what` (a display path).
    pub fn error(&self, err: SftpError, what: &str) -> VfsError {
        match err {
            SftpError::Status(status) => {
                let message = if status.error_message.trim().is_empty() {
                    status.status_code.to_string()
                } else {
                    status.error_message.trim().to_string()
                };
                let kind = match status.status_code {
                    StatusCode::NoSuchFile => return VfsError::NotFound(what.to_string()),
                    StatusCode::PermissionDenied => io::ErrorKind::PermissionDenied,
                    StatusCode::OpUnsupported => io::ErrorKind::Unsupported,
                    StatusCode::Eof => io::ErrorKind::UnexpectedEof,
                    StatusCode::NoConnection | StatusCode::ConnectionLost => return self.lost(),
                    _ => io::ErrorKind::Other,
                };
                VfsError::Io(io::Error::new(kind, format!("{message}: {what}")))
            }
            _ if !self.is_connected() => self.lost(),
            SftpError::Timeout => VfsError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{} did not answer in time: {what}", self.label),
            )),
            other => VfsError::Io(io::Error::other(format!("{other}: {what}"))),
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        if self.closed.load(Ordering::Relaxed) {
            return;
        }
        // The last user is gone: say goodbye without waiting for it.
        let ssh = self.ssh.clone();
        crate::runtime::spawn(async move {
            let _ = tokio::time::timeout(
                Duration::from_secs(3),
                ssh.disconnect(russh::Disconnect::ByApplication, "", "en"),
            )
            .await;
        });
    }
}

/// `posix-rename@openssh.com`: rename replacing an existing target.
pub(crate) async fn posix_rename(sftp: &Sftp, src: &str, dst: &str) -> Result<(), SftpError> {
    let mut data = Vec::with_capacity(src.len() + dst.len() + 8);
    for s in [src, dst] {
        data.extend_from_slice(&(s.len() as u32).to_be_bytes());
        data.extend_from_slice(s.as_bytes());
    }
    match sftp.raw.extended("posix-rename@openssh.com", data).await? {
        Packet::Status(s) if s.status_code == StatusCode::Ok => Ok(()),
        Packet::Status(s) => Err(SftpError::Status(s)),
        _ => Err(SftpError::UnexpectedPacket),
    }
}

pub(crate) fn is_eof(err: &SftpError) -> bool {
    matches!(err, SftpError::Status(s) if s.status_code == StatusCode::Eof)
}
