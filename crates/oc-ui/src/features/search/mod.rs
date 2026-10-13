//! Find files (Total Commander's Alt+F7 + "Feed to listbox"; port of the
//! search half of `main_window.py` and `ui/search_ui.py`).
//!
//! ⌘F / ⌥F7 opens [`dialog::FindFilesDialog`]; the search runs on a worker
//! (`oc_search`) and its hits appear in the active panel as an
//! [`oc_search::SearchResultsFS`] folder while it runs: every hit stands for
//! the real file (`FileSystem::real_location`), so copy/move/trash/preview
//! and Enter act on the real files, and ".." leads back to where the search
//! started. A timer refreshes the panels showing running results (new hits,
//! the "searching…" counters); Esc stops the search; ⇧⌘F shows the last
//! results again. After file operations the hits whose files are gone are
//! dropped ([`Registry::on_files_changed`]); renames call [`relocate`].

pub mod dialog;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use oc_search::{Query, SearchResultsFS};
use oc_vfs::{FileSystem, VPath};

use crate::app::App;
use crate::commands::{Command, Registry};
use crate::main_window::{self, SideMode};
use crate::menus::Menu;
use crate::panel::{self, LoadOptions};
pub use dialog::{FindFilesDialog, Form};

/// How often running results are refreshed in the panels.
pub const REFRESH: Duration = Duration::from_millis(250);

pub fn register(reg: &mut Registry) {
    reg.add(
        Command::new("search.find", "Find files", find_files)
            .help("Search by name, contents, size and date (Total Commander: Alt+F7)")
            .keys(&["Alt+F7", "Primary+F"])
            .menu(Menu::Files, "find"),
    );
    reg.add(
        Command::new("search.last", "Last search results", show_last_results)
            .help("Show the results of the last search again")
            .keys(&["Primary+Shift+F"])
            .menu(Menu::Files, "find"),
    );
    reg.add(
        Command::new("search.stop", "Stop search", stop_search)
            .help("Stop the running search")
            .keys(&["Escape"])
            // Esc stays with the side views (terminal, preview) and only
            // stops something while a search runs.
            .enabled(|app| is_running(app) && app.main.sides[app.main.active].mode == SideMode::Files),
    );
    reg.on_files_changed(prune_then_reload);
}

/// A search the app knows about: the last one and any shown in a panel.
struct Tracked {
    results: Arc<SearchResultsFS>,
    cancel: Arc<AtomicBool>,
    /// "Search finished: N hits" was said.
    announced: bool,
}

/// App-wide search state (`app.state::<SearchState>()`).
#[derive(Default)]
pub struct SearchState {
    /// The dialog's entries, kept until the app quits.
    pub form: Form,
    last: Option<Arc<SearchResultsFS>>,
    tracked: Vec<Tracked>,
    /// The refresh timer is scheduled.
    ticking: bool,
}

impl SearchState {
    /// The results ⇧⌘F shows.
    pub fn last_results(&self) -> Option<&Arc<SearchResultsFS>> {
        self.last.as_ref()
    }
}

/// Whether `fs` is these results.
fn is_results(fs: &Arc<dyn FileSystem>, results: &Arc<SearchResultsFS>) -> bool {
    std::ptr::addr_eq(Arc::as_ptr(fs), Arc::as_ptr(results))
}

/// The panels showing `results` (or about to).
fn shown_sides(app: &App, results: &Arc<SearchResultsFS>) -> Vec<usize> {
    (0..2)
        .filter(|&s| {
            let p = &app.main.sides[s].panel;
            is_results(&p.location.fs, results) || is_results(&p.loading_location.fs, results)
        })
        .collect()
}

/// A search is running.
pub fn is_running(app: &App) -> bool {
    app.try_state::<SearchState>().is_some_and(|s| s.tracked.iter().any(|t| t.results.is_running()))
}

/// The folder the search starts in: the side's folder, or where the
/// results it shows came from.
fn search_here(app: &App, side: usize) -> VPath {
    let here = main_window::location(app, side);
    if app.try_state::<SearchState>().is_some_and(|s| s.tracked.iter().any(|t| is_results(&here.fs, &t.results))) {
        return here.fs.exit_to().unwrap_or_else(|| oc_local::local(oc_local::home_dir()));
    }
    here
}

/// ⌘F / ⌥F7.
pub fn find_files(app: &mut App) {
    let side = app.main.files_side();
    let here = search_here(app, side);
    let form = app.state::<SearchState>().form.clone();
    let d = FindFilesDialog::new(here, side, &form);
    // A background check of typed folders closes the dialog by its id.
    let pending = d.pending.clone();
    let id = app.show(d);
    dialog::lock(&pending).id = Some(id);
}

/// The dialog accepted: remember the entries and search.
pub(crate) fn start_from_dialog(app: &mut App, query: Query, form: Form, side: usize) {
    app.state::<SearchState>().form = form;
    start_search(app, query, side);
}

/// Search in the background; the hits appear in panel `side` as a folder
/// while it runs. Returns the results (they fill up as the search goes).
pub fn start_search(app: &mut App, query: Query, side: usize) -> Arc<SearchResultsFS> {
    let results = Arc::new(SearchResultsFS::new(query));
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let st = app.state::<SearchState>();
        st.last = Some(results.clone());
        st.tracked.push(Tracked { results: results.clone(), cancel: cancel.clone(), announced: false });
    }
    show_results(app, side, results.clone());
    let worker = results.clone();
    app.tasks.spawn(
        move || worker.run(|| cancel.load(Ordering::Relaxed)).map_err(|e| e.to_string()),
        finished,
    );
    schedule_refresh(app);
    results
}

fn show_results(app: &mut App, side: usize, results: Arc<SearchResultsFS>) {
    if app.main.sides[side].mode != SideMode::Files {
        main_window::set_mode(app, side, SideMode::Files);
    }
    app.main.active = side;
    let fs: Arc<dyn FileSystem> = results;
    panel::load(app, side, VPath::new(fs, ""), LoadOptions::default());
}

/// A search ended: show its final hits and say how many.
fn finished(app: &mut App, outcome: Result<(), String>) {
    if let Err(e) = outcome {
        app.error(format!("Search failed: {e}"));
    }
    let mut done: Vec<Arc<SearchResultsFS>> = Vec::new();
    for t in app.state::<SearchState>().tracked.iter_mut().filter(|t| !t.results.is_running() && !t.announced) {
        t.announced = true;
        done.push(t.results.clone());
    }
    for results in done {
        let sides = shown_sides(app, &results);
        for &side in &sides {
            panel::reload(app, side);
        }
        let is_last = app.state::<SearchState>().last.as_ref().is_some_and(|l| Arc::ptr_eq(l, &results));
        if !sides.is_empty() || is_last {
            let n = results.hit_count();
            let how = if results.was_cancelled() { "stopped" } else { "finished" };
            app.info(format!("Search {how}: {} hit{}", thousands(n as u64), if n == 1 { "" } else { "s" }));
        }
    }
    forget_unused(app);
}

/// Drop results nobody can see again (not the last, not shown, finished).
fn forget_unused(app: &mut App) {
    let keep: Vec<bool> = {
        let st = app.try_state::<SearchState>();
        let Some(st) = st else { return };
        st.tracked
            .iter()
            .map(|t| {
                t.results.is_running()
                    || st.last.as_ref().is_some_and(|l| Arc::ptr_eq(l, &t.results))
                    || !shown_sides(app, &t.results).is_empty()
            })
            .collect()
    };
    let mut keep = keep.into_iter();
    app.state::<SearchState>().tracked.retain(|_| keep.next().unwrap_or(true));
}

fn schedule_refresh(app: &mut App) {
    let st = app.state::<SearchState>();
    if st.ticking {
        return;
    }
    st.ticking = true;
    app.schedule(REFRESH, refresh);
}

/// While searches run: show new hits (reload) and progress (redraw).
fn refresh(app: &mut App) {
    app.state::<SearchState>().ticking = false;
    let running: Vec<Arc<SearchResultsFS>> =
        app.state::<SearchState>().tracked.iter().filter(|t| t.results.is_running()).map(|t| t.results.clone()).collect();
    if running.is_empty() {
        return;
    }
    for results in &running {
        for side in shown_sides(app, results) {
            let p = &app.main.sides[side].panel;
            let listed = p.entries.len();
            // New hits: list again (the cursor stays); otherwise only the
            // counters in the status line changed.
            if listed != results.hit_count() && !p.loading() {
                panel::reload(app, side);
            }
        }
    }
    app.redraw();
    schedule_refresh(app);
}

/// Esc: stop the running searches (the hits found so far stay).
pub fn stop_search(app: &mut App) {
    if let Some(st) = app.try_state::<SearchState>() {
        for t in st.tracked.iter().filter(|t| t.results.is_running()) {
            t.cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// ⇧⌘F.
pub fn show_last_results(app: &mut App) {
    let Some(results) = app.try_state::<SearchState>().and_then(|s| s.last.clone()) else {
        let key = if cfg!(target_os = "macos") { "⌘F" } else { "Ctrl+F" };
        app.warn(format!("No search yet: Alt+F7 or {key} to find files"));
        return;
    };
    if !app.state::<SearchState>().tracked.iter().any(|t| Arc::ptr_eq(&t.results, &results)) {
        // Finished and forgotten as "tracked": track it again while shown.
        let cancel = Arc::new(AtomicBool::new(false));
        let announced = !results.is_running();
        app.state::<SearchState>().tracked.push(Tracked { results: results.clone(), cancel, announced });
    }
    let side = app.main.files_side();
    let running = results.is_running();
    show_results(app, side, results);
    if running {
        schedule_refresh(app);
    }
}

/// After file operations: forget hits that were moved or deleted (checked
/// on a worker), then list the results again.
fn prune_then_reload(app: &mut App) {
    let all: Vec<Arc<SearchResultsFS>> = match app.try_state::<SearchState>() {
        Some(st) => st.tracked.iter().map(|t| t.results.clone()).collect(),
        None => return,
    };
    if all.is_empty() {
        return;
    }
    app.tasks.run_in_background(
        move |_| {
            let pruned: Vec<_> = all.into_iter().filter(|r| r.prune() > 0).collect();
            pruned
        },
        |app, pruned| {
            for results in pruned {
                for side in shown_sides(app, &results) {
                    panel::reload(app, side);
                }
            }
        },
    );
}

/// Follow renamed files: `moved` maps old to new locations (multi-rename,
/// undo). Hits inside renamed folders follow too; panels showing the
/// results are listed again.
pub fn relocate(app: &mut App, moved: &HashMap<VPath, VPath>) {
    if moved.is_empty() {
        return;
    }
    let all: Vec<Arc<SearchResultsFS>> = match app.try_state::<SearchState>() {
        Some(st) => st.tracked.iter().map(|t| t.results.clone()).collect(),
        None => return,
    };
    for results in all {
        results.relocate(moved);
        for side in shown_sides(app, &results) {
            panel::reload(app, side);
        }
    }
}

/// "1,204".
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn thousands() {
        assert_eq!(super::thousands(0), "0");
        assert_eq!(super::thousands(1204), "1,204");
        assert_eq!(super::thousands(18_331_000), "18,331,000");
    }
}
