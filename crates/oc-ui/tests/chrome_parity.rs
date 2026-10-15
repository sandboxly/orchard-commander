//! 1.x behaviour of the terminal, Settings and window chrome that 2.0
//! first lacked: composed text and the pointer in the terminal, sideways
//! scrolling, the "go to panel folder" cd, typed spin box values, the color
//! picker, scheme files added while running, the palette's commands.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use oc_config::themes;
use orchard_grid::{Frame, Rgba};
#[cfg(unix)]
use oc_ui::features::terminal::{self, session};
#[cfg(unix)]
use oc_ui::main_window::layout;
use oc_ui::testing::{headless_app, press, press_with, settle, type_text, wait_until};
use oc_ui::{App, Event, Key, MAIN_WINDOW, Mods, MouseButton, MouseEvent, MouseKind};
#[cfg(unix)]
use oc_ui::Pointer;

const WAIT: Duration = Duration::from_secs(20);

struct Env {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    left: PathBuf,
    right: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let (config, left, right) = (base.join("config"), base.join("left"), base.join("right"));
    for d in [&config, &left.join("sub"), &right] {
        fs::create_dir_all(d).unwrap();
    }
    Env { _tmp: tmp, config, left, right }
}

fn app(e: &Env) -> App {
    let mut app = headless_app(&e.config, &e.left, &e.right);
    app.settings.terminal_shell = "/bin/sh".into();
    app.resize(MAIN_WINDOW, 120, 40);
    settle(&mut app);
    app
}

fn mouse(app: &mut App, kind: MouseKind, col: u16, row: u16) {
    app.handle_event(MAIN_WINDOW, Event::Mouse(MouseEvent { kind, col, row, mods: Mods::NONE }));
}

fn screen(app: &App) -> Frame {
    let mut f = Frame::new(120, 40, Rgba::BLACK);
    app.draw(MAIN_WINDOW, &mut f, (8.0, 16.0));
    f
}

/// Where `needle` is on screen: (column, row).
fn find(app: &App, needle: &str) -> (u16, u16) {
    let f = screen(app);
    (0..40)
        .find_map(|r| {
            let text = f.row_text(r);
            text.find(needle).map(|i| (text[..i].chars().count() as u16, r))
        })
        .unwrap_or_else(|| panic!("{needle:?} not on screen"))
}

#[cfg(unix)]
fn start_terminal(app: &mut App) -> std::sync::Arc<oc_ui::features::terminal::Session> {
    app.run_command("terminal.toggle");
    wait_until(app, "the shell", WAIT, |app| session(app, 0).is_some_and(|s| s.alive()));
    session(app, 0).unwrap()
}

#[cfg(unix)]
#[test]
fn terminal_takes_composed_text_and_shows_an_ibeam() {
    let e = env();
    let mut app = app(&e);
    assert!(!app.wants_text_input(MAIN_WINDOW), "panels: no input method");
    let s = start_terminal(&mut app);
    assert!(app.wants_text_input(MAIN_WINDOW), "1.x: WA_InputMethodEnabled on the terminal");
    terminal::record_input(&mut app);
    app.handle_event(MAIN_WINDOW, Event::Commit("é".into()));
    assert_eq!(terminal::take_recorded(&mut app), vec!["é".as_bytes().to_vec()]);

    let b = layout(&app).bottom;
    mouse(&mut app, MouseKind::Move, b.x + 3, b.y + 2);
    assert_eq!(app.pointer(MAIN_WINDOW), Pointer::Text);
    s.feed(b"\x1b[?1000h\x1b[?1006h"); // a program takes the mouse: an arrow
    assert_eq!(app.pointer(MAIN_WINDOW), Pointer::Default);
    // Sideways scrolling reaches it as wheel left (button 66), three steps a notch like vertical.
    mouse(&mut app, MouseKind::HScroll(1.0), b.x + 2, b.y + 2);
    assert_eq!(terminal::take_recorded(&mut app), vec![b"\x1b[<66;2;2M".repeat(3)]);
    s.feed(b"\x1b[?1000l");
    let f = layout(&app).frames[0];
    mouse(&mut app, MouseKind::Move, f.x + 3, f.y + 3);
    assert_eq!(app.pointer(MAIN_WINDOW), Pointer::Default, "over the panels");

    // A click in the panels: the keyboard (and the input method) go back there.
    mouse(&mut app, MouseKind::Down(MouseButton::Left), f.x + 3, f.bottom() - 3);
    mouse(&mut app, MouseKind::Up(MouseButton::Left), f.x + 3, f.bottom() - 3);
    assert!(!terminal::has_focus(&app) && !app.wants_text_input(MAIN_WINDOW));
}

#[cfg(unix)]
#[test]
fn go_to_panel_folder_types_1x_cd() {
    let e = env();
    let mut app = app(&e);
    app.update_settings(|s| s.terminal_link = Some(false));
    start_terminal(&mut app);
    terminal::record_input(&mut app);
    app.run_command("terminal.panel_folder");
    let typed = terminal::take_recorded(&mut app).concat();
    let want = format!("\x15cd {}\r", oc_term::shell_quote(&e.left.to_string_lossy()));
    assert_eq!(String::from_utf8(typed).unwrap(), want, "no leading space: it goes into the history");
}

#[test]
fn palette_lists_every_command() {
    let e = env();
    let mut app = app(&e);
    press_with(&mut app, Key::Char('p'), Mods::primary());
    type_text(&mut app, "menu");
    assert_eq!(app.top_modal_text().unwrap().lines().next(), Some("Menu"));
    press(&mut app, Key::Escape);
    press_with(&mut app, Key::Char('p'), Mods::primary());
    type_text(&mut app, "stop search");
    assert_eq!(app.top_modal_text().unwrap().lines().next(), Some("Stop search"), "even with no search running");
    press(&mut app, Key::Enter); // does nothing
    settle(&mut app);
    assert!(app.modal_titles().is_empty());
}

fn open_settings(app: &mut App) {
    press_with(app, Key::Char(','), Mods::primary());
    assert_eq!(app.modal_titles(), ["Settings"]);
}

fn tab_to(app: &mut App, want: &str) {
    for _ in 0..60 {
        if app.top_modal_text().unwrap_or_default().contains(&format!("focus={want}")) {
            return;
        }
        press(app, Key::Tab);
    }
    panic!("never reached {want}");
}

#[test]
fn spin_boxes_take_typed_numbers_and_the_wheel() {
    let e = env();
    let mut app = app(&e);
    open_settings(&mut app);
    tab_to(&mut app, "Ctl(Size)");
    type_text(&mut app, "2");
    assert_eq!(app.settings.font_size, None, "2 pt is below the range: not taken yet");
    find(&app, " 2 pt"); // shown as typed
    type_text(&mut app, "4");
    assert_eq!(app.settings.font_size, Some(24));
    type_text(&mut app, "9"); // 249: past the maximum, refused
    assert_eq!(app.settings.font_size, Some(24));
    press(&mut app, Key::Backspace);
    type_text(&mut app, "0");
    assert_eq!(app.settings.font_size, Some(20));

    press(&mut app, Key::Tab);
    type_text(&mut app, "1.25");
    assert_eq!(app.settings.line_spacing, Some(1.25));
    type_text(&mut app, "5"); // a third decimal: refused
    assert_eq!(app.settings.line_spacing, Some(1.25));

    let (col, row) = find(&app, "20 pt");
    mouse(&mut app, MouseKind::Scroll(1.0), col, row);
    assert_eq!(app.settings.font_size, Some(21));
    mouse(&mut app, MouseKind::Scroll(-2.0), col, row);
    assert_eq!(app.settings.font_size, Some(19));
}

#[test]
fn settings_reread_scheme_files() {
    let e = env();
    let mut app = app(&e);
    wait_until(&mut app, "the start", WAIT, |app| app.tasks.pending() == 0);
    let mut scheme = themes::builtin_scheme("nord").unwrap().clone();
    scheme.name = "Copied in".into();
    themes::save_user_scheme(&e.config.join("color-schemes"), &scheme, None).unwrap();
    open_settings(&mut app);
    wait_until(&mut app, "the schemes", WAIT, |app| app.color_schemes().iter().any(|s| s.name == "Copied in"));
}

#[test]
fn scheme_editor_swatches_open_a_color_picker() {
    let e = env();
    let mut app = app(&e);
    open_settings(&mut app);
    tab_to(&mut app, "Ctl(Edit(Color))");
    press(&mut app, Key::Enter);
    assert_eq!(app.modal_titles(), ["Settings", "Edit color scheme"]);

    // The hex fields take at most 7 characters (1.x: setMaxLength(7)).
    press(&mut app, Key::Down);
    press(&mut app, Key::Down);
    press(&mut app, Key::Down); // Window background
    assert!(app.top_modal_text().unwrap().contains("focus=Hex(0)"));
    press_with(&mut app, Key::Char('a'), Mods::primary());
    type_text(&mut app, "#123456789");
    assert_eq!(app.theme.colors.background, Rgba::hex(0x123456));
    let (hex_col, row) = find(&app, "#123456");
    assert!(!screen(&app).row_text(row).contains("#1234567"));

    // A click on the swatch: a picker titled with the role, like QColorDialog.
    mouse(&mut app, MouseKind::Down(MouseButton::Left), hex_col - 3, row);
    mouse(&mut app, MouseKind::Up(MouseButton::Left), hex_col - 3, row);
    let text = app.top_modal_text().unwrap();
    assert!(text.contains("picker=Window background color=#123456"), "{text}");
    find(&app, "Basic colors");
    press(&mut app, Key::Right); // Qt's basic colors, the second column's first: #aa0000
    assert!(app.top_modal_text().unwrap().contains("color=#aa0000"));
    press(&mut app, Key::Escape); // cancel: nothing changes
    assert_eq!(app.theme.colors.background, Rgba::hex(0x123456));
    assert_eq!(app.modal_titles(), ["Settings", "Edit color scheme"]);

    // Space on the focused swatch opens it too; the HTML field and OK take a color.
    assert!(app.top_modal_text().unwrap().contains("focus=Swatch(0)"));
    press(&mut app, Key::Space);
    for _ in 0..3 {
        press(&mut app, Key::Tab); // field, value, HTML
    }
    press_with(&mut app, Key::Char('a'), Mods::primary());
    type_text(&mut app, "#abcdef");
    press(&mut app, Key::Enter); // OK
    assert!(!app.top_modal_text().unwrap().contains("picker="));
    assert_eq!(app.theme.colors.background, Rgba::hex(0xabcdef), "previewed like a typed value");
    assert!(screen(&app).row_text(row).contains("#abcdef"));
}
