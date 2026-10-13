//! The main window: two sides (file panel or a side view each), the
//! in-window menu strip, the F-key bar and toasts (port of `main_window.py`'s
//! layout and focus handling; its commands live in [`crate::features`]).
//!
//! ```text
//! ┌ menu strip (F9 / "menu_bar_visible") ───────────────────────────────┐
//! │ Files  Preview                    │ Files  Preview                  │  tabs
//! │╭ ~/path ─────────────────────────╮│╭ ~/other ──────────────────────╮│
//! ││ Name ▲          Size  Modified  │││                               ││  panels / views
//! │╰──────────── 3 dirs, 2 files ────╯│╰───────────────────────────────╯│
//! │ [bottom pane: terminal, later]                                      │
//! │ F3 Preview F5 Copy …                                 ⌘P Commands    │  F-key bar
//! └─────────────────────────────────────────────────────────────────────┘
//! ```

pub mod side;

use std::path::{Path, PathBuf};
use std::rc::Rc;

use oc_config::Settings;
use oc_grid::{Flags, Frame, Style};
use oc_vfs::{SortKey, VPath};

use crate::app::{App, MAIN_WINDOW, Outcome, Severity};
use crate::commands::{Command, CommandId, Registry};
use crate::geom::Rect;
use crate::input::{Event, Key, MouseButton, MouseKind};
use crate::menus::{self, Menu, MenuBar};
use crate::panel::{self, FilePanel, LoadOptions, PanelAction};
use crate::widgets::boxframe::{draw_tabs, tab_at};
use crate::widgets::{DrawCtx, Tab, fill, put, width};
pub use side::{Side, SideMode, SideView, with_view};

pub const SIDE_NAMES: [&str; 2] = ["left", "right"];

pub struct MainWindow {
    pub sides: [Side; 2],
    /// The side with the keyboard focus.
    pub active: usize,
    /// Rows reserved below the sides for a bottom pane (the terminal, later).
    pub bottom_rows: u16,
    /// Where a side goes if its saved folder can't be listed at start (1.x:
    /// the current folder on the left, home on the right). The check runs
    /// with the first listing, on a worker: a saved folder on a dead network
    /// mount must not hang the window.
    startup_fallback: [Option<PathBuf>; 2],
}

impl MainWindow {
    pub fn new(settings: &Settings, registry: &Rc<Registry>, left: Option<PathBuf>, right: Option<PathBuf>) -> Self {
        let saved = |p: &str| (!p.is_empty() && Path::new(p).is_absolute()).then(|| PathBuf::from(p));
        let cwd_or_home = || std::env::current_dir().unwrap_or_else(|_| oc_local::home_dir());
        let mut startup_fallback = [None, None];
        let left = match left {
            Some(l) => l,
            None => match saved(&settings.left_path) {
                Some(p) => {
                    startup_fallback[0] = Some(cwd_or_home());
                    p
                }
                None => cwd_or_home(),
            },
        };
        let right = match right {
            Some(r) => r,
            None => match saved(&settings.right_path) {
                Some(p) => {
                    startup_fallback[1] = Some(oc_local::home_dir());
                    p
                }
                None => oc_local::home_dir(),
            },
        };
        let hidden = settings.hidden_visible();
        let make = |i: usize, path: PathBuf| {
            let mut p = FilePanel::new(i, oc_local::local(path), hidden);
            let name = SIDE_NAMES[i];
            if let Some(key) = settings.extra.get(&format!("{name}_sort")).and_then(|v| v.as_str()).and_then(SortKey::parse) {
                p.sort = key;
            }
            p.reverse = settings.extra.get(&format!("{name}_sort_reverse")).and_then(|v| v.as_bool()).unwrap_or(false);
            Side::new(name, p, &registry.side_views)
        };
        MainWindow { sides: [make(0, left), make(1, right)], active: 0, bottom_rows: 0, startup_fallback }
    }

    /// The side file operations act on: the active one, or the other if it shows a view.
    pub fn files_side(&self) -> usize {
        if self.sides[self.active].mode == SideMode::Files { self.active } else { 1 - self.active }
    }

    pub fn active_panel(&self) -> &FilePanel {
        &self.sides[self.files_side()].panel
    }

    pub fn other_panel(&self) -> &FilePanel {
        &self.sides[1 - self.files_side()].panel
    }
}

/// Where everything is for a window of `cols` x `rows`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub strip: Option<Rect>,
    pub tabs: [Rect; 2],
    pub frames: [Rect; 2],
    pub bottom: Rect,
    pub footer: Rect,
}

pub fn layout(app: &App) -> Layout {
    let screen = app.screen(MAIN_WINDOW);
    let (rest, footer) = screen.split_bottom(1);
    let strip_shown = app.settings.menu_bar_visible.unwrap_or(false) || menu_open(app);
    let (strip, rest) = if strip_shown {
        let (s, r) = rest.split_top(1);
        (Some(s), r)
    } else {
        (None, rest)
    };
    let (sides, bottom) = rest.split_bottom(app.main.bottom_rows.min(rest.h.saturating_sub(4)));
    let (tabs, frames) = sides.split_top(1);
    let half = sides.w / 2;
    let (lt, rt) = tabs.split_left(half);
    let (lf, rf) = frames.split_left(half);
    Layout { strip, tabs: [lt, rt], frames: [lf, rf], bottom, footer }
}

fn menu_open(app: &App) -> bool {
    app.modal_titles().iter().any(|t| t == "menu")
}

/// Keep each panel's idea of its visible rows in step with the layout.
fn sync_rows(app: &mut App) {
    let l = layout(app);
    for i in 0..2 {
        let rows = panel::draw::layout(l.frames[i]).rows.h.max(1) as usize;
        let p = &mut app.main.sides[i].panel;
        if p.rows != rows {
            p.rows = rows;
            p.list.ensure_visible(rows);
        }
    }
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Start: list both panels, restore the sides' modes.
pub(crate) fn start(app: &mut App) {
    sync_rows(app);
    for side in 0..2 {
        let loc = app.main.sides[side].panel.location.clone();
        let fallback = app.main.startup_fallback[side].take().map(oc_local::local);
        panel::load(app, side, loc, LoadOptions { fallback, ..Default::default() });
    }
    for (side, name) in SIDE_NAMES.iter().enumerate() {
        let mode = app.settings.extra.get(&format!("{name}_mode")).and_then(|v| v.as_str()).map(str::to_owned);
        if let Some(i) = mode.and_then(|m| app.main.sides[side].view_index(&m)) {
            set_mode_inner(app, side, SideMode::View(i), false);
        }
    }
    app.main.active = 0;
    if app.main.sides[0].mode != SideMode::Files {
        app.main.active = 1;
    }
}

pub(crate) fn resized(app: &mut App, _cols: u16, _rows: u16) {
    sync_rows(app);
}

/// A panel went somewhere else: remember the folder, tell the views.
pub(crate) fn location_changed(app: &mut App, side: usize) {
    let loc = app.main.sides[side].panel.location.clone();
    if let Some(anchor) = panel::local_anchor(&loc) {
        let value = anchor.to_string_lossy().into_owned();
        let name = SIDE_NAMES[side];
        let current = if side == 0 { &app.settings.left_path } else { &app.settings.right_path };
        if *current != value {
            app.update_settings(move |s| {
                if name == "left" {
                    s.left_path = value.clone();
                } else {
                    s.right_path = value.clone();
                }
            });
        }
    }
}

/// Panel `side`'s cursor moved: the other side's view may follow it.
pub fn cursor_moved(app: &mut App, side: usize) {
    let other = 1 - side;
    if let SideMode::View(i) = app.main.sides[other].mode {
        let target = app.main.sides[side].panel.cursor_target();
        with_view(app, other, i, |v, app| {
            if v.follows_other_panel() {
                v.target_changed(app, other, target);
            }
        });
    }
}

/// Show `mode` on `side` (port of `MainWindow.set_mode`).
pub fn set_mode(app: &mut App, side: usize, mode: SideMode) {
    set_mode_inner(app, side, mode, true);
}

fn set_mode_inner(app: &mut App, side: usize, mode: SideMode, save: bool) {
    let other = 1 - side;
    let old = app.main.sides[side].mode;
    if let (SideMode::View(i), true) = (old, old != mode) {
        with_view(app, side, i, |v, app| v.deactivated(app, side));
    }
    match mode {
        SideMode::View(i) => {
            let follows = app.main.sides[side].views.get(i).and_then(|v| v.as_ref()).is_some_and(|v| v.follows_other_panel());
            if follows {
                if let SideMode::View(j) = app.main.sides[other].mode {
                    with_view(app, other, j, |v, app| v.deactivated(app, other));
                    app.main.sides[other].mode = SideMode::Files;
                }
                app.main.sides[side].mode = mode;
                app.main.active = other;
                let target = app.main.sides[other].panel.cursor_target();
                with_view(app, side, i, |v, app| v.target_changed(app, side, target));
            } else {
                app.main.sides[side].mode = mode;
                app.main.active = side;
            }
        }
        SideMode::Files => {
            app.main.sides[side].mode = SideMode::Files;
            app.main.active = side;
        }
    }
    if save {
        let modes = [app.main.sides[0].mode_id(), app.main.sides[1].mode_id()];
        app.update_settings(move |s| {
            s.extra.insert("left_mode".into(), modes[0].into());
            s.extra.insert("right_mode".into(), modes[1].into());
        });
    }
    app.redraw();
}

/// Switch `side` to the view with this id, or back to files if it shows it already.
pub fn toggle_view(app: &mut App, side: usize, id: &str) {
    let Some(i) = app.main.sides[side].view_index(id) else {
        return;
    };
    let mode = if app.main.sides[side].mode == SideMode::View(i) { SideMode::Files } else { SideMode::View(i) };
    set_mode(app, side, mode);
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

pub(crate) fn draw(app: &App, f: &mut Frame, ctx: &DrawCtx) {
    let l = layout(app);
    let main = &app.main;
    if let Some(strip) = l.strip {
        menus::draw_strip(f, strip, None, ctx);
    }
    for (i, side) in main.sides.iter().enumerate() {
        let active = main.active == i;
        let labels = side.tab_labels();
        let tabs: Vec<Tab> = labels.iter().enumerate().map(|(k, label)| Tab { label, selected: k == side.mode_index() }).collect();
        draw_tabs(f, l.tabs[i], &tabs, active, ctx);
        match side.mode {
            SideMode::Files => panel::draw::draw(&side.panel, f, l.frames[i], ctx, active),
            SideMode::View(_) => match side.view() {
                Some(v) => v.draw(app, i, f, l.frames[i], ctx, active),
                None => fill(f, l.frames[i], ctx.theme.colors.background),
            },
        }
    }
    draw_footer(app, f, l.footer, ctx);
}

/// F-key bar items: (cells, command), left part then right part.
fn footer_items(app: &App, row: Rect) -> Vec<(Rect, CommandId, String, String)> {
    let mut out = Vec::new();
    let mut x = row.x + 1;
    for cmd in app.registry.fkeys() {
        let (key, label) = cmd.fkey.expect("fkeys have one");
        let w = width(key) + 1 + width(label);
        if x + w > row.right() {
            break;
        }
        out.push((Rect::new(x, row.y, w, 1), cmd.id, key.to_owned(), label.to_owned()));
        x += w + 2;
    }
    let mut right = row.right().saturating_sub(1);
    let left_end = x;
    for cmd in app.registry.footer_right().into_iter().rev() {
        let key = cmd.shortcut_text();
        let label = cmd.footer_right.unwrap_or_default();
        let w = width(&key) + 1 + width(label);
        if right < left_end + w {
            break;
        }
        out.push((Rect::new(right - w, row.y, w, 1), cmd.id, key, label.to_owned()));
        right = right.saturating_sub(w + 2);
    }
    out
}

fn draw_footer(app: &App, f: &mut Frame, row: Rect, ctx: &DrawCtx) {
    let c = &ctx.theme.colors;
    fill(f, row, c.footer_bg);
    for (r, _, key, label) in footer_items(app, row) {
        let x = put(f, r.x, r.y, &key, Style::new(c.footer_key, c.footer_bg).with(Flags::BOLD), r.w);
        put(f, x + 1, r.y, &label, Style::new(c.footer_fg, c.footer_bg), r.right().saturating_sub(x + 1));
    }
}

/// The toast, bottom right above the F-key bar.
pub(crate) fn draw_toast(app: &App, f: &mut Frame, ctx: &DrawCtx) {
    let Some(t) = &app.toast else { return };
    let c = &ctx.theme.colors;
    let l = layout(app);
    let screen = app.screen(MAIN_WINDOW);
    let w = 60.min(screen.w.saturating_sub(4));
    if w < 10 {
        return;
    }
    let lines = crate::dialogs::wrap(&t.text, (w - 4) as usize);
    let h = (lines.len() as u16 + 2).min(screen.h.saturating_sub(2));
    let x = screen.right().saturating_sub(w + 2);
    let y = l.footer.y.saturating_sub(h + 1);
    let r = Rect::new(x, y, w, h);
    crate::widgets::cover(f, r, ctx);
    fill(f, r, c.footer_bg);
    let bar = match t.severity {
        Severity::Error => c.error,
        Severity::Warning => c.warning,
        Severity::Info => c.border_active,
    };
    for row in r.y..r.bottom() {
        f.put_char(r.x, row, '▌', Style::new(bar, c.footer_bg));
    }
    for (i, line) in lines.iter().enumerate().take(h.saturating_sub(2) as usize) {
        put(f, r.x + 2, r.y + 1 + i as u16, line, Style::new(c.foreground, c.footer_bg), w - 3);
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

pub(crate) fn handle_event(app: &mut App, ev: &Event) -> Outcome {
    sync_rows(app);
    match ev {
        Event::Key(k) => {
            if let Some(cmd) = app.registry.for_key(k) {
                let id = cmd.id;
                app.run_command(id);
                return Outcome::Consumed;
            }
            if matches!(k.key, Key::Tab) && (k.mods.is_empty() || k.mods == crate::input::Mods::SHIFT) {
                app.main.active = 1 - app.main.active;
                return Outcome::Consumed;
            }
            let side = app.main.active;
            match app.main.sides[side].mode {
                SideMode::Files => panel_key(app, side, k),
                SideMode::View(i) => view_event(app, side, i, ev),
            }
        }
        Event::Mouse(m) => mouse(app, m),
        Event::Preedit(_) | Event::Commit(_) => {
            let side = app.main.active;
            match app.main.sides[side].mode {
                SideMode::View(i) => view_event(app, side, i, ev),
                SideMode::Files => match ev {
                    Event::Commit(text) => panel_key(app, side, &crate::input::KeyEvent::typed(text)),
                    _ => Outcome::Ignored,
                },
            }
        }
        Event::Focus(_) => Outcome::Ignored,
    }
}

fn view_event(app: &mut App, side: usize, i: usize, ev: &Event) -> Outcome {
    let frame = layout(app).frames[side];
    with_view(app, side, i, |v, app| v.handle_event(app, side, ev, frame)).unwrap_or(Outcome::Ignored)
}

/// A key for panel `side`.
pub fn panel_key(app: &mut App, side: usize, k: &crate::input::KeyEvent) -> Outcome {
    let before = app.main.sides[side].panel.cursor_name().map(str::to_owned);
    let action = app.main.sides[side].panel.handle_key(k, std::time::Instant::now());
    let outcome = match action {
        PanelAction::Ignored => Outcome::Ignored,
        PanelAction::Consumed => Outcome::Consumed,
        PanelAction::Activate(name) => {
            panel::activate(app, side, &name);
            Outcome::Consumed
        }
        PanelAction::OpenExternal(name) => {
            panel::open_external(app, side, &name);
            Outcome::Consumed
        }
        PanelAction::GoParent => {
            panel::go_parent(app, side);
            Outcome::Consumed
        }
    };
    if app.main.sides[side].panel.cursor_name() != before.as_deref() {
        cursor_moved(app, side);
    }
    outcome
}

fn mouse(app: &mut App, m: &crate::input::MouseEvent) -> Outcome {
    let l = layout(app);
    let pressed = matches!(m.kind, MouseKind::Down(_) | MouseKind::DoubleClick(_));
    if let Some(strip) = l.strip
        && strip.contains(m.col, m.row)
    {
        if pressed && let Some((menu, _)) = menus::strip_titles(strip).into_iter().find(|(_, r)| r.contains(m.col, m.row)) {
            open_menu(app, menu);
        }
        return Outcome::Consumed;
    }
    if l.footer.contains(m.col, m.row) {
        if matches!(m.kind, MouseKind::Down(MouseButton::Left)) {
            let hit = footer_items(app, l.footer).into_iter().find(|(r, ..)| r.contains(m.col, m.row));
            if let Some((_, id, ..)) = hit {
                app.run_command(id);
            }
        }
        return Outcome::Consumed;
    }
    for side in 0..2 {
        if l.tabs[side].contains(m.col, m.row) {
            if pressed {
                let labels: Vec<String> = app.main.sides[side].tab_labels().iter().map(|s| s.to_string()).collect();
                let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
                if let Some(t) = tab_at(l.tabs[side], &labels, m.col, m.row) {
                    set_mode(app, side, if t == 0 { SideMode::Files } else { SideMode::View(t - 1) });
                }
            }
            return Outcome::Consumed;
        }
        if l.frames[side].contains(m.col, m.row) {
            return match app.main.sides[side].mode {
                SideMode::Files => panel_mouse(app, side, l.frames[side], m),
                SideMode::View(i) => {
                    if pressed {
                        app.main.active = side;
                    }
                    let ev = Event::Mouse(*m);
                    view_event(app, side, i, &ev);
                    Outcome::Consumed
                }
            };
        }
    }
    Outcome::Ignored
}

fn panel_mouse(app: &mut App, side: usize, frame: Rect, m: &crate::input::MouseEvent) -> Outcome {
    let pl = panel::draw::layout(frame);
    let visible = pl.rows.h as usize;
    let before = app.main.sides[side].panel.cursor_name().map(str::to_owned);
    match m.kind {
        MouseKind::Scroll(lines) => {
            let p = &mut app.main.sides[side].panel;
            p.list.scroll_by((-lines * 3.0).round() as isize, visible);
        }
        MouseKind::Down(button) | MouseKind::DoubleClick(button) => {
            app.main.active = side;
            if let Some(key) = panel::draw::header_hit(&pl, m.col, m.row) {
                if button == MouseButton::Left && matches!(m.kind, MouseKind::Down(_)) {
                    panel::sort_by(app, side, key);
                }
                return Outcome::Consumed;
            }
            let p = &mut app.main.sides[side].panel;
            let top = panel::draw::first_row(p, visible);
            p.list.top = top;
            let Some(row) = p.list.hit(pl.rows, m.col, m.row) else {
                return Outcome::Consumed;
            };
            let mark = button == MouseButton::Right || (button == MouseButton::Left && m.mods.has_primary());
            p.list.cursor = row;
            if mark {
                p.toggle_mark_at(row);
            } else if matches!(m.kind, MouseKind::DoubleClick(MouseButton::Left))
                && let Some(name) = p.name_at(row).map(str::to_owned)
            {
                panel::activate(app, side, &name);
            }
        }
        _ => return Outcome::Ignored,
    }
    if app.main.sides[side].panel.cursor_name() != before.as_deref() {
        cursor_moved(app, side);
    }
    Outcome::Consumed
}

/// Open menu `menu` of the in-window strip (F9 opens the active side's).
pub fn open_menu(app: &mut App, menu: Menu) {
    // The strip row: the top row (shown while the menu is open).
    let row = app.screen(MAIN_WINDOW).split_top(1).0;
    let bar = MenuBar::open(app, menu, row);
    app.show(bar);
}

// ---------------------------------------------------------------------------
// Commands that belong to the window itself
// ---------------------------------------------------------------------------

/// Side mode commands ("Files" in the Left/Right menus).
pub fn register(reg: &mut Registry) {
    for side in 0..2 {
        let (id, menu, title) =
            if side == 0 { ("left.files", Menu::Left, "Left: Files") } else { ("right.files", Menu::Right, "Right: Files") };
        reg.add(
            Command::new(id, title, move |app| set_mode(app, side, SideMode::Files))
                .help("Show the file panel on this side")
                .menu_label("Files")
                .menu(menu, "mode")
                .checked(move |app| app.main.sides[side].mode == SideMode::Files),
        );
    }
}

/// Helpers for features: the panels' locations.
pub fn location(app: &App, side: usize) -> VPath {
    app.main.sides[side].panel.location.clone()
}
