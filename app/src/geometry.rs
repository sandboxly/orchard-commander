//! The main window's size, position and state, kept like 1.x: Qt's
//! `saveGeometry()` as base64 in `extra["geometry"]` (so an upgrade keeps the
//! window where it was, and 1.x can still read what 2.x wrote), restored
//! like `restoreGeometry()`: maximized / full screen, and moved back onto a
//! screen when the saved place is off every connected one.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// The settings key (1.x's).
pub const KEY: &str = "geometry";
/// Where 2.0 betas kept a plain `{width, height, x, y}` (read when there is no `geometry`).
pub const LEGACY_KEY: &str = "window_geometry";
/// 1.x's size without a saved geometry (`self.resize(1280, 800)`).
pub const DEFAULT_SIZE: (i32, i32) = (1280, 800);

const MAGIC: u32 = 0x01D9_D0CB;

/// A rectangle in logical pixels (Qt's device-independent coordinates).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Rect { x, y, w, h }
    }

    fn right(&self) -> i32 {
        self.x + self.w - 1
    }

    fn bottom(&self) -> i32 {
        self.y + self.h - 1
    }

    fn contains_point(&self, x: i32, y: i32) -> bool {
        x >= self.x && x <= self.right() && y >= self.y && y <= self.bottom()
    }

    fn contains(&self, r: &Rect) -> bool {
        r.x >= self.x && r.y >= self.y && r.right() <= self.right() && r.bottom() <= self.bottom()
    }
}

/// What `QWidget::saveGeometry()` writes (format 3.0, Qt 5.12 and later).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QtGeometry {
    /// The window with its title bar.
    pub frame: Rect,
    /// The content area when neither maximized nor full screen.
    pub normal: Rect,
    /// Index of the window's screen.
    pub screen: i32,
    pub maximized: bool,
    pub full_screen: bool,
    /// That screen's width (format 2.0 on).
    pub screen_width: i32,
    /// The content area now (format 3.0 on).
    pub geometry: Rect,
}

impl QtGeometry {
    /// Read a base64 `saveGeometry()` blob (formats 1.0–3.x).
    pub fn decode(b64: &str) -> Option<Self> {
        let data = STANDARD.decode(b64.trim()).ok()?;
        let mut r = Reader(&data);
        if r.u32()? != MAGIC {
            return None;
        }
        let major = r.u16()?;
        let _minor = r.u16()?;
        if !(1..=3).contains(&major) {
            return None;
        }
        let frame = r.rect()?;
        let normal = r.rect()?;
        let screen = r.i32()?;
        let maximized = r.u8()? != 0;
        let full_screen = r.u8()? != 0;
        let screen_width = if major >= 2 { r.i32()? } else { 0 };
        let geometry = if major >= 3 { r.rect()? } else { normal };
        Some(QtGeometry { frame, normal, screen, maximized, full_screen, screen_width, geometry })
    }

    /// The base64 blob Qt 6 writes (format 3.0).
    pub fn encode(&self) -> String {
        let mut out = Vec::with_capacity(66);
        out.extend_from_slice(&MAGIC.to_be_bytes());
        out.extend_from_slice(&3u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        put_rect(&mut out, self.frame);
        put_rect(&mut out, self.normal);
        out.extend_from_slice(&self.screen.to_be_bytes());
        // Qt stores `windowState() & flag`: 2 for maximized, 4 for full screen.
        out.push(if self.maximized { 2 } else { 0 });
        out.push(if self.full_screen { 4 } else { 0 });
        out.extend_from_slice(&self.screen_width.to_be_bytes());
        put_rect(&mut out, self.geometry);
        STANDARD.encode(out)
    }

    /// Title bar height (and left border) as saved; Qt's guess when the window
    /// was maximized or full screen (the frame then tells nothing).
    fn frame_offset(&self) -> (i32, i32) {
        let (dx, dy) = (self.geometry.x - self.frame.x, self.geometry.y - self.frame.y);
        if self.maximized || self.full_screen || dy <= 0 || dy > 100 || !(0..=50).contains(&dx) {
            (0, DEFAULT_TITLE_BAR)
        } else {
            (dx, dy)
        }
    }
}

/// Title bar height when the saved geometry doesn't tell (Qt: `PM_TitleBarHeight`).
const DEFAULT_TITLE_BAR: i32 = 28;

/// QRect is streamed as left, top, right, bottom (right = left + width - 1).
fn put_rect(out: &mut Vec<u8>, r: Rect) {
    for v in [r.x, r.y, r.right(), r.bottom()] {
        out.extend_from_slice(&v.to_be_bytes());
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_at_checked(N)?;
        self.0 = rest;
        head.try_into().ok()
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
        Some(Rect::new(l, t, r - l + 1, b - t + 1))
    }
}

/// How to open the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Restore {
    /// Content size (logical).
    pub size: (i32, i32),
    /// Content top-left (logical); None: let the system place it.
    pub content_pos: Option<(i32, i32)>,
    /// Title bar / border between the window's and the content's top-left.
    pub frame_offset: (i32, i32),
    pub maximized: bool,
    pub full_screen: bool,
}

impl Restore {
    /// The window's own top-left (title bar included).
    pub fn outer_pos(&self) -> Option<(i32, i32)> {
        self.content_pos.map(|(x, y)| (x - self.frame_offset.0, y - self.frame_offset.1))
    }
}

impl Default for Restore {
    fn default() -> Self {
        Restore { size: DEFAULT_SIZE, content_pos: None, frame_offset: (0, 0), maximized: false, full_screen: false }
    }
}

/// `restoreGeometry`: the normal geometry on the saved screen (else the one
/// it lies on, else the primary one), shrunk and moved so it fits the screen
/// with its title bar (Qt's `checkRestoredGeometry`). `screens` are the
/// connected screens' areas in logical pixels, the primary one first.
pub fn restore(g: &QtGeometry, screens: &[Rect]) -> Restore {
    let frame_offset = g.frame_offset();
    let mut normal = g.normal;
    if normal.w < 100 || normal.h < 100 {
        normal = Rect::new(normal.x, normal.y, DEFAULT_SIZE.0, DEFAULT_SIZE.1);
    }
    let (cx, cy) = (normal.x + normal.w / 2, normal.y + normal.h / 2);
    let screen = screens
        .iter()
        .find(|s| s.contains_point(cx, cy))
        .or_else(|| usize::try_from(g.screen).ok().and_then(|i| screens.get(i)))
        .or_else(|| screens.first());
    if let Some(s) = screen {
        normal = check_restored(s, normal, frame_offset.1);
    }
    Restore {
        size: (normal.w, normal.h),
        content_pos: Some((normal.x, normal.y)),
        frame_offset,
        maximized: g.maximized,
        full_screen: g.full_screen,
    }
}

/// Qt's `checkRestoredGeometry`: an exact fit would be full screen, so 2 px
/// smaller; then the edges that are off the screen are moved back onto it.
fn check_restored(avail: &Rect, mut r: Rect, frame_height: i32) -> Rect {
    if avail.h <= r.h + frame_height {
        r.h = avail.h - 2 - frame_height;
    }
    if avail.w <= r.w {
        r.w = avail.w - 2;
    }
    let with_frame = Rect::new(r.x, r.y - frame_height, r.w, r.h + frame_height);
    if avail.contains(&with_frame) {
        return r;
    }
    if with_frame.y <= avail.y {
        r.y = avail.y + 1 + frame_height;
    } else if with_frame.bottom() >= avail.bottom() {
        r.y = avail.bottom() - 1 - r.h + 1;
    }
    if with_frame.x <= avail.x {
        r.x = avail.x + 1;
    } else if with_frame.right() >= avail.right() {
        r.x = avail.right() - 1 - r.w + 1;
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    // Written by PySide6 6.x (offscreen, an 800×800 screen): a 1000×700
    // window at (120, 80), then maximized, then full screen.
    const NORMAL: &str = "AdnQywADAAAAAAB4AAAAUAAABGMAAAMPAAAAegAAAFIAAARhAAADDQAAAAAAAAAAAyAAAAB6AAAAUgAABGEAAAMN";
    const MAXIMIZED: &str = "AdnQywADAAAAAAAAAAAAAAAAAx8AAAMfAAAAegAAAFIAAARhAAADDQAAAAACAAAAAyAAAAACAAAAAgAAAx0AAAMd";
    const FULL: &str = "AdnQywADAAAAAAAAAAAAAAAAAx8AAAMfAAAAegAAAFIAAARhAAADDQAAAAAABAAAAyAAAAAAAAAAAAAAAx8AAAMf";

    #[test]
    fn reads_and_writes_qt_blobs() {
        let g = QtGeometry::decode(NORMAL).unwrap();
        assert_eq!(g.frame, Rect::new(120, 80, 1004, 704));
        assert_eq!(g.normal, Rect::new(122, 82, 1000, 700));
        assert_eq!(g.geometry, g.normal);
        assert_eq!((g.screen, g.maximized, g.full_screen, g.screen_width), (0, false, false, 800));
        assert_eq!(g.encode(), NORMAL, "byte for byte what Qt writes");

        let m = QtGeometry::decode(MAXIMIZED).unwrap();
        assert!(m.maximized && !m.full_screen);
        assert_eq!(m.normal, Rect::new(122, 82, 1000, 700));
        assert_eq!(m.encode(), MAXIMIZED);
        let f = QtGeometry::decode(FULL).unwrap();
        assert!(f.full_screen && !f.maximized);
        assert_eq!(f.encode(), FULL);

        assert_eq!(QtGeometry::decode("not base64!"), None);
        assert_eq!(QtGeometry::decode("AAAA"), None);
    }

    #[test]
    fn restores_on_screen() {
        let screens = [Rect::new(0, 0, 1728, 1117), Rect::new(1728, 0, 2560, 1440)];
        let g = QtGeometry::decode(NORMAL).unwrap();
        let r = restore(&g, &screens);
        assert_eq!(r.size, (1000, 700));
        assert_eq!(r.content_pos, Some((122, 82)));
        assert_eq!(r.outer_pos(), Some((120, 80)));
        assert!(!r.maximized);
        assert!(restore(&QtGeometry::decode(MAXIMIZED).unwrap(), &screens).maximized);

        // Saved on a second screen that is gone: moved back onto the first.
        let gone = QtGeometry { normal: Rect::new(3000, 200, 1000, 700), screen: 1, ..g };
        let r = restore(&gone, &screens[..1]);
        assert_eq!(r.content_pos, Some((1728 - 1 - 1000, 200)));
        // Bigger than the screen: shrunk to fit with the title bar.
        let big = QtGeometry { normal: Rect::new(0, 0, 3000, 2000), ..g };
        let r = restore(&big, &screens[..1]);
        assert_eq!(r.size, (1726, 1117 - 2 - 2));
        assert_eq!(r.content_pos, Some((1, 1 + 2)));
        // Still on the second screen: left alone.
        let second = QtGeometry { normal: Rect::new(2000, 300, 1000, 700), ..g };
        assert_eq!(restore(&second, &screens).content_pos, Some((2000, 300)));
    }
}
