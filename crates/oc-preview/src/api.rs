//! The previewer plugin interface.
//!
//! To support a new file format, implement [`Previewer`]:
//!
//! ```ignore
//! struct StlPreviewer;
//!
//! impl Previewer for StlPreviewer {
//!     fn id(&self) -> &str { "stl" }
//!     fn title(&self) -> &str { "3D model" }
//!     fn access(&self) -> Access { Access::LocalFile }
//!
//!     fn score(&self, f: &FileInfo) -> i32 {
//!         if f.ext == "stl" { Score::FORMAT } else { Score::NO }
//!     }
//!
//!     fn load(&self, src: &PreviewSource, ctx: &LoadContext) -> Result<Loaded, PreviewError> {
//!         let path = src.local_file(None)?;            // worker thread
//!         Ok(Loaded::new(PreviewData::Message(render(path)?), "mesh"))
//!     }
//! }
//! ```
//!
//! and register it with [`Registry::register`](crate::Registry::register).
//!
//! Threading: `score()` and `load()` run on worker threads; the result is plain
//! data ([`PreviewData`]) that the UI renders on its own thread. Previewers never
//! see widgets, so this crate stays toolkit-free.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use oc_vfs::{VPath, VfsError, human_size};

use crate::data::PreviewData;
use crate::source::PreviewSource;

/// What a previewer reads. [`PreviewSource`] gets it through the generic
/// [`oc_vfs::FileSystem`] interface, so previewers work on every file system,
/// including ones added later.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    /// name/size/date, listings — no file contents
    Metadata,
    /// the first bytes (text, hex dumps)
    Head,
    /// a stream, ideally seekable (video, audio)
    Stream,
    /// a real file on disk; fetched/extracted if needed (images, documents, 3D)
    LocalFile,
}

/// Conventional scores; any `i32` works, highest wins, `<= 0` means "can't show it".
pub struct Score;

impl Score {
    pub const NO: i32 = 0;
    /// Can show anything, e.g. a hex dump.
    pub const FALLBACK: i32 = 1;
    /// Recognised from the contents (e.g. "looks like text").
    pub const CONTENT: i32 = 50;
    /// The file's format, by extension or signature.
    pub const FORMAT: i32 = 100;
    /// A more specific variant of a format (e.g. Markdown among text).
    pub const SPECIFIC: i32 = 150;
}

/// What `score()` may look at. Computing a score needs no I/O.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileInfo {
    pub name: String,
    /// Lower case, without the dot; "" if none (see [`ext_for_name`]).
    pub ext: String,
    pub size: u64,
    pub mtime: f64,
    pub is_dir: bool,
    /// The file system can hand out a real path without copying.
    pub has_direct_path: bool,
    /// The first bytes, only if some previewer set `needs_head`.
    pub head: Option<Vec<u8>>,
}

impl FileInfo {
    /// A file (not a directory) with `ext` derived from `name`; handy for tests.
    pub fn file(name: impl Into<String>, size: u64) -> Self {
        let name = name.into();
        FileInfo { ext: ext_for_name(&name), name, size, ..FileInfo::default() }
    }

    pub fn with_head(mut self, head: impl Into<Vec<u8>>) -> Self {
        self.head = Some(head.into());
        self
    }

    pub fn is_empty(&self) -> bool {
        !self.is_dir && self.size == 0
    }
}

/// The extension previewers see: lower case, no dot, "" for names without
/// one. Leading dots don't count (".bashrc", "..x" have none), unlike
/// [`oc_vfs::ext_of`], to match what the Python version scored on.
pub fn ext_for_name(name: &str) -> String {
    let trimmed = name.trim_start_matches('.');
    match trimmed.rfind('.') {
        Some(i) => trimmed[i + 1..].to_lowercase(),
        None => String::new(),
    }
}

/// Why `load()` (or the whole preview) produced no preview.
#[derive(Debug, thiserror::Error)]
pub enum PreviewError {
    /// The file can't be shown after all (e.g. a corrupt image); the registry
    /// tries the next best previewer.
    #[error("{0}")]
    CannotPreview(String),
    /// Show this text instead of a preview, without trying other previewers
    /// (e.g. "too large to fetch", "needs unrar").
    #[error("{0}")]
    ShowMessage(String),
    /// An online-only cloud placeholder bigger than the automatic download
    /// limit; previewing it means downloading it first (the UI offers a button).
    #[error(
        "This file is stored online only ({}).\n\nPreviewing it means downloading it from the cloud first.",
        human_size(*size)
    )]
    NeedsDownload { size: u64 },
    /// An online-only file didn't arrive in time; its download goes on in the
    /// background (the UI offers "Try again").
    #[error(
        "{name} hasn't arrived from the cloud yet (waited {:.0} s).\n\nCheck that your cloud storage app is running, signed in and online. The download continues in the background.",
        waited.as_secs_f64()
    )]
    CloudNotDelivered { name: String, waited: Duration },
    /// A previewer failed in an unexpected way (I/O error, panic in an add-on).
    #[error("{0}")]
    Failed(String),
    #[error(transparent)]
    Vfs(#[from] VfsError),
}

impl PreviewError {
    pub fn cannot(msg: impl Into<String>) -> Self {
        PreviewError::CannotPreview(msg.into())
    }

    pub fn message(msg: impl Into<String>) -> Self {
        PreviewError::ShowMessage(msg.into())
    }

    /// A message meant for the user in place of the preview (the Python
    /// `ShowMessage` family), as opposed to a failure.
    pub fn is_message(&self) -> bool {
        matches!(
            self,
            PreviewError::ShowMessage(_) | PreviewError::NeedsDownload { .. } | PreviewError::CloudNotDelivered { .. }
        )
    }
}

impl From<std::io::Error> for PreviewError {
    fn from(e: std::io::Error) -> Self {
        PreviewError::Vfs(VfsError::Io(e))
    }
}

/// How long a preview waits for an online-only file: 20 s plus 1 s per MB.
pub fn fetch_timeout(size: u64) -> Duration {
    Duration::from_secs_f64(20.0 + size as f64 / (1024.0 * 1024.0))
}

/// Online-only cloud files up to this size are fetched for previews automatically.
pub const AUTO_DOWNLOAD_LIMIT: u64 = 25 * 1024 * 1024;

/// Opens an archive file as a browsable file system and returns its root.
/// Archive support lives in another crate; the app plugs it in here so this
/// crate needn't depend on it.
pub type ArchiveOpener = Arc<dyn Fn(&VPath) -> oc_vfs::Result<VPath> + Send + Sync>;

/// Everything `load()` may consult besides the file itself.
#[derive(Clone)]
pub struct LoadContext {
    pub show_hidden: bool,
    /// Polled by long loads; true once the pane has moved on.
    pub is_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
    /// For online-only files; None: always download.
    pub download_limit: Option<u64>,
    /// Pygments style name of the color scheme in use ("monokai", "default", …),
    /// for previewers that bake colors into their data. The built-ins don't:
    /// they emit theme roles (see [`crate::Color`]) that the UI resolves.
    pub syntax_style: String,
    /// Lists archives for the listing previewer; None: the process-wide
    /// opener registered with [`oc_vfs::set_archive_opener`] (if none is
    /// registered either, archives get a hex dump).
    pub open_archive: Option<ArchiveOpener>,
    /// Where copies of files from other file systems go (see
    /// [`PreviewSource::local_file`]); None: a folder in the system temp dir.
    pub cache_dir: Option<PathBuf>,
    /// How long to wait for an online-only file of a given size.
    pub fetch_timeout: fn(u64) -> Duration,
}

impl Default for LoadContext {
    fn default() -> Self {
        LoadContext {
            show_hidden: false,
            is_cancelled: Arc::new(|| false),
            download_limit: Some(AUTO_DOWNLOAD_LIMIT),
            syntax_style: "monokai".to_string(),
            open_archive: None,
            cache_dir: None,
            fetch_timeout,
        }
    }
}

impl LoadContext {
    pub fn cancelled(&self) -> bool {
        (self.is_cancelled)()
    }
}

impl std::fmt::Debug for LoadContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadContext")
            .field("show_hidden", &self.show_hidden)
            .field("download_limit", &self.download_limit)
            .field("syntax_style", &self.syntax_style)
            .field("open_archive", &self.open_archive.is_some())
            .field("cache_dir", &self.cache_dir)
            .finish_non_exhaustive()
    }
}

/// The result of `load()`: data for the view, plus the status line text.
/// Resources (open streams) are released when it's dropped.
#[derive(Debug)]
pub struct Loaded {
    pub data: PreviewData,
    pub info: String,
}

impl Loaded {
    pub fn new(data: PreviewData, info: impl Into<String>) -> Self {
        Loaded { data, info: info.into() }
    }
}

/// A file previewer. One shared instance per kind; must be thread-safe.
pub trait Previewer: Send + Sync {
    /// Unique, stable id (also the key for remembered "View as" choices).
    fn id(&self) -> &str;

    /// Shown in the "View as" switcher.
    fn title(&self) -> &str;

    fn access(&self) -> Access {
        Access::Head
    }

    /// For [`Access::LocalFile`] from other file systems: the download limit.
    fn max_size(&self) -> Option<u64> {
        None
    }

    /// Whether `score()` wants [`FileInfo::head`]. The registry reads the head
    /// only when such a previewer could still beat the name-based scores.
    fn needs_head(&self) -> bool {
        false
    }

    /// Load again when the color scheme changes (`load()` baked colors in).
    fn reload_on_theme_change(&self) -> bool {
        true
    }

    /// False if an optional dependency is missing; the previewer is then skipped.
    fn available(&self) -> bool {
        true
    }

    /// How well this previewer fits `f`: [`Score::NO`] for not at all. Cheap, no I/O.
    fn score(&self, f: &FileInfo) -> i32;

    /// Read what's needed and prepare data for the view. Worker thread.
    fn load(&self, src: &PreviewSource, ctx: &LoadContext) -> Result<Loaded, PreviewError>;
}

/// A previewer that fits a file, with its score.
#[derive(Clone)]
pub struct Candidate {
    pub previewer: Arc<dyn Previewer>,
    pub score: i32,
}

impl std::fmt::Debug for Candidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Candidate({}, {})", self.previewer.id(), self.score)
    }
}

/// What the pane receives from a background preview job. All empty means the
/// job was cancelled (the pane has moved on): show nothing.
#[derive(Default)]
pub struct Outcome {
    pub candidates: Vec<Candidate>,
    pub previewer: Option<Arc<dyn Previewer>>,
    pub loaded: Option<Loaded>,
    pub error: Option<PreviewError>,
}

impl Outcome {
    pub fn previewer_id(&self) -> Option<&str> {
        self.previewer.as_deref().map(|p| p.id())
    }

    pub fn candidate_ids(&self) -> Vec<&str> {
        self.candidates.iter().map(|c| c.previewer.id()).collect()
    }
}

impl std::fmt::Debug for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Outcome")
            .field("candidates", &self.candidates)
            .field("previewer", &self.previewer_id())
            .field("loaded", &self.loaded)
            .field("error", &self.error)
            .finish()
    }
}
