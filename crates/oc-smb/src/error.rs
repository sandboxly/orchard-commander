//! Turning SMB failures into the errors and messages users see.
//!
//! The Python version translated smbprotocol's `SMBOSError` into plain
//! `OSError`s, so error dialogs read like local ones:
//! `[Errno 2] No such file or directory: '\\server\share\x'`. Connection-level
//! failures became `ConnectionError("smb://server/share: …")`. The same shapes
//! are produced here; the `io::ErrorKind` is kept so `VfsError::is_not_found`
//! and friends work.

use std::io;
use std::net::{IpAddr, ToSocketAddrs};

use oc_vfs::VfsError;
use smb2::ErrorKind as Kind;
use smb2::types::status::NtStatus;

/// What went wrong with one request.
#[derive(Debug)]
pub(crate) enum Failure {
    Smb(smb2::Error),
    /// The file system was closed.
    Closed,
    /// Our own outer deadline passed (the library's per-request timeouts
    /// should normally fire first).
    TimedOut,
}

impl From<smb2::Error> for Failure {
    fn from(e: smb2::Error) -> Self {
        Failure::Smb(e)
    }
}

pub(crate) type SmbResult<T> = std::result::Result<T, Failure>;

/// STATUS_DIRECTORY_NOT_EMPTY, which smb2 doesn't classify.
const DIRECTORY_NOT_EMPTY: u32 = 0xC000_0101;

impl Failure {
    pub(crate) fn kind(&self) -> Option<Kind> {
        match self {
            Failure::Smb(e) => Some(e.kind()),
            _ => None,
        }
    }

    pub(crate) fn status(&self) -> Option<NtStatus> {
        match self {
            Failure::Smb(e) => e.status(),
            _ => None,
        }
    }

    pub(crate) fn is_not_found(&self) -> bool {
        self.kind() == Some(Kind::NotFound)
    }

    pub(crate) fn is_already_exists(&self) -> bool {
        self.kind() == Some(Kind::AlreadyExists)
    }

    /// Credentials refused, or the account may not use the share.
    pub(crate) fn is_auth(&self) -> bool {
        matches!(self.kind(), Some(Kind::AuthRequired | Kind::AccessDenied | Kind::SigningRequired))
    }

    /// A file-level failure (the server answered about this path) as
    /// `[Errno N] reason: 'unc'`, or a connection failure as
    /// `smb://server/share: what happened`.
    pub(crate) fn into_vfs(self, type_name: &str, unc: &str) -> VfsError {
        if let Some((kind, errno, reason)) = self.as_os_error() {
            return VfsError::Io(io::Error::new(kind, format!("[Errno {errno}] {reason}: '{unc}'")));
        }
        if let Some(status) = self.status() {
            // Python: "[Error 0] [NtStatus 0xc0000043] Unknown NtStatus error returned 'STATUS_…': '…'"
            let name = status.to_string();
            let name = if name.starts_with("0x") { "STATUS_UNKNOWN".to_string() } else { name };
            return VfsError::Io(io::Error::other(format!(
                "[NtStatus 0x{:08x}] Unknown NtStatus error returned '{name}': '{unc}'",
                status.0
            )));
        }
        let kind = match &self {
            Failure::Closed => io::ErrorKind::NotConnected,
            Failure::TimedOut => io::ErrorKind::TimedOut,
            Failure::Smb(e) => match e.kind() {
                Kind::TimedOut => io::ErrorKind::TimedOut,
                Kind::Cancelled => io::ErrorKind::Interrupted,
                Kind::InvalidData => io::ErrorKind::InvalidData,
                _ => io::ErrorKind::ConnectionAborted,
            },
        };
        VfsError::Io(io::Error::new(kind, format!("{type_name}: {}", self.describe())))
    }

    /// (io kind, errno, strerror) for the statuses Python mapped to an errno.
    fn as_os_error(&self) -> Option<(io::ErrorKind, i32, &'static str)> {
        use io::ErrorKind as K;
        if self.status().is_some_and(|s| s.0 == DIRECTORY_NOT_EMPTY) {
            return Some((K::DirectoryNotEmpty, errno::ENOTEMPTY, "Directory not empty"));
        }
        self.status()?; // no status: not an answer about the file but a connection problem
        Some(match self.kind()? {
            Kind::NotFound => (K::NotFound, errno::ENOENT, "No such file or directory"),
            Kind::AlreadyExists => (K::AlreadyExists, errno::EEXIST, "File exists"),
            Kind::AccessDenied => (K::PermissionDenied, errno::EACCES, "Permission denied"),
            Kind::IsADirectory => (K::IsADirectory, errno::EISDIR, "Is a directory"),
            Kind::NotADirectory => (K::NotADirectory, errno::ENOTDIR, "Not a directory"),
            Kind::SharingViolation => (
                K::ResourceBusy,
                errno::EPERM,
                "The process cannot access the file because it is being used by another process",
            ),
            Kind::DiskFull => (K::StorageFull, errno::ENOSPC, "No space left on device"),
            _ => return None,
        })
    }

    /// A short human description (no path).
    pub(crate) fn describe(&self) -> String {
        match self {
            Failure::Closed => "the connection was closed".into(),
            Failure::TimedOut => "the server did not answer in time".into(),
            Failure::Smb(smb2::Error::Protocol { status, .. }) => describe_status(*status),
            Failure::Smb(smb2::Error::ConnectFailed { attempts, .. }) => match attempts.first() {
                Some(a) => describe_io_kind(a.error_kind),
                None => "the server name could not be resolved".into(),
            },
            Failure::Smb(smb2::Error::Io(e)) => e.to_string(),
            Failure::Smb(e) => e.to_string(),
        }
    }

    /// The socket-level reason, if connecting failed at that level.
    fn connect_io_kind(&self) -> Option<io::ErrorKind> {
        match self {
            Failure::Smb(smb2::Error::ConnectFailed { attempts, .. }) => attempts.iter().find_map(|a| a.error_kind),
            Failure::Smb(smb2::Error::Io(e)) => Some(e.kind()),
            _ => None,
        }
    }
}

fn describe_status(status: NtStatus) -> String {
    let what = match status.0 {
        0xC000_00CC => "the share does not exist",
        0xC000_006D => "the user name or password is wrong",
        0xC000_0022 => "access denied",
        0xC000_0034 | 0xC000_003A | 0xC000_000F => "no such file or directory",
        0xC000_0035 => "the name already exists",
        0xC000_0043 => "the file is in use",
        DIRECTORY_NOT_EMPTY => "the directory is not empty",
        0xC000_007F => "the disk is full",
        _ => "",
    };
    if what.is_empty() { status.to_string() } else { format!("{what} ({status})") }
}

fn describe_io_kind(kind: Option<io::ErrorKind>) -> String {
    use io::ErrorKind as K;
    match kind {
        Some(K::ConnectionRefused) => "Connection refused".into(),
        Some(K::HostUnreachable) => "No route to host".into(),
        Some(K::NetworkUnreachable) => "Network is unreachable".into(),
        Some(K::TimedOut) | None => "timed out".into(),
        Some(other) => other.to_string(),
    }
}

/// Port of `remote.describe_connect_error`: the message for a failed connect,
/// with a hint for macOS's Local Network privacy, which silently blocks
/// programs without that permission from reaching devices on the LAN (they
/// just see "No route to host").
pub(crate) fn describe_connect_error(host: &str, port: u16, failure: &Failure) -> String {
    let mut message = format!("Cannot connect to {host}:{port}: {}", failure.describe());
    if cfg!(target_os = "macos") && failure.connect_io_kind() == Some(io::ErrorKind::HostUnreachable) && is_local(host)
    {
        message.push_str(
            ". macOS may be blocking local network access for this program: allow it in \
             System Settings → Privacy & Security → Local Network (look for Orchard Commander), then retry.",
        );
    }
    message
}

/// Whether `host` resolves to a private / link-local address (Python's
/// `ipaddress` `is_private or is_link_local`).
pub(crate) fn is_local(host: &str) -> bool {
    let Ok(addrs) = (host, 0u16).to_socket_addrs() else {
        return host.ends_with(".local");
    };
    addrs.map(|a| a.ip()).any(|ip| match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    })
}

/// errno values as Python reports them on this platform.
mod errno {
    pub const EPERM: i32 = 1;
    pub const ENOENT: i32 = 2;
    pub const EACCES: i32 = 13;
    pub const EEXIST: i32 = 17;
    pub const ENOTDIR: i32 = 20;
    pub const EISDIR: i32 = 21;
    pub const ENOSPC: i32 = 28;
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    pub const ENOTEMPTY: i32 = 66;
    #[cfg(windows)]
    pub const ENOTEMPTY: i32 = 41;
    #[cfg(not(any(
        windows,
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    pub const ENOTEMPTY: i32 = 39;
}

#[cfg(test)]
mod tests {
    use super::*;
    use smb2::types::Command;

    fn status(code: u32) -> Failure {
        Failure::Smb(smb2::Error::Protocol { status: NtStatus(code), command: Command::Create })
    }

    #[test]
    fn file_errors_read_like_os_errors() {
        let e = status(0xC000_0034).into_vfs("smb://h/S", r"\\h\S\x");
        assert!(e.is_not_found());
        assert_eq!(e.to_string(), r"[Errno 2] No such file or directory: '\\h\S\x'");
        let e = status(0xC000_0035).into_vfs("smb://h/S", r"\\h\S\x");
        assert!(matches!(&e, VfsError::Io(io) if io.kind() == io::ErrorKind::AlreadyExists));
        let e = status(DIRECTORY_NOT_EMPTY).into_vfs("smb://h/S", r"\\h\S\d");
        assert!(matches!(&e, VfsError::Io(io) if io.kind() == io::ErrorKind::DirectoryNotEmpty));
        let e = status(0xC000_0022).into_vfs("smb://h/S", r"\\h\S\x");
        assert_eq!(e.to_string(), r"[Errno 13] Permission denied: '\\h\S\x'");
        let e = status(0xC000_00BB).into_vfs("smb://h/S", r"\\h\S\x");
        assert_eq!(
            e.to_string(),
            r"[NtStatus 0xc00000bb] Unknown NtStatus error returned 'STATUS_NOT_SUPPORTED': '\\h\S\x'"
        );
    }

    #[test]
    fn connection_errors_name_the_server() {
        let e = Failure::Smb(smb2::Error::Disconnected).into_vfs("smb://h/S", r"\\h\S\x");
        assert_eq!(e.to_string(), "smb://h/S: Disconnected from server");
        let e = Failure::TimedOut.into_vfs("smb://h/S", "");
        assert!(matches!(&e, VfsError::Io(io) if io.kind() == io::ErrorKind::TimedOut));
    }

    #[test]
    fn connect_messages() {
        let refused = Failure::Smb(smb2::Error::ConnectFailed {
            host: "127.0.0.1:1".into(),
            attempts: vec![smb2::transport::ConnectAttempt {
                addr: "127.0.0.1:1".parse().unwrap(),
                error_kind: Some(io::ErrorKind::ConnectionRefused),
            }],
        });
        assert_eq!(
            describe_connect_error("127.0.0.1", 1, &refused),
            "Cannot connect to 127.0.0.1:1: Connection refused"
        );
        let no_share = status(0xC000_00CC);
        assert_eq!(
            describe_connect_error("nas", 445, &no_share),
            "Cannot connect to nas:445: the share does not exist (STATUS_BAD_NETWORK_NAME)"
        );
        let unreachable = Failure::Smb(smb2::Error::ConnectFailed {
            host: "192.168.1.20:445".into(),
            attempts: vec![smb2::transport::ConnectAttempt {
                addr: "192.168.1.20:445".parse().unwrap(),
                error_kind: Some(io::ErrorKind::HostUnreachable),
            }],
        });
        let msg = describe_connect_error("192.168.1.20", 445, &unreachable);
        assert_eq!(msg.contains("Local Network"), cfg!(target_os = "macos"));
        assert!(!describe_connect_error("192.168.1.20", 445, &refused).contains("Local Network"));
    }

    #[test]
    fn local_addresses() {
        assert!(is_local("192.168.1.20") && is_local("10.0.0.1") && is_local("169.254.3.4") && is_local("fe80::1"));
        assert!(is_local("fd00::5") && is_local("127.0.0.1"));
        assert!(!is_local("8.8.8.8") && !is_local("2001:4860::8888"));
    }
}
