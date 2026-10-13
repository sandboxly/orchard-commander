//! SFTP (SSH) file system: a port of `remote/sftp.py` and the `SFTPProvider`
//! of the Python version, on pure-Rust russh + russh-sftp.
//!
//! - [`SftpFileSystem`] implements [`oc_vfs::FileSystem`] with a blocking
//!   facade over an internal tokio runtime; it is thread-safe, and
//!   listings, reads and writes from several threads run concurrently.
//! - [`SftpProvider`] is the `sftp://` connection type; [`register_builtin`]
//!   adds it to the `oc_provider` registry.
//! - Host keys are checked against `<config dir>/known_hosts` (and the
//!   user's `~/.ssh/known_hosts`) in the format paramiko writes, so hosts
//!   trusted by 1.x stay trusted; unknown keys are confirmed by the user,
//!   changed ones refused.
//! - Authentication: a given password is used alone (agent keys could
//!   exhaust MaxAuthTries first); otherwise the key file, the SSH agent and
//!   `~/.ssh/id_*`, as paramiko does.

mod connect;
mod fs;
pub mod known_hosts;
mod paths;
mod provider;
mod runtime;
mod session;
mod stream;
#[cfg(feature = "testing")]
pub mod testing;

use std::sync::Arc;

pub use connect::{Agent, ConnectOptions, DEFAULT_PORT, HostKeyQuestion, connect, describe_connect_error, expand_user};
pub use fs::SftpFileSystem;
pub use provider::{SftpProvider, host_key_message};

/// Register the `sftp://` connection type (keeps one that is already registered).
pub fn register_builtin() {
    let _ = oc_provider::register(Arc::new(SftpProvider), false);
}
