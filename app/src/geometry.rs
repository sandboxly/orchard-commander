//! The main window's size and place, kept like 1.x: the base64 of Qt's
//! `QWidget::saveGeometry()` in `extra["geometry"]`, so an upgrade opens
//! the window where 1.x left it (maximized too), and 1.x can read ours.
//!
//! The blob (big-endian, `QDataStream`): magic `0x1D9D0CB`, version (u16
//! major, u16 minor), frame rect, normal rect, i32 screen number, u8
//! maximized, u8 full screen, then i32 screen width (version 2+) and the
//! client rect (version 3+). A rect is i32 left, top, right, bottom
//! (inclusive). All in logical pixels.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

const MAGIC: u32 = 0x01D9_D0CB;

/// A rectangle in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// The window including its title bar.
    pub frame: Rect,
    /// The content area when not maximized (or full screen).
    pub normal: Rect,
    /// The content area now.
    pub client: Rect,
    pub screen_width: i32,
    pub maximized: bool,
    pub full_screen: bool,
}

pub fn decode(text: &str) -> Option<Geometry> {
    let bytes = STANDARD.decode(text.trim()).ok()?;
    let mut r = Reader(&bytes);
    if r.u32()? != MAGIC {
        return None;
    }
    let (major, _minor) = (r.u16()?, r.u16()?);
    if !(1..=3).contains(&major) {
        return None;
    }
    let frame = r.rect()?;
    let normal = r.rect()?;
    let _screen = r.i32()?;
    let maximized = r.u8()? != 0;
    let full_screen = r.u8()? != 0;
    let screen_width = if major >= 2 { r.i32()? } else { 0 };
    let client = if major >= 3 { r.rect()? } else { normal };
    Some(Geometry { frame, normal, client, screen_width, maximized, full_screen })
}

/// Version 3.0, as Qt 6 writes it (screen number 0).
pub fn encode(g: &Geometry) -> String {
    let mut out = Vec::with_capacity(66);
    out.extend(MAGIC.to_be_bytes());
    out.extend(3u16.to_be_bytes());
    out.extend(0u16.to_be_bytes());
    put_rect(&mut out, g.frame);
    put_rect(&mut out, g.normal);
    out.extend(0i32.to_be_bytes());
    out.push(if g.maximized { 2 } else { 0 }); // Qt::WindowMaximized
    out.push(if g.full_screen { 4 } else { 0 }); // Qt::WindowFullScreen
    out.extend(g.screen_width.to_be_bytes());
    put_rect(&mut out, g.client);
    STANDARD.encode(out)
}

fn put_rect(out: &mut Vec<u8>, r: Rect) {
    for v in [r.x, r.y, r.x + r.w - 1, r.y + r.h - 1] {
        out.extend(v.to_be_bytes());
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take::<1>().map(|b| b[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take().map(u16::from_be_bytes)
    }

    fn u32(&mut self) -> Option<u32> {
        self.take().map(u32::from_be_bytes)
    }

    fn i32(&mut self) -> Option<i32> {
        self.take().map(i32::from_be_bytes)
    }

    fn rect(&mut self) -> Option<Rect> {
        let (l, t, r, b) = (self.i32()?, self.i32()?, self.i32()?, self.i32()?);
        Some(Rect { x: l, y: t, w: r - l + 1, h: b - t + 1 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Written by PySide6 6.12 (`setGeometry(100, 120, 900, 700)`, offscreen).
    const NORMAL: &str = "AdnQywADAAAAAABiAAAAdgAAA+kAAAM1AAAAZAAAAHgAAAPnAAADMwAAAAAAAAAAAyAAAABkAAAAeAAAA+cAAAMz";
    // The same window after `showMaximized()`.
    const MAXIMIZED: &str = "AdnQywADAAAAAAAAAAAAAAAAAx8AAAMfAAAAZAAAAHgAAAPnAAADMwAAAAACAAAAAyAAAAACAAAAAgAAAx0AAAMd";

    #[test]
    fn reads_what_qt_wrote() {
        let g = decode(NORMAL).unwrap();
        assert_eq!(g.client, Rect { x: 100, y: 120, w: 900, h: 700 });
        assert_eq!(g.normal, g.client);
        assert_eq!(g.frame, Rect { x: 98, y: 118, w: 904, h: 704 });
        assert_eq!(g.screen_width, 800);
        assert!(!g.maximized && !g.full_screen);
        let m = decode(MAXIMIZED).unwrap();
        assert!(m.maximized);
        assert_eq!(m.normal, Rect { x: 100, y: 120, w: 900, h: 700 }, "the size to go back to");
    }

    #[test]
    fn writes_what_qt_writes() {
        assert_eq!(encode(&decode(NORMAL).unwrap()), NORMAL);
        assert_eq!(encode(&decode(MAXIMIZED).unwrap()), MAXIMIZED);
    }

    #[test]
    fn rejects_other_data() {
        assert_eq!(decode(""), None);
        assert_eq!(decode("not base64!"), None);
        assert_eq!(decode(&NORMAL[..20]), None);
        assert_eq!(decode(&STANDARD.encode([0u8; 66])), None);
    }
}
