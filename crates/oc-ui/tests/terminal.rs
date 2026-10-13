//! The terminal panel driven without a window (port of test_terminal.py's
//! window tests): sessions per side, keys, mouse, linked folders, height.
//! The shell is /bin/sh: fast, no user profile.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use oc_ui::features::terminal::{self, grid_size, session};
use oc_ui::main_window::layout;
use oc_ui::testing::{headless_app, press, press_with, settle, type_text, wait_until};
use oc_ui::{App, Event, Key, MAIN_WINDOW, Mods, MouseButton, MouseEvent, MouseKind};

const WAIT: Duration = Duration::from_secs(20);

struct Env {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    src: PathBuf,
    dst: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let (config, src, dst) = (base.join("config"), base.join("src"), base.join("dst"));
    for d in [&config, &src.join("sub"), &dst] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(src.join("a.txt"), "a").unwrap();
    fs::write(src.join("b.bin"), "bb").unwrap();
    Env { _tmp: tmp, config, src, dst }
}

fn app(e: &Env) -> App {
    let mut app = headless_app(&e.config, &e.src, &e.dst);
    app.settings.terminal_shell = "/bin/sh".into();
    app.resize(MAIN_WINDOW, 120, 40);
    settle(&mut app);
    app
}

/// Wait until side `side`'s shell runs.
fn started(app: &mut App, side: usize) -> Arc<oc_ui::features::terminal::Session> {
    wait_until(app, "the shell", WAIT, |app| session(app, side).is_some_and(|s| s.alive()));
    session(app, side).unwrap()
}

fn text(app: &App, side: usize) -> String {
    session(app, side).map(|s| s.lock().text()).unwrap_or_default()
}

fn wait_text(app: &mut App, side: usize, what: &str) {
    let w = what.to_owned();
    wait_until(app, &format!("{what:?} in the terminal"), WAIT, move |app| text(app, side).contains(&w));
}

/// Type a command line into the focused terminal.
fn run(app: &mut App, line: &str) {
    type_text(app, line);
    press(app, Key::Enter);
}

fn mouse(app: &mut App, kind: MouseKind, col: u16, row: u16, mods: Mods) {
    app.handle_event(MAIN_WINDOW, Event::Mouse(MouseEvent { kind, col, row, mods }));
}

fn click(app: &mut App, col: u16, row: u16) {
    mouse(app, MouseKind::Down(MouseButton::Left), col, row, Mods::NONE);
    mouse(app, MouseKind::Up(MouseButton::Left), col, row, Mods::NONE);
}

/// Click an empty row of a side's panel (the keyboard goes back to the panels).
fn click_panel(app: &mut App, side: usize) {
    let f = layout(app).frames[side];
    click(app, f.x + 3, f.bottom() - 3);
}

/// The terminal's text area: (column, line) → window cell.
fn cell(app: &App, column: u16, line: u16) -> (u16, u16) {
    let b = layout(app).bottom;
    (b.x + 1 + column, b.y + 1 + line)
}

fn real(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap()
}

fn shell_dir(s: &oc_ui::features::terminal::Session) -> Option<PathBuf> {
    s.current_dir().and_then(|d| fs::canonicalize(d).ok())
}

fn toggle(app: &mut App) {
    app.run_command("terminal.toggle");
}

#[test]
fn one_shell_per_side() {
    let e = env();
    let mut app = app(&e);
    toggle(&mut app);
    assert!(terminal::is_visible(&app) && terminal::has_focus(&app));
    assert!(app.main.bottom_rows >= terminal::MIN_ROWS);
    let left = started(&mut app, 0);
    run(&mut app, "pwd; echo LEFT-$((1+1))"); // wait for the output, not the echoed command line
    wait_text(&mut app, 0, "LEFT-2");
    assert!(text(&app, 0).replace('\n', "").contains(&e.src.to_string_lossy().into_owned()));

    click_panel(&mut app, 1); // switching the active side swaps the session shown
    assert_eq!(app.main.active, 1);
    assert!(!terminal::has_focus(&app) && terminal::is_visible(&app));
    toggle(&mut app); // visible, but a panel had the focus: focus the terminal
    assert!(terminal::has_focus(&app));
    let right = started(&mut app, 1);
    assert!(!Arc::ptr_eq(&left, &right));
    run(&mut app, "echo RIGHT-$((1+1))");
    wait_text(&mut app, 1, "RIGHT-2");
    assert!(!text(&app, 0).contains("RIGHT-") && left.alive()); // the other keeps running
}

#[test]
fn keys_go_to_the_shell_not_the_window() {
    let e = env();
    let mut app = app(&e);
    toggle(&mut app);
    started(&mut app, 0);
    terminal::record_input(&mut app);
    for k in [Key::Escape, Key::Tab, Key::F(5)] {
        // Stop search, switch panel, Copy
        press(&mut app, k);
    }
    press_with(&mut app, Key::Char('c'), Mods { ctrl: true, ..Mods::NONE });
    assert_eq!(terminal::take_recorded(&mut app), vec![b"\x1b".to_vec(), b"\t".to_vec(), b"\x1b[15~".to_vec(), b"\x03".to_vec()]);
    assert!(app.modal_titles().is_empty() && app.main.active == 0 && terminal::has_focus(&app));
    // ⌃` stays with the app: it hides the terminal from inside.
    press_with(&mut app, Key::Char('`'), Mods { ctrl: true, ..Mods::NONE });
    assert!(!terminal::is_visible(&app) && app.main.bottom_rows == 0);
    assert!(terminal::take_recorded(&mut app).is_empty());
}

#[cfg(target_os = "macos")]
#[test]
fn cmd_j_toggles_from_inside() {
    let e = env();
    let mut app = app(&e);
    press_with(&mut app, Key::Char('j'), Mods::primary()); // ⌘J
    assert!(terminal::is_visible(&app) && terminal::has_focus(&app));
    press_with(&mut app, Key::Char('j'), Mods::primary());
    assert!(!terminal::is_visible(&app) && !terminal::has_focus(&app));
}

#[test]
fn toggle_and_go_to_panel_folder() {
    let e = env();
    let mut app = app(&e);
    app.update_settings(|s| s.terminal_link = Some(false)); // only the command moves the shell here
    toggle(&mut app);
    let s = started(&mut app, 0);
    click_panel(&mut app, 0);
    assert!(!terminal::has_focus(&app));
    oc_ui::panel::load(&mut app, 0, oc_local::local(e.src.join("sub")), Default::default());
    settle(&mut app);
    app.run_command("terminal.panel_folder");
    assert!(terminal::has_focus(&app));
    wait_until(&mut app, "the shell in sub", WAIT, |_| shell_dir(&s) == Some(real(&e.src.join("sub"))));
    toggle(&mut app); // from inside: hide, back to the panels
    assert!(!terminal::is_visible(&app) && !terminal::has_focus(&app));
}

#[test]
fn restart_after_exit() {
    let e = env();
    let mut app = app(&e);
    toggle(&mut app);
    let first = started(&mut app, 0);
    run(&mut app, "exit");
    wait_text(&mut app, 0, "press Enter for a new one");
    assert!(text(&app, 0).contains("[sh ended \u{2014} press Enter for a new one]"));
    press(&mut app, Key::Enter);
    let second = started(&mut app, 0);
    assert!(!Arc::ptr_eq(&first, &second) && second.alive());
}

#[test]
fn shell_starts_once_at_its_final_size() {
    let e = env();
    let mut app = app(&e);
    toggle(&mut app);
    let s = started(&mut app, 0);
    let size = {
        let screen = s.lock();
        (screen.columns(), screen.lines())
    };
    assert_eq!(size, grid_size(&app)); // no resize right after starting (it'd redraw prompts)
}

#[test]
fn clicks_drags_and_wheel_go_to_the_program() {
    let e = env();
    let mut app = app(&e);
    toggle(&mut app);
    let s = started(&mut app, 0);
    s.feed(b"\x1b[?1002h\x1b[?1006h"); // what vim/htop send to ask for the mouse
    terminal::record_input(&mut app);
    let (x, y) = cell(&app, 2, 1);
    mouse(&mut app, MouseKind::Down(MouseButton::Left), x, y, Mods::NONE);
    mouse(&mut app, MouseKind::Move, x + 3, y, Mods::NONE);
    mouse(&mut app, MouseKind::Up(MouseButton::Left), x + 3, y, Mods::NONE);
    assert_eq!(terminal::take_recorded(&mut app), vec![b"\x1b[<0;3;2M".to_vec(), b"\x1b[<32;6;2M".to_vec(), b"\x1b[<0;6;2m".to_vec()]);
    assert!(!s.lock().has_selection()); // the program got it: no local selection
    let (x, y) = cell(&app, 1, 1);
    mouse(&mut app, MouseKind::Scroll(-1.0), x, y, Mods::NONE); // one notch down: three steps
    assert_eq!(terminal::take_recorded(&mut app), vec![b"\x1b[<65;2;2M".repeat(3)]);
}

#[test]
fn shift_selects_even_when_the_program_has_the_mouse() {
    let e = env();
    let mut app = app(&e);
    toggle(&mut app);
    let s = started(&mut app, 0);
    s.feed(b"\x1b[2J\x1b[Hhello world\r\n\x1b[?1000h\x1b[?1006h");
    terminal::record_input(&mut app);
    let (x, y) = cell(&app, 0, 0);
    mouse(&mut app, MouseKind::Down(MouseButton::Left), x, y, Mods::SHIFT);
    mouse(&mut app, MouseKind::Move, x + 5, y, Mods::SHIFT);
    mouse(&mut app, MouseKind::Up(MouseButton::Left), x + 5, y, Mods::SHIFT);
    assert!(terminal::take_recorded(&mut app).is_empty());
    assert_eq!(s.lock().selected_text(), "hello");
}

#[test]
fn double_and_triple_click_select_and_copy() {
    let e = env();
    let mut app = app(&e);
    toggle(&mut app);
    let s = started(&mut app, 0);
    s.feed(b"\x1b[?25l\x1b[2J\x1b[Hls ~/src/project.txt done\r\n");
    terminal::record_input(&mut app);
    let (x, y) = cell(&app, 8, 0);
    click(&mut app, x, y);
    click(&mut app, x, y); // the app reports it as a double click
    assert_eq!(s.lock().selected_text(), "~/src/project.txt"); // a path counts as one word
    let copy = if cfg!(target_os = "macos") { Mods::primary() } else { Mods { ctrl: true, shift: true, ..Mods::NONE } };
    press_with(&mut app, Key::Char('c'), copy);
    assert_eq!(app.clipboard.get_text().as_deref(), Some("~/src/project.txt"));
    click(&mut app, x, y); // third click: the line
    assert_eq!(s.lock().selected_text().trim(), "ls ~/src/project.txt done");
    assert!(terminal::take_recorded(&mut app).is_empty());

    // Paste: bracketed when the shell asked for it; typing drops the selection.
    s.feed(b"\x1b[?2004h");
    app.clipboard.set_text("echo 1\necho 2");
    press_with(&mut app, Key::Char('v'), copy);
    assert_eq!(terminal::take_recorded(&mut app), vec![b"\x1b[200~echo 1\recho 2\x1b[201~".to_vec()]);
    assert!(!s.lock().has_selection());
}

#[test]
fn focus_reports() {
    let e = env();
    let mut app = app(&e);
    toggle(&mut app);
    let s = started(&mut app, 0);
    s.feed(b"\x1b[?1004h");
    terminal::record_input(&mut app);
    click_panel(&mut app, 0);
    toggle(&mut app);
    assert_eq!(terminal::take_recorded(&mut app), vec![b"\x1b[O".to_vec(), b"\x1b[I".to_vec()]);
}

#[test]
fn dragging_the_top_border_resizes_and_is_remembered() {
    let e = env();
    let mut app = app(&e);
    toggle(&mut app);
    let s = started(&mut app, 0);
    let before = app.main.bottom_rows;
    let top = layout(&app).bottom.y;
    mouse(&mut app, MouseKind::Down(MouseButton::Left), 60, top, Mods::NONE);
    mouse(&mut app, MouseKind::Move, 60, top - 4, Mods::NONE);
    mouse(&mut app, MouseKind::Up(MouseButton::Left), 60, top - 4, Mods::NONE);
    assert_eq!(app.main.bottom_rows, before + 4);
    assert_eq!(s.lock().lines(), grid_size(&app).1);
    assert_eq!(app.settings.extra.get("terminal_rows").and_then(|v| v.as_u64()), Some((before + 4) as u64));
    assert!(app.settings.extra.get("terminal_height").and_then(|v| v.as_i64()).is_some_and(|px| px > 0));
    assert_eq!(app.settings.extra.get("terminal_visible").and_then(|v| v.as_bool()), Some(true));
    app.flush_settings();
    drop(app);

    // The next start shows it again at that height, with the keyboard in the panels.
    let mut again = headless_app(&e.config, &e.src, &e.dst);
    again.settings.terminal_shell = "/bin/sh".into();
    again.resize(MAIN_WINDOW, 120, 40);
    assert!(terminal::is_visible(&again) && !terminal::has_focus(&again));
    oc_ui::testing::wait_until(&mut again, "rows", WAIT, |a| a.main.bottom_rows == before + 4);
}

// -- linked folders -------------------------------------------------------------------

fn linked(e: &Env) -> (App, Arc<oc_ui::features::terminal::Session>) {
    let mut app = app(e);
    toggle(&mut app);
    let s = started(&mut app, 0);
    wait_until(&mut app, "the shell's folder", WAIT, |app| s.at_prompt() && terminal::shell_dir(app, 0).is_some());
    (app, s)
}

fn panel_at(app: &App, side: usize) -> PathBuf {
    PathBuf::from(&app.main.sides[side].panel.location.path)
}

#[test]
fn cd_in_the_terminal_moves_the_panel() {
    let e = env();
    let (mut app, _s) = linked(&e);
    run(&mut app, "cd sub");
    wait_until(&mut app, "the panel in sub", WAIT, |app| panel_at(app, 0) == e.src.join("sub"));
    run(&mut app, "cd ..");
    wait_until(&mut app, "the panel back", WAIT, |app| panel_at(app, 0) == e.src);
    assert_eq!(panel_at(&app, 1), e.dst); // the other side keeps its own
}

#[test]
fn moving_the_panel_moves_the_shell() {
    let e = env();
    let (mut app, s) = linked(&e);
    oc_ui::panel::load(&mut app, 0, oc_local::local(&e.dst), Default::default());
    wait_until(&mut app, "the shell in dst", WAIT, |_| shell_dir(&s) == Some(real(&e.dst)));
    wait_until(&mut app, "the link to see it", WAIT, |app| terminal::shell_dir(app, 0).is_some_and(|d| real(&d) == real(&e.dst)));
    settle(&mut app);
    assert_eq!(panel_at(&app, 0), e.dst); // and it stays there (no ping-pong)
}

#[test]
fn no_typing_into_a_running_program() {
    let e = env();
    let (mut app, s) = linked(&e);
    run(&mut app, "sleep 1.5; echo SLEPT");
    wait_until(&mut app, "sleep to run", WAIT, |_| !s.at_prompt());
    oc_ui::panel::load(&mut app, 0, oc_local::local(e.src.join("sub")), Default::default());
    settle(&mut app);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(shell_dir(&s), Some(real(&e.src))); // sleep is still running: nothing typed
    wait_text(&mut app, 0, "SLEPT");
    wait_until(&mut app, "the shell to catch up", WAIT, |_| shell_dir(&s) == Some(real(&e.src.join("sub"))));
}

#[test]
fn panels_refresh_after_each_command() {
    let e = env();
    let (mut app, _s) = linked(&e);
    let names = |app: &App| app.main.sides[0].panel.entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
    run(&mut app, "rm a.txt");
    wait_until(&mut app, "a.txt gone", WAIT, |app| !names(app).contains(&"a.txt".to_string()));
    let size = |app: &App| app.main.sides[0].panel.entries.iter().find(|e| e.name == "b.bin").map(|e| e.size);
    let before = size(&app).unwrap();
    run(&mut app, "echo more >> b.bin"); // only a size changes
    wait_until(&mut app, "b.bin to grow", WAIT, |app| size(app) == Some(before + 5));
}

#[test]
fn link_can_be_switched_off() {
    let e = env();
    let (mut app, s) = linked(&e);
    app.update_settings(|s| s.terminal_link = Some(false));
    run(&mut app, "cd sub");
    wait_until(&mut app, "the shell in sub", WAIT, |_| shell_dir(&s) == Some(real(&e.src.join("sub"))));
    std::thread::sleep(Duration::from_millis(700));
    settle(&mut app);
    assert_eq!(panel_at(&app, 0), e.src);
}

#[test]
fn a_small_window_keeps_room_for_the_panels_and_the_height_comes_back() {
    let e = env();
    let mut app = app(&e);
    toggle(&mut app);
    let s = started(&mut app, 0);
    let rows = app.main.bottom_rows;
    app.resize(MAIN_WINDOW, 120, 16);
    assert!(app.main.bottom_rows < rows && app.main.bottom_rows >= terminal::MIN_ROWS);
    assert!(layout(&app).frames[0].h >= 5);
    wait_until(&mut app, "the shell resized", WAIT, |app| s.lock().lines() == grid_size(app).1);
    app.resize(MAIN_WINDOW, 120, 40);
    assert_eq!(app.main.bottom_rows, rows);
    wait_until(&mut app, "the shell resized back", WAIT, |app| s.lock().lines() == grid_size(app).1);
}

#[test]
fn hovering_over_the_terminal_does_not_redraw() {
    let e = env();
    let mut app = app(&e);
    toggle(&mut app);
    started(&mut app, 0);
    let (x, y) = cell(&app, 3, 1);
    let border = layout(&app).bottom.y;
    app.take_redraw();
    mouse(&mut app, MouseKind::Move, x, y, Mods::NONE);
    mouse(&mut app, MouseKind::Move, x + 1, y, Mods::NONE);
    mouse(&mut app, MouseKind::Move, x, border, Mods::NONE);
    let panel = layout(&app).frames[1];
    mouse(&mut app, MouseKind::Move, panel.x + 4, panel.y + 4, Mods::NONE); // over a panel, too
    assert!(!app.take_redraw());
    // A drag that selects does redraw.
    mouse(&mut app, MouseKind::Down(MouseButton::Left), x, y, Mods::NONE);
    app.take_redraw();
    mouse(&mut app, MouseKind::Move, x + 4, y, Mods::NONE);
    assert!(app.take_redraw());
    mouse(&mut app, MouseKind::Up(MouseButton::Left), x + 4, y, Mods::NONE);
}

#[test]
fn a_symlinked_panel_folder_keeps_its_spelling() {
    let e = env();
    let link = e.src.parent().unwrap().join("lnk");
    std::os::unix::fs::symlink(&e.src, &link).unwrap();
    let (mut app, s) = linked(&e);
    oc_ui::panel::load(&mut app, 0, oc_local::local(&link), Default::default());
    settle(&mut app);
    run(&mut app, "cd sub");
    wait_until(&mut app, "the panel in lnk/sub", WAIT, |app| panel_at(app, 0) == link.join("sub"));
    assert_eq!(shell_dir(&s), Some(real(&e.src.join("sub"))));
}
