//! Bookmarks driven through the app without a window: the ⌘D menu and its
//! hotkeys, adding / removing / organizing, ⌥⌘D search, archives, the
//! drive list and a click on a panel's path (ports of 1.x's
//! `test_bookmark_*`, `test_go_to_bookmark_search`, `test_organizer_*`).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use oc_config::bookmarks::{self as store, Bookmark, Folder, Item};
use oc_config::Spec;
use oc_ui::features::bookmarks;
use oc_ui::testing::{headless_app, press, press_with, settle, type_text, wait_until};
use oc_ui::{App, Event, Key, MAIN_WINDOW, Mods, MouseButton, MouseEvent, MouseKind};

struct Env {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    /// The left panel's folder ("src", with "sub" in it).
    src: PathBuf,
    /// The right panel's folder.
    dst: PathBuf,
    base: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = oc_local::absolute(&fs::canonicalize(tmp.path()).unwrap());
    let (config, src, dst) = (base.join("config"), base.join("src"), base.join("dst"));
    for d in [&config, &src, &dst, &src.join("sub")] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(src.join("sub/inner.txt"), "x").unwrap();
    Env { _tmp: tmp, config, src, dst, base }
}

fn local(p: &Path) -> Spec {
    Spec::local(p.to_string_lossy().into_owned())
}

fn with_bookmarks(e: &Env, items: &[Item]) {
    store::save(&e.config.join("bookmarks.json"), items).unwrap();
}

fn saved(e: &Env) -> Vec<Item> {
    store::load(&e.config.join("bookmarks.json"))
}

fn app(e: &Env) -> App {
    let mut app = headless_app(&e.config, &e.src, &e.dst);
    app.resize(MAIN_WINDOW, 120, 40);
    settle(&mut app);
    app
}

fn location(app: &App, side: usize) -> PathBuf {
    PathBuf::from(&app.main.sides[side].panel.location.path)
}

/// ⌘ / Ctrl + `key`, then wait: bookmark commands look at the file in the
/// background before they show anything.
fn primary(app: &mut App, key: char) {
    press_with(app, Key::Char(key), Mods::primary());
    settle(app);
}

fn top(app: &App) -> String {
    app.modal_titles().last().cloned().unwrap_or_default()
}

fn menu_lines(app: &App) -> Vec<String> {
    app.top_modal_text().unwrap_or_default().lines().map(str::to_owned).collect()
}

fn toast(app: &App) -> String {
    app.toast.as_ref().map(|t| t.text.clone()).unwrap_or_default()
}

fn sample(e: &Env) -> Vec<Item> {
    vec![
        Bookmark::new("&Src", local(&e.src), None).into(),
        Bookmark::new("&Dst", local(&e.dst), None).into(),
        Bookmark::new("&Sub", local(&e.src.join("sub")), None).into(),
        Item::separator(),
        Folder::new("&Work", vec![Bookmark::new("&Inside", local(&e.base), None).into()]).into(),
    ]
}

#[test]
fn commands_menu_groups_go_to_path_with_the_bookmarks() {
    let e = env();
    let app = app(&e);
    // 1.x ui/main_window.py: "Go to path" and the bookmark items are one group.
    let items = oc_ui::menus::menu_items(&app, oc_ui::menus::Menu::Commands);
    assert_eq!(
        items,
        [
            Some("app.palette"),
            None,
            Some("preview.toggle"),
            Some("preview.cycle"),
            None,
            Some("nav.goto"),
            Some("bookmarks.menu"),
            Some("bookmarks.goto"),
            Some("bookmarks.add"),
            Some("bookmarks.organize"),
            None,
            Some("conn.manager"),
            Some("conn.disconnect"),
            None,
            Some("panel.swap"),
            Some("panel.same"),
            Some("panel.reload"),
            None,
            Some("terminal.toggle"),
            Some("terminal.panel_folder"),
        ]
    );
}

#[test]
fn menu_lists_bookmarks_and_hotkeys_open_or_cycle() {
    let e = env();
    with_bookmarks(&e, &sample(&e));
    let mut app = app(&e);
    primary(&mut app, 'd');
    assert_eq!(top(&app), "Bookmarks");
    let lines = menu_lines(&app);
    assert_eq!(lines[..4], ["S  Src", "D  Dst", "S  Sub", "W  Work"]);
    assert!(lines.contains(&"-  Remove current folder (Src)".to_owned()) && !lines.iter().any(|l| l.starts_with('+')));
    assert!(lines.contains(&"*  Organize bookmarks…".to_owned()));

    // A unique hotkey opens right away.
    type_text(&mut app, "d");
    settle(&mut app);
    assert!(app.modal_titles().is_empty());
    assert_eq!(location(&app, 0), e.dst);
    assert_eq!(app.main.active, 0);

    // A shared one moves between the entries; Enter opens the highlighted one.
    primary(&mut app, 'd');
    type_text(&mut app, "s");
    assert_eq!(top(&app), "Bookmarks", "still open");
    type_text(&mut app, "s");
    press(&mut app, Key::Enter);
    settle(&mut app);
    assert_eq!(location(&app, 0), e.src.join("sub"));

    // Folders are submenus: their hotkey opens them, Escape goes back up.
    primary(&mut app, 'd');
    type_text(&mut app, "w");
    assert_eq!(menu_lines(&app), ["I  Inside"]);
    press(&mut app, Key::Escape);
    assert_eq!(menu_lines(&app)[0], "S  Src");
    press(&mut app, Key::Right); // not a folder: nothing
    type_text(&mut app, "w");
    type_text(&mut app, "i");
    settle(&mut app);
    assert_eq!(location(&app, 0), e.base);
    // Escape closes the menu.
    primary(&mut app, 'd');
    press(&mut app, Key::Escape);
    assert!(app.modal_titles().is_empty());
}

#[test]
fn commands_wait_for_the_file_to_be_read() {
    let e = env();
    with_bookmarks(&e, &sample(&e));
    let mut app = headless_app(&e.config, &e.src, &e.dst);
    primary(&mut app, 'd'); // before the background read is through
    settle(&mut app);
    assert_eq!(top(&app), "Bookmarks");
    assert_eq!(menu_lines(&app)[0], "S  Src");
}

#[test]
fn add_bookmark_with_other_panel_then_open_both() {
    let e = env();
    let mut app = app(&e);
    primary(&mut app, 'd');
    assert_eq!(menu_lines(&app), ["+  Add current folder…", "*  Organize bookmarks…"]);
    type_text(&mut app, "+");
    settle(&mut app);
    assert_eq!(top(&app), "Add bookmark");
    let text = app.top_modal_text().unwrap();
    assert!(text.starts_with("src\n"), "the folder's name is the default title: {text}");
    assert!(text.contains("Top level") && text.contains("Also open the other panel at"));
    type_text(&mut app, "&Pair"); // replaces the selected default
    press(&mut app, Key::Tab); // Put in
    press(&mut app, Key::Tab); // Also open the other panel…
    press(&mut app, Key::Space);
    press(&mut app, Key::Enter);
    settle(&mut app);
    assert!(app.modal_titles().is_empty());
    assert!(toast(&app).starts_with("Bookmarked "), "{}", toast(&app));
    let now = saved(&e);
    let [Item::Bookmark(b)] = now.as_slice() else { panic!("one bookmark saved: {now:?}") };
    assert_eq!((b.title.as_str(), &b.location, b.target.as_ref()), ("&Pair", &local(&e.src), Some(&local(&e.dst))));

    // Both panels elsewhere; the bookmark brings both back.
    for side in 0..2 {
        oc_ui::panel::load(&mut app, side, oc_local::local(&e.base), Default::default());
    }
    settle(&mut app);
    primary(&mut app, 'd');
    type_text(&mut app, "p");
    settle(&mut app);
    assert_eq!((location(&app, 0), location(&app, 1)), (e.src.clone(), e.dst.clone()));
    assert_eq!(app.main.active, 0, "the focus stays on the bookmark's side");
}

#[test]
fn add_into_a_folder_and_cancel() {
    let e = env();
    with_bookmarks(&e, &[Folder::new("&Work", vec![]).into()]);
    let mut app = app(&e);
    app.run_command("bookmarks.add");
    settle(&mut app);
    assert_eq!(top(&app), "Add bookmark");
    press(&mut app, Key::Escape);
    settle(&mut app);
    assert_eq!(saved(&e).len(), 1, "Cancel adds nothing");
    app.run_command("bookmarks.add");
    settle(&mut app);
    press(&mut app, Key::Tab);
    press(&mut app, Key::Down); // Put in: Work
    press(&mut app, Key::Enter);
    settle(&mut app);
    let now = saved(&e);
    let [Item::Folder(work)] = now.as_slice() else { panic!() };
    let [Item::Bookmark(b)] = work.items.as_slice() else { panic!("{work:?}") };
    assert_eq!(b.title, "src");
}

#[test]
fn remove_current_folder() {
    let e = env();
    with_bookmarks(&e, &sample(&e));
    let mut app = app(&e);
    primary(&mut app, 'd');
    type_text(&mut app, "-");
    settle(&mut app);
    assert_eq!(toast(&app), "Removed bookmark Src");
    let left = saved(&e);
    assert_eq!(left.len(), 4);
    assert!(store::find_location(&left, local(&e.src).as_ref()).is_none());
}

#[test]
fn go_to_bookmark_searches_titles_and_paths() {
    let e = env();
    let mut app = app(&e);
    press_with(&mut app, Key::Char('d'), Mods { alt: true, ..Mods::primary() });
    settle(&mut app);
    assert!(app.modal_titles().is_empty());
    assert!(toast(&app).starts_with("No bookmarks yet"));

    let e = env();
    with_bookmarks(
        &e,
        &[
            Bookmark::new("&Top", local(&e.dst), None).into(),
            Folder::new("&Work", vec![Bookmark::new("&Sub", local(&e.src.join("sub")), None).into()]).into(),
        ],
    );
    let mut app = self::app(&e);
    press_with(&mut app, Key::Char('d'), Mods { alt: true, ..Mods::primary() });
    settle(&mut app);
    assert_eq!(top(&app), "Go to bookmark");
    assert_eq!(menu_lines(&app), ["Top", "Work › Sub"]);
    type_text(&mut app, "sub");
    assert_eq!(menu_lines(&app)[0], "Work › Sub");
    press(&mut app, Key::Enter);
    settle(&mut app);
    assert_eq!(location(&app, 0), e.src.join("sub"));
}

#[test]
fn bookmark_inside_an_archive_opens_and_round_trips() {
    oc_archive::register();
    let e = env();
    let zip = e.dst.join("pack.zip");
    write_zip(&zip, &[("docs/readme.md", b"# hi"), ("top.txt", b"x")]);
    let spec = Spec::archive(local(&zip), "docs");
    with_bookmarks(&e, &[Bookmark::new("&Docs", spec.clone(), None).into()]);
    let mut app = app(&e);
    primary(&mut app, 'd');
    type_text(&mut app, "d");
    wait_until(&mut app, "the archive folder", Duration::from_secs(10), |app| {
        !app.main.sides[0].panel.location.is_local() && !app.main.sides[0].panel.loading()
    });
    settle(&mut app);
    let names: Vec<_> = app.main.sides[0].panel.entries.iter().map(|e| e.name.clone()).collect();
    assert_eq!(names, ["readme.md"]);
    assert_eq!(bookmarks::location_spec(&app, 0), Some(spec), "and it round-trips");
    // A damaged bookmark says so.
    let mut damaged = Spec::archive(local(&zip), "x");
    damaged.0.remove("archive");
    bookmarks::navigate(&mut app, &damaged, 0, true);
    assert!(toast(&app).starts_with("Can't open this bookmark: "), "{}", toast(&app));
    // A missing archive: an error, the panel stays.
    let missing = Spec::archive(local(&e.dst.join("gone.zip")), "");
    bookmarks::navigate(&mut app, &missing, 0, true);
    wait_until(&mut app, "the error", Duration::from_secs(10), |app| app.toast.as_ref().is_some_and(|t| t.text.contains("gone.zip")));
    assert_eq!(app.main.sides[0].panel.location.path, "docs");
}

#[test]
fn server_folders_are_described_by_their_account() {
    let e = env();
    let mut app = app(&e);
    let mem = oc_vfs::testing::MemFS::new();
    mem.add_dir("pub");
    let root = mem.root();
    oc_ui::panel::load(&mut app, 1, root.child(&["pub"]), Default::default());
    settle(&mut app);
    assert_eq!(bookmarks::location_spec(&app, 1), None, "an unknown server can't be bookmarked");
    bookmarks::locations::register_account_of(&mut app, |_, fs| (!fs.is_local()).then(|| "sftp://me@host".to_owned()));
    let spec = bookmarks::location_spec(&app, 1).unwrap();
    assert_eq!(spec, Spec::server("sftp://me@host", app.main.sides[1].panel.location.path.clone()));
    // The menu on that side offers to add it.
    app.main.active = 1;
    primary(&mut app, 'd');
    assert_eq!(menu_lines(&app)[0], "+  Add current folder…");
}

#[test]
fn organizer_edits_apply_only_with_ok() {
    let e = env();
    let items = sample(&e);
    with_bookmarks(&e, &items);
    let mut app = app(&e);
    primary(&mut app, 'd');
    type_text(&mut app, "*");
    settle(&mut app);
    assert_eq!(top(&app), "Organize bookmarks");
    let rows = menu_lines(&app);
    assert_eq!(rows.len(), 6, "{rows:?}");
    assert!(rows[0].starts_with("S  Src\t") && rows[0].ends_with('\t'));
    assert!(rows[3].starts_with("────────"));
    assert!(rows[4].starts_with("▾ W  Work"));
    // Move Dst up, then Cancel: nothing changes.
    press(&mut app, Key::Down);
    press_with(&mut app, Key::Up, Mods::ALT);
    assert!(menu_lines(&app)[0].starts_with("D  Dst"));
    press(&mut app, Key::Escape);
    settle(&mut app);
    assert_eq!(saved(&e), items);

    // Again: move, a new folder (F7), rename it (F2), delete Sub, OK.
    app.run_command("bookmarks.organize");
    settle(&mut app);
    press(&mut app, Key::Down);
    press_with(&mut app, Key::Up, Mods::ALT);
    press(&mut app, Key::F(7));
    assert_eq!(top(&app), "New folder");
    type_text(&mut app, "&Group");
    press(&mut app, Key::Enter);
    assert_eq!(top(&app), "Organize bookmarks");
    press(&mut app, Key::F(2));
    assert_eq!(top(&app), "Rename folder");
    type_text(&mut app, "&Team");
    press(&mut app, Key::Enter);
    press(&mut app, Key::Down); // Src
    press(&mut app, Key::Down); // Sub
    press(&mut app, Key::Delete);
    press(&mut app, Key::Enter); // OK
    settle(&mut app);
    assert!(app.modal_titles().is_empty());
    let now = saved(&e);
    let titles: Vec<String> = now
        .iter()
        .map(|i| match i {
            Item::Bookmark(b) => b.title.clone(),
            Item::Folder(f) => f.title.clone(),
            Item::Separator { .. } => "-".into(),
        })
        .collect();
    assert_eq!(titles, ["&Dst", "&Team", "&Src", "-", "&Work"]);
    // The menu shows the new order.
    primary(&mut app, 'd');
    assert_eq!(menu_lines(&app)[..3], ["D  Dst", "T  Team", "S  Src"]);
}

#[test]
fn organizer_edit_bookmark_dialog() {
    let e = env();
    with_bookmarks(&e, &[Bookmark::new("&Src", local(&e.src), None).into()]);
    let mut app = app(&e);
    app.run_command("bookmarks.organize");
    settle(&mut app);
    press(&mut app, Key::F(2));
    assert_eq!(top(&app), "Edit bookmark");
    type_text(&mut app, "x"); // appended to the title
    press(&mut app, Key::Tab); // Folder
    // Clear the folder: "Enter a folder to bookmark"
    press_with(&mut app, Key::Char('a'), Mods::primary());
    press(&mut app, Key::Backspace);
    press(&mut app, Key::Enter);
    assert_eq!(top(&app), "Edit bookmark");
    assert!(app.top_modal_text().unwrap().ends_with("Enter a folder to bookmark"));
    press(&mut app, Key::Tab); // Use active panel
    press(&mut app, Key::Space);
    press(&mut app, Key::Tab); // Other panel
    type_text(&mut app, &e.dst.to_string_lossy());
    press(&mut app, Key::Enter);
    assert_eq!(top(&app), "Organize bookmarks");
    press(&mut app, Key::Enter);
    settle(&mut app);
    let now = saved(&e);
    let [Item::Bookmark(b)] = now.as_slice() else { panic!() };
    assert_eq!((b.title.as_str(), &b.location, b.target.as_ref()), ("&Srcx", &local(&e.src), Some(&local(&e.dst))));
}

#[test]
fn click_on_panel_path_opens_that_sides_menu() {
    let e = env();
    with_bookmarks(&e, &sample(&e));
    let mut app = app(&e);
    let frame = oc_ui::main_window::layout(&app).frames[1];
    let ev = MouseEvent { kind: MouseKind::Down(MouseButton::Left), col: frame.x + 3, row: frame.y, mods: Mods::NONE };
    app.handle_event(MAIN_WINDOW, Event::Mouse(ev));
    settle(&mut app);
    assert_eq!(top(&app), "Bookmarks");
    assert!(menu_lines(&app).contains(&"-  Remove current folder (Dst)".to_owned()));
    // Clicking outside closes it.
    let away = MouseEvent { kind: MouseKind::Down(MouseButton::Left), col: 1, row: 38, mods: Mods::NONE };
    app.handle_event(MAIN_WINDOW, Event::Mouse(away));
    assert!(app.modal_titles().is_empty());
}

#[test]
fn drive_list_offers_bookmarks() {
    let e = env();
    with_bookmarks(&e, &sample(&e));
    let mut app = app(&e);
    press_with(&mut app, Key::F(2), Mods::ALT);
    wait_until(&mut app, "the drive list", Duration::from_secs(10), |app| app.modal_titles().last().is_some_and(|t| t == "Go to"));
    assert!(menu_lines(&app).contains(&"★ Inside".to_owned()));
    type_text(&mut app, "★ Sub");
    press(&mut app, Key::Enter);
    settle(&mut app);
    assert_eq!(location(&app, 1), e.src.join("sub"));
}

// -- Other apps share bookmarks.json -----------------------------------------

/// Another app (another Orchard app's file picker, another instance) adds a
/// bookmark to the file, the way they do.
fn added_elsewhere(e: &Env, title: &str) -> Bookmark {
    let b = Bookmark::new(title, local(&e.base), None);
    store::add_bookmark(&e.config.join("bookmarks.json"), b.clone(), None).unwrap();
    b
}

fn titles(items: &[Item]) -> Vec<String> {
    items
        .iter()
        .map(|i| match i {
            Item::Bookmark(b) => b.title.clone(),
            Item::Folder(f) => f.title.clone(),
            Item::Separator { .. } => "-".into(),
        })
        .collect()
}

#[test]
fn bookmarks_another_app_added_show_up() {
    let e = env();
    with_bookmarks(&e, &sample(&e));
    let mut app = app(&e);
    primary(&mut app, 'd');
    press(&mut app, Key::Escape);
    added_elsewhere(&e, "&Elsewhere");

    primary(&mut app, 'd');
    assert!(menu_lines(&app).contains(&"E  Elsewhere".to_owned()), "{:?}", menu_lines(&app));
    press(&mut app, Key::Escape);

    added_elsewhere(&e, "&Later");
    press_with(&mut app, Key::Char('d'), Mods { alt: true, ..Mods::primary() });
    settle(&mut app);
    assert_eq!(top(&app), "Go to bookmark");
    assert!(menu_lines(&app).contains(&"Later".to_owned()), "{:?}", menu_lines(&app));
    press(&mut app, Key::Escape);

    added_elsewhere(&e, "&Drive list");
    press_with(&mut app, Key::F(1), Mods::ALT);
    wait_until(&mut app, "the drive list", Duration::from_secs(10), |app| app.modal_titles().last().is_some_and(|t| t == "Go to"));
    assert!(menu_lines(&app).contains(&"★ Drive list".to_owned()), "{:?}", menu_lines(&app));
    press(&mut app, Key::Escape);

    added_elsewhere(&e, "&Organized");
    app.run_command("bookmarks.organize");
    settle(&mut app);
    assert!(menu_lines(&app).iter().any(|l| l.starts_with("O  Organized")), "{:?}", menu_lines(&app));
}

#[test]
fn adding_keeps_a_bookmark_another_app_added_meanwhile() {
    let e = env();
    with_bookmarks(&e, &[Bookmark::new("&Dst", local(&e.dst), None).into()]);
    let mut app = app(&e);
    app.run_command("bookmarks.add");
    settle(&mut app);
    assert_eq!(top(&app), "Add bookmark");
    // While the dialog is open.
    added_elsewhere(&e, "&Elsewhere");
    type_text(&mut app, "&Mine");
    press(&mut app, Key::Enter);
    settle(&mut app);
    assert_eq!(titles(&saved(&e)), ["&Dst", "&Elsewhere", "&Mine"]);
    primary(&mut app, 'd');
    assert_eq!(menu_lines(&app)[..3], ["D  Dst", "E  Elsewhere", "M  Mine"]);

    // Removing one keeps the others, also one added after the menu opened.
    added_elsewhere(&e, "&Another");
    let removed = titles(&saved(&e));
    assert_eq!(removed.len(), 4);
    press(&mut app, Key::Escape);
    oc_ui::panel::load(&mut app, 0, oc_local::local(&e.dst), Default::default());
    settle(&mut app);
    primary(&mut app, 'd');
    type_text(&mut app, "-");
    settle(&mut app);
    assert_eq!(toast(&app), "Removed bookmark Dst");
    assert_eq!(titles(&saved(&e)), ["&Elsewhere", "&Mine", "&Another"]);
}

#[test]
fn organizer_ok_keeps_a_bookmark_another_app_added_meanwhile() {
    let e = env();
    with_bookmarks(&e, &sample(&e));
    let mut app = app(&e);
    app.run_command("bookmarks.organize");
    settle(&mut app);
    assert_eq!(top(&app), "Organize bookmarks");
    press(&mut app, Key::Down); // Dst
    press(&mut app, Key::Delete);
    // While the organizer is open.
    let elsewhere = added_elsewhere(&e, "&Elsewhere");
    press(&mut app, Key::Enter); // OK
    settle(&mut app);
    assert!(app.modal_titles().is_empty());
    let now = saved(&e);
    assert_eq!(titles(&now), ["&Src", "&Sub", "-", "&Work", "&Elsewhere"]);
    assert_eq!(now.last(), Some(&Item::Bookmark(elsewhere)), "kept as it was saved");
    primary(&mut app, 'd');
    assert!(menu_lines(&app).contains(&"E  Elsewhere".to_owned()), "{:?}", menu_lines(&app));
}

fn mouse(app: &mut App, kind: MouseKind, col: u16, row: u16) {
    app.handle_event(MAIN_WINDOW, Event::Mouse(MouseEvent { kind, col, row, mods: Mods::NONE }));
}

/// Where the top dialog's text `needle` is drawn (col, row).
fn find_on_screen(app: &App, needle: &str) -> (u16, u16) {
    let (w, h) = (120u16, 40u16);
    let mut f = orchard_grid::Frame::new(w, h, app.theme.colors.background);
    app.draw(MAIN_WINDOW, &mut f, (8.0, 16.0));
    for row in 0..h {
        let line: String = (0..w).map(|c| f.cell(c, row).map_or(' ', |cell| cell.ch)).collect();
        if let Some(byte) = line.find(needle) {
            return (line[..byte].chars().count() as u16, row);
        }
    }
    panic!("{needle:?} not on screen");
}

#[test]
fn hovering_a_folder_opens_its_submenu() {
    let e = env();
    with_bookmarks(&e, &sample(&e));
    let mut app = app(&e);
    primary(&mut app, 'd');
    let (col, row) = find_on_screen(&app, "Work");
    mouse(&mut app, MouseKind::Move, col, row);
    assert_eq!(menu_lines(&app), ["I  Inside"], "the folder's submenu is open");
    // Another entry of the parent closes it again.
    let (col, row) = find_on_screen(&app, "Dst");
    mouse(&mut app, MouseKind::Move, col, row);
    assert_eq!(menu_lines(&app)[0], "S  Src");
    assert_eq!(top(&app), "Bookmarks");
}

#[test]
fn organizer_click_on_folder_mark_closes_and_opens_it() {
    let e = env();
    with_bookmarks(&e, &sample(&e));
    let mut app = app(&e);
    app.run_command("bookmarks.organize");
    settle(&mut app);
    assert_eq!(menu_lines(&app).len(), 6, "{:?} {:?}", app.modal_titles(), menu_lines(&app));
    let (col, row) = find_on_screen(&app, "▾ W  Work");
    mouse(&mut app, MouseKind::Down(MouseButton::Left), col, row);
    mouse(&mut app, MouseKind::Up(MouseButton::Left), col, row);
    assert_eq!(menu_lines(&app).len(), 5, "Work is closed: {:?}", menu_lines(&app));
    assert!(menu_lines(&app)[4].starts_with("▸ W  Work"));
    mouse(&mut app, MouseKind::Down(MouseButton::Left), col, row);
    mouse(&mut app, MouseKind::Up(MouseButton::Left), col, row);
    assert_eq!(menu_lines(&app).len(), 6);
}

#[test]
fn a_pending_save_is_written_on_quit() {
    let e = env();
    let mut app = app(&e);
    bookmarks::set_items(&mut app, vec![Bookmark::new("&Src", local(&e.src), None).into()]);
    // No waiting for the background write: quitting writes it.
    app.shutdown();
    assert_eq!(saved(&e).len(), 1);
}

#[test]
fn a_late_bookmark_does_not_take_the_panel_back() {
    oc_archive::register();
    let e = env();
    let zip = e.dst.join("pack.zip");
    write_zip(&zip, &[("docs/readme.md", b"# hi")]);
    let mut app = app(&e);
    // The archive opens on a worker; meanwhile the user goes elsewhere.
    bookmarks::navigate(&mut app, &Spec::archive(local(&zip), "docs"), 0, true);
    oc_ui::panel::load(&mut app, 0, oc_local::local(&e.base), Default::default());
    settle(&mut app);
    assert_eq!(location(&app, 0), e.base);
    // Two bookmarks in a row: the later one wins even if the first is slower.
    bookmarks::navigate(&mut app, &Spec::archive(local(&zip), "docs"), 0, true);
    bookmarks::navigate(&mut app, &local(&e.src.join("sub")), 0, true);
    settle(&mut app);
    assert_eq!(location(&app, 0), e.src.join("sub"));
}

// ---------------------------------------------------------------------------

/// A minimal stored (uncompressed) zip.
fn write_zip(path: &Path, files: &[(&str, &[u8])]) {
    let (mut data, mut central) = (Vec::new(), Vec::new());
    for (name, body) in files {
        let crc = crc32(body);
        let off = data.len() as u32;
        let mut h = Vec::new();
        h.extend(0x04034b50u32.to_le_bytes());
        h.extend([20u8, 0, 0, 0, 0, 0, 0, 0, 0x21, 0]);
        h.extend(crc.to_le_bytes());
        h.extend((body.len() as u32).to_le_bytes());
        h.extend((body.len() as u32).to_le_bytes());
        h.extend((name.len() as u16).to_le_bytes());
        h.extend([0u8, 0]);
        h.extend(name.as_bytes());
        data.extend(&h);
        data.extend(*body);
        central.extend(0x02014b50u32.to_le_bytes());
        central.extend([20u8, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0x21, 0]);
        central.extend(crc.to_le_bytes());
        central.extend((body.len() as u32).to_le_bytes());
        central.extend((body.len() as u32).to_le_bytes());
        central.extend((name.len() as u16).to_le_bytes());
        central.extend([0u8; 12]);
        central.extend(off.to_le_bytes());
        central.extend(name.as_bytes());
    }
    let cd_off = data.len() as u32;
    let n = files.len() as u16;
    data.extend(&central);
    data.extend(0x06054b50u32.to_le_bytes());
    data.extend([0u8; 4]);
    data.extend(n.to_le_bytes());
    data.extend(n.to_le_bytes());
    data.extend((central.len() as u32).to_le_bytes());
    data.extend(cd_off.to_le_bytes());
    data.extend([0u8; 2]);
    fs::write(path, data).unwrap();
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
