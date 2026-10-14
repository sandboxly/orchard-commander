//! The Settings window, the color scheme editor and the app chrome around
//! them, driven without a window (ports of 1.x's `test_appearance.py`
//! settings tests and the settings parts of `test_ui.py`).

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use oc_config::themes::{self, Appearance};
use oc_config::{ConfigPaths, Settings};
use oc_grid::{Frame, Rgba};
use oc_ui::testing::{headless_app, press, press_with, settle, type_text, wait_until};
use oc_ui::{App, AppConfig, Key, MAIN_WINDOW, Mods};

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

fn on_disk(e: &Env) -> Settings {
    Settings::load(&e.config.join("settings.json"))
}

fn screen_text(app: &App) -> String {
    let mut f = Frame::new(120, 40, Rgba::BLACK);
    app.draw(MAIN_WINDOW, &mut f, (8.0, 16.0));
    (0..40).map(|r| f.row_text(r)).collect::<Vec<_>>().join("\n")
}

fn open_settings(app: &mut App) {
    press_with(app, Key::Char(','), Mods::primary());
    assert_eq!(app.modal_titles(), ["Settings"]);
    assert!(app.top_modal_text().unwrap().contains("focus=Close"), "1.x: Close has the focus");
}

fn focus(app: &App) -> String {
    let text = app.top_modal_text().unwrap_or_default();
    text.split("focus=").nth(1).unwrap_or_default().split(' ').next().unwrap_or_default().to_owned()
}

/// Tab until the focus is `want` (as Debug-printed, e.g. "Ctl(Size)").
fn tab_to(app: &mut App, want: &str) {
    for _ in 0..40 {
        if focus(app) == want {
            return;
        }
        press(app, Key::Tab);
    }
    panic!("never reached {want}; at {}", focus(app));
}

fn wait_idle(app: &mut App) {
    wait_until(app, "background work", Duration::from_secs(20), |app| app.tasks.pending() == 0);
}

#[test]
fn settings_apply_live_and_persist_only_what_changed() {
    let e = env();
    let mut app = app(&e);
    open_settings(&mut app);
    let text = screen_text(&app);
    for expected in ["Appearance", "Preview", "Terminal", "Color scheme", "Typeface", "Line spacing", "Frames", "Sample", "Restore defaults"] {
        assert!(text.contains(expected), "{expected} missing:\n{text}");
    }

    // Another instance saves something meanwhile: it must survive our changes.
    let mut other = on_disk(&e);
    other.update(&e.config.join("settings.json"), |s| s.terminal_shell = "zsh".into()).unwrap();

    tab_to(&mut app, "Ctl(Scheme(Color))");
    press(&mut app, Key::Right); // monokai → dracula
    assert_eq!(app.theme.scheme.name, "dracula", "applied live");
    tab_to(&mut app, "Ctl(Size)");
    for _ in 0..4 {
        press(&mut app, Key::Right);
    }
    assert_eq!(app.theme.font.size, 17.0);
    tab_to(&mut app, "Ctl(Frames)");
    press(&mut app, Key::Enter); // the list opens…
    assert!(app.top_modal_text().unwrap().contains("list_open=true"));
    press(&mut app, Key::Down);
    press(&mut app, Key::Down);
    press(&mut app, Key::Enter); // …Double line
    assert_eq!(app.theme.frame, oc_grid::BoxStyle::Double);
    tab_to(&mut app, "Ctl(Spacing)");
    press(&mut app, Key::Right);
    assert_eq!(app.settings.line_spacing, Some(1.05));

    app.flush_settings();
    let saved = on_disk(&e);
    assert_eq!(
        (saved.font_size, saved.frame_style.as_str(), saved.color_scheme.as_str(), saved.line_spacing),
        (Some(17), "double", "dracula", Some(1.05))
    );
    assert_eq!(saved.terminal_shell, "zsh", "the other instance's setting is kept");

    // Following the system shows the light/dark rows instead of "Scheme".
    tab_to(&mut app, "Ctl(FollowSystem)");
    press(&mut app, Key::Space);
    let text = screen_text(&app);
    assert!(text.contains("Light mode") && text.contains("Dark mode") && !text.contains("Scheme "), "{text}");
    assert_eq!(app.theme.scheme.name, "monokai", "the dark scheme (default) in dark mode");
    app.set_system_dark(false);
    assert_eq!(app.theme.scheme.name, themes::DEFAULT_LIGHT_SCHEME, "the system went light");

    tab_to(&mut app, "Ctl(Restore)");
    press(&mut app, Key::Enter);
    assert_eq!(app.settings.appearance(), Appearance::default());
    assert_eq!(app.theme.font.size, themes::DEFAULT_FONT_SIZE as f32);
    app.flush_settings();
    assert_eq!(on_disk(&e).appearance(), Appearance::default());
    assert_eq!(on_disk(&e).font_size, None, "restored means \"not chosen\", like 1.x");

    press(&mut app, Key::Escape);
    assert!(app.modal_titles().is_empty());
}

#[test]
fn sections_preview_and_terminal_pages() {
    let e = env();
    let mut app = app(&e);
    open_settings(&mut app);
    tab_to(&mut app, "Sections");
    press(&mut app, Key::Down);
    assert!(app.top_modal_text().unwrap().contains("page=Preview"));
    let text = screen_text(&app);
    assert!(text.contains("[x] Start playing as soon as a file is selected (autoplay)"), "{text}");
    assert!(text.contains("[x] Play the next file when one ends"));
    press(&mut app, Key::Tab);
    assert_eq!(focus(&app), "Ctl(Autoplay)");
    assert!(screen_text(&app).contains("Also in the player: the [x] Autoplay button, or the A key"), "the tooltip as a hint");
    press(&mut app, Key::Space);
    press(&mut app, Key::Down);
    press(&mut app, Key::Space);
    assert!(!app.settings.autoplay_enabled() && !app.settings.play_next_enabled());

    tab_to(&mut app, "Sections");
    press(&mut app, Key::Down);
    assert!(app.top_modal_text().unwrap().contains("page=Terminal"));
    press(&mut app, Key::Tab);
    assert_eq!(focus(&app), "Ctl(Shell)");
    assert!(app.wants_text_input(MAIN_WINDOW));
    type_text(&mut app, "fish -l");
    assert_eq!(app.settings.terminal_shell, "", "saved when editing is finished");
    press(&mut app, Key::Down); // leaving the field finishes it
    assert_eq!(app.settings.terminal_shell, "fish -l");
    assert_eq!(focus(&app), "Ctl(Link)");
    press(&mut app, Key::Enter);
    assert!(!app.settings.terminal_link_enabled());

    app.flush_settings();
    let saved = on_disk(&e);
    assert_eq!(saved.extra.get("preview.media.autoplay").and_then(|v| v.as_bool()), Some(false));
    assert_eq!((saved.play_next, saved.terminal_link, saved.terminal_shell.as_str()), (Some(false), Some(false), "fish -l"));
}

#[test]
fn clicking_controls() {
    let e = env();
    let mut app = app(&e);
    open_settings(&mut app);
    let click = |app: &mut App, needle: &str, kind: oc_ui::MouseKind| {
        let mut f = Frame::new(120, 40, Rgba::BLACK);
        app.draw(MAIN_WINDOW, &mut f, (8.0, 16.0));
        let (row, col) = (0..40)
            .find_map(|r| f.row_text(r).find(needle).map(|i| (r, f.row_text(r)[..i].chars().count() as u16)))
            .unwrap_or_else(|| panic!("{needle} not on screen"));
        app.handle_event(MAIN_WINDOW, oc_ui::Event::Mouse(oc_ui::MouseEvent { kind, col: col + 1, row, mods: Mods::NONE }));
    };
    use oc_ui::{MouseButton, MouseKind};
    click(&mut app, "Always show the menu bar", MouseKind::Down(MouseButton::Left));
    assert_eq!(app.settings.menu_bar_visible, Some(true));
    click(&mut app, "Terminal", MouseKind::Down(MouseButton::Left));
    assert!(app.top_modal_text().unwrap().contains("page=Terminal"));
    click(&mut app, "Close", MouseKind::Up(MouseButton::Left));
    assert!(app.modal_titles().is_empty());
}

#[test]
fn scheme_editor_saves_a_builtin_as_a_copy_then_deletes_it() {
    let e = env();
    let mut app = app(&e);
    app.update_settings(|s| s.color_scheme = "nord".into());
    open_settings(&mut app);
    tab_to(&mut app, "Ctl(Edit(Color))");
    press(&mut app, Key::Enter);
    assert_eq!(app.modal_titles(), ["Settings", "Edit color scheme"]);
    assert!(app.top_modal_text().unwrap().starts_with("nord (custom)\n"));
    assert!(screen_text(&app).contains("'nord' is built in: your changes are saved as a new color scheme"));
    // To "Directories": name, dark, syntax, then the colors in role order.
    for _ in 0..13 {
        press(&mut app, Key::Down);
    }
    assert!(app.top_modal_text().unwrap().contains("focus=Hex(10)"));
    press_with(&mut app, Key::Char('a'), Mods::primary());
    type_text(&mut app, "#ABCDEF");
    assert_eq!(app.theme.colors.dir, Rgba::hex(0xabcdef), "live preview");
    press(&mut app, Key::Enter); // Save
    wait_until(&mut app, "the editor to close", Duration::from_secs(10), |app| app.modal_titles() == ["Settings"]);
    assert_eq!(app.settings.color_scheme, "nord (custom)");
    assert_eq!(app.theme.scheme.name, "nord (custom)");
    assert_eq!(app.theme.colors.dir, Rgba::hex(0xabcdef));
    let file = e.config.join("color-schemes").join("nord-custom.toml");
    assert!(fs::read_to_string(&file).unwrap().contains("\"#abcdef\""));
    assert_ne!(themes::builtin_scheme("nord").unwrap().dir, "#abcdef");
    assert!(screen_text(&app).contains("nord (custom)"), "the list shows the copy");

    // Edit it again: a user scheme can be deleted.
    press(&mut app, Key::Enter);
    assert_eq!(app.modal_titles(), ["Settings", "Edit color scheme"]);
    assert!(!screen_text(&app).contains("is built in"));
    press(&mut app, Key::Up); // wraps to the buttons: Save
    press(&mut app, Key::Right); // Delete
    press(&mut app, Key::Enter);
    assert_eq!(app.modal_titles().last().map(String::as_str), Some("Delete color scheme"));
    press(&mut app, Key::Enter);
    wait_until(&mut app, "the editor to close", Duration::from_secs(10), |app| app.modal_titles() == ["Settings"]);
    assert!(!file.exists());
    assert_eq!(app.settings.color_scheme, "", "whatever used it goes back to the default");
    assert_eq!(app.theme.scheme.name, themes::DEFAULT_SCHEME);
}

#[test]
fn scheme_editor_cancel_restores_colors_and_reports_errors() {
    let e = env();
    let mut app = app(&e);
    open_settings(&mut app);
    tab_to(&mut app, "Ctl(Edit(Color))");
    press(&mut app, Key::Enter);
    let before = app.theme.clone();
    for _ in 0..4 {
        press(&mut app, Key::Down); // Panel background
    }
    press_with(&mut app, Key::Char('a'), Mods::primary());
    type_text(&mut app, "#ffffff");
    assert_eq!(app.theme.colors.surface, Rgba::hex(0xffffff));
    press(&mut app, Key::Escape);
    assert_eq!(app.modal_titles(), ["Settings"]);
    assert_eq!(app.theme, before, "cancel restores the scheme in use");

    // An empty name can't be saved: the editor says why and stays open.
    press(&mut app, Key::Enter);
    press_with(&mut app, Key::Char('a'), Mods::primary());
    press(&mut app, Key::Backspace);
    press(&mut app, Key::Enter);
    wait_idle(&mut app);
    assert_eq!(app.modal_titles(), ["Settings", "Edit color scheme"]);
    assert!(screen_text(&app).contains("Enter a name for the color scheme"));
}

#[test]
fn zoom_uses_the_scheme_cache_not_the_disk() {
    let e = env();
    let dir = e.config.join("color-schemes");
    let mine = themes::get_scheme(&dir, "nord").renamed("Mine");
    themes::save_user_scheme(&dir, &mine, None).unwrap();
    Settings { color_scheme: "Mine".into(), ..Default::default() }.save(&e.config.join("settings.json")).unwrap();
    let mut app = app(&e);
    assert_eq!(app.theme.scheme.name, "Mine", "a user scheme is there for the first frame");
    fs::remove_dir_all(&dir).unwrap();
    press_with(&mut app, Key::Char('='), Mods::primary());
    assert_eq!(app.theme.font.size, (themes::DEFAULT_FONT_SIZE + 1) as f32);
    assert_eq!(app.theme.scheme.name, "Mine", "zooming didn't re-read the folder");
}

#[test]
fn menu_clicks_under_a_dialog_do_nothing_but_key_equivalents_reach_it() {
    let e = env();
    let mut app = app(&e);
    app.menu_activated("app.settings");
    assert_eq!(app.modal_titles(), ["Settings"]);
    app.menu_activated("app.palette"); // a click on Commands → Command palette
    app.menu_activated("files.mkdir");
    assert_eq!(app.modal_titles(), ["Settings"], "no palette, no F7 sent to the dialog");
    // ⌘, pressed again: macOS hands it to the menu; it goes to the dialog as a key (ignored there).
    app.menu_key_equivalent("app.settings");
    assert_eq!(app.modal_titles(), ["Settings"]);
    app.menu_key_equivalent("app.palette");
    assert_eq!(app.modal_titles(), ["Settings"]);
    // In the in-window Options menu, Settings… comes first (1.x order).
    let options = oc_ui::menus::menu_items(&app, oc_ui::menus::Menu::Options);
    assert_eq!(options[..2], [Some("app.settings"), Some("app.color_scheme")]);
    let cmd = app.registry.get("app.settings").unwrap();
    assert_eq!(cmd.role, oc_ui::MenuRole::Preferences);
}

#[test]
fn small_windows_draw_without_panicking() {
    let e = env();
    let mut app = app(&e);
    open_settings(&mut app);
    tab_to(&mut app, "Ctl(Scheme(Color))");
    press(&mut app, Key::Enter); // with the list open
    for (cols, rows) in [(120, 40), (80, 24), (40, 10), (12, 4), (1, 1)] {
        app.resize(MAIN_WINDOW, cols, rows);
        let mut f = Frame::new(cols, rows, Rgba::BLACK);
        app.draw(MAIN_WINDOW, &mut f, (8.0, 16.0));
    }
    press(&mut app, Key::Escape);
    app.resize(MAIN_WINDOW, 120, 40);
    tab_to(&mut app, "Ctl(Edit(Color))");
    press(&mut app, Key::Enter);
    for (cols, rows) in [(80, 24), (40, 10), (12, 4), (1, 1)] {
        app.resize(MAIN_WINDOW, cols, rows);
        let mut f = Frame::new(cols, rows, Rgba::BLACK);
        app.draw(MAIN_WINDOW, &mut f, (8.0, 16.0));
        press(&mut app, Key::Down);
    }
}

#[test]
fn a_missing_saved_folder_falls_back_without_forgetting_it() {
    let e = env();
    let gone = e.left.join("unmounted");
    Settings { left_path: gone.to_string_lossy().into_owned(), right_path: e.right.to_string_lossy().into_owned(), ..Default::default() }
        .save(&e.config.join("settings.json"))
        .unwrap();
    let mut cfg = AppConfig::new(ConfigPaths::new(&e.config), Arc::new(|| {}));
    cfg.clipboard = Box::new(oc_ui::clipboard::MemoryClipboard::default());
    cfg.watch = false;
    let mut app = App::new(cfg);
    settle(&mut app);
    let cwd = oc_local::local(std::env::current_dir().unwrap());
    assert_eq!(app.main.sides[0].panel.location, cwd, "left: the current folder, like 1.x");
    assert_eq!(app.main.sides[1].panel.location, oc_local::local(&e.right), "an existing saved folder is used");
    app.flush_settings();
    assert_eq!(on_disk(&e).left_path, gone.to_string_lossy(), "kept for when the mount is back");
}

#[test]
fn narrow_and_short_windows_keep_everything_inside_the_dialogs() {
    let e = env();
    let mut app = app(&e);
    open_settings(&mut app);
    let draw = |app: &App, cols: u16, rows: u16| {
        let mut f = Frame::new(cols, rows, Rgba::BLACK);
        app.draw(MAIN_WINDOW, &mut f, (8.0, 16.0));
        (0..rows).map(|r| f.row_text(r)).collect::<Vec<_>>()
    };
    // Too narrow for the Sample next to the form: it is left out instead of
    // being drawn over the controls.
    app.resize(MAIN_WINDOW, 70, 20);
    let text = draw(&app, 70, 20).join("\n");
    assert!(!text.contains("Sample") && !text.contains("README.md"), "{text}");
    assert!(text.contains("Typeface") && text.contains("Restore defaults"), "{text}");

    // The scheme editor in a short window: color rows that don't fit are
    // left out, not drawn below the buttons and over the dialog's frame.
    app.resize(MAIN_WINDOW, 120, 40);
    tab_to(&mut app, "Ctl(Edit(Color))");
    press(&mut app, Key::Enter);
    app.resize(MAIN_WINDOW, 60, 20);
    let rows = draw(&app, 60, 20);
    let buttons = rows.iter().position(|r| r.contains("Save") && r.contains("Cancel")).expect("buttons shown");
    assert!(rows[buttons..].iter().all(|r| !r.contains('#')), "no color rows at or below the buttons:\n{}", rows.join("\n"));
    assert!(!rows[buttons].contains("Key bar"));
    let syntax = rows.iter().find(|r| r.contains("Syntax colors")).unwrap();
    assert!(syntax.contains("[x] Dark scheme"), "the syntax list doesn't cover the checkbox: {syntax}");
}
