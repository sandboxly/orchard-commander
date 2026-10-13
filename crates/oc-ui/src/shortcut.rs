//! Keyboard shortcuts: parsing ("Primary+Shift+."), matching key events and
//! showing them the platform's way ("⇧⌘." on macOS, "Ctrl+Shift+." elsewhere).
//!
//! `Primary` is ⌘ on macOS and Ctrl elsewhere — what Qt calls
//! `ControlModifier`, so Python's `"Ctrl+R"` is `"Primary+R"` here. `Ctrl`
//! is the Control key itself (⌃ on macOS, Qt's `MetaModifier` there).

use std::fmt;

use crate::input::{Key, KeyEvent, Mods};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Shortcut {
    pub key: Key,
    pub mods: Mods,
}

impl Shortcut {
    pub const fn new(key: Key, mods: Mods) -> Self {
        Shortcut { key, mods }
    }

    /// Parse "Primary+Shift+.", "Alt+F7", "F5", "Primary++" (the plus key).
    /// Modifier names: Primary, Ctrl/Control, Alt/Option, Shift, Cmd/Super/Logo/Meta.
    pub fn parse(spec: &str) -> Option<Shortcut> {
        let mut mods = Mods::NONE;
        let mut rest = spec;
        // A trailing "+" after a separator is the plus key itself.
        while let Some((head, tail)) = rest.split_once('+') {
            if head.is_empty() {
                break; // "+" or "++": the key is "+"
            }
            match head.to_ascii_lowercase().as_str() {
                "primary" => mods = mods.union(Mods::primary()),
                "ctrl" | "control" => mods.ctrl = true,
                "alt" | "option" | "opt" => mods.alt = true,
                "shift" => mods.shift = true,
                "cmd" | "command" | "super" | "logo" | "meta" => mods.logo = true,
                _ => return None,
            }
            rest = tail;
        }
        let key = parse_key(rest)?;
        Some(Shortcut { key, mods })
    }

    pub fn matches(&self, ev: &KeyEvent) -> bool {
        self.key == ev.key && self.mods == ev.mods
    }

    /// The key event this shortcut stands for (menus forward accelerators
    /// as key events to an open dialog).
    pub fn to_event(&self) -> KeyEvent {
        KeyEvent::new(self.key, self.mods)
    }

    /// The platform's notation: "⇧⌘." on macOS, "Ctrl+Shift+." elsewhere.
    pub fn display(&self) -> String {
        if cfg!(target_os = "macos") { self.display_mac() } else { self.display_other() }
    }

    pub fn display_mac(&self) -> String {
        let mut s = String::new();
        if self.mods.ctrl {
            s.push('⌃');
        }
        if self.mods.alt {
            s.push('⌥');
        }
        if self.mods.shift {
            s.push('⇧');
        }
        if self.mods.logo {
            s.push('⌘');
        }
        s.push_str(&key_name(self.key, true));
        s
    }

    pub fn display_other(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.mods.ctrl {
            parts.push("Ctrl".into());
        }
        if self.mods.alt {
            parts.push("Alt".into());
        }
        if self.mods.shift {
            parts.push("Shift".into());
        }
        if self.mods.logo {
            parts.push("Super".into());
        }
        parts.push(key_name(self.key, false));
        parts.join("+")
    }
}

impl fmt::Display for Shortcut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

fn parse_key(s: &str) -> Option<Key> {
    let lower = s.to_ascii_lowercase();
    Some(match lower.as_str() {
        "enter" | "return" => Key::Enter,
        "esc" | "escape" => Key::Escape,
        "tab" => Key::Tab,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "insert" | "ins" => Key::Insert,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdown" => Key::PageDown,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "space" => Key::Space,
        _ => {
            if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok())
                && (1..=24).contains(&n)
            {
                return Some(Key::F(n));
            }
            let mut chars = s.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Key::Char(c.to_lowercase().next().unwrap_or(c))
        }
    })
}

fn key_name(key: Key, mac: bool) -> String {
    let named = |m: &str, o: &str| if mac { m.to_string() } else { o.to_string() };
    match key {
        Key::Char(c) => c.to_uppercase().to_string(),
        Key::Enter => named("↩", "Enter"),
        Key::Escape => named("⎋", "Esc"),
        Key::Tab => named("⇥", "Tab"),
        Key::Backspace => named("⌫", "Backspace"),
        Key::Delete => named("⌦", "Del"),
        Key::Insert => "Ins".into(),
        Key::Home => named("↖", "Home"),
        Key::End => named("↘", "End"),
        Key::PageUp => named("⇞", "PgUp"),
        Key::PageDown => named("⇟", "PgDown"),
        Key::Up => named("↑", "Up"),
        Key::Down => named("↓", "Down"),
        Key::Left => named("←", "Left"),
        Key::Right => named("→", "Right"),
        Key::Space => named("Space", "Space"),
        Key::F(n) => format!("F{n}"),
        Key::Other => "?".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_show() {
        let s = Shortcut::parse("Primary+Shift+.").unwrap();
        assert_eq!(s.key, Key::Char('.'));
        assert!(s.mods.shift && s.mods.has_primary());
        assert_eq!(Shortcut::parse("Ctrl+Shift+.").unwrap().display_other(), "Ctrl+Shift+.");
        assert_eq!(Shortcut::parse("Cmd+Alt+A").unwrap().display_mac(), "⌥⌘A");
        assert_eq!(Shortcut::parse("Cmd+P").unwrap().display_mac(), "⌘P");
        assert_eq!(Shortcut::parse("Alt+F7").unwrap().display_other(), "Alt+F7");
        assert_eq!(Shortcut::parse("Primary++").unwrap().key, Key::Char('+'));
        assert_eq!(Shortcut::parse("F10").unwrap(), Shortcut::new(Key::F(10), Mods::NONE));
        assert_eq!(Shortcut::parse("Cmd+Backspace").unwrap().display_mac(), "⌘⌫");
        assert!(Shortcut::parse("Hyper+X").is_none());
        assert!(Shortcut::parse("Ctrl+XY").is_none());
    }

    #[test]
    fn matching() {
        let s = Shortcut::parse("Primary+R").unwrap();
        assert!(s.matches(&KeyEvent::new(Key::Char('r'), Mods::primary())));
        assert!(!s.matches(&KeyEvent::new(Key::Char('r'), Mods::primary().union(Mods::SHIFT))));
        assert!(!s.matches(&KeyEvent::plain(Key::Char('r'))));
    }
}
