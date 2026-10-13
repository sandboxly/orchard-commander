//! Bookmarks: Total Commander's Directory Hotlist (port of `bookmarks_ui.py`
//! and the bookmark part of `main_window.py`).
//!
//! * ⌘D / Ctrl+D (or a click on a panel's path): the bookmark menu
//!   ([`menu::BookmarkMenu`]), with `+` add / `-` remove the current folder
//!   and `*` organize.
//! * ⌥⌘D / Ctrl+Alt+D: search all bookmarks as you type (a [`Picker`]).
//! * The organizer ([`organizer::Organizer`]) edits a copy of the tree;
//!   OK saves it.
//!
//! The list lives in `bookmarks.json` (`oc_config::bookmarks`, the 1.x
//! format), read in the background at start and written in the background
//! after each change. Locations are `oc_config::Spec`s; [`locations`] turns
//! them back into live folders (archives opened, servers connected through
//! the connections feature).

mod add;
mod form;
pub mod locations;
pub mod menu;
pub mod organizer;

use std::sync::{Arc, Mutex};

use oc_config::Spec;
use oc_config::bookmarks::{self as store, Bookmark, Item};

use crate::app::App;
use crate::commands::{Command, Registry};
use crate::dialogs::{Picker, PickerItem};
use crate::features::navigation;
use crate::main_window::{self, SideMode};
use crate::menus::Menu;
use crate::panel::{self, LoadOptions};

/// Work waiting for the file to be read.
type Deferred = Box<dyn FnOnce(&mut App)>;

/// The bookmark list (app-wide state).
#[derive(Default)]
pub struct Bookmarks {
    pub items: Vec<Item>,
    /// `bookmarks.json` has been read.
    pub loaded: bool,
    /// Commands that came before the file was read.
    waiting: Vec<Deferred>,
    /// Saves in order: each save has a number; a slower older one never
    /// overwrites a newer one.
    generation: u64,
    written: Arc<Mutex<u64>>,
    /// Per side: the latest bookmark being opened there. A slower earlier
    /// one (an archive, a server) arriving later is dropped.
    opening: [u64; 2],
}

pub fn register(reg: &mut Registry) {
    reg.add(
        Command::new("bookmarks.menu", "Bookmarks", |app| bookmark_menu(app, None, None))
            .help("Bookmarked folders: open, add the current folder, organize")
            .keys(&["Primary+D"])
            .menu(Menu::Commands, "bookmarks"),
    );
    reg.add(
        Command::new("bookmarks.goto", "Go to bookmark", go_to_bookmark)
            .help("Search bookmarks as you type")
            .keys(&["Primary+Alt+D"])
            .menu(Menu::Commands, "bookmarks"),
    );
    reg.add(
        Command::new("bookmarks.add", "Add current folder to bookmarks", |app| add_bookmark(app, None))
            .help("Bookmark the active panel's folder")
            .menu(Menu::Commands, "bookmarks"),
    );
    reg.add(
        Command::new("bookmarks.organize", "Organize bookmarks", organize)
            .help("Reorder, group, edit and delete bookmarks")
            .menu(Menu::Commands, "bookmarks"),
    );
    reg.on_start(load);
    reg.on_shutdown(flush);
    reg.on_panel_title_click(|app, side, col, row| bookmark_menu(app, Some(side), Some((col, row + 1))));
    reg.add_location_source(drive_list_entries);
}

// ---------------------------------------------------------------------------
// Loading and saving
// ---------------------------------------------------------------------------

fn load(app: &mut App) {
    let path = app.paths.bookmarks_file();
    app.tasks.run_in_background(
        move |_| store::load(&path),
        |app, items| {
            let st = app.state::<Bookmarks>();
            st.items = items;
            st.loaded = true;
            let waiting = std::mem::take(&mut st.waiting);
            for f in waiting {
                f(app);
            }
        },
    );
}

/// Run `f` once the bookmarks are read (right away, normally).
fn when_loaded(app: &mut App, f: impl FnOnce(&mut App) + 'static) {
    let st = app.state::<Bookmarks>();
    if st.loaded {
        f(app);
    } else {
        st.waiting.push(Box::new(f));
    }
}

/// The bookmarks (empty until read).
pub fn items(app: &App) -> &[Item] {
    app.try_state::<Bookmarks>().map_or(&[], |s| &s.items)
}

/// Replace the list and save it.
pub fn set_items(app: &mut App, items: Vec<Item>) {
    let st = app.state::<Bookmarks>();
    st.items = items;
    st.loaded = true;
    save(app);
}

/// Write `bookmarks.json` in the background (not in `--screenshot` mode).
fn save(app: &mut App) {
    if !app.persists() {
        return;
    }
    let st = app.state::<Bookmarks>();
    st.generation += 1;
    let generation = st.generation;
    let items = st.items.clone();
    let written = st.written.clone();
    let path = app.paths.bookmarks_file();
    app.tasks.spawn(
        move || {
            let mut last = written.lock().unwrap_or_else(|p| p.into_inner());
            if *last > generation {
                return Ok(()); // a newer list is on disk already
            }
            *last = generation;
            store::save(&path, &items)
        },
        |app, result| {
            if let Err(e) = result {
                app.error(format!("Could not save bookmarks: {e}"));
            }
        },
    );
}

/// Quitting: make sure the latest list is on disk (a background save may
/// not have run yet). Waits for a save in progress; usually does nothing.
fn flush(app: &mut App) {
    if !app.persists() {
        return;
    }
    let path = app.paths.bookmarks_file();
    let Some(st) = app.try_state::<Bookmarks>() else { return };
    let mut last = st.written.lock().unwrap_or_else(|p| p.into_inner());
    if *last < st.generation {
        if let Err(e) = store::save(&path, &st.items) {
            log::warn!("cannot save bookmarks: {e}");
        }
        *last = st.generation;
    }
}

// ---------------------------------------------------------------------------
// Titles
// ---------------------------------------------------------------------------

/// "P  Projects": the hotkey column (upper case, or a blank) and the title
/// without its `&` marker (`menu_text` in 1.x).
pub fn menu_text(title: &str, key: Option<&str>) -> String {
    let key = key.map(str::to_owned).or_else(|| store::hotkey(title));
    let key = key.map_or_else(|| " ".to_owned(), |k| k.to_uppercase());
    format!("{key}  {}", store::plain(title))
}

/// (label, folder id) for every place a bookmark can go; `None` is the top level.
pub fn folder_choices(items: &[Item]) -> Vec<(String, Option<String>)> {
    let mut out = vec![("Top level".to_owned(), None)];
    for (trail, folder) in store::folders(items) {
        let mut parts = trail.clone();
        parts.push(store::plain(&folder.title));
        out.push((parts.join(" › "), Some(folder.id.clone())));
    }
    out
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// The side bookmark commands act on: the active one, or the other if it shows a view.
fn side_of_location(app: &App) -> usize {
    app.main.files_side()
}

/// The spec of panel `side`'s folder, if it can be described.
pub fn location_spec(app: &App, side: usize) -> Option<Spec> {
    locations::spec_of(app, &app.main.sides[side].panel.location)
}

/// ⌘D, or a click on a panel's path (`at`: just below the click).
pub fn bookmark_menu(app: &mut App, side: Option<usize>, at: Option<(u16, u16)>) {
    when_loaded(app, move |app| {
        let side = side.unwrap_or_else(|| side_of_location(app));
        let (x, y) = at.unwrap_or_else(|| {
            let frame = main_window::layout(app).frames[side];
            (frame.x + 2, frame.y + 1)
        });
        let current = location_spec(app, side);
        let m = menu::BookmarkMenu::new(items(app), current.as_ref(), side, x, y);
        app.show(m);
    });
}

/// "Add current folder…": ask for title and place, then save.
pub fn add_bookmark(app: &mut App, side: Option<usize>) {
    when_loaded(app, move |app| {
        let side = side.unwrap_or_else(|| side_of_location(app));
        let Some(location) = location_spec(app, side) else {
            app.warn("This location can't be bookmarked");
            return;
        };
        let target = location_spec(app, 1 - side);
        let choices = folder_choices(items(app));
        let dialog = add::AddBookmark::new(location.clone(), target.clone(), choices, move |app, answer| {
            let Some((title, folder_id, with_target)) = answer else { return };
            let described = location.describe();
            let st = app.state::<Bookmarks>();
            store::container_of(&mut st.items, folder_id.as_deref()).push(Item::Bookmark(Bookmark::new(
                title,
                location,
                if with_target { target } else { None },
            )));
            save(app);
            app.info(format!("Bookmarked {described}"));
        });
        app.show(dialog);
    });
}

/// "Remove current folder (…)".
pub fn remove_bookmark(app: &mut App, id: &str) {
    let st = app.state::<Bookmarks>();
    if let Some(removed) = store::remove(&mut st.items, id) {
        save(app);
        let title = match removed {
            Item::Bookmark(b) => b.title,
            Item::Folder(f) => f.title,
            Item::Separator { .. } => String::new(),
        };
        app.info(format!("Removed bookmark {}", store::plain(&title)));
    }
}

/// The organizer; OK replaces the list.
pub fn organize(app: &mut App) {
    when_loaded(app, |app| {
        let side = side_of_location(app);
        let (active, other) = (location_spec(app, side), location_spec(app, 1 - side));
        let o = organizer::Organizer::new(items(app).to_vec(), active, other, |app, result| {
            if let Some(items) = result {
                set_items(app, items);
            }
        });
        app.show(o);
    });
}

/// ⌥⌘D: search all bookmarks as you type.
pub fn go_to_bookmark(app: &mut App) {
    when_loaded(app, |app| {
        let entries: Vec<(Vec<String>, Bookmark)> =
            store::bookmarks(items(app)).into_iter().map(|(trail, b)| (trail, b.clone())).collect();
        if entries.is_empty() {
            app.warn("No bookmarks yet: add one with Ctrl+D → Add current folder");
            return;
        }
        let shown = entries
            .iter()
            .map(|(trail, b)| {
                let mut parts = trail.clone();
                parts.push(store::plain(&b.title));
                PickerItem::new(parts.join(" › "), b.location.describe())
            })
            .collect();
        let picker = Picker::new("Go to bookmark", shown, move |app, choice| {
            if let Some(i) = choice {
                let side = side_of_location(app);
                open_bookmark(app, &entries[i].1, side);
            }
        })
        .width(90);
        app.show(picker);
    });
}

/// Bookmarks in the drive list (⌥F1 / ⌥F2), marked ★.
fn drive_list_entries(app: &App, _side: usize) -> Vec<navigation::LocationItem> {
    store::bookmarks(items(app))
        .into_iter()
        .map(|(_, b)| {
            let b = b.clone();
            navigation::LocationItem {
                label: format!("★ {}", store::plain(&b.title)),
                hint: b.location.describe(),
                open: Box::new(move |app, side| open_bookmark(app, &b, side)),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Going there
// ---------------------------------------------------------------------------

/// Open the bookmark's folder in `side`, and its other folder (if any) in the other side.
pub fn open_bookmark(app: &mut App, bookmark: &Bookmark, side: usize) {
    navigate(app, &bookmark.location, side, true);
    if let Some(target) = &bookmark.target {
        navigate(app, target, 1 - side, false);
    }
}

/// Show the place described by `spec` in `side` (connecting / opening
/// archives as needed, in the background).
pub fn navigate(app: &mut App, spec: &Spec, side: usize, focus: bool) {
    if app.main.sides[side].mode != SideMode::Files {
        let active = app.main.active;
        main_window::set_mode(app, side, SideMode::Files);
        if !focus {
            app.main.active = active;
        }
    }
    let st = app.state::<Bookmarks>();
    st.opening[side] += 1;
    let request = st.opening[side];
    // Where the panel was going when asked: if the user goes elsewhere
    // while an archive opens or a server connects, the result is dropped.
    let before = app.main.sides[side].panel.loading_location.clone();
    locations::resolve(
        app,
        spec,
        Box::new(move |app, result| {
            let current = app.state::<Bookmarks>().opening[side] == request
                && app.main.sides[side].panel.loading_location == before;
            match result {
                Ok(location) if current => {
                    panel::load(app, side, location, LoadOptions::default());
                    if focus {
                        app.main.active = side;
                    }
                }
                Ok(_) => {}
                Err(e) if e.is_empty() => {}
                Err(e) => app.error(e),
            }
        }),
    );
}
