//! Data access for previewers, through the generic file system interface.
//!
//! Previewers never see which file system a file is on. They ask for bytes,
//! a stream or a path; the file system decides how to provide them
//! (`open_read`, `local_path`, or a copy in the preview cache — e.g. reading a
//! disk, extracting from an archive, downloading from a server).

use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use oc_vfs::{Entry, ReadStream, VPath, VfsError, human_size};

use crate::api::{FileInfo, PreviewError, ext_for_name};

/// How much `info(with_head)` reads for previewers that sniff contents.
pub const SNIFF_BYTES: usize = 8192;

#[derive(Default)]
struct HeadCache {
    bytes: Vec<u8>,
    /// `bytes` holds the whole file.
    complete: bool,
}

/// The file being previewed. Reads only what's asked for, and caches it.
///
/// - [`head`](Self::head): the first n bytes (one read, shared by all previewers)
/// - [`open_stream`](Self::open_stream): a stream from the start (seekable on most file systems)
/// - [`local_file`](Self::local_file): a real path — the file itself, or a copy in
///   the preview cache, refused above `max_size`
/// - [`read_all`](Self::read_all): all bytes in memory, with the same size limit
pub struct PreviewSource {
    target: VPath,
    entry: Entry,
    name: String,
    /// None: [`default_cache_dir`], created only when a copy is first needed.
    cache_dir: Option<PathBuf>,
    head: Mutex<HeadCache>,
    direct: OnceLock<Option<PathBuf>>,
}

/// Where copies of files from other file systems go by default: a private
/// folder of this process in the system temp dir (like Python's `mkdtemp`),
/// created on first use. A fixed, shared name would let another user of a
/// shared `/tmp` plant files or symlinks there. The app removes it on exit
/// with [`remove_default_cache_dir`].
pub fn default_cache_dir() -> PathBuf {
    DEFAULT_CACHE.get_or_init(|| create_private_dir(&std::env::temp_dir())).clone()
}

/// Delete the default cache folder and the copies in it (call on app exit).
/// Harmless if it was never created.
pub fn remove_default_cache_dir() {
    if let Some(dir) = DEFAULT_CACHE.get() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

static DEFAULT_CACHE: OnceLock<PathBuf> = OnceLock::new();

/// A new folder only this user can read, with an unpredictable name. If
/// none can be created, a path is returned anyway and copying fails later
/// with a proper I/O error.
fn create_private_dir(parent: &Path) -> PathBuf {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let mut last = parent.join("orchard-commander-preview");
    for attempt in 0..16u64 {
        let tag = fnv1a(format!("{seed}|{}|{attempt}", std::process::id()).as_bytes());
        last = parent.join(format!("orchard-commander-{:x}-{tag:012x}", std::process::id()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&last) {
            Ok(()) => return last,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => break,
        }
    }
    last
}

/// Distinguishes concurrent copies of the same file (two panes, two previewers).
static PART_COUNTER: AtomicU64 = AtomicU64::new(0);

impl PreviewSource {
    /// Stats `target` (errors if it's gone).
    pub fn new(target: VPath) -> Result<Self, VfsError> {
        let entry = target.stat()?;
        let name = target.name();
        Ok(PreviewSource {
            target,
            entry,
            name,
            cache_dir: None,
            head: Mutex::new(HeadCache::default()),
            direct: OnceLock::new(),
        })
    }

    pub fn with_cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cache_dir = Some(dir.into());
        self
    }

    pub fn target(&self) -> &VPath {
        &self.target
    }

    pub fn entry(&self) -> &Entry {
        &self.entry
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn size(&self) -> u64 {
        self.entry.size
    }

    /// A real path to the file if the file system has one without copying.
    pub fn direct_path(&self) -> Option<&Path> {
        self.direct.get_or_init(|| self.target.local_path()).as_deref()
    }

    /// The containing folder, for display.
    pub fn location(&self) -> String {
        self.target.parent().map(|p| p.display()).unwrap_or_default()
    }

    /// What `score()` sees; with `with_head`, including the first
    /// [`SNIFF_BYTES`] (read once, cached).
    pub fn info(&self, with_head: bool) -> Result<FileInfo, VfsError> {
        let head = if with_head && !self.entry.is_dir { Some(self.head(SNIFF_BYTES)?) } else { None };
        Ok(FileInfo {
            name: self.name.clone(),
            ext: ext_for_name(&self.name),
            size: self.entry.size,
            mtime: self.entry.mtime,
            is_dir: self.entry.is_dir,
            has_direct_path: self.direct_path().is_some(),
            head,
        })
    }

    /// The first `n` bytes (fewer if the file is shorter), as a copy.
    pub fn head(&self, n: usize) -> Result<Vec<u8>, VfsError> {
        self.with_head(n, <[u8]>::to_vec)
    }

    /// Run `f` on the first `n` bytes without copying them.
    pub fn with_head<R>(&self, n: usize, f: impl FnOnce(&[u8]) -> R) -> Result<R, VfsError> {
        let mut cache = self.head.lock().unwrap_or_else(|e| e.into_inner());
        if cache.bytes.len() < n && !cache.complete {
            let mut stream = self.target.open_read()?;
            let have = cache.bytes.len();
            // Continue after what's cached instead of fetching it again.
            let resumed = have > 0 && stream.seekable() && stream.seek(SeekFrom::Start(have as u64)).is_ok();
            if !resumed {
                cache.bytes.clear();
            }
            let want = (n - cache.bytes.len()) as u64;
            cache.bytes.reserve(want.min(self.entry.size.max(4096)) as usize);
            stream.take(want).read_to_end(&mut cache.bytes)?;
            cache.complete = cache.bytes.len() < n;
        }
        let end = n.min(cache.bytes.len());
        Ok(f(&cache.bytes[..end]))
    }

    pub fn open_stream(&self) -> Result<Box<dyn ReadStream>, VfsError> {
        self.target.open_read()
    }

    fn too_large(&self) -> PreviewError {
        PreviewError::ShowMessage(format!(
            "{} is {}. Previewing it means fetching all of it from {}; copy it with F5 instead.",
            self.name,
            human_size(self.entry.size),
            self.target.fs.type_name()
        ))
    }

    /// Errors with the "copy it with F5 instead" message if previewing would
    /// mean fetching more than `max_size` from another file system. For
    /// previewers whose backend fetches the file itself (archive listings).
    pub fn check_fetch_size(&self, max_size: Option<u64>) -> Result<(), PreviewError> {
        if self.direct_path().is_none() && max_size.is_some_and(|m| self.entry.size > m) {
            return Err(self.too_large());
        }
        Ok(())
    }

    /// A local path with the file's contents. Errors with a message above
    /// `max_size` when the file would have to be fetched from elsewhere.
    pub fn local_file(&self, max_size: Option<u64>) -> Result<PathBuf, PreviewError> {
        if let Some(p) = self.direct_path() {
            return Ok(p.to_path_buf());
        }
        if max_size.is_some_and(|m| self.entry.size > m) {
            return Err(self.too_large());
        }
        Ok(self.materialize()?)
    }

    /// All of the file in memory, with the same size rule as `local_file`
    /// (local files are always read). Reuses the head cache when it holds
    /// the whole file.
    pub fn read_all(&self, max_size: Option<u64>) -> Result<Vec<u8>, PreviewError> {
        if let Some(p) = self.direct_path() {
            return Ok(std::fs::read(p)?);
        }
        {
            let cache = self.head.lock().unwrap_or_else(|e| e.into_inner());
            if cache.complete {
                return Ok(cache.bytes.clone());
            }
        }
        if max_size.is_some_and(|m| self.entry.size > m) {
            return Err(self.too_large());
        }
        let mut data = Vec::with_capacity(self.entry.size.min(256 * 1024 * 1024) as usize);
        let stream = self.open_stream()?;
        match max_size {
            // The listed size may be stale (or unknown): never read past the limit.
            Some(max) => {
                stream.take(max.saturating_add(1)).read_to_end(&mut data)?;
                if data.len() as u64 > max {
                    return Err(self.too_large());
                }
            }
            None => {
                let mut stream = stream;
                stream.read_to_end(&mut data)?;
            }
        }
        Ok(data)
    }

    /// Copy the file into the preview cache (once per name/size/mtime).
    fn materialize(&self) -> Result<PathBuf, VfsError> {
        let key = format!("{}|{}|{}", self.target.display(), self.entry.size, self.entry.mtime);
        let cache_dir = self.cache_dir.clone().unwrap_or_else(default_cache_dir);
        let dir = cache_dir.join("vfs").join(format!("{:016x}", fnv1a(key.as_bytes())));
        let mut base = self.target.fs.basename(&self.target.path);
        if base.is_empty() || base == "." || base == ".." || base.contains(['/', '\\', '\0']) {
            base = "file".into();
        }
        let target = dir.join(&base);
        if target.exists() {
            return Ok(target);
        }
        std::fs::create_dir_all(&dir)?;
        // Unique per copy: two threads copying the same file must not write
        // into one partial file (the loser would hand out a torn copy).
        let n = PART_COUNTER.fetch_add(1, Ordering::Relaxed);
        let partial = dir.join(format!("{base}.{}-{n}.part", std::process::id()));
        let result = (|| -> io::Result<()> {
            let mut src = self.target.open_read().map_err(io::Error::other)?;
            // create_new: never follow or reuse something already there.
            let mut dst = std::fs::OpenOptions::new().write(true).create_new(true).open(&partial)?;
            io::copy(&mut src, &mut dst)?;
            dst.sync_all().ok();
            drop(dst);
            std::fs::rename(&partial, &target)
        })();
        if let Err(e) = result {
            let _ = std::fs::remove_file(&partial);
            return Err(VfsError::Io(e));
        }
        Ok(target)
    }
}

impl std::fmt::Debug for PreviewSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PreviewSource({:?})", self.target)
    }
}

/// Stable across runs (unlike `DefaultHasher`), so cached copies are reused.
fn fnv1a(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_cache_dirs_are_new_and_owner_only() {
        let parent = tempfile::tempdir().unwrap();
        let a = create_private_dir(parent.path());
        let b = create_private_dir(parent.path());
        assert_ne!(a, b);
        assert!(a.is_dir() && b.is_dir() && a.starts_with(parent.path()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o700);
        }
    }
}
