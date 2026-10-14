//! The search itself: walking, filtering, content checks, cancellation.
//!
//! [`run`] does the walking on a helper thread (plus a rayon pool for local
//! trees) and hands hits to `on_hit` on the calling thread, so the callback
//! needn't be thread-safe and cancellation stays exact: once `is_cancelled`
//! returns true no further hit is delivered.
//!
//! Three walkers, chosen per tree:
//! - local trees (the root has a `local_path`): parallel `read_dir`, `stat`
//!   only where a decision needs it, content checks as separate tasks;
//! - other file systems, unlimited depth: one `FileSystem::walk` (servers and
//!   archives can enumerate a whole tree far cheaper than folder by folder);
//! - other file systems with a depth limit: `list_dir` folder by folder,
//!   depth-first in listing order like the Python version.
//!
//! Symbolic links to folders are reported but never followed (no loops).

use std::cell::RefCell;
use std::collections::HashSet;
use std::fs;
use std::path::{MAIN_SEPARATOR, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use oc_vfs::{Entry, ListOptions, VPath};

use crate::local::{HIDDEN_ATTRIBUTES, entry_from_dirent};
use crate::text::Outcome;
use crate::{ArchiveOpener, Hit, Kinds, NameMatcher, Query, SearchError, SearchProgress, TextMatcher, root_label};

/// How often the calling thread checks `is_cancelled` while no hits arrive.
const POLL: Duration = Duration::from_millis(25);

/// The query's patterns, compiled once (and validated before any walking).
pub(crate) struct Compiled {
    names: NameMatcher,
    text: Option<TextMatcher>,
}

impl Compiled {
    pub(crate) fn new(q: &Query) -> Result<Self, SearchError> {
        let names = NameMatcher::new(&q.names, q.name_regex, q.name_case).map_err(SearchError::NamePattern)?;
        let text = if q.text.is_empty() {
            None
        } else {
            Some(
                TextMatcher::new(&q.text, q.text_regex, q.text_case, q.whole_words)
                    .map_err(SearchError::TextPattern)?,
            )
        };
        Ok(Compiled { names, text })
    }
}

/// Search (blocking; call it on a worker thread). Hits are passed to `on_hit`
/// as they are found, on the calling thread; counters and errors go to
/// `progress`. Only invalid patterns are errors: unreadable folders and files
/// are recorded in `progress` and skipped.
pub fn run(
    query: &Query,
    mut on_hit: impl FnMut(Hit),
    progress: &SearchProgress,
    is_cancelled: impl Fn() -> bool,
) -> Result<(), SearchError> {
    let compiled = Compiled::new(query)?;
    run_compiled(query, &compiled, &mut on_hit, progress, &is_cancelled);
    Ok(())
}

pub(crate) fn run_compiled(
    query: &Query,
    compiled: &Compiled,
    on_hit: &mut dyn FnMut(Hit),
    progress: &SearchProgress,
    is_cancelled: &dyn Fn() -> bool,
) {
    let stop = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel::<Hit>();
    let walker = Walker {
        q: query,
        names: &compiled.names,
        text: compiled.text.as_ref(),
        progress,
        stop: &stop,
        tx,
        opener: if query.search_archives { query.effective_opener() } else { None },
        pool: OnceLock::new(),
    };
    std::thread::scope(|s| {
        s.spawn(move || walker.run());
        // If `on_hit` or `is_cancelled` panics, stop the walker instead of
        // letting the scope wait for the whole tree to be searched.
        let _stop_on_exit = StopOnDrop(&stop);
        loop {
            let next = rx.recv_timeout(POLL);
            if is_cancelled() {
                stop.store(true, Ordering::Relaxed);
                break;
            }
            match next {
                Ok(hit) => {
                    progress.add_hit();
                    on_hit(hit);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });
}

/// Sets the walker's stop flag when the receiving loop ends (also by unwinding).
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// A local folder waiting to be listed.
struct LocalDir {
    path: PathBuf,
    /// Names below the tree's root, for building locations in its file system.
    parts: Vec<String>,
    /// Relative path for display (with the root label).
    rel: String,
    depth: u32,
}

struct Walker<'a> {
    q: &'a Query,
    names: &'a NameMatcher,
    text: Option<&'a TextMatcher>,
    progress: &'a SearchProgress,
    stop: &'a AtomicBool,
    tx: Sender<Hit>,
    opener: Option<&'a dyn ArchiveOpener>,
    /// Built on the first local tree; None if the pool couldn't be created.
    pool: OnceLock<Option<rayon::ThreadPool>>,
}

thread_local! {
    /// One content buffer per thread, reused across files.
    static BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

fn join_rel(folder: &str, name: &str) -> String {
    if folder.is_empty() {
        name.to_string()
    } else {
        let mut s = String::with_capacity(folder.len() + 1 + name.len());
        s.push_str(folder);
        s.push(MAIN_SEPARATOR);
        s.push_str(name);
        s
    }
}

fn entry_ok(q: &Query, e: &Entry) -> bool {
    let kind_ok = match q.kinds {
        Kinds::Files => !e.is_dir,
        Kinds::Dirs => e.is_dir,
        Kinds::Both => true,
    };
    kind_ok
        && (e.is_dir || (q.min_size.is_none_or(|m| e.size >= m) && q.max_size.is_none_or(|m| e.size <= m)))
        && q.newer_than.is_none_or(|t| e.mtime >= t)
        && q.older_than.is_none_or(|t| e.mtime <= t)
}

impl<'a> Walker<'a> {
    fn run(&self) {
        let label_roots = self.q.roots.len() > 1;
        for root in &self.q.roots {
            if self.stopped() {
                return;
            }
            // A root taken from search results stands for the real folder.
            let root = root.resolved();
            let label = if label_roots { root_label(&root) } else { String::new() };
            self.search_tree(root, label, 0);
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn pool(&self) -> Option<&rayon::ThreadPool> {
        self.pool
            .get_or_init(|| {
                let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 12);
                rayon::ThreadPoolBuilder::new()
                    .num_threads(threads)
                    .thread_name(|i| format!("oc-search-{i}"))
                    .build()
                    .ok()
            })
            .as_ref()
    }

    /// Whether folders at `depth` are listed further.
    fn deeper(&self, depth: u32) -> bool {
        self.q.recursive && self.q.max_depth.is_none_or(|m| depth < m)
    }

    fn search_tree(&self, root: VPath, label: String, depth: u32) {
        if let Some((dir, pool)) = root.local_path().and_then(|dir| Some((dir, self.pool()?))) {
            let top = LocalDir { path: dir, parts: Vec::new(), rel: label, depth };
            pool.scope(|s| self.visit_local(s, &root, top));
            return;
        }
        if self.q.recursive && self.q.max_depth.is_none() {
            self.search_walk(root, label, depth);
        } else {
            self.search_listing(root, label, depth);
        }
    }

    // -- hits ---------------------------------------------------------------

    fn send(&self, location: VPath, entry: Entry, relative: String, snippet: String) {
        // The receiver is gone only after cancellation: nothing to do then.
        let _ = self.tx.send(Hit { location, entry, relative, snippet });
    }

    /// Content criteria: None if the entry needs no content check (report it
    /// now), Some(false) if it's out, Some(true) if its contents decide.
    fn needs_content(&self, entry: &Entry) -> Option<bool> {
        self.text?;
        if entry.is_dir {
            return Some(false); // content criteria apply to files only
        }
        if entry.is_online_only && !self.q.include_online_only {
            self.progress.add_skipped_online_only();
            return Some(false);
        }
        Some(true)
    }

    /// Search `stream`'s contents and report the hit if it qualifies.
    fn content_check(
        &self,
        open: impl FnOnce() -> Result<Box<dyn std::io::Read>, String>,
        location: VPath,
        entry: Entry,
        relative: String,
    ) {
        let Some(text) = self.text else { return };
        let outcome = open().and_then(|mut stream| {
            let is_cancelled = || self.stopped();
            BUFFER
                .with(|cell| match cell.try_borrow_mut() {
                    Ok(mut buf) => text.search_with(&mut *stream, &mut buf, &is_cancelled),
                    // Re-entered on this thread (a stream that waits on another
                    // rayon pool lets this worker run other tasks meanwhile).
                    Err(_) => text.search_with(&mut *stream, &mut Vec::new(), &is_cancelled),
                })
                .map_err(|e| e.to_string())
        });
        let snippet = match outcome {
            Err(e) => {
                self.progress.add_error(format!("{relative}: {e}"));
                return;
            }
            Ok(Outcome::Cancelled) => return,
            Ok(Outcome::Found(snippet)) if !self.q.not_containing => snippet,
            Ok(Outcome::NotFound) if self.q.not_containing => String::new(),
            Ok(_) => return,
        };
        self.send(location, entry, relative, snippet);
    }

    /// Check a hit on any file system (content via `open_read`).
    fn consider(&self, location: VPath, entry: Entry, relative: String) {
        if !entry_ok(self.q, &entry) {
            return;
        }
        match self.needs_content(&entry) {
            None => self.send(location, entry, relative, String::new()),
            Some(false) => {}
            Some(true) => {
                let fs = location.fs.clone();
                let path = location.path.clone();
                let open = move || {
                    fs.open_read(&path).map(|s| Box::new(s) as Box<dyn std::io::Read>).map_err(|e| e.to_string())
                };
                self.content_check(open, location, entry, relative);
            }
        }
    }

    fn open_archive(&self, file: VPath, relative: String, depth: u32) {
        let Some(opener) = self.opener else { return };
        match opener.open(&file) {
            Ok(root) => self.search_tree(root, relative, depth),
            Err(e) => self.progress.add_error(format!("{relative}: {e}")),
        }
    }

    fn wants_archive(&self, entry: &Entry) -> bool {
        !entry.is_dir
            && !(entry.is_online_only && !self.q.include_online_only)
            && self.opener.is_some_and(|o| o.is_archive(&entry.name))
    }

    // -- local trees: parallel ------------------------------------------------

    fn local_location(root: &VPath, parts: &[String], name: Option<&str>) -> VPath {
        let mut names: Vec<&str> = Vec::with_capacity(parts.len() + 1);
        names.extend(parts.iter().map(String::as_str));
        names.extend(name);
        if names.is_empty() {
            return root.clone();
        }
        VPath::new(root.fs.clone(), root.fs.join(&root.path, &names))
    }

    fn visit_local<'s>(&'s self, s: &rayon::Scope<'s>, root: &'s VPath, dir: LocalDir) {
        if self.stopped() {
            return;
        }
        self.progress.add_folder();
        let display = || Self::local_location(root, &dir.parts, None).display();
        self.progress.set_current_with(display);
        let read_dir = match fs::read_dir(&dir.path) {
            Ok(r) => r,
            Err(e) => {
                self.progress.add_error(format!("{}: {e}", display()));
                return;
            }
        };
        let q = self.q;
        let deeper = self.deeper(dir.depth);
        let mut files = 0u64;
        for de in read_dir {
            if self.stopped() {
                break;
            }
            let de = match de {
                Ok(de) => de,
                Err(e) => {
                    self.progress.add_error(format!("{}: {e}", display()));
                    continue;
                }
            };
            let os_name = de.file_name();
            let name = os_name.to_string_lossy();
            if !q.include_hidden && name.starts_with('.') {
                continue;
            }
            let file_type = de.file_type().ok();
            let is_link = file_type.is_some_and(|t| t.is_symlink());
            let type_dir = file_type.is_some_and(|t| t.is_dir());
            // A known plain file can't be a folder hit and vice versa: no stat
            // needed for it (links and unknown types are decided by the entry).
            let kind_possible = match q.kinds {
                Kinds::Both => true,
                Kinds::Files => !type_dir,
                Kinds::Dirs => type_dir || is_link || file_type.is_none(),
            };
            let name_hit = kind_possible && self.names.matches(&name);
            let archive_candidate = deeper && !type_dir && self.opener.is_some_and(|o| o.is_archive(&name));
            let need_entry = file_type.is_none()
                || is_link
                || name_hit
                || archive_candidate
                || (!q.include_hidden && HIDDEN_ATTRIBUTES);
            if !need_entry {
                // The common case of a name search: no stat at all.
                if !type_dir {
                    files += 1;
                } else if deeper {
                    self.spawn_local_dir(s, root, &dir, de.path(), name.into_owned());
                }
                continue;
            }
            let entry = entry_from_dirent(&de, name.into_owned(), is_link);
            if entry.is_hidden && !q.include_hidden {
                continue;
            }
            if !entry.is_dir {
                files += 1;
            }
            if deeper && entry.is_dir && !entry.is_link {
                self.spawn_local_dir(s, root, &dir, de.path(), entry.name.clone());
            } else if archive_candidate && self.wants_archive(&entry) {
                let location = Self::local_location(root, &dir.parts, Some(&entry.name));
                let relative = join_rel(&dir.rel, &entry.name);
                let depth = dir.depth + 1;
                s.spawn(move |_| self.open_archive(location, relative, depth));
            }
            if name_hit && entry_ok(q, &entry) {
                self.consider_local(s, root, &dir, de.path(), entry);
            }
        }
        self.progress.add_files(files);
    }

    fn spawn_local_dir<'s>(&'s self, s: &rayon::Scope<'s>, root: &'s VPath, dir: &LocalDir, path: PathBuf, name: String) {
        let rel = join_rel(&dir.rel, &name);
        let mut parts = Vec::with_capacity(dir.parts.len() + 1);
        parts.extend_from_slice(&dir.parts);
        parts.push(name);
        let child = LocalDir { path, parts, rel, depth: dir.depth + 1 };
        s.spawn(move |s| self.visit_local(s, root, child));
    }

    /// A local entry that passed the name and attribute filters.
    fn consider_local<'s>(&'s self, s: &rayon::Scope<'s>, root: &VPath, dir: &LocalDir, path: PathBuf, entry: Entry) {
        let location = Self::local_location(root, &dir.parts, Some(&entry.name));
        let relative = join_rel(&dir.rel, &entry.name);
        match self.needs_content(&entry) {
            None => self.send(location, entry, relative, String::new()),
            Some(false) => {}
            Some(true) => s.spawn(move |_| {
                let open = move || {
                    fs::File::open(&path).map(|f| Box::new(f) as Box<dyn std::io::Read>).map_err(|e| e.to_string())
                };
                self.content_check(open, location, entry, relative);
            }),
        }
    }

    // -- other file systems: one walk ----------------------------------------

    fn search_walk(&self, root: VPath, label: String, depth: u32) {
        self.progress.add_folder();
        self.progress.set_current_with(|| root.display());
        let items = match root.fs.walk(&root.path) {
            Ok(items) => items,
            Err(e) => {
                self.progress.add_error(format!("{}: {e}", root.display()));
                return;
            }
        };
        // Folders whose contents are skipped (hidden ones, links); the walk
        // yields folders before their contents, so they're known in time.
        let mut pruned: HashSet<Vec<String>> = HashSet::new();
        for item in items {
            if self.stopped() {
                return;
            }
            let (rel, entry) = match item {
                Ok(item) => item,
                Err(e) => {
                    self.progress.add_error(format!("{}: {e}", root.display()));
                    continue;
                }
            };
            if rel.is_empty() {
                if !entry.is_dir {
                    self.progress.add_error(format!("{}: not a folder", root.display()));
                    return;
                }
                continue;
            }
            if !pruned.is_empty() && (1..rel.len()).any(|k| pruned.contains(&rel[..k])) {
                continue;
            }
            if entry.is_hidden && !self.q.include_hidden {
                if entry.is_dir {
                    pruned.insert(rel);
                }
                continue;
            }
            let names: Vec<&str> = rel.iter().map(String::as_str).collect();
            let location = || VPath::new(root.fs.clone(), root.fs.join(&root.path, &names));
            let relative = || {
                let mut r = label.clone();
                for part in &rel {
                    if !r.is_empty() {
                        r.push(MAIN_SEPARATOR);
                    }
                    r.push_str(part);
                }
                r
            };
            if entry.is_dir {
                if entry.is_link {
                    pruned.insert(rel.clone());
                } else {
                    self.progress.add_folder();
                    self.progress.set_current_with(|| location().display());
                }
            } else {
                self.progress.add_files(1);
            }
            let archive = self.wants_archive(&entry);
            if self.names.matches(&entry.name) {
                self.consider(location(), entry, relative());
            }
            if archive {
                self.open_archive(location(), relative(), depth + rel.len() as u32);
            }
        }
    }

    // -- other file systems, depth-limited: folder by folder ---------------------

    fn search_listing(&self, root: VPath, label: String, depth: u32) {
        let opts = ListOptions { show_hidden: self.q.include_hidden, ..ListOptions::default() };
        let mut stack = vec![(root, label, depth)];
        while let Some((folder, rel_folder, depth)) = stack.pop() {
            if self.stopped() {
                return;
            }
            self.progress.add_folder();
            self.progress.set_current_with(|| folder.display());
            let entries = match folder.list_dir(opts) {
                Ok(entries) => entries,
                Err(e) => {
                    self.progress.add_error(format!("{}: {e}", folder.display()));
                    continue;
                }
            };
            let deeper = self.deeper(depth);
            let mut subfolders = Vec::new();
            let mut files = 0;
            for entry in entries {
                if self.stopped() {
                    break;
                }
                if !entry.is_dir {
                    files += 1;
                }
                let descend = deeper && entry.is_dir && !entry.is_link;
                let archive = deeper && self.wants_archive(&entry);
                let name_hit = self.names.matches(&entry.name);
                if !(descend || archive || name_hit) {
                    continue;
                }
                let location = folder.child(&[&entry.name]);
                let relative = join_rel(&rel_folder, &entry.name);
                if descend {
                    subfolders.push((location.clone(), relative.clone(), depth + 1));
                } else if archive {
                    match self.opener.map(|o| o.open(&location)) {
                        Some(Ok(archive_root)) => subfolders.push((archive_root, relative.clone(), depth + 1)),
                        Some(Err(e)) => self.progress.add_error(format!("{relative}: {e}")),
                        None => {}
                    }
                }
                if name_hit {
                    self.consider(location, entry, relative);
                }
            }
            self.progress.add_files(files);
            stack.extend(subfolders.into_iter().rev()); // depth-first, in listing order
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_joining() {
        assert_eq!(join_rel("", "a"), "a");
        assert_eq!(join_rel("x", "a"), format!("x{MAIN_SEPARATOR}a"));
    }

    #[test]
    fn entry_filters() {
        let q = Query { min_size: Some(10), max_size: Some(20), ..Query::default() };
        assert!(!entry_ok(&q, &Entry::file("a", 9, 0.0)));
        assert!(entry_ok(&q, &Entry::file("a", 10, 0.0)));
        assert!(!entry_ok(&q, &Entry::file("a", 21, 0.0)));
        assert!(entry_ok(&q, &Entry::dir("d", 0.0))); // sizes don't apply to folders
        let q = Query { kinds: Kinds::Dirs, newer_than: Some(5.0), older_than: Some(7.0), ..Query::default() };
        assert!(!entry_ok(&q, &Entry::file("a", 0, 6.0)));
        assert!(entry_ok(&q, &Entry::dir("d", 5.0)));
        assert!(entry_ok(&q, &Entry::dir("d", 7.0)));
        assert!(!entry_ok(&q, &Entry::dir("d", 7.5)));
        let q = Query { kinds: Kinds::Files, ..Query::default() };
        assert!(!entry_ok(&q, &Entry::dir("d", 0.0)));
    }
}
