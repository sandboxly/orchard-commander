//! Panel navigation commands: reload, hidden files, swap, same folder,
//! transfer left/right, select all, go to path, location chooser.

use std::path::PathBuf;

use crate::app::App;
use crate::commands::{Command, Registry};
use crate::dialogs::{self, Picker, PickerItem};
use crate::main_window::{self, SideMode};
use crate::menus::Menu;
use crate::panel::{self, LoadOptions};

pub fn register(reg: &mut Registry) {
    reg.add(
        Command::new("panel.select_all", "Select all", |app| set_marks(app, true))
            .help("Mark every file and folder in the active panel")
            .keys(&["Primary+A"])
            .menu(Menu::Files, "select"),
    );
    reg.add(
        Command::new("panel.clear_selection", "Clear selection", |app| set_marks(app, false))
            .help("Unmark everything in the active panel")
            .keys(&["Primary+Alt+A"])
            .menu(Menu::Files, "select"),
    );
    reg.add(
        Command::new("nav.goto", "Go to path", goto_path)
            .help("Type a directory or a server address")
            .keys(&["Primary+G", "Primary+Shift+G"])
            .menu(Menu::Commands, "navigate"),
    );
    reg.add(
        Command::new("panel.swap", "Swap panels", swap_panels)
            .help("Exchange the left and right folders (Total Commander: Ctrl+U)")
            .keys(&["Primary+U"])
            .menu(Menu::Commands, "panels"),
    );
    reg.add(
        Command::new("panel.same", "Same directory", same_dir)
            .help("Show the active folder in the other panel too (Target = Source)")
            .keys(&["Primary+E"])
            .menu(Menu::Commands, "panels"),
    );
    reg.add(
        Command::new("panel.reload", "Reload panels", reload_all)
            .help("Re-read both directories")
            .keys(&["Primary+R"])
            .menu(Menu::Commands, "panels"),
    );
    reg.add(
        Command::new("panel.hidden", "Toggle hidden files", toggle_hidden)
            .help("Show or hide hidden files")
            .keys(&["Primary+Shift+."])
            .other_keys(&["Primary+H"])
            .menu(Menu::Options, "view")
            .checked(|app| app.settings.hidden_visible()),
    );
    for side in 0..2 {
        let (loc_id, loc_title, loc_key, tr_id, tr_title, tr_key, menu) = if side == 0 {
            (
                "nav.location_left",
                "Left drive / location",
                "Alt+F1",
                "panel.transfer_left",
                "Transfer to left panel",
                "Primary+Left",
                Menu::Left,
            )
        } else {
            (
                "nav.location_right",
                "Right drive / location",
                "Alt+F2",
                "panel.transfer_right",
                "Transfer to right panel",
                "Primary+Right",
                Menu::Right,
            )
        };
        reg.add(
            Command::new(loc_id, loc_title, move |app| choose_location(app, side))
                .help("Pick a drive or volume for this panel")
                .keys(&[loc_key])
                .menu(menu, "location"),
        );
        reg.add(
            Command::new(tr_id, tr_title, move |app| transfer(app, side))
                .help("Open the folder under the cursor there, or match the other panel")
                .keys(&[tr_key])
                .menu(menu, "location"),
        );
    }
}

fn set_marks(app: &mut App, value: bool) {
    let side = app.main.files_side();
    app.main.sides[side].panel.set_all_marks(value);
}

pub fn reload_all(app: &mut App) {
    for side in 0..2 {
        panel::reload(app, side);
    }
}

pub fn toggle_hidden(app: &mut App) {
    let show = !app.settings.hidden_visible();
    app.update_settings(move |s| s.show_hidden = Some(show));
    for side in 0..2 {
        app.main.sides[side].panel.show_hidden = show;
        panel::reload(app, side);
    }
    app.info(if show { "Showing hidden files" } else { "Hiding hidden files" });
}

pub fn swap_panels(app: &mut App) {
    let (a, b) = (main_window::location(app, 0), main_window::location(app, 1));
    panel::load(app, 0, b, LoadOptions::default());
    panel::load(app, 1, a, LoadOptions::default());
}

pub fn same_dir(app: &mut App) {
    let side = app.main.files_side();
    let loc = main_window::location(app, side);
    panel::load(app, 1 - side, loc, LoadOptions::default());
}

/// Total Commander's Ctrl+Left / Ctrl+Right. From the other side: open the
/// folder (or archive) under the cursor in `dest`, or that side's folder if
/// the cursor is on a file. From `dest` itself: show the other side's folder.
pub fn transfer(app: &mut App, dest: usize) {
    let source = app.main.files_side();
    if app.main.sides[dest].mode != SideMode::Files {
        main_window::set_mode(app, dest, SideMode::Files);
    }
    if source == dest {
        let loc = main_window::location(app, 1 - dest);
        panel::load(app, dest, loc, LoadOptions::default());
        return;
    }
    let p = &app.main.sides[source].panel;
    let entry = p.cursor_entry().cloned();
    match entry {
        Some(e) if e.is_dir => {
            let loc = p.location.child(&[&e.name]);
            panel::load(app, dest, loc, LoadOptions::default());
        }
        Some(e) if oc_vfs::is_archive_name(&e.name) => {
            let file = p.location.child(&[&e.name]);
            panel::enter_archive(app, dest, file);
        }
        _ => {
            let loc = p.location.clone();
            panel::load(app, dest, loc, LoadOptions::default());
        }
    }
}

/// ⌘G: type a folder (absolute, `~/…`, or relative to the active folder),
/// or a server address (`sftp://…`, see [`crate::connections`]).
pub fn goto_path(app: &mut App) {
    let side = app.main.files_side();
    let here = main_window::location(app, side);
    let message = format!("Directory, or server address ({}):", oc_provider::address_examples());
    dialogs::ask_text(app, "Go to", &message, &here.display(), "Go", move |app, value| {
        let Some(value) = value else { return };
        if crate::connections::is_url(&value) {
            crate::connections::open_url(app, side, &value);
            return;
        }
        let path = expand_home(&value);
        let loc = if path.is_absolute() {
            oc_local::local(path)
        } else {
            match panel::local_anchor(&main_window::location(app, side)) {
                Some(base) => oc_local::local(base.join(path)),
                None => oc_local::local(oc_local::home_dir().join(path)),
            }
        };
        panel::load(app, side, loc, LoadOptions::default());
    });
}

/// "~" and "~/x" → the home folder.
pub fn expand_home(value: &str) -> PathBuf {
    if value == "~" {
        return oc_local::home_dir();
    }
    if let Some(rest) = value.strip_prefix("~/").or_else(|| value.strip_prefix("~\\")) {
        return oc_local::home_dir().join(rest);
    }
    PathBuf::from(value)
}

/// Opens a chooser entry in side `usize`.
pub type OpenLocation = Box<dyn FnOnce(&mut App, usize)>;

/// An extra entry of the ⌥F1 / ⌥F2 chooser, from a feature's
/// [`LocationSource`](crate::commands::LocationSource) (saved servers, bookmarks…).
pub struct LocationItem {
    pub label: String,
    /// Shown dimmed after the label (an address, a path).
    pub hint: String,
    /// Chosen: go there in side `usize`.
    pub open: OpenLocation,
}

/// ⌥F1 / ⌥F2: drives and volumes (`oc_local::locations`, listed in the
/// background), then the features' entries (servers ⇄, bookmarks ★).
pub fn choose_location(app: &mut App, side: usize) {
    app.tasks.run_in_background(
        |_| oc_local::locations(),
        move |app, places| {
            let sources = app.registry.location_sources.clone();
            let mut extra: Vec<LocationItem> = Vec::new();
            for source in sources {
                extra.extend(source(app, side));
            }
            let mut items: Vec<PickerItem> =
                places.iter().map(|(label, path)| PickerItem::new(label.clone(), path.display().to_string())).collect();
            items.extend(extra.iter().map(|e| PickerItem::new(e.label.clone(), e.hint.clone())));
            let picker = Picker::new("Go to", items, move |app, choice| {
                let Some(i) = choice else { return };
                if let Some(k) = i.checked_sub(places.len()) {
                    if let Some(item) = extra.into_iter().nth(k) {
                        (item.open)(app, side);
                    }
                    return;
                }
                if app.main.sides[side].mode != SideMode::Files {
                    main_window::set_mode(app, side, SideMode::Files);
                }
                app.main.active = side;
                panel::load(app, side, oc_local::local(&places[i].1), LoadOptions::default());
            });
            app.show(picker);
        },
    );
}
