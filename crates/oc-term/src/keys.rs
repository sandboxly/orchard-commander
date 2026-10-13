//! Keyboard input → the bytes a shell expects, independent of any UI toolkit.
//!
//! The UI maps its key events onto [`Key`] + [`Modifiers`] (+ the text the
//! key typed) and sends whatever [`encode_key`] returns. Platform quirks
//! (Qt calling ⌘ "Control" on macOS, ⌘ shortcuts staying with the app) are
//! the UI's business: `ctrl` here is always the physical Control key.

use std::borrow::Cow;

/// Modifier keys held during a key or mouse event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub shift: bool,
    /// Alt / ⌥ Option.
    pub alt: bool,
    /// The Control key (⌃ on macOS, not ⌘).
    pub ctrl: bool,
}

impl Modifiers {
    pub const NONE: Self = Self {
        shift: false,
        alt: false,
        ctrl: false,
    };
    pub const SHIFT: Self = Self {
        shift: true,
        alt: false,
        ctrl: false,
    };
    pub const ALT: Self = Self {
        shift: false,
        alt: true,
        ctrl: false,
    };
    pub const CTRL: Self = Self {
        shift: false,
        alt: false,
        ctrl: true,
    };
}

/// A key, as far as the terminal cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// A key that types a character; for letters either case (like Qt's key
    /// codes, the key not the text: ⌥O on macOS is `Char('o')` typing "ø").
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// Return and keypad Enter.
    Enter,
    Backspace,
    Tab,
    /// Shift+Tab as some toolkits report it.
    BackTab,
    Escape,
    /// F1…F12 (others send nothing).
    F(u8),
}

/// Terminal state and platform conventions that change what keys send.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyOptions {
    /// DECCKM (`CSI ? 1 h`): arrows send `ESC O A` instead of `ESC [ A` (vim, less).
    pub app_cursor: bool,
    /// Alt+text sends ESC + text ("Meta"). Off on macOS, where ⌥ types
    /// characters like ø.
    pub alt_sends_escape: bool,
}

impl KeyOptions {
    /// The platform default for [`KeyOptions::alt_sends_escape`].
    pub fn for_platform(app_cursor: bool) -> Self {
        Self {
            app_cursor,
            alt_sends_escape: !cfg!(target_os = "macos"),
        }
    }
}

/// Keys that send escape sequences (normal cursor-key mode).
fn sequence(key: Key) -> Option<&'static [u8]> {
    Some(match key {
        Key::Up => b"\x1b[A",
        Key::Down => b"\x1b[B",
        Key::Right => b"\x1b[C",
        Key::Left => b"\x1b[D",
        Key::Home => b"\x1b[H",
        Key::End => b"\x1b[F",
        Key::PageUp => b"\x1b[5~",
        Key::PageDown => b"\x1b[6~",
        Key::Insert => b"\x1b[2~",
        Key::Delete => b"\x1b[3~",
        Key::Enter => b"\r",
        Key::Backspace => b"\x7f",
        Key::Tab => b"\t",
        Key::BackTab => b"\x1b[Z",
        Key::Escape => b"\x1b",
        Key::F(n) => match n {
            1 => b"\x1bOP",
            2 => b"\x1bOQ",
            3 => b"\x1bOR",
            4 => b"\x1bOS",
            5 => b"\x1b[15~",
            6 => b"\x1b[17~",
            7 => b"\x1b[18~",
            8 => b"\x1b[19~",
            9 => b"\x1b[20~",
            10 => b"\x1b[21~",
            11 => b"\x1b[23~",
            12 => b"\x1b[24~",
            _ => return None,
        },
        Key::Char(_) => return None,
    })
}

/// Application cursor keys (DECCKM).
fn app_cursor_sequence(key: Key) -> Option<&'static [u8]> {
    Some(match key {
        Key::Up => b"\x1bOA",
        Key::Down => b"\x1bOB",
        Key::Right => b"\x1bOC",
        Key::Left => b"\x1bOD",
        Key::Home => b"\x1bOH",
        Key::End => b"\x1bOF",
        _ => return None,
    })
}

/// What an arrow key sends in the given cursor mode (also used to turn wheel
/// steps into arrow presses for full-screen programs without mouse support).
pub fn arrow(up: bool, app_cursor: bool) -> &'static [u8] {
    match (up, app_cursor) {
        (true, false) => b"\x1b[A",
        (false, false) => b"\x1b[B",
        (true, true) => b"\x1bOA",
        (false, true) => b"\x1bOB",
    }
}

/// What a key press sends to the shell; `None` for nothing.
///
/// `text` is what the key typed (may be empty, e.g. for Ctrl combos).
pub fn encode_key(
    key: Key,
    mods: Modifiers,
    text: &str,
    opts: KeyOptions,
) -> Option<Cow<'static, [u8]>> {
    if mods.ctrl
        && let Key::Char(c) = key
    {
        if c.is_ascii_alphabetic() {
            // Ctrl+C → ETX, …
            return Some(Cow::Owned(vec![(c.to_ascii_uppercase() as u8) - b'A' + 1]));
        }
        let control: Option<&'static [u8]> = match c {
            ' ' | '@' | '2' => Some(b"\x00"),
            '[' => Some(b"\x1b"),
            '\\' => Some(b"\x1c"),
            ']' => Some(b"\x1d"),
            '^' => Some(b"\x1e"),
            '_' | '-' => Some(b"\x1f"),
            _ => None,
        };
        if let Some(bytes) = control {
            return Some(Cow::Borrowed(bytes));
        }
    }
    if mods.alt {
        // ⌥←/⌥→/⌥⌫: by word, as in Terminal.app and iTerm
        let word: Option<&'static [u8]> = match key {
            Key::Left => Some(b"\x1bb"),
            Key::Right => Some(b"\x1bf"),
            Key::Backspace => Some(b"\x1b\x7f"),
            _ => None,
        };
        if let Some(bytes) = word {
            return Some(Cow::Borrowed(bytes));
        }
    }
    if opts.app_cursor
        && !mods.ctrl
        && !mods.alt
        && let Some(bytes) = app_cursor_sequence(key)
    {
        return Some(Cow::Borrowed(bytes));
    }
    if let Some(bytes) = sequence(key) {
        if mods.shift && key == Key::Tab {
            return Some(Cow::Borrowed(b"\x1b[Z"));
        }
        return Some(Cow::Borrowed(bytes));
    }
    if text.is_empty() {
        return None;
    }
    if mods.alt && opts.alt_sends_escape {
        let mut out = Vec::with_capacity(text.len() + 1);
        out.push(0x1b);
        out.extend_from_slice(text.as_bytes());
        return Some(Cow::Owned(out));
    }
    Some(Cow::Owned(text.as_bytes().to_vec()))
}

/// Pasted text as it is sent: line breaks as Return, and wrapped in
/// bracketed-paste markers when the program asked (`CSI ? 2004 h`) so the
/// shell doesn't run pasted lines one by one.
pub fn encode_paste(text: &str, bracketed: bool) -> String {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        // A pasted end marker would end the paste early and run the rest.
        let text = text.replace("\x1b[201~", "");
        format!("\x1b[200~{text}\x1b[201~")
    } else {
        text
    }
}

/// Focus report (`CSI ? 1004 h`): vim reloads changed files on focus.
pub fn encode_focus(focused: bool) -> &'static [u8] {
    if focused { b"\x1b[I" } else { b"\x1b[O" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(key: Key, mods: Modifiers, text: &str, app_cursor: bool) -> Vec<u8> {
        let opts = KeyOptions {
            app_cursor,
            alt_sends_escape: true,
        };
        encode_key(key, mods, text, opts)
            .map(Cow::into_owned)
            .unwrap_or_default()
    }

    #[test]
    fn key_translation_matches_python() {
        let none = Modifiers::NONE;
        let cases: &[(Key, Modifiers, &str, bool, &[u8])] = &[
            (Key::Char('C'), Modifiers::CTRL, "", false, b"\x03"),
            (Key::Char('d'), Modifiers::CTRL, "", false, b"\x04"),
            (Key::Up, none, "", false, b"\x1b[A"),
            (Key::Up, none, "", true, b"\x1bOA"), // vim, less: application cursor keys
            (Key::Enter, none, "\r", false, b"\r"),
            (Key::Backspace, none, "", false, b"\x7f"),
            (Key::Escape, none, "", false, b"\x1b"),
            (Key::Tab, none, "\t", false, b"\t"),
            (Key::Tab, Modifiers::SHIFT, "", false, b"\x1b[Z"),
            (Key::Left, Modifiers::ALT, "", false, b"\x1bb"), // a word back
            (Key::F(5), none, "", false, b"\x1b[15~"),
            (Key::Char('A'), none, "a", false, b"a"),
        ];
        for &(key, mods, text, app, expected) in cases {
            assert_eq!(
                k(key, mods, text, app),
                expected,
                "{key:?} {mods:?} app={app}"
            );
        }
    }

    #[test]
    fn control_symbols_and_alt() {
        assert_eq!(k(Key::Char(' '), Modifiers::CTRL, "", false), b"\x00");
        assert_eq!(k(Key::Char('['), Modifiers::CTRL, "", false), b"\x1b");
        assert_eq!(k(Key::Char('_'), Modifiers::CTRL, "", false), b"\x1f");
        // Ctrl+digit without a mapping: falls through to the text (none here)
        assert_eq!(k(Key::Char('5'), Modifiers::CTRL, "", false), b"");
        assert_eq!(k(Key::Char('x'), Modifiers::ALT, "x", false), b"\x1bx");
        let mac = KeyOptions {
            app_cursor: false,
            alt_sends_escape: false,
        };
        assert_eq!(
            encode_key(Key::Char('o'), Modifiers::ALT, "ø", mac)
                .unwrap()
                .as_ref(),
            "ø".as_bytes()
        );
        // Alt+arrow in app cursor mode stays a word key; Ctrl+arrow is the plain sequence
        assert_eq!(k(Key::Right, Modifiers::ALT, "", true), b"\x1bf");
        assert_eq!(k(Key::Up, Modifiers::CTRL, "", true), b"\x1b[A");
        assert_eq!(k(Key::Home, none(), "", true), b"\x1bOH");
        assert_eq!(k(Key::F(13), none(), "", false), b"");
        assert_eq!(k(Key::Backspace, Modifiers::ALT, "", false), b"\x1b\x7f");
    }

    fn none() -> Modifiers {
        Modifiers::NONE
    }

    #[test]
    fn paste_and_focus() {
        assert_eq!(encode_paste("a\r\nb\nc", false), "a\rb\rc");
        assert_eq!(encode_paste("ls\n", true), "\x1b[200~ls\r\x1b[201~");
        assert_eq!(encode_paste("x\x1b[201~rm", true), "\x1b[200~xrm\x1b[201~");
        assert_eq!(encode_focus(true), b"\x1b[I");
        assert_eq!(encode_focus(false), b"\x1b[O");
        assert_eq!(arrow(true, true), b"\x1bOA");
    }
}
