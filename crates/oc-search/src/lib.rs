//! Find files — after Total Commander's Find Files (Alt+F7) and "Feed to listbox".
//!
//! [`run`] walks one or more folders on any file system (local, servers,
//! inside archives) and reports hits as it finds them. [`SearchResultsFS`]
//! shows the hits as a virtual folder: each entry stands for the real file
//! ([`FileSystem::real_location`](oc_vfs::FileSystem::real_location)), so
//! copy/move/trash/preview/open act on the real files.
//!
//! Name patterns follow Total Commander: masks separated by spaces or ";", one
//! "|" separates included from excluded masks ("*.zip|*big*.zip"), and a mask
//! without wildcards matches anywhere in the name ("report" = "*report*").
//!
//! Local trees (the root has a `local_path`) are walked in parallel; other
//! file systems are walked on one thread through `walk`/`list_dir`. Archives
//! are opened through an [`ArchiveOpener`] the app supplies, so this crate
//! doesn't depend on the archive back ends. Port of `search.py`.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use oc_vfs::{Entry, VPath};

mod local;
mod names;
mod results;
mod text;
mod walk;

pub use names::NameMatcher;
pub use results::{SearchHandle, SearchResultsFS, start};
pub use text::{CHUNK, OVERLAP, SNIPPET_CHARS, TextMatcher};
pub use walk::run;

/// Opens archives found during a search ("Inside archives"). The app wires
/// the archive back ends in; tests use fakes.
pub trait ArchiveOpener: Send + Sync {
    /// Whether a file of this name is an archive worth opening.
    fn is_archive(&self, name: &str) -> bool;

    /// The root of the archive stored in `file` (a VPath whose file system
    /// keeps `file` as its container, so ".." and names work).
    fn open(&self, file: &VPath) -> oc_vfs::Result<VPath>;
}

/// Which kinds of entries count as hits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Kinds {
    Files,
    Dirs,
    #[default]
    Both,
}

impl Kinds {
    pub fn as_str(self) -> &'static str {
        match self {
            Kinds::Files => "files",
            Kinds::Dirs => "dirs",
            Kinds::Both => "both",
        }
    }

    pub fn parse(s: &str) -> Option<Kinds> {
        Some(match s {
            "files" => Kinds::Files,
            "dirs" => Kinds::Dirs,
            "both" => Kinds::Both,
            _ => return None,
        })
    }
}

/// Everything the Find Files dialog asks for.
#[derive(Clone)]
pub struct Query {
    pub roots: Vec<VPath>,
    pub names: String,
    pub name_regex: bool,
    pub name_case: bool,
    pub recursive: bool,
    /// None: unlimited; 0: only the root folders themselves.
    pub max_depth: Option<u32>,
    pub include_hidden: bool,
    pub kinds: Kinds,
    /// Empty: search by name only.
    pub text: String,
    pub text_regex: bool,
    pub text_case: bool,
    pub whole_words: bool,
    /// Files that do NOT contain the text.
    pub not_containing: bool,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    /// mtime >= this (seconds since the epoch).
    pub newer_than: Option<f64>,
    /// mtime <= this.
    pub older_than: Option<f64>,
    pub search_archives: bool,
    /// Content search downloads cloud placeholders only if true.
    pub include_online_only: bool,
    /// For `search_archives`; None: the process-wide opener registered with
    /// [`oc_vfs::set_archive_opener`] ([`VfsArchiveOpener`]). Without
    /// either, archives stay closed.
    pub archive_opener: Option<Arc<dyn ArchiveOpener>>,
}

/// [`ArchiveOpener`] over the hooks the app registered with `oc_vfs`
/// ([`oc_vfs::set_archive_opener`], [`oc_vfs::set_archive_name_test`]).
pub struct VfsArchiveOpener;

impl ArchiveOpener for VfsArchiveOpener {
    fn is_archive(&self, name: &str) -> bool {
        oc_vfs::is_archive_name(name)
    }

    fn open(&self, file: &VPath) -> oc_vfs::Result<VPath> {
        oc_vfs::open_archive(file).unwrap_or_else(|| {
            Err(oc_vfs::VfsError::Operation(format!("{} is not a supported archive", file.name())))
        })
    }
}

impl Query {
    /// The opener a search uses: the query's own, else the registered one.
    pub(crate) fn effective_opener(&self) -> Option<&dyn ArchiveOpener> {
        static HOOK: VfsArchiveOpener = VfsArchiveOpener;
        match self.archive_opener.as_deref() {
            Some(own) => Some(own),
            None => oc_vfs::archive_opener().is_some().then_some(&HOOK as &dyn ArchiveOpener),
        }
    }
}

impl Default for Query {
    fn default() -> Self {
        Query {
            roots: Vec::new(),
            names: "*".to_string(),
            name_regex: false,
            name_case: false,
            recursive: true,
            max_depth: None,
            include_hidden: true,
            kinds: Kinds::Both,
            text: String::new(),
            text_regex: false,
            text_case: false,
            whole_words: false,
            not_containing: false,
            min_size: None,
            max_size: None,
            newer_than: None,
            older_than: None,
            search_archives: false,
            include_online_only: false,
            archive_opener: None,
        }
    }
}

impl Query {
    /// Default options (all names, all subfolders, hidden files included) in `roots`.
    pub fn new(roots: Vec<VPath>) -> Self {
        Query { roots, ..Query::default() }
    }

    /// "*.pdf containing “total” in Documents" — the results folder's title.
    pub fn describe(&self) -> String {
        let mut what = match self.names.trim() {
            "" => "*".to_string(),
            names => names.to_string(),
        };
        if !self.text.is_empty() {
            let how = if self.not_containing { "without" } else { "containing" };
            what.push_str(&format!(" {how} “{}”", self.text));
        }
        let mut place = String::new();
        for (i, root) in self.roots.iter().enumerate() {
            if i > 0 {
                place.push_str(", ");
            }
            place.push_str(&root_label(root));
        }
        format!("{what} in {place}")
    }
}

/// A root's name, or its display string for nameless roots ("/", an archive root).
pub(crate) fn root_label(root: &VPath) -> String {
    match root.name() {
        n if n.is_empty() => root.display(),
        n => n,
    }
}

impl fmt::Debug for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Query")
            .field("roots", &self.roots)
            .field("names", &self.names)
            .field("text", &self.text)
            .field("kinds", &self.kinds)
            .field("max_depth", &self.max_depth)
            .field("search_archives", &self.search_archives)
            .finish_non_exhaustive()
    }
}

/// One search hit.
#[derive(Clone, Debug)]
pub struct Hit {
    /// The real file or folder.
    pub location: VPath,
    pub entry: Entry,
    /// Path below the search root (OS separator), for display.
    pub relative: String,
    /// The first matching line, for content searches.
    pub snippet: String,
}

/// Why a search couldn't start; reported before anything is walked.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("invalid name pattern: {0}")]
    NamePattern(regex::Error),
    #[error("invalid text pattern: {0}")]
    TextPattern(regex::Error),
    #[error("could not start the search: {0}")]
    Thread(std::io::Error),
}

/// Live counters of a running search. Shared between the walker threads and
/// the UI (through [`SearchResultsFS::progress`]), hence atomics.
#[derive(Debug, Default)]
pub struct SearchProgress {
    folders: AtomicU64,
    files: AtomicU64,
    hits: AtomicU64,
    skipped_online_only: AtomicU64,
    current: Mutex<String>,
    errors: Mutex<Vec<String>>,
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl SearchProgress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Folders listed so far (archives count as folders).
    pub fn folders(&self) -> u64 {
        self.folders.load(Ordering::Relaxed)
    }

    /// Files seen so far (hits or not).
    pub fn files(&self) -> u64 {
        self.files.load(Ordering::Relaxed)
    }

    /// Hits reported so far.
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Cloud-only files not content-searched (no download allowed).
    pub fn skipped_online_only(&self) -> u64 {
        self.skipped_online_only.load(Ordering::Relaxed)
    }

    /// The folder being listed, as displayed.
    pub fn current(&self) -> String {
        lock(&self.current).clone()
    }

    /// "<where>: <what went wrong>" for unreadable folders and files.
    pub fn errors(&self) -> Vec<String> {
        lock(&self.errors).clone()
    }

    pub fn error_count(&self) -> usize {
        lock(&self.errors).len()
    }

    pub(crate) fn add_folder(&self) {
        self.folders.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn add_files(&self, n: u64) {
        if n > 0 {
            self.files.fetch_add(n, Ordering::Relaxed);
        }
    }

    pub(crate) fn add_hit(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn add_skipped_online_only(&self) {
        self.skipped_online_only.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn add_error(&self, message: String) {
        lock(&self.errors).push(message);
    }

    /// Best effort: parallel walkers skip the update (and building the
    /// string) when another thread holds the lock.
    pub(crate) fn set_current_with(&self, folder: impl FnOnce() -> String) {
        if let Ok(mut current) = self.current.try_lock() {
            *current = folder();
        }
    }
}

/// "1,204" — counts in status lines.
pub(crate) fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let lead = digits.len() % 3; // digits before the first comma (0 = three)
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && i % 3 == lead {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1204), "1,204");
        assert_eq!(thousands(18_331_000), "18,331,000");
    }

    #[test]
    fn kinds_round_trip() {
        for k in [Kinds::Files, Kinds::Dirs, Kinds::Both] {
            assert_eq!(Kinds::parse(k.as_str()), Some(k));
        }
        assert_eq!(Kinds::parse("x"), None);
    }
}
