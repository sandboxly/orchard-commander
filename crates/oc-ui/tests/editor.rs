//! The F4 editor driven without a window (ports of 1.x `tests/test_editor.py`):
//! editing and saving (line endings, permissions), the unsaved-changes
//! question on close and on quit, Shift+F4, find/replace, read-only files,
//! other file systems, reloading, undo/redo, go to line, highlighting.

use std::fs;
use std::path::Path;
use std::time::Duration;

use oc_ui::features::editor::{EditorState, EditorWindow, open_editor};
use oc_ui::testing::{headless_app, press, press_with, settle, type_text, wait_until};
use oc_ui::{App, Event, Key, KeyEvent, MAIN_WINDOW, Mods, PlatformRequest, WinId};

struct Env {
    _tmp: tempfile::TempDir,
    config: std::path::PathBuf,
    left: std::path::PathBuf,
    right: std::path::PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = oc_local::absolute(&fs::canonicalize(tmp.path()).unwrap());
    let (config, left, right) = (base.join("config"), base.join("left"), base.join("right"));
    for d in [&config, &left, &right] {
        fs::create_dir_all(d).unwrap();
    }
    Env { _tmp: tmp, config, left, right }
}

fn app(e: &Env) -> App {
    let mut app = headless_app(&e.config, &e.left, &e.right);
    app.resize(MAIN_WINDOW, 120, 40);
    settle(&mut app);
    app
}

fn select(app: &mut App, name: &str) {
    settle(app);
    let p = &mut app.main.sides[0].panel;
    let row = p.row_of(name).unwrap_or_else(|| panic!("{name} not listed"));
    p.set_cursor(row);
}

fn editors(app: &App) -> Vec<WinId> {
    app.try_state::<EditorState>().map(|s| s.windows.iter().map(|(id, _)| *id).collect()).unwrap_or_default()
}

fn ed(app: &App, id: WinId) -> &EditorWindow {
    app.window::<EditorWindow>(id).expect("editor window")
}

/// The editor for `name`, once its file is read.
fn editor_for(app: &mut App, name: &str) -> WinId {
    let n = name.to_owned();
    wait_until(app, "editor window", Duration::from_secs(10), move |app| {
        editors(app).iter().any(|id| app.window::<EditorWindow>(*id).is_some_and(|w| w.name() == n && (w.loaded || w.load_failed)))
    });
    let id = *editors(app).iter().find(|id| ed(app, **id).name() == name).unwrap();
    app.resize(id, 100, 40);
    id
}

fn key(app: &mut App, id: WinId, k: Key, mods: Mods) {
    app.handle_event(id, Event::Key(KeyEvent::new(k, mods)));
}

fn type_in(app: &mut App, id: WinId, text: &str) {
    for c in text.chars() {
        let ev = if c == '\n' { KeyEvent::plain(Key::Enter) } else { KeyEvent::typed(&c.to_string()) };
        app.handle_event(id, Event::Key(ev));
    }
}

fn text(app: &App, id: WinId) -> String {
    ed(app, id).buf.text()
}

fn primary() -> Mods {
    Mods::primary()
}

fn primary_shift() -> Mods {
    Mods { shift: true, ..Mods::primary() }
}

fn primary_alt() -> Mods {
    Mods { alt: true, ..Mods::primary() }
}

fn wait_saved(app: &mut App, id: WinId) {
    wait_until(app, "save", Duration::from_secs(10), move |app| app.window::<EditorWindow>(id).is_none_or(|w| !w.saving && !w.modified()));
    settle(app);
}

fn wait_dialog(app: &mut App, title: &str) {
    let t = title.to_owned();
    wait_until(app, &format!("dialog {title}"), Duration::from_secs(10), move |app| app.modal_titles().last().is_some_and(|x| *x == t));
}

#[test]
fn f4_edits_and_saves_keeping_line_endings_and_permissions() {
    let e = env();
    let path = e.left.join("a.txt");
    fs::write(&path, b"alpha\r\nbeta\r\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    }
    let mut app = app(&e);
    select(&mut app, "a.txt");
    press(&mut app, Key::F(4)); // Total Commander's F4
    let id = editor_for(&mut app, "a.txt");
    assert!(app.take_requests().iter().any(|r| matches!(r, PlatformRequest::OpenWindow { id: w, .. } if *w == id)));
    assert_eq!(text(&app, id), "alpha\nbeta\n");
    assert!(ed(&app, id).status().contains("CRLF"), "{}", ed(&app, id).status());
    key(&mut app, id, Key::Down, primary_mods_doc_end());
    type_in(&mut app, id, "gamma\n");
    assert!(ed(&app, id).modified());
    assert!(ed(&app, id).frame_title().starts_with("● "));
    key(&mut app, id, Key::Char('s'), primary());
    wait_saved(&mut app, id);
    assert_eq!(fs::read(&path).unwrap(), b"alpha\r\nbeta\r\ngamma\r\n", "line endings kept");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o7777, 0o640, "permissions kept");
    }
    let leftovers: Vec<_> =
        fs::read_dir(&e.left).unwrap().map(|d| d.unwrap().file_name()).filter(|n| n.to_string_lossy().starts_with(".a.txt.")).collect();
    assert!(leftovers.is_empty(), "no temporary files left: {leftovers:?}");
    assert!(!ed(&app, id).frame_title().starts_with("● "));
    assert!(app.window_title(id).starts_with("a.txt — "));
}

/// ⌘↓ on macOS, Ctrl+End elsewhere: the end of the text.
fn primary_mods_doc_end() -> Mods {
    if cfg!(target_os = "macos") { Mods::primary() } else { Mods { ctrl: true, ..Mods::NONE } }
}

#[test]
fn f4_is_in_the_function_key_bar_and_edits_files_only() {
    let e = env();
    fs::create_dir(e.left.join("dir")).unwrap();
    let mut app = app(&e);
    let edit = app.registry.get("files.edit").unwrap();
    assert_eq!(edit.fkey, Some(("F4", "Edit")));
    assert_eq!(edit.title, "Edit");
    assert!(app.registry.get("files.new_file").is_some());
    select(&mut app, "dir");
    press(&mut app, Key::F(4));
    assert!(editors(&app).is_empty());
    assert!(app.toast.as_ref().unwrap().text.contains("F4 edits files"));
}

#[test]
fn one_window_per_file() {
    let e = env();
    fs::write(e.left.join("a.txt"), "x").unwrap();
    let mut app = app(&e);
    select(&mut app, "a.txt");
    press(&mut app, Key::F(4));
    let id = editor_for(&mut app, "a.txt");
    app.take_requests();
    press(&mut app, Key::F(4));
    assert_eq!(editors(&app), [id]);
    assert!(app.take_requests().contains(&PlatformRequest::RaiseWindow(id)));
}

#[test]
fn closing_with_unsaved_changes_asks() {
    let e = env();
    let path = e.left.join("a.txt");
    fs::write(&path, "text\n").unwrap();
    let mut app = app(&e);
    open_editor(&mut app, oc_local::local(&path));
    let id = editor_for(&mut app, "a.txt");
    type_in(&mut app, id, "changed ");
    app.close_requested(id);
    assert_eq!(app.modal_titles(), ["Unsaved changes"]);
    assert!(app.top_modal_text().unwrap().contains("Save the changes to a.txt?"));
    key(&mut app, id, Key::Escape, Mods::NONE); // Cancel
    assert!(app.window_ids().contains(&id) && app.modal_titles().is_empty());
    // ⌘W asks again; Enter = Save, then the window closes.
    key(&mut app, id, Key::Char('w'), primary());
    wait_dialog(&mut app, "Unsaved changes");
    key(&mut app, id, Key::Enter, Mods::NONE);
    wait_until(&mut app, "editor closed", Duration::from_secs(10), |app| editors(app).is_empty());
    assert_eq!(fs::read_to_string(&path).unwrap(), "changed text\n");
    assert!(!app.window_ids().contains(&id));
    assert!(app.take_requests().contains(&PlatformRequest::CloseWindow(id)));
}

#[test]
fn quitting_asks_about_unsaved_editors() {
    let e = env();
    let path = e.left.join("a.txt");
    fs::write(&path, "text\n").unwrap();
    let mut app = app(&e);
    open_editor(&mut app, oc_local::local(&path));
    let id = editor_for(&mut app, "a.txt");
    type_in(&mut app, id, "x");
    press_with(&mut app, Key::Char('q'), Mods::primary());
    assert_eq!(app.modal_titles(), ["Unsaved changes"]);
    assert!(!app.quitting);
    key(&mut app, id, Key::Escape, Mods::NONE); // Cancel: keep running
    assert!(!app.quitting && !app.quit_pending && app.window_ids().contains(&id));
    app.quit();
    // "Don't save": the editor closes and quitting carries on.
    key(&mut app, id, Key::Right, Mods::NONE);
    key(&mut app, id, Key::Enter, Mods::NONE);
    wait_until(&mut app, "quit", Duration::from_secs(10), |app| app.quitting);
    assert!(app.take_requests().contains(&PlatformRequest::Quit));
    assert_eq!(fs::read_to_string(&path).unwrap(), "text\n");
}

#[test]
fn shift_f4_creates_a_file_and_edits_it() {
    let e = env();
    let mut app = app(&e);
    press_with(&mut app, Key::F(4), Mods::SHIFT);
    assert_eq!(app.modal_titles(), ["New file"]);
    type_text(&mut app, "notes.md");
    press(&mut app, Key::Enter);
    let id = editor_for(&mut app, "notes.md");
    assert!(e.left.join("notes.md").exists());
    assert_eq!(ed(&app, id).language, "Markdown");
    type_in(&mut app, id, "# Hello\n");
    key(&mut app, id, Key::Char('s'), primary());
    wait_saved(&mut app, id);
    assert_eq!(fs::read_to_string(e.left.join("notes.md")).unwrap(), "# Hello\n");
    settle(&mut app);
    assert_eq!(app.main.sides[0].panel.cursor_name(), Some("notes.md"));
    // The name is taken now.
    press_with(&mut app, Key::F(4), Mods::SHIFT);
    type_text(&mut app, "notes.md");
    press(&mut app, Key::Enter);
    wait_until(&mut app, "warning", Duration::from_secs(10), |app| app.toast.as_ref().is_some_and(|t| t.text.contains("already exists")));
}

#[test]
fn find_and_replace() {
    let e = env();
    let path = e.left.join("a.txt");
    fs::write(&path, "cat dog cat\nCat\n").unwrap();
    let mut app = app(&e);
    open_editor(&mut app, oc_local::local(&path));
    let id = editor_for(&mut app, "a.txt");
    key(&mut app, id, Key::Char('f'), primary_alt()); // ⌥⌘F
    assert!(ed(&app, id).bar.visible && ed(&app, id).bar.replace);
    type_in(&mut app, id, "cat");
    key(&mut app, id, Key::Tab, Mods::NONE);
    type_in(&mut app, id, "bird");
    let w = app.window_mut::<EditorWindow>(id).unwrap();
    assert!(w.find_next(false));
    assert_eq!(w.buf.selection(), Some(0..3));
    assert_eq!(w.replace_all(), 3, "case-insensitive by default");
    assert_eq!(w.buf.text(), "bird dog bird\nbird\n");
    assert_eq!(w.bar.status, "3 replaced");
    key(&mut app, id, Key::Escape, Mods::NONE); // closes the bar, not the window
    assert!(!ed(&app, id).bar.visible && app.window_ids().contains(&id));
    key(&mut app, id, Key::Char('z'), primary()); // one undo step for all of them
    assert_eq!(text(&app, id), "cat dog cat\nCat\n");
    let w = app.window_mut::<EditorWindow>(id).unwrap();
    w.bar.match_case.checked = true;
    assert_eq!(w.replace_all(), 2);
}

#[test]
fn find_next_wraps_and_replace_one_by_one() {
    let e = env();
    let path = e.left.join("a.txt");
    fs::write(&path, "one two one\n").unwrap();
    let mut app = app(&e);
    open_editor(&mut app, oc_local::local(&path));
    let id = editor_for(&mut app, "a.txt");
    key(&mut app, id, Key::Char('f'), primary());
    type_in(&mut app, id, "one");
    key(&mut app, id, Key::Enter, Mods::NONE);
    assert_eq!(ed(&app, id).buf.selection(), Some(0..3));
    key(&mut app, id, Key::F(3), Mods::NONE);
    assert_eq!(ed(&app, id).buf.selection(), Some(8..11));
    key(&mut app, id, Key::Char('g'), primary());
    assert_eq!(ed(&app, id).buf.selection(), Some(0..3));
    assert_eq!(ed(&app, id).bar.status, "wrapped around");
    key(&mut app, id, Key::Char('g'), primary_shift());
    assert_eq!(ed(&app, id).buf.selection(), Some(8..11));
    // ⌥⌘F keeps the search text; Enter in the replacement field replaces one.
    key(&mut app, id, Key::Char('f'), primary_alt());
    assert_eq!(ed(&app, id).bar.find.text, "one");
    key(&mut app, id, Key::Tab, Mods::NONE);
    type_in(&mut app, id, "1");
    key(&mut app, id, Key::Enter, Mods::NONE);
    assert_eq!(text(&app, id), "one two 1\n");
    assert_eq!(ed(&app, id).buf.selection(), Some(0..3));
    key(&mut app, id, Key::Enter, Mods::NONE);
    assert_eq!(text(&app, id), "1 two 1\n");
    assert_eq!(ed(&app, id).bar.status, "not found");
}

#[test]
fn archives_open_read_only_and_binary_files_dont_open() {
    let e = env();
    write_zip(&e.left.join("pack.zip"), "inner.txt", b"inside");
    fs::write(e.left.join("image.bin"), b"\0\x01\x02binary").unwrap();
    oc_archive::register();
    let mut app = app(&e);
    let archive = oc_vfs::open_archive(&oc_local::local(e.left.join("pack.zip"))).unwrap().unwrap();
    open_editor(&mut app, archive.child(&["inner.txt"]));
    assert!(app.toast.as_ref().unwrap().text.contains("the file opens read-only"));
    let id = editor_for(&mut app, "inner.txt");
    assert!(ed(&app, id).read_only);
    assert_eq!(text(&app, id), "inside");
    assert!(ed(&app, id).status().ends_with("read-only"));
    type_in(&mut app, id, "typed");
    key(&mut app, id, Key::Char('v'), primary());
    assert_eq!(text(&app, id), "inside", "can't type in it");
    assert!(!ed(&app, id).modified());
    app.close_requested(id);
    assert!(app.modal_titles().is_empty() && editors(&app).is_empty(), "closes without asking");

    open_editor(&mut app, oc_local::local(e.left.join("image.bin")));
    let id = editor_for(&mut app, "image.bin");
    assert!(ed(&app, id).read_only && ed(&app, id).load_failed);
    assert!(text(&app, id).contains("isn't a text file"), "{}", text(&app, id));
}

#[test]
fn editing_on_another_file_system() {
    let e = env();
    let mem = oc_vfs::testing::MemFS::new();
    mem.add_file("remote.py", b"x = 1\r\n".to_vec());
    let root = mem.root();
    let mut app = app(&e);
    open_editor(&mut app, root.child(&["remote.py"]));
    let id = editor_for(&mut app, "remote.py");
    assert!(!ed(&app, id).read_only);
    assert_eq!(ed(&app, id).language, "Python");
    type_in(&mut app, id, "y = 2\n");
    key(&mut app, id, Key::Char('s'), primary());
    wait_saved(&mut app, id);
    let fs = root.fs.clone();
    let mut data = Vec::new();
    std::io::Read::read_to_end(&mut fs.open_read("remote.py").unwrap(), &mut data).unwrap();
    assert_eq!(data, b"y = 2\r\nx = 1\r\n", "saved through the file system, CRLF kept");
}

#[test]
fn reloads_when_changed_on_disk() {
    let e = env();
    let path = e.left.join("a.txt");
    fs::write(&path, "original\n").unwrap();
    let mut app = app(&e);
    app.watch = true;
    open_editor(&mut app, oc_local::local(&path));
    let id = editor_for(&mut app, "a.txt");
    // Saving is not an outside change.
    type_in(&mut app, id, "1");
    key(&mut app, id, Key::Char('s'), primary());
    wait_saved(&mut app, id);
    std::thread::sleep(Duration::from_millis(600));
    wait_until(&mut app, "quiet", Duration::from_secs(5), |app| app.modal_titles().is_empty() && app.tasks.pending() == 0);
    assert!(app.modal_titles().is_empty());
    fs::write(&path, "changed elsewhere\n").unwrap();
    wait_until(&mut app, "reload", Duration::from_secs(10), move |app| ed(app, id).buf.text() == "changed elsewhere\n");
    assert!(!ed(&app, id).modified());
    // With unsaved changes it asks first.
    type_in(&mut app, id, "mine ");
    fs::write(&path, "again\n").unwrap();
    wait_dialog(&mut app, "Changed on disk");
    key(&mut app, id, Key::Enter, Mods::NONE); // Reload
    wait_until(&mut app, "reload", Duration::from_secs(10), move |app| ed(app, id).buf.text() == "again\n");
}

#[test]
fn undo_redo_and_go_to_line() {
    let e = env();
    let path = e.left.join("a.txt");
    let lines: String = (1..=50).map(|n| format!("line {n}\n")).collect();
    fs::write(&path, &lines).unwrap();
    let mut app = app(&e);
    open_editor(&mut app, oc_local::local(&path));
    let id = editor_for(&mut app, "a.txt");
    let menus = app.window_menus(id);
    let undo = menus[1].items[0].clone().unwrap();
    assert_eq!((undo.label.as_str(), undo.enabled), ("Undo", false), "loading the file is not an edit");
    type_in(&mut app, id, "new ");
    assert!(app.window_menus(id)[1].items[0].as_ref().unwrap().enabled);
    key(&mut app, id, Key::Char('z'), primary());
    assert_eq!(text(&app, id), lines);
    assert!(!ed(&app, id).modified());
    app.handle_event(id, Event::Focus(true));
    app.menu_activated("editor.redo"); // the Edit menu's Redo = its key
    assert_eq!(text(&app, id), format!("new {lines}"));
    if !cfg!(target_os = "macos") {
        key(&mut app, id, Key::Char('z'), primary());
        key(&mut app, id, Key::Char('y'), primary()); // Ctrl+Y
        assert_eq!(text(&app, id), format!("new {lines}"));
    }
    key(&mut app, id, Key::Char('l'), primary_alt()); // ⌥⌘L as well as ⌘L
    assert_eq!(app.modal_titles(), ["Go to line"]);
    type_in(&mut app, id, "42");
    key(&mut app, id, Key::Enter, Mods::NONE);
    let w = ed(&app, id);
    assert_eq!(w.buf.line(w.buf.line_of(w.buf.cursor)).to_string(), "line 42");
    assert!(w.top > 0 && w.top <= 41, "scrolled to it");
    key(&mut app, id, Key::Char('l'), primary());
    assert_eq!(app.modal_titles(), ["Go to line"]);
}

#[test]
fn enter_keeps_the_indentation() {
    let e = env();
    let path = e.left.join("a.py");
    fs::write(&path, "def f():\n    x = 1").unwrap();
    let mut app = app(&e);
    open_editor(&mut app, oc_local::local(&path));
    let id = editor_for(&mut app, "a.py");
    key(&mut app, id, Key::Down, primary_mods_doc_end());
    type_in(&mut app, id, "\ny");
    assert_eq!(text(&app, id), "def f():\n    x = 1\n    y");
}

#[test]
fn characters_the_encoding_cant_store_offer_utf8() {
    let e = env();
    let path = e.left.join("old.txt");
    fs::write(&path, b"caf\xe9\n").unwrap();
    let mut app = app(&e);
    open_editor(&mut app, oc_local::local(&path));
    let id = editor_for(&mut app, "old.txt");
    assert!(ed(&app, id).status().contains("Windows-1252"));
    app.handle_event(id, Event::Commit("日".into()));
    key(&mut app, id, Key::Char('s'), primary());
    assert_eq!(app.modal_titles(), ["Save as UTF-8"]);
    assert!(app.top_modal_text().unwrap().contains("Windows-1252"));
    key(&mut app, id, Key::Enter, Mods::NONE);
    wait_saved(&mut app, id);
    assert_eq!(fs::read_to_string(&path).unwrap(), "日café\n");
    assert!(ed(&app, id).status().contains("UTF-8"));
}

#[test]
fn large_files_ask_first() {
    let e = env();
    let f = fs::File::create(e.left.join("big.log")).unwrap();
    f.set_len(21 * 1024 * 1024).unwrap();
    let mut app = app(&e);
    select(&mut app, "big.log");
    press(&mut app, Key::F(4));
    assert_eq!(app.modal_titles(), ["Large file"]);
    assert!(app.top_modal_text().unwrap().contains("Open it in the editor anyway?"));
    press(&mut app, Key::Escape);
    assert!(editors(&app).is_empty());
}

#[test]
fn highlighting_runs_in_the_background_and_follows_edits() {
    let e = env();
    let path = e.left.join("app.py");
    let src: String = (0..3000).map(|i| format!("def f{i}(x):\n    return x  # {i}\n")).collect();
    fs::write(&path, &src).unwrap();
    let mut app = app(&e);
    open_editor(&mut app, oc_local::local(&path));
    let id = editor_for(&mut app, "app.py");
    wait_until(&mut app, "highlighted", Duration::from_secs(30), move |app| ed(app, id).hl.done());
    assert!(!ed(&app, id).hl.spans(0).is_empty());
    assert!(!ed(&app, id).hl.spans(5999).is_empty(), "the whole file");
    // Opening a string recolors what follows; closing it again restores.
    type_in(&mut app, id, "s = \"\"\"\n");
    wait_until(&mut app, "rehighlighted", Duration::from_secs(30), move |app| ed(app, id).hl.done());
    let string_role = ed(&app, id).hl.spans(3000)[0].role;
    assert_eq!(string_role, oc_preview::data::SyntaxRole::String);
    key(&mut app, id, Key::Char('z'), primary()); // the line break
    key(&mut app, id, Key::Char('z'), primary()); // the typing
    wait_until(&mut app, "rehighlighted", Duration::from_secs(30), move |app| ed(app, id).hl.done());
    assert_ne!(ed(&app, id).hl.spans(3000)[0].role, oc_preview::data::SyntaxRole::String);
}

#[test]
fn drawing_the_window() {
    let e = env();
    let path = e.left.join("app.py");
    fs::write(&path, "import os\n\nprint(os.name)\n").unwrap();
    let mut app = app(&e);
    open_editor(&mut app, oc_local::local(&path));
    let id = editor_for(&mut app, "app.py");
    app.resize(id, 60, 12);
    key(&mut app, id, Key::Down, Mods::NONE);
    key(&mut app, id, Key::Down, Mods::NONE);
    key(&mut app, id, Key::Right, Mods::NONE);
    let mut f = oc_grid::Frame::new(60, 12, app.theme.colors.background);
    app.draw(id, &mut f, (8.0, 16.0));
    assert!(f.row_text(0).contains("app.py"), "{}", f.row_text(0));
    assert!(f.row_text(1).contains("  1 import os"), "{:?}", f.row_text(1));
    assert!(f.row_text(3).contains("  3 print(os.name)"), "{:?}", f.row_text(3));
    assert!(f.row_text(11).contains("Ln 3, Col 2 · UTF-8 · LF · Python"), "{:?}", f.row_text(11));
    let cursor = f.cursor.expect("text cursor");
    assert_eq!((cursor.col, cursor.row), (1 + 5 + 1, 3));
    // The find bar takes the last row inside the frame.
    key(&mut app, id, Key::Char('f'), primary());
    app.draw(id, &mut f, (8.0, 16.0));
    assert!(f.row_text(10).contains("Match case"), "{:?}", f.row_text(10));
    // Unsaved changes: the ● stays in front of a long path cut at its start.
    key(&mut app, id, Key::Escape, Mods::NONE);
    type_in(&mut app, id, "x");
    app.resize(id, 20, 12);
    let mut f = oc_grid::Frame::new(20, 12, app.theme.colors.background);
    app.draw(id, &mut f, (8.0, 16.0));
    assert!(f.row_text(0).contains(" ● …"), "{:?}", f.row_text(0));
    assert!(f.row_text(0).contains("app.py"), "{:?}", f.row_text(0));
}

#[test]
fn title_shortens_the_home_folder() {
    let home = oc_local::home_dir();
    let loc = oc_local::local(home.join("src").join("a.txt"));
    let shown = oc_ui::features::editor::window::short_display(&loc);
    assert_eq!(shown, format!("~{}", Path::new("/src/a.txt").display()).replace('/', std::path::MAIN_SEPARATOR_STR));
}

/// A zip with one stored (uncompressed) file, so the test needs no zip crate.
fn write_zip(path: &Path, name: &str, body: &[u8]) {
    let mut crc = 0xffff_ffffu32;
    for &b in body {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    let crc = !crc;
    let (n, len) = (name.len() as u16, body.len() as u32);
    let mut out = Vec::new();
    let mut local = Vec::new();
    local.extend(0x04034b50u32.to_le_bytes());
    for v in [20u16, 0, 0, 0, 0x21] {
        local.extend(v.to_le_bytes());
    }
    for v in [crc, len, len] {
        local.extend(v.to_le_bytes());
    }
    local.extend(n.to_le_bytes());
    local.extend(0u16.to_le_bytes());
    local.extend(name.as_bytes());
    out.extend(&local);
    out.extend(body);
    let cd_off = out.len() as u32;
    let mut central = Vec::new();
    central.extend(0x02014b50u32.to_le_bytes());
    for v in [20u16, 20, 0, 0, 0, 0x21] {
        central.extend(v.to_le_bytes());
    }
    for v in [crc, len, len] {
        central.extend(v.to_le_bytes());
    }
    for v in [n, 0, 0, 0, 0] {
        central.extend(v.to_le_bytes());
    }
    central.extend(0u32.to_le_bytes());
    central.extend(0u32.to_le_bytes());
    central.extend(name.as_bytes());
    out.extend(&central);
    out.extend(0x06054b50u32.to_le_bytes());
    out.extend([0u8; 4]);
    out.extend(1u16.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend((central.len() as u32).to_le_bytes());
    out.extend(cd_off.to_le_bytes());
    out.extend([0u8; 2]);
    fs::write(path, out).unwrap();
}
