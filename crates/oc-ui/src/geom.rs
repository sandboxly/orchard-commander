//! Cell rectangles for layout and hit testing.

use orchard_grid::CellRect;

/// A rectangle of character cells. Unlike [`CellRect`] it may be empty and
/// offers the splitting helpers layouts need.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub const fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Rect { x, y, w, h }
    }

    /// The whole grid of a `cols` x `rows` frame.
    pub const fn screen(cols: u16, rows: u16) -> Self {
        Rect { x: 0, y: 0, w: cols, h: rows }
    }

    pub fn right(&self) -> u16 {
        self.x.saturating_add(self.w)
    }

    pub fn bottom(&self) -> u16 {
        self.y.saturating_add(self.h)
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    pub fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.x && col < self.right() && row >= self.y && row < self.bottom()
    }

    /// Shrunk by `dx` columns on each side and `dy` rows top and bottom.
    pub fn inset(&self, dx: u16, dy: u16) -> Rect {
        Rect {
            x: self.x.saturating_add(dx),
            y: self.y.saturating_add(dy),
            w: self.w.saturating_sub(dx.saturating_mul(2)),
            h: self.h.saturating_sub(dy.saturating_mul(2)),
        }
    }

    /// One row of this rectangle (empty past the bottom).
    pub fn row(&self, i: u16) -> Rect {
        if i >= self.h {
            return Rect::new(self.x, self.bottom(), self.w, 0);
        }
        Rect::new(self.x, self.y + i, self.w, 1)
    }

    /// The top `n` rows and the rest.
    pub fn split_top(&self, n: u16) -> (Rect, Rect) {
        let n = n.min(self.h);
        (Rect::new(self.x, self.y, self.w, n), Rect::new(self.x, self.y + n, self.w, self.h - n))
    }

    /// The rest and the bottom `n` rows.
    pub fn split_bottom(&self, n: u16) -> (Rect, Rect) {
        let n = n.min(self.h);
        (Rect::new(self.x, self.y, self.w, self.h - n), Rect::new(self.x, self.bottom() - n, self.w, n))
    }

    /// The left `n` columns and the rest.
    pub fn split_left(&self, n: u16) -> (Rect, Rect) {
        let n = n.min(self.w);
        (Rect::new(self.x, self.y, n, self.h), Rect::new(self.x + n, self.y, self.w - n, self.h))
    }

    /// A `w` x `h` rectangle centered in this one (clamped to fit).
    pub fn centered(&self, w: u16, h: u16) -> Rect {
        let (w, h) = (w.min(self.w), h.min(self.h));
        Rect::new(self.x + (self.w - w) / 2, self.y + (self.h - h) / 2, w, h)
    }

    pub fn cell_rect(&self) -> CellRect {
        CellRect::new(self.x, self.y, self.w, self.h)
    }
}

impl From<Rect> for CellRect {
    fn from(r: Rect) -> Self {
        r.cell_rect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitting_and_hits() {
        let r = Rect::new(2, 3, 10, 5);
        assert!(r.contains(2, 3) && r.contains(11, 7) && !r.contains(12, 7) && !r.contains(2, 8));
        let (top, rest) = r.split_top(1);
        assert_eq!((top, rest), (Rect::new(2, 3, 10, 1), Rect::new(2, 4, 10, 4)));
        assert_eq!(r.inset(1, 1), Rect::new(3, 4, 8, 3));
        assert_eq!(r.centered(4, 1), Rect::new(5, 5, 4, 1));
        assert!(r.row(9).is_empty());
    }
}
