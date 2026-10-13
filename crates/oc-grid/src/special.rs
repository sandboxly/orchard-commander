//! Characters drawn geometrically so they fill their cell exactly.
//!
//! Powerline separators, block elements and box-drawing lines are meant to
//! touch their neighbours; a font's glyphs leave gaps or misalign as soon as
//! its proportions differ from the cell (other font, line spacing). Like
//! iTerm, kitty and WezTerm — and the Python version's `term_glyphs.py` —
//! rasterize them ourselves at the exact cell size into a coverage mask that
//! goes into the glyph atlas like any other glyph.

use tiny_skia::{FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

/// Box drawing: char -> (left, right, up, down) with 1 = light, 2 = heavy, 3 = double.
fn box_arms(ch: char) -> Option<[u8; 4]> {
    Some(match ch {
        '─' => [1, 1, 0, 0],
        '━' => [2, 2, 0, 0],
        '│' => [0, 0, 1, 1],
        '┃' => [0, 0, 2, 2],
        '┌' => [0, 1, 0, 1],
        '┐' => [1, 0, 0, 1],
        '└' => [0, 1, 1, 0],
        '┘' => [1, 0, 1, 0],
        '├' => [0, 1, 1, 1],
        '┤' => [1, 0, 1, 1],
        '┬' => [1, 1, 0, 1],
        '┴' => [1, 1, 1, 0],
        '┼' => [1, 1, 1, 1],
        '┏' => [0, 2, 0, 2],
        '┓' => [2, 0, 0, 2],
        '┗' => [0, 2, 2, 0],
        '┛' => [2, 0, 2, 0],
        '┣' => [0, 2, 2, 2],
        '┫' => [2, 0, 2, 2],
        '┳' => [2, 2, 0, 2],
        '┻' => [2, 2, 2, 0],
        '╋' => [2, 2, 2, 2],
        '╴' => [1, 0, 0, 0],
        '╵' => [0, 0, 1, 0],
        '╶' => [0, 1, 0, 0],
        '╷' => [0, 0, 0, 1],
        '═' => [3, 3, 0, 0],
        '║' => [0, 0, 3, 3],
        '╔' => [0, 3, 0, 3],
        '╗' => [3, 0, 0, 3],
        '╚' => [0, 3, 3, 0],
        '╝' => [3, 0, 3, 0],
        '╠' => [0, 3, 3, 3],
        '╣' => [3, 0, 3, 3],
        '╦' => [3, 3, 0, 3],
        '╩' => [3, 3, 3, 0],
        '╬' => [3, 3, 3, 3],
        _ => return None,
    })
}

/// Rounded corners: (horizontal arm goes right, vertical arm goes down).
fn round_arms(ch: char) -> Option<(bool, bool)> {
    Some(match ch {
        '╭' => (true, true),
        '╮' => (false, true),
        '╯' => (false, false),
        '╰' => (true, false),
        _ => return None,
    })
}

/// Whether `ch` is drawn by [`rasterize`] instead of from a font.
pub fn handles(ch: char) -> bool {
    let cp = ch as u32;
    (0x2580..=0x2595).contains(&cp)
        || (0xE0B0..=0xE0BF).contains(&cp)
        || box_arms(ch).is_some()
        || round_arms(ch).is_some()
}

/// Line thickness for a cell `w` pixels wide (same rule as the Python version).
pub fn thin(w: u32) -> u32 {
    ((w as f32 / 8.0).round() as u32).max(1)
}

/// Whether `ch` is a box-drawing line (light, heavy, double or rounded),
/// the characters [`rasterize_strong`] draws thicker.
pub fn is_line(ch: char) -> bool {
    box_arms(ch).is_some() || round_arms(ch).is_some()
}

/// Characters drawn geometrically only when no installed font has them:
/// the check marks the file panel and menus use for marked items.
pub fn has_fallback(ch: char) -> bool {
    matches!(ch, '✓' | '✔')
}

/// A drawing of one of the [`has_fallback`] characters, a `w`×`h` coverage
/// mask like [`rasterize`]'s.
pub fn rasterize_fallback(ch: char, w: u32, h: u32) -> Option<Vec<u8>> {
    if !has_fallback(ch) || w == 0 || h == 0 {
        return None;
    }
    let mut canvas = Canvas::new(w, h, false);
    canvas.check(ch == '✔');
    Some(canvas.into_mask())
}

/// Rasterize `ch` (one for which [`handles`] is true) into a `w`×`h` coverage
/// mask, row-major, 0 = empty, 255 = full. `None` for other characters.
pub fn rasterize(ch: char, w: u32, h: u32) -> Option<Vec<u8>> {
    rasterize_strong(ch, w, h, false)
}

/// Like [`rasterize`]; with `strong`, box-drawing lines are one step thicker
/// (light lines twice the thin width, heavy three times, double lines twice):
/// the focused panel's and dialogs' frames, like the Python version's 2 px
/// pen. Other characters ignore `strong`.
pub fn rasterize_strong(ch: char, w: u32, h: u32, strong: bool) -> Option<Vec<u8>> {
    if !handles(ch) || w == 0 || h == 0 {
        return None;
    }
    let mut canvas = Canvas::new(w, h, strong && is_line(ch));
    let cp = ch as u32;
    if (0xE0B0..=0xE0BF).contains(&cp) {
        canvas.powerline(cp);
    } else if (0x2580..=0x2595).contains(&cp) {
        canvas.block(cp);
    } else if let Some(arms) = round_arms(ch) {
        canvas.rounded(arms);
    } else if let Some(arms) = box_arms(ch) {
        canvas.boxed(arms);
    }
    Some(canvas.into_mask())
}

struct Canvas {
    w: u32,
    h: u32,
    /// Lines one step thicker.
    strong: bool,
    pixmap: Pixmap,
}

impl Canvas {
    fn new(w: u32, h: u32, strong: bool) -> Self {
        Canvas {
            w,
            h,
            strong,
            pixmap: Pixmap::new(w, h).expect("non-zero cell"),
        }
    }

    /// The light line width.
    fn line(&self) -> u32 {
        if self.strong {
            2 * thin(self.w)
        } else {
            thin(self.w)
        }
    }

    /// A check mark: a short stroke down, a long one up, in a square box
    /// centred in the cell like a font's ✓.
    fn check(&mut self, heavy: bool) {
        let (w, h) = (self.w as f32, self.h as f32);
        let size = w.min(h * 0.6);
        let (x0, y0) = ((w - size) / 2.0, (h - size) / 2.0 + size * 0.05);
        let mut pb = PathBuilder::new();
        pb.move_to(x0 + size * 0.1, y0 + size * 0.55);
        pb.line_to(x0 + size * 0.4, y0 + size * 0.85);
        pb.line_to(x0 + size * 0.92, y0 + size * 0.12);
        let Some(path) = pb.finish() else { return };
        let width = (thin(self.w) as f32 * if heavy { 2.0 } else { 1.3 }).max(1.0);
        let stroke = Stroke {
            width,
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            ..Stroke::default()
        };
        self.pixmap.stroke_path(
            &path,
            &Self::paint(255, true),
            &stroke,
            Transform::identity(),
            None,
        );
    }

    fn into_mask(self) -> Vec<u8> {
        self.pixmap.pixels().iter().map(|p| p.alpha()).collect()
    }

    fn paint(alpha: u8, anti_alias: bool) -> Paint<'static> {
        let mut paint = Paint::default();
        paint.set_color_rgba8(255, 255, 255, alpha);
        paint.anti_alias = anti_alias;
        paint
    }

    /// Fill an axis-aligned rectangle given by its edges, clamped to the cell.
    fn fill(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, alpha: u8) {
        let (x0, x1) = (x0.max(0.0), x1.min(self.w as f32));
        let (y0, y1) = (y0.max(0.0), y1.min(self.h as f32));
        if let Some(rect) = Rect::from_ltrb(x0, y0, x1, y1) {
            self.pixmap.fill_rect(
                rect,
                &Self::paint(alpha, false),
                Transform::identity(),
                None,
            );
        }
    }

    /// Light and heavy lines are pixel-aligned rectangles from the cell edge
    /// through the centre, so a `─` meets the `┼` next to it without seams.
    /// Double lines get proper inner/outer corners.
    fn boxed(&mut self, arms: [u8; 4]) {
        let (w, h) = (self.w as i32, self.h as i32);
        let t = thin(self.w) as i32;
        let [left, right, up, down] = arms;
        if arms.contains(&3) {
            return self.double(arms);
        }
        let (light, heavy) = if self.strong { (2 * t, 3 * t) } else { (t, 2 * t) };
        let weight = |a: u8| if a == 2 { heavy } else { light };
        // Horizontal arms.
        for (on, is_left) in [(left, true), (right, false)] {
            if on == 0 {
                continue;
            }
            let lt = weight(on);
            let y0 = (h - lt) / 2;
            let cross = weight(up.max(down).max(1));
            let xs = (w - cross) / 2;
            let (x0, x1) = if is_left { (0, xs + cross) } else { (xs, w) };
            self.fill(x0 as f32, y0 as f32, x1 as f32, (y0 + lt) as f32, 255);
        }
        // Vertical arms.
        for (on, is_up) in [(up, true), (down, false)] {
            if on == 0 {
                continue;
            }
            let lt = weight(on);
            let x0 = (w - lt) / 2;
            let cross = weight(left.max(right).max(1));
            let ys = (h - cross) / 2;
            let (y0, y1) = if is_up { (0, ys + cross) } else { (ys, h) };
            self.fill(x0 as f32, y0 as f32, (x0 + lt) as f32, y1 as f32, 255);
        }
    }

    /// Double lines: two thin lines 1.5 line widths either side of the
    /// centre. Each line stops at the perpendicular line it meets, so corners
    /// and T-junctions show both an outer and an inner corner.
    fn double(&mut self, arms: [u8; 4]) {
        let (w, h) = (self.w as i32, self.h as i32);
        let t = self.line() as i32;
        let d = ((t as f32 * 1.5).round() as i32).max(1);
        let [left, right, up, down] = arms.map(|a| a != 0);
        // Upper/lower horizontal lines (y) and left/right vertical lines (x), top/left edges.
        let (hy, vx) = ((h - t) / 2, (w - t) / 2);
        let (u, lo) = (hy - d, hy + d);
        let (l, r) = (vx - d, vx + d);
        let vertical = up || down;
        let horizontal = left || right;
        if left {
            // Upper line: inner corner if the up arm exists, else outer.
            let end_u = if !vertical {
                w / 2 + t
            } else if up {
                l + t
            } else {
                r + t
            };
            let end_d = if !vertical {
                w / 2 + t
            } else if down {
                l + t
            } else {
                r + t
            };
            self.fill(0.0, u as f32, end_u as f32, (u + t) as f32, 255);
            self.fill(0.0, lo as f32, end_d as f32, (lo + t) as f32, 255);
        }
        if right {
            let start_u = if !vertical {
                w / 2 - t
            } else if up {
                r
            } else {
                l
            };
            let start_d = if !vertical {
                w / 2 - t
            } else if down {
                r
            } else {
                l
            };
            self.fill(start_u as f32, u as f32, w as f32, (u + t) as f32, 255);
            self.fill(start_d as f32, lo as f32, w as f32, (lo + t) as f32, 255);
        }
        if up {
            let end_l = if !horizontal {
                h / 2 + t
            } else if left {
                u + t
            } else {
                lo + t
            };
            let end_r = if !horizontal {
                h / 2 + t
            } else if right {
                u + t
            } else {
                lo + t
            };
            self.fill(l as f32, 0.0, (l + t) as f32, end_l as f32, 255);
            self.fill(r as f32, 0.0, (r + t) as f32, end_r as f32, 255);
        }
        if down {
            let start_l = if !horizontal {
                h / 2 - t
            } else if left {
                lo
            } else {
                u
            };
            let start_r = if !horizontal {
                h / 2 - t
            } else if right {
                lo
            } else {
                u
            };
            self.fill(l as f32, start_l as f32, (l + t) as f32, h as f32, 255);
            self.fill(r as f32, start_r as f32, (r + t) as f32, h as f32, 255);
        }
    }

    /// A quarter curve from the edge midpoint of the horizontal arm to that of
    /// the vertical arm, centred on the same pixel rows/columns as straight
    /// lines so `╭──` joins exactly.
    fn rounded(&mut self, (right, down): (bool, bool)) {
        let (w, h) = (self.w as f32, self.h as f32);
        let t = self.line();
        let cx = ((self.w - t) / 2) as f32 + t as f32 / 2.0;
        let cy = ((self.h - t) / 2) as f32 + t as f32 / 2.0;
        let hx = if right { w + 1.0 } else { -1.0 };
        let vy = if down { h + 1.0 } else { -1.0 };
        let mut pb = PathBuilder::new();
        pb.move_to(hx, cy);
        // Short straight run past the edge, then the curve: continues the
        // neighbouring line without a visible kink.
        pb.quad_to(cx, cy, cx, vy);
        let Some(path) = pb.finish() else { return };
        let stroke = Stroke {
            width: t as f32,
            line_cap: LineCap::Butt,
            ..Stroke::default()
        };
        self.pixmap.stroke_path(
            &path,
            &Self::paint(255, true),
            &stroke,
            Transform::identity(),
            None,
        );
    }

    fn block(&mut self, cp: u32) {
        let (w, h) = (self.w as f32, self.h as f32);
        match cp {
            0x2580 => self.fill(0.0, 0.0, w, (h / 2.0).round(), 255), // upper half
            0x2581..=0x2588 => {
                // lower 1/8 … full block
                let part = (h * (cp - 0x2580) as f32 / 8.0).round();
                self.fill(0.0, h - part, w, h, 255);
            }
            0x2589..=0x258F => {
                // left 7/8 … 1/8
                let part = (w * (0x2590 - cp) as f32 / 8.0).round();
                self.fill(0.0, 0.0, part, h, 255);
            }
            0x2590 => self.fill((w / 2.0).round(), 0.0, w, h, 255), // right half
            0x2591..=0x2593 => {
                // light, medium, dark shade: 25 / 50 / 75 % coverage
                let alpha = ((cp - 0x2590) as f32 / 4.0 * 255.0).round() as u8;
                self.fill(0.0, 0.0, w, h, alpha);
            }
            0x2594 => self.fill(0.0, 0.0, w, (h / 8.0).round().max(1.0), 255), // upper 1/8
            0x2595 => self.fill(w - (w / 8.0).round().max(1.0), 0.0, w, h, 255), // right 1/8
            _ => {}
        }
    }

    fn powerline(&mut self, cp: u32) {
        let (w, h) = (self.w as f32, self.h as f32);
        // Slightly past the cell edges, so no hairline of background shows next to the segment.
        let (x0, x1) = (-0.5, w + 0.5);
        let (y0, y1) = (-0.5, h + 0.5);
        let ym = (y0 + y1) / 2.0;
        let solid = cp.is_multiple_of(2);
        let mut pb = PathBuilder::new();
        let pts: &[(f32, f32)] = match cp {
            0xE0B0 | 0xE0B1 => &[(x0, y0), (x1 - 0.5, ym), (x0, y1)], //  right-pointing
            0xE0B2 | 0xE0B3 => &[(x1, y0), (x0 + 0.5, ym), (x1, y1)], //  left-pointing
            0xE0B4 | 0xE0B5 => {
                //  right half circle
                pb.move_to(x0, y0);
                pb.cubic_to(x1 + w * 0.3, y0, x1 + w * 0.3, y1, x0, y1);
                &[]
            }
            0xE0B6 | 0xE0B7 => {
                //  left half circle
                pb.move_to(x1, y0);
                pb.cubic_to(x0 - w * 0.3, y0, x0 - w * 0.3, y1, x1, y1);
                &[]
            }
            0xE0B8 | 0xE0B9 => &[(x0, y0), (x1, y1), (x0, y1)], //  lower-left triangle
            0xE0BA | 0xE0BB => &[(x1, y0), (x1, y1), (x0, y1)], //  lower-right triangle
            0xE0BC | 0xE0BD => &[(x0, y0), (x1, y0), (x0, y1)], //  upper-left triangle
            _ => &[(x0, y0), (x1, y0), (x1, y1)],               //  upper-right triangle
        };
        if let Some(&(x, y)) = pts.first() {
            pb.move_to(x, y);
            for &(x, y) in &pts[1..] {
                pb.line_to(x, y);
            }
        }
        if solid {
            pb.close();
            if let Some(path) = pb.finish() {
                self.pixmap.fill_path(
                    &path,
                    &Self::paint(255, true),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
            return;
        }
        if matches!(cp, 0xE0B9 | 0xE0BB | 0xE0BD | 0xE0BF) {
            // The thin slants: just the diagonal.
            let (a, b) = if matches!(cp, 0xE0B9 | 0xE0BD) {
                (pts[0], pts[1])
            } else {
                (pts[1], pts[2])
            };
            pb = PathBuilder::new();
            pb.move_to(a.0, a.1);
            pb.line_to(b.0, b.1);
        }
        if let Some(path) = pb.finish() {
            let stroke = Stroke {
                width: thin(self.w) as f32,
                ..Stroke::default()
            };
            self.pixmap.stroke_path(
                &path,
                &Self::paint(255, true),
                &stroke,
                Transform::identity(),
                None,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10x20 cell like the Python tests; "painted" means > 40/255 coverage (░ is 25 %).
    fn render(ch: char) -> impl Fn(u32, u32) -> bool {
        let mask = rasterize(ch, 10, 20).unwrap();
        move |x, y| mask[(y * 10 + x) as usize] > 40
    }

    fn row_full(f: &impl Fn(u32, u32) -> bool, y: u32) -> bool {
        (0..10).all(|x| f(x, y))
    }

    #[test]
    fn strong_lines_are_thicker_and_still_centred() {
        let count = |mask: &[u8]| mask.iter().filter(|&&a| a > 128).count();
        for ch in ['─', '│', '━', '╭', '═', '┼'] {
            let normal = rasterize(ch, 16, 32).unwrap();
            let strong = rasterize_strong(ch, 16, 32, true).unwrap();
            assert!(count(&strong) > count(&normal), "{ch}");
        }
        // Thickness 2 → 4 at a 16 px cell; the line stays on the centre rows.
        let strong = rasterize_strong('─', 16, 32, true).unwrap();
        let rows: Vec<u32> = (0..32).filter(|y| strong[(y * 16) as usize] > 128).collect();
        assert_eq!(rows, [14, 15, 16, 17]);
        // Blocks don't change.
        assert_eq!(rasterize('█', 16, 32), rasterize_strong('█', 16, 32, true));
    }

    #[test]
    fn check_mark_fallback() {
        assert!(has_fallback('✓') && !handles('✓') && !has_fallback('x'));
        let mask = rasterize_fallback('✓', 10, 20).unwrap();
        let painted = mask.iter().filter(|&&a| a > 100).count();
        assert!(painted > 10 && painted < 100, "{painted}");
        assert!(rasterize_fallback('a', 10, 20).is_none());
    }

    #[test]
    fn handles_only_cell_glyphs() {
        for ch in [
            '\u{e0b0}', '\u{e0b2}', '\u{e0b4}', '─', '│', '┼', '╭', '═', '█', '▄', '░',
        ] {
            assert!(handles(ch), "{ch}");
        }
        for ch in ['a', ' ', '╱', '\u{e0c0}', '▖', '日'] {
            assert!(!handles(ch), "{ch}");
        }
    }

    #[test]
    fn cell_glyphs_are_drawn() {
        for ch in [
            '\u{e0b0}', '\u{e0b2}', '\u{e0b4}', '─', '│', '┼', '╭', '═', '█', '▄', '░',
        ] {
            let red = render(ch);
            let painted = (0..10)
                .flat_map(|x| (0..20).map(move |y| (x, y)))
                .filter(|&(x, y)| red(x, y))
                .count();
            assert!(painted > 0, "{ch} drew nothing");
        }
    }

    #[test]
    fn powerline_separator_fills_to_the_cell_edges() {
        let red = render('\u{e0b0}'); // right-pointing triangle: the whole left edge, the tip at the right
        assert!((1..19).all(|y| red(0, y)) && red(9, 10) && !red(9, 1));
        let full = render('█');
        assert!((0..10).all(|x| (0..20).all(|y| full(x, y))));
        let line = render('─'); // a horizontal line runs from edge to edge, so neighbours touch
        let rows: Vec<u32> = (0..20).filter(|&y| row_full(&line, y)).collect();
        assert!(!rows.is_empty());
        // Python's red_row(line, 10): the line sits on row 10 or 9 (the centre).
        assert!(rows.contains(&10) || rows.contains(&9), "{rows:?}");
        assert!(!row_full(&line, 0) && !row_full(&line, 19));
    }

    #[test]
    fn lines_join_across_cells() {
        // The horizontal line's rows in ─ are the same rows the arms of ┼, ╭ and ├ use,
        // and the vertical line's columns in │ match those of ┼ and ╭.
        let (w, h) = (9, 23);
        let rows = |ch: char| -> Vec<u32> {
            let m = rasterize(ch, w, h).unwrap();
            (0..h)
                .filter(|&y| m[(y * w) as usize] > 128 || m[(y * w + w - 1) as usize] > 128)
                .collect()
        };
        let cols = |ch: char, y: u32| -> Vec<u32> {
            let m = rasterize(ch, w, h).unwrap();
            (0..w).filter(|&x| m[(y * w + x) as usize] > 128).collect()
        };
        let line = rows('─');
        assert_eq!(rows('┼'), line);
        assert_eq!(rows('├'), line);
        assert_eq!(cols('│', 0), cols('┼', 0));
        assert_eq!(cols('│', h - 1), cols('┬', h - 1));
        // ╭ ends on the line's rows at its right edge and the column's at the bottom.
        let m = rasterize('╭', w, h).unwrap();
        for &y in &line {
            assert!(m[(y * w + w - 1) as usize] > 100, "╭ right edge at row {y}");
        }
        for x in cols('│', 0) {
            assert!(
                m[((h - 1) * w + x) as usize] > 100,
                "╭ bottom edge at col {x}"
            );
        }
    }

    #[test]
    fn heavy_is_twice_light() {
        let count = |ch| {
            rasterize(ch, 16, 32)
                .unwrap()
                .iter()
                .filter(|&&a| a == 255)
                .count()
        };
        assert_eq!(count('━'), 2 * count('─'));
        assert_eq!(thin(16), 2);
        assert_eq!(thin(3), 1);
    }

    #[test]
    fn double_lines_have_two_strokes_and_corners() {
        let (w, h) = (10, 20);
        let m = rasterize('═', w, h).unwrap();
        let painted: Vec<u32> = (0..h).filter(|&y| m[(y * w) as usize] == 255).collect();
        assert_eq!(painted.len(), 2, "two separate lines: {painted:?}");
        assert!(painted[1] - painted[0] > 1, "with a gap between");
        // ╔ joins ═ on the right and ║ at the bottom.
        let c = rasterize('╔', w, h).unwrap();
        for &y in &painted {
            assert_eq!(c[(y * w + w - 1) as usize], 255);
        }
        let v = rasterize('║', w, h).unwrap();
        for x in 0..w {
            assert_eq!(
                v[((h - 1) * w + x) as usize],
                c[((h - 1) * w + x) as usize],
                "col {x}"
            );
        }
        // Nothing left of the outer vertical line.
        assert_eq!(c[(painted[1] * w) as usize], 0);
    }

    #[test]
    fn blocks_and_shades() {
        let lower = rasterize('▄', 10, 20).unwrap();
        assert_eq!(lower[0], 0);
        assert_eq!(lower[19 * 10], 255);
        assert_eq!(lower.iter().filter(|&&a| a == 255).count(), 100);
        let shade = rasterize('░', 10, 20).unwrap();
        assert!(shade.iter().all(|&a| a == 64));
        let left = rasterize('▌', 10, 20).unwrap();
        assert_eq!(left[4], 255);
        assert_eq!(left[5], 0);
        assert!(rasterize('a', 10, 20).is_none());
        assert!(rasterize('█', 0, 20).is_none());
    }
}
