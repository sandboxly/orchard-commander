//! Persistent user settings, stored as JSON (`settings.json`) in the config directory.
//!
//! Empty strings and `None` mean "not chosen": the default applies and can
//! change in a later version without overwriting anyone's choice.

use std::fs;
use std::io;
use std::path::Path;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::pyjson;
use crate::themes::{self, Appearance};

/// Hidden files are shown unless the user toggled them off.
pub const DEFAULT_SHOW_HIDDEN: bool = true;

/// The settings file's contents. Field order is the file's key order.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Settings {
    // Appearance; empty / None: the default in `Appearance`.
    pub color_scheme: String,
    pub follow_system_appearance: Option<bool>,
    pub light_scheme: String,
    pub dark_scheme: String,
    pub font_family: String,
    pub font_style: String,
    pub font_size: Option<i64>,
    pub line_spacing: Option<f64>,
    pub frame_style: String,
    /// In-window menu bar always shown (otherwise F9 shows it).
    pub menu_bar_visible: Option<bool>,

    /// None: [`DEFAULT_SHOW_HIDDEN`] (only a toggle stores a choice).
    pub show_hidden: Option<bool>,
    /// Previews: play the next audio/video file when one ends (None: on).
    pub play_next: Option<bool>,
    /// The terminal panel's shell command; "": the user's login shell / PowerShell.
    pub terminal_shell: String,
    /// A side's shell and panel follow each other's folder (None: on).
    pub terminal_link: Option<bool>,
    pub left_path: String,
    pub right_path: String,
    /// Everything else (window geometry, panel modes, column widths, …), by key.
    pub extra: Map<String, Value>,
}

impl Settings {
    /// Audio/video previews start playing when selected (the player's own toggle, shared).
    pub fn autoplay_enabled(&self) -> bool {
        self.extra
            .get("preview.media.autoplay")
            .is_none_or(pyjson::truthy)
    }

    pub fn terminal_link_enabled(&self) -> bool {
        self.terminal_link.unwrap_or(true)
    }

    pub fn play_next_enabled(&self) -> bool {
        self.play_next.unwrap_or(true)
    }

    pub fn hidden_visible(&self) -> bool {
        self.show_hidden.unwrap_or(DEFAULT_SHOW_HIDDEN)
    }

    /// The chosen appearance, with defaults for whatever wasn't chosen.
    pub fn appearance(&self) -> Appearance {
        let default = Appearance::default();
        let or_default = |value: &str, default: String| {
            if value.is_empty() {
                default
            } else {
                value.to_owned()
            }
        };
        let (lo, hi) = themes::FONT_SIZE_RANGE;
        let (spacing_lo, spacing_hi) = themes::LINE_SPACING_RANGE;
        Appearance {
            color_scheme: or_default(&self.color_scheme, default.color_scheme),
            follow_system_appearance: self.follow_system_appearance.unwrap_or(false),
            light_scheme: or_default(&self.light_scheme, default.light_scheme),
            dark_scheme: or_default(&self.dark_scheme, default.dark_scheme),
            font_family: self.font_family.clone(),
            font_style: self.font_style.clone(),
            font_size: self
                .font_size
                .map_or(default.font_size, |s| s.clamp(lo, hi)),
            line_spacing: self
                .line_spacing
                .map_or(default.line_spacing, |s| s.min(spacing_hi).max(spacing_lo)),
            frame_style: if themes::is_frame_style(&self.frame_style) {
                self.frame_style.clone()
            } else {
                default.frame_style
            },
        }
    }

    /// Read `path`. A missing or damaged file gives the defaults; unknown keys
    /// and values of the wrong type are ignored.
    pub fn load(path: &Path) -> Settings {
        let Ok(text) = fs::read_to_string(path) else {
            return Settings::default();
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(data)) => Settings::from_map(data),
            _ => Settings::default(),
        }
    }

    /// Settings from the file's parsed JSON object, migrating older keys.
    pub fn from_map(mut data: Map<String, Value>) -> Settings {
        // Before appearance settings: "theme" was the color scheme, the font size an extra.
        if let Some(theme) = data.shift_remove("theme") {
            data.entry("color_scheme").or_insert(theme);
        }
        if let Some(Value::Object(extra)) = data.get_mut("extra")
            && let Some(size) = extra.shift_remove("font_size")
        {
            data.entry("font_size").or_insert(size);
        }

        let mut s = Settings::default();
        for (key, value) in data {
            match key.as_str() {
                "color_scheme" => set_str(&mut s.color_scheme, value),
                "follow_system_appearance" => s.follow_system_appearance = opt_bool(&value),
                "light_scheme" => set_str(&mut s.light_scheme, value),
                "dark_scheme" => set_str(&mut s.dark_scheme, value),
                "font_family" => set_str(&mut s.font_family, value),
                "font_style" => set_str(&mut s.font_style, value),
                // Python's int() truncates a float size.
                "font_size" => {
                    s.font_size = value.as_i64().or_else(|| value.as_f64().map(|f| f as i64))
                }
                "line_spacing" => s.line_spacing = value.as_f64(),
                "frame_style" => set_str(&mut s.frame_style, value),
                "menu_bar_visible" => s.menu_bar_visible = opt_bool(&value),
                "show_hidden" => s.show_hidden = opt_bool(&value),
                "play_next" => s.play_next = opt_bool(&value),
                "terminal_shell" => set_str(&mut s.terminal_shell, value),
                "terminal_link" => s.terminal_link = opt_bool(&value),
                "left_path" => set_str(&mut s.left_path, value),
                "right_path" => set_str(&mut s.right_path, value),
                "extra" => {
                    if let Value::Object(extra) = value {
                        s.extra = extra;
                    }
                }
                _ => {}
            }
        }
        s
    }

    /// The file's text: `json.dumps(asdict(settings), indent=2)`.
    pub fn to_json(&self) -> String {
        pyjson::dumps(self, true)
    }

    /// Write all settings to `path`. Settings are a convenience: callers
    /// usually log and otherwise ignore an error.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        crate::write_atomic(path, self.to_json().as_bytes())
    }

    /// Apply `change` and persist only what it changes.
    ///
    /// The file is re-read first and `change` is applied to that too, so
    /// several running instances don't overwrite each other's unrelated
    /// settings with stale values. `change` therefore runs twice.
    pub fn update(&mut self, path: &Path, change: impl Fn(&mut Settings)) -> io::Result<()> {
        change(self);
        let mut on_disk = Settings::load(path);
        change(&mut on_disk);
        on_disk.save(path)
    }
}

fn set_str(field: &mut String, value: Value) {
    if let Value::String(s) = value {
        *field = s;
    }
}

fn opt_bool(value: &Value) -> Option<bool> {
    value.as_bool()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cfg").join("settings.json");
        (tmp, path)
    }

    #[test]
    fn missing_or_damaged_file_gives_defaults() {
        let (_tmp, path) = file();
        assert_eq!(Settings::load(&path), Settings::default());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not json").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
        fs::write(&path, "[1, 2]").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
    }

    #[test]
    fn file_format_matches_python() {
        let s = Settings {
            font_size: Some(15),
            line_spacing: Some(1.2),
            show_hidden: Some(true),
            ..Default::default()
        };
        let expected = r#"{
  "color_scheme": "",
  "follow_system_appearance": null,
  "light_scheme": "",
  "dark_scheme": "",
  "font_family": "",
  "font_style": "",
  "font_size": 15,
  "line_spacing": 1.2,
  "frame_style": "",
  "menu_bar_visible": null,
  "show_hidden": true,
  "play_next": null,
  "terminal_shell": "",
  "terminal_link": null,
  "left_path": "",
  "right_path": "",
  "extra": {}
}"#;
        assert_eq!(s.to_json(), expected);
    }

    #[test]
    fn round_trip_keeps_extra_order_and_unicode() {
        let (_tmp, path) = file();
        let mut s = Settings {
            left_path: "/home/jörg".into(),
            ..Default::default()
        };
        s.extra.insert("zeta".into(), json!(1));
        s.extra.insert("alpha".into(), json!({"nested": [1, "ü"]}));
        s.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"left_path\": \"/home/j\\u00f6rg\"")); // ensure_ascii like Python
        let loaded = Settings::load(&path);
        assert_eq!(loaded, s);
        assert_eq!(loaded.extra.keys().collect::<Vec<_>>(), ["zeta", "alpha"]);
    }

    #[test]
    fn old_theme_and_font_size_settings_are_migrated() {
        let (_tmp, path) = file();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"theme": "nord", "extra": {"font_size": 15, "geometry": "x"}}"#,
        )
        .unwrap();
        let mut s = Settings::load(&path);
        assert_eq!(s.color_scheme, "nord");
        assert_eq!(s.font_size, Some(15));
        assert_eq!(Value::Object(s.extra.clone()), json!({"geometry": "x"}));
        s.update(&path, |s| s.frame_style = "double".into())
            .unwrap();
        let on_disk: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(on_disk.get("theme").is_none());
        assert_eq!(on_disk["color_scheme"], "nord");
        assert_eq!(on_disk["frame_style"], "double");
    }

    #[test]
    fn theme_does_not_override_a_chosen_scheme() {
        let s = Settings::from_map(
            json!({"theme": "nord", "color_scheme": "paper", "font_size": 9, "extra": {"font_size": 20}})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert_eq!(s.color_scheme, "paper");
        assert_eq!(s.font_size, Some(9));
        assert!(s.extra.is_empty());
    }

    #[test]
    fn wrong_types_and_unknown_keys_are_ignored() {
        let s = Settings::from_map(
            json!({"color_scheme": 5, "show_hidden": "yes", "font_size": 14.7, "line_spacing": 2,
                   "extra": [1], "bogus": true, "play_next": false})
            .as_object()
            .unwrap()
            .clone(),
        );
        assert_eq!(s.color_scheme, "");
        assert_eq!(s.show_hidden, None);
        assert_eq!(s.font_size, Some(14));
        assert_eq!(s.line_spacing, Some(2.0));
        assert!(s.extra.is_empty());
        assert_eq!(s.play_next, Some(false));
        assert!(!s.play_next_enabled());
    }

    #[test]
    fn update_writes_only_changed_keys() {
        let (_tmp, path) = file();
        let mut mine = Settings::load(&path);
        // Another instance saves its own choice meanwhile.
        let mut other = Settings::load(&path);
        other
            .update(&path, |s| s.color_scheme = "dracula".into())
            .unwrap();
        mine.update(&path, |s| {
            s.left_path = "/a".into();
            s.extra.insert("left_mode".into(), json!("files"));
        })
        .unwrap();
        assert_eq!(mine.color_scheme, ""); // not reloaded
        assert_eq!(mine.left_path, "/a");
        let on_disk = Settings::load(&path);
        assert_eq!(on_disk.color_scheme, "dracula"); // not overwritten with a stale value
        assert_eq!(on_disk.left_path, "/a");
        assert_eq!(on_disk.extra["left_mode"], "files");
    }

    #[test]
    fn defaults_and_toggles() {
        let mut s = Settings::default();
        assert!(
            s.hidden_visible()
                && s.terminal_link_enabled()
                && s.play_next_enabled()
                && s.autoplay_enabled()
        );
        s.show_hidden = Some(false);
        s.terminal_link = Some(false);
        s.extra
            .insert("preview.media.autoplay".into(), json!(false));
        assert!(!s.hidden_visible() && !s.terminal_link_enabled() && !s.autoplay_enabled());
        s.extra.insert("preview.media.autoplay".into(), json!(0));
        assert!(!s.autoplay_enabled());
    }

    #[test]
    fn appearance_defaults_and_limits() {
        assert_eq!(Settings::default().appearance(), Appearance::default());
        let a = Settings {
            font_size: Some(99),
            line_spacing: Some(0.1),
            frame_style: "bogus".into(),
            color_scheme: "nord".into(),
            ..Default::default()
        }
        .appearance();
        assert_eq!(a.font_size, themes::FONT_SIZE_RANGE.1);
        assert_eq!(a.line_spacing, themes::LINE_SPACING_RANGE.0);
        assert_eq!(a.frame_style, themes::DEFAULT_FRAME_STYLE);
        assert_eq!(a.scheme_name(false), "nord");
        let a = Settings {
            font_size: Some(1),
            line_spacing: Some(9.0),
            frame_style: "heavy".into(),
            ..Default::default()
        }
        .appearance();
        assert_eq!(
            (a.font_size, a.line_spacing, a.frame_style.as_str()),
            (7, 2.0, "heavy")
        );
    }
}
