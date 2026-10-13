//! A terminal-style box: border in the theme's frame style, a title set into
//! the top edge and a subtitle into the bottom edge (port of `boxframe.py`),
//! plus the mode tabs drawn above a panel.

use oc_grid::{Flags, Frame, Layer, PxRect, Style};

use super::{DrawCtx, fill, put, width};
use crate::geom::Rect;

/// A box with an optional title (top edge, left) and subtitle (bottom edge, right).
#[derive(Clone, Copy, Debug, Default)]
pub struct BoxFrame<'a> {
    pub title: &'a str,
    pub subtitle: &'a str,
    /// Focused: border and title in the active color.
    pub active: bool,
}

impl<'a> BoxFrame<'a> {
    pub fn new(title: &'a str) -> Self {
        BoxFrame { title, ..Default::default() }
    }

    /// The cells the title occupies (for clicks on it), if shown.
    pub fn title_rect(&self, r: Rect) -> Option<Rect> {
        let text = self.title_text(r)?;
        Some(Rect::new(r.x + 2, r.y, width(&text), 1))
    }

    fn title_text(&self, r: Rect) -> Option<String> {
        let room = r.w.saturating_sub(6);
        if self.title.is_empty() || room < 2 {
            return None;
        }
        Some(format!(" {} ", oc_grid::elide_left(self.title, room.saturating_sub(2) as usize)))
    }

    fn subtitle_text(&self, r: Rect) -> Option<String> {
        let room = r.w.saturating_sub(6);
        if self.subtitle.is_empty() || room < 2 {
            return None;
        }
        Some(format!(" {} ", oc_grid::elide_left(self.subtitle, room.saturating_sub(2) as usize)))
    }

    /// Paint the box into `r` and return the inside (one cell in from each edge).
    ///
    /// The border runs through the middle of the edge cells, like the Python
    /// version's box drawn half a cell in: the outer halves of those cells
    /// keep the window background, the inner halves get the surface color
    /// (pixel strips under the text layer).
    pub fn draw(&self, f: &mut Frame, r: Rect, ctx: &DrawCtx) -> Rect {
        if r.w < 2 || r.h < 2 {
            return Rect::default();
        }
        let c = &ctx.theme.colors;
        let inner = r.inset(1, 1);
        fill(f, r, c.background);
        fill(f, inner, c.surface);
        let line = if self.active { c.border_active } else { c.border };
        // The focused frame is drawn with thicker lines too (1.x: a 2 px pen).
        let border = if self.active { Style::new(line, c.background).with(Flags::STRONG) } else { Style::new(line, c.background) };
        f.draw_box(r.cell_rect(), ctx.theme.frame, border);

        // Title and subtitle cells interrupt the top/bottom strips.
        let title = self.title_text(r);
        let title_span = title.as_ref().map(|t| (r.x + 2, r.x + 2 + width(t)));
        let subtitle = self.subtitle_text(r);
        let sub_span = subtitle.as_ref().map(|t| {
            let w = width(t);
            (r.right().saturating_sub(2 + w).max(r.x + 1), r.right().saturating_sub(2))
        });
        surface_strips(f, r, ctx, title_span, sub_span);

        if let (Some(text), Some((x, _))) = (&title, title_span) {
            let style = if self.active { Style::new(c.surface, line).with(Flags::BOLD) } else { Style::new(c.muted, c.surface) };
            put(f, x, r.y, text, style, r.w.saturating_sub(4));
        }
        if let (Some(text), Some((x, end))) = (&subtitle, sub_span) {
            put(f, x, r.bottom() - 1, text, Style::new(c.muted, c.surface), end - x);
        }
        inner
    }
}

/// Inner half-cell strips along the border, skipping `[a, b)` column spans on
/// the top (`top_gap`) and bottom (`bottom_gap`) rows.
fn surface_strips(f: &mut Frame, r: Rect, ctx: &DrawCtx, top_gap: Option<(u16, u16)>, bottom_gap: Option<(u16, u16)>) {
    let (cw, ch) = ctx.cell_px;
    let color = ctx.theme.colors.surface;
    let (x0, y0) = (r.x as f32 * cw, r.y as f32 * ch);
    let (x1, y1) = (r.right() as f32 * cw, r.bottom() as f32 * ch);
    let (hx, hy) = ((cw / 2.0).floor(), (ch / 2.0).floor());
    let mut hstrip = |y: f32, gap: Option<(u16, u16)>| {
        let (start, end) = (x0 + hx, x1 - hx);
        let mut segs = vec![(start, end)];
        if let Some((a, b)) = gap {
            let (ga, gb) = (a as f32 * cw, b as f32 * cw);
            segs = vec![(start, ga.max(start)), (gb.min(end), end)];
        }
        for (a, b) in segs {
            if b > a {
                f.add_rect(PxRect::new(a, y, b - a, ch - hy), color, Layer::BelowText);
            }
        }
    };
    hstrip(y0 + hy, top_gap);
    hstrip(y1 - ch, bottom_gap);
    let (sy, sh) = (y0 + ch, (y1 - ch) - (y0 + ch));
    if sh > 0.0 {
        f.add_rect(PxRect::new(x0 + hx, sy, cw - hx, sh), color, Layer::BelowText);
        f.add_rect(PxRect::new(x1 - cw, sy, cw - hx, sh), color, Layer::BelowText);
    }
}

/// One mode tab ("Files", "Preview").
#[derive(Clone, Copy, Debug)]
pub struct Tab<'a> {
    pub label: &'a str,
    pub selected: bool,
}

/// Padding around a tab label, in cells.
const TAB_PAD: u16 = 3;

/// The cells of each tab in a tab row starting one cell in.
pub fn tab_rects(row: Rect, labels: &[&str]) -> Vec<Rect> {
    let mut x = row.x + 1;
    labels
        .iter()
        .map(|l| {
            let w = width(l) + 2 * TAB_PAD;
            let r = Rect::new(x, row.y, w.min(row.right().saturating_sub(x)), 1);
            x = x.saturating_add(w + 1);
            r
        })
        .collect()
}

/// Which tab is at `col`.
pub fn tab_at(row: Rect, labels: &[&str], col: u16, line: u16) -> Option<usize> {
    tab_rects(row, labels).iter().position(|r| r.contains(col, line))
}

/// Draw a row of mode tabs on the window background. The selected tab is
/// highlighted; in the active side it takes the active color.
pub fn draw_tabs(f: &mut Frame, row: Rect, tabs: &[Tab], side_active: bool, ctx: &DrawCtx) {
    let c = &ctx.theme.colors;
    fill(f, row, c.background);
    let labels: Vec<&str> = tabs.iter().map(|t| t.label).collect();
    for (tab, r) in tabs.iter().zip(tab_rects(row, &labels)) {
        let style = match (tab.selected, side_active) {
            (true, true) => Style::new(c.surface, c.border_active).with(Flags::BOLD),
            (true, false) => Style::new(c.foreground, c.border).with(Flags::BOLD),
            (false, _) => Style::new(c.muted, c.background),
        };
        fill(f, r, style.bg);
        put(f, r.x + TAB_PAD, r.y, tab.label, style, r.w.saturating_sub(TAB_PAD));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn frame_with_title_and_subtitle() {
        let theme = Theme::default_dark();
        let ctx = DrawCtx::new(&theme, (8.0, 16.0));
        let mut f = Frame::new(30, 5, theme.colors.background);
        let b = BoxFrame { title: "~/src", subtitle: "2 files", active: true };
        let inner = b.draw(&mut f, Rect::new(0, 0, 30, 5), &ctx);
        assert_eq!(inner, Rect::new(1, 1, 28, 3));
        assert_eq!(f.row_text(0), "╭─ ~/src ────────────────────╮");
        assert_eq!(f.row_text(4), "╰────────────────── 2 files ─╯");
        assert_eq!(b.title_rect(Rect::new(0, 0, 30, 5)), Some(Rect::new(2, 0, 7, 1)));
        // Active title: inverted in the active border color.
        assert_eq!(f.cell(3, 0).unwrap().bg, theme.colors.border_active);
        // The active border is drawn strong (thicker), an inactive one not.
        assert!(f.cell(0, 2).unwrap().flags.contains(Flags::STRONG));
        BoxFrame { active: false, ..b }.draw(&mut f, Rect::new(0, 0, 30, 5), &ctx);
        assert!(!f.cell(0, 2).unwrap().flags.contains(Flags::STRONG));
    }

    #[test]
    fn tabs_hit_test() {
        let row = Rect::new(0, 0, 40, 1);
        let labels = ["Files", "Preview"];
        assert_eq!(tab_at(row, &labels, 1, 0), Some(0));
        assert_eq!(tab_at(row, &labels, 11, 0), Some(0));
        assert_eq!(tab_at(row, &labels, 12, 0), None);
        assert_eq!(tab_at(row, &labels, 13, 0), Some(1));
    }
}
