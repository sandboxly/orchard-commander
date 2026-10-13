//! Location bookmarks (a directory hotlist).
//!
//! A bookmark has a title (an "&" marks its hotkey letter), a
//! location for the active panel and optionally one for the other panel.
//! Bookmarks can be grouped in folders (submenus) and
//! separated by lines. Locations are [`Spec`]s, so bookmarks can point into
//! archives and onto servers, not just local folders.
//!
//! Stored as JSON in the config directory (`bookmarks.json`).

use std::fs;
use std::io;
use std::path::Path;

use serde_json::{Map, Value, json};

use crate::locations::{Spec, SpecRef, same_location};
use crate::pyjson::{self, py_str, truthy};

pub const FORMAT_VERSION: i64 = 1;

/// A fresh item id: 32 hex digits, like Python's `uuid4().hex`.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Bookmark {
    pub title: String,
    pub location: Spec,
    /// Opened in the other panel at the same time.
    pub target: Option<Spec>,
    pub id: String,
}

impl Bookmark {
    pub fn new(title: impl Into<String>, location: Spec, target: Option<Spec>) -> Self {
        Bookmark {
            title: title.into(),
            location,
            target,
            id: new_id(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Folder {
    pub title: String,
    pub items: Vec<Item>,
    pub id: String,
}

impl Folder {
    pub fn new(title: impl Into<String>, items: Vec<Item>) -> Self {
        Folder {
            title: title.into(),
            items,
            id: new_id(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Bookmark(Bookmark),
    Folder(Folder),
    Separator { id: String },
}

impl Item {
    pub fn separator() -> Self {
        Item::Separator { id: new_id() }
    }

    pub fn id(&self) -> &str {
        match self {
            Item::Bookmark(b) => &b.id,
            Item::Folder(f) => &f.id,
            Item::Separator { id } => id,
        }
    }
}

impl From<Bookmark> for Item {
    fn from(b: Bookmark) -> Item {
        Item::Bookmark(b)
    }
}

impl From<Folder> for Item {
    fn from(f: Folder) -> Item {
        Item::Folder(f)
    }
}

// ---------------------------------------------------------------------------
// Titles and hotkeys ("&Projects": P is the hotkey, "&&" is a literal "&")
// ---------------------------------------------------------------------------

/// The hotkey letter of a title (lower case), if it has one.
pub fn hotkey(title: &str) -> Option<String> {
    let mut chars = title.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            match chars.next() {
                Some('&') => continue,
                Some(key) => return Some(key.to_lowercase().collect()),
                None => return None,
            }
        }
    }
    None
}

/// The title as shown: without hotkey markers.
pub fn plain(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut chars = title.chars();
    while let Some(c) = chars.next() {
        if c == '&' {
            match chars.next() {
                Some(next) => out.push(next),
                None => out.push('&'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tree helpers
// ---------------------------------------------------------------------------

/// Every item, depth first, with the (plain) folder titles leading to it.
pub fn walk(items: &[Item]) -> Vec<(Vec<String>, &Item)> {
    fn go<'a>(items: &'a [Item], trail: &mut Vec<String>, out: &mut Vec<(Vec<String>, &'a Item)>) {
        for item in items {
            out.push((trail.clone(), item));
            if let Item::Folder(f) = item {
                trail.push(plain(&f.title));
                go(&f.items, trail, out);
                trail.pop();
            }
        }
    }
    let mut out = Vec::new();
    go(items, &mut Vec::new(), &mut out);
    out
}

/// Every bookmark with its folder trail.
pub fn bookmarks(items: &[Item]) -> Vec<(Vec<String>, &Bookmark)> {
    walk(items)
        .into_iter()
        .filter_map(|(trail, item)| match item {
            Item::Bookmark(b) => Some((trail, b)),
            _ => None,
        })
        .collect()
}

/// Every folder with its folder trail.
pub fn folders(items: &[Item]) -> Vec<(Vec<String>, &Folder)> {
    walk(items)
        .into_iter()
        .filter_map(|(trail, item)| match item {
            Item::Folder(f) => Some((trail, f)),
            _ => None,
        })
        .collect()
}

/// The first bookmark (depth first) whose active-panel location is `spec`.
pub fn find_location<'a>(items: &'a [Item], spec: SpecRef<'_>) -> Option<&'a Bookmark> {
    items.iter().find_map(|item| match item {
        Item::Bookmark(b) if same_location(Some(b.location.as_ref()), Some(spec)) => Some(b),
        Item::Folder(f) => find_location(&f.items, spec),
        _ => None,
    })
}

/// The item (anywhere in the tree) with this id.
pub fn find<'a>(items: &'a [Item], id: &str) -> Option<&'a Item> {
    items.iter().find_map(|item| {
        if item.id() == id {
            return Some(item);
        }
        match item {
            Item::Folder(f) => find(&f.items, id),
            _ => None,
        }
    })
}

/// Remove the item (anywhere in the tree) with this id; returns it.
///
/// Depth first, like [`find`]: a hand-edited file may repeat an id, and the
/// item removed must be the one found.
pub fn remove(items: &mut Vec<Item>, id: &str) -> Option<Item> {
    for i in 0..items.len() {
        if items[i].id() == id {
            return Some(items.remove(i));
        }
        if let Item::Folder(f) = &mut items[i]
            && let Some(found) = remove(&mut f.items, id)
        {
            return Some(found);
        }
    }
    None
}

/// The item list of a folder (by id); the top level for `None` or unknown ids.
pub fn container_of<'a>(items: &'a mut Vec<Item>, folder_id: Option<&str>) -> &'a mut Vec<Item> {
    // Find the route first: returning a borrow from a failed search would
    // keep `items` borrowed for the fallback.
    fn route(items: &[Item], id: &str, path: &mut Vec<usize>) -> bool {
        for (i, item) in items.iter().enumerate() {
            if let Item::Folder(f) = item {
                path.push(i);
                if f.id == id || route(&f.items, id, path) {
                    return true;
                }
                path.pop();
            }
        }
        false
    }
    let mut path = Vec::new();
    if folder_id.is_none_or(|id| !route(items, id, &mut path)) {
        return items;
    }
    let mut list = items;
    for i in path {
        let Item::Folder(f) = &mut list[i] else {
            unreachable!("route only goes through folders")
        };
        list = &mut f.items;
    }
    list
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

fn to_json(item: &Item) -> Value {
    match item {
        Item::Bookmark(b) => {
            let mut m = Map::with_capacity(5);
            m.insert("type".into(), "bookmark".into());
            m.insert("id".into(), b.id.clone().into());
            m.insert("title".into(), b.title.clone().into());
            m.insert("location".into(), b.location.clone().into());
            if let Some(target) = &b.target {
                m.insert("target".into(), target.clone().into());
            }
            Value::Object(m)
        }
        Item::Folder(f) => json!({
            "type": "folder",
            "id": f.id,
            "title": f.title,
            "items": f.items.iter().map(to_json).collect::<Vec<_>>(),
        }),
        Item::Separator { id } => json!({"type": "separator", "id": id}),
    }
}

fn from_json(data: &Value) -> Option<Item> {
    let data = data.as_object()?;
    let id = match data.get("id") {
        Some(v) if truthy(v) => py_str(v),
        _ => new_id(),
    };
    let title = || data.get("title").map(py_str).unwrap_or_default();
    match data.get("type").and_then(Value::as_str)? {
        "bookmark" => {
            let location = Spec::from_value(data.get("location")?)?;
            let target = data.get("target").and_then(Spec::from_value);
            Some(Item::Bookmark(Bookmark {
                title: title(),
                location,
                target,
                id,
            }))
        }
        "folder" => Some(Item::Folder(Folder {
            title: title(),
            items: items_from(data.get("items")),
            id,
        })),
        "separator" => Some(Item::Separator { id }),
        _ => None,
    }
}

/// The items of a list; damaged entries are skipped.
fn items_from(list: Option<&Value>) -> Vec<Item> {
    match list {
        Some(Value::Array(a)) => a.iter().filter_map(from_json).collect(),
        _ => Vec::new(),
    }
}

/// Parse a bookmarks file's text; damaged files and entries are tolerated.
pub fn parse(text: &str) -> Vec<Item> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(data)) => items_from(data.get("items")),
        _ => Vec::new(),
    }
}

/// The file's text, as the Python version writes it.
pub fn to_text(items: &[Item]) -> String {
    let data =
        json!({"version": FORMAT_VERSION, "items": items.iter().map(to_json).collect::<Vec<_>>()});
    pyjson::dumps(&data, false)
}

/// The bookmarks in `path` (empty if there is no readable file).
pub fn load(path: &Path) -> Vec<Item> {
    fs::read_to_string(path)
        .map(|t| parse(&t))
        .unwrap_or_default()
}

/// Write the bookmarks to `path`, via a temporary file so a crash never
/// leaves a half-written file behind.
pub fn save(path: &Path, items: &[Item]) -> io::Result<()> {
    // Via `bookmarks.json.tmp`, like the Python version.
    crate::paths::write_atomic(path, to_text(items).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(p: &str) -> Spec {
        Spec::local(p)
    }

    #[test]
    fn hotkeys_and_titles() {
        assert_eq!(hotkey("&Projects").as_deref(), Some("p"));
        assert_eq!(hotkey("Sound && &Vision").as_deref(), Some("v"));
        assert_eq!(hotkey("No hotkey"), None);
        assert_eq!(hotkey("trailing &"), None);
        assert_eq!(hotkey("&&"), None);
        assert_eq!(hotkey("&Über").as_deref(), Some("ü"));
        assert_eq!(plain("Sound && &Vision"), "Sound & Vision");
        assert_eq!(plain("trailing &"), "trailing &");
        assert_eq!(plain("&&&a"), "&a");
    }

    #[test]
    fn tree_helpers() {
        let repo = Bookmark::new("&Repo", local("/src/repo"), None);
        let work = Folder::new("&Work", vec![repo.clone().into(), Item::separator()]);
        let work_id = work.id.clone();
        let mut items: Vec<Item> = vec![
            Bookmark::new("&Home", local("/home/me"), None).into(),
            work.into(),
        ];
        let found: Vec<_> = bookmarks(&items)
            .into_iter()
            .map(|(t, b)| (t, b.title.clone()))
            .collect();
        assert_eq!(
            found,
            [
                (vec![], "&Home".to_string()),
                (vec!["Work".to_string()], "&Repo".to_string())
            ]
        );
        assert_eq!(folders(&items).len(), 1);
        assert_eq!(walk(&items).len(), 4);
        assert_eq!(
            find_location(&items, local("/src/repo/").as_ref())
                .unwrap()
                .title,
            "&Repo"
        ); // normalised
        assert!(find_location(&items, local("/elsewhere").as_ref()).is_none());
        assert_eq!(find(&items, &repo.id).unwrap().id(), repo.id);
        assert_eq!(container_of(&mut items, Some(&work_id)).len(), 2);
        assert_eq!(container_of(&mut items, Some("unknown")).len(), 2);
        let first = items[0].id().to_owned();
        assert_eq!(container_of(&mut items, None)[0].id(), first);
        container_of(&mut items, Some(&work_id)).push(Item::separator());
        assert!(remove(&mut items, &repo.id).is_some());
        assert!(remove(&mut items, "nope").is_none());
        let Item::Folder(work) = &items[1] else {
            panic!()
        };
        assert_eq!(work.items.len(), 2);
    }

    #[test]
    fn nested_containers() {
        let inner = Folder::new("Inner", vec![]);
        let inner_id = inner.id.clone();
        let mut items: Vec<Item> = vec![
            Item::separator(),
            Folder::new("Outer", vec![inner.into()]).into(),
        ];
        container_of(&mut items, Some(&inner_id))
            .push(Bookmark::new("x", local("/x"), None).into());
        assert_eq!(bookmarks(&items)[0].0, ["Outer", "Inner"]);
        assert_eq!(
            remove(&mut items, &inner_id).map(|i| matches!(i, Item::Folder(_))),
            Some(true)
        );
        assert!(bookmarks(&items).is_empty());
    }

    #[test]
    fn duplicate_ids_remove_the_first_in_tree_order() {
        let dup = |title: &str| {
            Item::Bookmark(Bookmark {
                title: title.into(),
                location: local("/x"),
                target: None,
                id: "dup".into(),
            })
        };
        let mut items = vec![Folder::new("F", vec![dup("inner")]).into(), dup("outer")];
        let Some(Item::Bookmark(b)) = remove(&mut items, "dup") else {
            panic!()
        };
        assert_eq!(b.title, "inner"); // as Python's recursive remove()
        let Item::Bookmark(b) = find(&items, "dup").unwrap() else {
            panic!()
        };
        assert_eq!(b.title, "outer");
    }

    #[test]
    fn save_replaces_the_file_and_creates_the_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("new/dir/bookmarks.json");
        save(&path, &[Item::separator()]).unwrap();
        save(&path, &[]).unwrap();
        assert!(load(&path).is_empty());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\n  \"version\": 1,\n  \"items\": []\n}"
        );
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["bookmarks.json"]); // no temporary file left
    }

    #[test]
    fn save_and_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cfg/bookmarks.json");
        let target = local("/tmp/other");
        let items: Vec<Item> = vec![
            Bookmark::new("&Docs", local("/tmp/docs"), Some(target.clone())).into(),
            Folder::new(
                "&Servers",
                vec![
                    Bookmark::new("&Web", Spec::server("sftp://root@web", "/var/www"), None).into(),
                ],
            )
            .into(),
            Item::separator(),
        ];
        save(&path, &items).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded, items);
        let Item::Bookmark(docs) = &loaded[0] else {
            panic!()
        };
        assert_eq!(docs.target.as_ref(), Some(&target));
        let text = fs::read_to_string(&path).unwrap();
        let data: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(data["version"], 1);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn file_format_matches_python() {
        let items = vec![
            Item::Bookmark(Bookmark {
                title: "&Ä".into(),
                location: local("/ä"),
                target: None,
                id: "b1".into(),
            }),
            Item::Folder(Folder {
                title: "F".into(),
                items: vec![],
                id: "f1".into(),
            }),
            Item::Separator { id: "s1".into() },
        ];
        // json.dumps(data, indent=2, ensure_ascii=False)
        let expected = r#"{
  "version": 1,
  "items": [
    {
      "type": "bookmark",
      "id": "b1",
      "title": "&Ä",
      "location": {
        "kind": "local",
        "path": "/ä"
      }
    },
    {
      "type": "folder",
      "id": "f1",
      "title": "F",
      "items": []
    },
    {
      "type": "separator",
      "id": "s1"
    }
  ]
}"#;
        assert_eq!(to_text(&items), expected);
    }

    #[test]
    fn damaged_file_is_tolerated() {
        let items = parse(r#"{"items": [{"type": "bookmark"}, {"type": "separator"}, 42]}"#);
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], Item::Separator { .. }));
        assert_eq!(items[0].id().len(), 32); // a fresh id
        assert!(parse("not json").is_empty());
        assert!(parse("[]").is_empty());
        assert!(parse(r#"{"items": {"a": 1}}"#).is_empty());
        let items = parse(
            r#"{"items": [{"type": "folder", "id": "", "items": [{"type": "bookmark", "id": 7, "title": 3,
                "location": {"kind": "local", "path": "/x"}, "target": "nope"}, {"type": "x"}]}]}"#,
        );
        let Item::Folder(f) = &items[0] else { panic!() };
        assert_eq!((f.title.as_str(), f.items.len()), ("", 1));
        let Item::Bookmark(b) = &f.items[0] else {
            panic!()
        };
        assert_eq!(
            (b.id.as_str(), b.title.as_str(), b.target.is_none()),
            ("7", "3", true)
        );
        assert!(load(Path::new("/nonexistent/bookmarks.json")).is_empty());
    }
}
