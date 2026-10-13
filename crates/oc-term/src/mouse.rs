//! xterm mouse reporting: which events a program asked for and how they are
//! encoded on the wire.
//!
//! alacritty_terminal tracks only some of these modes (no X10 `9`, no urxvt
//! `1015`) and treats them as mutually exclusive, so the session tracks them
//! itself (see [`crate::scan`]) with the Python version's set semantics: the
//! most detailed tracking mode that is on wins, and so does the most capable
//! encoding.

use crate::keys::Modifiers;

/// The mouse-related private modes (`CSI ? n h`) currently set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MouseModes(u8);

impl MouseModes {
    pub const NONE: Self = Self(0);
    /// `9`: presses only, no modifiers.
    pub const X10: Self = Self(1);
    /// `1000`: presses and releases.
    pub const CLICKS: Self = Self(1 << 1);
    /// `1002`: plus motion while a button is held.
    pub const DRAGS: Self = Self(1 << 2);
    /// `1003`: plus all motion.
    pub const MOTION: Self = Self(1 << 3);
    /// `1005`: coordinates as UTF-8 characters.
    pub const UTF8: Self = Self(1 << 4);
    /// `1006`: `CSI < b ; x ; y M/m` — any size, releases keep the button.
    pub const SGR: Self = Self(1 << 5);
    /// `1015`: `CSI b ; x ; y M` in decimal.
    pub const URXVT: Self = Self(1 << 6);

    /// The flag for an xterm private mode number, if it is a mouse mode.
    pub fn from_private_mode(mode: u16) -> Option<Self> {
        Some(match mode {
            9 => Self::X10,
            1000 => Self::CLICKS,
            1002 => Self::DRAGS,
            1003 => Self::MOTION,
            1005 => Self::UTF8,
            1006 => Self::SGR,
            1015 => Self::URXVT,
            _ => return None,
        })
    }

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0 && other.0 != 0
    }

    pub fn set(&mut self, flag: Self, on: bool) {
        if on {
            self.0 |= flag.0;
        } else {
            self.0 &= !flag.0;
        }
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// What the program wants reported, if anything.
    pub fn tracking(self) -> Option<MouseTracking> {
        [
            (Self::MOTION, MouseTracking::Motion),
            (Self::DRAGS, MouseTracking::Drags),
            (Self::CLICKS, MouseTracking::Clicks),
            (Self::X10, MouseTracking::X10),
        ]
        .into_iter()
        .find_map(|(flag, tracking)| self.contains(flag).then_some(tracking))
    }
}

impl std::ops::BitOr for MouseModes {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// Which mouse events a program asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseTracking {
    X10,
    Clicks,
    Drags,
    Motion,
}

/// A button (or wheel direction) as xterm numbers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Button {
    Left,
    Middle,
    Right,
    /// Motion without a button held.
    None,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}

impl Button {
    /// xterm's button number.
    pub fn code(self) -> u32 {
        match self {
            Button::Left => 0,
            Button::Middle => 1,
            Button::Right => 2,
            Button::None => 3,
            Button::WheelUp => 64,
            Button::WheelDown => 65,
            Button::WheelLeft => 66,
            Button::WheelRight => 67,
        }
    }

    pub fn is_wheel(self) -> bool {
        self.code() >= 64
    }
}

/// One mouse event to report, at a 0-based screen cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseReport {
    pub button: Button,
    pub column: usize,
    pub line: usize,
    /// `false` for a release.
    pub press: bool,
    pub motion: bool,
    pub mods: Modifiers,
}

impl MouseReport {
    /// A button press (or wheel step) without modifiers.
    pub fn press(button: Button, column: usize, line: usize) -> Self {
        Self {
            button,
            column,
            line,
            press: true,
            motion: false,
            mods: Modifiers::NONE,
        }
    }

    pub fn release(self) -> Self {
        Self {
            press: false,
            ..self
        }
    }

    pub fn motion(self) -> Self {
        Self {
            motion: true,
            ..self
        }
    }

    pub fn with_mods(self, mods: Modifiers) -> Self {
        Self { mods, ..self }
    }
}

/// The bytes reporting `event`, or `None` if it isn't to be sent (nobody
/// asked, the mode doesn't report it, or the encoding can't express it).
pub fn encode_mouse(modes: MouseModes, event: MouseReport) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    encode_mouse_into(modes, event, &mut out).then_some(out)
}

/// [`encode_mouse`] appending to `out`; returns whether anything was written.
/// Handy for wheel bursts, which report several steps at once.
pub fn encode_mouse_into(modes: MouseModes, event: MouseReport, out: &mut Vec<u8>) -> bool {
    let Some(tracking) = modes.tracking() else {
        return false;
    };
    let button = event.button;
    let mut code = if tracking == MouseTracking::X10 {
        // presses only, no modifiers
        if !event.press || event.motion {
            return false;
        }
        button.code()
    } else {
        if event.motion
            && (tracking == MouseTracking::Clicks
                || (tracking == MouseTracking::Drags && button == Button::None))
        {
            return false;
        }
        if button.is_wheel() && !event.press {
            return false; // wheels have no release
        }
        let m = event.mods;
        button.code()
            + if event.motion { 32 } else { 0 }
            + if m.shift { 4 } else { 0 }
            + if m.alt { 8 } else { 0 }
            + if m.ctrl { 16 } else { 0 }
    };
    let (x, y) = (event.column + 1, event.line + 1);
    use std::io::Write as _;
    if modes.contains(MouseModes::SGR) {
        // modern: any size, releases keep the button
        let end = if event.press { 'M' } else { 'm' };
        let _ = write!(out, "\x1b[<{code};{x};{y}{end}");
        return true;
    }
    if !event.press {
        code = (code & !3) | 3; // legacy: a release doesn't say which button
    }
    if modes.contains(MouseModes::URXVT) {
        let _ = write!(out, "\x1b[{};{x};{y}M", code + 32);
        return true;
    }
    if modes.contains(MouseModes::UTF8) {
        out.extend_from_slice(b"\x1b[M");
        for v in [code as usize, x, y] {
            // Like Python's chr(): values past char range are unreachable in practice.
            let c = char::from_u32((v + 32) as u32).unwrap_or(char::REPLACEMENT_CHARACTER);
            let mut buf = [0; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
        return true;
    }
    if x + 32 > 255 || y + 32 > 255 {
        return false; // the classic encoding can't express it
    }
    out.extend_from_slice(&[
        0x1b,
        b'[',
        b'M',
        (code + 32) as u8,
        (x + 32) as u8,
        (y + 32) as u8,
    ]);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const SGR_DRAGS: MouseModes = MouseModes(MouseModes::DRAGS.0 | MouseModes::SGR.0);

    fn enc(modes: MouseModes, e: MouseReport) -> Option<Vec<u8>> {
        encode_mouse(modes, e)
    }

    #[test]
    fn mouse_encoding_matches_python() {
        use Button::*;
        let p = MouseReport::press;
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        };
        let cases: Vec<(MouseModes, MouseReport, Option<&[u8]>)> = vec![
            (SGR_DRAGS, p(Left, 4, 2), Some(b"\x1b[<0;5;3M")),
            (SGR_DRAGS, p(Left, 4, 2).release(), Some(b"\x1b[<0;5;3m")),
            (
                SGR_DRAGS,
                p(Right, 0, 0).with_mods(ctrl),
                Some(b"\x1b[<18;1;1M"),
            ),
            (SGR_DRAGS, p(Left, 7, 1).motion(), Some(b"\x1b[<32;8;2M")),
            (SGR_DRAGS, p(None, 7, 1).motion(), Option::None), // drags only
            (
                MouseModes::MOTION | MouseModes::SGR,
                p(None, 7, 1).motion(),
                Some(b"\x1b[<35;8;2M"),
            ),
            (SGR_DRAGS, p(WheelDown, 1, 1), Some(b"\x1b[<65;2;2M")),
            (SGR_DRAGS, p(WheelUp, 1, 1).release(), Option::None),
            (SGR_DRAGS, p(Left, 400, 90), Some(b"\x1b[<0;401;91M")), // any size
            (MouseModes::CLICKS, p(Left, 4, 2), Some(b"\x1b[M %#")), // legacy
            (
                MouseModes::CLICKS,
                p(Middle, 4, 2).release(),
                Some(b"\x1b[M#%#"),
            ),
            (MouseModes::CLICKS, p(Left, 300, 2), Option::None), // too wide for it
            (
                MouseModes::CLICKS | MouseModes::URXVT,
                p(Left, 300, 2),
                Some(b"\x1b[32;301;3M"),
            ),
            (MouseModes::CLICKS, p(Left, 4, 2).motion(), Option::None),
            (MouseModes::X10, p(Left, 4, 2).release(), Option::None),
            (MouseModes::NONE, p(Left, 4, 2), Option::None), // nobody asked
        ];
        for (modes, event, expected) in cases {
            assert_eq!(
                enc(modes, event).as_deref(),
                expected,
                "{modes:?} {event:?}"
            );
        }
    }

    #[test]
    fn x10_ignores_modifiers_and_utf8_encodes_large_coordinates() {
        let alt = Modifiers {
            alt: true,
            ..Modifiers::NONE
        };
        let e = MouseReport::press(Button::Right, 0, 0).with_mods(alt);
        assert_eq!(enc(MouseModes::X10, e).unwrap(), b"\x1b[M\"!!");
        let e = MouseReport::press(Button::Left, 299, 0);
        let mut expected = b"\x1b[M !".to_vec();
        expected.splice(4..4, "\u{14c}".bytes()); // 300 + 32
        assert_eq!(
            enc(MouseModes::CLICKS | MouseModes::UTF8, e).unwrap(),
            expected
        );
    }

    #[test]
    fn tracking_prefers_the_most_detailed_mode() {
        assert_eq!(
            (MouseModes::CLICKS | MouseModes::MOTION).tracking(),
            Some(MouseTracking::Motion)
        );
        assert_eq!(
            (MouseModes::X10 | MouseModes::DRAGS).tracking(),
            Some(MouseTracking::Drags)
        );
        assert_eq!(MouseModes::SGR.tracking(), None);
        let mut m = MouseModes::CLICKS | MouseModes::SGR;
        m.set(MouseModes::CLICKS, false);
        assert_eq!(m, MouseModes::SGR);
        assert_eq!(MouseModes::from_private_mode(1015), Some(MouseModes::URXVT));
        assert_eq!(MouseModes::from_private_mode(1049), None);
    }

    #[test]
    fn wheel_into_appends_steps() {
        let mut out = Vec::new();
        for _ in 0..3 {
            assert!(encode_mouse_into(
                SGR_DRAGS,
                MouseReport::press(Button::WheelDown, 1, 1),
                &mut out
            ));
        }
        assert_eq!(out, b"\x1b[<65;2;2M".repeat(3));
    }
}
