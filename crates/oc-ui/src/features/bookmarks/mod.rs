//! Bookmarks: a directory hotlist (port of `bookmarks_ui.py`
//! and the bookmark part of `main_window.py`).
//!
//! * ⌘D / Ctrl+D (or a click on a panel's path): the bookmark menu
//!   ([`menu::BookmarkMenu`]), with `+` add / `-` remove the current folder
//!   and `*` organize.
//! * ⌥⌘D / Ctrl+Alt+D: search all bookmarks as you type (a [`Picker`]).
//! * The organizer ([`organizer::Organizer`]) edits a copy of the tree;
//!   OK saves it.
//!
//! The list lives in `bookmarks.json` (`oc_config::bookmarks`, i.e.
//! `orchard_places::bookmarks`, the 1.x format), which the other Orchard
//! apps share: their file pickers show the bookmarks and add new ones. So
//! the list in memory is only a copy: before the menu, the search, the drive
//! list or the organizer shows it, the file's [`Stamp`] is checked in the
//! background and the file read again if another app changed it. Changes are
//! applied to the copy at once and to the file in the background, each to
//! the file as it is then ([`store::update`]); the organizer's OK keeps
//! what other apps added while it was open ([`store::merge_external`]).
//! Locations are `oc_config::Spec`s; [`locations`] turns them back into
//! live folders (archives opened, servers connected through the connections
//! feature).

mod add;
mod form;
pub mod locations;
pub mod menu;
pub mod organizer;

use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use oc_config::Spec;
use oc_config::bookmarks::{self as store, Bookmark, Item, Stamp};

use crate::app::App;
use crate::commands::{Command, Registry};
use crate::dialogs::{Picker, PickerItem};
use crate::features::navigation;
use crate::main_window::{self, SideMode};
use crate::menus::Menu;
use crate::panel::{self, LoadOptions};

/// Work waiting for a fresh look at the file.
type Deferred = Box<dyn FnOnce(&mut App)>;
/// A change for the file, applied to the bookmarks as they are on disk.
type Change = Box<dyn FnOnce(&mut Vec<Item>) + Send>;

/// The bookmark list (app-wide state): a copy of `bookmarks.json`.
#[derive(Default)]
pub struct Bookmarks {
    pub items: Vec<Item>,
    /// `bookmarks.json` has been read.
    pub loaded: bool,
    /// The file's stamp when `items` were read from it; the default one
    /// (as for no file) after a change of our own, so the next look reads
    /// the file again.
    stamp: Stamp,
    /// Looks at the file are numbered; `waiting` holds the commands that
    /// wait for one (by number) to finish.
    looks: u64,
    waiting: Vec<(u64, Deferred)>,
    /// The newest look whose result was taken: an older one finishing later
    /// is out of date.
    applied: u64,
    /// Counts our own changes: a look started before one is out of date.
    changes: u64,
    /// Our changes not on disk yet, in order. The next look writes them;
    /// quitting writes what is left.
    unwritten: Arc<Mutex<Vec<Change>>>,
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
    reg.before_location_list(|app, then| fresh(app, then));
}

// ---------------------------------------------------------------------------
// Loading and saving
// ---------------------------------------------------------------------------

fn load(app: &mut App) {
    fresh(app, |_| {});
}

/// Run `f` once the list matches `bookmarks.json`: after a look at the
/// file's stamp in the background, and reading it again if another app
/// changed it. In `--screenshot` mode (nothing is saved) the file is read
/// only once.
fn fresh(app: &mut App, f: impl FnOnce(&mut App) + 'static) {
    let persists = app.persists();
    let st = app.state::<Bookmarks>();
    if st.loaded && !persists {
        f(app);
        return;
    }
    st.looks += 1;
    st.waiting.push((st.looks, Box::new(f)));
    look(app);
}

/// Write our changes not on disk yet, then read the file if its stamp
/// changed; then run the commands waiting for this look.
fn look(app: &mut App) {
    let path = app.paths.bookmarks_file();
    let persists = app.persists();
    let st = app.state::<Bookmarks>();
    let (number, changes) = (st.looks, st.changes);
    let known = st.loaded.then_some(st.stamp);
    let unwritten = st.unwritten.clone();
    app.tasks.spawn(
        move || {
            let written = if persists { write_unwritten(&path, &unwritten) } else { Ok(()) };
            let stamp = Stamp::of(&path); // before reading: a change in between shows next time
            let read = (known != Some(stamp)).then(|| (stamp, store::load(&path)));
            (written, read)
        },
        move |app, (written, read)| {
            let st = app.state::<Bookmarks>();
            if let Some((stamp, items)) = read
                && st.changes == changes
                && st.applied < number
            {
                st.items = items;
                st.stamp = stamp;
                st.applied = number;
            }
            st.loaded = true;
            let (now, later): (Vec<_>, Vec<_>) = std::mem::take(&mut st.waiting).into_iter().partition(|(n, _)| *n == number);
            st.waiting = later;
            if let Err(e) = written {
                app.error(format!("Can't save bookmarks: {e}"));
            }
            for (_, f) in now {
                f(app);
            }
        },
    );
}

/// Apply `unwritten` to the file as it is now. Holds the lock while
/// writing, so quitting waits for a write under way.
fn write_unwritten(path: &Path, unwritten: &Mutex<Vec<Change>>) -> io::Result<()> {
    let mut pending = unwritten.lock().unwrap_or_else(PoisonError::into_inner);
    if pending.is_empty() {
        return Ok(());
    }
    let changes = std::mem::take(&mut *pending);
    store::update(path, move |items| {
        for change in changes {
            change(items);
        }
    })
}

/// Change the list: the copy at once, the file in the background (as it is
/// then, so what other apps saved meanwhile stays).
fn change(app: &mut App, change: impl FnOnce(&mut Vec<Item>) + Clone + Send + 'static) {
    let persists = app.persists();
    let st = app.state::<Bookmarks>();
    change.clone()(&mut st.items);
    st.loaded = true;
    st.changes += 1;
    st.stamp = Stamp::default();
    if !persists {
        return;
    }
    st.unwritten.lock().unwrap_or_else(PoisonError::into_inner).push(Box::new(change));
    st.looks += 1;
    look(app);
}

/// The bookmarks (empty until read).
pub fn items(app: &App) -> &[Item] {
    app.try_state::<Bookmarks>().map_or(&[], |s| &s.items)
}

/// Replace the whole list (what other apps added meanwhile is lost; the
/// organizer merges instead).
pub fn set_items(app: &mut App, items: Vec<Item>) {
    change(app, move |all| *all = items);
}

/// Quitting: make sure our changes are on disk (a background write may not
/// have run yet). Waits for a write in progress; usually does nothing.
fn flush(app: &mut App) {
    if !app.persists() {
        return;
    }
    let path = app.paths.bookmarks_file();
    let Some(st) = app.try_state::<Bookmarks>() else { return };
    if let Err(e) = write_unwritten(&path, &st.unwritten) {
        log::warn!("cannot save bookmarks: {e}");
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
    fresh(app, move |app| {
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
    fresh(app, move |app| {
        let side = side.unwrap_or_else(|| side_of_location(app));
        let Some(location) = location_spec(app, side) else {
            app.warn("Can't bookmark this location: only folders can be bookmarked");
            return;
        };
        let target = location_spec(app, 1 - side);
        let choices = store::folder_choices(items(app));
        let dialog = add::AddBookmark::new(location.clone(), target.clone(), choices, move |app, answer| {
            let Some((title, folder_id, with_target)) = answer else { return };
            let described = location.describe();
            let bookmark = Bookmark::new(title, location, if with_target { target } else { None });
            // Like `store::add_bookmark`, on the copy and on the file.
            change(app, move |items| store::container_of(items, folder_id.as_deref()).push(bookmark.into()));
            app.info(format!("Bookmarked {described}"));
        });
        app.show(dialog);
    });
}

/// "Remove current folder (…)".
pub fn remove_bookmark(app: &mut App, id: &str) {
    let Some(removed) = store::find(items(app), id).cloned() else { return };
    let id = id.to_owned();
    change(app, move |items| {
        store::remove(items, &id);
    });
    let title = match removed {
        Item::Bookmark(b) => b.title,
        Item::Folder(f) => f.title,
        Item::Separator { .. } => String::new(),
    };
    app.info(format!("Removed bookmark {}", store::plain(&title)));
}

/// The organizer edits a copy; OK saves it, keeping what other apps
/// added while it was open.
pub fn organize(app: &mut App) {
    fresh(app, |app| {
        let side = side_of_location(app);
        let (active, other) = (location_spec(app, side), location_spec(app, 1 - side));
        let base = items(app).to_vec();
        let o = organizer::Organizer::new(base.clone(), active, other, move |app, result| {
            if let Some(edited) = result {
                change(app, move |items| *items = store::merge_external(&base, edited, items));
            }
        });
        app.show(o);
    });
}

/// ⌥⌘D: search all bookmarks as you type.
pub fn go_to_bookmark(app: &mut App) {
    fresh(app, |app| {
        let entries: Vec<(Vec<String>, Bookmark)> =
            store::bookmarks(items(app)).into_iter().map(|(trail, b)| (trail, b.clone())).collect();
        if entries.is_empty() {
            let key = if cfg!(target_os = "macos") { "⌘D" } else { "Ctrl+D" };
            app.warn(format!("No bookmarks yet — press {key} and choose Add current folder"));
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
