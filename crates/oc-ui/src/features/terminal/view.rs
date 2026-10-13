//! Drawing a session's screen into the grid, and keys → shell bytes (port of
//! `TerminalView.paintEvent` / `key_to_text`).
//!
//! The screen is copied out under the session's lock ([`oc_term::Snapshot`],
//! colors already resolved against the theme's terminal palette) and then
//! written cell by cell; the grid renderer does the rest: bold/italic faces,
//! underline/strikeout, dim, wide characters, box/block/powerline glyphs
//! drawn geometrically, Nerd Font fallback for prompt icons.

use std::borrow::Cow;

use oc_grid::{Cursor, CursorShape, Flags, Frame, Rgba, Style};
use oc_term::{CellFlags, KeyOptions, Modifiers, Palette, Rgb, Snapshot};

use super::Terminal;
use crate::app::App;
use crate::geom::Rect;
use crate::input::{Key, KeyEvent, Mods};
use crate::widgets::{BoxFrame, DrawCtx};

fn rgb(c: Rgba) -> Rgb {
    Rgb::new(c.r, c.g, c.b)
}

fn rgba(c: Rgb) -> Rgba {
    Rgba::rgb(c.r, c.g, c.b)
}

/// The theme's terminal palette: its text and panel colors, xterm's 16
/// colors softened for dark or light schemes.
pub fn palette(ctx: &DrawCtx) -> Palette {
    let c = &ctx.theme.colors;
    Palette::for_theme(rgb(c.foreground), rgb(c.surface), ctx.theme.scheme.dark)
}

pub(super) fn draw(app: &App, t: &Terminal, f: &mut Frame, area: Rect, ctx: &DrawCtx) {
    if area.h < 3 || area.w < 4 {
        return;
    }
    let side = app.main.active;
    let title = t.title(side);
    let inner = BoxFrame { title: &title, subtitle: "", active: t.focused }.draw(f, area, ctx);
    let Some(session) = t.session(side) else { return };
    let mut snap = t.snapshot.borrow_mut();
    session.lock().snapshot_into(&palette(ctx), &mut snap);
    paint(f, inner, &snap, ctx, t.focused);
}

/// Write a snapshot into `inner` (clipped to it), plus the cursor.
pub fn paint(f: &mut Frame, inner: Rect, snap: &Snapshot, ctx: &DrawCtx, focused: bool) {
    let c = &ctx.theme.colors;
    let rows = snap.lines.min(inner.h as usize);
    let cols = snap.columns.min(inner.w as usize);
    let mut text = String::new();
    for line in 0..rows {
        let y = inner.y + line as u16;
        let row = snap.row(line);
        for (col, cell) in row.iter().enumerate().take(cols) {
            let x = inner.x + col as u16;
            let flags = cell.flags;
            let (mut fg, mut bg) = (rgba(cell.fg), rgba(cell.bg));
            // Selection shows inverted; on inverse text it flips back.
            if flags.contains(CellFlags::INVERSE) != flags.contains(CellFlags::SELECTED) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if flags.contains(CellFlags::HIDDEN) {
                fg = bg;
            }
            let mut g = Flags::NONE;
            g.set(Flags::BOLD, flags.contains(CellFlags::BOLD));
            g.set(Flags::ITALIC, flags.contains(CellFlags::ITALIC));
            g.set(Flags::UNDERLINE, flags.contains(CellFlags::UNDERLINE));
            g.set(Flags::STRIKETHROUGH, flags.contains(CellFlags::STRIKEOUT));
            g.set(Flags::DIM, flags.contains(CellFlags::DIM));
            let style = Style::new(fg, bg).with(g);
            if flags.contains(CellFlags::WIDE_SPACER) {
                // Already covered by the wide character before it, unless the
                // font's idea of its width differs: then just the background.
                if !f.cell(x, y).is_some_and(|c| c.flags.contains(Flags::WIDE_CONT)) {
                    f.put_char(x, y, ' ', style);
                }
                continue;
            }
            let ch = if cell.ch == '\0' || cell.ch.is_control() { ' ' } else { cell.ch };
            if flags.contains(CellFlags::COMBINING) {
                text.clear();
                text.push(ch);
                text.extend(snap.combining_at(line * snap.columns + col));
                let span = if flags.contains(CellFlags::WIDE) { 2 } else { 1 };
                f.put_str_clipped(x, y, &text, style, x + span);
            } else {
                f.put_char(x, y, ch, style);
            }
        }
    }
    let Some(cur) = snap.cursor else { return };
    if cur.line >= rows || cur.column >= cols {
        return;
    }
    let (col, row) = (inner.x + cur.column as u16, inner.y + cur.line as u16);
    if focused {
        let shape = match cur.shape {
            oc_term::CursorShape::Block => CursorShape::Block,
            oc_term::CursorShape::Underline => CursorShape::Underline,
            oc_term::CursorShape::Beam => CursorShape::Beam,
            oc_term::CursorShape::HollowBlock => CursorShape::HollowBlock,
        };
        f.cursor = Some(Cursor { col, row, shape, color: c.cursor_bg, text: Some(c.cursor_fg) });
    } else if f.cursor.is_none() {
        // Without the focus: an outline, like 1.x (and only if nothing else has a cursor).
        f.cursor = Some(Cursor { col, row, shape: CursorShape::HollowBlock, color: c.cursor_bg, text: None });
    }
}

/// The modifiers as the terminal sees them: Control is the Control key (⌃ on macOS, not ⌘).
pub fn term_mods(m: Mods) -> Modifiers {
    Modifiers { shift: m.shift, alt: m.alt, ctrl: m.ctrl }
}

/// What a key press sends to the shell (`None`: nothing).
pub fn encode_key(k: &KeyEvent, opts: KeyOptions) -> Option<Vec<u8>> {
    use oc_term::Key as T;
    let key = match k.key {
        Key::Char(c) => T::Char(c),
        Key::Space => T::Char(' '),
        Key::Enter => T::Enter,
        Key::Escape => T::Escape,
        Key::Tab => T::Tab,
        Key::Backspace => T::Backspace,
        Key::Delete => T::Delete,
        Key::Insert => T::Insert,
        Key::Home => T::Home,
        Key::End => T::End,
        Key::PageUp => T::PageUp,
        Key::PageDown => T::PageDown,
        Key::Up => T::Up,
        Key::Down => T::Down,
        Key::Left => T::Left,
        Key::Right => T::Right,
        Key::F(n) => T::F(n),
        Key::Other => return None,
    };
    let text = match (&k.text, k.key) {
        (Some(t), _) => Cow::Borrowed(t.as_str()),
        (None, Key::Space) if !k.mods.ctrl => Cow::Borrowed(" "),
        _ => Cow::Borrowed(""),
    };
    oc_term::encode_key(key, term_mods(k.mods), &text, opts).map(Cow::into_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(k: KeyEvent, app_cursor: bool) -> Vec<u8> {
        encode_key(&k, KeyOptions { app_cursor, alt_sends_escape: !cfg!(target_os = "macos") }).unwrap_or_default()
    }

    const CTRL: Mods = Mods { ctrl: true, shift: false, alt: false, logo: false };

    /// Port of test_terminal.py's test_key_translation.
    #[test]
    fn key_translation() {
        assert_eq!(enc(KeyEvent::new(Key::Char('c'), CTRL), false), b"\x03");
        assert_eq!(enc(KeyEvent::new(Key::Char('d'), CTRL), false), b"\x04");
        assert_eq!(enc(KeyEvent::plain(Key::Up), false), b"\x1b[A");
        assert_eq!(enc(KeyEvent::plain(Key::Up), true), b"\x1bOA");
        assert_eq!(enc(KeyEvent::plain(Key::Enter), false), b"\r");
        assert_eq!(enc(KeyEvent::plain(Key::Backspace), false), b"\x7f");
        assert_eq!(enc(KeyEvent::plain(Key::Escape), false), b"\x1b");
        assert_eq!(enc(KeyEvent::plain(Key::Tab), false), b"\t");
        assert_eq!(enc(KeyEvent::new(Key::Tab, Mods::SHIFT), false), b"\x1b[Z");
        assert_eq!(enc(KeyEvent::new(Key::Left, Mods::ALT), false), b"\x1bb");
        assert_eq!(enc(KeyEvent::plain(Key::F(5)), false), b"\x1b[15~");
        assert_eq!(enc(KeyEvent::typed("a"), false), b"a");
        assert_eq!(enc(KeyEvent::typed("A"), false), b"A");
        assert_eq!(enc(KeyEvent::plain(Key::Space), false), b" ");
        assert_eq!(enc(KeyEvent::new(Key::Space, CTRL), false), b"\x00");
        assert_eq!(enc(KeyEvent::typed("ø"), false), "ø".as_bytes());
    }

    #[test]
    fn snapshot_cells_become_grid_cells() {
        let theme = crate::theme::Theme::default_dark();
        let ctx = DrawCtx { theme: &theme, cell_px: (8.0, 16.0), window_focused: true };
        let mut screen = oc_term::Screen::new(20, 3);
        screen.feed("a\x1b[1;31mB\x1b[0m\x1b[7mi\x1b[0m界x\r\n\u{e0b0}─".as_bytes());
        let snap = screen.snapshot(&palette(&ctx));
        let mut f = Frame::new(22, 5, theme.colors.background);
        paint(&mut f, Rect::new(1, 1, 20, 3), &snap, &ctx, true);
        assert_eq!(f.row_text(1).trim_end(), " aBi界x");
        let bold = f.cell(2, 1).unwrap();
        assert!(bold.flags.contains(Flags::BOLD));
        assert_eq!(bold.fg, rgba(oc_term::DARK_ANSI[9])); // bold red shows bright
        let inverse = f.cell(3, 1).unwrap();
        assert_eq!((inverse.fg, inverse.bg), (theme.colors.surface, theme.colors.foreground));
        assert!(f.cell(4, 1).unwrap().flags.contains(Flags::WIDE));
        assert!(f.cell(5, 1).unwrap().flags.contains(Flags::WIDE_CONT));
        assert_eq!(f.cell(6, 1).unwrap().ch, 'x');
        assert_eq!(f.cell(1, 2).unwrap().ch, '\u{e0b0}');
        assert_eq!(f.cell(2, 2).unwrap().ch, '─');
        let cursor = f.cursor.unwrap();
        assert_eq!((cursor.col, cursor.row, cursor.shape), (3, 2, CursorShape::Block));
        // Without the focus: an outline.
        let mut f = Frame::new(22, 5, theme.colors.background);
        paint(&mut f, Rect::new(1, 1, 20, 3), &snap, &ctx, false);
        assert_eq!(f.cursor.unwrap().shape, CursorShape::HollowBlock);
    }
}
