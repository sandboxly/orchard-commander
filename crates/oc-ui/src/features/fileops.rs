//! F5 copy, F6 move/rename, F7 make directory, F8 trash / delete — with the
//! File exists dialog (asked from the worker through `ask_ui`), a progress
//! dialog with Cancel, and errors in a toast (port of the file
//! operation half of `main_window.py` and `ui/operations.py`). Files
//! dropped from the system file manager are copied or moved at once
//! ([`dropped`]).
//!
//! Trash and delete don't wait for the disk: the entries leave the panels at
//! once and the work runs as background jobs, one after another, beside a
//! copy or move ([`Removals`]).

use std::collections::{HashSet, VecDeque};
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

/// A trash / delete job running this long says so on its panel's bottom line.
const REMOVAL_NOTE_DELAY: Duration = Duration::from_millis(500);
/// A trash / delete job's entries stay hidden at most this long; then they
/// are listed again until it ends (an OS trash that hangs mustn't make
/// files look gone that are still there).
const REMOVAL_HIDE_LIMIT: Duration = Duration::from_secs(5);

pub fn register(reg: &mut Registry) {
    reg.add(
        Command::new("files.copy", "Copy", |app| transfer(app, Kind::Copy))
            .help("Copy marked files to the other panel")
            .keys(&["F5"])
            .fkey("F5", "Copy")
            .menu(Menu::Files, "ops"),
    );
    reg.add(
        Command::new("files.move", "Move / rename", |app| transfer(app, Kind::Move))
            .help("Move or rename marked files")
            .keys(&["F6"])
            .fkey("F6", "Move")
            .menu(Menu::Files, "ops"),
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
    reg.on_shutdown(finish_removals);
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
    let default = match &single {
        Some(name) if destination == source_loc => name.clone(),
        _ => destination.display(),
    };
    ask_target(app, kind, &what, &default, sources, source_loc, destination, Origin::Panel(side));
}

/// Where a copy / move came from: the marked files of a panel (whose marks
/// go afterwards), or a drop on panel `n` from another app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Panel(usize),
    Dropped(usize),
}

impl Origin {
    /// The panel that is active afterwards.
    fn side(self) -> usize {
        match self {
            Origin::Panel(s) | Origin::Dropped(s) => s,
        }
    }
}

/// The Copy / Move prompt: the target (prefilled with `default`), then run.
/// A relative path is taken from `source_loc`.
#[allow(clippy::too_many_arguments)]
fn ask_target(
    app: &mut App,
    kind: Kind,
    what: &str,
    default: &str,
    sources: Vec<VPath>,
    source_loc: VPath,
    destination: VPath,
    origin: Origin,
) {
    let verb = if kind == Kind::Copy { "Copy" } else { "Move" };
    let message = format!("{verb} {what} to:");
    dialogs::ask_text(app, verb, &message, default, verb, move |app, value| {
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
        start(app, kind, sources, Some(target), origin);
    });
}

/// Copy or move for files dropped from another app (the binary reads the
/// modifier keys held at the drop).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DropAction {
    /// The system file manager's rule: move within a volume, copy across volumes.
    #[default]
    Auto,
    Copy,
    Move,
}

/// Files dropped on the main window from the system file manager: into the
/// folder row under the pointer, else that panel's folder (`at`: the cell;
/// None: the active panel). No prompt, like the system file manager: the
/// copy / move starts at once (File exists dialog, progress, Cancel).
pub fn dropped(app: &mut App, paths: Vec<std::path::PathBuf>, at: Option<(u16, u16)>, action: DropAction) {
    if paths.is_empty() {
        return;
    }
    if app.has_modal(crate::MAIN_WINDOW) {
        app.warn("Close the dialog first, then drop the files again");
        return;
    }
    let (side, target) = main_window::drop_target(app, at);
    if refuse(app, &target, "copy there", target.fs.capabilities().writable) {
        return;
    }
    let local_target = target.is_local().then(|| std::path::PathBuf::from(&target.path));
    // The volume check on a worker: the files may be on a slow network drive.
    app.tasks.run_in_background(
        move |_| {
            let same = local_target.as_ref().is_some_and(|t| paths.iter().all(|p| oc_local::same_volume(p, t)));
            (paths, same)
        },
        move |app, (paths, same)| {
            let parent = paths[0].parent().map(oc_local::local);
            if paths.iter().all(|p| p.parent().map(oc_local::local) == parent) && parent.as_ref() == Some(&target) {
                return; // dropped where they already are
            }
            let kind = match action {
                DropAction::Copy => Kind::Copy,
                DropAction::Move => Kind::Move,
                DropAction::Auto if same => Kind::Move,
                DropAction::Auto => Kind::Copy,
            };
            let sources: Vec<VPath> = paths.iter().map(oc_local::local).collect();
            start(app, kind, sources, Some(target), Origin::Dropped(side));
        },
    );
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
/// their cursors and the source's marks are cleared. Trash and delete are
/// queued instead (see [`Removals`]).
pub fn run(app: &mut App, kind: Kind, sources: Vec<VPath>, target: Option<VPath>, side: usize) {
    start(app, kind, sources, target, Origin::Panel(side));
}

fn start(app: &mut App, kind: Kind, sources: Vec<VPath>, target: Option<VPath>, origin: Origin) {
    let side = origin.side();
    if matches!(kind, Kind::Trash | Kind::Delete) {
        remove(app, kind, sources, side);
        return;
    }
    // Until the worker has ended, also while a cancelled one winds down
    // (1.x: `isRunning()`); `finished` clears it. One copy/move at a time:
    // its File exists dialogs need the user's undivided attention.
    if app.operation.is_some() {
        app.warn("Another file operation is still running");
        return;
    }
    let title = kind.title(&sources);
    let token = CancelToken::new();
    app.operation = Some(token.clone());
    let state = Arc::new(Mutex::new(ProgressState::default()));
    let progress_dialog = Some(app.show(ProgressBox::new(title, state.clone(), token.clone())));
    let tasks = app.tasks.clone();
    let running = Arc::new(AtomicBool::new(true));
    app.operation_running = Some(running.clone());
    app.tasks.spawn(
        move || {
            // Cleared however the worker ends (also on a panic): quitting waits for it.
            let _running = RunningGuard(running);
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
        },
        move |app, result| finished(app, origin, title, progress_dialog, result),
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
pub fn finished(app: &mut App, origin: Origin, title: &str, dialog: Option<crate::modal::ModalId>, result: oc_vfs::Result<()>) {
    let side = origin.side();
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
    if let Origin::Panel(s) = origin {
        app.main.sides[s].panel.set_all_marks(false);
    }
    // Out of the terminal first: it reports the focus loss to the side it was on.
    crate::features::terminal::focus_panels(app);
    app.main.active = side;
    for s in 0..2 {
        panel::reload(app, s);
    }
    crate::commands::files_changed(app);
}

// ---------------------------------------------------------------------------
// Trash / delete in the background
// ---------------------------------------------------------------------------

/// The trash and delete jobs: one runs, the others wait their turn (first
/// come, first served). They don't take `app.operation`, so F8 works while
/// a copy runs or another trash job is still busy, and a copy can start
/// meanwhile. Their paths stay hidden from every listing until their job
/// has ended ([`hide_removed`]), so a reload in between doesn't bring them
/// back and they can't be chosen again — but for [`REMOVAL_HIDE_LIMIT`] at
/// most: a job still waiting or running by then lists its entries again
/// (the bottom line says it's still deleting), so a stuck OS trash can't
/// make files look gone that are still there.
///
/// Why this can take long at all: local files go through `oc_local`'s
/// `trash_paths` to the OS trash. On Windows that is the shell's file
/// operation, which visits every file inside a folder to make it
/// restorable: slow for big trees, folders kept in sync by cloud storage
/// clients and network drives. Nothing here can speed that up; it can only
/// stay out of the way.
#[derive(Default)]
pub struct Removals {
    running: Option<Removal>,
    queue: VecDeque<Removal>,
    /// True while the running job's worker thread lives (quitting waits for it).
    worker: Option<Arc<AtomicBool>>,
    next_id: u64,
    /// The running job takes long enough to say so on the bottom line.
    noted: bool,
    /// [`REMOVAL_HIDE_LIMIT`], unless a test shortens it.
    hide_limit: Option<Duration>,
}

struct Removal {
    id: u64,
    kind: Kind,
    paths: Vec<VPath>,
    side: usize,
    /// Hidden from listings (until [`REMOVAL_HIDE_LIMIT`] has passed).
    hidden: bool,
}

impl Removals {
    fn jobs(&self) -> impl Iterator<Item = &Removal> {
        self.running.iter().chain(&self.queue)
    }
}

/// Tests: how long a trash / delete job's entries stay hidden.
#[doc(hidden)]
pub fn set_removal_hide_limit(app: &mut App, limit: Duration) {
    app.state::<Removals>().hide_limit = Some(limit);
}

/// A trash or delete job is running or waiting.
pub fn removing(app: &App) -> bool {
    app.try_state::<Removals>().is_some_and(|r| r.jobs().next().is_some())
}

/// Leave out of a listing of `location` what a trash / delete job is still
/// removing (search results: by the files they stand for).
pub(crate) fn hide_removed(app: &App, location: &VPath, entries: &mut Vec<Entry>) {
    let Some(removals) = app.try_state::<Removals>() else { return };
    let pending: HashSet<&VPath> = removals.jobs().filter(|j| j.hidden).flat_map(|j| &j.paths).collect();
    if !pending.is_empty() {
        entries.retain(|e| !pending.contains(&location.child(&[&e.name]).resolved()));
    }
}

/// Queue a confirmed trash / delete and take its entries out of both panels now.
fn remove(app: &mut App, kind: Kind, paths: Vec<VPath>, side: usize) {
    let gone: HashSet<VPath> = paths.iter().cloned().collect();
    let r = app.state::<Removals>();
    r.next_id += 1;
    let id = r.next_id;
    r.queue.push_back(Removal { id, kind, paths, side, hidden: true });
    let limit = r.hide_limit.unwrap_or(REMOVAL_HIDE_LIMIT);
    app.schedule(limit, move |app| {
        let r = app.state::<Removals>();
        let Some(job) = r.running.iter_mut().chain(r.queue.iter_mut()).find(|j| j.id == id) else { return };
        job.hidden = false;
        for s in 0..2 {
            panel::reload(app, s);
        }
    });
    panel::panel_mut(app, side).set_all_marks(false);
    for s in 0..2 {
        let p = panel::panel_mut(app, s);
        // A panel inside a folder on its way out goes to where that folder
        // was (it would list files that are about to go, too).
        if let Some(out) = inside(&p.location, &gone).and_then(|f| f.parent()) {
            panel::load(app, s, out, LoadOptions { walk_up: true, ..Default::default() });
            continue;
        }
        let before = p.cursor_target();
        let location = p.location.clone();
        if p.remove_entries(|name| gone.contains(&location.child(&[name]).resolved())) && p.cursor_target() != before {
            main_window::cursor_moved(app, s);
        }
    }
    start_next_removal(app);
    update_removal_notes(app);
}

/// The outermost of `gone` that `location` is (or is inside of), on the
/// same file system.
fn inside(location: &VPath, gone: &HashSet<VPath>) -> Option<VPath> {
    let mut found = None;
    let mut at = Some(location.clone());
    while let Some(a) = at.filter(|a| a.same_fs(location)) {
        if gone.contains(&a) {
            found = Some(a.clone());
        }
        at = a.parent();
    }
    found
}

/// The work itself, on a worker. A panic (the OS trash failing in an
/// unexpected way) becomes an error, so the queue goes on and the entries
/// come back.
fn remove_now(kind: Kind, paths: &[VPath]) -> oc_vfs::Result<()> {
    let work = || match kind {
        Kind::Delete => oc_vfs::ops::delete(paths),
        // All paths of a file system in one call: one OS operation per job.
        _ => oc_vfs::ops::trash(paths),
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
        .unwrap_or_else(|_| Err(VfsError::Operation(format!("{} failed unexpectedly", kind.title(paths)))))
}

fn start_next_removal(app: &mut App) {
    let r = app.state::<Removals>();
    if r.running.is_some() {
        return;
    }
    let Some(job) = r.queue.pop_front() else { return };
    let (id, kind, paths) = (job.id, job.kind, job.paths.clone());
    r.running = Some(job);
    r.noted = false;
    let running = Arc::new(AtomicBool::new(true));
    r.worker = Some(running.clone());
    app.schedule(REMOVAL_NOTE_DELAY, move |app| {
        let r = app.state::<Removals>();
        if r.running.as_ref().is_some_and(|j| j.id == id) {
            r.noted = true;
            update_removal_notes(app);
        }
    });
    app.tasks.spawn(
        move || {
            // Cleared however the worker ends: quitting waits for it.
            let _running = RunningGuard(running);
            remove_now(kind, &paths)
        },
        move |app, result| removed(app, id, result),
    );
}

/// A job ended: report like any operation, start the next one, and reload
/// both panels keeping their cursors (what failed comes back; what the
/// other jobs remove stays hidden).
fn removed(app: &mut App, id: u64, result: oc_vfs::Result<()>) {
    let r = app.state::<Removals>();
    let Some(job) = r.running.take_if(|j| j.id == id) else { return };
    r.worker = None;
    r.noted = false;
    let title = job.kind.title(&job.paths);
    if result.is_ok() {
        // Before the reloads: search results would otherwise list them again
        // until their prune has run.
        let gone: HashSet<VPath> = job.paths.iter().cloned().collect();
        crate::commands::files_removed(app, &gone);
    }
    match result {
        Ok(()) => app.info(format!("{title} finished")),
        Err(VfsError::Cancelled) => app.notify(format!("{title} cancelled"), Severity::Warning),
        Err(e) => app.notify(e.to_string(), Severity::Error),
    }
    start_next_removal(app);
    update_removal_notes(app);
    for s in 0..2 {
        panel::reload(app, s);
    }
    crate::commands::files_changed(app);
}

/// "Deleting 12 items…" on the bottom line of the panel the jobs came
/// from, once the running one takes a while.
fn update_removal_notes(app: &mut App) {
    let r = app.state::<Removals>();
    let notes: [Option<String>; 2] = std::array::from_fn(|side| {
        let jobs: Vec<&Removal> = r.jobs().filter(|j| j.side == side).collect();
        if !r.noted || jobs.is_empty() {
            return None;
        }
        let n: usize = jobs.iter().map(|j| j.paths.len()).sum();
        let items = format!("{n} item{}", if n == 1 { "" } else { "s" });
        if jobs.iter().all(|j| j.kind == Kind::Delete) {
            Some(format!("Permanently deleting {items}…"))
        } else {
            Some(format!("Deleting {items}…"))
        }
    });
    for (side, note) in notes.into_iter().enumerate() {
        panel::panel_mut(app, side).note = note;
    }
    app.redraw();
}

/// Quitting: wait for the running job like for a copy (bounded), and run
/// the queued ones too, after it: the user confirmed them, and their entries
/// are gone from the panels already.
fn finish_removals(app: &mut App) {
    let mut r = app.take_state::<Removals>();
    let mut workers: Vec<Arc<AtomicBool>> = r.worker.take().into_iter().collect();
    let queued: Vec<Removal> = r.queue.drain(..).collect();
    if !queued.is_empty() {
        let running = Arc::new(AtomicBool::new(true));
        let before = workers.first().cloned();
        workers.push(running.clone());
        let spawned = std::thread::Builder::new().name("oc-trash".into()).spawn(move || {
            let _running = RunningGuard(running);
            // Still one at a time.
            while before.as_ref().is_some_and(|w| w.load(Ordering::SeqCst)) {
                std::thread::sleep(Duration::from_millis(10));
            }
            for job in queued {
                if let Err(e) = remove_now(job.kind, &job.paths) {
                    log::warn!("{}: {e}", job.kind.title(&job.paths));
                }
            }
        });
        if spawned.is_err() {
            log::error!("cannot start a worker thread");
        }
    }
    let end = Instant::now() + crate::app::OPERATION_QUIT_WAIT;
    while workers.iter().any(|w| w.load(Ordering::SeqCst)) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(10));
    }
}
