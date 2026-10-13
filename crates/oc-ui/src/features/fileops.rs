//! F5 copy, F6 move/rename, F7 make directory, F8 trash / delete — with the
//! File exists dialog (asked from the worker through `ask_ui`), a progress
//! dialog with Cancel, and errors in a toast (port of the file
//! operation half of `main_window.py` and `ui/operations.py`).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oc_vfs::{Conflict, Entry, Progress, TransferCallbacks, VPath, VfsError};

use crate::app::{App, Severity};
use crate::commands::{Command, Registry};
use crate::dialogs::{self, ProgressBox, ProgressState};
use crate::main_window;
use crate::menus::Menu;
use crate::panel::{self, LoadOptions, draw::format_mtime};
use crate::tasks::{CancelToken, Tasks};

/// At most this often the progress dialog is redrawn.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

pub fn register(reg: &mut Registry) {
    reg.add(
        Command::new("files.copy", "Copy", |app| transfer(app, Kind::Copy))
            .help("Copy marked files to the other panel")
            .keys(&["F5"])
            .fkey("F5", "Copy")
            .menu(Menu::Files, "transfer"),
    );
    reg.add(
        Command::new("files.move", "Move / rename", |app| transfer(app, Kind::Move))
            .help("Move or rename marked files")
            .keys(&["F6"])
            .fkey("F6", "Move")
            .menu(Menu::Files, "transfer"),
    );
    reg.add(
        Command::new("files.mkdir", "Make directory", mkdir)
            .help("Create a directory")
            .keys(&["F7"])
            .fkey("F7", "MkDir")
            .menu(Menu::Files, "ops"),
    );
    reg.add(
        Command::new("files.trash", "Move to trash", trash)
            .help("Move marked files to the trash (delete where there is none)")
            .keys(&["F8", "Delete", "Primary+Backspace"])
            .fkey("F8", "Trash")
            .menu(Menu::Files, "ops"),
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Copy,
    Move,
    Trash,
    Delete,
}

impl Kind {
    fn title(self, sources: &[VPath]) -> &'static str {
        match self {
            Kind::Copy if sources.iter().any(|s| !s.is_local()) => "Extracting",
            Kind::Copy => "Copying",
            Kind::Move => "Moving",
            Kind::Trash => "Moving to trash",
            Kind::Delete => "Deleting",
        }
    }
}

/// "'name'", "'dir' (directory)", "3 files and 1 directory".
pub fn describe(entries: &[&Entry]) -> String {
    if let [e] = entries {
        return format!("'{}'{}", e.name, if e.is_dir { " (directory)" } else { "" });
    }
    let ndirs = entries.iter().filter(|e| e.is_dir).count();
    let nfiles = entries.len() - ndirs;
    let mut parts = Vec::new();
    if nfiles > 0 {
        parts.push(format!("{nfiles} file{}", if nfiles == 1 { "" } else { "s" }));
    }
    if ndirs > 0 {
        parts.push(format!("{ndirs} director{}", if ndirs == 1 { "y" } else { "ies" }));
    }
    parts.join(" and ")
}

/// Tell the user (and return true) if `location`'s file system can't do `what`.
fn refuse(app: &mut App, location: &VPath, what: &str, ok: bool) -> bool {
    if !ok {
        app.warn(format!("Can't {what}: {}", location.fs.read_only_reason()));
    }
    !ok
}

/// F5 / F6: ask for the target (the other panel; for a single item when both
/// panels show the same folder, its name — a rename), then run.
pub fn transfer(app: &mut App, kind: Kind) {
    let side = app.main.files_side();
    let source = &app.main.sides[side].panel;
    let entries = source.selected_entries();
    if entries.is_empty() {
        return;
    }
    let what = describe(&entries);
    let single = (entries.len() == 1).then(|| entries[0].name.clone());
    let sources = source.selected_paths();
    let source_loc = source.location.clone();
    let destination = app.main.sides[1 - side].panel.location.clone();
    if kind == Kind::Move
        && let Some(blocked) = sources.iter().find(|p| !p.fs.capabilities().writable).cloned()
        && refuse(app, &blocked, "move from here", false)
    {
        return;
    }
    if refuse(app, &destination, "copy there", destination.fs.capabilities().writable) {
        return;
    }
    let verb = if kind == Kind::Copy { "Copy" } else { "Move" };
    let default = match &single {
        Some(name) if destination == source_loc => name.clone(),
        _ => destination.display(),
    };
    let message = format!("{verb} {what} to:");
    dialogs::ask_text(app, verb, &message, &default, verb, move |app, value| {
        let Some(value) = value else { return };
        let target = if value == destination.display() {
            destination
        } else {
            let path = super::navigation::expand_home(&value);
            if path.is_absolute() {
                oc_local::local(path)
            } else if source_loc.fs.capabilities().writable {
                // A new name in the same folder (rename), or a path below it.
                source_loc.child(&[&value])
            } else {
                let base = panel::local_anchor(&source_loc).unwrap_or_else(oc_local::home_dir);
                oc_local::local(base.join(path))
            }
        };
        run(app, kind, sources, Some(target), side);
    });
}

/// F7.
pub fn mkdir(app: &mut App) {
    let side = app.main.files_side();
    let here = main_window::location(app, side);
    if refuse(app, &here, "create a directory here", here.fs.capabilities().writable) {
        return;
    }
    let message = format!("Create in {}:", here.display());
    dialogs::ask_text(app, "Make directory", &message, "", "Create", move |app, name| {
        let Some(name) = name else { return };
        let first = name.replace('\\', "/").split('/').next().unwrap_or_default().to_owned();
        let at = here.clone();
        app.tasks.run_in_background(
            move |_| oc_vfs::ops::mkdir(&at, &name),
            move |app, result| match result {
                Ok(_) => panel::load(app, side, here, LoadOptions { select: Some(first), ..Default::default() }),
                Err(e) => app.error(format!("Cannot create directory: {}", panel::error_reason(&e))),
            },
        );
    });
}

/// F8: to the trash, or — where there is none (servers) — delete permanently.
pub fn trash(app: &mut App) {
    let side = app.main.files_side();
    let p = &app.main.sides[side].panel;
    let entries = p.selected_entries();
    if entries.is_empty() {
        return;
    }
    let what = describe(&entries);
    let paths = p.selected_paths();
    if let Some(blocked) = paths.iter().find(|p| !(p.fs.capabilities().can_trash || p.fs.capabilities().writable)).cloned()
        && refuse(app, &blocked, "delete here", false)
    {
        return;
    }
    if paths.iter().all(|p| p.fs.capabilities().can_trash) {
        let msg = format!("Move {what} to the trash?");
        dialogs::confirm(app, "Move to trash", &msg, "Trash", move |app| run(app, Kind::Trash, paths, None, side));
    } else {
        let fs_name = paths.iter().find(|p| !p.fs.capabilities().can_trash).map(|p| p.fs.type_name()).unwrap_or_default();
        let msg = format!("{fs_name} has no trash.\n\nDelete {what} permanently? This can't be undone.");
        dialogs::confirm(app, "Delete permanently", &msg, "Delete", move |app| run(app, Kind::Delete, paths, None, side));
    }
}

/// One line per file for the File exists dialog: path, then size and date.
fn describe_file(shown: &str) -> String {
    match std::fs::metadata(Path::new(shown)) {
        Ok(m) => {
            let size = if m.is_dir() { "<DIR>".to_owned() } else { oc_vfs::human_size(m.len()) };
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(String::new(), |d| format_mtime(d.as_secs_f64()));
            format!("{shown}\n    {size}, {mtime}")
        }
        Err(_) => shown.to_owned(),
    }
}

/// The worker's conflict handler: asks the UI, remembers "all" answers.
fn conflict_asker(tasks: Tasks) -> impl FnMut(&str, &str) -> Conflict {
    let mut sticky: Option<Conflict> = None;
    move |src: &str, dst: &str| {
        if let Some(choice) = sticky {
            return choice;
        }
        let (src, dst) = (describe_file(src), describe_file(dst));
        match tasks.ask_ui(move |app, reply| dialogs::ask_conflict(app, &src, &dst, reply)) {
            Some((choice, all)) => {
                if all {
                    sticky = Some(choice);
                }
                choice
            }
            None => Conflict::Cancel,
        }
    }
}

/// Run an operation on a worker; afterwards both panels reload keeping
/// their cursors and the source's marks are cleared.
pub fn run(app: &mut App, kind: Kind, sources: Vec<VPath>, target: Option<VPath>, side: usize) {
    // Until the worker has ended, also while a cancelled one winds down
    // (1.x: `isRunning()`); `finished` clears it.
    if app.operation.is_some() {
        app.warn("Another file operation is still running");
        return;
    }
    let title = kind.title(&sources);
    let token = CancelToken::new();
    app.operation = Some(token.clone());
    let state = Arc::new(Mutex::new(ProgressState::default()));
    let progress_dialog = matches!(kind, Kind::Copy | Kind::Move).then(|| app.show(ProgressBox::new(title, state.clone(), token.clone())));
    let tasks = app.tasks.clone();
    let running = Arc::new(AtomicBool::new(true));
    app.operation_running = Some(running.clone());
    app.tasks.spawn(
        move || {
            // Cleared however the worker ends (also on a panic): quitting waits for it.
            let _running = RunningGuard(running);
            match kind {
                Kind::Trash => oc_vfs::ops::trash(&sources),
                Kind::Delete => oc_vfs::ops::delete(&sources),
                Kind::Copy | Kind::Move => {
                    let target = target.expect("copy/move has a target");
                    let mut ask = conflict_asker(tasks.clone());
                    let mut last = Instant::now() - PROGRESS_INTERVAL;
                    let mut report = |p: &Progress| {
                        let now = Instant::now();
                        if now - last >= PROGRESS_INTERVAL || p.files_done == p.files_total {
                            last = now;
                            state.lock().unwrap_or_else(|e| e.into_inner()).progress = p.clone();
                            tasks.post(|app| app.redraw());
                        }
                    };
                    let cancelled = token.as_fn();
                    let callbacks = TransferCallbacks { on_conflict: &mut ask, on_progress: Some(&mut report), is_cancelled: &cancelled };
                    oc_local::copy(&sources, &target, kind == Kind::Move, callbacks)
                }
            }
        },
        move |app, result| finished(app, side, title, progress_dialog, result),
    );
}

struct RunningGuard(Arc<AtomicBool>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// After an operation: close the progress dialog, report (errors too as a
/// toast, like 1.x), refresh, and make the source panel the active one.
pub fn finished(app: &mut App, side: usize, title: &str, dialog: Option<crate::modal::ModalId>, result: oc_vfs::Result<()>) {
    app.operation = None;
    app.operation_running = None;
    if let Some(id) = dialog {
        app.close_modal(id);
    }
    match result {
        Ok(()) => app.info(format!("{title} finished")),
        Err(VfsError::Cancelled) => app.notify(format!("{title} cancelled"), Severity::Warning),
        Err(e) => app.notify(e.to_string(), Severity::Error),
    }
    app.main.sides[side].panel.set_all_marks(false);
    // Out of the terminal first: it reports the focus loss to the side it was on.
    crate::features::terminal::focus_panels(app);
    app.main.active = side;
    for s in 0..2 {
        panel::reload(app, s);
    }
    crate::commands::files_changed(app);
}
