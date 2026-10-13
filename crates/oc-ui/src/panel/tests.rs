use std::time::{Duration, Instant};

use oc_vfs::testing::MemFS;
use oc_vfs::{Entry, VPath};

use super::*;
use crate::input::{Key, KeyEvent, Mods};

fn panel_with(names: &[(&str, bool)]) -> FilePanel {
    let fs = MemFS::new();
    fs.add_dir("dir");
    let root = fs.root();
    let loc = root.child(&["dir"]);
    let mut p = FilePanel::new(0, root, true);
    p.rows = 5;
    let entries: Vec<Entry> = names.iter().map(|(n, d)| if *d { Entry::dir(*n, 0.0) } else { Entry::file(*n, 100, 0.0) }).collect();
    p.set_entries(loc, entries, true, None);
    p
}

fn key(p: &mut FilePanel, k: Key) -> PanelAction {
    p.handle_key(&KeyEvent::plain(k), Instant::now())
}

#[test]
fn rows_include_parent() {
    let p = panel_with(&[("a", true), ("b.txt", false)]);
    assert_eq!(p.len(), 3);
    assert_eq!(p.name_at(0), Some(PARENT));
    assert_eq!(p.entry_at(0), None);
    assert_eq!(p.row_of("b.txt"), Some(2));
    assert_eq!(p.row_of(PARENT), Some(0));
    assert_eq!(p.cursor_name(), Some(PARENT));
    // ".." selects nothing.
    assert!(p.selected_entries().is_empty());
}

#[test]
fn cursor_keys_and_paging() {
    let names: Vec<(String, bool)> = (0..20).map(|i| (format!("f{i:02}"), false)).collect();
    let refs: Vec<(&str, bool)> = names.iter().map(|(n, d)| (n.as_str(), *d)).collect();
    let mut p = panel_with(&refs);
    assert_eq!(key(&mut p, Key::Down), PanelAction::Consumed);
    assert_eq!(p.cursor_name(), Some("f00"));
    key(&mut p, Key::PageDown);
    assert_eq!(p.cursor(), 5);
    key(&mut p, Key::End);
    assert_eq!((p.cursor(), p.list.top), (20, 16));
    key(&mut p, Key::Home);
    assert_eq!((p.cursor(), p.list.top), (0, 0));
    assert_eq!(key(&mut p, Key::Enter), PanelAction::Activate(PARENT.into()));
    assert_eq!(key(&mut p, Key::Backspace), PanelAction::GoParent);
    let shift_enter = p.handle_key(&KeyEvent::new(Key::Enter, Mods::SHIFT), Instant::now());
    assert_eq!(shift_enter, PanelAction::OpenExternal(PARENT.into()));
    let cmd_enter = p.handle_key(&KeyEvent::new(Key::Enter, Mods::primary()), Instant::now());
    assert_eq!(cmd_enter, PanelAction::OpenExternal(PARENT.into()));
}

#[test]
fn marking() {
    let mut p = panel_with(&[("d", true), ("a.txt", false), ("b.txt", false)]);
    key(&mut p, Key::Space); // on "..": nothing marked, moves down
    assert!(p.marked.is_empty());
    assert_eq!(p.cursor_name(), Some("d"));
    key(&mut p, Key::Insert);
    key(&mut p, Key::Space);
    assert_eq!(p.cursor_name(), Some("b.txt"));
    assert!(p.marked.contains("d") && p.marked.contains("a.txt"));
    assert_eq!(p.subtitle(), "2 marked, 100 B · memory");
    let sel: Vec<&str> = p.selected_entries().iter().map(|e| e.name.as_str()).collect();
    assert_eq!(sel, ["d", "a.txt"]);
    p.handle_key(&KeyEvent::typed("*"), Instant::now());
    assert_eq!(p.marked.iter().collect::<Vec<_>>(), [&"b.txt".to_string()]);
    p.handle_key(&KeyEvent::typed("+"), Instant::now());
    assert_eq!(p.marked.len(), 3);
    p.handle_key(&KeyEvent::typed("-"), Instant::now());
    assert!(p.marked.is_empty());
    assert_eq!(p.subtitle(), "1 dir, 2 files · memory");
    // Toggling twice unmarks again.
    p.toggle_mark_at(2);
    p.toggle_mark_at(2);
    assert!(p.marked.is_empty());
}

#[test]
fn marks_survive_reload_but_not_other_folders() {
    let mut p = panel_with(&[("a", false), ("b", false)]);
    p.set_all_marks(true);
    let loc = p.location.clone();
    p.set_entries(loc.clone(), vec![Entry::file("b", 1, 0.0), Entry::file("c", 1, 0.0)], true, None);
    assert_eq!(p.marked.iter().collect::<Vec<_>>(), [&"b".to_string()], "only names still there");
    let parent = loc.parent().unwrap();
    p.set_entries(parent, vec![Entry::file("b", 1, 0.0)], false, None);
    assert!(p.marked.is_empty());
}

#[test]
fn reload_keeps_cursor_row_or_name() {
    let mut p = panel_with(&[("a", false), ("b", false), ("c", false)]);
    p.set_cursor(2);
    let loc = p.location.clone();
    // Same folder without "select": the cursor keeps its row.
    p.set_entries(loc.clone(), vec![Entry::file("x", 1, 0.0), Entry::file("y", 1, 0.0), Entry::file("z", 1, 0.0)], true, None);
    assert_eq!(p.cursor(), 2);
    p.set_entries(loc, vec![Entry::file("x", 1, 0.0), Entry::file("z", 1, 0.0)], true, Some("z"));
    assert_eq!(p.cursor_name(), Some("z"));
}

#[test]
fn quick_search() {
    let mut p = panel_with(&[("Alpha", true), ("beta", false), ("bravo", false), ("charlie", false)]);
    let t0 = Instant::now();
    p.handle_key(&KeyEvent::typed("b"), t0);
    assert_eq!(p.cursor_name(), Some("beta"));
    p.handle_key(&KeyEvent::typed("r"), t0 + Duration::from_millis(100));
    assert_eq!(p.cursor_name(), Some("bravo"), "extends the search");
    p.handle_key(&KeyEvent::typed("a"), t0 + Duration::from_secs(5));
    assert_eq!(p.cursor_name(), Some("Alpha"), "a pause starts over, case-insensitive, wraps");
    let t1 = t0 + Duration::from_secs(10);
    p.handle_key(&KeyEvent::typed("b"), t1);
    p.handle_key(&KeyEvent::typed("b"), t1 + Duration::from_millis(100));
    assert_eq!(p.cursor_name(), Some("bravo"), "the same letter again cycles");
    assert!(!p.quick_search("zzz", t1 + Duration::from_secs(5)));
    assert_eq!(p.cursor_name(), Some("bravo"), "no match: the cursor stays");
}

#[test]
fn display_path_uses_tilde() {
    let home = oc_local::home_dir();
    let p = FilePanel::new(0, oc_local::local(home.join("src")), true);
    assert_eq!(p.display_path(), format!("~{}src", std::path::MAIN_SEPARATOR));
}

#[test]
fn local_anchor_of_archives_is_their_folder() {
    let t = tempfile::tempdir().unwrap();
    let v: VPath = oc_local::local(t.path());
    assert_eq!(local_anchor(&v), Some(oc_local::absolute(t.path())));
    let mem = MemFS::new().root();
    assert_eq!(local_anchor(&mem), None);
}
