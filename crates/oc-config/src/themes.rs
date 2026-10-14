//! The look of the app: color schemes, fonts and frame styles.
//!
//! A theme is the whole appearance ([`Appearance`]): a color scheme (or one for
//! light and one for dark system mode), a font, line spacing and a frame style.
//! Color schemes are named, shareable data: the built-ins below plus TOML files
//! in the config directory's `color-schemes/` folder, which the scheme editor
//! writes. The UI derives its whole look from a scheme's colors.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::ConfigError;
use crate::pyjson::string_literal;

/// A named set of colors ("#rrggbb", lower case) plus a syntax highlighting style.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ColorScheme {
    pub name: String,
    pub dark: bool,
    /// Window background, around the panels.
    pub background: String,
    /// Inside panels and dialogs.
    pub surface: String,
    pub foreground: String,
    /// Secondary text, inactive borders' titles.
    pub muted: String,
    /// Inactive panel border.
    pub border: String,
    /// Active panel border and title chip.
    pub border_active: String,
    /// Column headers, dialog titles.
    pub accent: String,
    pub cursor_bg: String,
    pub cursor_fg: String,
    /// Cursor in the panel without focus.
    pub cursor_inactive_bg: String,
    pub dir: String,
    pub exec: String,
    pub link: String,
    pub hidden: String,
    pub marked: String,
    pub error: String,
    pub warning: String,
    pub success: String,
    pub footer_bg: String,
    pub footer_fg: String,
    pub footer_key: String,
    /// A Pygments style name; the previewer maps it to its highlighter's theme.
    pub pygments_style: String,
}

/// Every color of a scheme: (field, label, group), in the order the scheme editor shows them.
pub const COLOR_ROLES: &[(&str, &str, &str)] = &[
    ("background", "Window background", "General"),
    ("surface", "Panel background", "General"),
    ("foreground", "Text", "General"),
    ("muted", "Secondary text", "General"),
    ("accent", "Headers and titles", "General"),
    ("border", "Border", "General"),
    ("border_active", "Active border", "General"),
    ("cursor_bg", "Cursor", "Cursor"),
    ("cursor_fg", "Cursor text", "Cursor"),
    ("cursor_inactive_bg", "Cursor, inactive panel", "Cursor"),
    ("dir", "Directories", "Files"),
    ("exec", "Executables", "Files"),
    ("link", "Symbolic links", "Files"),
    ("hidden", "Hidden files", "Files"),
    ("marked", "Marked files", "Files"),
    ("error", "Errors", "Messages"),
    ("warning", "Warnings", "Messages"),
    ("success", "Success", "Messages"),
    ("footer_bg", "Key bar background", "Key bar"),
    ("footer_fg", "Key bar text", "Key bar"),
    ("footer_key", "Key bar keys", "Key bar"),
];

impl ColorScheme {
    /// A color by role name (see [`COLOR_ROLES`]); `None` for unknown roles.
    pub fn color(&self, role: &str) -> Option<&str> {
        Some(match role {
            "background" => &self.background,
            "surface" => &self.surface,
            "foreground" => &self.foreground,
            "muted" => &self.muted,
            "border" => &self.border,
            "border_active" => &self.border_active,
            "accent" => &self.accent,
            "cursor_bg" => &self.cursor_bg,
            "cursor_fg" => &self.cursor_fg,
            "cursor_inactive_bg" => &self.cursor_inactive_bg,
            "dir" => &self.dir,
            "exec" => &self.exec,
            "link" => &self.link,
            "hidden" => &self.hidden,
            "marked" => &self.marked,
            "error" => &self.error,
            "warning" => &self.warning,
            "success" => &self.success,
            "footer_bg" => &self.footer_bg,
            "footer_fg" => &self.footer_fg,
            "footer_key" => &self.footer_key,
            _ => return None,
        })
    }

    /// Mutable access to a color by role name; `None` for unknown roles.
    pub fn color_mut(&mut self, role: &str) -> Option<&mut String> {
        Some(match role {
            "background" => &mut self.background,
            "surface" => &mut self.surface,
            "foreground" => &mut self.foreground,
            "muted" => &mut self.muted,
            "border" => &mut self.border,
            "border_active" => &mut self.border_active,
            "accent" => &mut self.accent,
            "cursor_bg" => &mut self.cursor_bg,
            "cursor_fg" => &mut self.cursor_fg,
            "cursor_inactive_bg" => &mut self.cursor_inactive_bg,
            "dir" => &mut self.dir,
            "exec" => &mut self.exec,
            "link" => &mut self.link,
            "hidden" => &mut self.hidden,
            "marked" => &mut self.marked,
            "error" => &mut self.error,
            "warning" => &mut self.warning,
            "success" => &mut self.success,
            "footer_bg" => &mut self.footer_bg,
            "footer_fg" => &mut self.footer_fg,
            "footer_key" => &mut self.footer_key,
            _ => return None,
        })
    }

    /// The same scheme under another name.
    pub fn renamed(&self, name: impl Into<String>) -> Self {
        ColorScheme {
            name: name.into(),
            ..self.clone()
        }
    }
}

// ---------------------------------------------------------------------------
// Built-in schemes
// ---------------------------------------------------------------------------

pub const DEFAULT_SCHEME: &str = "monokai";
pub const DEFAULT_LIGHT_SCHEME: &str = "solarized-light";

/// name, dark, the 21 colors in [`COLOR_ROLES`] *field* order (as declared on
/// the struct), pygments style.
type Builtin = (&'static str, bool, [&'static str; 21], &'static str);

#[rustfmt::skip]
const BUILTINS: [Builtin; 8] = [
    ("monokai", true, [
        "#1e1f1c", "#272822", "#f8f8f2", "#75715e", "#49483e", "#a6e22e", "#fd971f",
        "#66d9ef", "#1e1f1c", "#3e3d32", "#66d9ef", "#a6e22e", "#ae81ff", "#75715e",
        "#e6db74", "#f92672", "#fd971f", "#a6e22e", "#3e3d32", "#f8f8f2", "#fd971f",
    ], "monokai"),
    ("dracula", true, [
        "#21222c", "#282a36", "#f8f8f2", "#6272a4", "#44475a", "#bd93f9", "#ff79c6",
        "#bd93f9", "#282a36", "#44475a", "#8be9fd", "#50fa7b", "#ff79c6", "#6272a4",
        "#f1fa8c", "#ff5555", "#ffb86c", "#50fa7b", "#44475a", "#f8f8f2", "#ff79c6",
    ], "dracula"),
    ("nord", true, [
        "#242933", "#2e3440", "#d8dee9", "#616e88", "#434c5e", "#88c0d0", "#ebcb8b",
        "#88c0d0", "#2e3440", "#3b4252", "#81a1c1", "#a3be8c", "#b48ead", "#616e88",
        "#ebcb8b", "#bf616a", "#d08770", "#a3be8c", "#3b4252", "#d8dee9", "#88c0d0",
    ], "nord"),
    ("gruvbox", true, [
        "#1d2021", "#282828", "#ebdbb2", "#928374", "#504945", "#fabd2f", "#fe8019",
        "#fabd2f", "#282828", "#3c3836", "#83a598", "#b8bb26", "#d3869b", "#928374",
        "#fe8019", "#fb4934", "#fe8019", "#b8bb26", "#3c3836", "#ebdbb2", "#fabd2f",
    ], "gruvbox-dark"),
    ("tokyo-night", true, [
        "#16161e", "#1a1b26", "#c0caf5", "#565f89", "#292e42", "#7aa2f7", "#bb9af7",
        "#7aa2f7", "#1a1b26", "#292e42", "#7aa2f7", "#9ece6a", "#7dcfff", "#565f89",
        "#e0af68", "#f7768e", "#e0af68", "#9ece6a", "#24283b", "#c0caf5", "#bb9af7",
    ], "one-dark"),
    ("commander", true, [
        "#000060", "#000080", "#55ffff", "#5f87af", "#00a8a8", "#55ffff", "#ffff55",
        "#00a8a8", "#000000", "#005f87", "#ffffff", "#55ff55", "#ff55ff", "#5f87af",
        "#ffff55", "#ff5555", "#ffff55", "#55ff55", "#00a8a8", "#000000", "#ffffff",
    ], "native"),
    ("solarized-light", false, [
        "#eee8d5", "#fdf6e3", "#586e75", "#93a1a1", "#d3cbb7", "#268bd2", "#cb4b16",
        "#268bd2", "#fdf6e3", "#e4ddc8", "#268bd2", "#859900", "#6c71c4", "#93a1a1",
        "#cb4b16", "#dc322f", "#b58900", "#859900", "#e4ddc8", "#586e75", "#cb4b16",
    ], "solarized-light"),
    ("paper", false, [
        "#e8e8e3", "#fafaf7", "#1c1c1c", "#8a8a8a", "#c8c8c0", "#005f87", "#af5f00",
        "#005f87", "#ffffff", "#dcdcd4", "#005f87", "#008700", "#8700af", "#8a8a8a",
        "#d75f00", "#d70000", "#af5f00", "#008700", "#dcdcd4", "#1c1c1c", "#af5f00",
    ], "default"),
];

fn build((name, dark, c, pygments): &Builtin) -> ColorScheme {
    let s = |i: usize| c[i].to_owned();
    ColorScheme {
        name: (*name).to_owned(),
        dark: *dark,
        background: s(0),
        surface: s(1),
        foreground: s(2),
        muted: s(3),
        border: s(4),
        border_active: s(5),
        accent: s(6),
        cursor_bg: s(7),
        cursor_fg: s(8),
        cursor_inactive_bg: s(9),
        dir: s(10),
        exec: s(11),
        link: s(12),
        hidden: s(13),
        marked: s(14),
        error: s(15),
        warning: s(16),
        success: s(17),
        footer_bg: s(18),
        footer_fg: s(19),
        footer_key: s(20),
        pygments_style: (*pygments).to_owned(),
    }
}

/// The built-in schemes, in the order menus show them.
pub fn builtin_schemes() -> &'static [ColorScheme] {
    static SCHEMES: OnceLock<Vec<ColorScheme>> = OnceLock::new();
    SCHEMES.get_or_init(|| BUILTINS.iter().map(build).collect())
}

pub fn builtin_scheme(name: &str) -> Option<&'static ColorScheme> {
    builtin_schemes().iter().find(|s| s.name == name)
}

pub fn is_builtin(name: &str) -> bool {
    BUILTINS.iter().any(|b| b.0 == name)
}

fn default_scheme(dark: bool) -> &'static ColorScheme {
    builtin_scheme(if dark {
        DEFAULT_SCHEME
    } else {
        DEFAULT_LIGHT_SCHEME
    })
    .expect("default schemes are built in")
}

// ---------------------------------------------------------------------------
// User color schemes (TOML files)
// ---------------------------------------------------------------------------

/// A "#rrggbb" color, as schemes store them.
pub fn is_color(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() == 7 && b[0] == b'#' && b[1..].iter().all(u8::is_ascii_hexdigit)
}

/// A scheme from parsed TOML. Missing or invalid colors come from a built-in
/// scheme of the same brightness, so files stay valid when roles are added.
pub fn scheme_from_table(data: &toml::Table, fallback_name: &str) -> ColorScheme {
    let dark = !matches!(data.get("dark"), Some(toml::Value::Boolean(false)));
    let mut scheme = default_scheme(dark).clone();
    for (role, _, _) in COLOR_ROLES {
        if let Some(toml::Value::String(value)) = data.get(*role)
            && is_color(value)
        {
            *scheme.color_mut(role).expect("COLOR_ROLES are fields") = value.to_ascii_lowercase();
        }
    }
    let name = match data.get("name").filter(|v| toml_truthy(v)) {
        Some(value) => toml_str(value),
        None => fallback_name.to_owned(),
    };
    scheme.name = name.trim().to_owned();
    scheme.dark = dark;
    if let Some(toml::Value::String(style)) = data.get("pygments_style")
        && !style.is_empty()
    {
        scheme.pygments_style = style.clone();
    }
    scheme
}

/// The TOML file the scheme editor writes (with comments naming each role).
pub fn scheme_to_toml(scheme: &ColorScheme) -> String {
    use std::fmt::Write;

    let width = COLOR_ROLES
        .iter()
        .map(|(role, _, _)| role.len())
        .max()
        .unwrap_or(0);
    let mut out = String::with_capacity(1500);
    let _ = writeln!(out, "# {}: an Orchard Commander color scheme", scheme.name);
    let _ = writeln!(out, "name = {}", string_literal(&scheme.name));
    let _ = writeln!(out, "dark = {}", scheme.dark);
    let _ = writeln!(
        out,
        "pygments_style = {}  # syntax highlighting in text previews",
        string_literal(&scheme.pygments_style)
    );
    out.push('\n');
    for (role, label, _) in COLOR_ROLES {
        let value = scheme.color(role).expect("COLOR_ROLES are fields");
        let _ = writeln!(out, "{role:<width$} = {}  # {label}", string_literal(value));
    }
    out
}

/// Python truthiness of a TOML value (`data.get("name") or fallback`).
fn toml_truthy(value: &toml::Value) -> bool {
    match value {
        toml::Value::String(s) => !s.is_empty(),
        toml::Value::Integer(i) => *i != 0,
        toml::Value::Float(f) => *f != 0.0,
        toml::Value::Boolean(b) => *b,
        toml::Value::Datetime(_) => true,
        toml::Value::Array(a) => !a.is_empty(),
        toml::Value::Table(t) => !t.is_empty(),
    }
}

/// Python's `str()` of a TOML value, near enough for a name.
fn toml_str(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => s.clone(),
        toml::Value::Boolean(true) => "True".into(),
        toml::Value::Boolean(false) => "False".into(),
        other => other.to_string(),
    }
}

/// User schemes by name, with their files, in file name order. Unreadable
/// files, nameless schemes, built-in names and later duplicates are skipped.
fn user_schemes(dir: &Path) -> Vec<(ColorScheme, PathBuf)> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(String, PathBuf)> = read
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            is_toml_name(&name).then(|| (name, e.path()))
        })
        .collect();
    // Python sorts the paths: case-insensitively on Windows (WindowsPath
    // compares lower-cased), by code point elsewhere. The order decides
    // which file wins when two use the same scheme name.
    if cfg!(windows) {
        files.sort_by_cached_key(|(name, _)| name.to_lowercase());
    } else {
        files.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    }
    let mut found: Vec<(ColorScheme, PathBuf)> = Vec::new();
    for (_, path) in files {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(data) = text.parse::<toml::Table>() else {
            continue;
        };
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default();
        let scheme = scheme_from_table(&data, &stem);
        if !scheme.name.is_empty()
            && !is_builtin(&scheme.name)
            && !found.iter().any(|(s, _)| s.name == scheme.name)
        {
            found.push((scheme, path));
        }
    }
    found
}

/// `*.toml` as Python's `Path.glob` matches it (case-insensitively only on Windows).
fn is_toml_name(name: &str) -> bool {
    if cfg!(windows) {
        name.len() >= 5
            && name.is_char_boundary(name.len() - 5)
            && name[name.len() - 5..].eq_ignore_ascii_case(".toml")
    } else {
        name.ends_with(".toml")
    }
}

/// Built-in schemes, then the user's (from `dir`, the `color-schemes` folder).
/// Read from disk on every call: another running instance may have saved one.
pub fn all_schemes(dir: &Path) -> Vec<ColorScheme> {
    let mut all = builtin_schemes().to_vec();
    all.extend(user_schemes(dir).into_iter().map(|(s, _)| s));
    all
}

/// The scheme with this name, or the default scheme.
pub fn get_scheme(dir: &Path, name: &str) -> ColorScheme {
    if let Some(s) = builtin_scheme(name) {
        return s.clone();
    }
    user_schemes(dir)
        .into_iter()
        .find_map(|(s, _)| (s.name == name).then_some(s))
        .unwrap_or_else(|| default_scheme(true).clone())
}

/// Write `scheme` to the user's scheme folder `dir`. `replaces` is the user
/// scheme being edited (it may be renamed). Fails with
/// [`ConfigError::Invalid`] for an empty or taken name.
pub fn save_user_scheme(
    dir: &Path,
    scheme: &ColorScheme,
    replaces: Option<&str>,
) -> Result<PathBuf, ConfigError> {
    let name = scheme.name.trim();
    if name.is_empty() {
        return Err(ConfigError::Invalid("Enter a name for the color scheme".into()));
    }
    if is_builtin(name) {
        return Err(ConfigError::Invalid(format!(
            "'{name}' is a built-in color scheme — choose another name"
        )));
    }
    let existing = user_schemes(dir);
    let find = |n: &str| existing.iter().find(|(s, _)| s.name == n).map(|(_, p)| p);
    if find(name).is_some() && Some(name) != replaces {
        return Err(ConfigError::Invalid(format!(
            "A color scheme named '{name}' already exists — choose another name"
        )));
    }
    // A renamed scheme's old file. Python deletes it first and then picks a
    // free file name; here its name counts as free, but it is only deleted
    // once the new file is written, so a failed write loses nothing.
    let old = replaces.filter(|r| *r != name).and_then(find);
    let path = match find(name) {
        Some(path) => path.clone(),
        None => {
            let slug = slug(name);
            let free = |p: &PathBuf| Some(p) == old || !p.exists();
            let mut path = dir.join(format!("{slug}.toml"));
            let mut n = 2;
            while !free(&path) {
                path = dir.join(format!("{slug}-{n}.toml"));
                n += 1;
            }
            path
        }
    };
    fs::create_dir_all(dir)?;
    fs::write(&path, scheme_to_toml(&scheme.renamed(name)))?;
    if let Some(old) = old.filter(|old| **old != path) {
        remove_if_exists(old)?;
    }
    Ok(path)
}

/// Delete the user scheme with this name (nothing if there is none).
pub fn delete_user_scheme(dir: &Path, name: &str) -> io::Result<()> {
    match user_schemes(dir).into_iter().find(|(s, _)| s.name == name) {
        Some((_, path)) => remove_if_exists(&path),
        None => Ok(()),
    }
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// A file name for a scheme: case-folded, runs of anything but a-z0-9 as "-".
fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut push = |c: char| {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    };
    for c in name.chars() {
        match casefold_special(c) {
            Some(folded) => folded.chars().for_each(&mut push),
            None => c.to_lowercase().for_each(&mut push),
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "scheme".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Where Python's `str.casefold()` differs from lower-casing in a way a slug
/// can see (the result contains ASCII letters): "Straße" is "strasse.toml"
/// there, so it must be here too. Every other difference only involves
/// non-ASCII letters, which become "-" either way.
fn casefold_special(c: char) -> Option<&'static str> {
    Some(match c {
        'ß' | 'ẞ' => "ss",
        'ſ' => "s",
        'ŉ' => "\u{2bc}n",
        'ǰ' => "j\u{30c}",
        'ẖ' => "h\u{331}",
        'ẗ' => "t\u{308}",
        'ẘ' => "w\u{30a}",
        'ẙ' => "y\u{30a}",
        'ẚ' => "a\u{2be}",
        'ﬀ' => "ff",
        'ﬁ' => "fi",
        'ﬂ' => "fl",
        'ﬃ' => "ffi",
        'ﬄ' => "ffl",
        'ﬅ' | 'ﬆ' => "st",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// The whole appearance
// ---------------------------------------------------------------------------

/// Frame styles (setting value, label), in menu order.
pub const FRAME_STYLES: &[(&str, &str)] = &[
    ("rounded", "Rounded"),
    ("single", "Single line"),
    ("double", "Double line"),
    ("heavy", "Heavy"),
];
pub const DEFAULT_FRAME_STYLE: &str = "rounded";
pub const DEFAULT_FONT_SIZE: i64 = 13;
pub const FONT_SIZE_RANGE: (i64, i64) = (7, 40);
pub const DEFAULT_LINE_SPACING: f64 = 1.0;
pub const LINE_SPACING_RANGE: (f64, f64) = (0.8, 2.0);

pub fn is_frame_style(name: &str) -> bool {
    FRAME_STYLES.iter().any(|(k, _)| *k == name)
}

/// What the user chose; [`Settings::appearance`](crate::Settings::appearance) fills in the defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct Appearance {
    pub color_scheme: String,
    /// Use `light_scheme` / `dark_scheme` by the OS mode.
    pub follow_system_appearance: bool,
    pub light_scheme: String,
    pub dark_scheme: String,
    /// Empty: the first installed of a list of good monospace fonts.
    pub font_family: String,
    /// E.g. "Light", "Medium"; empty: regular.
    pub font_style: String,
    pub font_size: i64,
    /// File list row height, as a multiple of the font's.
    pub line_spacing: f64,
    pub frame_style: String,
}

impl Default for Appearance {
    fn default() -> Self {
        Appearance {
            color_scheme: DEFAULT_SCHEME.into(),
            follow_system_appearance: false,
            light_scheme: DEFAULT_LIGHT_SCHEME.into(),
            dark_scheme: DEFAULT_SCHEME.into(),
            font_family: String::new(),
            font_style: String::new(),
            font_size: DEFAULT_FONT_SIZE,
            line_spacing: DEFAULT_LINE_SPACING,
            frame_style: DEFAULT_FRAME_STYLE.into(),
        }
    }
}

impl Appearance {
    /// The name of the setting that holds the scheme in use.
    pub fn scheme_setting(&self, system_dark: bool) -> &'static str {
        if !self.follow_system_appearance {
            "color_scheme"
        } else if system_dark {
            "dark_scheme"
        } else {
            "light_scheme"
        }
    }

    /// The name of the scheme in use.
    pub fn scheme_name(&self, system_dark: bool) -> &str {
        match self.scheme_setting(system_dark) {
            "dark_scheme" => &self.dark_scheme,
            "light_scheme" => &self.light_scheme,
            _ => &self.color_scheme,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("color-schemes");
        (tmp, d)
    }

    #[test]
    fn builtins_in_order_with_exact_colors() {
        let names: Vec<_> = builtin_schemes().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "monokai",
                "dracula",
                "nord",
                "gruvbox",
                "tokyo-night",
                "commander",
                "solarized-light",
                "paper"
            ]
        );
        let nord = builtin_scheme("nord").unwrap();
        assert_eq!(
            (nord.background.as_str(), nord.surface.as_str()),
            ("#242933", "#2e3440")
        );
        assert_eq!(
            (nord.dir.as_str(), nord.footer_key.as_str()),
            ("#81a1c1", "#88c0d0")
        );
        let paper = builtin_scheme("paper").unwrap();
        assert!(!paper.dark);
        assert_eq!(paper.cursor_fg, "#ffffff");
        assert_eq!(paper.pygments_style, "default");
        assert_eq!(
            builtin_scheme("gruvbox").unwrap().pygments_style,
            "gruvbox-dark"
        );
        assert_eq!(builtin_scheme("commander").unwrap().marked, "#ffff55");
        assert_eq!(builtin_scheme("tokyo-night").unwrap().footer_bg, "#24283b");
        for s in builtin_schemes() {
            for (role, _, _) in COLOR_ROLES {
                assert!(is_color(s.color(role).unwrap()), "{} {role}", s.name);
            }
        }
    }

    #[test]
    fn every_color_has_an_editor_role() {
        let mut s = builtin_scheme("monokai").unwrap().clone();
        assert_eq!(COLOR_ROLES.len(), 21);
        for (role, _, _) in COLOR_ROLES {
            *s.color_mut(role).unwrap() = "#000000".into();
        }
        assert!(s.color("name").is_none() && s.color_mut("pygments_style").is_none());
        // All 21 color fields were reached by role names.
        assert_eq!(
            build(&("x", true, ["#000000"; 21], "monokai")).renamed("monokai"),
            s
        );
    }

    #[test]
    fn colors() {
        assert!(is_color("#a0B1c2"));
        assert!(
            !is_color("red")
                && !is_color("#12345")
                && !is_color("#1234567")
                && !is_color("#12345g")
        );
    }

    #[test]
    fn toml_round_trip_and_format() {
        let scheme = get_scheme(Path::new("/nonexistent"), "nord").renamed("My \"Nord\" é");
        let text = scheme_to_toml(&scheme);
        assert!(text.starts_with(
            "# My \"Nord\" é: an Orchard Commander color scheme\nname = \"My \\\"Nord\\\" é\"\n"
        ));
        assert!(text.contains(
            "dark = true\npygments_style = \"nord\"  # syntax highlighting in text previews\n\n"
        ));
        assert!(text.contains("\nbackground         = \"#242933\"  # Window background\n"));
        assert!(text.ends_with("footer_key         = \"#88c0d0\"  # Key bar keys\n"));
        let table: toml::Table = text.parse().unwrap();
        assert_eq!(scheme_from_table(&table, ""), scheme);
    }

    #[test]
    fn user_scheme_round_trip() {
        let (_tmp, dir) = dir();
        let mut scheme = get_scheme(&dir, "nord").renamed("My Nord");
        scheme.dir = "#123456".into();
        scheme.pygments_style = "monokai".into();
        let path = save_user_scheme(&dir, &scheme, None).unwrap();
        assert_eq!(path, dir.join("my-nord.toml"));
        assert_eq!(get_scheme(&dir, "My Nord"), scheme);
        let all = all_schemes(&dir);
        assert_eq!(&all[..builtin_schemes().len()], builtin_schemes());
        assert_eq!(all.len(), builtin_schemes().len() + 1);
    }

    #[test]
    fn user_scheme_files_tolerate_missing_and_bad_values() {
        let (_tmp, dir) = dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("partial.toml"),
            "name = \"Partial\"\ndark = false\ndir = \"#00FF00\"\nerror = \"red\"\n",
        )
        .unwrap();
        fs::write(dir.join("broken.toml"), "name = [unclosed").unwrap();
        fs::write(dir.join("builtin.toml"), "name = \"nord\"\n").unwrap(); // can't shadow a built-in
        fs::write(dir.join("nameless.toml"), "dark = true\n").unwrap(); // named after the file
        fs::write(dir.join("ignored.txt"), "name = \"Text\"\n").unwrap();
        let partial = get_scheme(&dir, "Partial");
        let base = builtin_scheme(DEFAULT_LIGHT_SCHEME).unwrap();
        assert_eq!(partial.dir, "#00ff00");
        assert!(!partial.dark);
        assert_eq!(partial.error, base.error);
        assert_eq!(partial.surface, base.surface);
        assert_eq!(partial.pygments_style, base.pygments_style);
        assert_eq!(&get_scheme(&dir, "nord"), builtin_scheme("nord").unwrap());
        let names: Vec<_> = all_schemes(&dir).into_iter().map(|s| s.name).collect();
        assert_eq!(&names[8..], ["nameless", "Partial"]); // file name order
        assert_eq!(get_scheme(&dir, "nope").name, DEFAULT_SCHEME);
    }

    #[test]
    fn duplicate_names_first_file_wins() {
        let (_tmp, dir) = dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.toml"), "name = \"Same\"\ndir = \"#111111\"\n").unwrap();
        fs::write(
            dir.join("b.toml"),
            "name = \"  Same \"\ndir = \"#222222\"\n",
        )
        .unwrap();
        assert_eq!(get_scheme(&dir, "Same").dir, "#111111");
        assert_eq!(all_schemes(&dir).len(), 9);
    }

    #[test]
    fn user_scheme_names_rename_and_delete() {
        let (_tmp, dir) = dir();
        let base = get_scheme(&dir, "monokai");
        let err = save_user_scheme(&dir, &base, None).unwrap_err();
        assert_eq!(err.to_string(), "'monokai' is a built-in color scheme — choose another name");
        let err = save_user_scheme(&dir, &base.renamed("  "), None).unwrap_err();
        assert_eq!(err.to_string(), "Enter a name for the color scheme");
        save_user_scheme(&dir, &base.renamed("One"), None).unwrap();
        save_user_scheme(&dir, &base.renamed("Two"), None).unwrap();
        let err = save_user_scheme(&dir, &base.renamed("Two"), Some("One")).unwrap_err();
        assert_eq!(err.to_string(), "A color scheme named 'Two' already exists — choose another name");
        save_user_scheme(&dir, &base.renamed("Uno"), Some("One")).unwrap();
        let names: Vec<_> = all_schemes(&dir).into_iter().map(|s| s.name).collect();
        assert!(
            names.contains(&"Uno".into())
                && names.contains(&"Two".into())
                && !names.contains(&"One".into())
        );
        delete_user_scheme(&dir, "Uno").unwrap();
        delete_user_scheme(&dir, "missing").unwrap();
        assert!(!all_schemes(&dir).iter().any(|s| s.name == "Uno"));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn saving_in_place_keeps_the_file_and_slugs_dont_collide() {
        let (_tmp, dir) = dir();
        let base = get_scheme(&dir, "monokai");
        let first = save_user_scheme(&dir, &base.renamed("My Scheme"), None).unwrap();
        let mut edited = base.renamed(" My Scheme ");
        edited.dir = "#abcdef".into();
        assert_eq!(
            save_user_scheme(&dir, &edited, Some("My Scheme")).unwrap(),
            first
        );
        assert_eq!(get_scheme(&dir, "My Scheme").dir, "#abcdef");
        let second = save_user_scheme(&dir, &base.renamed("my-scheme"), None).unwrap();
        assert_eq!(second, dir.join("my-scheme-2.toml"));
        assert_eq!(
            save_user_scheme(&dir, &base.renamed("äö"), None).unwrap(),
            dir.join("scheme.toml")
        );
    }

    #[test]
    fn renaming_reuses_or_replaces_the_old_file() {
        let (_tmp, dir) = dir();
        let base = get_scheme(&dir, "monokai");
        let first = save_user_scheme(&dir, &base.renamed("My Scheme"), None).unwrap();
        // Same slug: Python deletes the old file and writes the same name again.
        let again = save_user_scheme(&dir, &base.renamed("my scheme"), Some("My Scheme")).unwrap();
        assert_eq!(again, first);
        assert_eq!(get_scheme(&dir, "my scheme").name, "my scheme");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        // A new slug: the new file is written, then the old one goes.
        let renamed = save_user_scheme(&dir, &base.renamed("Other"), Some("my scheme")).unwrap();
        assert_eq!(renamed, dir.join("other.toml"));
        assert!(!first.exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        // `replaces` naming a missing scheme is just a new scheme.
        save_user_scheme(&dir, &base.renamed("Third"), Some("gone")).unwrap();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("My Nord"), "my-nord");
        assert_eq!(slug("--A  b!!c--"), "a-b-c");
        assert_eq!(slug("nord (custom)"), "nord-custom");
        assert_eq!(slug("ÄÖÜ"), "scheme");
        // Python's casefold(), checked against CPython.
        assert_eq!(slug("Straße"), "strasse");
        assert_eq!(slug("ﬁne"), "fine");
        assert_eq!(slug("ẖ x"), "h-x");
        assert_eq!(slug("ſun"), "sun");
        assert_eq!(slug("İstanbul"), "i-stanbul");
        assert_eq!(slug("\u{212a}"), "k"); // Kelvin sign
    }

    #[test]
    fn appearance_scheme_choice() {
        let mut a = Appearance {
            color_scheme: "nord".into(),
            ..Appearance::default()
        };
        assert_eq!(a.scheme_name(false), "nord"); // one scheme for both modes
        assert_eq!(a.scheme_setting(true), "color_scheme");
        a.follow_system_appearance = true;
        a.light_scheme = "paper".into();
        a.dark_scheme = "dracula".into();
        assert_eq!(a.scheme_name(false), "paper");
        assert_eq!(a.scheme_name(true), "dracula");
        assert_eq!(a.scheme_setting(true), "dark_scheme");
        assert_eq!(a.scheme_setting(false), "light_scheme");
    }
}
