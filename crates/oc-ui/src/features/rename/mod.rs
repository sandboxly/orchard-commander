//! F2 renames the entry under the cursor in place; multi-rename (Ctrl+M)
//! on `oc_rename`: ⌘M opens the
//! dialog for the marked items (or the one under the cursor), the batch runs
//! on a worker, ⌘Z gives the last batch its old names back. Port of the
//! multi-rename half of `main_window.py` plus `ui/rename_ui.py`.
//!
//! Flow: the folders of the items are listed on a worker first (so the
//! preview's "already exists" checks never touch the disk), then the dialog
//! opens; metadata for `[=field]` placeholders is read on another worker
//! while the dialog is open. After a batch, features that show files by
//! location (search results) hear about it through
//! [`crate::commands::files_moved`], and both panels reload, following
//! renamed cursors and folders.

pub mod dialog;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oc_grid::{Frame, Style};
use oc_rename::{Done, ExistingNames, Item, Plan, Rule, existing_names, item_for, renamed_locations};
use oc_vfs::{Entry, VPath};

pub use dialog::{Field, RenameDialog, RenameModal};

use crate::app::App;
use crate::commands::{self, Command, Registry};
use crate::dialogs::{self, InputBox, dialog_rects, draw_bar, draw_box};
use crate::geom::Rect;
use crate::input::Event;
use crate::menus::Menu;
use crate::modal::{Modal, ModalId, ModalOutcome};
use crate::panel::{self, LoadOptions};
use crate::widgets::{DrawCtx, put};

/// The progress dialog shows when a batch takes longer than this.
const PROGRESS_DELAY: Duration = Duration::from_millis(250);
/// At most this often the progress dialog is redrawn.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

pub fn register(reg: &mut Registry) {
    reg.add(
        Command::new("files.rename", "Rename", rename_in_place)
            .help("Rename the file under the cursor in its folder")
            .keys(&["F2", "Shift+F6"])
            .menu(Menu::Files, "ops"),
    );
    reg.add(
        Command::new("files.multi_rename", "Multi-rename", multi_rename)
            .help("Rename marked files with a pattern: [N], [C], dates, search & replace")
            .keys(&["Primary+M"])
            .menu(Menu::Files, "ops"),
    );
    reg.add(
        Command::new("files.undo_rename", "Undo rename", undo_rename)
            .help("Undo the last rename or multi-rename")
            .keys(&["Primary+Z"])
            .menu(Menu::Files, "ops"),
    );
}

/// The feature's app-wide state.
#[derive(Default)]
pub struct RenameState {
    /// The dialog's last settings, for the next time (this session only, like 1.x).
    pub rule: Rule,
    /// The last finished batch, for ⌘Z.
    pub last: Option<Done>,
    /// The open dialog.
    pub dialog: Option<RenameDialog>,
    /// A batch (or its undo) is running.
    pub running: bool,
    progress: Option<ModalId>,
    next_id: u64,
}

/// The open Multi-rename dialog, if any (tests, diagnostics).
pub fn dialog(app: &App) -> Option<&RenameDialog> {
    app.try_state::<RenameState>().and_then(|s| s.dialog.as_ref())
}

/// Change the open dialog (tests): `f` gets the dialog and the app.
pub fn with_open_dialog(app: &mut App, f: impl FnOnce(&mut RenameDialog, &mut App)) {
    if let Some(id) = dialog(app).map(|d| d.id) {
        dialog::with_dialog(app, id, f);
    }
}

/// ⌘M: rename the marked items (or the cursor's) of the active panel.
pub fn multi_rename(app: &mut App) {
    let side = app.main.files_side();
    let p = &app.main.sides[side].panel;
    let entries: Vec<Entry> = p.selected_entries().into_iter().cloned().collect();
    if entries.is_empty() {
        return;
    }
    // The real files: search hits are renamed where they are.
    let paths = p.selected_paths();
    if crate::features::fileops::refuse_drive(app, &p.location.clone(), &paths, "rename") {
        return;
    }
    for path in &paths {
        let caps = path.fs.capabilities();
        if !caps.writable {
            app.warn(format!("Can't rename here: {}", path.fs.read_only_reason()));
            return;
        }
        if !caps.can_rename {
            app.warn(format!("Can't rename on {}", path.fs.type_name()));
            return;
        }
    }
    if app.state::<RenameState>().running {
        app.warn("The last multi-rename is still running: wait for it to finish");
        return;
    }
    let items: Vec<Item> = paths.iter().zip(&entries).map(|(p, e)| item_for(p, e)).collect();
    app.tasks.run_in_background(
        move |_| {
            let exists = existing_names(&items);
            (items, exists)
        },
        move |app, (items, exists)| open_dialog(app, side, items, Arc::new(exists)),
    );
}

/// Show the dialog for `items` (of panel `side`) with the session's last rule.
pub fn open_dialog(app: &mut App, side: usize, items: Vec<Item>, exists: Arc<ExistingNames>) {
    let st = app.state::<RenameState>();
    if st.dialog.is_some() {
        return;
    }
    st.next_id += 1;
    let (id, rule) = (st.next_id, st.rule.clone());
    let mut d = RenameDialog::new(id, side, items, &rule, exists);
    d.update(app);
    let title = d.title.clone();
    app.state::<RenameState>().dialog = Some(d);
    app.show(RenameModal { title });
}

/// The dialog closed: remember its settings; on Rename, run the batch.
pub(crate) fn finish_dialog(app: &mut App, mut d: RenameDialog, accept: bool) {
    d.close(app);
    app.state::<RenameState>().rule = d.current_rule();
    if accept {
        let plan = std::mem::take(&mut d.plan);
        run(app, d.side, Job::Rename(plan));
    }
}

/// ⌘Z: give the files of the last batch their old names back.
pub fn undo_rename(app: &mut App) {
    let last = app.state::<RenameState>().last.clone().filter(|d| !d.renames.is_empty());
    let Some(done) = last else {
        app.warn("No rename to undo");
        return;
    };
    if app.state::<RenameState>().running {
        app.warn("The last multi-rename is still running: wait for it to finish");
        return;
    }
    let n = done.renames.len();
    let side = app.main.files_side();
    let message = if n == 1 { "Give the item its old name back?".to_owned() } else { format!("Give {n} items their old names back?") };
    dialogs::confirm(app, "Undo rename", &message, "Undo rename", move |app| run(app, side, Job::Undo(done)));
}

/// F2 / ⇧F6: rename the entry under the cursor (not the marked ones) in its
/// folder, through a one-item plan: a taken name is refused, a change of
/// letter case only works, and ⌘Z undoes it like a batch. Marks stay.
pub fn rename_in_place(app: &mut App) {
    let side = app.main.files_side();
    let p = &app.main.sides[side].panel;
    let Some(entry) = p.cursor_entry() else { return }; // ".." or nothing
    let row = entry.name.clone();
    // Search results: the real file (the row's name is its relative path).
    let location = p.target(&row);
    let item = item_for(&location, entry);
    if crate::features::fileops::refuse_drive(app, &p.location.clone(), std::slice::from_ref(&location), "rename") {
        return;
    }
    if !oc_rename::rename_supported(&location) {
        app.warn(format!("Can't rename here: {}", location.fs.read_only_reason()));
        return;
    }
    if app.state::<RenameState>().running {
        app.warn("The last multi-rename is still running: wait for it to finish");
        return;
    }
    let old = item.name.clone();
    let stem = oc_rename::split_name(&old, item.is_dir).0.len();
    let message = format!("Rename '{old}' to:");
    let ask = InputBox::new("Rename", &message, &item.name.clone(), "Rename", move |app, value| {
        let Some(new) = value.filter(|v| *v != old) else { return };
        if let Some(problem) = oc_rename::name_problem(&new) {
            app.error(format!("Can't rename to '{new}': {problem}"));
            return;
        }
        app.state::<RenameState>().running = true;
        app.tasks.run_in_background(
            move |_| {
                let plan = oc_rename::plan_one(&item, &new, &|l: &VPath| l.exists());
                match plan.problems().next() {
                    Some(c) => Err(format!("'{}': {}", c.new, c.error)),
                    None => Ok(oc_rename::execute(&plan, None)),
                }
            },
            move |app, result| renamed_in_place(app, side, row, result),
        );
    });
    app.show(ask.select_to(stem));
}

fn renamed_in_place(app: &mut App, side: usize, row: String, result: Result<Done, String>) {
    app.state::<RenameState>().running = false;
    let done = match result {
        Ok(done) => done,
        Err(e) => return app.error(format!("Can't rename to {e}")),
    };
    let Some(r) = done.renames.first().cloned() else {
        app.error(format!("Can't rename {}", done.errors.first().map_or("", String::as_str)));
        for s in 0..2 {
            panel::reload(app, s);
        }
        return;
    };
    let moved = renamed_locations(&done);
    commands::files_moved(app, &moved);
    app.state::<RenameState>().last = Some(done);
    // The row's new name (search results: the relative path's last part).
    let new_row = format!("{}{}", row.strip_suffix(r.old_name.as_str()).unwrap_or(""), r.new_name);
    let p = &mut app.main.sides[side].panel;
    if p.marked.remove(&row) {
        p.marked.insert(new_row.clone());
    }
    let here = p.location.clone();
    panel::load(app, side, here, LoadOptions { select: Some(new_row), walk_up: true, ..Default::default() });
    refresh(app, 1 - side, &moved);
    let key = if cfg!(target_os = "macos") { "⌘Z" } else { "Ctrl+Z" };
    app.info(format!("Renamed to '{}' · {key} to undo", r.new_name));
}

enum Job {
    Rename(Plan),
    Undo(Done),
}

/// Run a batch on a worker; a progress dialog shows if it takes a while.
fn run(app: &mut App, side: usize, job: Job) {
    let undo = matches!(job, Job::Undo(_));
    let progress: Arc<Mutex<(usize, usize)>> = Arc::default();
    let st = app.state::<RenameState>();
    st.running = true;
    st.next_id += 1;
    let batch = st.next_id;
    let title = if undo { "Undoing rename" } else { "Renaming" };
    let shown = progress.clone();
    app.schedule(PROGRESS_DELAY, move |app| {
        let st = app.state::<RenameState>();
        if st.running && st.next_id == batch && st.progress.is_none() {
            let id = app.show(RenameProgress { title: title.into(), progress: shown });
            app.state::<RenameState>().progress = Some(id);
        }
    });
    let tasks = app.tasks.clone();
    app.tasks.spawn(
        move || match job {
            Job::Rename(plan) => {
                let mut last = Instant::now() - PROGRESS_INTERVAL;
                let mut report = |done: usize, total: usize| {
                    *progress.lock().unwrap_or_else(|e| e.into_inner()) = (done, total);
                    if last.elapsed() >= PROGRESS_INTERVAL {
                        last = Instant::now();
                        tasks.post(|app| app.redraw());
                    }
                };
                oc_rename::execute(&plan, Some(&mut report))
            }
            Job::Undo(done) => oc_rename::undo(&done),
        },
        move |app, done| renamed(app, side, done, undo),
    );
}

/// After a batch: search results follow, ⌘Z is armed, panels reload.
fn renamed(app: &mut App, side: usize, done: Done, undo: bool) {
    let st = app.state::<RenameState>();
    st.running = false;
    if let Some(id) = st.progress.take() {
        app.close_modal(id);
    }
    let moved = renamed_locations(&done);
    commands::files_moved(app, &moved);
    let n = done.renames.len();
    let what = format!("{} {n} item{}", if undo { "Undid renaming" } else { "Renamed" }, if n == 1 { "" } else { "s" });
    let errors = done.errors.clone();
    app.state::<RenameState>().last = if undo { None } else { Some(done) };
    app.main.sides[side].panel.set_all_marks(false);
    for s in 0..2 {
        refresh(app, s, &moved);
    }
    if let Some(first) = errors.first() {
        app.warn(format!("{what}; {} failed: {first}", errors.len()));
    } else if undo {
        app.info(what);
    } else {
        let key = if cfg!(target_os = "macos") { "⌘Z" } else { "Ctrl+Z" };
        app.info(format!("{what} · {key} to undo"));
    }
}

/// Where `location` is now if it, or a folder above it, was renamed.
pub fn relocated(location: &VPath, moved: &HashMap<VPath, VPath>) -> Option<VPath> {
    if moved.is_empty() {
        return None;
    }
    let mut below: Vec<String> = Vec::new();
    let mut at = Some(location.clone());
    while let Some(a) = at {
        if let Some(target) = moved.get(&a) {
            below.reverse();
            let names: Vec<&str> = below.iter().map(String::as_str).collect();
            return Some(if names.is_empty() { target.clone() } else { target.child(&names) });
        }
        below.push(a.name());
        at = a.parent();
    }
    None
}

/// Reload panel `side` after renames: a renamed folder it shows is followed,
/// and so is a renamed item under the cursor.
fn refresh(app: &mut App, side: usize, moved: &HashMap<VPath, VPath>) {
    let p = panel::panel(app, side);
    let location = p.location.clone();
    let new_location = relocated(&location, moved);
    let shown = new_location.clone().unwrap_or_else(|| location.clone());
    let renamed = p
        .cursor_target()
        .and_then(|c| moved.get(&c).cloned())
        .filter(|new| new.parent().as_ref() == Some(&shown))
        .map(|new| new.name());
    if new_location.is_none() && renamed.is_none() {
        panel::reload(app, side);
        return;
    }
    // In a renamed folder the cursor stays on its name (the folder's
    // location changes, so "keep the cursor" alone would reset it).
    let keep_cursor = renamed.is_none();
    let select = renamed.or_else(|| p.cursor_name().filter(|n| *n != panel::PARENT).map(str::to_owned));
    panel::load(app, side, shown, LoadOptions { select, keep_cursor, walk_up: true, ..Default::default() });
}

/// "Renaming 120 of 3000" with a bar; no Cancel (a batch stops only between
/// its two phases, which would leave temporary names behind).
struct RenameProgress {
    title: String,
    progress: Arc<Mutex<(usize, usize)>>,
}

impl Modal for RenameProgress {
    fn title(&self) -> &str {
        &self.title
    }

    fn draw(&self, _app: &App, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        let (outer, _) = dialog_rects(screen, 60, 2);
        let content = draw_box(f, outer, &self.title, ctx);
        let (done, total) = *self.progress.lock().unwrap_or_else(|e| e.into_inner());
        let text = if total == 0 { "Preparing…".to_owned() } else { format!("{done} of {total}") };
        put(f, content.x, content.y, &text, Style::new(c.foreground, c.surface), content.w);
        let fraction = if total > 0 { done as f64 / total as f64 } else { 0.0 };
        draw_bar(f, Rect::new(content.x, content.y + 1, content.w, 1), fraction, ctx);
    }

    fn handle_event(&mut self, _app: &mut App, _ev: &Event, _screen: Rect) -> ModalOutcome {
        ModalOutcome::Consumed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_follow_renamed_folders() {
        let root = oc_local::local(std::env::temp_dir());
        let a = root.child(&["a"]);
        let b = root.child(&["b"]);
        let moved: HashMap<VPath, VPath> = [(a.clone(), b.clone())].into();
        assert_eq!(relocated(&a, &moved), Some(b.clone()));
        assert_eq!(relocated(&a.child(&["x", "y"]), &moved), Some(b.child(&["x", "y"])));
        assert_eq!(relocated(&root.child(&["c"]), &moved), None);
        assert_eq!(relocated(&a, &HashMap::new()), None);
    }
}
