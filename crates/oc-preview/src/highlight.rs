//! Syntax highlighting with syntect, producing theme roles instead of colors.
//!
//! Syntaxes (syntect's defaults plus bat's extras via `two-face`) load once,
//! lazily. Rather than a color theme we highlight with a synthetic syntect
//! theme whose "colors" encode a [`SyntaxRole`]: syntect's own scope matching
//! does the hard work and the UI maps roles to the color scheme's palette
//! ([`palette`]), so theme switches need no re-highlighting.

use std::str::FromStr;
use std::sync::OnceLock;

use syntect::highlighting::{
    Color as SynColor, FontStyle, ScopeSelectors, Style, StyleModifier, Theme, ThemeItem, ThemeSettings,
};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::data::{Color, Rgb, StyledLine, SyntaxRole, TextStyle};

/// Larger texts are shown unhighlighted (as in the Python version, which
/// counted characters; see [`within_highlight_limit`]).
pub const MAX_HIGHLIGHT_BYTES: usize = 256 * 1024;

/// Whether `text` is small enough to highlight: at most
/// [`MAX_HIGHLIGHT_BYTES`] *characters*, like Python's `len(text)`, so
/// non-ASCII text isn't cut off earlier than before. Counting only happens
/// when the byte length is over the limit.
pub fn within_highlight_limit(text: &str) -> bool {
    text.len() <= MAX_HIGHLIGHT_BYTES || text.chars().nth(MAX_HIGHLIGHT_BYTES).is_none()
}

/// Poll for cancellation after this many lines or bytes, whichever comes
/// first (long minified lines are slow to highlight).
const CANCEL_CHECK_LINES: usize = 512;
const CANCEL_CHECK_BYTES: usize = 32 * 1024;

pub fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

/// The language for a file name, by full name ("Makefile") or extension;
/// None for plain text and unknown files.
pub fn syntax_for_file(name: &str) -> Option<&'static SyntaxReference> {
    let set = syntax_set();
    let found = set.find_syntax_by_extension(name).or_else(|| {
        let ext = name.rsplit_once('.').map(|(_, e)| e)?;
        set.find_syntax_by_extension(ext).or_else(|| set.find_syntax_by_extension(&ext.to_lowercase()))
    })?;
    (found.name != "Plain Text").then_some(found)
}

/// The language for a Markdown code fence ("rust", "py", "C++").
pub fn syntax_for_token(token: &str) -> Option<&'static SyntaxReference> {
    if token.is_empty() {
        return None;
    }
    let found = syntax_set().find_syntax_by_token(token)?;
    (found.name != "Plain Text").then_some(found)
}

// Markers in the synthetic theme's colors: g/b identify a role, r is its index.
const MARK_G: u8 = 0xab;
const MARK_B: u8 = 0xcd;

fn role_theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        use SyntaxRole::*;
        let rules: &[(&str, SyntaxRole)] = &[
            ("comment, punctuation.definition.comment", Comment),
            ("keyword, storage.type, storage.modifier, keyword.control", Keyword),
            ("keyword.operator, punctuation.separator.key-value.mapping", Operator),
            ("keyword.operator.word, keyword.operator.logical.python", Keyword),
            ("string, punctuation.definition.string", String),
            ("string.regexp", String),
            ("constant.character.escape, constant.character.entity", Escape),
            ("constant.numeric", Number),
            ("constant.language, constant.other, support.constant, variable.other.constant", Constant),
            ("variable.language, support.function.builtin, support.type.builtin", Builtin),
            ("entity.name.function, support.function, variable.function, meta.function-call.generic", Function),
            (
                "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, \
                 entity.other.inherited-class, support.class, support.type, storage.type.class",
                Type,
            ),
            ("variable.parameter, variable.other.member", Variable),
            ("punctuation", Punctuation),
            ("entity.name.tag, meta.tag.sgml", Tag),
            ("entity.other.attribute-name, meta.attribute-with-value", Attribute),
            ("meta.preprocessor, keyword.control.import.c, keyword.other.preprocessor, meta.annotation, meta.decorator", Preprocessor),
            ("markup.heading, entity.name.section", Heading),
            ("markup.inserted", Inserted),
            ("markup.deleted", Deleted),
            ("invalid", Error),
        ];
        let scopes = rules
            .iter()
            .filter_map(|&(sel, role)| {
                Some(ThemeItem {
                    scope: ScopeSelectors::from_str(sel).ok()?,
                    style: StyleModifier {
                        foreground: Some(SynColor { r: role as u8, g: MARK_G, b: MARK_B, a: 0xff }),
                        background: None,
                        font_style: Some(FontStyle::empty()),
                    },
                })
            })
            .collect();
        Theme {
            name: Some("oc-roles".into()),
            author: None,
            settings: ThemeSettings {
                foreground: Some(SynColor { r: 0, g: 0, b: 0, a: 0xff }),
                ..ThemeSettings::default()
            },
            scopes,
        }
    })
}

fn role_of(style: &Style) -> Option<SyntaxRole> {
    let c = style.foreground;
    (c.g == MARK_G && c.b == MARK_B).then(|| SyntaxRole::from_index(c.r)).flatten()
}

/// Highlight `text` as `syntax`, one [`StyledLine`] per "\n"-separated line
/// (a trailing "\n" gives a final empty line, like `str::split`). Polls
/// `is_cancelled` every few hundred lines (or few dozen KB). None if
/// cancelled, or if the grammar failed or panicked; callers then show plain
/// text.
pub fn highlight_lines(
    text: &str,
    syntax: &SyntaxReference,
    is_cancelled: &dyn Fn() -> bool,
) -> Option<Vec<StyledLine>> {
    // syntect compiles grammar regexes lazily and panics on ones the regex
    // engine rejects: degrade to plain text rather than fail the preview.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| highlight_unguarded(text, syntax, is_cancelled)))
        .unwrap_or_else(|_| {
            log::warn!("highlighting as {} panicked; showing plain text", syntax.name);
            None
        })
}

fn highlight_unguarded(
    text: &str,
    syntax: &SyntaxReference,
    is_cancelled: &dyn Fn() -> bool,
) -> Option<Vec<StyledLine>> {
    use syntect::easy::HighlightLines;
    let set = syntax_set();
    let mut h = HighlightLines::new(syntax, role_theme());
    let mut lines = Vec::with_capacity(text.len() / 32 + 1);
    let (mut lines_since_check, mut bytes_since_check) = (0usize, 0usize);
    for line in LinesWithEndings::from(text) {
        lines_since_check += 1;
        bytes_since_check += line.len();
        if lines_since_check >= CANCEL_CHECK_LINES || bytes_since_check >= CANCEL_CHECK_BYTES {
            if is_cancelled() {
                return None;
            }
            (lines_since_check, bytes_since_check) = (0, 0);
        }
        let ranges = h.highlight_line(line, set).ok()?;
        // The line's text without its ending ("\n" or "\r\n"); a lone "\r"
        // inside the line is content and stays, as in the unhighlighted view.
        let content_len = match line.strip_suffix('\n') {
            Some(rest) => rest.strip_suffix('\r').unwrap_or(rest).len(),
            None => line.len(),
        };
        let mut out = StyledLine { text: String::with_capacity(content_len), runs: Vec::new() };
        let mut pos = 0;
        for (style, piece) in ranges {
            let end = (pos + piece.len()).min(content_len);
            if pos < end {
                let style = role_of(&style).map_or_else(TextStyle::default, |r| TextStyle::fg(Color::Syntax(r)));
                out.push(&piece[..end - pos], style);
            }
            pos += piece.len();
        }
        lines.push(out);
    }
    if text.is_empty() || text.ends_with('\n') {
        lines.push(StyledLine::default());
    }
    Some(lines)
}

// ---------------------------------------------------------------------------
// Palettes: the colors of the Pygments styles the color schemes name
// ---------------------------------------------------------------------------

/// How a role looks in a palette.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RoleStyle {
    pub color: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
}

/// Role colors of one Pygments style ("monokai", "default", …).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntaxPalette {
    pub name: &'static str,
    pub roles: [RoleStyle; SyntaxRole::COUNT],
}

impl SyntaxPalette {
    pub fn style(&self, role: SyntaxRole) -> RoleStyle {
        self.roles[role as usize]
    }

    /// Resolve syntax roles in `style` to RGB (other colors are kept).
    pub fn resolve(&self, style: TextStyle) -> TextStyle {
        match style.fg {
            Some(Color::Syntax(role)) => {
                let rs = self.style(role);
                TextStyle {
                    fg: rs.color.map(Color::Rgb),
                    bold: style.bold || rs.bold,
                    italic: style.italic || rs.italic,
                    ..style
                }
            }
            _ => style,
        }
    }
}

const fn c(v: u32) -> RoleStyle {
    RoleStyle { color: Some(Rgb::hex(v)), bold: false, italic: false }
}
const fn b(v: u32) -> RoleStyle {
    RoleStyle { color: Some(Rgb::hex(v)), bold: true, italic: false }
}
const fn i(v: u32) -> RoleStyle {
    RoleStyle { color: Some(Rgb::hex(v)), bold: false, italic: true }
}
const N: RoleStyle = RoleStyle { color: None, bold: false, italic: false };

// Order: Comment, Keyword, String, Number, Constant, Builtin, Function, Type, Variable,
// Operator, Punctuation, Tag, Attribute, Escape, Preprocessor, Heading, Inserted, Deleted, Error
static PALETTES: [SyntaxPalette; 8] = [
    SyntaxPalette {
        name: "monokai",
        roles: [
            c(0x959077), c(0x66d9ef), c(0xe6db74), c(0xae81ff), c(0x66d9ef), c(0xf8f8f2), c(0xa6e22e), c(0xa6e22e),
            N, c(0xff4689), N, c(0xff4689), c(0xa6e22e), c(0xae81ff), c(0x75715e), b(0xf8f8f2), c(0xa6e22e),
            c(0xff4689), c(0xed007e),
        ],
    },
    SyntaxPalette {
        name: "dracula",
        roles: [
            c(0x6272a4), c(0xff79c6), c(0xf1fa8c), c(0xffb86c), c(0xbd93f9), c(0x8be9fd), c(0x50fa7b), c(0x8be9fd),
            c(0xf8f8f2), c(0xff79c6), N, c(0xff79c6), c(0x50fa7b), c(0xff79c6), c(0xff79c6), b(0xbd93f9), c(0x50fa7b),
            c(0xff5555), c(0xff5555),
        ],
    },
    SyntaxPalette {
        name: "nord",
        roles: [
            i(0x616e88), b(0x81a1c1), c(0xa3be8c), c(0xb48ead), c(0x81a1c1), c(0x81a1c1), c(0x88c0d0), c(0x8fbcbb),
            N, b(0x81a1c1), c(0xeceff4), c(0x81a1c1), c(0x8fbcbb), c(0xebcb8b), c(0x5e81ac), b(0x88c0d0), c(0xa3be8c),
            c(0xbf616a), c(0xbf616a),
        ],
    },
    SyntaxPalette {
        name: "gruvbox-dark",
        roles: [
            i(0x928374), c(0xfb4934), c(0xb8bb26), c(0xd3869b), c(0xd3869b), c(0xfe8019), c(0xfabd2f), c(0x8ec07c),
            c(0xebdbb2), c(0xfe8019), N, c(0x8ec07c), c(0xfabd2f), c(0xfe8019), c(0x8ec07c), b(0xebdbb2), c(0xb8bb26),
            c(0xfb4934), c(0xfb4934),
        ],
    },
    SyntaxPalette {
        name: "one-dark",
        roles: [
            i(0x7f848e), c(0xc678dd), c(0x98c379), c(0xd19a66), c(0xd19a66), c(0xe5c07b), c(0x61afef), c(0xe5c07b),
            c(0xe06c75), c(0x56b6c2), N, c(0xe06c75), c(0xd19a66), c(0x56b6c2), c(0xc678dd), b(0xe06c75), c(0x98c379),
            c(0xe06c75), c(0xe06c75),
        ],
    },
    SyntaxPalette {
        name: "native",
        roles: [
            i(0x999999), b(0x6ab825), c(0xed9d13), c(0x3677a9), b(0x6ab825), c(0x24909d), c(0x447fcf), c(0x447fcf),
            c(0x40ffff), N, N, b(0x6ab825), c(0xbbbbbb), c(0xed9d13), b(0xcd2828), b(0xffffff), c(0x589819),
            c(0xd22323), c(0xa61717),
        ],
    },
    SyntaxPalette {
        name: "solarized-light",
        roles: [
            i(0x93a1a1), c(0x859900), c(0x2aa198), c(0x2aa198), c(0xcb4b16), c(0x268bd2), c(0x268bd2), c(0x268bd2),
            c(0x268bd2), c(0x859900), N, c(0x268bd2), c(0x93a1a1), c(0xdc322f), c(0xcb4b16), b(0x268bd2), c(0x859900),
            c(0xdc322f), c(0xdc322f),
        ],
    },
    SyntaxPalette {
        name: "default",
        roles: [
            i(0x3d7b7b), b(0x008000), c(0xba2121), c(0x666666), b(0x008000), c(0x008000), c(0x0000ff), b(0x0000ff),
            c(0x19177c), c(0x666666), N, b(0x008000), c(0x687822), b(0xaa5d1f), c(0x9c6500), b(0x000080), c(0x008400),
            c(0xa00000), c(0xe40000),
        ],
    },
];

/// The palette for a Pygments style name; unknown names get "default".
pub fn palette(style: &str) -> &'static SyntaxPalette {
    PALETTES.iter().find(|p| p.name == style).unwrap_or(&PALETTES[PALETTES.len() - 1])
}

/// Names of the built-in palettes.
pub fn palette_names() -> impl Iterator<Item = &'static str> {
    PALETTES.iter().map(|p| p.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(line: &StyledLine) -> Vec<(&str, Option<SyntaxRole>)> {
        line.runs
            .iter()
            .map(|r| {
                let role = match r.style.fg {
                    Some(Color::Syntax(role)) => Some(role),
                    _ => None,
                };
                (&line.text[r.start..r.end], role)
            })
            .collect()
    }

    #[test]
    fn python_is_highlighted() {
        let syntax = syntax_for_file("x.py").expect("python");
        assert_eq!(syntax.name, "Python");
        let lines = highlight_lines("def f():\n    return \"😀 text\"  # note\n", syntax, &|| false).unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "def f():");
        let first = roles(&lines[0]);
        assert!(first.contains(&("def", Some(SyntaxRole::Keyword))), "{first:?}");
        assert!(first.iter().any(|(t, r)| *t == "f" && *r == Some(SyntaxRole::Function)), "{first:?}");
        let second = roles(&lines[1]);
        assert!(second.iter().any(|(t, r)| t.contains("note") && *r == Some(SyntaxRole::Comment)), "{second:?}");
        assert!(second.iter().any(|(t, r)| t.contains("😀") && *r == Some(SyntaxRole::String)), "{second:?}");
        assert!(lines[1].runs.iter().all(|r| r.end <= lines[1].text.len()));
        assert_eq!(lines[2].text, "");
    }

    #[test]
    fn plain_and_unknown_files_have_no_syntax() {
        assert!(syntax_for_file("notes.txt").is_none());
        assert!(syntax_for_file("noext").is_none());
        assert!(syntax_for_file("data.unknownext").is_none());
        assert_eq!(syntax_for_file("Makefile").map(|s| s.name.as_str()), Some("Makefile"));
        assert_eq!(syntax_for_file("MAIN.RS").map(|s| s.name.as_str()), Some("Rust"));
        assert!(syntax_for_file("Cargo.toml").is_some());
        assert_eq!(syntax_for_token("rust").map(|s| s.name.as_str()), Some("Rust"));
        assert!(syntax_for_token("").is_none());
    }

    #[test]
    fn cancellation_stops_highlighting() {
        let text = "x = 1\n".repeat(2000);
        assert!(highlight_lines(&text, syntax_for_file("a.py").unwrap(), &|| true).is_none());
    }

    #[test]
    fn palettes_resolve_roles() {
        assert_eq!(palette("monokai").name, "monokai");
        assert_eq!(palette("no-such-style").name, "default");
        assert_eq!(palette_names().count(), 8);
        let p = palette("nord");
        let resolved = p.resolve(TextStyle::fg(Color::Syntax(SyntaxRole::Comment)));
        assert_eq!(resolved.fg, Some(Color::Rgb(Rgb::hex(0x616e88))));
        assert!(resolved.italic);
        let link = TextStyle::fg(Color::Theme(crate::data::ThemeRole::Link));
        assert_eq!(p.resolve(link), link);
    }
}
