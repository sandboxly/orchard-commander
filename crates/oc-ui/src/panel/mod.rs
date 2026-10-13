//! A file panel: one directory listing with a cursor and marked files
//! (port of `ui/panel.py`).
//!
//! [`FilePanel`] is plain state with pure key handling (testable without an
//! app); the free functions below do what needs the app: listing in the
//! background ([`load`]), entering archives, opening files, watching the
//! folder for changes.

pub mod draw;
pub mod watch;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use oc_vfs::{Entry, ListOptions, SortKey, VPath};

use crate::app::{App, Severity};
use crate::input::{Key, KeyEvent};
use crate::widgets::ListState;

/// The parent row's name.
pub const PARENT: &str = "..";

/// Typing more letters within this time extends the quick search.
pub const QUICK_SEARCH_TIMEOUT: Duration = Duration::from_millis(1000);

/// After this long without a listing, the status line says "loading…".
const LOADING_HINT: Duration = Duration::from_millis(200);

/// Folder change notifications are collected this long before reloading.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(250);

/// What a key did; the caller acts on the ones that need the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelAction {
    Ignored,
    /// Handled inside the panel (cursor, marks, quick search).
    Consumed,
    /// Enter: open a folder or archive, or the file with its app.
    Activate(String),
    /// Shift+Enter / ⌘Enter: open with the OS default app.
    OpenExternal(String),
    GoParent,
}

pub struct FilePanel {
    /// 0 = left, 1 = right.
    pub side: usize,
    pub location: VPath,
    pub entries: Vec<Entry>,
    /// A ".." row comes first.
    pub has_parent: bool,
    pub marked: HashSet<String>,
    /// Cursor and scroll position over the rows (".." included).
    pub list: ListState,
    /// Entry rows that fit on screen (set from the layout).
    pub rows: usize,
    pub sort: SortKey,
    pub reverse: bool,
    pub show_hidden: bool,
    pub free: Option<u64>,
    pub(crate) request: u64,
    pub(crate) completed: u64,
    pub(crate) loading_location: VPath,
    /// The listing takes long enough to say so.
    pub loading_hint: bool,
    /// Other background work for this panel (opening an archive…).
    pub jobs: usize,
    /// Replaces the status line while `jobs` run ("opening x.zip…").
    pub busy_text: Option<String>,
    quick: Option<(String, Instant)>,
    ndirs: usize,
    nfiles: usize,
    marked_bytes: u64,
    pub(crate) watch: Option<watch::DirWatch>,
    /// The folder the watcher should be on (set at once; the watcher itself
    /// is made on a worker).
    pub(crate) watch_wanted: Option<PathBuf>,
    pub(crate) reload_due: Option<Instant>,
    pub(crate) reload_timer: bool,
}

impl FilePanel {
    pub fn new(side: usize, location: VPath, show_hidden: bool) -> Self {
        FilePanel {
            side,
            loading_location: location.clone(),
            location,
            entries: Vec::new(),
            has_parent: false,
            marked: HashSet::new(),
            list: ListState::new(0),
            rows: 20,
            sort: SortKey::Name,
            reverse: false,
            show_hidden,
            free: None,
            request: 0,
            completed: 0,
            loading_hint: false,
            jobs: 0,
            busy_text: None,
            quick: None,
            ndirs: 0,
            nfiles: 0,
            marked_bytes: 0,
            watch: None,
            watch_wanted: None,
            reload_due: None,
            reload_timer: false,
        }
    }

    // -- rows ---------------------------------------------------------------

    pub fn len(&self) -> usize {
        self.entries.len() + usize::from(self.has_parent)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The entry of `row`; None for the ".." row.
    pub fn entry_at(&self, row: usize) -> Option<&Entry> {
        let i = if self.has_parent { row.checked_sub(1)? } else { row };
        self.entries.get(i)
    }

    pub fn name_at(&self, row: usize) -> Option<&str> {
        if self.has_parent && row == 0 {
            return Some(PARENT);
        }
        self.entry_at(row).map(|e| e.name.as_str())
    }

    pub fn row_of(&self, name: &str) -> Option<usize> {
        if name == PARENT {
            return self.has_parent.then_some(0);
        }
        let off = usize::from(self.has_parent);
        self.entries.iter().position(|e| e.name == name).map(|i| i + off)
    }

    pub fn entry(&self, name: &str) -> Option<&Entry> {
        self.row_of(name).and_then(|r| self.entry_at(r))
    }

    pub fn cursor(&self) -> usize {
        self.list.cursor
    }

    pub fn cursor_name(&self) -> Option<&str> {
        self.name_at(self.list.cursor)
    }

    pub fn cursor_entry(&self) -> Option<&Entry> {
        self.entry_at(self.list.cursor)
    }

    pub fn set_cursor(&mut self, row: usize) {
        self.list.select(row, self.rows);
    }

    /// The location a row's name stands for (search hits: the real file).
    pub fn target(&self, name: &str) -> VPath {
        if name == PARENT {
            return self.location.parent().unwrap_or_else(|| self.location.clone());
        }
        self.location.child(&[name]).resolved()
    }

    /// What the cursor is on (previews follow it).
    pub fn cursor_target(&self) -> Option<VPath> {
        self.cursor_name().map(|n| self.target(n))
    }

    /// Marked entries, or the one under the cursor if nothing is marked.
    pub fn selected_entries(&self) -> Vec<&Entry> {
        if !self.marked.is_empty() {
            return self.entries.iter().filter(|e| self.marked.contains(&e.name)).collect();
        }
        self.cursor_entry().into_iter().collect()
    }

    /// The selection as real locations.
    pub fn selected_paths(&self) -> Vec<VPath> {
        self.selected_entries().iter().map(|e| self.location.child(&[&e.name]).resolved()).collect()
    }

    pub fn options(&self) -> ListOptions {
        ListOptions { show_hidden: self.show_hidden, sort_key: self.sort, reverse: self.reverse }
    }

    /// A listing is on its way, or other work for this panel runs.
    pub fn loading(&self) -> bool {
        self.completed != self.request || self.jobs > 0
    }

    // -- listing results ------------------------------------------------------

    /// Show a new listing. `select` puts the cursor on that name; otherwise
    /// it stays on its row in the same folder, or goes to the top.
    pub fn set_entries(&mut self, location: VPath, entries: Vec<Entry>, has_parent: bool, select: Option<&str>) {
        let changed = location != self.location;
        if changed {
            self.marked.clear();
        }
        let old_row = self.list.cursor;
        self.location = location;
        self.entries = entries;
        self.has_parent = has_parent;
        if !self.marked.is_empty() {
            let names: HashSet<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
            self.marked.retain(|n| names.contains(n.as_str()));
        }
        self.recount();
        self.list.set_len(self.len());
        let row = select.and_then(|s| self.row_of(s));
        let row = row.unwrap_or(if !changed && old_row > 0 { old_row } else { 0 });
        if changed {
            self.list.top = 0;
        }
        self.list.select(row, self.rows);
        self.quick = None;
    }

    fn recount(&mut self) {
        self.ndirs = self.entries.iter().filter(|e| e.is_dir).count();
        self.nfiles = self.entries.len() - self.ndirs;
        self.recount_marked();
    }

    fn recount_marked(&mut self) {
        self.marked_bytes =
            if self.marked.is_empty() { 0 } else { self.entries.iter().filter(|e| self.marked.contains(&e.name)).map(|e| e.size).sum() };
    }

    // -- marks ----------------------------------------------------------------

    /// Space / Insert: mark or unmark the cursor's entry and move down.
    pub fn toggle_mark(&mut self) {
        if let Some(e) = self.cursor_entry() {
            let (name, size) = (e.name.clone(), e.size);
            if !self.marked.remove(&name) {
                self.marked.insert(name);
                self.marked_bytes += size;
            } else {
                self.marked_bytes -= size;
            }
        }
        self.list.move_by(1, self.rows);
    }

    /// Mark or unmark without moving (mouse).
    pub fn toggle_mark_at(&mut self, row: usize) {
        if let Some(name) = self.entry_at(row).map(|e| e.name.clone()) {
            if !self.marked.remove(&name) {
                self.marked.insert(name);
            }
            self.recount_marked();
        }
    }

    pub fn set_all_marks(&mut self, value: bool) {
        self.marked = if value { self.entries.iter().map(|e| e.name.clone()).collect() } else { HashSet::new() };
        self.recount_marked();
    }

    pub fn invert_marks(&mut self) {
        let old = std::mem::take(&mut self.marked);
        self.marked = self.entries.iter().filter(|e| !old.contains(&e.name)).map(|e| e.name.clone()).collect();
        self.recount_marked();
    }

    // -- keys -------------------------------------------------------------------

    /// Panel keys (commander-style). Window shortcuts are handled before this.
    pub fn handle_key(&mut self, ev: &KeyEvent, now: Instant) -> PanelAction {
        let m = ev.mods;
        let plain = m.is_empty();
        match ev.key {
            Key::Enter => {
                let Some(name) = self.cursor_name().map(str::to_owned) else {
                    return PanelAction::Consumed;
                };
                if plain {
                    return PanelAction::Activate(name);
                }
                if m.shift || m.has_primary() {
                    return PanelAction::OpenExternal(name);
                }
                PanelAction::Ignored
            }
            Key::Backspace if plain => PanelAction::GoParent,
            Key::Insert | Key::Space if plain => {
                self.toggle_mark();
                PanelAction::Consumed
            }
            Key::Up | Key::Down | Key::PageUp | Key::PageDown | Key::Home | Key::End if plain => {
                self.quick = None;
                self.list.handle_key(ev, self.rows);
                PanelAction::Consumed
            }
            _ => {
                let Some(text) = ev.typed_text() else {
                    return PanelAction::Ignored;
                };
                match text {
                    "+" => self.set_all_marks(true),
                    "-" => self.set_all_marks(false),
                    "*" => self.invert_marks(),
                    _ => {
                        self.quick_search(text, now);
                    }
                }
                PanelAction::Consumed
            }
        }
    }

    /// Type-to-jump: letters typed in quick succession find the next name
    /// starting with them (case-insensitive); the same letter again cycles.
    pub fn quick_search(&mut self, typed: &str, now: Instant) -> bool {
        let text = match &self.quick {
            Some((t, at)) if now.duration_since(*at) < QUICK_SEARCH_TIMEOUT => {
                format!("{t}{typed}")
            }
            _ => typed.to_owned(),
        };
        let needle = text.to_lowercase();
        // "aa…" from repeating one letter cycles through names starting with it.
        let repeated = needle.chars().count() > 1 && needle.chars().all(|c| needle.starts_with(c));
        let (needle, start) = if repeated || needle.chars().count() == 1 {
            (needle.chars().take(1).collect::<String>(), self.list.cursor + 1)
        } else {
            (needle, self.list.cursor)
        };
        self.quick = Some((text, now));
        let n = self.len();
        for k in 0..n {
            let row = (start + k) % n;
            if self.name_at(row).is_some_and(|name| name != PARENT && name.to_lowercase().starts_with(&needle)) {
                self.list.select(row, self.rows);
                return true;
            }
        }
        false
    }

    // -- chrome -----------------------------------------------------------------

    /// The folder for the title, with the home folder as "~".
    pub fn display_path(&self) -> String {
        let text = self.location.display();
        let home = oc_local::home_dir();
        let home = home.to_string_lossy();
        let sep = std::path::MAIN_SEPARATOR;
        if text == home {
            return "~".into();
        }
        if let Some(rest) = text.strip_prefix(&*home)
            && rest.starts_with(sep)
        {
            return format!("~{rest}");
        }
        text
    }

    /// The bottom line: "N dirs, M files · X free", or the marked totals.
    pub fn subtitle(&self) -> String {
        if let Some(busy) = self.busy_text.as_ref().filter(|_| self.jobs > 0) {
            return busy.clone();
        }
        if self.loading_hint && self.completed != self.request {
            return "loading…".into();
        }
        let fs = &self.location.fs;
        let place = match (fs.status_text(), self.free) {
            (Some(s), _) => s,
            (None, Some(free)) => format!("{} free", oc_vfs::human_size(free)),
            (None, None) => {
                let ro = if fs.capabilities().writable { "" } else { " · read-only" };
                format!("{}{ro}", fs.type_name())
            }
        };
        let info = if self.marked.is_empty() {
            let (d, f) = (self.ndirs, self.nfiles);
            format!("{d} dir{}, {f} file{}", if d == 1 { "" } else { "s" }, if f == 1 { "" } else { "s" })
        } else {
            format!("{} marked, {}", self.marked.len(), oc_vfs::human_size(self.marked_bytes))
        };
        [info, place].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" · ")
    }
}

/// The nearest real folder of a location: itself on the local disk, the
/// folder holding an archive, where a search started. Saved as the side's
/// folder for the next start.
pub fn local_anchor(v: &VPath) -> Option<PathBuf> {
    if v.is_local() {
        return Some(PathBuf::from(&v.path));
    }
    if let Some(c) = v.fs.container() {
        return c.parent().and_then(|p| local_anchor(&p));
    }
    v.fs.exit_to().and_then(|p| local_anchor(&p))
}

// ---------------------------------------------------------------------------
// Working with the app
// ---------------------------------------------------------------------------

/// How to show a listing.
#[derive(Clone, Debug, Default)]
pub struct LoadOptions {
    /// Put the cursor on this name.
    pub select: Option<String>,
    /// Keep the cursor where it is now (reloads).
    pub keep_cursor: bool,
    /// Show the nearest existing ancestor if the folder is gone.
    pub walk_up: bool,
}

struct Listed {
    location: VPath,
    entries: Vec<Entry>,
    has_parent: bool,
    free: Option<u64>,
}

pub fn panel(app: &App, side: usize) -> &FilePanel {
    &app.main.sides[side].panel
}

pub fn panel_mut(app: &mut App, side: usize) -> &mut FilePanel {
    &mut app.main.sides[side].panel
}

/// Show `location` in panel `side` once it is listed (in the background).
/// The current listing stays until then; on error it is kept and the error
/// shown. Newer loads supersede older ones.
pub fn load(app: &mut App, side: usize, location: VPath, opts: LoadOptions) {
    let p = panel_mut(app, side);
    p.request += 1;
    let request = p.request;
    p.loading_location = location.clone();
    let list_opts = p.options();
    app.schedule(LOADING_HINT, move |app| {
        let p = panel_mut(app, side);
        if p.request == request && p.completed != request {
            p.loading_hint = true;
        }
    });
    let shown = location.display();
    app.tasks.run_in_background(
        move |_| -> Result<Listed, String> {
            let mut loc = Some(location.clone());
            if opts.walk_up {
                while let Some(l) = &loc {
                    if l.exists() {
                        break;
                    }
                    loc = l.parent();
                }
            }
            let loc = loc.ok_or_else(|| format!("{shown} no longer exists"))?;
            let entries = loc.list_dir(list_opts).map_err(|e| e.to_string())?;
            let has_parent = loc.parent().is_some();
            let free = loc.fs.free_space(&loc.path);
            Ok(Listed { location: loc, entries, has_parent, free })
        },
        move |app, result| listed(app, side, request, opts, result),
    );
}

fn listed(app: &mut App, side: usize, request: u64, opts: LoadOptions, result: Result<Listed, String>) {
    let p = panel_mut(app, side);
    if request != p.request {
        return; // superseded by a newer load
    }
    p.completed = request;
    p.loading_hint = false;
    let listed = match result {
        Ok(l) => l,
        Err(e) => {
            let what = p.loading_location.display();
            app.error(format!("Cannot open {what}: {e}"));
            return;
        }
    };
    let changed = listed.location != p.location;
    let select = if opts.keep_cursor && !changed { p.cursor_name().map(str::to_owned) } else { opts.select };
    let before = p.cursor_target();
    p.free = listed.free;
    p.set_entries(listed.location, listed.entries, listed.has_parent, select.as_deref());
    if app.watch {
        watch::update(app, side);
    }
    if changed {
        crate::main_window::location_changed(app, side);
    }
    if panel(app, side).cursor_target() != before || changed {
        crate::main_window::cursor_moved(app, side);
    }
}

/// Re-read the folder, keeping cursor and marks. While a move to another
/// folder is still loading, that folder is the one to re-read.
pub fn reload(app: &mut App, side: usize) {
    let p = panel(app, side);
    if p.completed != p.request && p.loading_location != p.location {
        let loc = p.loading_location.clone();
        load(app, side, loc, LoadOptions { walk_up: true, ..Default::default() });
        return;
    }
    let loc = p.location.clone();
    let select = p.cursor_name().map(str::to_owned);
    load(app, side, loc, LoadOptions { select, keep_cursor: true, walk_up: true });
}

/// Backspace / "..": the parent, with the cursor on the folder we came from.
pub fn go_parent(app: &mut App, side: usize) {
    let p = panel(app, side);
    if let Some(parent) = p.location.parent() {
        let name = p.location.name();
        load(app, side, parent, LoadOptions { select: Some(name), ..Default::default() });
    }
}

/// Enter on a name: folder → list it, archive → enter it, file → default app.
pub fn activate(app: &mut App, side: usize, name: &str) {
    if name == PARENT {
        go_parent(app, side);
        return;
    }
    let p = panel(app, side);
    let target = p.target(name);
    let is_dir = p.entry(name).is_some_and(|e| e.is_dir);
    if is_dir {
        load(app, side, target, LoadOptions::default());
    } else if oc_vfs::is_archive_name(name) {
        enter_archive(app, side, target);
    } else {
        open_external(app, side, name);
    }
}

/// Open archive `file` (in the background) and show its root.
pub fn enter_archive(app: &mut App, side: usize, file: VPath) {
    let name = file.name();
    let p = panel_mut(app, side);
    p.jobs += 1;
    p.busy_text = Some(format!("opening {name}…"));
    let file2 = file.clone();
    app.tasks.run_in_background(
        move |_| oc_vfs::open_archive(&file2),
        move |app, opened| {
            let p = panel_mut(app, side);
            p.jobs -= 1;
            match opened {
                Some(Ok(root)) => load(app, side, root, LoadOptions::default()),
                Some(Err(e)) => app.error(format!("Cannot open {name}: {e}")),
                None => app.error(format!("{name} can't be opened as an archive")),
            }
        },
    );
}

/// Open with the OS default application (folders open in the file manager).
/// Files not on the local disk are copied to a temp folder first.
pub fn open_external(app: &mut App, side: usize, name: &str) {
    let p = panel(app, side);
    let target = p.target(name);
    let is_dir = name == PARENT || p.entry(name).is_some_and(|e| e.is_dir);
    if is_dir {
        match target.local_path() {
            Some(local) => open_local(app, name, local),
            None => app.warn("Copy the folder somewhere local (F5) to open it"),
        }
        return;
    }
    if !target.is_local() {
        app.info(format!("Fetching {name}…"));
    }
    let cache = app.cache_dir();
    let name = name.to_owned();
    app.tasks.run_in_background(
        move |token| oc_vfs::ops::materialize_cancellable(&target, &cache, &token.as_fn()),
        move |app, result| match result {
            Ok(local) => open_local(app, &name, local),
            Err(e) => app.error(format!("Cannot open {name}: {e}")),
        },
    );
}

fn open_local(app: &mut App, name: &str, local: PathBuf) {
    // Spawning the opener is quick but still a process start: off the UI thread.
    let name = name.to_owned();
    app.tasks.run_in_background(
        move |_| oc_local::open_with_default_app(&local),
        move |app, result| {
            if let Err(e) = result {
                app.notify(format!("Cannot open {name}: {e}"), Severity::Error);
            }
        },
    );
}

/// A click on a column header: sort by it, or reverse if it already is.
pub fn sort_by(app: &mut App, side: usize, key: SortKey) {
    let p = panel_mut(app, side);
    if p.sort == key {
        p.reverse = !p.reverse;
    } else {
        p.sort = key;
        p.reverse = false;
    }
    let (sort, reverse) = (p.sort.as_str(), p.reverse);
    let name = crate::main_window::SIDE_NAMES[side];
    app.update_settings(move |s| {
        s.extra.insert(format!("{name}_sort"), sort.into());
        s.extra.insert(format!("{name}_sort_reverse"), reverse.into());
    });
    reload(app, side);
}

/// The watcher saw a change in the shown folder: reload after a short quiet time.
pub(crate) fn directory_changed(app: &mut App, side: usize) {
    let p = panel_mut(app, side);
    p.reload_due = Some(Instant::now() + WATCH_DEBOUNCE);
    if !p.reload_timer {
        p.reload_timer = true;
        app.schedule(WATCH_DEBOUNCE, move |app| reload_when_quiet(app, side));
    }
}

fn reload_when_quiet(app: &mut App, side: usize) {
    let p = panel_mut(app, side);
    let now = Instant::now();
    match p.reload_due {
        Some(due) if due > now => {
            app.schedule(due - now, move |app| reload_when_quiet(app, side));
        }
        _ => {
            p.reload_timer = false;
            p.reload_due = None;
            // Never let a refresh of the folder being left supersede a listing on its way.
            if p.completed == p.request {
                reload(app, side);
            }
        }
    }
}

#[cfg(test)]
mod tests;
