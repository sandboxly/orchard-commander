//! Multi-rename (⌘M) end to end without a window: the dialog's fields and
//! preview, metadata read in the background, the batch on a worker, panel
//! refresh, ⌘Z undo, and search results following renamed files (ports of
//! the multi-rename tests in 1.x's `tests/test_ui.py`).

use std::collections::HashMap;
use std::fs;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use oc_search::{Query, SearchResultsFS};
use oc_ui::features::rename::{self, Field};
use oc_ui::panel::{self, LoadOptions};
use oc_ui::testing::{headless_app, press, press_with, settle, type_text, wait_until};
use oc_ui::{App, AppConfig, Key, MAIN_WINDOW, Mods, Registry};
use oc_vfs::VPath;

struct Env {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    left: PathBuf,
    right: PathBuf,
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

fn start(app: &mut App) {
    app.resize(MAIN_WINDOW, 140, 42);
    settle(app);
}

fn app(e: &Env) -> App {
    let mut app = headless_app(&e.config, &e.left, &e.right);
    start(&mut app);
    app
}

fn names(app: &App, side: usize) -> Vec<String> {
    app.main.sides[side].panel.entries.iter().map(|e| e.name.replace(MAIN_SEPARATOR, "/")).collect()
}

#[test]
fn files_menu_keeps_the_1x_order() {
    // 1.x main_window.py: Multi-rename and Undo rename sit between Move / rename
    // and Make directory, in one group with the other file operations.
    let e = env();
    let app = app(&e);
    let labels: Vec<&str> = oc_ui::menus::menu_items(&app, oc_ui::menus::Menu::Files)
        .into_iter()
        .map(|id| id.map_or("-", |id| app.registry.get(id).unwrap().label_in_menu()))
        .collect();
    assert_eq!(
        labels,
        [
            "Select all", "Clear selection", "-", "Copy", "Move / rename", "Multi-rename", "Undo rename",
            "Make directory", "Move to trash", "-", "Find files", "Last search results", "-", "Quit",
        ]
    );
}

fn mark(app: &mut App, side: usize, names: &[&str]) {
    let p = &mut app.main.sides[side].panel;
    for n in names {
        p.marked.insert((*n).to_owned());
    }
}

fn put_cursor_on(app: &mut App, side: usize, name: &str) {
    let p = &mut app.main.sides[side].panel;
    let row = p.row_of(name).unwrap_or_else(|| panic!("{name} not listed"));
    p.set_cursor(row);
}

/// ⌘M and wait for the dialog.
fn open(app: &mut App) {
    press_with(app, Key::Char('m'), Mods::primary());
    wait_until(app, "the Multi-rename dialog", Duration::from_secs(10), |app| rename::dialog(app).is_some());
    assert!(app.modal_titles().last().unwrap().starts_with("Multi-rename"));
}

fn rows(app: &App) -> Vec<(String, String)> {
    let d = rename::dialog(app).unwrap();
    (0..d.items.len()).map(|i| d.row(i)).collect()
}

fn new_names(app: &App) -> Vec<String> {
    rows(app).into_iter().map(|r| r.0).collect()
}

fn summary(app: &App) -> String {
    rename::dialog(app).unwrap().summary()
}

fn wait_idle(app: &mut App) {
    wait_until(app, "the batch", Duration::from_secs(20), |app| {
        app.modal_titles().is_empty() && !app.try_state::<rename::RenameState>().is_some_and(|s| s.running)
    });
    settle(app);
}

fn toast(app: &App) -> String {
    app.toast.as_ref().map(|t| t.text.clone()).unwrap_or_default()
}

#[test]
fn counter_rename_and_undo() {
    let e = env();
    for n in ["a.txt", "b.bin", "c.txt"] {
        fs::write(e.left.join(n), n).unwrap();
    }
    let mut app = app(&e);
    mark(&mut app, 0, &["a.txt", "b.bin"]);
    open(&mut app);
    assert_eq!(app.modal_titles(), ["Multi-rename 2 items"]);
    assert_eq!(rows(&app), [("a.txt".into(), "unchanged".into()), ("b.bin".into(), "unchanged".into())]);
    assert!(!rename::dialog(&app).unwrap().can_rename(), "nothing to do yet");
    // The default [N] is selected: typing replaces it.
    type_text(&mut app, "file_[C:2]");
    assert_eq!(new_names(&app), ["file_01.txt", "file_02.bin"]);
    assert_eq!(summary(&app), "2 of 2 will be renamed");
    press(&mut app, Key::Enter);
    wait_idle(&mut app);
    assert!(e.left.join("file_01.txt").exists() && e.left.join("file_02.bin").exists());
    assert!(!e.left.join("a.txt").exists());
    assert_eq!(names(&app, 0), ["c.txt", "file_01.txt", "file_02.bin"]);
    assert!(app.main.sides[0].panel.marked.is_empty(), "marks are cleared");
    let undo_hint = if cfg!(target_os = "macos") { "⌘Z" } else { "Ctrl+Z" };
    assert_eq!(toast(&app), format!("Renamed 2 items · {undo_hint} to undo"));

    // ⌘Z asks, then gives the old names back.
    press_with(&mut app, Key::Char('z'), Mods::primary());
    assert_eq!(app.modal_titles(), ["Undo rename"]);
    assert_eq!(app.top_modal_text().unwrap(), "Give 2 items their old names back?");
    press(&mut app, Key::Enter);
    wait_idle(&mut app);
    let mut on_disk: Vec<String> = fs::read_dir(&e.left).unwrap().map(|d| d.unwrap().file_name().to_string_lossy().into()).collect();
    on_disk.sort();
    assert_eq!(on_disk, ["a.txt", "b.bin", "c.txt"]);
    assert_eq!(names(&app, 0), ["a.txt", "b.bin", "c.txt"]);
    assert_eq!(toast(&app), "Undid renaming 2 items");
    // Only the last batch can be undone.
    press_with(&mut app, Key::Char('z'), Mods::primary());
    assert!(app.modal_titles().is_empty());
    assert_eq!(toast(&app), "Nothing to undo");
}

#[test]
fn problems_block_rename_and_the_rule_is_remembered() {
    let e = env();
    for n in ["a.txt", "b.bin"] {
        fs::write(e.left.join(n), n).unwrap();
    }
    let mut app = app(&e);
    mark(&mut app, 0, &["a.txt", "b.bin"]);
    open(&mut app);
    type_text(&mut app, "same");
    press(&mut app, Key::Tab);
    assert_eq!(rename::dialog(&app).unwrap().focus, Field::Ext);
    press_with(&mut app, Key::Char('a'), Mods::primary());
    type_text(&mut app, "x");
    let d = rename::dialog(&app).unwrap();
    assert!(!d.can_rename() && !d.buttons.buttons[0].enabled);
    assert!(rows(&app)[0].1.starts_with("same new name"), "{:?}", rows(&app));
    assert!(summary(&app).contains("2 problems to fix before renaming"), "{}", summary(&app));
    // A broken mask: the summary says what's wrong.
    press(&mut app, Key::Tab);
    press_with(&mut app, Key::Tab, Mods::SHIFT);
    press_with(&mut app, Key::Tab, Mods::SHIFT);
    assert_eq!(rename::dialog(&app).unwrap().focus, Field::Name);
    press_with(&mut app, Key::Char('a'), Mods::primary());
    type_text(&mut app, "[N");
    assert!(summary(&app).contains("never closed"), "{}", summary(&app));
    assert!(new_names(&app).iter().all(String::is_empty));
    // Enter does nothing while there are problems; Escape cancels.
    press(&mut app, Key::Enter);
    assert!(rename::dialog(&app).is_some());
    press(&mut app, Key::Escape);
    assert!(app.modal_titles().is_empty() && rename::dialog(&app).is_none());
    settle(&mut app);
    assert!(e.left.join("a.txt").exists() && e.left.join("b.bin").exists());
    // The settings stay for the next time (this session).
    open(&mut app);
    let d = rename::dialog(&app).unwrap();
    assert_eq!((d.name_mask.text.as_str(), d.ext_mask.text.as_str()), ("[N", "x"));
    press(&mut app, Key::Escape);
}

#[test]
fn search_replace_case_and_counter_fields() {
    let e = env();
    fs::write(e.left.join("My_Holiday photo.JPG"), "").unwrap();
    let mut app = app(&e);
    put_cursor_on(&mut app, 0, "My_Holiday photo.JPG");
    open(&mut app);
    assert_eq!(app.modal_titles(), ["Multi-rename 1 item"], "the cursor's item when nothing is marked");
    press(&mut app, Key::Tab); // extension
    press(&mut app, Key::Tab); // search
    type_text(&mut app, "_| ");
    press(&mut app, Key::Tab); // replace
    type_text(&mut app, "-");
    assert_eq!(new_names(&app), ["My-Holiday-photo.JPG"]);
    // RegEx with groups.
    rename::with_open_dialog(&mut app, |d, app| {
        d.search.set_text(r"(\w+)_(\w+)");
        d.replace.set_text("$2 $1");
        d.update(app);
    });
    assert_eq!(new_names(&app), ["My_Holiday photo.JPG"], "without RegEx, searched literally");
    press(&mut app, Key::Tab); // RegEx
    press(&mut app, Key::Space);
    assert_eq!(new_names(&app), ["Holiday My photo.JPG"]);
    // Case: ↓ goes through the choices.
    press(&mut app, Key::Tab); // case sensitive
    press(&mut app, Key::Tab); // case
    assert_eq!(rename::dialog(&app).unwrap().focus, Field::Case);
    press(&mut app, Key::Down);
    assert_eq!(new_names(&app), ["holiday my photo.jpg"]);
    // Space opens the list; Enter picks.
    press(&mut app, Key::Space);
    press(&mut app, Key::Down);
    press(&mut app, Key::Down);
    press(&mut app, Key::Enter);
    assert_eq!(new_names(&app), ["Holiday my photo.jpg"], "First letter uppercase");
    // Home / End / F4 as in a Qt combo box.
    press(&mut app, Key::Home);
    assert_eq!(new_names(&app), ["Holiday My photo.JPG"], "Unchanged");
    press(&mut app, Key::End);
    press(&mut app, Key::F(4));
    press(&mut app, Key::Up);
    press(&mut app, Key::Enter);
    assert_eq!(new_names(&app), ["Holiday my photo.jpg"], "one above Every Word Uppercase");
    // Counter fields: typed and stepped with ↑/↓, used by [C].
    rename::with_open_dialog(&mut app, |d, app| {
        d.name_mask.set_text("[C]");
        d.search.set_text("");
        d.case = 0;
        d.update(app);
    });
    press(&mut app, Key::Tab); // start
    press_with(&mut app, Key::Char('a'), Mods::primary());
    type_text(&mut app, "7x");
    assert_eq!(new_names(&app), ["7.JPG"], "letters are not typed into a number field");
    press(&mut app, Key::Up);
    press(&mut app, Key::Tab); // step
    press(&mut app, Key::Tab); // digits
    press(&mut app, Key::Up);
    press(&mut app, Key::Up);
    assert_eq!(new_names(&app), ["008.JPG"]);
    press(&mut app, Key::Escape);
}

#[test]
fn insert_menu_puts_placeholders_at_the_cursor() {
    let e = env();
    fs::write(e.left.join("report.pdf"), "").unwrap();
    let mut app = app(&e);
    put_cursor_on(&mut app, 0, "report.pdf");
    open(&mut app);
    // Alt+↓ opens the Insert… menu; the selected default [N] is replaced.
    press_with(&mut app, Key::Down, Mods::ALT);
    press(&mut app, Key::Down); // [N1-3]
    press(&mut app, Key::Enter);
    assert_eq!(rename::dialog(&app).unwrap().name_mask.text, "[N1-3]");
    assert_eq!(new_names(&app), ["rep.pdf"]);
    // The metadata fields are in one submenu per kind of file.
    press_with(&mut app, Key::Down, Mods::ALT);
    press(&mut app, Key::End); // the fallback example
    press(&mut app, Key::Up);
    press(&mut app, Key::Up);
    press(&mut app, Key::Up); // the first provider (audio tags)
    press(&mut app, Key::Right);
    press(&mut app, Key::Enter); // [=artist]
    assert_eq!(rename::dialog(&app).unwrap().name_mask.text, "[N1-3][=artist]");
    let labels: Vec<String> = oc_rename::metadata::registry().providers().iter().map(|p| p.label().to_owned()).collect();
    assert!(labels.contains(&"Audio tags".to_owned()) && labels.contains(&"Photo (EXIF)".to_owned()), "{labels:?}");
    // Escape closes the menu first, then the dialog.
    press_with(&mut app, Key::Down, Mods::ALT);
    press(&mut app, Key::Escape);
    assert!(rename::dialog(&app).is_some());
    press(&mut app, Key::Escape);
    assert!(rename::dialog(&app).is_none());
    settle(&mut app);
}

#[test]
fn metadata_is_read_in_the_background() {
    let e = env();
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("../oc-rename/tests/data/clip-160x90.mp4");
    fs::copy(&data, e.left.join("a.mp4")).unwrap();
    fs::write(e.left.join("b.mp4"), "not a video").unwrap();
    let mut app = app(&e);
    mark(&mut app, 0, &["a.mp4", "b.mp4"]);
    open(&mut app);
    type_text(&mut app, "[=title] ([=quality])");
    let s = summary(&app);
    assert!(s.contains("reading metadata") || rename::dialog(&app).unwrap().plan.pending() == 0, "{s}");
    wait_until(&mut app, "metadata", Duration::from_secs(10), |app| rename::dialog(app).unwrap().plan.pending() == 0);
    assert_eq!(new_names(&app)[0], "Test Clip (90p).mp4");
    assert!(rows(&app)[1].1.starts_with("skipped: no"), "{:?}", rows(&app));
    assert!(rename::dialog(&app).unwrap().can_rename());
    assert!(summary(&app).contains("1 skipped"), "{}", summary(&app));
    press(&mut app, Key::Enter);
    wait_idle(&mut app);
    assert!(e.left.join("Test Clip (90p).mp4").exists() && e.left.join("b.mp4").exists());
    assert_eq!(rename::dialog(&app).map(|d| d.id), None);
}

#[test]
fn cursor_follows_the_renamed_item() {
    let e = env();
    for n in ["a.txt", "m.txt"] {
        fs::write(e.left.join(n), "").unwrap();
    }
    let mut app = app(&e);
    put_cursor_on(&mut app, 0, "m.txt");
    open(&mut app);
    type_text(&mut app, "0-first");
    press(&mut app, Key::Enter);
    wait_idle(&mut app);
    assert_eq!(names(&app, 0), ["0-first.txt", "a.txt"]);
    assert_eq!(app.main.sides[0].panel.cursor_name(), Some("0-first.txt"));
}

#[test]
fn a_panel_inside_a_renamed_folder_follows_it() {
    let e = env();
    fs::create_dir_all(e.left.join("old/sub")).unwrap();
    let mut app = app(&e);
    panel::load(&mut app, 1, oc_local::local(e.left.join("old/sub")), LoadOptions::default());
    settle(&mut app);
    put_cursor_on(&mut app, 0, "old");
    open(&mut app);
    type_text(&mut app, "new");
    press(&mut app, Key::Enter);
    wait_idle(&mut app);
    assert_eq!(app.main.sides[1].panel.location, oc_local::local(e.left.join("new/sub")));
    assert_eq!(app.main.sides[0].panel.cursor_name(), Some("new"));
}

// -- search results follow renamed files (through the files-moved hook) ----------------

static RESULTS: Mutex<Option<Arc<SearchResultsFS>>> = Mutex::new(None);
static MOVED: Mutex<Vec<HashMap<VPath, VPath>>> = Mutex::new(Vec::new());

/// What the search feature does: its result lists follow renamed files.
fn search_stub(reg: &mut Registry) {
    reg.on_files_moved(|_, moved| {
        if let Some(results) = RESULTS.lock().unwrap().as_ref() {
            results.relocate(moved);
        }
        MOVED.lock().unwrap().push(moved.clone());
    });
}

#[test]
fn search_hits_are_renamed_where_they_are() {
    let e = env();
    let root = e.left.join("src");
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir_all(root.join("sub")).unwrap();
    for f in ["a.txt", "docs/one.txt", "sub/c.txt", "keep.md"] {
        fs::write(root.join(f), f).unwrap();
    }
    let features: &'static [fn(&mut Registry)] =
        Box::leak(oc_ui::features::ALL.iter().copied().chain([search_stub as fn(&mut Registry)]).collect::<Vec<_>>().into_boxed_slice());
    let mut cfg = AppConfig::new(oc_config::ConfigPaths::new(&e.config), Arc::new(|| {}));
    cfg.left = Some(e.left.clone());
    cfg.right = Some(e.right.clone());
    cfg.clipboard = Box::new(oc_ui::clipboard::MemoryClipboard::default());
    cfg.watch = false;
    cfg.features = features;
    let mut app = App::new(cfg);
    start(&mut app);

    let results = Arc::new(SearchResultsFS::new(Query { names: "*.txt".into(), ..Query::new(vec![oc_local::local(&root)]) }));
    results.run(|| false).unwrap();
    *RESULTS.lock().unwrap() = Some(results.clone());
    panel::load(&mut app, 0, VPath::new(results.clone(), ""), LoadOptions::default());
    settle(&mut app);
    assert_eq!(names(&app, 0), ["a.txt", "docs/one.txt", "sub/c.txt"]);
    press_with(&mut app, Key::Char('a'), Mods::primary()); // mark all
    open(&mut app);
    assert_eq!(app.modal_titles(), ["Multi-rename 3 items"]);
    press(&mut app, Key::Tab);
    press(&mut app, Key::Tab);
    type_text(&mut app, ".txt");
    press(&mut app, Key::Tab);
    type_text(&mut app, ".log");
    assert_eq!(new_names(&app), ["a.log", "one.log", "c.log"]);
    press(&mut app, Key::Enter);
    wait_idle(&mut app);
    assert!(root.join("a.log").exists() && root.join("docs/one.log").exists() && root.join("sub/c.log").exists());
    assert_eq!(MOVED.lock().unwrap().last().map(HashMap::len), Some(3));
    assert_eq!(names(&app, 0), ["a.log", "docs/one.log", "sub/c.log"], "the results follow the renamed files");
    assert!(results.hits().iter().all(|h| h.location.exists()));
    *RESULTS.lock().unwrap() = None;
}

#[test]
fn big_batches_update_the_preview_after_a_pause() {
    let e = env();
    for i in 0..2100 {
        fs::write(e.left.join(format!("f{i:04}.txt")), "").unwrap();
    }
    let mut app = app(&e);
    press_with(&mut app, Key::Char('a'), Mods::primary());
    open(&mut app);
    type_text(&mut app, "g[C:4]");
    assert!(!rename::dialog(&app).unwrap().can_rename(), "the preview waits for a pause in typing");
    assert_eq!(new_names(&app)[0], "f0000.txt");
    wait_until(&mut app, "the preview", Duration::from_secs(5), |app| rename::dialog(app).unwrap().can_rename());
    assert_eq!(new_names(&app)[..2], ["g0001.txt", "g0002.txt"]);
    // Enter right after typing renames with what was typed.
    type_text(&mut app, "-");
    press(&mut app, Key::Enter);
    wait_idle(&mut app);
    assert!(e.left.join("g2100-.txt").exists());
    let listed = names(&app, 0);
    assert_eq!(listed.len(), 2100);
    assert!(listed.iter().all(|n| n.starts_with('g')));
}

#[test]
fn a_panel_showing_a_renamed_folder_keeps_its_cursor() {
    let e = env();
    fs::create_dir_all(e.left.join("old")).unwrap();
    for n in ["a.txt", "b.txt", "c.txt"] {
        fs::write(e.left.join("old").join(n), "").unwrap();
    }
    let mut app = app(&e);
    panel::load(&mut app, 1, oc_local::local(e.left.join("old")), LoadOptions::default());
    settle(&mut app);
    put_cursor_on(&mut app, 1, "b.txt");
    put_cursor_on(&mut app, 0, "old");
    open(&mut app);
    type_text(&mut app, "new");
    press(&mut app, Key::Enter);
    wait_idle(&mut app);
    assert_eq!(app.main.sides[1].panel.location, oc_local::local(e.left.join("new")));
    assert_eq!(app.main.sides[1].panel.cursor_name(), Some("b.txt"));
}

#[test]
fn tab_selects_the_next_field_s_text() {
    let e = env();
    fs::write(e.left.join("a.txt"), "").unwrap();
    let mut app = app(&e);
    put_cursor_on(&mut app, 0, "a.txt");
    open(&mut app);
    press(&mut app, Key::Tab); // extension: [E] selected, like Qt
    type_text(&mut app, "md");
    assert_eq!(new_names(&app), ["a.md"]);
    press(&mut app, Key::Escape);
}

#[test]
fn the_dialog_draws_in_any_window_size() {
    let e = env();
    for n in ["a.txt", "b.txt"] {
        fs::write(e.left.join(n), "").unwrap();
    }
    let mut app = app(&e);
    mark(&mut app, 0, &["a.txt", "b.txt"]);
    open(&mut app);
    for (cols, rows) in [(140, 42), (80, 24), (40, 12), (12, 4), (3, 2), (1, 1)] {
        app.resize(MAIN_WINDOW, cols, rows);
        for keys in [&[][..], &[(Key::Down, Mods::ALT)][..], &[(Key::End, Mods::NONE), (Key::Up, Mods::NONE), (Key::Right, Mods::NONE)][..]] {
            for &(k, m) in keys {
                press_with(&mut app, k, m);
            }
            let mut f = oc_grid::Frame::new(cols, rows, oc_grid::Rgba::BLACK);
            app.draw(MAIN_WINDOW, &mut f, (8.0, 16.0));
        }
        press(&mut app, Key::Escape);
        press(&mut app, Key::Escape);
        assert!(rename::dialog(&app).is_some(), "Escape closes the menus first");
        // A click anywhere must not panic either.
        for (c, r) in [(0, 0), (cols / 2, rows / 2), (cols.saturating_sub(1), rows.saturating_sub(1))] {
            app.handle_event(MAIN_WINDOW, oc_ui::Event::Mouse(oc_ui::MouseEvent { kind: oc_ui::MouseKind::Down(oc_ui::MouseButton::Left), col: c, row: r, mods: Mods::NONE }));
        }
        while rename::dialog(&app).is_some() {
            press(&mut app, Key::Escape);
        }
        app.resize(MAIN_WINDOW, 140, 42);
        open(&mut app);
    }
    press(&mut app, Key::Escape);
}
