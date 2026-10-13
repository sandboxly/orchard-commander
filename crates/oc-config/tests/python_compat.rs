//! Files written by the Python version (generated from it into `tests/data`)
//! must load, and saving them again must reproduce them byte for byte.

use std::fs;
use std::path::{Path, PathBuf};

use oc_config::themes::{self, ColorScheme};
use oc_config::{Settings, bookmarks, connections};
use serde_json::Value;

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name)
}

fn read(name: &str) -> String {
    fs::read_to_string(data(name)).unwrap()
}

#[test]
fn settings_round_trip() {
    let s = Settings::load(&data("settings.json"));
    assert_eq!(s.color_scheme, "nord");
    assert_eq!(
        (s.font_size, s.line_spacing, s.show_hidden),
        (Some(15), Some(1.25), Some(false))
    );
    assert_eq!(s.left_path, "/home/jörg/Ö 😀");
    assert_eq!(s.to_json(), read("settings.json"));
}

/// Floats in Python's repr ("1e-05", "1e+16", "-0.0") and control characters.
#[test]
fn settings_with_floats_round_trip() {
    let s = Settings::load(&data("settings_floats.json"));
    assert_eq!((s.font_size, s.line_spacing), (Some(9), Some(0.85)));
    assert_eq!(s.extra["ratio"], 1e-5);
    assert_eq!(s.to_json(), read("settings_floats.json"));
    // Saved through update(), the file is still Python's byte for byte.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("settings.json");
    fs::copy(data("settings_floats.json"), &path).unwrap();
    let mut mine = Settings::default();
    mine.update(&path, |s| s.font_size = Some(9)).unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        read("settings_floats.json")
    );
}

#[test]
fn bookmarks_round_trip() {
    let items = bookmarks::load(&data("bookmarks.json"));
    assert_eq!(items.len(), 3);
    assert_eq!(bookmarks::to_text(&items), read("bookmarks.json"));
}

#[test]
fn connections_round_trip() {
    let items = connections::load_saved(&data("connections.json"));
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].name, "Prod ✓");
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("connections.json");
    connections::save_all(&path, &items).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), read("connections.json"));
}

#[test]
fn builtin_schemes_match_python() {
    let python: Vec<Value> = serde_json::from_str(&read("builtin_schemes.json")).unwrap();
    assert_eq!(python.len(), themes::builtin_schemes().len());
    for (p, r) in python.iter().zip(themes::builtin_schemes()) {
        assert_eq!(p["name"], r.name.as_str());
        assert_eq!(p["dark"], r.dark);
        assert_eq!(p["pygments_style"], r.pygments_style.as_str());
        for (role, _, _) in themes::COLOR_ROLES {
            assert_eq!(p[*role], r.color(role).unwrap(), "{} {role}", r.name);
        }
    }
}

#[test]
fn scheme_file_matches_python() {
    let paper: &ColorScheme = themes::builtin_scheme("paper").unwrap();
    let scheme = paper.renamed("Mein \"Papier\" ü\t");
    assert_eq!(themes::scheme_to_toml(&scheme), read("scheme.toml"));
    let table: toml::Table = read("scheme.toml").parse().unwrap();
    // The name is stripped on load, as in Python.
    assert_eq!(
        themes::scheme_from_table(&table, ""),
        paper.renamed("Mein \"Papier\" ü")
    );
}
