//! Drawing a file panel: frame with the path as title and the status as
//! subtitle, a column header, the rows (only the visible ones).

use chrono::TimeZone;
use oc_grid::{Flags, Frame, Style};
use oc_vfs::{Entry, SortKey};

use super::{FilePanel, PARENT};
use crate::geom::Rect;
use crate::widgets::{BoxFrame, DrawCtx, Scrollbar, fill, put, put_elided, put_right};

pub const DATE_FORMAT: &str = "%Y-%m-%d %H:%M";
const SIZE_W: u16 = 11;
const MTIME_W: u16 = 18;
/// Below this inner width the date column is dropped.
const MTIME_MIN_INNER: u16 = 52;

/// Column geometry of a panel's inside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Columns {
    pub name: Rect,
    pub size: Rect,
    /// Empty when the panel is too narrow.
    pub mtime: Rect,
}

/// Where things are inside a panel's frame rectangle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanelLayout {
    pub frame: Rect,
    pub header: Rect,
    pub rows: Rect,
    pub cols: Columns,
}

pub fn layout(frame: Rect) -> PanelLayout {
    let inner = frame.inset(1, 1);
    let (header, rows) = inner.split_top(1);
    let mtime_w = if inner.w >= MTIME_MIN_INNER { MTIME_W } else { 0 };
    let size_w = SIZE_W.min(inner.w);
    let name_w = inner.w.saturating_sub(size_w + mtime_w);
    let name = Rect::new(inner.x, inner.y, name_w, inner.h);
    let size = Rect::new(inner.x + name_w, inner.y, size_w, inner.h);
    let mtime = Rect::new(size.right(), inner.y, mtime_w, inner.h);
    PanelLayout { frame, header, rows, cols: Columns { name, size, mtime } }
}

/// Which sort column a header click at `col` means.
pub fn header_hit(l: &PanelLayout, col: u16, row: u16) -> Option<SortKey> {
    if !l.header.contains(col, row) {
        return None;
    }
    let c = &l.cols;
    if col < c.size.x {
        Some(SortKey::Name)
    } else if col < c.mtime.x || c.mtime.w == 0 {
        Some(SortKey::Size)
    } else {
        Some(SortKey::Mtime)
    }
}

/// The first row shown: the panel's scroll position (cursor moves scroll it
/// into view; the mouse wheel scrolls without moving the cursor, like Qt).
/// The main window keeps `p.rows` in step with the layout, so the cursor is
/// in view after keys and resizes.
pub fn first_row(p: &FilePanel, visible: usize) -> usize {
    p.list.first_visible(visible)
}

pub fn format_mtime(mtime: f64) -> String {
    if mtime == 0.0 {
        return String::new();
    }
    match chrono::Local.timestamp_opt(mtime.floor() as i64, 0) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => t.format(DATE_FORMAT).to_string(),
        chrono::LocalResult::None => String::new(),
    }
}

/// Draw the panel into its frame rectangle. `active`: the focused side.
pub fn draw(p: &FilePanel, f: &mut Frame, frame: Rect, ctx: &DrawCtx, active: bool) {
    let c = &ctx.theme.colors;
    let title = p.display_path();
    let subtitle = p.subtitle();
    BoxFrame { title: &title, subtitle: &subtitle, active }.draw(f, frame, ctx);
    let l = layout(frame);
    if l.rows.is_empty() {
        return;
    }

    // Header: accent, bold, sort indicator on the sort column.
    let head = Style::new(c.accent, c.surface).with(Flags::BOLD);
    let arrow = if p.reverse { " ▼" } else { " ▲" };
    let label = |key: SortKey, text: &str| {
        if p.sort == key { format!("{text}{arrow}") } else { text.to_owned() }
    };
    let y = l.header.y;
    put(f, l.cols.name.x + 1, y, &label(SortKey::Name, "Name"), head, l.cols.name.w.saturating_sub(1));
    put_right(f, l.cols.size.right().saturating_sub(1), y, &label(SortKey::Size, "Size"), head, l.cols.size.x);
    if l.cols.mtime.w > 0 {
        // Header labels sit a cell in from the column start (Qt's header padding).
        put(f, l.cols.mtime.x + 1, y, &label(SortKey::Mtime, "Modified"), head, l.cols.mtime.w.saturating_sub(1));
    }

    let visible = l.rows.h as usize;
    let top = first_row(p, visible);
    let focused = active && ctx.window_focused;
    for n in 0..visible {
        let row = top + n;
        if row >= p.len() {
            break;
        }
        let r = l.rows.row(n as u16);
        draw_row(p, f, &l, r, row, row == p.cursor(), focused, ctx);
    }
    Scrollbar { total: p.len(), visible, top }.draw(f, l.rows, ctx);
}

#[allow(clippy::too_many_arguments)]
fn draw_row(p: &FilePanel, f: &mut Frame, l: &PanelLayout, r: Rect, row: usize, is_cursor: bool, focused: bool, ctx: &DrawCtx) {
    let c = &ctx.theme.colors;
    let entry: Option<&Entry> = p.entry_at(row);
    let marked = entry.is_some_and(|e| p.marked.contains(&e.name));
    let mut fg = match entry {
        None => c.dir,
        Some(e) if e.is_dir => c.dir,
        Some(e) if e.is_link => c.link,
        Some(e) if e.is_executable => c.exec,
        Some(e) if e.is_hidden => c.hidden,
        Some(_) => c.foreground,
    };
    if marked {
        fg = c.marked;
    }
    let mut bg = c.surface;
    if is_cursor {
        if focused {
            bg = c.cursor_bg;
            fg = c.cursor_fg;
        } else {
            bg = c.cursor_inactive_bg;
        }
    }
    let mut style = Style::new(fg, bg);
    if entry.is_none_or(|e| e.is_dir) || marked {
        style = style.with(Flags::BOLD);
    }
    if entry.is_some_and(|e| e.is_link) {
        style = style.with(Flags::ITALIC);
    }
    fill(f, r, bg);

    let (name, size, mtime) = match entry {
        None => (PARENT.to_owned(), "<UP>".to_owned(), String::new()),
        Some(e) => {
            let name = if e.is_dir { format!("{}{}", e.name, std::path::MAIN_SEPARATOR) } else { e.name.clone() };
            let size = if e.is_dir {
                "<DIR>".to_owned()
            } else if e.is_online_only {
                format!("☁ {}", oc_vfs::human_size(e.size))
            } else {
                oc_vfs::human_size(e.size)
            };
            (name, size, format_mtime(e.mtime))
        }
    };
    let cols = &l.cols;
    put(f, cols.name.x, r.y, if marked { "✓" } else { " " }, style, 1);
    put_elided(f, cols.name.x + 1, r.y, &name, style, cols.name.w.saturating_sub(2));
    put_right(f, cols.size.right().saturating_sub(1), r.y, &size, style, cols.size.x);
    if cols.mtime.w > 0 {
        put(f, cols.mtime.x, r.y, &mtime, style, cols.mtime.w.saturating_sub(1));
    }
}
