//! A thin vertical scrollbar (1ex wide, like the Python style sheet), drawn
//! as a pixel-exact thumb over the right edge of a view.

use oc_grid::{Frame, Layer, PxRect};

use super::DrawCtx;
use crate::geom::Rect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scrollbar {
    pub total: usize,
    pub visible: usize,
    pub top: usize,
}

impl Scrollbar {
    pub fn needed(&self) -> bool {
        self.total > self.visible && self.visible > 0
    }

    /// Thumb position as (start, length) in fractions of the track.
    pub fn thumb(&self) -> (f32, f32) {
        if !self.needed() {
            return (0.0, 1.0);
        }
        let len = (self.visible as f32 / self.total as f32).max(0.05);
        let max_top = (self.total - self.visible) as f32;
        let start = (self.top as f32 / max_top).min(1.0) * (1.0 - len);
        (start, len)
    }

    /// Draw over the right half of the last column of `r`.
    pub fn draw(&self, f: &mut Frame, r: Rect, ctx: &DrawCtx) {
        if !self.needed() || r.is_empty() {
            return;
        }
        let (cw, ch) = ctx.cell_px;
        let w = (cw / 2.0).round().max(2.0);
        let x = r.right() as f32 * cw - w;
        let (y0, h) = (r.y as f32 * ch, r.h as f32 * ch);
        let (start, len) = self.thumb();
        let thumb_h = (len * h).max(ch);
        // Where the thumb is along its free travel (0 = top, 1 = bottom).
        let travel = if len < 1.0 { start / (1.0 - len) } else { 0.0 };
        let y = y0 + travel * (h - thumb_h);
        f.add_rect(PxRect::new(x, y0, w, h), ctx.theme.colors.surface, Layer::AboveText);
        f.add_rect(PxRect::new(x, y, w, thumb_h), ctx.theme.colors.border, Layer::AboveText);
    }

    /// The `top` a click at `row` inside `r`'s track jumps to.
    pub fn top_for_click(&self, r: Rect, row: u16) -> usize {
        if !self.needed() || r.h == 0 {
            return self.top;
        }
        let frac = (row.saturating_sub(r.y)) as f32 / r.h.max(1) as f32;
        ((frac * self.total as f32) as usize).saturating_sub(self.visible / 2).min(self.total - self.visible)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumb_geometry() {
        assert!(!Scrollbar { total: 5, visible: 10, top: 0 }.needed());
        let s = Scrollbar { total: 100, visible: 10, top: 90 };
        let (start, len) = s.thumb();
        assert!((len - 0.1).abs() < 1e-6 && (start + len - 1.0).abs() < 1e-6);
        assert_eq!(s.top_for_click(Rect::new(0, 0, 5, 10), 0), 0);
        assert_eq!(s.top_for_click(Rect::new(0, 0, 5, 10), 9), 85);
    }
}
