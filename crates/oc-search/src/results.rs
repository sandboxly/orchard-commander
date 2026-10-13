//! Search results as a folder ("Feed to listbox").
//!
//! [`SearchResultsFS`] is a read-only virtual file system whose only
//! directory is the root (""); its entries are named by their path below the
//! search root and stand for the real files through `real_location`, so
//! copy/move/trash/preview act on the real files. It is filled while the
//! panel lists it, hence the lock around the hits.

use std::collections::HashMap;
use std::io;
use std::panic::AssertUnwindSafe;
use std::path::{MAIN_SEPARATOR, MAIN_SEPARATOR_STR};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use oc_vfs::{Entry, FileSystem, ListOptions, ReadStream, RelPath, Result, VPath, VfsError, sort_entries};

use crate::walk::{Compiled, run_compiled};
use crate::{Hit, Query, SearchError, SearchProgress, lock, thousands};

/// A virtual folder of search hits; each entry stands for the real file.
pub struct SearchResultsFS {
    query: Query,
    /// Insertion order = the order hits were found.
    hits: Mutex<IndexMap<String, Hit>>,
    progress: Arc<SearchProgress>,
    running: AtomicBool,
    cancelled: AtomicBool,
    started: Instant,
    finished: Mutex<Option<Instant>>,
    changes: AtomicU64,
}

impl SearchResultsFS {
    /// Empty results for `query`, marked as running.
    pub fn new(query: Query) -> Self {
        SearchResultsFS {
            query,
            hits: Mutex::new(IndexMap::new()),
            progress: Arc::new(SearchProgress::new()),
            running: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            started: Instant::now(),
            finished: Mutex::new(None),
            changes: AtomicU64::new(0),
        }
    }

    pub fn query(&self) -> &Query {
        &self.query
    }

    /// The counters the search updates (shown in the status line).
    pub fn progress(&self) -> &Arc<SearchProgress> {
        &self.progress
    }

    /// Acquire pairs with the Release in [`finish`](Self::finish): once this
    /// says false, `was_cancelled` and the final counters are up to date
    /// (Relaxed could show a finished search without its "stopped" on ARM).
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn was_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Bumped on every new hit, prune, relocation and at the end; the UI
    /// refreshes when it changes.
    pub fn changes(&self) -> u64 {
        self.changes.load(Ordering::Relaxed)
    }

    /// Time searched so far, or in total once finished.
    pub fn elapsed(&self) -> Duration {
        lock(&self.finished).unwrap_or_else(Instant::now) - self.started
    }

    // -- filling ----------------------------------------------------------------

    pub fn add(&self, hit: Hit) {
        let mut hits = lock(&self.hits);
        let mut name = hit.relative.clone();
        let mut n = 2;
        while hits.contains_key(&name) {
            // Same relative path from two roots.
            name = format!("{} ({n})", hit.relative);
            n += 1;
        }
        hits.insert(name, hit);
        self.changes.fetch_add(1, Ordering::Relaxed);
    }

    pub fn finish(&self, cancelled: bool) {
        self.cancelled.store(cancelled, Ordering::Relaxed);
        *lock(&self.finished) = Some(Instant::now());
        self.running.store(false, Ordering::Release);
        self.changes.fetch_add(1, Ordering::Relaxed);
    }

    /// Run the query into these results (blocking) and mark them finished.
    pub fn run(&self, is_cancelled: impl Fn() -> bool) -> std::result::Result<(), SearchError> {
        let compiled = match Compiled::new(&self.query) {
            Ok(c) => c,
            Err(e) => {
                self.finish(false);
                return Err(e);
            }
        };
        self.run_compiled(&compiled, &is_cancelled);
        Ok(())
    }

    /// Always finishes the results, even if a file system panics mid-search:
    /// otherwise the panel would say "searching…" forever. The panic is
    /// reported like an unreadable folder and the hits found so far are kept
    /// (the Python version did the same for exceptions).
    fn run_compiled(&self, compiled: &Compiled, is_cancelled: &dyn Fn() -> bool) {
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            run_compiled(&self.query, compiled, &mut |hit| self.add(hit), &self.progress, is_cancelled);
        }));
        if let Err(payload) = outcome {
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unexpected failure".to_string());
            self.progress.add_error(format!("search failed: {message}"));
        }
        self.finish(is_cancelled());
    }

    /// Forget hits whose files are gone (after a move/trash); returns how many.
    pub fn prune(&self) -> usize {
        let snapshot: Vec<(String, VPath)> =
            lock(&self.hits).iter().map(|(name, hit)| (name.clone(), hit.location.clone())).collect();
        // Checked without the lock: on a server this takes a while.
        let gone: Vec<String> =
            snapshot.into_iter().filter(|(_, location)| !location.exists()).map(|(name, _)| name).collect();
        if !gone.is_empty() {
            let mut hits = lock(&self.hits);
            for name in &gone {
                hits.shift_remove(name);
            }
            self.changes.fetch_add(1, Ordering::Relaxed);
        }
        gone.len()
    }

    /// Follow renamed files and folders (also hits inside a renamed folder).
    pub fn relocate(&self, moved: &HashMap<VPath, VPath>) {
        if moved.is_empty() {
            return;
        }
        let mut hits = lock(&self.hits);
        let old = std::mem::take(&mut *hits);
        let mut updated = IndexMap::with_capacity(old.len());
        for (name, hit) in old {
            let mut below: Vec<String> = Vec::new();
            let mut ancestor = Some(hit.location.clone());
            while let Some(a) = ancestor.as_ref().filter(|a| !moved.contains_key(*a)) {
                below.push(a.name());
                ancestor = a.parent();
            }
            let Some(target) = ancestor.and_then(|a| moved.get(&a)) else {
                updated.insert(name, hit);
                continue;
            };
            below.reverse();
            let location = if below.is_empty() {
                target.clone()
            } else {
                let names: Vec<&str> = below.iter().map(String::as_str).collect();
                target.child(&names)
            };
            let target_name = target.name();
            let mut parts: Vec<&str> = hit.relative.split(MAIN_SEPARATOR).collect();
            if let Some(index) = parts.len().checked_sub(below.len() + 1) {
                parts[index] = &target_name;
            }
            let relative = parts.join(MAIN_SEPARATOR_STR);
            let key = if updated.contains_key(&relative) { name } else { relative.clone() };
            let entry = Entry { name: location.name(), ..hit.entry };
            updated.insert(key, Hit { location, entry, relative, snippet: hit.snippet });
        }
        *hits = updated;
        self.changes.fetch_add(1, Ordering::Relaxed);
    }

    /// The hits in the order found.
    pub fn hits(&self) -> Vec<Hit> {
        lock(&self.hits).values().cloned().collect()
    }

    pub fn hit_count(&self) -> usize {
        lock(&self.hits).len()
    }

    fn hit(&self, path: &str) -> Result<Hit> {
        lock(&self.hits).get(path).cloned().ok_or_else(|| VfsError::NotFound(path.to_string()))
    }

    fn location(&self, path: &str) -> Option<VPath> {
        lock(&self.hits).get(path).map(|h| h.location.clone())
    }

    fn status(&self) -> String {
        let p = &self.progress;
        let count = self.hit_count() as u64;
        let hits = format!("{} hit{}", thousands(count), if count == 1 { "" } else { "s" });
        if self.is_running() {
            return format!("searching… {} folders, {} files · {hits}", thousands(p.folders()), thousands(p.files()));
        }
        let mut parts = vec![hits];
        if self.was_cancelled() {
            parts.push("stopped".to_string());
        }
        if p.skipped_online_only() > 0 {
            parts.push(format!("{} cloud-only files not searched", thousands(p.skipped_online_only())));
        }
        let errors = p.error_count() as u64;
        if errors > 0 {
            parts.push(format!("{} unreadable", thousands(errors)));
        }
        parts.join(" · ")
    }
}

/// A search running on its own thread, filling a [`SearchResultsFS`].
pub struct SearchHandle {
    cancel: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl SearchHandle {
    /// Ask the search to stop; the results are then finished as "stopped".
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    /// Wait for the search thread to end.
    pub fn join(self) {
        let _ = self.thread.join();
    }
}

/// Start filling `results` on a plain worker thread. Pattern errors are
/// reported here, before anything runs (the results are then finished).
pub fn start(results: &Arc<SearchResultsFS>) -> std::result::Result<SearchHandle, SearchError> {
    let compiled = match Compiled::new(&results.query) {
        Ok(c) => c,
        Err(e) => {
            results.finish(false);
            return Err(e);
        }
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let runner = results.clone();
    let spawned = std::thread::Builder::new()
        .name("oc-search".into())
        .spawn(move || runner.run_compiled(&compiled, &|| flag.load(Ordering::Relaxed)));
    match spawned {
        Ok(thread) => Ok(SearchHandle { cancel, thread }),
        Err(e) => {
            results.finish(true);
            Err(SearchError::Thread(e))
        }
    }
}

impl FileSystem for SearchResultsFS {
    fn type_name(&self) -> String {
        "search results".to_string()
    }

    fn list_dir(&self, path: &str, opts: ListOptions) -> Result<Vec<Entry>> {
        if !path.is_empty() {
            return Err(VfsError::Io(io::Error::new(io::ErrorKind::NotADirectory, path.to_string())));
        }
        let entries: Vec<Entry> = lock(&self.hits)
            .iter()
            .map(|(name, hit)| Entry { name: name.clone(), ..hit.entry.clone() })
            .collect();
        // Hidden-ness was decided by the query.
        Ok(sort_entries(entries, ListOptions { show_hidden: true, ..opts }))
    }

    fn stat(&self, path: &str) -> Result<Entry> {
        if path.is_empty() {
            return Ok(Entry::dir(self.display(""), 0.0));
        }
        self.hit(path)?.location.stat()
    }

    fn exists(&self, path: &str) -> bool {
        path.is_empty() || self.location(path).is_some_and(|l| l.exists())
    }

    fn is_dir(&self, path: &str) -> bool {
        path.is_empty() || lock(&self.hits).get(path).is_some_and(|h| h.entry.is_dir)
    }

    fn parent_path(&self, path: &str) -> Option<String> {
        if path.is_empty() { None } else { Some(String::new()) }
    }

    fn join(&self, path: &str, names: &[&str]) -> String {
        names.last().map_or_else(|| path.to_string(), |n| n.to_string())
    }

    fn basename(&self, path: &str) -> String {
        if path.is_empty() { "Search results".to_string() } else { path.to_string() }
    }

    fn display(&self, path: &str) -> String {
        if path.is_empty() { format!("Search: {}", self.query.describe()) } else { path.to_string() }
    }

    fn open_read(&self, path: &str) -> Result<Box<dyn ReadStream>> {
        let real = self.hit(path)?.location;
        real.fs.open_read(&real.path)
    }

    /// Collected up front: the iterator would borrow the real file system,
    /// which only lives as long as the hit's `Arc` here. Walking a hit is rare
    /// (operations use the resolved path).
    fn walk<'a>(&'a self, path: &str) -> Result<Box<dyn Iterator<Item = Result<(RelPath, Entry)>> + 'a>> {
        let real = self.hit(path)?.location;
        let items: Vec<_> = real.fs.walk(&real.path)?.collect();
        Ok(Box::new(items.into_iter()))
    }

    fn local_path(&self, path: &str) -> Option<std::path::PathBuf> {
        if path.is_empty() {
            return None;
        }
        self.location(path)?.local_path()
    }

    fn real_location(&self, path: &str) -> Option<VPath> {
        if path.is_empty() { None } else { self.location(path) }
    }

    fn exit_to(&self) -> Option<VPath> {
        self.query.roots.first().cloned()
    }

    fn status_text(&self) -> Option<String> {
        Some(self.status())
    }

    fn fetch_online_only(&self, path: &str, timeout: Duration, is_cancelled: &dyn Fn() -> bool) -> Result<()> {
        let real = self.hit(path)?.location;
        real.fs.fetch_online_only(&real.path, timeout, is_cancelled)
    }
}
