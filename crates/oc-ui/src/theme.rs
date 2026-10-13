//! The look: a color scheme resolved to grid colors, the frame style and the
//! font (port of `ui/style.py`'s `ThemeManager`, minus Qt).

use std::path::Path;

use oc_config::{Appearance, ColorScheme};
use oc_grid::{BoxStyle, FontConfig, Rgba, Style};

/// Every color role of a [`ColorScheme`], parsed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Colors {
    pub background: Rgba,
    pub surface: Rgba,
    pub foreground: Rgba,
    pub muted: Rgba,
    pub border: Rgba,
    pub border_active: Rgba,
    pub accent: Rgba,
    pub cursor_bg: Rgba,
    pub cursor_fg: Rgba,
    pub cursor_inactive_bg: Rgba,
    pub dir: Rgba,
    pub exec: Rgba,
    pub link: Rgba,
    pub hidden: Rgba,
    pub marked: Rgba,
    pub error: Rgba,
    pub warning: Rgba,
    pub success: Rgba,
    pub footer_bg: Rgba,
    pub footer_fg: Rgba,
    pub footer_key: Rgba,
}

impl Colors {
    pub fn from_scheme(s: &ColorScheme) -> Colors {
        let c = |v: &str, fallback: Rgba| Rgba::parse(v).unwrap_or(fallback);
        let fg = c(&s.foreground, Rgba::WHITE);
        let bg = c(&s.background, Rgba::BLACK);
        Colors {
            background: bg,
            surface: c(&s.surface, bg),
            foreground: fg,
            muted: c(&s.muted, fg),
            border: c(&s.border, fg),
            border_active: c(&s.border_active, fg),
            accent: c(&s.accent, fg),
            cursor_bg: c(&s.cursor_bg, fg),
            cursor_fg: c(&s.cursor_fg, bg),
            cursor_inactive_bg: c(&s.cursor_inactive_bg, bg),
            dir: c(&s.dir, fg),
            exec: c(&s.exec, fg),
            link: c(&s.link, fg),
            hidden: c(&s.hidden, fg),
            marked: c(&s.marked, fg),
            error: c(&s.error, fg),
            warning: c(&s.warning, fg),
            success: c(&s.success, fg),
            footer_bg: c(&s.footer_bg, bg),
            footer_fg: c(&s.footer_fg, fg),
            footer_key: c(&s.footer_key, fg),
        }
    }
}

/// Every color scheme (built-ins, then the user's `color-schemes/*.toml`),
/// read once in the background and kept in the app's state store, so a
/// theme change (zoom steps, the system going dark) never reads the disk on
/// the UI thread. Refreshed when the scheme editor saves or deletes, and
/// when the color scheme picker opens.
#[derive(Clone, Debug, Default)]
pub struct SchemeCache {
    /// `None` until the folder has been read.
    pub all: Option<Vec<ColorScheme>>,
}

impl SchemeCache {
    /// The scheme called `name`: a built-in, a cached user scheme, or the
    /// default. `None` only when `name` is a user scheme and the folder
    /// hasn't been read yet.
    pub fn get(&self, name: &str) -> Option<ColorScheme> {
        if let Some(s) = oc_config::themes::builtin_scheme(name) {
            return Some(s.clone());
        }
        let all = self.all.as_ref()?;
        Some(all.iter().find(|s| s.name == name).cloned().unwrap_or_else(default_scheme))
    }
}

/// The default scheme (monokai).
pub fn default_scheme() -> ColorScheme {
    oc_config::themes::builtin_scheme(oc_config::themes::DEFAULT_SCHEME).cloned().expect("default scheme is built in")
}

/// Everything drawing needs to know about the look.
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub scheme: ColorScheme,
    pub colors: Colors,
    pub frame: BoxStyle,
    pub font: FontConfig,
    pub appearance: Appearance,
}

impl Theme {
    /// The theme for `appearance`; user schemes are looked up in `schemes_dir`.
    pub fn new(appearance: &Appearance, schemes_dir: &Path, system_dark: bool) -> Theme {
        let scheme = oc_config::themes::get_scheme(schemes_dir, appearance.scheme_name(system_dark));
        Theme::with_scheme(appearance, scheme)
    }

    pub fn with_scheme(appearance: &Appearance, scheme: ColorScheme) -> Theme {
        let font = FontConfig {
            family: appearance.font_family.clone(),
            style: appearance.font_style.clone(),
            size: appearance.font_size as f32,
            line_spacing: appearance.line_spacing as f32,
            fallback: Vec::new(),
        }
        .sanitized();
        Theme {
            colors: Colors::from_scheme(&scheme),
            scheme,
            frame: BoxStyle::from_name(&appearance.frame_style).unwrap_or_default(),
            font,
            appearance: appearance.clone(),
        }
    }

    /// Built-in default (monokai, default font): tests and fallbacks.
    pub fn default_dark() -> Theme {
        let a = Appearance::default();
        let scheme = oc_config::themes::builtin_scheme(&a.color_scheme).cloned().expect("default scheme is built in");
        Theme::with_scheme(&a, scheme)
    }

    pub fn style(&self, fg: Rgba, bg: Rgba) -> Style {
        Style::new(fg, bg)
    }

    /// Text on the panel surface.
    pub fn text(&self) -> Style {
        Style::new(self.colors.foreground, self.colors.surface)
    }

    pub fn muted(&self) -> Style {
        Style::new(self.colors.muted, self.colors.surface)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appearance_maps_to_grid_settings() {
        let a = Appearance { frame_style: "double".into(), font_size: 15, color_scheme: "gruvbox".into(), ..Appearance::default() };
        let tmp = tempfile::tempdir().unwrap();
        let t = Theme::new(&a, tmp.path(), true);
        assert_eq!(t.frame, BoxStyle::Double);
        assert_eq!(t.font.size, 15.0);
        assert_eq!(t.colors.surface, Rgba::hex(0x282828));
        assert_eq!(t.scheme.name, "gruvbox");
    }
}
