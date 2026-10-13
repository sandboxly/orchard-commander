//! App-wide commands: quit, menu (F9), command palette, zoom, color scheme.

use std::rc::Rc;

use oc_config::themes::FONT_SIZE_RANGE;

use crate::app::App;
use crate::commands::{Command, MenuRole, Registry};
use crate::dialogs::{Picker, PickerItem};
use crate::main_window;
use crate::menus::Menu;
use crate::theme::Theme;

pub fn register(reg: &mut Registry) {
    reg.add(
        Command::new("app.menu", "Menu", |app| {
            let menu = if app.main.active == 0 { Menu::Left } else { Menu::Right };
            main_window::open_menu(app, menu);
        })
        .help("Open the menu bar")
        .keys(&["F9"])
        .fkey("F9", "Menu"),
    );
    reg.add(
        Command::new("app.quit", "Quit", |app| app.quit())
            .help("Quit Orchard Commander")
            .keys(&["F10", "Primary+Q"])
            .fkey("F10", "Quit")
            .menu(Menu::Files, "quit")
            .role(MenuRole::Quit)
            .global(),
    );
    reg.add(
        Command::new("app.palette", "Command palette", command_palette)
            .help("Search all commands")
            .keys(&["Primary+P", "Primary+Shift+P"])
            .footer_right("Commands")
            .menu(Menu::Commands, "palette"),
    );
    reg.add(
        Command::new("app.color_scheme", "Choose color scheme…", choose_color_scheme)
            .help("Switch the color scheme, previewing as you move")
            .menu(Menu::Options, "settings"),
    );
    reg.add(
        Command::new("app.zoom_in", "Zoom in", |app| zoom(app, 1))
            .help("Larger font")
            // "+" is its own key on e.g. German layouts (1.x: "Ctrl++").
            .keys(&["Primary+=", "Primary+Shift+=", "Primary++"])
            .menu(Menu::Options, "zoom"),
    );
    reg.add(
        Command::new("app.zoom_out", "Zoom out", |app| zoom(app, -1)).help("Smaller font").keys(&["Primary+-"]).menu(Menu::Options, "zoom"),
    );
    reg.add(
        Command::new("app.zoom_reset", "Reset zoom", |app| zoom(app, 0))
            .help("Default font size")
            .keys(&["Primary+0"])
            .menu(Menu::Options, "zoom"),
    );
}

/// ⌘= / ⌘- / ⌘0: font size (reset stores "not chosen", like Python's None).
pub fn zoom(app: &mut App, delta: i64) {
    let (lo, hi) = FONT_SIZE_RANGE;
    let size = if delta == 0 { None } else { Some((app.settings.appearance().font_size + delta).clamp(lo, hi)) };
    app.update_settings(move |s| s.font_size = size);
}

/// The palette's order: 1.x's command list (`_build_commands`). Commands not
/// named here follow in registration order.
const PALETTE_ORDER: &[&str] = &[
    "preview.toggle",
    "preview.cycle",
    "files.edit",
    "files.new_file",
    "files.copy",
    "files.move",
    "files.mkdir",
    "files.trash",
    "app.menu",
    "app.quit",
    "app.settings",
    "app.color_scheme",
    "app.palette",
    "panel.reload",
    "panel.hidden",
    "panel.swap",
    "panel.same",
    "panel.transfer_left",
    "panel.transfer_right",
    "panel.select_all",
    "panel.clear_selection",
    "terminal.toggle",
    "terminal.panel_folder",
    "files.multi_rename",
    "files.undo_rename",
    "search.find",
    "search.stop",
    "search.last",
    "bookmarks.menu",
    "bookmarks.goto",
    "bookmarks.add",
    "bookmarks.organize",
    "nav.goto",
    "conn.manager",
    "conn.disconnect",
    "nav.location_left",
    "nav.location_right",
    "app.zoom_in",
    "app.zoom_out",
    "app.zoom_reset",
];

/// ⌘P: every command (in 1.x's order), filtered by the typed words; Enter runs it.
pub fn command_palette(app: &mut App) {
    let rank = |id: &str| PALETTE_ORDER.iter().position(|&o| o == id).unwrap_or(PALETTE_ORDER.len());
    let mut listed: Vec<&crate::commands::Command> = app.registry.commands().iter().filter(|c| c.in_palette).collect();
    listed.sort_by_key(|c| rank(c.id)); // stable: the rest keep registration order
    let cmds: Vec<(&'static str, PickerItem)> = listed
        .into_iter()
        .map(|c| {
            let keys = c.shortcuts.iter().map(|s| s.display()).collect::<Vec<_>>().join(", ");
            (c.id, PickerItem::new(c.title.clone(), if keys.is_empty() { c.help.clone() } else { keys }))
        })
        .collect();
    let ids: Vec<&'static str> = cmds.iter().map(|(id, _)| *id).collect();
    let items = cmds.into_iter().map(|(_, it)| it).collect();
    let picker = Picker::new("Commands", items, move |app, choice| {
        if let Some(i) = choice {
            let id = ids[i];
            app.later(move |app| app.run_command(id));
        }
    })
    .width(72);
    app.show(picker);
}

/// Pick a scheme for the current mode (light or dark when following the
/// system), previewing each as it is highlighted.
pub fn choose_color_scheme(app: &mut App) {
    // Re-read the folder first (in the background): scheme files may have
    // been added by hand.
    app.refresh_color_schemes(show_color_schemes);
}

fn show_color_schemes(app: &mut App) {
    let schemes = Rc::new(app.color_schemes());
    let appearance = app.settings.appearance();
    let slot = appearance.scheme_setting(app.system_dark);
    let current = appearance.scheme_name(app.system_dark).to_owned();
    let items = schemes.iter().map(|s| PickerItem::new(s.name.clone(), if s.dark { "dark" } else { "light" })).collect();
    let start = schemes.iter().position(|s| s.name == current).unwrap_or(0);
    let original = app.theme.clone();
    let preview = schemes.clone();
    let chosen = schemes.clone();
    let picker = Picker::new("Color scheme", items, move |app, choice| {
        app.theme = original;
        app.request(crate::app::PlatformRequest::ThemeChanged);
        if let Some(i) = choice {
            let name = chosen[i].name.clone();
            app.update_settings(move |s| match slot {
                "dark_scheme" => s.dark_scheme = name.clone(),
                "light_scheme" => s.light_scheme = name.clone(),
                _ => s.color_scheme = name.clone(),
            });
        }
    })
    .current(start)
    .on_highlight(move |app, i| {
        app.theme = Theme::with_scheme(&app.settings.appearance(), preview[i].clone());
        app.request(crate::app::PlatformRequest::ThemeChanged);
    });
    app.show(picker);
}
