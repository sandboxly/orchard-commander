//! Settings and other persisted user data: settings, color schemes, bookmarks,
//! saved connections (passwords in the OS keychain).
//!
//! Everything is read from and written to the same files, in the same
//! formats, as the Python version (`config.py`, `themes.py`, `bookmarks.py`,
//! `locations.py`, the storage half of `connections.py`), so users keep their
//! data when switching. Files are re-read on every load rather than cached:
//! several app instances may share them.

pub mod bookmarks;
pub mod connections;
pub mod locations;
pub mod paths;
mod pyjson;
pub mod settings;
pub mod themes;

pub use bookmarks::{Bookmark, Folder, Item};
pub use connections::{
    Connection, CredentialError, CredentialStore, MemoryCredentials, OsKeyring, Passwords,
};
pub use locations::{Spec, SpecContext, SpecRef, Target};
pub use paths::{APP_NAME, COMMAND, ConfigPaths};
pub use settings::Settings;
pub use themes::{Appearance, ColorScheme};

/// Errors with a message for the user (e.g. a taken color scheme name) or from the disk.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
