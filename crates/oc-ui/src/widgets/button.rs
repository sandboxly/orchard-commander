//! Push buttons with hotkey letters, laid out in a centered row.

use oc_grid::{Flags, Frame, Style};

use super::{DrawCtx, fill, parse_hotkey, put_hotkey_label, width};
use crate::geom::Rect;
use crate::input::{Key, KeyEvent};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Button {
    /// The label without the `&` marker.
    pub label: String,
    /// Index (in chars) and lower-case letter of the hotkey.
    pub hotkey: Option<(usize, char)>,
    pub enabled: bool,
}

impl Button {
    /// From a label like "&Overwrite" (`&` marks the hotkey).
    pub fn new(label: &str) -> Self {
        let (label, hotkey) = parse_hotkey(label);
        Button { label, hotkey, enabled: true }
    }

    fn cells(&self) -> u16 {
        (width(&self.label) + 4).max(8)
    }
}

/// What a key or click did to a [`ButtonRow`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowOutcome {
    Ignored,
    /// Focus moved.
    Consumed,
    /// Button `i` was pressed.
    Activated(usize),
}

/// A row of buttons, one of which has the focus (Enter presses it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ButtonRow {
    pub buttons: Vec<Button>,
    pub focus: usize,
}

impl ButtonRow {
    pub fn new(labels: &[&str], focus: usize) -> Self {
        ButtonRow { buttons: labels.iter().map(|l| Button::new(l)).collect(), focus }
    }

    /// Total width in cells, gaps included.
    pub fn cells(&self) -> u16 {
        let n = self.buttons.len() as u16;
        self.buttons.iter().map(Button::cells).sum::<u16>() + 2 * n.saturating_sub(1)
    }

    /// Each button's cells, centered in `row`.
    pub fn rects(&self, row: Rect) -> Vec<Rect> {
        let mut x = row.x + row.w.saturating_sub(self.cells()) / 2;
        self.buttons
            .iter()
            .map(|b| {
                let r = Rect::new(x, row.y, b.cells(), 1);
                x += b.cells() + 2;
                r
            })
            .collect()
    }

    pub fn hit(&self, row: Rect, col: u16, line: u16) -> Option<usize> {
        self.rects(row).iter().position(|r| r.contains(col, line)).filter(|&i| self.buttons[i].enabled)
    }

    /// `focused`: the row has the dialog's focus (shows the focused button).
    pub fn draw(&self, f: &mut Frame, row: Rect, ctx: &DrawCtx, focused: bool) {
        let c = &ctx.theme.colors;
        for (i, (b, r)) in self.buttons.iter().zip(self.rects(row)).enumerate() {
            let on = focused && i == self.focus;
            let style = if !b.enabled {
                Style::new(c.muted, c.border)
            } else if on {
                Style::new(c.surface, c.border_active).with(Flags::BOLD)
            } else {
                Style::new(c.foreground, c.border)
            };
            fill(f, r, style.bg);
            let x = r.x + (r.w - width(&b.label)) / 2;
            let hot = if on { c.surface } else { c.accent };
            put_hotkey_label(f, x, r.y, &b.label, b.hotkey, style, hot, r.w);
        }
    }

    /// Arrow keys / Tab move the focus; Enter or Space press the focused
    /// button; a hotkey letter presses its button (`plain_hotkeys`: without
    /// Alt — when no text field takes the letters).
    pub fn handle_key(&mut self, ev: &KeyEvent, plain_hotkeys: bool) -> RowOutcome {
        let n = self.buttons.len();
        if n == 0 {
            return RowOutcome::Ignored;
        }
        let m = ev.mods;
        match ev.key {
            Key::Left if m.is_empty() => self.move_focus(-1),
            Key::Right if m.is_empty() => self.move_focus(1),
            Key::Enter | Key::Space if m.is_empty() => {
                if self.buttons[self.focus].enabled {
                    RowOutcome::Activated(self.focus)
                } else {
                    RowOutcome::Consumed
                }
            }
            Key::Char(ch) if (m.is_empty() && plain_hotkeys) || (m.alt && !m.ctrl && !m.logo) => {
                let hit = self.buttons.iter().position(|b| b.enabled && b.hotkey.is_some_and(|(_, k)| k == ch));
                match hit {
                    Some(i) => {
                        self.focus = i;
                        RowOutcome::Activated(i)
                    }
                    None => RowOutcome::Ignored,
                }
            }
            _ => RowOutcome::Ignored,
        }
    }

    fn move_focus(&mut self, delta: isize) -> RowOutcome {
        let n = self.buttons.len() as isize;
        let mut i = self.focus as isize;
        for _ in 0..n {
            i = (i + delta).rem_euclid(n);
            if self.buttons[i as usize].enabled {
                self.focus = i as usize;
                break;
            }
        }
        RowOutcome::Consumed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Mods;

    #[test]
    fn keys_move_and_press() {
        let mut row = ButtonRow::new(&["&Overwrite", "&All", "&Skip", "S&kip all", "Cancel"], 2);
        assert_eq!(row.handle_key(&KeyEvent::plain(Key::Enter), true), RowOutcome::Activated(2));
        assert_eq!(row.handle_key(&KeyEvent::plain(Key::Right), true), RowOutcome::Consumed);
        assert_eq!(row.focus, 3);
        assert_eq!(row.handle_key(&KeyEvent::plain(Key::Char('k')), true), RowOutcome::Activated(3));
        assert_eq!(row.handle_key(&KeyEvent::plain(Key::Char('o')), false), RowOutcome::Ignored);
        assert_eq!(row.handle_key(&KeyEvent::new(Key::Char('o'), Mods::ALT), false), RowOutcome::Activated(0));
        assert_eq!(row.handle_key(&KeyEvent::plain(Key::Left), true), RowOutcome::Consumed);
        assert_eq!(row.focus, 4, "wraps around");
    }

    #[test]
    fn centered_hit_test() {
        let row = ButtonRow::new(&["OK", "Cancel"], 0);
        let r = Rect::new(0, 5, 40, 1);
        let rects = row.rects(r);
        assert_eq!(rects[0], Rect::new(10, 5, 8, 1));
        assert_eq!(rects[1], Rect::new(20, 5, 10, 1));
        assert_eq!(row.hit(r, 12, 5), Some(0));
        assert_eq!(row.hit(r, 22, 5), Some(1));
        assert_eq!(row.hit(r, 19, 5), None);
    }
}
