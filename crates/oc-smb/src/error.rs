//! Turning SMB failures into the errors and messages users see.
//!
//! Answers about a file read like local errors
//! (`\\server\share\x not found`, [`oc_vfs::io_message`]); connection-level
//! failures name the share (`smb://server/share: the connection was closed`).
//! The `io::ErrorKind` is kept so `VfsError::is_not_found` and friends work.

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
    /// `\\server\share\x not found`, or a connection failure as
    /// `smb://server/share: what happened`.
    pub(crate) fn into_vfs(self, type_name: &str, unc: &str) -> VfsError {
        if let Some(kind) = self.as_os_error() {
            return oc_vfs::io_error_at(kind.into(), unc);
        }
        if let Some(status) = self.status() {
            let what = describe_status(status);
            return VfsError::Io(io::Error::other(format!("{}: {unc}", capitalized(&what))));
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

    /// The local error kind for answers about a file that have one.
    fn as_os_error(&self) -> Option<io::ErrorKind> {
        use io::ErrorKind as K;
        if self.status().is_some_and(|s| s.0 == DIRECTORY_NOT_EMPTY) {
            return Some(K::DirectoryNotEmpty);
        }
        self.status()?; // no status: not an answer about the file but a connection problem
        Some(match self.kind()? {
            Kind::NotFound => K::NotFound,
            Kind::AlreadyExists => K::AlreadyExists,
            Kind::AccessDenied => K::PermissionDenied,
            Kind::IsADirectory => K::IsADirectory,
            Kind::NotADirectory => K::NotADirectory,
            Kind::SharingViolation => K::ResourceBusy,
            Kind::DiskFull => K::StorageFull,
            _ => return None,
        })
    }

    /// A short human description (no path).
    pub(crate) fn describe(&self) -> String {
        match self {
            Failure::Closed => "the connection was closed".into(),
            Failure::TimedOut => "the server didn't answer in time".into(),
            Failure::Smb(smb2::Error::Protocol { status, .. }) => describe_status(*status),
            Failure::Smb(smb2::Error::ConnectFailed { attempts, .. }) => match attempts.first() {
                Some(a) => oc_provider::describe_network_error(a.error_kind.unwrap_or(io::ErrorKind::TimedOut), ""),
                None => "the server name wasn't found — check the address".into(),
            },
            Failure::Smb(smb2::Error::Io(e)) => oc_provider::describe_network_error(e.kind(), &oc_vfs::os_text(e)),
            Failure::Smb(e) => match e.kind() {
                Kind::ConnectionLost => "the connection was lost".into(),
                Kind::TimedOut => "the server didn't answer in time".into(),
                Kind::Cancelled => "cancelled".into(),
                Kind::SessionExpired => "the session expired — connect again".into(),
                Kind::AuthRequired | Kind::AccessDenied => {
                    "the server didn't accept the user name or password — check them in the connection settings"
                        .into()
                }
                _ => {
                    let text = e.to_string();
                    let mut chars = text.chars();
                    chars.next().map(|c| c.to_lowercase().chain(chars).collect()).unwrap_or_default()
                }
            },
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

/// What an SMB status means, in words (lower case, for after a colon).
fn describe_status(status: NtStatus) -> String {
    match status.0 {
        0xC000_00CC => "the share doesn't exist — check the share name".into(),
        0xC000_006D => {
            "the server didn't accept the user name or password — check them in the connection settings".into()
        }
        0xC000_0022 => "you don't have permission".into(),
        0xC000_0034 | 0xC000_003A | 0xC000_000F => "not found".into(),
        0xC000_0035 => "the name already exists".into(),
        0xC000_0043 => "the file is in use by another program".into(),
        DIRECTORY_NOT_EMPTY => "the folder isn't empty".into(),
        0xC000_007F => "the server's disk is full".into(),
        0xC000_00BB => "the server doesn't support this".into(),
        code => format!("the server couldn't do this (error 0x{code:08X})"),
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// The message for a failed connect, with a hint for macOS's Local Network
/// privacy, which silently blocks programs without that permission from
/// reaching devices on the LAN (they just see "No route to host").
pub(crate) fn describe_connect_error(host: &str, port: u16, failure: &Failure) -> String {
    let unreachable = failure.connect_io_kind() == Some(io::ErrorKind::HostUnreachable) && is_local(host);
    oc_provider::connect_error_message(host, port, &failure.describe(), unreachable)
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

#[cfg(test)]
mod tests {
    use super::*;
    use smb2::types::Command;

    fn status(code: u32) -> Failure {
        Failure::Smb(smb2::Error::Protocol { status: NtStatus(code), command: Command::Create })
    }

    #[test]
    fn file_errors_read_like_local_ones() {
        let e = status(0xC000_0034).into_vfs("smb://h/S", r"\\h\S\x");
        assert!(e.is_not_found());
        assert_eq!(e.to_string(), r"\\h\S\x not found");
        let e = status(0xC000_0035).into_vfs("smb://h/S", r"\\h\S\x");
        assert!(matches!(&e, VfsError::Io(io) if io.kind() == io::ErrorKind::AlreadyExists));
        let e = status(DIRECTORY_NOT_EMPTY).into_vfs("smb://h/S", r"\\h\S\d");
        assert!(matches!(&e, VfsError::Io(io) if io.kind() == io::ErrorKind::DirectoryNotEmpty));
        let e = status(0xC000_0022).into_vfs("smb://h/S", r"\\h\S\x");
        assert_eq!(e.to_string(), r"You don't have permission to access \\h\S\x");
        let e = status(0xC000_00BB).into_vfs("smb://h/S", r"\\h\S\x");
        assert_eq!(e.to_string(), r"The server doesn't support this: \\h\S\x");
        let e = status(0xC000_0061).into_vfs("smb://h/S", r"\\h\S\x");
        assert_eq!(e.to_string(), r"The server couldn't do this (error 0xC0000061): \\h\S\x");
    }

    #[test]
    fn connection_errors_name_the_server() {
        let e = Failure::Smb(smb2::Error::Disconnected).into_vfs("smb://h/S", r"\\h\S\x");
        assert_eq!(e.to_string(), "smb://h/S: the connection was lost");
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
            "Can't connect to 127.0.0.1:1: the connection was refused — check the port and that the server is running"
        );
        let no_share = status(0xC000_00CC);
        assert_eq!(
            describe_connect_error("nas", 445, &no_share),
            "Can't connect to nas:445: the share doesn't exist — check the share name"
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
