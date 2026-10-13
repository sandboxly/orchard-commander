//! The terminal panel below both sides (port of `ui/terminal_view.py`'s
//! `TerminalPanel` / `TerminalView` and the terminal parts of
//! `main_window.py`; manual chapter 11).
//!
//! * ⌘J / ⌃` (Ctrl+` elsewhere) shows the panel and focuses it; pressed in
//!   the terminal it hides it again. The height (`terminal_height`, plus the
//!   exact `terminal_rows`) and `terminal_visible` persist like 1.x.
//! * One shell per side ([`oc_term::Session`]), started lazily in that side's
//!   folder ~50 ms after the panel shows (so it starts once at its final
//!   size: zsh themes reprint their prompt on an early resize); the panel
//!   shows the active side's session, the other keeps running.
//! * While the terminal has the focus every key goes to the shell (Esc, Tab,
//!   F-keys, Ctrl+C) except the toggle and, on macOS, ⌘ shortcuts.
//! * Output only wakes the UI for the session on screen, coalesced to one
//!   redraw per wake-up; drawing copies the visible screen under the lock.
//! * Linked folders: see [`link`].

pub mod link;
pub mod view;

pub use oc_term::Session;

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_grid::Frame;
use oc_term::{Button, MouseAction, MouseInput, PointerEvent, SessionEvent};

use crate::app::{App, Outcome};
use crate::commands::{Command, Registry};
use crate::geom::Rect;
use crate::input::{Event, Key, KeyEvent, MouseButton, MouseEvent, MouseKind};
use crate::main_window;
use crate::menus::Menu;
use crate::widgets::DrawCtx;

/// The smallest terminal panel, border included (1.x: six lines).
pub const MIN_ROWS: u16 = 6;
/// Rows the panels keep at least while the terminal is shown.
const MIN_PANEL_ROWS: u16 = 6;
/// The shell starts once the layout has settled (see the module docs).
const START_DELAY: Duration = Duration::from_millis(50);
/// Resizes are applied once the size stopped changing for this long.
const RESIZE_DELAY: Duration = Duration::from_millis(60);
const NO_SIDE: usize = usize::MAX;

/// Settings keys (`extra`), as 1.x writes them; `terminal_rows` is new in 2.x.
pub const VISIBLE_KEY: &str = "terminal_visible";
pub const HEIGHT_KEY: &str = "terminal_height";
pub const ROWS_KEY: &str = "terminal_rows";

/// The terminal's app-wide state (`app.state::<Terminal>()`).
pub struct Terminal {
    pub visible: bool,
    /// The keyboard is in the terminal (else in the active side).
    pub focused: bool,
    /// Rows the panel takes when shown, border included (0: not chosen yet).
    pub rows: u16,
    sessions: [Option<Arc<Session>>; 2],
    /// Which start of a side's shell a session is (stale notifications are dropped).
    generations: [u64; 2],
    next_generation: u64,
    /// Where a side's shell starts (a `cd` asked for before it ran).
    starting_dirs: [Option<PathBuf>; 2],
    scheduled: [bool; 2],
    /// The side's shell ended (or couldn't start): Enter starts a new one.
    ended: [bool; 2],
    /// The side whose session is on screen (`NO_SIDE` while hidden).
    shown: usize,
    /// The same, for the sessions' threads: they wake the UI only for it.
    shown_shared: Arc<AtomicUsize>,
    mouse: MouseInput,
    /// A press started inside the terminal: moves and the release follow it outside.
    capture: bool,
    /// Dragging the top border to resize.
    dragging: bool,
    resize_timer: Option<u64>,
    clock: Instant,
    /// Reused for each frame's copy of the screen.
    snapshot: RefCell<oc_term::Snapshot>,
    link: link::Link,
    /// The sessions as the link thread sees them.
    watched: link::Watched,
    /// Tests: what was sent to the shells (instead of sending it).
    recorded: Option<Vec<Vec<u8>>>,
}

impl Default for Terminal {
    fn default() -> Self {
        Terminal {
            visible: false,
            focused: false,
            rows: 0,
            sessions: [None, None],
            generations: [0, 0],
            next_generation: 0,
            starting_dirs: [None, None],
            scheduled: [false, false],
            ended: [false, false],
            shown: NO_SIDE,
            shown_shared: Arc::new(AtomicUsize::new(NO_SIDE)),
            mouse: new_mouse(),
            capture: false,
            dragging: false,
            resize_timer: None,
            clock: Instant::now(),
            snapshot: RefCell::new(oc_term::Snapshot::default()),
            link: link::Link::default(),
            watched: link::Watched::default(),
            recorded: None,
        }
    }
}

impl Terminal {
    /// The panel's title: "Terminal · zsh · ◀ left".
    pub fn title(&self, side: usize) -> String {
        let shell = self.sessions.get(side).and_then(|s| s.as_ref()).map(|s| s.title()).unwrap_or_default();
        let arrow = match side {
            0 => "◀ left",
            1 => "right ▶",
            _ => "",
        };
        ["Terminal", shell.as_str(), arrow].iter().filter(|x| !x.is_empty()).copied().collect::<Vec<_>>().join(" · ")
    }

    pub fn session(&self, side: usize) -> Option<&Arc<Session>> {
        self.sessions.get(side).and_then(|s| s.as_ref())
    }
}

/// Mouse positions reach the terminal in cells: one "pixel" per cell.
fn new_mouse() -> MouseInput {
    MouseInput::new(1.0, 1.0)
}

pub fn register(reg: &mut Registry) {
    reg.add(
        Command::new("terminal.toggle", "Terminal", toggle)
            .help("Show the terminal (one shell per side) and type into it; inside it: hide it")
            // The Control key on every platform, as in VS Code; ⌘J for layouts where ` is a dead key.
            .mac_keys(&["Ctrl+`", "Primary+J"])
            .other_keys(&["Ctrl+`"])
            .menu(Menu::Commands, "terminal"),
    );
    reg.add(
        Command::new("terminal.panel_folder", "Terminal: go to panel folder", to_panel_folder)
            .help("Change the active side's shell to its panel's folder")
            .keys(&["Ctrl+Shift+`", "Ctrl+Shift+~"])
            .menu(Menu::Commands, "terminal"),
    );
    reg.on_start(restore);
    reg.on_location_changed(link::panel_moved);
    reg.on_resized(resized);
}

// ---------------------------------------------------------------------------
// Queries (drawing, main window, tests)
// ---------------------------------------------------------------------------

pub fn is_visible(app: &App) -> bool {
    app.try_state::<Terminal>().is_some_and(|t| t.visible)
}

/// Whether the keyboard is in the terminal (the panels then look inactive).
pub fn has_focus(app: &App) -> bool {
    app.try_state::<Terminal>().is_some_and(|t| t.visible && t.focused)
}

/// Side `side`'s shell, if it was started.
pub fn session(app: &App, side: usize) -> Option<Arc<Session>> {
    app.try_state::<Terminal>().and_then(|t| t.session(side).cloned())
}

/// Where side `side`'s shell was last seen (linked folders; `None` before the first look).
pub fn shell_dir(app: &App, side: usize) -> Option<PathBuf> {
    app.try_state::<Terminal>().and_then(|t| t.link.shell_dir(side).map(PathBuf::from))
}

/// The shell's grid size for the current layout: the panel inside its border.
pub fn grid_size(app: &App) -> (usize, usize) {
    let r = main_window::layout(app).bottom;
    ((r.w.saturating_sub(2) as usize).max(2), (r.h.saturating_sub(2) as usize).max(2))
}

/// Tests: record what would be sent to the shells instead of sending it.
#[doc(hidden)]
pub fn record_input(app: &mut App) {
    app.state::<Terminal>().recorded = Some(Vec::new());
}

/// Tests: what was recorded since the last call (see [`record_input`]).
#[doc(hidden)]
pub fn take_recorded(app: &mut App) -> Vec<Vec<u8>> {
    app.state::<Terminal>().recorded.as_mut().map(std::mem::take).unwrap_or_default()
}

pub fn draw(app: &App, f: &mut Frame, area: Rect, ctx: &DrawCtx) {
    if let Some(t) = app.try_state::<Terminal>()
        && t.visible
    {
        view::draw(app, t, f, area, ctx);
    }
}

// ---------------------------------------------------------------------------
// Showing and hiding
// ---------------------------------------------------------------------------

/// ⌘J / ⌃`: show the terminal and type into it; from inside it, hide it again.
pub fn toggle(app: &mut App) {
    let t = app.state::<Terminal>();
    if !t.visible {
        show(app, true);
    } else if t.focused {
        hide(app);
    } else {
        set_focus(app, true);
    }
    remember(app);
}

/// ⌃⇧`: change the active side's shell to its panel's folder.
pub fn to_panel_folder(app: &mut App) {
    if !is_visible(app) {
        show(app, true);
        remember(app);
    }
    let side = app.main.active;
    let folder = side_folder(app, side);
    run_in(app, side, folder);
    set_focus(app, true);
}

/// Restore 1.x's `terminal_visible`: shown, but the app starts in the panels.
fn restore(app: &mut App) {
    if app.settings.extra.get(VISIBLE_KEY).and_then(|v| v.as_bool()) == Some(true) {
        show(app, false);
    }
}

fn show(app: &mut App, focus: bool) {
    let wanted = wanted_rows(app);
    let rows = clamp_rows(app, wanted);
    let t = app.state::<Terminal>();
    t.visible = true;
    t.rows = wanted.max(MIN_ROWS); // kept while a small window can't show it all
    app.main.bottom_rows = rows;
    main_window::resized(app, 0, 0);
    ensure_side(app);
    if focus {
        set_focus(app, true);
    }
    app.redraw();
}

fn hide(app: &mut App) {
    set_focus(app, false);
    let t = app.state::<Terminal>();
    t.visible = false;
    t.shown = NO_SIDE;
    t.shown_shared.store(NO_SIDE, Ordering::SeqCst);
    t.dragging = false;
    t.capture = false;
    app.main.bottom_rows = 0;
    main_window::resized(app, 0, 0);
    app.redraw();
}

/// The height to show: as last time (this session), else the saved one, else a third of the window.
fn wanted_rows(app: &mut App) -> u16 {
    let rows = app.state::<Terminal>().rows;
    if rows > 0 {
        return rows;
    }
    let extra = &app.settings.extra;
    if let Some(rows) = extra.get(ROWS_KEY).and_then(|v| v.as_u64()) {
        return rows.min(u16::MAX as u64) as u16;
    }
    if let Some(px) = extra.get(HEIGHT_KEY).and_then(|v| v.as_f64()).filter(|px| *px > 0.0) {
        return (px / row_px(app)).round() as u16;
    }
    available_rows(app) / 3
}

/// Rows below the menu strip and above the F-key bar.
fn available_rows(app: &App) -> u16 {
    let l = main_window::layout(app);
    l.footer.y.saturating_sub(l.tabs[0].y)
}

fn clamp_rows(app: &App, rows: u16) -> u16 {
    let max = available_rows(app).saturating_sub(MIN_PANEL_ROWS).max(MIN_ROWS);
    rows.clamp(MIN_ROWS, max)
}

/// A row's height in logical pixels, as near as we know it (1.x saved the
/// height in pixels; the exact row count is kept in `terminal_rows`).
fn row_px(app: &App) -> f64 {
    let font = &app.theme.font;
    (font.size * oc_grid::px_per_pt() * 1.2 * font.line_spacing.max(0.5)) as f64
}

/// Save `terminal_visible` / the height when they changed.
fn remember(app: &mut App) {
    let visible = app.state::<Terminal>().visible;
    let rows = app.main.bottom_rows;
    let height = (rows as f64 * row_px(app)).round() as i64;
    let extra = &app.settings.extra;
    let same = extra.get(VISIBLE_KEY).and_then(|v| v.as_bool()) == Some(visible)
        && (!visible || extra.get(ROWS_KEY).and_then(|v| v.as_u64()) == Some(rows as u64));
    if same {
        return;
    }
    app.update_settings(move |s| {
        s.extra.insert(VISIBLE_KEY.into(), visible.into());
        if visible {
            s.extra.insert(HEIGHT_KEY.into(), height.into());
            s.extra.insert(ROWS_KEY.into(), (rows as u64).into());
        }
    });
}

/// The window's size changed: keep the panel's height possible, resize the shell soon.
fn resized(app: &mut App) {
    if !is_visible(app) {
        return;
    }
    let wanted = app.state::<Terminal>().rows;
    let rows = clamp_rows(app, wanted);
    if rows != app.main.bottom_rows {
        app.main.bottom_rows = rows;
        main_window::resized(app, 0, 0);
    }
    resize_soon(app);
}

fn resize_soon(app: &mut App) {
    if let Some(id) = app.state::<Terminal>().resize_timer.take() {
        app.cancel_timer(id);
    }
    let id = app.schedule(RESIZE_DELAY, |app| {
        app.state::<Terminal>().resize_timer = None;
        apply_size(app);
    });
    app.state::<Terminal>().resize_timer = Some(id);
}

/// Give the shown session the panel's size.
fn apply_size(app: &mut App) {
    if !is_visible(app) {
        return;
    }
    let (columns, lines) = grid_size(app);
    if let Some(s) = session(app, app.main.active) {
        s.resize(columns, lines);
    }
    app.redraw();
}

fn set_focus(app: &mut App, focused: bool) {
    let side = app.main.active;
    let t = app.state::<Terminal>();
    if t.focused == focused {
        return;
    }
    t.focused = focused;
    // Focus reports: vim reloads files changed meanwhile.
    let report = t.session(side).and_then(|s| s.lock().focus_report(focused));
    if let Some(bytes) = report {
        send(app, side, bytes.to_vec(), Input::Raw);
    }
    app.redraw();
}

/// The panel shows the active side's session: swap (and start it) when the active side changed.
pub fn ensure_side(app: &mut App) {
    if !is_visible(app) {
        return;
    }
    let side = app.main.active;
    let t = app.state::<Terminal>();
    if t.shown != side {
        t.shown = side;
        t.shown_shared.store(side, Ordering::SeqCst);
        t.mouse = new_mouse();
        t.capture = false;
        let resize = t.sessions[side].is_some();
        if resize {
            apply_size(app);
        }
        app.redraw();
    }
    if app.state::<Terminal>().sessions[side].is_none() {
        start_soon(app, side);
    }
}

/// The folder a side's shell belongs in: its panel's (nearest local) folder, else home.
pub fn side_folder(app: &App, side: usize) -> PathBuf {
    crate::panel::local_anchor(&app.main.sides[side].panel.location).unwrap_or_else(oc_local::home_dir)
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

fn start_soon(app: &mut App, side: usize) {
    let t = app.state::<Terminal>();
    if t.scheduled[side] {
        return;
    }
    t.scheduled[side] = true;
    app.schedule(START_DELAY, move |app| {
        let t = app.state::<Terminal>();
        t.scheduled[side] = false;
        if t.sessions[side].is_some() || !t.visible {
            return;
        }
        start(app, side);
    });
}

/// Start side `side`'s shell now, at the panel's size (spawned on a worker).
fn start(app: &mut App, side: usize) {
    let (columns, lines) = grid_size(app);
    let cwd = app.state::<Terminal>().starting_dirs[side].take().unwrap_or_else(|| side_folder(app, side));
    let configured = app.settings.terminal_shell.clone();
    let t = app.state::<Terminal>();
    t.next_generation += 1;
    let generation = t.next_generation;
    let notify = notifier(app, side, generation);
    let (session, failed) = match oc_term::shell_command(&configured) {
        Ok(argv) => (Session::new(argv, cwd, columns, lines, notify), None),
        Err(e) => (Session::new(vec![configured.clone()], cwd, columns, lines, notify), Some(format!("Cannot start {configured}: {e}\r\n"))),
    };
    let session = Arc::new(session);
    let t = app.state::<Terminal>();
    t.sessions[side] = Some(session.clone());
    t.generations[side] = generation;
    t.ended[side] = failed.is_some();
    t.link.reset(side);
    t.watched.set(side, generation, Some(session.clone()));
    link::watch(app);
    if let Some(message) = failed {
        session.feed(message.as_bytes());
    } else {
        // openpty + fork/exec: off the UI thread.
        let worker = session.clone();
        app.tasks.spawn(
            move || worker.start().err().map(|e| oc_term::start_failed_message(worker.argv(), &e)),
            move |app, failed: Option<String>| {
                if let Some(message) = failed {
                    session.feed(message.as_bytes());
                    let t = app.state::<Terminal>();
                    if t.generations[side] == generation {
                        t.ended[side] = true;
                    }
                }
            },
        );
    }
    app.redraw();
}

/// The session's callback: output wakes the UI (coalesced, only for the
/// session on screen); its end is announced in it.
fn notifier(app: &mut App, side: usize, generation: u64) -> oc_term::Notify {
    let tasks = app.tasks.clone();
    let shown = app.state::<Terminal>().shown_shared.clone();
    let pending = Arc::new(AtomicBool::new(false));
    Arc::new(move |ev| match ev {
        SessionEvent::Output => {
            if shown.load(Ordering::SeqCst) == side && !pending.swap(true, Ordering::SeqCst) {
                let pending = pending.clone();
                tasks.post(move |app| {
                    pending.store(false, Ordering::SeqCst);
                    if app.try_state::<Terminal>().is_some_and(|t| t.generations[side] == generation) {
                        app.redraw();
                    }
                });
            }
        }
        SessionEvent::Exited(status) => tasks.post(move |app| exited(app, side, generation, status)),
    })
}

fn exited(app: &mut App, side: usize, generation: u64, status: Option<i32>) {
    let t = app.state::<Terminal>();
    if t.generations[side] != generation {
        return;
    }
    t.ended[side] = true;
    if let Some(s) = t.session(side).cloned() {
        s.feed(oc_term::ended_message(&s.title(), status).as_bytes());
    }
    app.redraw();
}

/// Enter after the shell ended: a new one in its place.
fn restart(app: &mut App, side: usize) {
    if let Some(old) = app.state::<Terminal>().sessions[side].take() {
        old.close();
    }
    start(app, side);
}

/// Make `side`'s shell change to `folder` (typed for the user, as if they did),
/// or start there if it isn't running.
pub fn run_in(app: &mut App, side: usize, folder: PathBuf) {
    let t = app.state::<Terminal>();
    match t.session(side).cloned() {
        Some(s) if s.alive() => {
            let cd = oc_term::cd_command(s.argv(), &folder);
            send(app, side, cd.into_bytes(), Input::Typed);
        }
        _ => t.starting_dirs[side] = Some(folder),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Input {
    /// Keys and text: Enter counts as a command.
    Typed,
    /// Mouse and focus reports.
    Raw,
}

fn send(app: &mut App, side: usize, bytes: Vec<u8>, kind: Input) {
    let t = app.state::<Terminal>();
    if let Some(rec) = t.recorded.as_mut() {
        rec.push(bytes);
        return;
    }
    if let Some(s) = t.session(side) {
        match kind {
            Input::Typed => s.send_key(&bytes),
            Input::Raw => s.write_bytes(&bytes),
        }
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

/// Called by the main window before its own handling: the terminal takes
/// the keys while it has the focus, and the mouse over it.
pub fn pre_event(app: &mut App, ev: &Event) -> Outcome {
    if !is_visible(app) {
        return Outcome::Ignored;
    }
    // A native menu command may have switched sides since the last event.
    ensure_side(app);
    // After the window handled it: Tab and clicks may change the active side.
    // (Only for those: a queued `later` makes the app redraw, so not on every hover.)
    if matches!(ev, Event::Key(_)) || matches!(ev, Event::Mouse(m) if matches!(m.kind, MouseKind::Down(_) | MouseKind::DoubleClick(_))) {
        app.later(ensure_side);
    }
    let focused = app.state::<Terminal>().focused;
    match ev {
        Event::Mouse(m) => mouse(app, m),
        Event::Key(k) if focused => key(app, k),
        Event::Commit(text) if focused => {
            typed(app, text.as_bytes().to_vec());
            Outcome::Consumed
        }
        Event::Preedit(_) if focused => Outcome::Consumed,
        _ => Outcome::Ignored,
    }
}

/// The toggle key (⌃` / Ctrl+`, also with Shift): never for the shell.
fn is_toggle(k: &KeyEvent) -> bool {
    matches!(k.key, Key::Char('`') | Key::Char('~')) && k.mods.ctrl
}

fn is_copy_paste(k: &KeyEvent, letter: char) -> bool {
    if k.key != Key::Char(letter) {
        return false;
    }
    let m = k.mods;
    if cfg!(target_os = "macos") { m.logo && !m.ctrl && !m.alt && !m.shift } else { m.ctrl && m.shift && !m.alt && !m.logo }
}

fn key(app: &mut App, k: &KeyEvent) -> Outcome {
    if is_copy_paste(k, 'c') {
        copy(app);
        return Outcome::Consumed;
    }
    if is_copy_paste(k, 'v') {
        paste(app);
        return Outcome::Consumed;
    }
    if is_toggle(k) || (cfg!(target_os = "macos") && k.mods.logo) {
        // The app's: the toggle and ⌘ shortcuts (other ⌘ keys do nothing here).
        return if app.registry.for_key(k).is_some() { Outcome::Ignored } else { Outcome::Consumed };
    }
    let side = app.main.active;
    let Some(s) = session(app, side) else {
        return Outcome::Consumed;
    };
    if app.state::<Terminal>().ended[side] {
        if k.key == Key::Enter {
            restart(app, side);
        }
        return Outcome::Consumed;
    }
    let bytes = {
        let screen = s.lock();
        view::encode_key(k, screen.key_options())
    };
    if let Some(bytes) = bytes {
        typed(app, bytes);
    }
    Outcome::Consumed
}

/// Typing: the view returns to the bottom, the selection goes.
fn typed(app: &mut App, bytes: Vec<u8>) {
    let side = app.main.active;
    if let Some(s) = session(app, side) {
        s.lock().reset_view();
    }
    send(app, side, bytes, Input::Typed);
    app.redraw();
}

pub fn copy(app: &mut App) {
    let Some(s) = session(app, app.main.active) else { return };
    let text = s.lock().selected_text();
    if !text.is_empty() {
        app.clipboard.set_text(&text);
    }
}

pub fn paste(app: &mut App) {
    let Some(s) = session(app, app.main.active) else { return };
    let Some(text) = app.clipboard.get_text().filter(|t| !t.is_empty()) else { return };
    let bytes = s.lock().paste_bytes(&text).into_bytes();
    typed(app, bytes);
}

fn button(b: MouseButton) -> Button {
    match b {
        MouseButton::Left => Button::Left,
        MouseButton::Middle => Button::Middle,
        MouseButton::Right => Button::Right,
    }
}

fn mouse(app: &mut App, m: &MouseEvent) -> Outcome {
    let l = main_window::layout(app);
    let area = l.bottom;
    let inner = area.inset(1, 1);
    let (dragging, capture) = {
        let t = app.state::<Terminal>();
        (t.dragging, t.capture)
    };
    if dragging {
        match m.kind {
            MouseKind::Move => {
                let rows = clamp_rows(app, l.footer.y.saturating_sub(m.row));
                if rows != app.main.bottom_rows {
                    app.main.bottom_rows = rows;
                    app.state::<Terminal>().rows = rows;
                    main_window::resized(app, 0, 0);
                    app.redraw();
                }
            }
            MouseKind::Up(_) => {
                app.state::<Terminal>().dragging = false;
                remember(app);
                apply_size(app);
            }
            _ => {}
        }
        return Outcome::Consumed;
    }
    let pressed = matches!(m.kind, MouseKind::Down(_) | MouseKind::DoubleClick(_));
    if !area.contains(m.col, m.row) && !capture {
        if pressed {
            set_focus(app, false); // a click in the panels: work there again
        }
        return Outcome::Ignored;
    }
    if !capture && !inner.contains(m.col, m.row) {
        // The border; its top line is the one between the panels and the terminal.
        if m.row == area.y && matches!(m.kind, MouseKind::Down(MouseButton::Left)) {
            app.state::<Terminal>().dragging = true;
        }
        if pressed {
            set_focus(app, true);
        }
        return if m.kind == MouseKind::Move { Outcome::Ignored } else { Outcome::Consumed };
    }
    let side = app.main.active;
    let Some(s) = session(app, side) else {
        if pressed {
            set_focus(app, true);
        }
        return Outcome::Consumed;
    };
    let t = app.state::<Terminal>();
    let e = PointerEvent {
        x: m.col as f64 - inner.x as f64 + 0.5,
        y: m.row as f64 - inner.y as f64 + 0.5,
        mods: view::term_mods(m.mods),
        time_ms: t.clock.elapsed().as_millis() as u64,
    };
    let action = {
        let mut screen = s.lock();
        match m.kind {
            MouseKind::Down(b) | MouseKind::DoubleClick(b) => {
                t.capture = true;
                t.mouse.press(&mut screen, button(b), e)
            }
            MouseKind::Up(b) => {
                t.capture = false;
                t.mouse.release(&mut screen, button(b), e)
            }
            MouseKind::Move => t.mouse.motion(&mut screen, e),
            MouseKind::Scroll(lines) => t.mouse.wheel(&mut screen, 0, (lines * 120.0).round() as i32, e),
            // Sideways scrolling isn't passed on (as before it existed).
            MouseKind::HScroll(_) => MouseAction::None,
        }
    };
    if pressed {
        set_focus(app, true);
    }
    let redraw = matches!(action, MouseAction::Redraw);
    match action {
        MouseAction::None => {}
        MouseAction::Send(bytes) => send(app, side, bytes, Input::Raw),
        MouseAction::Redraw => app.redraw(),
        MouseAction::Paste => paste(app),
    }
    // Hovering (or motion reported to a program) changes nothing on screen: no redraw.
    if m.kind == MouseKind::Move && !redraw { Outcome::Ignored } else { Outcome::Consumed }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles() {
        let t = Terminal::default();
        assert_eq!(t.title(0), "Terminal · ◀ left");
        assert_eq!(t.title(1), "Terminal · right ▶");
    }

    #[test]
    fn toggle_keys() {
        let ctrl = crate::input::Mods { ctrl: true, ..Default::default() };
        assert!(is_toggle(&KeyEvent::new(Key::Char('`'), ctrl)));
        assert!(is_toggle(&KeyEvent::new(Key::Char('`'), crate::input::Mods { shift: true, ..ctrl })));
        assert!(!is_toggle(&KeyEvent::new(Key::Char('`'), Default::default())));
    }
}
