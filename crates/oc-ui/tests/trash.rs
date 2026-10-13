//! F8 without waiting: entries leave the panels at once, trash / delete jobs
//! run in the background one after another, beside copies and moves. The
//! trash is an in-memory file system held by a gate (never the real trash).

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use oc_ui::features::fileops;
use oc_ui::panel::{self, LoadOptions};
use oc_ui::testing::{headless_app, press, settle, wait_until};
use oc_ui::{App, CancelToken, Key, MAIN_WINDOW, Severity};
use oc_vfs::testing::{Faults, Gate, MemFS};
use oc_vfs::{Capabilities, VPath};

const WAIT: Duration = Duration::from_secs(20);

struct Env {
    _tmp: tempfile::TempDir,
    app: App,
    mem: Arc<MemFS>,
    gate: Gate,
}

/// Both panels on the root of one in-memory file system with a trash
/// holding "a" … "e"; its trash waits for `gate`.
fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = oc_local::absolute(&fs::canonicalize(tmp.path()).unwrap());
    let (config, left, right) = (base.join("config"), base.join("left"), base.join("right"));
    for d in [&config, &left, &right] {
        fs::create_dir_all(d).unwrap();
    }
    let mut app = headless_app(&config, &left, &right);
    app.resize(MAIN_WINDOW, 120, 40);
    settle(&mut app);
    let mem = Arc::new(MemFS::with_capabilities(Capabilities { writable: true, can_trash: true, can_rename: true, watchable: false }));
    for name in ["a", "b", "c", "d", "e"] {
        mem.add_file(name, name);
    }
    let gate = Gate::new();
    mem.set_trash_gate(Some(gate.clone()));
    for side in 0..2 {
        panel::load(&mut app, side, VPath::new(mem.clone(), ""), LoadOptions::default());
    }
    settle(&mut app);
    Env { _tmp: tmp, app, mem, gate }
}

fn names(app: &App, side: usize) -> Vec<String> {
    app.main.sides[side].panel.entries.iter().map(|e| e.name.clone()).collect()
}

fn cursor(app: &App, side: usize) -> Option<String> {
    app.main.sides[side].panel.cursor_name().map(str::to_owned)
}

fn put_cursor_on(app: &mut App, side: usize, name: &str) {
    let p = &mut app.main.sides[side].panel;
    let row = p.row_of(name).unwrap_or_else(|| panic!("{name} not listed"));
    p.set_cursor(row);
}

/// F8 and confirm.
fn trash(app: &mut App) {
    press(app, Key::F(8));
    assert_eq!(app.modal_titles(), ["Move to trash"]);
    press(app, Key::Enter);
    assert!(app.modal_titles().is_empty());
}

fn wait_done(app: &mut App) {
    wait_until(app, "the trash jobs", WAIT, |app| !fileops::removing(app));
    settle(app);
}

#[test]
fn trashed_entries_leave_both_panels_at_once() {
    let Env { mut app, mem, gate, .. } = env();
    app.main.active = 0;
    for name in ["b", "c"] {
        put_cursor_on(&mut app, 0, name);
        press(&mut app, Key::Insert); // mark, cursor down
    }
    assert_eq!(cursor(&app, 0).as_deref(), Some("d"));
    put_cursor_on(&mut app, 0, "c");
    put_cursor_on(&mut app, 1, "b");
    trash(&mut app);
    // Gone before the trash has even started.
    assert!(mem.trash_calls().is_empty());
    assert_eq!(names(&app, 0), ["a", "d", "e"]);
    assert_eq!(names(&app, 1), ["a", "d", "e"], "the other panel shows the same folder");
    assert!(app.main.sides[0].panel.marked.is_empty());
    assert_eq!(cursor(&app, 0).as_deref(), Some("d"), "the entry after the last one removed");
    assert_eq!(cursor(&app, 1).as_deref(), Some("d"));
    assert!(fileops::removing(&app));
    assert_eq!(app.main.sides[0].panel.subtitle(), "0 dirs, 3 files · memory", "no note for a quick job");
    // A reload meanwhile (a watcher, another operation) doesn't bring them back.
    panel::reload(&mut app, 1);
    wait_until(&mut app, "reload", WAIT, |app| !app.main.sides[1].panel.loading());
    assert_eq!(names(&app, 1), ["a", "d", "e"]);
    // A job that takes a while says so on its panel's bottom line.
    wait_until(&mut app, "the note", WAIT, |app| app.main.sides[0].panel.subtitle() == "moving 2 items to the trash…");
    assert_eq!(app.main.sides[1].panel.subtitle(), "0 dirs, 3 files · memory");
    gate.open();
    wait_done(&mut app);
    assert_eq!(mem.trash_calls(), [["b", "c"]], "one call for the whole job");
    assert_eq!(names(&app, 0), ["a", "d", "e"]);
    assert_eq!(cursor(&app, 0).as_deref(), Some("d"), "the reload keeps the cursor");
    assert_eq!(app.main.sides[0].panel.subtitle(), "0 dirs, 3 files · memory");
    assert_eq!(app.toast.as_ref().map(|t| t.text.as_str()), Some("Moving to trash finished"));
}

#[test]
fn at_the_end_the_cursor_goes_to_the_entry_before() {
    let Env { mut app, gate, .. } = env();
    gate.open();
    put_cursor_on(&mut app, 0, "e");
    trash(&mut app);
    assert_eq!(names(&app, 0), ["a", "b", "c", "d"]);
    assert_eq!(cursor(&app, 0).as_deref(), Some("d"));
    wait_done(&mut app);
    assert_eq!(cursor(&app, 0).as_deref(), Some("d"));
}

#[test]
fn a_second_trash_waits_for_the_running_one() {
    let Env { mut app, mem, gate, .. } = env();
    put_cursor_on(&mut app, 0, "b");
    trash(&mut app);
    put_cursor_on(&mut app, 0, "d");
    trash(&mut app);
    assert!(app.toast.is_none(), "not refused: {:?}", app.toast.as_ref().map(|t| &t.text));
    assert_eq!(names(&app, 0), ["a", "c", "e"]);
    assert_eq!(cursor(&app, 0).as_deref(), Some("e"));
    wait_until(&mut app, "the first job at the gate", WAIT, |_| gate.waiting() == 1);
    wait_until(&mut app, "the note", WAIT, |app| app.main.sides[0].panel.subtitle() == "moving 2 items to the trash…");
    // A job that has started holds the first gate; one that starts later meets the second.
    let second = Gate::new();
    mem.set_trash_gate(Some(second.clone()));
    gate.open();
    wait_until(&mut app, "the second job at its gate", WAIT, |_| second.waiting() == 1);
    assert_eq!(mem.trashed(), ["b"], "one after the other");
    // The first job's end reloads both panels: what the second removes stays hidden.
    wait_until(&mut app, "the reload after the first job", WAIT, |app| {
        app.toast.is_some() && app.main.sides.iter().all(|s| !s.panel.loading())
    });
    assert_eq!(names(&app, 0), ["a", "c", "e"]);
    assert_eq!(names(&app, 1), ["a", "c", "e"]);
    assert!(fileops::removing(&app));
    second.open();
    wait_done(&mut app);
    assert_eq!(mem.trash_calls(), [["b"], ["d"]]);
    assert_eq!(names(&app, 0), ["a", "c", "e"]);
}

#[test]
fn a_failed_trash_brings_the_entries_back() {
    let Env { mut app, mem, gate, .. } = env();
    mem.set_faults(Faults { trash_fails: true, ..Faults::default() });
    put_cursor_on(&mut app, 0, "c");
    trash(&mut app);
    assert_eq!(names(&app, 0), ["a", "b", "d", "e"]);
    gate.open();
    wait_done(&mut app);
    assert_eq!(names(&app, 0), ["a", "b", "c", "d", "e"]);
    assert_eq!(names(&app, 1), ["a", "b", "c", "d", "e"]);
    let toast = app.toast.clone().expect("a toast");
    assert_eq!(toast.severity, Severity::Error);
    assert!(toast.text.contains("simulated trash failure"), "{}", toast.text);
}

#[test]
fn trash_and_copy_dont_wait_for_each_other() {
    let Env { mut app, mem, gate, .. } = env();
    // A copy still running: F8 works anyway.
    app.operation = Some(CancelToken::new());
    put_cursor_on(&mut app, 0, "b");
    trash(&mut app);
    assert!(app.toast.is_none());
    assert_eq!(names(&app, 0), ["a", "c", "d", "e"]);
    // Copy / move stay one at a time.
    put_cursor_on(&mut app, 0, "a");
    press(&mut app, Key::F(5));
    assert_eq!(app.modal_titles(), ["Copy"]);
    press(&mut app, Key::Enter);
    assert_eq!(app.toast.as_ref().map(|t| t.text.as_str()), Some("Another file operation is still running"));
    // No copy running: one starts while the trash job still waits.
    app.operation = None;
    let other = Arc::new(MemFS::new());
    panel::load(&mut app, 1, VPath::new(other.clone(), ""), LoadOptions::default());
    wait_until(&mut app, "the other panel", WAIT, |app| !app.main.sides[1].panel.loading());
    press(&mut app, Key::F(5));
    assert_eq!(app.modal_titles(), ["Copy"]);
    press(&mut app, Key::Enter);
    wait_until(&mut app, "the copy", WAIT, |app| app.operation.is_none() && app.modal_titles().is_empty());
    assert!(other.is_file("a"));
    assert!(mem.trash_calls().is_empty(), "the trash job is still held");
    gate.open();
    wait_done(&mut app);
    assert_eq!(mem.trashed(), ["b"]);
}

#[test]
fn quitting_waits_for_running_and_queued_jobs() {
    let Env { mut app, mem, gate, .. } = env();
    put_cursor_on(&mut app, 0, "b");
    trash(&mut app);
    put_cursor_on(&mut app, 0, "d");
    trash(&mut app);
    gate.open();
    app.shutdown();
    let mut trashed = mem.trashed();
    trashed.sort();
    assert_eq!(trashed, ["b", "d"]);
}

#[test]
fn delete_where_there_is_no_trash_is_queued_too() {
    let Env { mut app, .. } = env();
    let plain = Arc::new(MemFS::new());
    plain.add_file("x", "1");
    plain.add_file("y", "2");
    panel::load(&mut app, 0, VPath::new(plain.clone(), ""), LoadOptions::default());
    wait_until(&mut app, "the listing", WAIT, |app| !app.main.sides[0].panel.loading());
    put_cursor_on(&mut app, 0, "x");
    press(&mut app, Key::F(8));
    assert_eq!(app.modal_titles(), ["Delete permanently"]);
    press(&mut app, Key::Enter);
    assert_eq!(names(&app, 0), ["y"]);
    assert_eq!(cursor(&app, 0).as_deref(), Some("y"));
    wait_done(&mut app);
    assert!(!plain.contains("x"));
    assert_eq!(names(&app, 0), ["y"]);
}
