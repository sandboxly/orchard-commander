//! Text previews: syntax-highlighted source, plain text, hex dumps and
//! folder/archive listings (port of `textview.py`'s plain and highlighted
//! modes). Lines are drawn as they are (no wrapping) with a horizontal
//! scrollbar for long ones; only the visible rows are painted. Text can be
//! selected with the mouse and copied ([`super::select`]).
//!
//! Also the painting helpers the Markdown view shares: style resolution
//! (syntax roles → the scheme's Pygments palette, theme roles → scheme
//! colors) and [`paint`], which writes styled pieces into a row with tabs,
//! wide characters and a horizontal offset.

use std::any::Any;

use oc_grid::{Flags, Frame, Layer, PxRect, Rgba, Style};
use oc_preview::highlight::SyntaxPalette;
use oc_preview::{Color, StyledLine, StyledText, TextStyle, ThemeRole};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::select::{self, MenuChoice, RowText, Selection};
use super::views::{ContentView, Handle};
use crate::app::{App, Outcome};
use crate::geom::Rect;
use crate::input::{Event, Key, MouseButton, MouseKind};
use crate::theme::Colors;
use crate::widgets::{DrawCtx, Scrollbar};

/// Tab stops every this many columns.
pub const TAB: usize = 4;
/// Rows per wheel step.
const WHEEL_ROWS: f32 = 3.0;
/// Columns per ← / → press.
const SIDE_STEP: usize = 4;

/// Text ready to draw.
#[derive(Debug, Default)]
pub struct TextDoc {
    pub lines: Vec<StyledLine>,
    /// Widest line in cells (for the horizontal scrollbar).
    pub max_width: usize,
}

impl TextDoc {
    pub fn styled(t: StyledText) -> TextDoc {
        let max_width = t.lines.iter().map(|l| line_width(&l.text)).max().unwrap_or(0);
        TextDoc { lines: t.lines, max_width }
    }

    pub fn plain(text: &str) -> TextDoc {
        TextDoc::styled(StyledText::plain(text))
    }
}

/// Display width of a line, tabs expanded.
pub fn line_width(text: &str) -> usize {
    let mut col = 0;
    for g in text.graphemes(true) {
        col += if g == "\t" { TAB - col % TAB } else { g.width() };
    }
    col
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

/// Resolves preview styles with the current scheme.
pub struct Painter<'a> {
    pub palette: &'static SyntaxPalette,
    pub colors: &'a Colors,
}

impl<'a> Painter<'a> {
    pub fn new(ctx: &'a DrawCtx) -> Self {
        Painter { palette: oc_preview::highlight::palette(&ctx.theme.scheme.pygments_style), colors: &ctx.theme.colors }
    }

    pub fn color(&self, c: Color) -> Option<Rgba> {
        match c {
            Color::Rgb(rgb) => Some(Rgba::rgb(rgb.r, rgb.g, rgb.b)),
            Color::Syntax(role) => self.palette.style(role).color.map(|c| Rgba::rgb(c.r, c.g, c.b)),
            Color::Theme(t) => Some(match t {
                ThemeRole::Muted => self.colors.muted,
                ThemeRole::Link => self.colors.link,
                ThemeRole::CodeBackground => code_background(self.colors),
                ThemeRole::Border => self.colors.border,
            }),
        }
    }

    /// `ts` on top of `base`.
    pub fn style(&self, ts: TextStyle, base: Style) -> Style {
        let ts = self.palette.resolve(ts);
        let mut s = base;
        if let Some(fg) = ts.fg.and_then(|c| self.color(c)) {
            s.fg = fg;
        }
        if let Some(bg) = ts.bg.and_then(|c| self.color(c)) {
            s.bg = bg;
        }
        s.flags.set(Flags::BOLD, ts.bold || base.flags.contains(Flags::BOLD));
        s.flags.set(Flags::ITALIC, ts.italic || base.flags.contains(Flags::ITALIC));
        s.flags.set(Flags::UNDERLINE, ts.underline || base.flags.contains(Flags::UNDERLINE));
        s.flags.set(Flags::STRIKETHROUGH, ts.strikethrough);
        s
    }
}

/// Behind inline code and code blocks: the surface, a shade towards the text color.
pub fn code_background(c: &Colors) -> Rgba {
    c.foreground.with_alpha(22).over(c.surface)
}

/// Write styled pieces into row `y` from column `x`, skipping the first
/// `skip` columns of the line and stopping at `max_x`. Returns the line's
/// columns passed (the logical position after the last piece written).
pub fn paint<'s>(
    f: &mut Frame,
    x: u16,
    y: u16,
    max_x: u16,
    skip: usize,
    pieces: impl IntoIterator<Item = (&'s str, Style)>,
) -> usize {
    let mut col = 0usize;
    let limit = skip + max_x.saturating_sub(x) as usize;
    for (text, style) in pieces {
        for g in text.graphemes(true) {
            if col >= limit {
                return col;
            }
            let (glyph, w) = match g {
                "\t" => (" ", TAB - col % TAB),
                "\r" | "\r\n" | "\n" => continue,
                _ if g.chars().all(char::is_control) => ("\u{fffd}", 1),
                _ => (g, g.width()),
            };
            if w == 0 {
                continue;
            }
            if col + w <= skip {
                col += w;
                continue;
            }
            if glyph == " " || col < skip || col + w > limit {
                // Spaces (tabs), and wide characters cut by an edge.
                for c in col.max(skip)..(col + w).min(limit) {
                    f.put_char(x + (c - skip) as u16, y, ' ', style);
                }
            } else {
                f.put_str_clipped(x + (col - skip) as u16, y, glyph, style, max_x);
            }
            col += w;
        }
    }
    col
}

// ---------------------------------------------------------------------------
// Scrolling
// ---------------------------------------------------------------------------

/// The first visible row for `top` when `total` rows show `visible` at a time.
pub fn clamp_top(top: usize, total: usize, visible: usize) -> usize {
    top.min(total.saturating_sub(visible))
}

/// Vertical scrolling keys and the wheel; true if `top` was handled.
pub fn scroll_event(top: &mut usize, ev: &Event, total: usize, visible: usize) -> bool {
    let max = total.saturating_sub(visible);
    let page = visible.saturating_sub(1).max(1);
    let cur = (*top).min(max);
    let new = match ev {
        Event::Key(k) if k.mods.is_empty() => match k.key {
            Key::Up => cur.saturating_sub(1),
            Key::Down => cur + 1,
            Key::PageUp => cur.saturating_sub(page),
            Key::PageDown | Key::Space => cur + page,
            Key::Home => 0,
            Key::End => max,
            _ => return false,
        },
        Event::Mouse(m) => match m.kind {
            MouseKind::Scroll(lines) => {
                let rows = (-lines * WHEEL_ROWS).round() as isize;
                (cur as isize + rows).max(0) as usize
            }
            _ => return false,
        },
        _ => return false,
    };
    *top = new.min(max);
    true
}

/// A thin horizontal scrollbar over the lower half of row `r`.
pub fn draw_hbar(f: &mut Frame, r: Rect, total: usize, visible: usize, left: usize, ctx: &DrawCtx) {
    let bar = Scrollbar { total, visible, top: left };
    if !bar.needed() || r.is_empty() {
        return;
    }
    let (cw, ch) = ctx.cell_px;
    let h = (ch / 4.0).round().max(2.0);
    let (x0, w) = (r.x as f32 * cw, r.w as f32 * cw);
    let y = r.y as f32 * ch + (ch - h) / 2.0;
    let (start, len) = bar.thumb();
    let thumb_w = (len * w).max(cw);
    let travel = if len < 1.0 { start / (1.0 - len) } else { 0.0 };
    f.add_rect(PxRect::new(x0, y, w, h), ctx.theme.colors.surface, Layer::AboveText);
    f.add_rect(PxRect::new(x0 + travel * (w - thumb_w), y, thumb_w, h), ctx.theme.colors.border, Layer::AboveText);
}

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

pub struct TextView {
    pub doc: TextDoc,
    pub top: usize,
    pub left: usize,
    pub selection: Selection,
}

/// Where the text rows go, the horizontal scrollbar's row and the text width.
struct TextLayout {
    rows: Rect,
    hbar: Option<Rect>,
    text_w: u16,
}

impl TextView {
    pub fn new(doc: TextDoc) -> Self {
        TextView { doc, top: 0, left: 0, selection: Selection::default() }
    }

    /// The selected text, tabs kept.
    pub fn selected_text(&self) -> String {
        self.selection.text(|r| self.doc.lines.get(r).map(|l| RowText { text: l.text.as_str().into(), lead: 0, joiner: "\n" }))
    }

    /// Mouse selection and the context menu; None if the event is neither.
    fn mouse(&mut self, app: &mut App, h: Handle, ev: &Event, l: &TextLayout) -> Option<Outcome> {
        let Event::Mouse(m) = ev else { return None };
        let inside = Rect { w: l.text_w, ..l.rows }.contains(m.col, m.row);
        match m.kind {
            MouseKind::Down(MouseButton::Right) | MouseKind::DoubleClick(MouseButton::Right) if inside => {
                select::context_menu(app, m, !self.selection.is_empty(), false, move |app, choice| {
                    h.with::<TextView, _>(app, |v, app, _| match choice {
                        MenuChoice::Copy => select::copy(app, &v.selected_text()),
                        MenuChoice::SelectAll => v.selection.select_all(v.doc.lines.len()),
                        MenuChoice::CopyLink => {}
                    });
                });
                Some(Outcome::Consumed)
            }
            MouseKind::Down(MouseButton::Left) | MouseKind::DoubleClick(MouseButton::Left) if !inside => None,
            MouseKind::Down(MouseButton::Left)
            | MouseKind::DoubleClick(MouseButton::Left)
            | MouseKind::Up(MouseButton::Left)
            | MouseKind::Move => {
                let top = clamp_top(self.top, self.doc.lines.len(), l.rows.h as usize);
                let left = self.left.min(self.doc.max_width.saturating_sub(l.text_w as usize));
                let pos = select::pos_at(m, l.rows, top, left);
                let lines = &self.doc.lines;
                let changed = self.selection.mouse(m, pos, |(r, c)| lines.get(r).and_then(|l| select::word_at(&l.text, c)));
                // Hovering, or dragging within the same cell, changes nothing on screen.
                let mine = changed || m.kind != MouseKind::Move;
                Some(if mine { Outcome::Consumed } else { Outcome::Ignored })
            }
            _ => None,
        }
    }

    fn layout(&self, area: Rect) -> TextLayout {
        let wide = self.doc.max_width > area.w as usize;
        let (rows, hbar) = if wide && area.h > 1 {
            let (rows, bar) = area.split_bottom(1);
            (rows, Some(bar))
        } else {
            (area, None)
        };
        let vbar = self.doc.lines.len() > rows.h as usize;
        TextLayout { rows, hbar, text_w: area.w.saturating_sub(vbar as u16) }
    }
}

impl ContentView for TextView {
    fn kind(&self) -> &'static str {
        "text"
    }

    fn draw(&self, _app: &App, f: &mut Frame, area: Rect, ctx: &DrawCtx, _focused: bool) {
        let l = self.layout(area);
        let painter = Painter::new(ctx);
        let base = ctx.theme.text();
        let visible = l.rows.h as usize;
        let total = self.doc.lines.len();
        let top = clamp_top(self.top, total, visible);
        let left = self.left.min(self.doc.max_width.saturating_sub(l.text_w as usize));
        let max_x = l.rows.x + l.text_w;
        for (i, line) in self.doc.lines.iter().skip(top).take(visible).enumerate() {
            let pieces = line.segments().map(|(t, ts, _)| (t, painter.style(ts, base)));
            let y = l.rows.y + i as u16;
            paint(f, l.rows.x, y, max_x, left, pieces);
            if let Some(cells) = self.selection.cells_in_row(top + i, line_width(&line.text)) {
                select::paint(f, ctx, cells, l.rows.x, y, left, max_x);
            }
        }
        Scrollbar { total, visible, top }.draw(f, l.rows, ctx);
        if let Some(bar) = l.hbar {
            draw_hbar(f, Rect { w: l.text_w, ..bar }, self.doc.max_width, l.text_w as usize, left, ctx);
        }
    }

    fn handle_event(&mut self, app: &mut App, h: Handle, ev: &Event, area: Rect) -> Outcome {
        let l = self.layout(area);
        if let Some(outcome) = self.mouse(app, h, ev, &l) {
            return outcome;
        }
        if let Event::Key(k) = ev
            && select::is_copy(k)
        {
            select::copy(app, &self.selected_text());
            return Outcome::Consumed;
        }
        if scroll_event(&mut self.top, ev, self.doc.lines.len(), l.rows.h as usize) {
            return Outcome::Consumed;
        }
        let max_left = self.doc.max_width.saturating_sub(l.text_w as usize);
        if let Event::Key(k) = ev
            && k.mods.is_empty()
        {
            let left = self.left.min(max_left);
            match k.key {
                Key::Left => self.left = left.saturating_sub(SIDE_STEP),
                Key::Right => self.left = (left + SIDE_STEP).min(max_left),
                _ => return Outcome::Ignored,
            }
            return Outcome::Consumed;
        }
        Outcome::Ignored
    }

    fn text(&self) -> Option<String> {
        Some(self.doc.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n"))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pieces: &[&str], skip: usize, w: u16) -> String {
        let mut f = Frame::new(w, 1, Rgba::BLACK);
        paint(&mut f, 0, 0, w, skip, pieces.iter().map(|p| (*p, Style::default())));
        f.row_text(0).trim_end().to_owned()
    }

    #[test]
    fn tabs_wide_chars_and_offsets() {
        assert_eq!(line_width("a\tb"), 5);
        assert_eq!(line_width("日本"), 4);
        assert_eq!(row(&["a\tb"], 0, 10), "a   b");
        assert_eq!(row(&["abc", "def"], 2, 10), "cdef");
        assert_eq!(row(&["日本語"], 1, 10), " 本語", "a wide char cut by the left edge is blanked");
        assert_eq!(row(&["abcdefgh"], 0, 4), "abcd");
        assert_eq!(row(&["x\r"], 0, 4), "x");
    }

    #[test]
    fn scrolling_clamps() {
        let mut top = 0;
        let key = |k| Event::Key(crate::input::KeyEvent::plain(k));
        assert!(scroll_event(&mut top, &key(Key::End), 100, 10));
        assert_eq!(top, 90);
        scroll_event(&mut top, &key(Key::Down), 100, 10);
        assert_eq!(top, 90);
        scroll_event(&mut top, &key(Key::PageUp), 100, 10);
        assert_eq!(top, 81);
        scroll_event(&mut top, &key(Key::Home), 100, 10);
        assert_eq!(top, 0);
        assert!(!scroll_event(&mut top, &key(Key::Enter), 100, 10));
    }
}
