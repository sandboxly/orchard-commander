//! Mouse handling for the terminal view, without the toolkit: clicks, drags
//! and the wheel go to programs that asked for the mouse (vim, htop, tmux,
//! mc); otherwise they select text and scroll the history.
//!
//! The view converts its events into [`PointerEvent`]s (pixels relative to
//! the terminal's top left, modifiers, a millisecond timestamp), calls the
//! matching method with the session's locked [`Screen`], and carries out the
//! returned [`MouseAction`] after releasing the lock.

use crate::keys::{self, Modifiers};
use crate::mouse::{Button, MouseReport, encode_mouse_into};
use crate::screen::{Screen, SelectionKind};

/// Wheel angle units per step: trackpads send many small deltas.
const WHEEL_STEP: i32 = 40;
/// How far (Manhattan, in pixels) a click may land from the previous one and
/// still count towards a double/triple click.
const CLICK_SLOP: f64 = 6.0;

/// A pointer event in the terminal's pixel coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PointerEvent {
    pub x: f64,
    pub y: f64,
    pub mods: Modifiers,
    /// Milliseconds on any monotonic clock (for double clicks).
    pub time_ms: u64,
}

impl PointerEvent {
    pub fn at(x: f64, y: f64) -> Self {
        Self {
            x,
            y,
            ..Self::default()
        }
    }
}

/// What the view should do after a mouse event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MouseAction {
    None,
    /// Write these bytes to the session ([`crate::Session::write_bytes`]).
    Send(Vec<u8>),
    /// The selection or scroll position changed.
    Redraw,
    /// Middle click: paste the clipboard (or primary selection).
    Paste,
}

/// The view's mouse state between events.
#[derive(Clone, Debug)]
pub struct MouseInput {
    /// Cell size in pixels (keep in sync with the font).
    pub cell_width: f64,
    pub cell_height: f64,
    /// The platform's double-click interval.
    pub double_click_ms: u64,
    /// The button reported as held to the program.
    pressed: Option<Button>,
    last_cell: Option<(usize, usize)>,
    selecting: bool,
    clicks: u32,
    last_click: Option<(u64, f64, f64)>,
    wheel_rest: i32,
}

impl MouseInput {
    pub fn new(cell_width: f64, cell_height: f64) -> Self {
        Self {
            cell_width: cell_width.max(1.0),
            cell_height: cell_height.max(1.0),
            double_click_ms: 400,
            pressed: None,
            last_cell: None,
            selecting: false,
            clicks: 0,
            last_click: None,
            wheel_rest: 0,
        }
    }

    /// Whether events go to the program. Shift keeps the mouse for
    /// selecting, as in xterm.
    pub fn reporting(screen: &Screen, mods: Modifiers) -> bool {
        !mods.shift && screen.mouse_tracking().is_some()
    }

    /// Whether a drag-selection is in progress.
    pub fn selecting(&self) -> bool {
        self.selecting
    }

    /// (column, line) on the screen, 0-based and clamped.
    fn screen_cell(&self, screen: &Screen, e: &PointerEvent) -> (usize, usize) {
        let column = ((e.x / self.cell_width).max(0.0) as usize).min(screen.columns() - 1);
        let line = ((e.y / self.cell_height).max(0.0) as usize).min(screen.lines() - 1);
        (column, line)
    }

    /// (line, column boundary) for selections: the boundary nearest the
    /// left of the pointer, up to `columns`.
    fn selection_cell(&self, screen: &Screen, e: &PointerEvent) -> (usize, usize) {
        let line = (e.y / self.cell_height).max(0.0) as usize;
        let column = ((e.x / self.cell_width).max(0.0) as usize).min(screen.columns());
        (line, column)
    }

    fn report(
        &self,
        screen: &Screen,
        e: &PointerEvent,
        button: Button,
        press: bool,
        motion: bool,
        out: &mut Vec<u8>,
    ) {
        let (column, line) = self.screen_cell(screen, e);
        let mods = Modifiers {
            shift: false,
            ..e.mods
        };
        let report = MouseReport {
            button,
            column,
            line,
            press,
            motion,
            mods,
        };
        encode_mouse_into(screen.mouse_modes(), report, out);
    }

    fn send(out: Vec<u8>) -> MouseAction {
        if out.is_empty() {
            MouseAction::None
        } else {
            MouseAction::Send(out)
        }
    }

    /// 1, 2, 3… for single, double, triple clicks on (about) the same spot.
    fn click_count(&mut self, e: &PointerEvent) -> u32 {
        let near = self.last_click.is_some_and(|(t, x, y)| {
            (e.x - x).abs() + (e.y - y).abs() < CLICK_SLOP
                && e.time_ms.saturating_sub(t) <= self.double_click_ms
        });
        self.clicks = if near { self.clicks + 1 } else { 1 };
        self.last_click = Some((e.time_ms, e.x, e.y));
        self.clicks
    }

    /// A button went down. Call it for every press, including the toolkit's
    /// double-click events: clicks are counted here.
    pub fn press(&mut self, screen: &mut Screen, button: Button, e: PointerEvent) -> MouseAction {
        let is_button = matches!(button, Button::Left | Button::Middle | Button::Right);
        if is_button && Self::reporting(screen, e.mods) {
            self.pressed = Some(button);
            self.last_cell = Some(self.screen_cell(screen, &e));
            let mut out = Vec::new();
            self.report(screen, &e, button, true, false, &mut out);
            return Self::send(out);
        }
        match button {
            Button::Left => {
                let (line, column) = self.selection_cell(screen, &e);
                match self.click_count(&e) {
                    2 => screen.select(SelectionKind::Word, line, column),
                    1 => {
                        screen.select(SelectionKind::Simple, line, column);
                        self.selecting = true;
                    }
                    _ => screen.select(SelectionKind::Line, line, column), // the whole line
                }
                MouseAction::Redraw
            }
            Button::Middle => MouseAction::Paste,
            _ => MouseAction::None,
        }
    }

    /// The pointer moved (with or without a button held).
    pub fn motion(&mut self, screen: &mut Screen, e: PointerEvent) -> MouseAction {
        if Self::reporting(screen, e.mods) && !self.selecting {
            let cell = self.screen_cell(screen, &e);
            if self.last_cell == Some(cell) {
                return MouseAction::None; // one report per cell, not per pixel
            }
            self.last_cell = Some(cell);
            let button = self.pressed.unwrap_or(Button::None);
            let mut out = Vec::new();
            self.report(screen, &e, button, true, true, &mut out);
            return Self::send(out);
        }
        if self.selecting {
            let (line, column) = self.selection_cell(screen, &e);
            screen.extend_selection(line, column);
            return MouseAction::Redraw;
        }
        MouseAction::None
    }

    /// A button went up.
    pub fn release(&mut self, screen: &mut Screen, button: Button, e: PointerEvent) -> MouseAction {
        if self.pressed == Some(button) {
            self.pressed = None;
            let mut out = Vec::new();
            self.report(screen, &e, button, false, false, &mut out);
            return Self::send(out);
        }
        let was_selecting = std::mem::replace(&mut self.selecting, false);
        if was_selecting && !screen.has_selection() {
            screen.clear_selection(); // a click without a drag selects nothing
            return MouseAction::Redraw;
        }
        MouseAction::None
    }

    /// Wheel / trackpad scrolling; `dx`, `dy` in angle units (120 per notch,
    /// positive = up/left). Reported to programs that take the mouse; arrow
    /// keys for full-screen programs that don't; otherwise the history.
    pub fn wheel(&mut self, screen: &mut Screen, dx: i32, dy: i32, e: PointerEvent) -> MouseAction {
        let vertical = dy.unsigned_abs() >= dx.unsigned_abs();
        self.wheel_rest = self
            .wheel_rest
            .saturating_add(if vertical { dy } else { dx });
        let steps = self.wheel_rest / WHEEL_STEP;
        if steps == 0 {
            return MouseAction::None;
        }
        self.wheel_rest -= steps * WHEEL_STEP;
        if Self::reporting(screen, e.mods) {
            let button = match (vertical, steps > 0) {
                (true, true) => Button::WheelUp,
                (true, false) => Button::WheelDown,
                (false, true) => Button::WheelLeft,
                (false, false) => Button::WheelRight,
            };
            let mut out = Vec::new();
            for _ in 0..steps.unsigned_abs() {
                self.report(screen, &e, button, true, false, &mut out);
            }
            return Self::send(out);
        }
        if !vertical {
            return MouseAction::None;
        }
        if screen.alternate() {
            // full-screen programs without mouse support: scroll like the arrow keys
            let arrow = keys::arrow(steps > 0, screen.app_cursor());
            return MouseAction::Send(arrow.repeat(steps.unsigned_abs() as usize));
        }
        screen.scroll(steps);
        MouseAction::Redraw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CW: f64 = 8.0;
    const CH: f64 = 16.0;

    /// The middle of a cell.
    fn cell(column: usize, line: usize) -> PointerEvent {
        PointerEvent::at((column as f64 + 0.5) * CW, (line as f64 + 0.5) * CH)
    }

    fn setup(output: &str) -> (Screen, MouseInput) {
        let mut screen = Screen::new(40, 6);
        screen.feed(output.as_bytes());
        (screen, MouseInput::new(CW, CH))
    }

    #[test]
    fn clicks_drags_and_wheel_go_to_the_program() {
        let (mut s, mut m) = setup("\x1b[?1002h\x1b[?1006h"); // what vim/htop send to ask for the mouse
        let mut sent = Vec::new();
        for action in [
            m.press(&mut s, Button::Left, cell(2, 1)),
            m.motion(&mut s, cell(5, 1)),
            m.motion(
                &mut s,
                PointerEvent {
                    x: cell(5, 1).x + 1.0,
                    ..cell(5, 1)
                },
            ), // same cell: nothing
            m.release(&mut s, Button::Left, cell(5, 1)),
        ] {
            if let MouseAction::Send(bytes) = action {
                sent.push(bytes);
            }
        }
        assert_eq!(
            sent,
            [
                b"\x1b[<0;3;2M".to_vec(),
                b"\x1b[<32;6;2M".to_vec(),
                b"\x1b[<0;6;2m".to_vec()
            ]
        );
        assert!(!s.has_selection()); // the program got it: no local selection
        assert_eq!(
            m.wheel(&mut s, 0, -120, cell(1, 1)),
            MouseAction::Send(b"\x1b[<65;2;2M".repeat(3))
        );
        // trackpad: small deltas add up
        assert_eq!(m.wheel(&mut s, 0, 20, cell(1, 1)), MouseAction::None);
        assert_eq!(
            m.wheel(&mut s, 0, 20, cell(1, 1)),
            MouseAction::Send(b"\x1b[<64;2;2M".to_vec())
        );
        assert_eq!(
            m.wheel(&mut s, -50, 10, cell(1, 1)),
            MouseAction::Send(b"\x1b[<67;2;2M".to_vec())
        );
    }

    #[test]
    fn motion_without_a_button_only_in_all_motion_mode() {
        let (mut s, mut m) = setup("\x1b[?1002h\x1b[?1006h");
        assert_eq!(m.motion(&mut s, cell(3, 3)), MouseAction::None);
        s.feed(b"\x1b[?1003h");
        assert_eq!(
            m.motion(&mut s, cell(4, 3)),
            MouseAction::Send(b"\x1b[<35;5;4M".to_vec())
        );
        let ctrl = PointerEvent {
            mods: Modifiers::CTRL,
            ..cell(0, 0)
        };
        assert_eq!(
            m.press(&mut s, Button::Right, ctrl),
            MouseAction::Send(b"\x1b[<18;1;1M".to_vec())
        );
    }

    #[test]
    fn shift_selects_even_when_the_program_has_the_mouse() {
        let (mut s, mut m) = setup("hello world\r\n\x1b[?1000h\x1b[?1006h");
        let shift = |e: PointerEvent| PointerEvent {
            mods: Modifiers::SHIFT,
            ..e
        };
        assert_eq!(
            m.press(&mut s, Button::Left, shift(cell(0, 0))),
            MouseAction::Redraw
        );
        assert!(m.selecting());
        assert_eq!(
            m.release(&mut s, Button::Left, shift(cell(0, 0))),
            MouseAction::Redraw
        );
        assert!(!s.has_selection());
        // a shift-drag selects
        m.press(
            &mut s,
            Button::Left,
            PointerEvent {
                time_ms: 10_000,
                ..shift(cell(0, 0))
            },
        );
        assert_eq!(
            m.motion(&mut s, shift(PointerEvent::at(5.0 * CW, 0.5 * CH))),
            MouseAction::Redraw
        );
        assert_eq!(
            m.release(&mut s, Button::Left, shift(cell(5, 0))),
            MouseAction::None
        );
        assert_eq!(s.selected_text(), "hello");
    }

    #[test]
    fn double_and_triple_click_select() {
        let (mut s, mut m) = setup("\x1b[2J\x1b[Hls ~/src/project.txt done\r\n");
        let at = |t: u64, c: usize| PointerEvent {
            time_ms: t,
            ..cell(c, 0)
        };
        m.press(&mut s, Button::Left, at(0, 8));
        m.release(&mut s, Button::Left, at(10, 8));
        assert_eq!(
            m.press(&mut s, Button::Left, at(100, 8)),
            MouseAction::Redraw
        );
        assert_eq!(s.selected_text(), "~/src/project.txt"); // a path counts as one word
        m.release(&mut s, Button::Left, at(110, 8));
        m.press(&mut s, Button::Left, at(200, 8)); // third click
        assert_eq!(s.selected_text().trim(), "ls ~/src/project.txt done");
        // too slow: a fresh single click
        m.release(&mut s, Button::Left, at(210, 8));
        m.press(&mut s, Button::Left, at(5000, 8));
        m.release(&mut s, Button::Left, at(5010, 8));
        assert!(!s.has_selection());
        // too far apart: not a double click
        m.press(&mut s, Button::Left, at(6000, 1));
        m.release(&mut s, Button::Left, at(6010, 1));
        m.press(&mut s, Button::Left, at(6100, 8));
        assert!(!s.has_selection());
    }

    #[test]
    fn middle_click_pastes_and_wheel_scrolls_history() {
        let (mut s, mut m) = setup("");
        assert_eq!(
            m.press(&mut s, Button::Middle, cell(0, 0)),
            MouseAction::Paste
        );
        for i in 0..20 {
            s.feed(format!("l{i}\r\n").as_bytes());
        }
        assert_eq!(m.wheel(&mut s, 0, 120, cell(0, 0)), MouseAction::Redraw);
        assert_eq!(s.display_offset(), 3);
        assert_eq!(m.wheel(&mut s, 120, 0, cell(0, 0)), MouseAction::None); // sideways: nothing to scroll
        m.wheel(&mut s, 0, -1200, cell(0, 0));
        assert_eq!(s.display_offset(), 0);
    }

    #[test]
    fn wheel_on_the_alternate_screen_sends_arrows() {
        let (mut s, mut m) = setup("\x1b[?1049h");
        assert_eq!(
            m.wheel(&mut s, 0, -80, cell(0, 0)),
            MouseAction::Send(b"\x1b[B\x1b[B".to_vec())
        );
        s.feed(b"\x1b[?1h");
        assert_eq!(
            m.wheel(&mut s, 0, 40, cell(0, 0)),
            MouseAction::Send(b"\x1bOA".to_vec())
        );
    }

    #[test]
    fn coordinates_are_clamped() {
        let (mut s, mut m) = setup("\x1b[?1000h\x1b[?1006h");
        let far = PointerEvent::at(10_000.0, 10_000.0);
        assert_eq!(
            m.press(&mut s, Button::Left, far),
            MouseAction::Send(b"\x1b[<0;40;6M".to_vec())
        );
        let neg = PointerEvent::at(-5.0, -5.0);
        assert_eq!(
            m.release(&mut s, Button::Left, neg),
            MouseAction::Send(b"\x1b[<0;1;1m".to_vec())
        );
    }
}
