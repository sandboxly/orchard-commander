//! The Sample on the Appearance page: a small fake file panel and key bar in
//! the current appearance (port of `SamplePanel` in `settings_dialog.py`).

use orchard_grid::{Flags, Frame, Style};

use crate::geom::Rect;
use crate::widgets::{BoxFrame, DrawCtx, fill, put, put_right, width};

/// (name, kind, size): kinds pick the color as in the file panel.
pub const SAMPLE_ROWS: [(&str, &str, &str); 8] = [
    ("..", "dir", "<UP>"),
    ("src", "dir", "<DIR>"),
    ("build.sh", "exec", "1.2K"),
    ("latest", "link", "<LNK>"),
    (".env", "hidden", "312"),
    ("notes.md", "marked", "4.0K"),
    ("README.md", "file", "8.1K"),
    ("setup.cfg", "file", "640"),
];
pub const SAMPLE_CURSOR: usize = 6;
pub const SAMPLE_KEYS: [(&str, &str); 3] = [("F5", "Copy"), ("F6", "Move"), ("F7", "MkDir")];

/// Width in cells (1.x: 30 × the font's "M").
pub const WIDTH: u16 = 30;
/// Frame (2) + header + rows + key bar.
pub const HEIGHT: u16 = 2 + 1 + SAMPLE_ROWS.len() as u16 + 1;

/// Draw the sample with its top-left corner at `r`'s.
pub fn draw(f: &mut Frame, r: Rect, ctx: &DrawCtx) {
    let c = &ctx.theme.colors;
    let r = Rect::new(r.x, r.y, r.w.min(WIDTH), r.h.min(HEIGHT));
    if r.h < 4 {
        return;
    }
    let (panel, bar) = r.split_bottom(1);
    let inner = BoxFrame { title: "~/projects/orchard", subtitle: "1 marked", active: true }.draw(f, panel, ctx);
    let head = Style::new(c.accent, c.surface).with(Flags::BOLD);
    put(f, inner.x + 1, inner.y, "Name", head, inner.w.saturating_sub(1));
    put_right(f, inner.right().saturating_sub(1), inner.y, "Size", head, inner.x + 6);
    for (i, (name, kind, size)) in SAMPLE_ROWS.iter().enumerate() {
        let y = inner.y + 1 + i as u16;
        if y >= inner.bottom() {
            break;
        }
        let row = Rect::new(inner.x, y, inner.w, 1);
        let mut fg = match *kind {
            "dir" => c.dir,
            "exec" => c.exec,
            "link" => c.link,
            "hidden" => c.hidden,
            "marked" => c.marked,
            _ => c.foreground,
        };
        let mut bg = c.surface;
        if i == SAMPLE_CURSOR {
            bg = c.cursor_bg;
            fg = c.cursor_fg;
        }
        let mut style = Style::new(fg, bg);
        if matches!(*kind, "dir" | "marked") {
            style = style.with(Flags::BOLD);
        }
        if *kind == "link" {
            style = style.with(Flags::ITALIC);
        }
        fill(f, row, bg);
        let mark = if *kind == "marked" { "✓" } else { " " };
        let x = put(f, row.x, y, mark, style, 1);
        put(f, x, y, name, style, row.w.saturating_sub(8));
        put_right(f, row.right().saturating_sub(1), y, size, style, row.x + 10);
    }
    fill(f, bar, c.footer_bg);
    let mut x = bar.x + 1;
    for (key, label) in SAMPLE_KEYS {
        x = put(f, x, bar.y, key, Style::new(c.footer_key, c.footer_bg).with(Flags::BOLD), bar.right().saturating_sub(x));
        x = put(f, x + 1, bar.y, label, Style::new(c.footer_fg, c.footer_bg), bar.right().saturating_sub(x + 1));
        x += 2;
        if x + width(key) >= bar.right() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn sample_shows_rows_cursor_and_keys() {
        let theme = Theme::default_dark();
        let ctx = DrawCtx::new(&theme, (8.0, 16.0));
        let mut f = Frame::new(WIDTH, HEIGHT, theme.colors.background);
        draw(&mut f, Rect::new(0, 0, WIDTH, HEIGHT), &ctx);
        assert!(f.row_text(0).contains("~/projects/orchard"));
        assert!(f.row_text(1).contains("Name") && f.row_text(1).contains("Size"));
        assert!(f.row_text(7).contains("✓notes.md"));
        assert_eq!(f.cell(3, 8).unwrap().bg, theme.colors.cursor_bg, "README.md has the cursor");
        assert!(f.row_text(HEIGHT - 1).starts_with(" F5 Copy  F6 Move  F7 MkDir"));
    }
}
