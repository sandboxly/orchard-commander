//! The widget kit, drawn into an `orchard_grid::Frame`.
//!
//! Every widget is plain state plus two functions:
//! `draw(&state, &mut Frame, Rect, &DrawCtx)` paints it into a rectangle,
//! `handle_*(&mut state, event, …) -> bool/Outcome` reacts to input. Layout
//! is computed from the rectangle on both paths, so mouse hit testing needs
//! no retained geometry.

pub mod boxframe;
pub mod button;
pub mod checkbox;
pub mod list;
pub mod menu;
pub mod scrollbar;
pub mod text_input;

pub use boxframe::{BoxFrame, Tab};
pub use button::{Button, ButtonRow};
pub use checkbox::Checkbox;
pub use list::ListState;
pub use menu::{DropdownMenu, MenuEntry, MenuOutcome};
pub use scrollbar::Scrollbar;
pub use text_input::TextInput;

use orchard_grid::{Area, FillRect, Flags, Frame, ImagePlacement, PxRect, Rgba, Style};
use unicode_width::UnicodeWidthStr;

use crate::geom::Rect;
use crate::theme::Theme;

/// What drawing needs besides the state.
#[derive(Clone, Copy)]
pub struct DrawCtx<'a> {
    pub theme: &'a Theme,
    /// A cell's size in physical pixels (for pixel-exact overlays).
    pub cell_px: (f32, f32),
    /// The window has keyboard focus (cursor bars look inactive otherwise).
    pub window_focused: bool,
}

impl<'a> DrawCtx<'a> {
    pub fn new(theme: &'a Theme, cell_px: (f32, f32)) -> Self {
        DrawCtx { theme, cell_px, window_focused: true }
    }
}

/// Hide what was drawn so far as pixel overlays under `r`: a dialog, menu
/// or toast drawn over a panel must not let the panel's pixel strips and
/// scrollbars (which the renderer paints after all cell backgrounds) show
/// through. Rects are cut around `r`; images are clipped to what stays
/// visible around it (an image placement may be split into up to four).
pub fn cover(f: &mut Frame, r: Rect, ctx: &DrawCtx) {
    let (cw, ch) = ctx.cell_px;
    let to_px = |a: Area| match a {
        Area::Px(p) => p,
        Area::Cells(c) => PxRect::new(c.col as f32 * cw, c.row as f32 * ch, c.cols as f32 * cw, c.rows as f32 * ch),
    };
    let hole = to_px(Area::Cells(r.cell_rect()));
    let old = std::mem::take(&mut f.rects);
    for fr in old {
        for piece in subtract(to_px(fr.area), hole) {
            f.rects.push(FillRect { area: Area::Px(piece), ..fr });
        }
    }
    let old = std::mem::take(&mut f.images);
    for img in old {
        let dest = to_px(img.dest);
        let visible = img.clip.map_or(dest, |c| dest.intersect(&to_px(c)));
        if visible.intersect(&hole).is_empty() {
            f.images.push(img);
            continue;
        }
        for piece in subtract(visible, hole) {
            f.images.push(ImagePlacement { clip: Some(Area::Px(piece)), ..img });
        }
    }
}

/// `a` minus `b`, as up to four rectangles.
pub fn subtract(a: PxRect, b: PxRect) -> Vec<PxRect> {
    let i = a.intersect(&b);
    if i.is_empty() {
        return vec![a];
    }
    let mut out = Vec::with_capacity(4);
    if i.y > a.y {
        out.push(PxRect::new(a.x, a.y, a.w, i.y - a.y));
    }
    if i.bottom() < a.bottom() {
        out.push(PxRect::new(a.x, i.bottom(), a.w, a.bottom() - i.bottom()));
    }
    if i.x > a.x {
        out.push(PxRect::new(a.x, i.y, i.x - a.x, i.h));
    }
    if i.right() < a.right() {
        out.push(PxRect::new(i.right(), i.y, a.right() - i.right(), i.h));
    }
    out.retain(|r| !r.is_empty());
    out
}

/// Display width in cells.
pub fn width(text: &str) -> u16 {
    text.width().min(u16::MAX as usize) as u16
}

/// Write `text` at (x, y), never past `x + max`. Returns the column after it.
pub fn put(f: &mut Frame, x: u16, y: u16, text: &str, style: Style, max: u16) -> u16 {
    f.put_str_clipped(x, y, text, style, x.saturating_add(max))
}

/// `text` shortened with "…" at the end to fit `max` cells, then written.
pub fn put_elided(f: &mut Frame, x: u16, y: u16, text: &str, style: Style, max: u16) -> u16 {
    let t = orchard_grid::elide_right(text, max as usize);
    put(f, x, y, &t, style, max)
}

/// `text` right-aligned so it ends at column `right` (exclusive), clipped on
/// the left at `min_x`.
pub fn put_right(f: &mut Frame, right: u16, y: u16, text: &str, style: Style, min_x: u16) -> u16 {
    let avail = right.saturating_sub(min_x);
    let t = orchard_grid::elide_left(text, avail as usize);
    let w = width(&t);
    put(f, right.saturating_sub(w), y, &t, style, w)
}

/// Fill a rectangle with blanks in `bg`.
pub fn fill(f: &mut Frame, r: Rect, bg: Rgba) {
    if !r.is_empty() {
        f.fill(r.cell_rect(), ' ', Style::new(bg, bg));
    }
}

/// A label with an `&`-marked hotkey ("&Overwrite", "S&kip all"): the text
/// without the marker and the hotkey (lower case) with its char index.
pub fn parse_hotkey(label: &str) -> (String, Option<(usize, char)>) {
    let mut out = String::with_capacity(label.len());
    let mut hot = None;
    let mut chars = label.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            match chars.next() {
                Some('&') => out.push('&'),
                Some(n) => {
                    if hot.is_none() {
                        hot = Some((out.chars().count(), n.to_lowercase().next().unwrap_or(n)));
                    }
                    out.push(n);
                }
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    (out, hot)
}

#[allow(clippy::too_many_arguments)]
/// Draw a label whose hotkey char (index into chars) is underlined in `hot`.
pub fn put_hotkey_label(
    f: &mut Frame,
    x: u16,
    y: u16,
    text: &str,
    hotkey: Option<(usize, char)>,
    style: Style,
    hot: Rgba,
    max: u16,
) -> u16 {
    let mut col = x;
    let end = x.saturating_add(max);
    for (i, ch) in text.chars().enumerate() {
        if col >= end {
            break;
        }
        let s = if hotkey.is_some_and(|(hi, _)| hi == i) { Style { fg: hot, ..style }.with(Flags::UNDERLINE) } else { style };
        col = f.put_str_clipped(col, y, ch.encode_utf8(&mut [0; 4]), s, end);
    }
    col
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkeys() {
        assert_eq!(parse_hotkey("&Overwrite"), ("Overwrite".into(), Some((0, 'o'))));
        assert_eq!(parse_hotkey("S&kip all"), ("Skip all".into(), Some((1, 'k'))));
        assert_eq!(parse_hotkey("Fish && Chips"), ("Fish & Chips".into(), None));
    }

    #[test]
    fn subtracting_rects() {
        let a = PxRect::new(0.0, 0.0, 10.0, 10.0);
        assert_eq!(subtract(a, PxRect::new(20.0, 0.0, 5.0, 5.0)), vec![a]);
        let parts = subtract(a, PxRect::new(2.0, 2.0, 4.0, 4.0));
        let area: f32 = parts.iter().map(|r| r.w * r.h).sum();
        assert_eq!((parts.len(), area), (4, 84.0));
        assert!(subtract(a, PxRect::new(-1.0, -1.0, 20.0, 20.0)).is_empty());
    }

    #[test]
    fn cover_clips_images_around_the_hole() {
        let theme = crate::theme::Theme::default_dark();
        let ctx = DrawCtx::new(&theme, (10.0, 20.0));
        let mut f = Frame::new(20, 10, Rgba::BLACK);
        // An image over cells 0..10 x 0..5 (100 x 100 px), another far away.
        let id = image_id;
        f.add_image(ImagePlacement::new(id(1), PxRect::new(0.0, 0.0, 100.0, 100.0)));
        f.add_image(ImagePlacement::new(id(2), PxRect::new(150.0, 150.0, 20.0, 20.0)));
        // A dialog over cells 2..6 x 1..3 (px 20..60 x 20..60).
        cover(&mut f, Rect::new(2, 1, 4, 2), &ctx);
        let first: Vec<_> = f.images.iter().filter(|i| i.image == id(1)).collect();
        assert_eq!(first.len(), 4, "split around the hole, not dropped");
        let area: f32 = first.iter().map(|i| match i.clip { Some(Area::Px(p)) => p.w * p.h, _ => 0.0 }).sum();
        assert_eq!(area, 100.0 * 100.0 - 40.0 * 40.0);
        assert!(f.images.iter().any(|i| i.image == id(2) && i.clip.is_none()), "untouched images stay as they are");
    }

    /// An `ImageId` for tests (only renderers make real ones; its field is private).
    fn image_id(n: u64) -> orchard_grid::ImageId {
        assert_eq!(std::mem::size_of::<orchard_grid::ImageId>(), std::mem::size_of::<u64>());
        // SAFETY: ImageId is a plain `u64` newtype; the id is only compared here.
        unsafe { std::mem::transmute::<u64, orchard_grid::ImageId>(n) }
    }

    #[test]
    fn right_aligned_text() {
        let mut f = Frame::new(10, 1, Rgba::BLACK);
        put_right(&mut f, 10, 0, "abc", Style::default(), 0);
        assert_eq!(f.row_text(0), "       abc");
    }
}
