//! The player's remembered settings, kept in `Settings.extra` under
//! `"preview.media.<key>"` exactly as 1.x stored them, so upgrades keep them.
//!
//! The player starts muted until it is unmuted once; autoplay is on until
//! turned off (with **A**, the button, or Settings → Preview). Both apply to
//! the next file and the next run.

use serde_json::{Map, Value};

/// The media previewer's id: the middle part of its `extra` keys.
pub const PREVIEWER_ID: &str = "media";
/// Key (below the previewer's prefix) of the remembered mute switch.
pub const MUTED: &str = "muted";
/// Key of the autoplay switch.
pub const AUTOPLAY: &str = "autoplay";

/// The full `Settings.extra` key: `"preview.media.muted"`.
pub fn extra_key(key: &str) -> String {
    format!("preview.{PREVIEWER_ID}.{key}")
}

/// Python's truthiness of a stored value (the 1.x store read `bool(value)`).
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaPrefs {
    pub muted: bool,
    pub autoplay: bool,
}

impl Default for MediaPrefs {
    /// Nothing stored yet: muted, autoplay on.
    fn default() -> Self {
        MediaPrefs { muted: true, autoplay: true }
    }
}

impl MediaPrefs {
    /// Read from `Settings.extra`; missing keys take the defaults.
    pub fn from_extra(extra: &Map<String, Value>) -> Self {
        let get = |key: &str, default: bool| extra.get(&extra_key(key)).map_or(default, truthy);
        let d = MediaPrefs::default();
        MediaPrefs { muted: get(MUTED, d.muted), autoplay: get(AUTOPLAY, d.autoplay) }
    }

    /// Store one switch in a copy of `extra`; None when it already has that
    /// value (1.x wrote only changes, as several app instances may run).
    /// Hand the result to `Settings::update` as the new `extra`.
    pub fn set(extra: &Map<String, Value>, key: &str, value: bool) -> Option<Map<String, Value>> {
        let full = extra_key(key);
        if extra.get(&full) == Some(&Value::Bool(value)) {
            return None;
        }
        let mut out = extra.clone();
        out.insert(full, Value::Bool(value));
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(v: Value) -> Map<String, Value> {
        let Value::Object(m) = v else { panic!() };
        m
    }

    #[test]
    fn defaults_to_muted_with_autoplay() {
        assert_eq!(MediaPrefs::from_extra(&Map::new()), MediaPrefs { muted: true, autoplay: true });
    }

    #[test]
    fn reads_the_1x_keys() {
        let extra = map(json!({"preview.media.muted": false, "preview.media.autoplay": 0, "other": 1}));
        assert_eq!(MediaPrefs::from_extra(&extra), MediaPrefs { muted: false, autoplay: false });
        assert_eq!(extra_key(MUTED), "preview.media.muted");
    }

    #[test]
    fn set_writes_only_changes() {
        let extra = map(json!({"preview.media.muted": true}));
        assert!(MediaPrefs::set(&extra, MUTED, true).is_none());
        let changed = MediaPrefs::set(&extra, MUTED, false).unwrap();
        assert_eq!(changed["preview.media.muted"], json!(false));
        let added = MediaPrefs::set(&extra, AUTOPLAY, false).unwrap();
        assert_eq!(Value::Object(added), json!({"preview.media.muted": true, "preview.media.autoplay": false}));
    }
}
