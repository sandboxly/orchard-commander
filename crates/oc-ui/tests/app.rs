//! The main window driven without a window: navigation, commands, dialogs
//! and file operations end to end (workers, ask_ui, progress, reloads).

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::Duration;

use oc_ui::testing::{headless_app, press, press_with, settle, type_text, wait_until};
use oc_ui::{App, Event, Key, KeyEvent, MAIN_WINDOW, Mods};

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

fn wait_dialog(app: &mut App, title: &str) {
    let t = title.to_owned();
    wait_until(app, &format!("dialog {title}"), Duration::from_secs(10), move |app| app.modal_titles().last().is_some_and(|x| *x == t));
}

fn wait_no_dialogs(app: &mut App) {
    wait_until(app, "dialogs to close", Duration::from_secs(20), |app| app.modal_titles().is_empty() && app.operation.is_none());
    settle(app);
}

#[test]
fn listing_dirs_first_and_navigation_keeps_cursor() {
    let e = env();
    fs::create_dir(e.left.join("sub")).unwrap();
    fs::write(e.left.join("sub/inner.txt"), "x").unwrap();
    fs::write(e.left.join("a.txt"), "a").unwrap();
    let mut app = app(&e);
    assert_eq!(names(&app, 0), ["sub", "a.txt"]);
    assert_eq!(cursor(&app, 0).as_deref(), Some(".."));
    press(&mut app, Key::Down);
    assert_eq!(cursor(&app, 0).as_deref(), Some("sub"));
    press(&mut app, Key::Enter);
    settle(&mut app);
    assert_eq!(names(&app, 0), ["inner.txt"]);
    assert_eq!(cursor(&app, 0).as_deref(), Some(".."));
    press(&mut app, Key::Backspace);
    settle(&mut app);
    assert_eq!(cursor(&app, 0).as_deref(), Some("sub"), "back on the folder we came from");
    // Tab switches sides; ⌘E shows the active folder in the other panel too.
    press(&mut app, Key::Tab);
    assert_eq!(app.main.active, 1);
    press(&mut app, Key::Tab);
    press_with(&mut app, Key::Char('e'), Mods::primary());
    settle(&mut app);
    assert_eq!(app.main.sides[1].panel.location, app.main.sides[0].panel.location);
    // The folder is remembered for the next start.
    app.flush_settings();
    let saved = oc_config::Settings::load(&app.paths.settings_file());
    assert_eq!(saved.left_path, e.left.to_string_lossy());
    assert_eq!(saved.right_path, e.left.to_string_lossy());
}

#[test]
fn swap_reload_and_hidden_files() {
    let e = env();
    fs::write(e.left.join(".hidden"), "h").unwrap();
    fs::write(e.right.join("r.txt"), "r").unwrap();
    let mut app = app(&e);
    assert_eq!(names(&app, 0), [".hidden"], "hidden files show by default");
    press_with(&mut app, Key::Char('u'), Mods::primary());
    settle(&mut app);
    assert_eq!(names(&app, 0), ["r.txt"]);
    assert_eq!(names(&app, 1), [".hidden"]);
    // ⇧⌘. hides them and persists the choice.
    press_with(&mut app, Key::Char('.'), Mods::primary().union(Mods::SHIFT));
    settle(&mut app);
    assert!(names(&app, 1).is_empty());
    app.flush_settings();
    assert_eq!(oc_config::Settings::load(&app.paths.settings_file()).show_hidden, Some(false));
    // A file created outside shows after ⌘R, with the cursor kept.
    put_cursor_on(&mut app, 0, "r.txt");
    fs::write(e.right.join("0-first.txt"), "").unwrap();
    press_with(&mut app, Key::Char('r'), Mods::primary());
    settle(&mut app);
    assert_eq!(names(&app, 0), ["0-first.txt", "r.txt"]);
    assert_eq!(cursor(&app, 0).as_deref(), Some("r.txt"));
}

#[test]
fn zoom_and_sort_persist() {
    let e = env();
    let mut app = app(&e);
    let size = app.theme.font.size;
    press_with(&mut app, Key::Char('='), Mods::primary());
    assert_eq!(app.theme.font.size, size + 1.0);
    assert!(app.take_requests().contains(&oc_ui::PlatformRequest::ThemeChanged));
    press_with(&mut app, Key::Char('0'), Mods::primary());
    assert_eq!(app.theme.font.size, size);
    oc_ui::panel::sort_by(&mut app, 0, oc_vfs::SortKey::Size);
    oc_ui::panel::sort_by(&mut app, 0, oc_vfs::SortKey::Size);
    settle(&mut app);
    app.flush_settings();
    let s = oc_config::Settings::load(&app.paths.settings_file());
    assert_eq!(s.font_size, None);
    assert_eq!(s.extra.get("left_sort").and_then(|v| v.as_str()), Some("size"));
    assert_eq!(s.extra.get("left_sort_reverse").and_then(|v| v.as_bool()), Some(true));
}

#[test]
fn command_palette_filters_and_runs() {
    let e = env();
    fs::write(e.right.join("r.txt"), "r").unwrap();
    let mut app = app(&e);
    press_with(&mut app, Key::Char('p'), Mods::primary());
    assert_eq!(app.modal_titles(), ["Commands"]);
    type_text(&mut app, "swap");
    assert_eq!(app.top_modal_text().unwrap().lines().next(), Some("Swap panels"));
    press(&mut app, Key::Enter);
    assert!(app.modal_titles().is_empty());
    settle(&mut app);
    assert_eq!(names(&app, 0), ["r.txt"]);
    // Escape closes without running anything.
    press_with(&mut app, Key::Char('p'), Mods::primary());
    press(&mut app, Key::Escape);
    assert!(app.modal_titles().is_empty());
}

#[test]
fn menu_strip_runs_commands() {
    let e = env();
    let mut app = app(&e);
    press(&mut app, Key::F(9));
    assert_eq!(app.modal_titles(), ["menu"]);
    press(&mut app, Key::Right); // Files menu
    press(&mut app, Key::Escape);
    assert!(app.modal_titles().is_empty());
    // Options → "Toggle hidden files" by its position: open, then Enter on the first item.
    press(&mut app, Key::F(9));
    press(&mut app, Key::Right);
    press(&mut app, Key::Right);
    press(&mut app, Key::Right); // Options
    press(&mut app, Key::Enter); // first item: Choose color scheme…
    assert_eq!(app.modal_titles(), ["Color scheme"]);
    press(&mut app, Key::Escape);
}

#[test]
fn every_shortcut_belongs_to_one_command() {
    let reg = oc_ui::Registry::build(oc_ui::features::ALL);
    let mut seen = HashSet::new();
    for c in reg.commands() {
        for s in &c.shortcuts {
            assert!(seen.insert(*s), "{} is bound twice (again by {})", s.display(), c.id);
        }
    }
    for id in ["files.copy", "files.move", "files.mkdir", "files.trash", "app.quit", "app.palette", "preview.toggle"] {
        assert!(reg.get(id).is_some(), "{id}");
    }
    let fkeys: Vec<&str> = reg.fkeys().iter().map(|c| c.fkey.unwrap().0).collect();
    assert_eq!(fkeys, ["F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10"]);
}

#[test]
fn f5_copy_with_conflict_answered_in_the_dialog() {
    let e = env();
    fs::create_dir(e.left.join("docs")).unwrap();
    fs::write(e.left.join("docs/one.txt"), "new one").unwrap();
    fs::write(e.left.join("docs/two.txt"), "new two").unwrap();
    fs::write(e.left.join("docs/three.txt"), "new three").unwrap();
    fs::create_dir(e.right.join("docs")).unwrap();
    fs::write(e.right.join("docs/one.txt"), "old one").unwrap();
    fs::write(e.right.join("docs/two.txt"), "old two").unwrap();
    let mut app = app(&e);

    // Navigate onto "docs" and press F5: the target is the other panel.
    press(&mut app, Key::Down);
    assert_eq!(cursor(&app, 0).as_deref(), Some("docs"));
    press(&mut app, Key::F(5));
    assert_eq!(app.modal_titles(), ["Copy"]);
    let text = app.top_modal_text().unwrap();
    assert!(text.contains("Copy 'docs' (directory) to:"), "{text}");
    assert!(text.ends_with(&e.right.to_string_lossy().into_owned()), "prefilled with the other panel: {text}");
    press(&mut app, Key::Enter);

    // The worker finds one.txt in the way and asks through ask_ui.
    wait_dialog(&mut app, "File exists");
    assert_eq!(app.modal_titles(), ["Copying", "File exists"]);
    let question = app.top_modal_text().unwrap();
    assert!(question.contains("one.txt") || question.contains("two.txt"), "{question}");
    // "s" = Skip this one; the next conflict asks again; "o" = Overwrite.
    let first = if question.contains("one.txt") { "one.txt" } else { "two.txt" };
    let second = if first == "one.txt" { "two.txt" } else { "one.txt" };
    type_text(&mut app, "s");
    wait_dialog(&mut app, "File exists");
    type_text(&mut app, "o");
    wait_no_dialogs(&mut app);

    let read = |name: &str| fs::read_to_string(e.right.join("docs").join(name)).unwrap();
    assert_eq!(read(first), format!("old {}", &first[..first.len() - 4]), "skipped");
    assert_eq!(read(second), format!("new {}", &second[..second.len() - 4]), "overwritten");
    assert_eq!(read("three.txt"), "new three", "no conflict, copied");
    assert!(app.toast.as_ref().is_some_and(|t| t.text == "Copying finished"));
    // Both panels were reloaded, cursors kept.
    assert_eq!(cursor(&app, 0).as_deref(), Some("docs"));
}

#[test]
fn conflict_answers_for_all_and_cancel() {
    let e = env();
    for n in ["a", "b", "c"] {
        fs::write(e.left.join(n), format!("new {n}")).unwrap();
        fs::write(e.right.join(n), format!("old {n}")).unwrap();
    }
    let mut app = app(&e);
    // Mark all, copy, "A" = overwrite all: asked once.
    press_with(&mut app, Key::Char('a'), Mods::primary());
    assert_eq!(app.main.sides[0].panel.marked.len(), 3);
    press(&mut app, Key::F(5));
    assert!(app.top_modal_text().unwrap().starts_with("Copy 3 files to:"));
    press(&mut app, Key::Enter);
    wait_dialog(&mut app, "File exists");
    type_text(&mut app, "a");
    wait_no_dialogs(&mut app);
    for n in ["a", "b", "c"] {
        assert_eq!(fs::read_to_string(e.right.join(n)).unwrap(), format!("new {n}"));
    }
    assert!(app.main.sides[0].panel.marked.is_empty(), "marks cleared after the operation");

    // Escape in the question cancels the whole copy.
    for n in ["a", "b", "c"] {
        fs::write(e.left.join(n), format!("newer {n}")).unwrap();
    }
    app.handle_event(MAIN_WINDOW, Event::Key(KeyEvent::typed("+")));
    press(&mut app, Key::F(5));
    press(&mut app, Key::Enter);
    wait_dialog(&mut app, "File exists");
    press(&mut app, Key::Escape);
    wait_no_dialogs(&mut app);
    assert!(app.toast.as_ref().is_some_and(|t| t.text == "Copying cancelled"));
    let newer = ["a", "b", "c"].iter().filter(|n| fs::read_to_string(e.right.join(n)).unwrap().starts_with("newer")).count();
    assert_eq!(newer, 0);
}

#[test]
fn f6_renames_in_the_same_folder_and_f7_makes_folders() {
    let e = env();
    fs::write(e.left.join("old.txt"), "x").unwrap();
    let mut app = app(&e);
    // Both panels on the same folder: F6 suggests the name (a rename).
    press_with(&mut app, Key::Char('e'), Mods::primary());
    settle(&mut app);
    put_cursor_on(&mut app, 0, "old.txt");
    press(&mut app, Key::F(6));
    assert!(app.top_modal_text().unwrap().ends_with("\nold.txt"));
    // Typing replaces the selected text.
    type_text(&mut app, "new.txt");
    press(&mut app, Key::Enter);
    wait_no_dialogs(&mut app);
    assert!(e.left.join("new.txt").exists() && !e.left.join("old.txt").exists());
    assert_eq!(names(&app, 1), ["new.txt"], "the other panel refreshed too");

    press(&mut app, Key::F(7));
    type_text(&mut app, "made/deeper");
    press(&mut app, Key::Enter);
    wait_until(&mut app, "mkdir", Duration::from_secs(10), |app| app.main.sides[0].panel.row_of("made").is_some());
    settle(&mut app);
    assert!(e.left.join("made/deeper").is_dir());
    assert_eq!(cursor(&app, 0).as_deref(), Some("made"));
}

#[test]
fn errors_show_in_a_message_box() {
    let e = env();
    fs::create_dir(e.left.join("d")).unwrap();
    let mut app = app(&e);
    press(&mut app, Key::Down);
    press(&mut app, Key::F(5));
    // Copying a folder into itself fails.
    let target = e.left.join("d").join("inside");
    type_text(&mut app, &target.to_string_lossy());
    press(&mut app, Key::Enter);
    wait_until(&mut app, "error box", Duration::from_secs(10), |app| app.operation.is_none() && app.modal_titles() == ["Copying"]);
    assert!(app.top_modal_text().unwrap().contains("into itself"), "{:?}", app.top_modal_text());
    press(&mut app, Key::Enter);
    assert!(app.modal_titles().is_empty());
}

#[test]
fn archives_open_like_folders() {
    oc_archive::register();
    let e = env();
    let zip_path = e.left.join("pack.zip");
    {
        let mut z = zip_writer(&zip_path);
        z.add("inside.txt", b"hello");
        z.finish();
    }
    let mut app = app(&e);
    put_cursor_on(&mut app, 0, "pack.zip");
    press(&mut app, Key::Enter);
    settle(&mut app);
    assert_eq!(names(&app, 0), ["inside.txt"]);
    assert!(app.main.sides[0].panel.subtitle().contains("read-only"));
    press(&mut app, Key::Backspace);
    settle(&mut app);
    assert_eq!(cursor(&app, 0).as_deref(), Some("pack.zip"));
    // Copy out of the archive (an "extraction").
    put_cursor_on(&mut app, 0, "pack.zip");
    press(&mut app, Key::Enter);
    settle(&mut app);
    put_cursor_on(&mut app, 0, "inside.txt");
    press(&mut app, Key::F(5));
    press(&mut app, Key::Enter);
    wait_no_dialogs(&mut app);
    assert_eq!(fs::read(e.right.join("inside.txt")).unwrap(), b"hello");
    assert!(app.toast.as_ref().is_some_and(|t| t.text == "Extracting finished"));
}

/// A minimal stored (uncompressed) zip writer, so the test needs no zip crate.
struct ZipW {
    path: std::path::PathBuf,
    data: Vec<u8>,
    central: Vec<u8>,
    count: u16,
}

fn zip_writer(path: &Path) -> ZipW {
    ZipW { path: path.to_path_buf(), data: Vec::new(), central: Vec::new(), count: 0 }
}

impl ZipW {
    fn add(&mut self, name: &str, body: &[u8]) {
        let crc = crc32(body);
        let off = self.data.len() as u32;
        let le16 = |v: u16| v.to_le_bytes();
        let le32 = |v: u32| v.to_le_bytes();
        let mut h = Vec::new();
        h.extend(le32(0x04034b50));
        h.extend(le16(20));
        h.extend(le16(0));
        h.extend(le16(0));
        h.extend(le16(0));
        h.extend(le16(0x21));
        h.extend(le32(crc));
        h.extend(le32(body.len() as u32));
        h.extend(le32(body.len() as u32));
        h.extend(le16(name.len() as u16));
        h.extend(le16(0));
        h.extend(name.as_bytes());
        self.data.extend(&h);
        self.data.extend(body);
        let mut c = Vec::new();
        c.extend(le32(0x02014b50));
        c.extend(le16(20));
        c.extend(le16(20));
        c.extend(le16(0));
        c.extend(le16(0));
        c.extend(le16(0));
        c.extend(le16(0x21));
        c.extend(le32(crc));
        c.extend(le32(body.len() as u32));
        c.extend(le32(body.len() as u32));
        c.extend(le16(name.len() as u16));
        c.extend(le16(0));
        c.extend(le16(0));
        c.extend(le16(0));
        c.extend(le16(0));
        c.extend(le32(0));
        c.extend(le32(off));
        c.extend(name.as_bytes());
        self.central.extend(c);
        self.count += 1;
    }

    fn finish(self) {
        let mut out = self.data;
        let cd_off = out.len() as u32;
        out.extend(&self.central);
        out.extend(0x06054b50u32.to_le_bytes());
        out.extend([0u8; 4]);
        out.extend(self.count.to_le_bytes());
        out.extend(self.count.to_le_bytes());
        out.extend((self.central.len() as u32).to_le_bytes());
        out.extend(cd_off.to_le_bytes());
        out.extend([0u8; 2]);
        fs::write(self.path, out).unwrap();
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn click(app: &mut App, kind: oc_ui::MouseKind, col: u16, row: u16) {
    app.handle_event(MAIN_WINDOW, Event::Mouse(oc_ui::MouseEvent { kind, col, row, mods: Mods::NONE }));
}

#[test]
fn mouse_on_tabs_header_rows_and_footer() {
    use oc_ui::{MouseButton::*, MouseKind::*};
    let e = env();
    fs::create_dir(e.left.join("sub")).unwrap();
    fs::write(e.left.join("big.txt"), vec![0u8; 5000]).unwrap();
    fs::write(e.left.join("a.txt"), "a").unwrap();
    let mut app = app(&e);
    let l = oc_ui::main_window::layout(&app);
    let pl = oc_ui::panel::draw::layout(l.frames[0]);
    // Rows: ".." at the first list row, then sub, a.txt, big.txt.
    click(&mut app, Down(Left), pl.rows.x + 3, pl.rows.y + 2);
    assert_eq!(cursor(&app, 0).as_deref(), Some("a.txt"));
    // Right click marks without moving on.
    click(&mut app, Down(Right), pl.rows.x + 3, pl.rows.y + 3);
    assert!(app.main.sides[0].panel.marked.contains("big.txt"));
    assert_eq!(cursor(&app, 0).as_deref(), Some("big.txt"));
    // Header click on "Size" sorts by size (dirs stay first).
    click(&mut app, Down(Left), pl.cols.size.x + 2, pl.header.y);
    settle(&mut app);
    assert_eq!(names(&app, 0), ["sub", "a.txt", "big.txt"]);
    assert_eq!(app.main.sides[0].panel.sort, oc_vfs::SortKey::Size);
    // Double click on a folder opens it.
    click(&mut app, Down(Left), pl.rows.x + 3, pl.rows.y + 1);
    click(&mut app, Down(Left), pl.rows.x + 3, pl.rows.y + 1);
    settle(&mut app);
    assert!(app.main.sides[0].panel.location.path.ends_with("sub"));
    // A click on the other side activates it; its Preview tab switches the mode.
    let rpl = oc_ui::panel::draw::layout(l.frames[1]);
    click(&mut app, Down(Left), rpl.rows.x + 2, rpl.rows.y);
    assert_eq!(app.main.active, 1);
    let preview_tab = oc_ui::widgets::boxframe::tab_rects(l.tabs[1], &["Files", "Preview"])[1];
    click(&mut app, Down(Left), preview_tab.x + 1, preview_tab.y);
    assert_eq!(app.main.sides[1].mode_id(), "preview");
    assert_eq!(app.main.active, 0, "the preview follows the other panel, which gets the focus");
    // The F-key bar runs commands: F9 Menu sits after F3..F8.
    let mut f = oc_grid::Frame::new(120, 40, oc_grid::Rgba::BLACK);
    app.draw(MAIN_WINDOW, &mut f, (8.0, 16.0));
    let footer = f.row_text(39);
    let x = footer.find("F9").unwrap() as u16; // ASCII up to there
    click(&mut app, Down(Left), x, 39);
    assert_eq!(app.modal_titles(), ["menu"]);
    press(&mut app, Key::Escape);
    // Wheel scrolling moves the view, not past the end.
    click(&mut app, Scroll(-5.0), pl.rows.x + 3, pl.rows.y);
    assert_eq!(app.main.sides[0].panel.list.top, 0);
}

struct Dummy {
    keys: usize,
    refuse_close: bool,
}

impl oc_ui::UiWindow for Dummy {
    fn title(&self) -> String {
        "dummy".into()
    }
    fn draw(&self, _app: &App, f: &mut oc_grid::Frame, ctx: &oc_ui::DrawCtx) {
        f.put_str(0, 0, &format!("keys {}", self.keys), oc_grid::Style::fg(ctx.theme.colors.foreground));
    }
    fn handle_event(&mut self, _app: &mut App, _id: oc_ui::WinId, ev: &Event) -> oc_ui::Outcome {
        if matches!(ev, Event::Key(_)) {
            self.keys += 1;
            return oc_ui::Outcome::Consumed;
        }
        oc_ui::Outcome::Ignored
    }
    fn close_requested(&mut self, _app: &mut App, _id: oc_ui::WinId) -> bool {
        !std::mem::take(&mut self.refuse_close)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[test]
fn other_windows_get_their_events_and_may_refuse_to_close() {
    let e = env();
    let mut app = app(&e);
    app.take_requests();
    let id = app.open_window(Box::new(Dummy { keys: 0, refuse_close: true }), 80, 24);
    assert!(matches!(app.take_requests()[..], [oc_ui::PlatformRequest::OpenWindow { cols: 80, rows: 24, .. }]));
    app.handle_event(id, Event::Key(KeyEvent::plain(Key::Char('x'))));
    assert_eq!(app.window_mut::<Dummy>(id).unwrap().keys, 1);
    let mut f = oc_grid::Frame::new(20, 2, oc_grid::Rgba::BLACK);
    app.draw(id, &mut f, (8.0, 16.0));
    assert!(f.row_text(0).starts_with("keys 1"));
    // Dialogs belong to their window.
    app.push_modal(id, Box::new(oc_ui::dialogs::MessageBox::new("Hi", "there", &["OK"], 0)));
    assert!(app.has_modal(id) && !app.has_modal(MAIN_WINDOW));
    press(&mut app, Key::Char('r')); // main window keys still work
    // Quitting asks the window, which refuses once.
    app.quit();
    assert!(!app.quitting);
    app.quit();
    assert!(app.quitting);
    assert!(app.take_requests().contains(&oc_ui::PlatformRequest::Quit));
}

#[test]
fn watched_folders_refresh_by_themselves() {
    let e = env();
    fs::write(e.left.join("first.txt"), "").unwrap();
    let mut cfg = oc_ui::AppConfig::new(oc_config::ConfigPaths::new(&e.config), std::sync::Arc::new(|| {}));
    cfg.left = Some(e.left.clone());
    cfg.right = Some(e.right.clone());
    cfg.clipboard = Box::new(oc_ui::clipboard::MemoryClipboard::default());
    cfg.watch = true;
    let mut app = App::new(cfg);
    app.resize(MAIN_WINDOW, 120, 40);
    settle(&mut app);
    put_cursor_on(&mut app, 0, "first.txt");
    // The watcher is started on a worker; give FSEvents & co. a moment to be live.
    wait_until(&mut app, "the watcher", Duration::from_secs(10), |app| app.tasks.pending() == 0);
    std::thread::sleep(Duration::from_millis(300));
    fs::write(e.left.join("0-new.txt"), "").unwrap();
    wait_until(&mut app, "the new file to show", Duration::from_secs(15), |app| {
        app.main.sides[0].panel.row_of("0-new.txt").is_some()
    });
    assert_eq!(cursor(&app, 0).as_deref(), Some("first.txt"), "the reload keeps the cursor");
}

#[test]
fn the_wheel_scrolls_the_view_not_the_cursor() {
    use oc_ui::MouseKind::Scroll;
    let e = env();
    for i in 0..200 {
        fs::write(e.left.join(format!("f{i:03}")), "").unwrap();
    }
    let mut app = app(&e);
    let pl = oc_ui::panel::draw::layout(oc_ui::main_window::layout(&app).frames[0]);
    press(&mut app, Key::Down);
    click(&mut app, Scroll(-10.0), pl.rows.x + 3, pl.rows.y);
    let p = &app.main.sides[0].panel;
    assert_eq!(p.cursor_name(), Some("f000"), "like 1.x (Qt): the cursor stays on its file");
    assert_eq!(p.list.top, 30);
    // A cursor key brings it back into view.
    press(&mut app, Key::Down);
    let p = &app.main.sides[0].panel;
    assert_eq!(p.cursor_name(), Some("f001"));
    assert!(p.list.top <= p.cursor());
}

#[test]
fn primary_plus_zooms_on_layouts_with_a_plus_key() {
    let e = env();
    let mut app = app(&e);
    let size = app.theme.font.size;
    press_with(&mut app, Key::Char('+'), Mods::primary());
    assert_eq!(app.theme.font.size, size + 1.0);
}

#[test]
fn features_keep_their_own_state_without_app_fields() {
    #[derive(Default)]
    struct Counter(u32);
    let e = env();
    let mut app = app(&e);
    assert!(app.try_state::<Counter>().is_none());
    app.state::<Counter>().0 += 2;
    let mut c = app.take_state::<Counter>();
    c.0 += 1;
    app.put_state(c);
    assert_eq!(app.try_state::<Counter>().map(|c| c.0), Some(3));
}
