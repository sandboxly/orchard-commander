//! A checkbox: "[x] Label".

use orchard_grid::{Frame, Style};

use super::{DrawCtx, parse_hotkey, put, put_hotkey_label};
use crate::geom::Rect;
use crate::input::{Key, KeyEvent};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkbox {
    pub label: String,
    pub hotkey: Option<(usize, char)>,
    pub checked: bool,
}

impl Checkbox {
    pub fn new(label: &str, checked: bool) -> Self {
        let (label, hotkey) = parse_hotkey(label);
        Checkbox { label, hotkey, checked }
    }

    /// Space toggles (when focused); Alt+hotkey toggles always. Returns
    /// whether the key was used.
    pub fn handle_key(&mut self, ev: &KeyEvent, focused: bool) -> bool {
        let toggle = match ev.key {
            Key::Space => focused && ev.mods.is_empty(),
            Key::Char(c) => ev.mods.alt && self.hotkey.is_some_and(|(_, k)| k == c),
            _ => false,
        };
        if toggle {
            self.checked = !self.checked;
        }
        toggle
    }

    /// A click anywhere on its row toggles it.
    pub fn click(&mut self, r: Rect, col: u16, row: u16) -> bool {
        let hit = r.contains(col, row);
        if hit {
            self.checked = !self.checked;
        }
        hit
    }

    pub fn draw(&self, f: &mut Frame, r: Rect, ctx: &DrawCtx, focused: bool) {
        let c = &ctx.theme.colors;
        let fg = if focused { c.border_active } else { c.foreground };
        let style = Style::new(fg, c.surface);
        let mark = if self.checked { "[x] " } else { "[ ] " };
        let x = put(f, r.x, r.y, mark, style, r.w);
        put_hotkey_label(f, x, r.y, &self.label, self.hotkey, style, c.accent, r.right().saturating_sub(x));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Mods;

    #[test]
    fn toggles() {
        let mut cb = Checkbox::new("&Recursive", false);
        assert!(!cb.handle_key(&KeyEvent::plain(Key::Space), false));
        assert!(cb.handle_key(&KeyEvent::plain(Key::Space), true) && cb.checked);
        assert!(cb.handle_key(&KeyEvent::new(Key::Char('r'), Mods::ALT), false) && !cb.checked);
        assert!(cb.click(Rect::new(0, 0, 10, 1), 3, 0) && cb.checked);
    }
}
