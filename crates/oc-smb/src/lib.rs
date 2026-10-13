//! SMB (Windows / Samba share) file system: port of `remote/smb.py` and
//! `SMBProvider` from `remote/providers.py`.
//!
//! One [`SmbFileSystem`] is one share. Paths are POSIX-style absolute paths
//! inside the share ("/" is the share root); `display` shows them as
//! `smb://server[:port]/share/path`, exactly like the Python version, and
//! [`SmbProvider`] formats addresses and keychain account strings the same
//! way (`smb://user@server/share`).
//!
//! # Client library
//!
//! Built on [`smb2`](https://crates.io/crates/smb2), a pure-Rust SMB 2/3
//! client (no libsmbclient, no C): NTLM (user / password / domain from
//! `DOMAIN\user`) and Kerberos, signing, encryption, DFS, and pipelined I/O.
//! The `smb` crate (smb-rs) was evaluated first: it builds and works, but
//! pulls in ~280 crates (sspi, picky, reqwest), selects sync vs. async with
//! mutually exclusive cargo features, and issues one READ at a time. `smb2`
//! is far lighter, was written for a file manager, and has the pieces this
//! crate needs directly: positioned reads on one handle (`FileReader`), raw
//! CREATE/WRITE for a share-mode-controlled writer, `set_times`, a credit
//! pool that *waits* for credits (smbprotocol failed instead, which is why
//! the Python version serialized everything), and response/send timeouts.
//!
//! # Concurrency
//!
//! As in Python, every request takes the connection's lock, so a listing
//! waits while a preview's read is answered; a NAS granting few credits is
//! never asked for more than it can fund. File reads use shared access
//! (read/write/delete) so a preview, a copy and video playback can read the
//! same file. All network waits are bounded (the library's per-request
//! timeouts plus an outer ceiling here), so nothing blocks forever.
//!
//! The async client runs on a small shared tokio runtime; the
//! synchronous `FileSystem` methods hand it work and wait on a channel, which
//! is safe from any thread.

mod error;
mod fs;
mod provider;
mod runtime;
mod stream;

use std::sync::Arc;

pub use fs::{ConnectOptions, DEFAULT_PORT, SmbConnectError, SmbFileSystem, connect, list_shares};
pub use provider::SmbProvider;

/// Register the `smb://` connection type (idempotent).
pub fn register_builtin() {
    // Ok with replace = true: re-registering just replaces the same provider.
    let _ = oc_provider::register(Arc::new(SmbProvider), true);
}
