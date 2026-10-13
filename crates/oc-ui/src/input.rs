//! Input events in the UI's own terms, independent of winit, so state
//! machines can be driven by tests. [`from_winit`] converts.

use oc_grid::winit;

/// A key, as it is without modifiers: Shift+. is `Char('.')` with `shift`
/// set (the produced text, ">", is in [`KeyEvent::text`]). Letters are lower case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Enter,
    Escape,
    Tab,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    Space,
    /// F1 = `F(1)`.
    F(u8),
    /// Anything else (media keys, lone modifiers…).
    Other,
}

/// Modifier keys. `logo` is ⌘ on macOS (Qt's ControlModifier there), the
/// Windows/Super key elsewhere.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub logo: bool,
}

impl Mods {
    pub const NONE: Mods = Mods { shift: false, ctrl: false, alt: false, logo: false };
    pub const SHIFT: Mods = Mods { shift: true, ..Mods::NONE };
    pub const ALT: Mods = Mods { alt: true, ..Mods::NONE };

    /// The platform's main shortcut modifier: ⌘ on macOS, Ctrl elsewhere.
    pub const fn primary() -> Mods {
        if cfg!(target_os = "macos") { Mods { logo: true, ..Mods::NONE } } else { Mods { ctrl: true, ..Mods::NONE } }
    }

    pub fn is_empty(&self) -> bool {
        *self == Mods::NONE
    }

    /// Whether the primary modifier (⌘ / Ctrl) is held.
    pub fn has_primary(&self) -> bool {
        if cfg!(target_os = "macos") { self.logo } else { self.ctrl }
    }

    /// Only Shift (or nothing): typing, not a shortcut.
    pub fn is_typing(&self) -> bool {
        !self.ctrl && !self.logo && !(self.alt && !cfg!(target_os = "macos"))
    }

    pub fn union(self, o: Mods) -> Mods {
        Mods { shift: self.shift || o.shift, ctrl: self.ctrl || o.ctrl, alt: self.alt || o.alt, logo: self.logo || o.logo }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: Key,
    pub mods: Mods,
    /// What the key types (with Shift/Option applied), if anything.
    pub text: Option<String>,
}

impl KeyEvent {
    pub fn new(key: Key, mods: Mods) -> Self {
        let text = match key {
            Key::Char(c) if mods.is_typing() => Some(if mods.shift { c.to_uppercase().to_string() } else { c.to_string() }),
            Key::Space if mods.is_typing() => Some(" ".into()),
            _ => None,
        };
        KeyEvent { key, mods, text }
    }

    /// A plain key without modifiers.
    pub fn plain(key: Key) -> Self {
        KeyEvent::new(key, Mods::NONE)
    }

    /// A key typing `text` (e.g. "+" from Shift+=).
    pub fn typed(text: &str) -> Self {
        let c = text.chars().next().unwrap_or(' ');
        let key = if c == ' ' { Key::Space } else { Key::Char(c.to_ascii_lowercase()) };
        KeyEvent { key, mods: Mods::NONE, text: Some(text.into()) }
    }

    pub fn with(key: Key, mods: Mods) -> Self {
        KeyEvent::new(key, mods)
    }

    pub fn is(&self, key: Key) -> bool {
        self.key == key && self.mods.is_empty()
    }

    /// Typed text, if this key types (no ⌘/Ctrl held).
    pub fn typed_text(&self) -> Option<&str> {
        self.text.as_deref().filter(|t| !t.is_empty() && !t.chars().any(char::is_control) && self.mods.is_typing())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MouseKind {
    Down(MouseButton),
    Up(MouseButton),
    /// Second press within the double-click interval at the same cell.
    DoubleClick(MouseButton),
    Move,
    /// Lines to scroll; positive = towards the top (content moves down).
    Scroll(f32),
    /// Columns to scroll sideways (trackpads, tilt wheels); positive =
    /// towards the left (content moves right).
    HScroll(f32),
}

/// The mouse pointer's shape over a window (`App::pointer`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Pointer {
    #[default]
    Default,
    /// The I-beam: text can be selected here.
    Text,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MouseEvent {
    pub kind: MouseKind,
    pub col: u16,
    pub row: u16,
    pub mods: Mods,
}

/// Everything a window's UI reacts to.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    /// IME composition in progress (shown at the cursor, not yet typed).
    Preedit(String),
    /// IME committed text.
    Commit(String),
    /// The window gained (true) or lost focus.
    Focus(bool),
}

impl Event {
    pub fn key(key: Key) -> Event {
        Event::Key(KeyEvent::plain(key))
    }

    pub fn as_key(&self) -> Option<&KeyEvent> {
        match self {
            Event::Key(k) => Some(k),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// winit conversion
// ---------------------------------------------------------------------------

pub fn mods_from_winit(m: winit::keyboard::ModifiersState) -> Mods {
    Mods { shift: m.shift_key(), ctrl: m.control_key(), alt: m.alt_key(), logo: m.super_key() }
}

/// Convert a winit key press. `unmodified` is the key without modifiers
/// (`KeyEventExtModifierSupplement::key_without_modifiers`) so ⌥⌘A still
/// reads as "a" on macOS, where Option changes the logical key to "å".
pub fn from_winit(logical: &winit::keyboard::Key, unmodified: &winit::keyboard::Key, text: Option<&str>, mods: Mods) -> Option<KeyEvent> {
    use winit::keyboard::{Key as WKey, NamedKey as N};
    let named = |k: &WKey| -> Option<Key> {
        Some(match k {
            WKey::Named(n) => match n {
                N::Enter => Key::Enter,
                N::Escape => Key::Escape,
                N::Tab => Key::Tab,
                N::Backspace => Key::Backspace,
                N::Delete => Key::Delete,
                N::Insert => Key::Insert,
                N::Home => Key::Home,
                N::End => Key::End,
                N::PageUp => Key::PageUp,
                N::PageDown => Key::PageDown,
                N::ArrowUp => Key::Up,
                N::ArrowDown => Key::Down,
                N::ArrowLeft => Key::Left,
                N::ArrowRight => Key::Right,
                N::Space => Key::Space,
                N::F1 => Key::F(1),
                N::F2 => Key::F(2),
                N::F3 => Key::F(3),
                N::F4 => Key::F(4),
                N::F5 => Key::F(5),
                N::F6 => Key::F(6),
                N::F7 => Key::F(7),
                N::F8 => Key::F(8),
                N::F9 => Key::F(9),
                N::F10 => Key::F(10),
                N::F11 => Key::F(11),
                N::F12 => Key::F(12),
                N::Shift | N::Control | N::Alt | N::Super | N::Meta | N::CapsLock | N::Fn => {
                    return None;
                }
                _ => Key::Other,
            },
            WKey::Character(s) => {
                let mut chars = s.chars();
                let c = chars.next()?;
                if c == ' ' { Key::Space } else { Key::Char(c.to_lowercase().next().unwrap_or(c)) }
            }
            _ => return None,
        })
    };
    let key = match (named(unmodified), named(logical)) {
        (Some(Key::Char(c)), _) => Key::Char(c),
        (_, Some(k)) => k,
        (Some(k), None) => k,
        (None, None) => return None,
    };
    let text = text.map(str::to_owned).or_else(|| match logical {
        WKey::Character(s) if mods.is_typing() => Some(s.to_string()),
        _ => None,
    });
    let text = text.filter(|t| !t.is_empty() && !t.chars().any(char::is_control));
    Some(KeyEvent { key, mods, text })
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::{Key as WKey, NamedKey, SmolStr};

    #[test]
    fn winit_keys_convert() {
        let primary = Mods::primary();
        let k = from_winit(&WKey::Character(SmolStr::new("å")), &WKey::Character(SmolStr::new("a")), None, primary).unwrap();
        assert_eq!(k.key, Key::Char('a'));
        let k = from_winit(&WKey::Named(NamedKey::F5), &WKey::Named(NamedKey::F5), None, Mods::NONE).unwrap();
        assert_eq!(k.key, Key::F(5));
        let plus = from_winit(&WKey::Character(SmolStr::new("+")), &WKey::Character(SmolStr::new("=")), Some("+"), Mods::SHIFT).unwrap();
        assert_eq!((plus.key, plus.typed_text()), (Key::Char('='), Some("+")));
        assert!(from_winit(&WKey::Named(NamedKey::Shift), &WKey::Named(NamedKey::Shift), None, Mods::NONE).is_none());
    }
}
