//! Web-based file systems: WebDAV and S3 (Amazon and compatible), over a
//! small blocking HTTP/1.1 client. Port of `remote/httpclient.py`,
//! `remote/webdav.py`, `remote/s3.py` and the WebDAV/S3 providers of
//! `remote/providers.py` in the Python version.
//!
//! - [`http`]: keep-alive connections (pooled, so every thread gets its own
//!   while it needs one), timeouts on every socket operation, ranged reads
//!   giving seekable streams ([`http::RangeReader`]).
//! - [`tls`]: certificates checked against the operating system's trust store
//!   (rustls + rustls-platform-verifier); a certificate the system doesn't
//!   trust (a NAS with a self-signed one) can be trusted by its fingerprint
//!   once, like an SSH host key, and is then pinned.
//! - [`webdav`], [`s3`]: the file systems; [`providers`]: the connection types
//!   (`webdavs://`, `webdav://`, `s3://`, `s3+http://`), registered by
//!   [`register_builtin`].
//!
//! Errors are [`Error`]s; they convert into `VfsError::Io` for file system
//! calls (authentication, certificate and connection problems carry a
//! [`RemoteError`] inside, for the UI to tell apart) and into
//! `ConnectError` for providers.
//!
//! Testing: `tests/` runs against real servers — WsgiDAV (Basic, Digest,
//! HTTPS with a self-signed certificate) and moto from the Python version's
//! test venv (`../fcmd/.venv`, or `$OC_TEST_PYTHON`), MinIO in Docker (real
//! signature checks), a tiny in-process server for edge cases, and, with
//! `--ignored`, Amazon itself. Tests whose server isn't available are skipped.

use std::io;

pub mod dates;
pub mod http;
pub mod providers;
pub mod s3;
pub mod tls;
pub mod webdav;

pub use providers::{PlainS3Provider, PlainWebDavProvider, S3Provider, WebDavProvider, register_builtin};
pub use s3::S3FileSystem;
pub use tls::CertificateQuestion;
pub use webdav::WebDavFileSystem;

/// Shown in certificate prompts and the User-Agent.
pub const APP_NAME: &str = "Orchard Commander";

/// What can go wrong talking to a server. Converts into [`oc_vfs::VfsError`]
/// (file system operations) and [`oc_provider::ConnectError`] (connecting).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The server rejected the credentials (or a secret is missing).
    #[error("{0}")]
    AuthenticationRequired(String),
    /// The server's certificate differs from the one the user trusted: possible attack.
    #[error("{0}")]
    CertificateChanged(String),
    /// The user declined to trust the server's certificate.
    #[error("{0}")]
    CertificateRejected(String),
    /// The server can't be reached, the connection broke or timed out.
    #[error("{0}")]
    Connection(String),
    /// What the server answered, as an OS-style error (not found, permission denied, …).
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// The `io::ErrorKind` of an [`Error::Io`]; None for the other variants.
    pub fn io_kind(&self) -> Option<io::ErrorKind> {
        match self {
            Error::Io(e) => Some(e.kind()),
            _ => None,
        }
    }

    pub(crate) fn is_not_found(&self) -> bool {
        self.io_kind() == Some(io::ErrorKind::NotFound)
    }

    pub(crate) fn is_permission(&self) -> bool {
        self.io_kind() == Some(io::ErrorKind::PermissionDenied)
    }

    pub(crate) fn is_exists(&self) -> bool {
        self.io_kind() == Some(io::ErrorKind::AlreadyExists)
    }

    /// An `Error::Io` with this kind and message.
    pub(crate) fn os(kind: io::ErrorKind, message: impl Into<String>) -> Error {
        Error::Io(io::Error::new(kind, message.into()))
    }
}

/// Carried inside `VfsError::Io` for errors that aren't plain OS errors, so
/// the UI can tell them apart: `e.get_ref().and_then(|e| e.downcast_ref::<RemoteError>())`.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("{0}")]
    AuthenticationRequired(String),
    #[error("{0}")]
    CertificateChanged(String),
    #[error("{0}")]
    CertificateRejected(String),
    #[error("{0}")]
    Connection(String),
}

impl From<Error> for oc_vfs::VfsError {
    fn from(e: Error) -> Self {
        let (kind, inner) = match e {
            Error::Io(e) => return oc_vfs::VfsError::Io(e),
            Error::AuthenticationRequired(m) => (io::ErrorKind::PermissionDenied, RemoteError::AuthenticationRequired(m)),
            Error::CertificateChanged(m) => (io::ErrorKind::PermissionDenied, RemoteError::CertificateChanged(m)),
            Error::CertificateRejected(m) => (io::ErrorKind::Interrupted, RemoteError::CertificateRejected(m)),
            Error::Connection(m) => (io::ErrorKind::NotConnected, RemoteError::Connection(m)),
        };
        oc_vfs::VfsError::Io(io::Error::new(kind, inner))
    }
}

impl From<Error> for oc_provider::ConnectError {
    fn from(e: Error) -> Self {
        use oc_provider::ConnectError;
        match e {
            Error::AuthenticationRequired(m) => ConnectError::AuthenticationRequired(m),
            Error::CertificateRejected(_) => ConnectError::Cancelled,
            Error::CertificateChanged(m) | Error::Connection(m) => ConnectError::Connection(m),
            Error::Io(e) => ConnectError::Io(e),
        }
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        match oc_vfs::VfsError::from(e) {
            oc_vfs::VfsError::Io(e) => e,
            other => io::Error::other(other.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Connection error messages
// ---------------------------------------------------------------------------

/// A connection error message, with a hint for macOS's Local Network privacy.
///
/// macOS (15+) silently blocks programs without Local Network permission from
/// reaching devices on the LAN; they just see "No route to host".
pub fn describe_connect_error(host: &str, port: u16, err: &io::Error) -> String {
    let what = match err.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => "timed out".to_string(),
        _ => {
            // The OS's description without std's " (os error N)" suffix (as oc-local does).
            let text = err.to_string();
            match text.rfind(" (os error ") {
                Some(i) if text.ends_with(')') => text[..i].to_string(),
                _ => text,
            }
        }
    };
    let mut message = format!("Cannot connect to {host}:{port}: {what}");
    if cfg!(target_os = "macos") && is_host_unreachable(err) && is_local(host) {
        message.push_str(&format!(
            ". macOS may be blocking local network access for this program: allow it in \
             System Settings → Privacy & Security → Local Network (look for {APP_NAME}), then retry."
        ));
    }
    message
}

fn is_host_unreachable(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::HostUnreachable || err.to_string().contains("No route to host")
}

fn is_local(host: &str) -> bool {
    use std::net::{IpAddr, ToSocketAddrs};
    let Ok(addresses) = (host, 0).to_socket_addrs() else {
        return host.ends_with(".local");
    };
    addresses.into_iter().any(|a| match a.ip() {
        IpAddr::V4(ip) => ip.is_private() || ip.is_link_local() || ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_unique_local() || ip.is_unicast_link_local() || ip.is_loopback(),
    })
}

// ---------------------------------------------------------------------------
// Small path helpers shared by the file systems (Python's posixpath)
// ---------------------------------------------------------------------------

/// `posixpath.normpath("/" + path.lstrip("/"))`: an absolute path without
/// ".", "..", empty segments or a trailing slash ("/" stays "/").
pub(crate) fn norm_abs(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    format!("/{}", parts.join("/"))
}

/// `posixpath.dirname` of a normalized absolute path.
pub(crate) fn dirname(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => "",
    }
}

/// `posixpath.join(path, *names)`.
pub(crate) fn posix_join(path: &str, names: &[&str]) -> String {
    let mut out = path.to_string();
    for name in names {
        if name.starts_with('/') {
            out = name.to_string();
        } else if out.is_empty() || out.ends_with('/') {
            out.push_str(name);
        } else {
            out.push('/');
            out.push_str(name);
        }
    }
    out
}

/// `posixpath.basename(path.rstrip("/"))`.
pub(crate) fn base_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rfind('/').map_or(trimmed, |i| &trimmed[i + 1..])
}

/// The parent of an absolute path, None at "/".
pub(crate) fn parent_abs(path: &str) -> Option<String> {
    let path = norm_abs(path);
    if path == "/" { None } else { Some(dirname(&path).to_string()) }
}

// ---------------------------------------------------------------------------
// Which file systems are still connected (for FileSystemProvider::is_connected)
// ---------------------------------------------------------------------------

pub(crate) mod live {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, OnceLock, Weak};

    fn registry() -> &'static Mutex<HashMap<String, Weak<AtomicBool>>> {
        static R: OnceLock<Mutex<HashMap<String, Weak<AtomicBool>>>> = OnceLock::new();
        R.get_or_init(Default::default)
    }

    /// A unique identity for a new file system, whose `closed` flag is looked
    /// up by [`is_connected`].
    pub fn register(kind: &str, closed: &Arc<AtomicBool>) -> String {
        static N: AtomicU64 = AtomicU64::new(1);
        let id = format!("{kind}#{}", N.fetch_add(1, Ordering::Relaxed));
        let mut r = registry().lock().unwrap_or_else(|e| e.into_inner());
        r.retain(|_, w| w.strong_count() > 0);
        r.insert(id.clone(), Arc::downgrade(closed));
        id
    }

    /// Whether the file system with this identity is still open (true for
    /// file systems that aren't ours).
    pub fn is_connected(identity: &str) -> bool {
        let r = registry().lock().unwrap_or_else(|e| e.into_inner());
        match r.get(identity).and_then(Weak::upgrade) {
            Some(closed) => !closed.load(Ordering::SeqCst),
            None => !r.contains_key(identity),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_paths() {
        assert_eq!(norm_abs(""), "/");
        assert_eq!(norm_abs("//a/./b/../c/"), "/a/c");
        assert_eq!(norm_abs("/.."), "/");
        assert_eq!(dirname("/a/b"), "/a");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(posix_join("/a", &["b", "c"]), "/a/b/c");
        assert_eq!(posix_join("/", &["b"]), "/b");
        assert_eq!(posix_join("/a", &["/x", "y"]), "/x/y");
        assert_eq!(base_name("/a/b/"), "b");
        assert_eq!(base_name("/"), "");
        assert_eq!(parent_abs("/a/b"), Some("/a".into()));
        assert_eq!(parent_abs("/a"), Some("/".into()));
        assert_eq!(parent_abs("/"), None);
    }

    #[test]
    fn connect_errors() {
        let e = io::Error::new(io::ErrorKind::ConnectionRefused, "Connection refused");
        assert_eq!(describe_connect_error("nas", 445, &e), "Cannot connect to nas:445: Connection refused");
        let e = io::Error::new(io::ErrorKind::WouldBlock, "Resource temporarily unavailable");
        assert_eq!(describe_connect_error("nas", 80, &e), "Cannot connect to nas:80: timed out");
        let e = io::Error::from_raw_os_error(61);
        let message = describe_connect_error("nas", 80, &e);
        assert!(message.starts_with("Cannot connect to nas:80: ") && !message.contains("os error"), "{message}");
    }
}
