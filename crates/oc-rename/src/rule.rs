//! What to rename and how: [`Rule`] (the dialog's settings) and [`Item`]
//! (one file, with just what the rule needs).

use std::sync::Arc;

use oc_vfs::{Entry, VPath};

use crate::metadata::Metadata;

/// Whole-name case conversion, applied last.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Case {
    #[default]
    Unchanged,
    Lower,
    Upper,
    /// "First letter uppercase": the first character up, the rest down.
    First,
    /// "Every Word Uppercase".
    Words,
}

/// All cases in the dialog's order, by their settings name.
pub const CASES: [&str; 5] = ["unchanged", "lower", "upper", "first", "words"];

impl Case {
    pub fn as_str(self) -> &'static str {
        match self {
            Case::Unchanged => "unchanged",
            Case::Lower => "lower",
            Case::Upper => "upper",
            Case::First => "first",
            Case::Words => "words",
        }
    }

    pub fn parse(s: &str) -> Option<Case> {
        Some(match s {
            "unchanged" => Case::Unchanged,
            "lower" => Case::Lower,
            "upper" => Case::Upper,
            "first" => Case::First,
            "words" => Case::Words,
            _ => return None,
        })
    }
}

/// The Multi-Rename settings. `Default` is the dialog's start state, which
/// renames nothing.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Rule {
    pub name_mask: String,
    pub ext_mask: String,
    /// Several searches separated by "|".
    pub search: String,
    /// One replacement for all searches, or one per search separated by "|".
    pub replace: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub case: Case,
    /// Defaults for `[C]`; the placeholder can override each.
    pub counter_start: i64,
    pub counter_step: i64,
    pub counter_digits: i64,
}

impl Default for Rule {
    fn default() -> Self {
        Rule {
            name_mask: "[N]".into(),
            ext_mask: "[E]".into(),
            search: String::new(),
            replace: String::new(),
            regex: false,
            case_sensitive: false,
            case: Case::Unchanged,
            counter_start: 1,
            counter_step: 1,
            counter_digits: 1,
        }
    }
}

/// One file or folder to rename (only what the rule needs).
#[derive(Clone, Debug)]
pub struct Item {
    pub location: VPath,
    pub name: String,
    pub is_dir: bool,
    /// Seconds since the epoch; 0.0 means unknown (dates then show "now").
    pub mtime: f64,
    /// The parent folder's name, for `[P]`.
    pub parent: String,
    /// The grandparent folder's name, for `[G]`.
    pub grandparent: String,
    /// File metadata for `[=field]` placeholders: `None` until read (see
    /// [`crate::metadata::read`]). Shared, because plans copy items.
    pub meta: Option<Arc<Metadata>>,
    /// Why metadata couldn't be read; shown instead of "no artist".
    pub meta_error: String,
    /// A cloud placeholder: reading metadata would download it.
    pub online_only: bool,
}

impl Item {
    /// An item without folder names or metadata (e.g. for undo).
    pub fn new(location: VPath, name: impl Into<String>, is_dir: bool, mtime: f64) -> Self {
        Item {
            location,
            name: name.into(),
            is_dir,
            mtime,
            parent: String::new(),
            grandparent: String::new(),
            meta: None,
            meta_error: String::new(),
            online_only: false,
        }
    }

    /// The same item with metadata attached (what the dialog's reader thread produces).
    pub fn with_meta(mut self, meta: Metadata) -> Self {
        self.meta = Some(Arc::new(meta));
        self
    }
}

/// The item for `location`, whose listing entry is `entry`.
pub fn item_for(location: &VPath, entry: &Entry) -> Item {
    let parent = location.parent();
    let grand = parent.as_ref().and_then(VPath::parent);
    // location.name(), not entry.name: search results list entries by their relative path.
    Item {
        parent: parent.as_ref().map(VPath::name).unwrap_or_default(),
        grandparent: grand.as_ref().map(VPath::name).unwrap_or_default(),
        online_only: entry.is_online_only,
        ..Item::new(location.clone(), location.name(), entry.is_dir, entry.mtime)
    }
}

/// `("report.final", "pdf")` for files; folders and dot-files have no extension.
pub fn split_name(name: &str, is_dir: bool) -> (&str, &str) {
    if is_dir {
        return (name, "");
    }
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split() {
        assert_eq!(split_name("a.tar.gz", false), ("a.tar", "gz"));
        assert_eq!(split_name(".bashrc", false), (".bashrc", ""));
        assert_eq!(split_name("..x", false), (".", "x"));
        assert_eq!(split_name("trailing.", false), ("trailing", ""));
        assert_eq!(split_name("v1.2", true), ("v1.2", ""));
        assert_eq!(split_name("plain", false), ("plain", ""));
    }

    #[test]
    fn cases_round_trip() {
        for name in CASES {
            assert_eq!(Case::parse(name).unwrap().as_str(), name);
        }
        assert_eq!(Case::parse("Upper"), None);
    }
}
