//! Read-only access to archives as if they were directories.
//!
//! Supported: zip, tar (plain/gz/bz2/xz), single-file gzip/bzip2/xz, 7z and
//! rar — all in-process (the RAR decoder is the vendored unrar library, behind
//! the default `rar` cargo feature), so, unlike the Python version, no
//! external extraction tool is needed.
//!
//! Call [`register`] once at app start: it plugs [`open_archive`] into
//! [`oc_vfs::open_archive`] so panels, previews and searches can enter
//! archives without depending on this crate.
//!
//! Inside an archive, paths are POSIX strings relative to the archive root,
//! with "" meaning the root. An [`Archive`] is shared between the UI and
//! worker threads: its member index is immutable after opening, and every
//! read gets its own stream, so concurrent reads never wait on each other.
//! The rest of the app reaches archives through [`ArchiveFS`] (see
//! [`open_archive`]). Port of `archives.py` and the archive half of `vfs.py`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use oc_vfs::VfsError;

mod archive;
mod fs;
mod io;
#[cfg(feature = "rar")]
mod rar;
mod sevenz;
mod single;
mod tar;
mod zip;

pub use archive::{Archive, Member};
pub use fs::{ArchiveFS, is_archive, open_archive};

/// Register the archive back ends with `oc_vfs` (see [`oc_vfs::set_archive_opener`]
/// and [`oc_vfs::set_archive_name_test`]). Idempotent; call once at start.
pub fn register() {
    oc_vfs::set_archive_name_test(std::sync::Arc::new(is_archive_name));
    oc_vfs::set_archive_opener(std::sync::Arc::new(|file: &oc_vfs::VPath| {
        if !is_archive_name(&file.name()) {
            return None;
        }
        Some(open_archive(file).map_err(VfsError::from))
    }));
}

/// What kind of archive a file name says it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArchiveKind {
    Zip,
    Tar,
    SevenZ,
    Rar,
    /// A single gzip-compressed file.
    Gz,
    /// A single bzip2-compressed file.
    Bz2,
    /// A single xz- (or legacy lzma-) compressed file.
    Xz,
}

impl ArchiveKind {
    /// The short name used in the file system's type name ("zip archive").
    pub fn as_str(self) -> &'static str {
        match self {
            ArchiveKind::Zip => "zip",
            ArchiveKind::Tar => "tar",
            ArchiveKind::SevenZ => "7z",
            ArchiveKind::Rar => "rar",
            ArchiveKind::Gz => "gz",
            ArchiveKind::Bz2 => "bz2",
            ArchiveKind::Xz => "xz",
        }
    }
}

/// Longest suffixes first so "x.tar.gz" is a tarball, not a gzip file.
const SUFFIXES: &[(&str, ArchiveKind)] = &[
    (".tar.gz", ArchiveKind::Tar),
    (".tar.bz2", ArchiveKind::Tar),
    (".tar.xz", ArchiveKind::Tar),
    (".tgz", ArchiveKind::Tar),
    (".tbz", ArchiveKind::Tar),
    (".tbz2", ArchiveKind::Tar),
    (".txz", ArchiveKind::Tar),
    (".tar", ArchiveKind::Tar),
    (".zip", ArchiveKind::Zip),
    (".7z", ArchiveKind::SevenZ),
    #[cfg(feature = "rar")]
    (".rar", ArchiveKind::Rar),
    (".gz", ArchiveKind::Gz),
    (".bz2", ArchiveKind::Bz2),
    (".xz", ArchiveKind::Xz),
    (".lzma", ArchiveKind::Xz),
];

/// The archive kind of a file name, by suffix (case-insensitive). A bare
/// suffix (".zip") is a hidden file, not an archive.
pub fn archive_type(name: &str) -> Option<ArchiveKind> {
    let bytes = name.as_bytes();
    SUFFIXES.iter().find_map(|&(suffix, kind)| {
        let n = suffix.len();
        (bytes.len() > n && bytes[bytes.len() - n..].eq_ignore_ascii_case(suffix.as_bytes()))
            .then_some(kind)
    })
}

/// Whether a file name looks like a supported archive (see [`is_archive`]
/// for the VPath version that also rules out directories).
pub fn is_archive_name(name: &str) -> bool {
    archive_type(name).is_some()
}

/// An archive can't be opened or read (corrupt, encrypted, unsupported).
/// One error type so the UI can show a single message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ArchiveError(pub String);

impl From<ArchiveError> for VfsError {
    fn from(e: ArchiveError) -> Self {
        VfsError::Operation(e.0)
    }
}

/// Clean a member name; None for names that would escape the archive root
/// ("../x"). Backslashes count as separators, empty and "." parts vanish, so
/// "/abs/x" becomes "abs/x" and "a/./b/" becomes "a/b". "" means the root
/// (such members are skipped by the formats).
pub fn normalize(name: &str) -> Option<String> {
    let mut out = String::with_capacity(name.len());
    for part in name.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return None,
            p => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(p);
            }
        }
    }
    Some(out)
}

/// Like `os.path.normpath` for link targets: ".." pops a part; None if it
/// climbs above the root.
pub(crate) fn normpath(name: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in name.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

/// POSIX dirname of a member name ("" at the top level).
pub(crate) fn dirname(name: &str) -> &str {
    name.rfind('/').map_or("", |i| &name[..i])
}

/// POSIX basename of a member name.
pub(crate) fn basename(name: &str) -> &str {
    name.rfind('/').map_or(name, |i| &name[i + 1..])
}

/// A local wall-clock time (zip and RAR4 store these) as seconds since the
/// epoch, like Python's `time.mktime` (DST resolved by the system zone).
pub(crate) fn local_time_to_epoch(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> f64 {
    use chrono::{LocalResult, NaiveDate, Offset, TimeZone};
    let Some(naive) = NaiveDate::from_ymd_opt(y, mo, d).and_then(|date| date.and_hms_opt(h, mi, s))
    else {
        return 0.0;
    };
    match chrono::Local.from_local_datetime(&naive) {
        LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => t.timestamp() as f64,
        // A time skipped by a DST switch: mktime still answers; use the zone's offset there.
        LocalResult::None => {
            let off = chrono::Local
                .offset_from_utc_datetime(&naive)
                .fix()
                .local_minus_utc();
            (naive.and_utc().timestamp() - i64::from(off)) as f64
        }
    }
}

/// An MS-DOS date/time (zip, RAR) in local time as seconds since the epoch.
#[cfg_attr(not(feature = "rar"), allow(dead_code))]
pub(crate) fn dos_time_to_epoch(dos: u32) -> f64 {
    let (date, time) = (dos >> 16, dos & 0xFFFF);
    local_time_to_epoch(
        1980 + (date >> 9) as i32,
        (date >> 5) & 0xF,
        date & 0x1F,
        time >> 11,
        (time >> 5) & 0x3F,
        (time & 0x1F) * 2,
    )
}

pub(crate) fn file_mtime(meta: &std::fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64())
}

/// A short stable key for cache directory names. Not cryptographic: the
/// cache lives only as long as the process.
pub(crate) fn cache_key(parts: impl Hash) -> String {
    let mut h = DefaultHasher::new();
    parts.hash(&mut h);
    format!("{:016x}", h.finish())
}

// ---------------------------------------------------------------------------
// Opening with a small cache
// ---------------------------------------------------------------------------

/// How many opened archives to keep. Reopening is cheap for zip but means
/// decompressing the whole stream for a .tar.gz.
const OPEN_CACHE_SIZE: usize = 8;

type CacheKey = (PathBuf, u64, u64);

static OPEN_CACHE: Mutex<Vec<(CacheKey, Arc<Archive>)>> = Mutex::new(Vec::new());

/// Open (or reuse an already opened) archive on the local disk.
///
/// The cache is keyed by path, mtime and size, so a changed file is read
/// again. Evicted archives may still be in use (open panels, search hits):
/// eviction only closes their file handle, they reopen it when read again.
pub fn open_archive_file(path: &Path) -> Result<Arc<Archive>, ArchiveError> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let kind = archive_type(&name)
        .ok_or_else(|| ArchiveError(format!("Not a supported archive: {name}")))?;
    let meta = std::fs::metadata(path).map_err(|e| ArchiveError(e.to_string()))?;
    let key: CacheKey = (path.to_path_buf(), file_mtime(&meta).to_bits(), meta.len());
    {
        let mut cache = OPEN_CACHE.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(i) = cache.iter().position(|(k, _)| *k == key) {
            let hit = cache.remove(i);
            let archive = hit.1.clone();
            cache.push(hit);
            return Ok(archive);
        }
    }
    let archive = Arc::new(Archive::open_kind(path, kind)?);
    let evicted: Vec<Arc<Archive>> = {
        let mut cache = OPEN_CACHE.lock().unwrap_or_else(|p| p.into_inner());
        cache.retain(|(k, _)| *k != key);
        cache.push((key, archive.clone()));
        let excess = cache.len().saturating_sub(OPEN_CACHE_SIZE);
        cache.drain(..excess).map(|(_, a)| a).collect()
    };
    for old in evicted {
        old.close();
    }
    Ok(archive)
}

// ---------------------------------------------------------------------------
// Temp cache for previewing / opening members
// ---------------------------------------------------------------------------

static CACHE_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// This process's temp directory for extracted members (created on first
/// use). The app should call [`cleanup_cache_dir`] when it exits.
pub fn cache_dir() -> std::io::Result<PathBuf> {
    let mut dir = CACHE_DIR.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(d) = dir.as_ref() {
        return Ok(d.clone());
    }
    let d = tempfile::Builder::new()
        .prefix("orchard-commander-")
        .tempdir()?
        .keep();
    *dir = Some(d.clone());
    Ok(d)
}

/// Remove the temp cache (Python registered this with `atexit`). Safe to call
/// more than once; a later [`cache_dir`] call creates a fresh directory.
pub fn cleanup_cache_dir() {
    if let Some(d) = CACHE_DIR.lock().unwrap_or_else(|p| p.into_inner()).take() {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// Extract a file member to the temp cache (once) and return its path.
///
/// The file keeps its original name, so its extension still identifies it.
pub fn extract_cached(archive: &Archive, name: &str) -> oc_vfs::Result<PathBuf> {
    let key = cache_key((archive.path(), archive.mtime().to_bits(), name));
    let target = cache_dir()?.join(key).join(basename(name));
    if !target.exists() {
        std::fs::create_dir_all(target.parent().expect("has a parent"))?;
        archive.extract_file(name, &target, None)?;
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_type_detection() {
        assert_eq!(archive_type("x.tar.gz"), Some(ArchiveKind::Tar));
        assert_eq!(archive_type("X.TGZ"), Some(ArchiveKind::Tar));
        assert_eq!(archive_type("x.gz"), Some(ArchiveKind::Gz));
        assert_eq!(archive_type("x.zip"), Some(ArchiveKind::Zip));
        assert_eq!(archive_type("x.tbz2"), Some(ArchiveKind::Tar));
        assert_eq!(archive_type("x.txz"), Some(ArchiveKind::Tar));
        assert_eq!(archive_type("x.lzma"), Some(ArchiveKind::Xz));
        assert_eq!(archive_type("x.7Z"), Some(ArchiveKind::SevenZ));
        #[cfg(feature = "rar")]
        assert_eq!(archive_type("x.rar"), Some(ArchiveKind::Rar));
        #[cfg(not(feature = "rar"))]
        assert_eq!(archive_type("x.rar"), None);
        assert_eq!(archive_type(".zip"), None);
        assert_eq!(archive_type(".tar.gz"), Some(ArchiveKind::Gz)); // like Python: ".tar" + ".gz"
        assert_eq!(archive_type("x.txt"), None);
        assert_eq!(archive_type("ü.zip"), Some(ArchiveKind::Zip));
        assert_eq!(archive_type("ü"), None);
        assert!(is_archive_name("a.bz2"));
    }

    #[test]
    fn normalize_rules() {
        assert_eq!(normalize("../escape.txt"), None);
        assert_eq!(normalize("a/../b"), None);
        assert_eq!(normalize("ok/./fine.txt").as_deref(), Some("ok/fine.txt"));
        assert_eq!(normalize("/abs//x/").as_deref(), Some("abs/x"));
        assert_eq!(
            normalize("win\\path\\f.txt").as_deref(),
            Some("win/path/f.txt")
        );
        assert_eq!(normalize("./").as_deref(), Some(""));
        assert_eq!(normalize("..foo").as_deref(), Some("..foo"));
    }

    #[test]
    fn normpath_rules() {
        assert_eq!(normpath("a/b/../c").as_deref(), Some("a/c"));
        assert_eq!(normpath("../x"), None);
        assert_eq!(normpath("a//./b").as_deref(), Some("a/b"));
    }

    #[test]
    fn dos_times_decode() {
        // 2020-06-15 12:34:56 local
        let dos = ((40 << 25) | (6 << 21) | (15 << 16) | (12 << 11) | (34 << 5) | 28) as u32;
        let t = dos_time_to_epoch(dos);
        let expect = local_time_to_epoch(2020, 6, 15, 12, 34, 56);
        assert_eq!(t, expect);
        assert!(t > 1.5e9);
        assert_eq!(dos_time_to_epoch(0), 0.0); // month/day 0: invalid
    }
}
