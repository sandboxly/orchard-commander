//! Find files end to end: the dialog, results as a folder in the panel,
//! acting on the real files, stopping, ⇧⌘F, pruning and relocating
//! (ports of the search tests in fcmd's `tests/test_ui.py`).

use std::collections::HashMap;
use std::fs;
use std::path::{MAIN_SEPARATOR, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use oc_search::{Query, SearchResultsFS};
use oc_ui::features::search::{self, SearchState};
use oc_ui::testing::{headless_app, press, press_with, settle, type_text, wait_until};
use oc_ui::{App, Key, MAIN_WINDOW, Mods};
use oc_vfs::{Capabilities, Entry, FileSystem, ListOptions, ReadStream, VPath, VfsError, WalkIter};

struct Env {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    /// src/{a.txt, b.bin, sub/c.txt, .hidden, docs/{one.txt, two.md}}
    src: PathBuf,
    dst: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = oc_local::absolute(&fs::canonicalize(tmp.path()).unwrap());
    let (config, src, dst) = (base.join("config"), base.join("src"), base.join("dst"));
    for d in [&config, &src, &dst, &src.join("sub"), &src.join("docs")] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(src.join("a.txt"), "alpha").unwrap();
    fs::write(src.join("b.bin"), [0u8; 2000]).unwrap();
    fs::write(src.join("sub/c.txt"), "gamma").unwrap();
    fs::write(src.join(".hidden"), "h").unwrap();
    fs::write(src.join("docs/one.txt"), "needle here").unwrap();
    fs::write(src.join("docs/two.md"), "no match").unwrap();
    Env { _tmp: tmp, config, src, dst }
}

fn app(e: &Env) -> App {
    let mut app = headless_app(&e.config, &e.src, &e.dst);
    app.resize(MAIN_WINDOW, 120, 40);
    settle(&mut app);
    app
}

fn rel(s: &str) -> String {
    s.replace('/', std::path::MAIN_SEPARATOR_STR)
}

fn names(app: &App, side: usize) -> Vec<String> {
    let mut v: Vec<String> = app.main.sides[side].panel.entries.iter().map(|e| e.name.replace(MAIN_SEPARATOR, "/")).collect();
    v.sort();
    v
}

fn last(app: &App) -> Arc<SearchResultsFS> {
    app.try_state::<SearchState>().and_then(|s| s.last_results().cloned()).expect("a search ran")
}

fn shows(app: &App, side: usize, results: &Arc<SearchResultsFS>) -> bool {
    std::ptr::addr_eq(Arc::as_ptr(&app.main.sides[side].panel.location.fs), Arc::as_ptr(results))
}

/// Wait until the search is over and panel `side` lists all its hits.
fn wait_results(app: &mut App, side: usize) -> Arc<SearchResultsFS> {
    wait_until(app, "the search", Duration::from_secs(20), |app| {
        let Some(r) = app.try_state::<SearchState>().and_then(|s| s.last_results().cloned()) else { return false };
        let p = &app.main.sides[side].panel;
        !r.is_running() && shows(app, side, &r) && !p.loading() && p.entries.len() == r.hit_count()
    });
    settle(app);
    last(app)
}

/// Like the Python tests' `run_search`: a query on the side's folder.
fn run_search(app: &mut App, side: usize, configure: impl FnOnce(&mut Query)) -> Arc<SearchResultsFS> {
    let here = app.main.sides[side].panel.location.clone();
    let here = if here.fs.type_name() == "search results" { here.fs.exit_to().unwrap() } else { here };
    let mut q = Query::new(vec![here]);
    configure(&mut q);
    search::start_search(app, q, side);
    wait_results(app, side)
}

#[test]
fn dialog_search_shows_results_as_a_folder() {
    let e = env();
    let mut app = app(&e);
    press_with(&mut app, Key::Char('f'), Mods::primary());
    assert_eq!(app.modal_titles(), ["Find files"]);
    let text = app.top_modal_text().unwrap();
    assert!(text.contains(&format!("Search in: {}", e.src.display())), "{text}");
    type_text(&mut app, "*.txt"); // replaces the selected "*"
    press(&mut app, Key::Enter);
    assert!(app.modal_titles().is_empty());
    let results = wait_results(&mut app, 0);
    assert_eq!(results.hit_count(), 3);
    assert_eq!(names(&app, 0), ["a.txt", "docs/one.txt", "sub/c.txt"]);
    let p = &app.main.sides[0].panel;
    assert!(p.display_path().starts_with("Search: *.txt in src"), "{}", p.display_path());
    assert!(p.subtitle().contains("3 hits"), "{}", p.subtitle());
    assert!(p.has_parent);
    assert!(app.toast.as_ref().is_some_and(|t| t.text == "Search finished: 3 hits"));

    // ".." / Backspace: back to where the search started.
    press(&mut app, Key::Backspace);
    settle(&mut app);
    assert_eq!(app.main.sides[0].panel.location, oc_local::local(&e.src));
    // ⇧⌘F: the last results again.
    press_with(&mut app, Key::Char('f'), Mods::primary().union(Mods::SHIFT));
    settle(&mut app);
    assert!(shows(&app, 0, &results));

    // The dialog remembers the entries; from the results it searches where they came from.
    press(&mut app, Key::F(7)); // can't create folders in the results
    assert!(app.toast.as_ref().is_some_and(|t| t.text.contains("search results are read-only")));
    press_with(&mut app, Key::F(7), Mods::ALT);
    let text = app.top_modal_text().unwrap();
    assert!(text.contains("Search for: *.txt") && text.contains(&format!("Search in: {}", e.src.display())), "{text}");
    press(&mut app, Key::Escape);
    assert!(app.modal_titles().is_empty());
}

#[test]
fn dialog_options_build_the_query() {
    let e = env();
    let mut app = app(&e);
    press_with(&mut app, Key::F(7), Mods::ALT);
    type_text(&mut app, "*.txt|*one*");
    // Tab order: names, RegEx, Case, Search in, Depth, Find, Hidden, Archives, Containing…
    for _ in 0..4 {
        press(&mut app, Key::Tab);
    }
    // Choices are combo boxes as in 1.x: ↑/↓ change them without wrapping
    // around and ←/→ do nothing, so this ends on the second option.
    press(&mut app, Key::Up);
    press(&mut app, Key::Right);
    press(&mut app, Key::Down); // Depth: This folder only
    for _ in 0..4 {
        press(&mut app, Key::Tab);
    }
    type_text(&mut app, "a");
    for _ in 0..6 {
        press(&mut app, Key::Tab);
    }
    type_text(&mut app, "lots"); // at least
    press(&mut app, Key::Enter);
    assert_eq!(app.modal_titles(), ["Find files"], "a bad size keeps the dialog open");
    assert!(app.top_modal_text().unwrap().contains("Not a size: 'lots'"));
    press_with(&mut app, Key::Char('a'), Mods::primary());
    type_text(&mut app, "1k");
    press(&mut app, Key::Enter);
    assert!(app.modal_titles().is_empty());
    let results = wait_results(&mut app, 0);
    let q = results.query();
    assert_eq!((q.names.as_str(), q.text.as_str(), q.min_size, q.max_depth), ("*.txt|*one*", "a", Some(1024), Some(0)));
    // Only a.txt is in the folder itself, but it is smaller than 1k.
    assert_eq!(results.hit_count(), 0);
    assert_eq!(app.state::<SearchState>().form.depth, 1);

    // A typed folder that doesn't exist: checked in the background, the dialog stays.
    press_with(&mut app, Key::Char('f'), Mods::primary());
    for _ in 0..3 {
        press(&mut app, Key::Tab);
    }
    type_text(&mut app, &e.src.join("nope").display().to_string());
    press(&mut app, Key::Enter);
    wait_until(&mut app, "the folder check", Duration::from_secs(10), |app| {
        app.top_modal_text().is_some_and(|t| t.contains("Not a folder:"))
    });
    // Several folders separated by ";".
    press_with(&mut app, Key::Char('a'), Mods::primary());
    type_text(&mut app, &format!("{};{}", e.src.join("sub").display(), e.src.join("docs").display()));
    // Search for "*", no size limit, all subfolders.
    for _ in 0..3 {
        press_with(&mut app, Key::Tab, Mods::SHIFT);
    }
    press_with(&mut app, Key::Char('a'), Mods::primary());
    type_text(&mut app, "*");
    for _ in 0..4 {
        press(&mut app, Key::Tab);
    }
    press(&mut app, Key::End);
    press(&mut app, Key::Home); // back to All subfolders
    press(&mut app, Key::F(4)); // opens the list…
    press(&mut app, Key::Down);
    press(&mut app, Key::Escape); // …which Esc closes without picking
    assert_eq!(app.modal_titles(), ["Find files"], "F4 opened the list, Esc closed only it");
    for _ in 0..4 {
        press(&mut app, Key::Tab);
    }
    press_with(&mut app, Key::Char('a'), Mods::primary());
    press(&mut app, Key::Backspace); // no text
    for _ in 0..6 {
        press(&mut app, Key::Tab);
    }
    press_with(&mut app, Key::Char('a'), Mods::primary());
    press(&mut app, Key::Backspace);
    press(&mut app, Key::Enter);
    wait_until(&mut app, "dialog to close", Duration::from_secs(10), |app| app.modal_titles().is_empty());
    let results = wait_results(&mut app, 0);
    assert_eq!(names(&app, 0), ["docs/one.txt", "docs/two.md", "sub/c.txt"], "several roots: named below each root");
    assert_eq!(results.query().roots.len(), 2);
}

#[test]
fn regex_options_accept_python_look_around() {
    // 1.x compiles both patterns with Python's `re`: look-around works.
    let e = env();
    let mut app = app(&e);
    press_with(&mut app, Key::Char('f'), Mods::primary());
    type_text(&mut app, r"^(?!a).*\.txt$");
    press(&mut app, Key::Tab);
    press(&mut app, Key::Space); // RegEx
    for _ in 0..7 {
        press(&mut app, Key::Tab);
    }
    type_text(&mut app, "(?<=nee)dle"); // Containing
    press(&mut app, Key::Tab);
    press(&mut app, Key::Space); // RegEx
    press(&mut app, Key::Enter);
    assert!(app.modal_titles().is_empty(), "{:?}", app.top_modal_text());
    let results = wait_results(&mut app, 0);
    assert!(results.query().name_regex && results.query().text_regex);
    assert_eq!(names(&app, 0), ["docs/one.txt"]);
}

#[test]
fn results_act_on_the_real_files_move_all_hits() {
    let e = env();
    let mut app = app(&e);
    run_search(&mut app, 0, |q| q.names = "*.txt".into());
    type_text(&mut app, "+"); // mark all hits
    press(&mut app, Key::F(6));
    assert_eq!(app.modal_titles(), ["Move"]);
    press(&mut app, Key::Enter); // to the other panel
    wait_until(&mut app, "the move", Duration::from_secs(20), |app| {
        app.operation.is_none() && app.main.sides[0].panel.entries.is_empty() && !app.main.sides[0].panel.loading()
    });
    let mut moved: Vec<String> = fs::read_dir(&e.dst).unwrap().map(|d| d.unwrap().file_name().to_string_lossy().into_owned()).collect();
    moved.sort();
    assert_eq!(moved, ["a.txt", "c.txt", "one.txt"]);
    assert!(!e.src.join("docs/one.txt").exists(), "really moved, not copied");
    assert_eq!(last(&app).hit_count(), 0, "moved hits leave the results");
}

#[test]
fn copy_into_the_results_is_refused() {
    let e = env();
    let mut app = app(&e);
    run_search(&mut app, 1, |q| {
        q.roots = vec![oc_local::local(&e.src)];
        q.names = "*.md".into();
    });
    app.main.active = 0;
    let row = app.main.sides[0].panel.row_of("a.txt").unwrap();
    app.main.sides[0].panel.set_cursor(row);
    press(&mut app, Key::F(5));
    assert!(app.modal_titles().is_empty());
    assert!(app.toast.as_ref().is_some_and(|t| t.text == "Can't copy there: search results are read-only — open a folder in this panel first"));
}

#[test]
fn trash_hits_on_another_file_system() {
    let e = env();
    let mut app = app(&e);
    let mem = Arc::new(oc_vfs::testing::MemFS::with_capabilities(Capabilities {
        writable: true,
        can_trash: true,
        can_rename: true,
        watchable: false,
    }));
    mem.add_file("docs/two.md", "x");
    mem.add_file("docs/one.txt", "needle here");
    mem.add_file("top.md", "y");
    let root = VPath::new(mem.clone(), "");
    run_search(&mut app, 0, |q| {
        q.roots = vec![root.clone()];
        q.names = "*.md".into();
    });
    assert_eq!(names(&app, 0), ["docs/two.md", "top.md"]);
    let row = app.main.sides[0].panel.row_of(&rel("docs/two.md")).unwrap();
    app.main.sides[0].panel.set_cursor(row);
    press(&mut app, Key::F(8));
    assert_eq!(app.modal_titles(), ["Move to trash"]);
    assert!(app.top_modal_text().unwrap().contains(&rel("'docs/two.md'")));
    let gate = oc_vfs::testing::Gate::new();
    mem.set_trash_gate(Some(gate.clone()));
    press(&mut app, Key::Enter);
    // The hit leaves the results at once, and a reload meanwhile doesn't bring it back.
    assert_eq!(names(&app, 0), ["top.md"]);
    oc_ui::panel::reload(&mut app, 0);
    wait_until(&mut app, "reload", Duration::from_secs(20), |app| !app.main.sides[0].panel.loading());
    assert_eq!(names(&app, 0), ["top.md"]);
    assert!(mem.trashed().is_empty(), "still held");
    gate.open();
    wait_until(&mut app, "trash", Duration::from_secs(20), |app| !oc_ui::features::fileops::removing(app));
    // The hit is forgotten as the job ends, before the panels reload: no
    // waiting for the prune, so it never shows again in between.
    assert_eq!(last(&app).hit_count(), 1);
    assert_eq!(names(&app, 0), ["top.md"]);
    settle(&mut app);
    assert_eq!(mem.trashed(), ["docs/two.md"]);
    assert_eq!(names(&app, 0), ["top.md"]);
    // ".." leads to the root of that file system.
    press(&mut app, Key::Backspace);
    settle(&mut app);
    assert_eq!(app.main.sides[0].panel.location, root);
}

#[test]
fn enter_and_preview_targets_are_the_real_files() {
    let e = env();
    let mut app = app(&e);
    run_search(&mut app, 0, |q| q.text = "needle".into());
    assert_eq!(names(&app, 0), ["docs/one.txt"]);
    let p = &mut app.main.sides[0].panel;
    p.set_cursor(p.row_of(&rel("docs/one.txt")).unwrap());
    assert_eq!(p.cursor_target(), Some(oc_local::local(e.src.join("docs/one.txt"))));
    assert_eq!(p.selected_paths(), vec![oc_local::local(e.src.join("docs/one.txt"))]);

    run_search(&mut app, 0, |q| {
        q.names = "docs".into();
        q.kinds = oc_search::Kinds::Dirs;
    });
    let p = &mut app.main.sides[0].panel;
    p.set_cursor(p.row_of("docs").unwrap());
    press(&mut app, Key::Enter); // a folder hit: into the real folder
    settle(&mut app);
    assert_eq!(app.main.sides[0].panel.location, oc_local::local(e.src.join("docs")));
}

#[test]
fn searches_inside_archives_with_the_registered_opener() {
    oc_archive::register();
    let e = env();
    {
        let file = fs::File::create(e.src.join("pack.zip")).unwrap();
        let mut z = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        z.start_file("inner/deep.txt", opts).unwrap();
        std::io::Write::write_all(&mut z, b"needle").unwrap();
        z.finish().unwrap();
    }
    let mut app = app(&e);
    run_search(&mut app, 0, |q| {
        q.names = "deep*".into();
        q.search_archives = true;
    });
    assert_eq!(names(&app, 0), ["pack.zip/inner/deep.txt"]);
    run_search(&mut app, 0, |q| q.names = "deep*".into());
    assert!(names(&app, 0).is_empty(), "archives stay closed unless asked");
}

/// An endless tree: every folder holds one folder, listed slowly.
struct Endless;

impl FileSystem for Endless {
    fn type_name(&self) -> String {
        "endless".into()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    fn list_dir(&self, _path: &str, _opts: ListOptions) -> oc_vfs::Result<Vec<Entry>> {
        std::thread::sleep(Duration::from_millis(5));
        Ok(vec![Entry::dir("d", 0.0)])
    }
    fn stat(&self, _path: &str) -> oc_vfs::Result<Entry> {
        Ok(Entry::dir("d", 0.0))
    }
    fn parent_path(&self, path: &str) -> Option<String> {
        (!path.is_empty()).then(|| path.rsplit_once('/').map_or(String::new(), |(p, _)| p.to_owned()))
    }
    fn join(&self, path: &str, names: &[&str]) -> String {
        let mut p = path.to_owned();
        for n in names {
            if !p.is_empty() {
                p.push('/');
            }
            p.push_str(n);
        }
        p
    }
    fn basename(&self, path: &str) -> String {
        path.rsplit('/').next().unwrap_or_default().to_owned()
    }
    fn display(&self, path: &str) -> String {
        format!("endless:/{path}")
    }
    fn open_read(&self, path: &str) -> oc_vfs::Result<Box<dyn ReadStream>> {
        Err(VfsError::NotFound(path.into()))
    }
    fn walk<'a>(&'a self, path: &str) -> oc_vfs::Result<WalkIter<'a>> {
        Err(VfsError::NotFound(path.into()))
    }
}

#[test]
fn esc_stops_a_running_search() {
    let e = env();
    let mut app = app(&e);
    let mut q = Query::new(vec![VPath::new(Arc::new(Endless), "")]);
    q.max_depth = Some(1_000_000); // listed folder by folder
    let results = search::start_search(&mut app, q, 0);
    wait_until(&mut app, "progress", Duration::from_secs(10), |_| results.progress().folders() > 3);
    wait_until(&mut app, "the hits listed", Duration::from_secs(10), |app| {
        !app.main.sides[0].panel.loading() && !app.main.sides[0].panel.entries.is_empty()
    });
    assert!(app.main.sides[0].panel.subtitle().contains("searching…"), "{}", app.main.sides[0].panel.subtitle());
    assert!(search::is_running(&app));
    press(&mut app, Key::Escape);
    wait_until(&mut app, "the search to stop", Duration::from_secs(10), |_| !results.is_running());
    assert!(results.was_cancelled());
    wait_until(&mut app, "the final listing", Duration::from_secs(10), |app| {
        app.main.sides[0].panel.entries.len() == results.hit_count() && !app.main.sides[0].panel.loading()
    });
    assert!(app.main.sides[0].panel.subtitle().contains("stopped"));
    assert!(app.toast.as_ref().is_some_and(|t| t.text.starts_with("Search stopped: ")));
    assert!(!search::is_running(&app));
}

#[test]
fn esc_stops_a_running_search_from_a_preview_side_too() {
    let e = env();
    let mut app = app(&e);
    let mut q = Query::new(vec![VPath::new(Arc::new(Endless), "")]);
    q.max_depth = Some(1_000_000);
    let results = search::start_search(&mut app, q, 0);
    wait_until(&mut app, "progress", Duration::from_secs(10), |_| results.progress().folders() > 3);
    // The active side shows a preview (1.x: Esc is a window shortcut).
    app.run_command("right.preview");
    app.main.active = 1;
    assert!(app.main.sides[1].mode != oc_ui::main_window::SideMode::Files);
    press(&mut app, Key::Escape);
    wait_until(&mut app, "the search to stop", Duration::from_secs(10), |_| !results.is_running());
    assert!(results.was_cancelled());
}

#[test]
fn last_results_without_a_search_and_relocation() {
    let e = env();
    let mut app = app(&e);
    press_with(&mut app, Key::Char('f'), Mods::primary().union(Mods::SHIFT));
    assert!(app.toast.as_ref().is_some_and(|t| t.text.starts_with("No search yet — press Alt+F7 or ")));

    let results = run_search(&mut app, 0, |q| q.names = "*.txt".into());
    // A renamed folder: the hits inside follow (multi-rename calls relocate).
    fs::rename(e.src.join("docs"), e.src.join("papers")).unwrap();
    fs::rename(e.src.join("a.txt"), e.src.join("z.txt")).unwrap();
    let moved = HashMap::from([
        (oc_local::local(e.src.join("docs")), oc_local::local(e.src.join("papers"))),
        (oc_local::local(e.src.join("a.txt")), oc_local::local(e.src.join("z.txt"))),
    ]);
    search::relocate(&mut app, &moved);
    settle(&mut app);
    assert_eq!(names(&app, 0), ["papers/one.txt", "sub/c.txt", "z.txt"]);
    assert!(results.hits().iter().all(|h| h.location.exists()));
}

#[test]
fn esc_is_left_alone_without_a_search() {
    let e = env();
    let mut app = app(&e);
    // No search: the stop command is disabled and doesn't swallow Esc.
    assert!(!search::is_running(&app));
    press(&mut app, Key::Escape);
    assert!(app.toast.is_none());
}

#[test]
fn cancelling_while_typed_folders_are_checked_starts_nothing() {
    let e = env();
    let mut app = app(&e);
    press_with(&mut app, Key::Char('f'), Mods::primary());
    for _ in 0..3 {
        press(&mut app, Key::Tab);
    }
    type_text(&mut app, &e.src.join("sub").display().to_string()); // a typed folder: checked in the background
    press(&mut app, Key::Enter);
    assert_eq!(app.modal_titles(), ["Find files"], "still checking");
    press(&mut app, Key::Escape);
    assert!(app.modal_titles().is_empty());
    settle(&mut app);
    assert!(app.try_state::<SearchState>().is_none_or(|s| s.last_results().is_none()), "Esc cancelled the search");
    assert_eq!(app.main.sides[0].panel.location, oc_local::local(&e.src));
}

#[test]
fn a_new_search_stops_the_one_it_replaces() {
    let e = env();
    let mut app = app(&e);
    let mut q = Query::new(vec![VPath::new(Arc::new(Endless), "")]);
    q.max_depth = Some(1_000_000);
    let endless = search::start_search(&mut app, q, 0);
    wait_until(&mut app, "progress", Duration::from_secs(10), |_| endless.progress().folders() > 3);
    // The second search takes the panel and "last results": the first can't be seen again.
    let second = run_search(&mut app, 0, |q| {
        q.roots = vec![oc_local::local(&e.src)];
        q.names = "*.md".into();
    });
    wait_until(&mut app, "the first search to stop", Duration::from_secs(10), |_| !endless.is_running());
    settle(&mut app);
    assert!(endless.was_cancelled());
    assert!(!search::is_running(&app));
    assert_eq!(second.hit_count(), 1);
    assert!(app.toast.as_ref().is_some_and(|t| t.text == "Search finished: 1 hit"), "{:?}", app.toast.as_ref().map(|t| &t.text));
}

#[test]
fn a_search_in_the_other_panel_keeps_running() {
    let e = env();
    let mut app = app(&e);
    let mut q = Query::new(vec![VPath::new(Arc::new(Endless), "")]);
    q.max_depth = Some(1_000_000);
    let endless = search::start_search(&mut app, q, 1);
    wait_until(&mut app, "progress", Duration::from_secs(10), |_| endless.progress().folders() > 3);
    // (No settle() while the endless search runs: it waits for all tasks.)
    let md = search::start_search(&mut app, Query { names: "*.md".into(), ..Query::new(vec![oc_local::local(&e.src)]) }, 0);
    wait_until(&mut app, "the second search", Duration::from_secs(10), |_| !md.is_running());
    assert!(endless.is_running(), "still shown in the right panel");
    press(&mut app, Key::Escape);
    wait_until(&mut app, "the search to stop", Duration::from_secs(10), |_| !endless.is_running());
}
