//! Files dropped from the system file manager: the panel or folder row under
//! the pointer, copy or move (the volume rule or the held keys), through the
//! F5 / F6 prompt.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use oc_ui::testing::{headless_app, press, settle, type_text, wait_until};
use oc_ui::{App, DropAction, Key, MAIN_WINDOW};

struct Env {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    left: PathBuf,
    right: PathBuf,
    /// Where the dragged files come from (another app's window).
    outside: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = oc_local::absolute(&fs::canonicalize(tmp.path()).unwrap());
    let (config, left, right, outside) = (base.join("config"), base.join("left"), base.join("right"), base.join("outside"));
    for d in [&config, &left, &right, &outside] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(outside.join("a.txt"), "a").unwrap();
    fs::create_dir_all(outside.join("photos")).unwrap();
    fs::write(outside.join("photos/p.jpg"), "p").unwrap();
    Env { _tmp: tmp, config, left, right, outside }
}

fn app(e: &Env) -> App {
    let mut app = headless_app(&e.config, &e.left, &e.right);
    app.resize(MAIN_WINDOW, 120, 40);
    settle(&mut app);
    app
}

/// A cell on row `row` of `side`'s file list (0: the first row shown).
fn cell(app: &App, side: usize, row: u16) -> (u16, u16) {
    let frame = oc_ui::main_window::layout(app).frames[side];
    let rows = oc_ui::panel::draw::layout(frame).rows;
    (rows.x + 2, rows.y + row)
}

fn wait_dialog(app: &mut App, title: &str) {
    let t = title.to_owned();
    wait_until(app, &format!("dialog {title}"), Duration::from_secs(10), move |app| app.modal_titles().last().is_some_and(|x| *x == t));
}

fn wait_done(app: &mut App) {
    wait_until(app, "the operation", Duration::from_secs(20), |app| app.modal_titles().is_empty() && app.operation.is_none());
    settle(app);
}

fn toast(app: &App) -> String {
    app.toast.as_ref().map(|t| t.text.clone()).unwrap_or_default()
}

#[test]
fn a_drop_on_a_panel_moves_within_the_volume_after_the_prompt() {
    let e = env();
    let mut app = app(&e);
    assert_eq!(app.main.active, 0);
    let paths = vec![e.outside.join("a.txt"), e.outside.join("photos")];
    let below_the_files = cell(&app, 1, 10);
    app.drop_files(MAIN_WINDOW, paths, Some(below_the_files), DropAction::Auto);
    wait_dialog(&mut app, "Move");
    let text = app.top_modal_text().unwrap();
    assert!(text.starts_with("Move 1 file and 1 directory to:"), "{text}");
    assert!(text.ends_with(&format!("\n{}", e.right.display())), "the right panel's folder: {text}");
    press(&mut app, Key::Enter);
    wait_done(&mut app);
    assert!(e.right.join("a.txt").exists() && e.right.join("photos/p.jpg").exists());
    assert!(!e.outside.join("a.txt").exists(), "moved, not copied");
    assert_eq!(app.main.active, 1, "the panel dropped on is active");
}

#[test]
fn held_keys_choose_copy_and_folder_rows_take_the_drop() {
    let e = env();
    fs::create_dir_all(e.left.join("inbox")).unwrap();
    let mut app = app(&e);
    // Row 0 is "..", row 1 the folder.
    assert_eq!(app.main.sides[0].panel.name_at(1), Some("inbox"));
    app.main.sides[0].panel.marked.insert("inbox".into());
    app.drop_files(MAIN_WINDOW, vec![e.outside.join("a.txt")], Some(cell(&app, 0, 1)), DropAction::Copy);
    wait_dialog(&mut app, "Copy");
    assert!(app.top_modal_text().unwrap().ends_with(&format!("\n{}", e.left.join("inbox").display())));
    press(&mut app, Key::Enter);
    wait_done(&mut app);
    assert!(e.left.join("inbox/a.txt").exists() && e.outside.join("a.txt").exists(), "copied");
    assert!(app.main.sides[0].panel.marked.contains("inbox"), "a drop leaves the panel's marks alone");
}

#[test]
fn without_a_position_the_active_panel_takes_it_and_the_prompt_can_change_the_target() {
    let e = env();
    let mut app = app(&e);
    app.drop_files(MAIN_WINDOW, vec![e.outside.join("a.txt")], None, DropAction::Move);
    wait_dialog(&mut app, "Move");
    assert!(app.top_modal_text().unwrap().ends_with(&format!("\n{}", e.left.display())));
    // A relative name: next to where the file came from (a rename there).
    type_text(&mut app, "b.txt");
    press(&mut app, Key::Enter);
    wait_done(&mut app);
    assert!(e.outside.join("b.txt").exists() && !e.outside.join("a.txt").exists());
}

#[test]
fn drops_where_they_are_nothing_happens_and_dialogs_come_first() {
    let e = env();
    let mut app = app(&e);
    oc_ui::panel::load(&mut app, 0, oc_local::local(&e.outside), oc_ui::panel::LoadOptions::default());
    settle(&mut app);
    app.drop_files(MAIN_WINDOW, vec![e.outside.join("a.txt")], Some(cell(&app, 0, 10)), DropAction::Auto);
    settle(&mut app);
    assert!(app.modal_titles().is_empty());

    press(&mut app, Key::F(7));
    app.drop_files(MAIN_WINDOW, vec![e.outside.join("a.txt")], Some(cell(&app, 1, 10)), DropAction::Auto);
    assert_eq!(app.modal_titles(), ["Make directory"]);
    assert!(toast(&app).starts_with("Close the dialog first"), "{}", toast(&app));
}

#[test]
fn same_volume_tells_drives_apart() {
    let e = env();
    assert!(oc_local::same_volume(&e.outside.join("a.txt"), &e.right));
    #[cfg(target_os = "macos")]
    assert!(!oc_local::same_volume(&e.outside.join("a.txt"), std::path::Path::new("/dev")));
}
