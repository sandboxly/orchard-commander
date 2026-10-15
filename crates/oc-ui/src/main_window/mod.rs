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
//! │ terminal (features::terminal, ⌘J / ⌃`)                              │
//! │ F3 Preview F5 Copy …                                 ⌘P Commands    │  F-key bar
//! └─────────────────────────────────────────────────────────────────────┘
//! ```

pub mod side;

use std::path::{Path, PathBuf};
use std::rc::Rc;

use oc_config::Settings;
use orchard_grid::{Flags, Frame, Style};
use oc_vfs::VPath;

use crate::app::{App, MAIN_WINDOW, Outcome, Severity};
use crate::commands::{Command, CommandId, Registry};
use crate::geom::Rect;
use crate::input::{Event, Key, MouseButton, MouseKind};
use crate::menus::{self, Menu, MenuBar};
use crate::panel::{self, FilePanel, LoadOptions, PanelAction};
use crate::widgets::boxframe::{draw_tabs, tab_at};
use crate::widgets::{DrawCtx, Tab, fill, put, width};
pub use side::{Side, SideMode, SideView, view_as, with_view, with_view_as};

pub const SIDE_NAMES: [&str; 2] = ["left", "right"];

pub struct MainWindow {
    pub sides: [Side; 2],
    /// The side with the keyboard focus.
    pub active: usize,
    /// Rows reserved below the sides for a bottom pane (the terminal).
    pub bottom_rows: u16,
    /// Where a side goes if its saved folder can't be listed at start (1.x:
    /// the current folder on the left, home on the right). The check runs
    /// with the first listing, on a worker: a saved folder on a dead network
    /// mount must not hang the window.
    startup_fallback: [Option<PathBuf>; 2],
    /// The side view the left button went down in: it gets the moves and
    /// the release until then, wherever they are (selecting text).
    view_drag: Option<usize>,
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
            let p = FilePanel::new(i, oc_local::local(path), hidden);
            Side::new(SIDE_NAMES[i], p, &registry.side_views)
        };
        MainWindow { sides: [make(0, left), make(1, right)], active: 0, bottom_rows: 0, startup_fallback, view_drag: None }
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
    let frames = layout(app).frames;
    for (side, frame) in frames.into_iter().enumerate() {
        if let SideMode::View(i) = app.main.sides[side].mode {
            with_view(app, side, i, |v, app| v.resized(app, side, frame));
        }
    }
    for hook in app.registry.on_resized.clone() {
        hook(app);
    }
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
    for hook in app.registry.on_location_changed.clone() {
        hook(app, side);
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
        let active = main.active == i && !crate::features::terminal::has_focus(app);
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
    crate::features::terminal::draw(app, f, l.bottom, ctx);
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
    if crate::features::terminal::pre_event(app, ev) == Outcome::Consumed {
        return Outcome::Consumed;
    }
    match ev {
        Event::Key(k) => {
            // A disabled command leaves its key to the panel / side view.
            if let Some(cmd) = app.registry.for_key(k).filter(|c| c.is_enabled(app)) {
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
    if pressed {
        app.main.view_drag = None; // a release lost to a dialog doesn't keep the grab
    }
    if let Some(side) = app.main.view_drag
        && matches!(m.kind, MouseKind::Move | MouseKind::Up(MouseButton::Left))
    {
        if m.kind == MouseKind::Up(MouseButton::Left) {
            app.main.view_drag = None;
        }
        if let SideMode::View(i) = app.main.sides[side].mode {
            return view_event(app, side, i, &Event::Mouse(*m));
        }
    }
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
                    if m.kind == MouseKind::Down(MouseButton::Left) {
                        app.main.view_drag = Some(side);
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
            if button == MouseButton::Left && panel::draw::title_hit(&app.main.sides[side].panel, frame, m.col, m.row) {
                if let Some(hook) = app.registry.on_title_click {
                    hook(app, side, m.col, m.row);
                }
                return Outcome::Consumed;
            }
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
            // 1.x: Control or Meta, i.e. ⌘ and ⌃ on macOS.
            let mark = button == MouseButton::Right || (button == MouseButton::Left && (m.mods.ctrl || m.mods.logo));
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
                .hidden_from_palette() // a menu entry only, as in 1.x
                .checked(move |app| app.main.sides[side].mode == SideMode::Files),
        );
    }
}

/// Helpers for features: the panels' locations.
/// Where files dropped at cell `at` go: the folder row under the pointer,
/// else that side's folder; without a position, the active panel's folder.
pub fn drop_target(app: &App, at: Option<(u16, u16)>) -> (usize, VPath) {
    let l = layout(app);
    let side = at
        .and_then(|(c, r)| (0..2).find(|&s| l.frames[s].contains(c, r) || l.tabs[s].contains(c, r)))
        .unwrap_or_else(|| app.main.files_side());
    let p = &app.main.sides[side].panel;
    if let Some((c, r)) = at
        && app.main.sides[side].mode == SideMode::Files
    {
        let rows = panel::draw::layout(l.frames[side]).rows;
        if rows.contains(c, r) {
            let row = panel::draw::first_row(p, rows.h as usize) + (r - rows.y) as usize;
            if let Some(e) = p.entry_at(row).filter(|e| e.is_dir) {
                return (side, p.target(&e.name));
            }
        }
    }
    (side, p.location.clone())
}

pub fn location(app: &App, side: usize) -> VPath {
    app.main.sides[side].panel.location.clone()
}
