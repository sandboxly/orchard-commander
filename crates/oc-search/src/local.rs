//! Entries for the parallel local walker, built straight from directory
//! entries so a name-only search can skip `stat` for names that don't match.
//!
//! Mirrors the local file system's rules (`fs.py` `_entry`): links are
//! followed for type/size/time, dangling links fall back to the link itself,
//! dot-names and the platform's hidden flags count as hidden.

use std::fs::{self, DirEntry, Metadata};
use std::time::UNIX_EPOCH;

use oc_vfs::Entry;

/// Whether hidden-ness can come from file attributes rather than the name
/// alone (then hiding hidden files needs a `stat` per entry).
pub(crate) const HIDDEN_ATTRIBUTES: bool = cfg!(any(target_os = "macos", windows));

/// Build the entry for `de`, following a symlink when `is_link`.
pub(crate) fn entry_from_dirent(de: &DirEntry, name: String, is_link: bool) -> Entry {
    let lstat = match de.metadata() {
        Ok(m) => m,
        Err(_) => {
            // Unreadable: a plain file entry, like the Python version.
            let hidden = name.starts_with('.');
            return Entry { is_hidden: hidden, ..Entry::file(name, 0, 0.0) };
        }
    };
    let followed = if is_link { fs::metadata(de.path()).ok() } else { None };
    let eff = followed.as_ref().unwrap_or(&lstat);
    let is_dir = eff.is_dir();
    Entry {
        is_dir,
        is_link,
        is_hidden: name.starts_with('.') || attr_hidden(eff),
        is_executable: !is_dir && is_executable(&name, eff),
        size: if is_dir { 0 } else { eff.len() },
        mtime: mtime(eff),
        is_online_only: !is_dir && online_only(eff),
        name,
    }
}

fn mtime(m: &Metadata) -> f64 {
    match m.modified().map(|t| t.duration_since(UNIX_EPOCH)) {
        Ok(Ok(d)) => d.as_secs_f64(),
        Ok(Err(before_epoch)) => -before_epoch.duration().as_secs_f64(),
        Err(_) => 0.0,
    }
}

#[cfg(target_os = "macos")]
fn st_flags(m: &Metadata) -> u32 {
    use std::os::macos::fs::MetadataExt;
    m.st_flags()
}

#[cfg(target_os = "macos")]
fn attr_hidden(m: &Metadata) -> bool {
    const UF_HIDDEN: u32 = 0x8000;
    st_flags(m) & UF_HIDDEN != 0
}

/// File Provider placeholders (Dropbox, iCloud Drive, OneDrive) on APFS.
#[cfg(target_os = "macos")]
fn online_only(m: &Metadata) -> bool {
    const SF_DATALESS: u32 = 0x4000_0000;
    st_flags(m) & SF_DATALESS != 0
}

#[cfg(windows)]
fn attr_hidden(m: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const HIDDEN_OR_SYSTEM: u32 = 0x2 | 0x4;
    m.file_attributes() & HIDDEN_OR_SYSTEM != 0
}

/// RECALL_ON_DATA_ACCESS | RECALL_ON_OPEN | OFFLINE.
#[cfg(windows)]
fn online_only(m: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    m.file_attributes() & (0x0040_0000 | 0x0004_0000 | 0x0000_1000) != 0
}

#[cfg(not(any(target_os = "macos", windows)))]
fn attr_hidden(_m: &Metadata) -> bool {
    false
}

#[cfg(not(any(target_os = "macos", windows)))]
fn online_only(_m: &Metadata) -> bool {
    false
}

#[cfg(unix)]
fn is_executable(_name: &str, m: &Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    m.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(name: &str, _m: &Metadata) -> bool {
    matches!(oc_vfs::ext_of(name).as_str(), "exe" | "bat" | "cmd" | "com" | "ps1")
}
