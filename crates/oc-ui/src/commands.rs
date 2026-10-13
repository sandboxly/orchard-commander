//! The command registry (port of `MainWindow._build_commands`).
//!
//! A command is an id, a title, default shortcuts for this platform and a
//! handler `Fn(&mut App)`. Key dispatch, the menus (native and in-window),
//! the F-key bar and the command palette are all built from the registry, so
//! a feature only registers its commands once, from its own module:
//!
//! ```ignore
//! pub fn register(reg: &mut Registry) {
//!     reg.add(Command::new("files.copy", "Copy", |app| copy(app))
//!         .help("Copy marked files to the other panel")
//!         .keys(&["F5"])
//!         .fkey("F5", "Copy")
//!         .menu(Menu::Files, "ops"));
//! }
//! ```
//!
//! and adds that function to [`crate::features::ALL`].
//!
//! Shortcut strings use `Primary` for ⌘ on macOS / Ctrl elsewhere (see
//! [`Shortcut::parse`]); `.mac_keys()` / `.other_keys()` add platform-only ones.

use std::collections::HashMap;
use std::rc::Rc;

use crate::app::App;
use crate::input::KeyEvent;
use crate::main_window::side::SideViewFactory;
use crate::menus::Menu;
use crate::shortcut::Shortcut;

pub type CommandId = &'static str;
pub type Handler = Rc<dyn Fn(&mut App)>;
pub type Predicate = Rc<dyn Fn(&App) -> bool>;

/// Where macOS puts a command in the application menu.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MenuRole {
    #[default]
    None,
    Quit,
    Preferences,
    About,
}

pub struct Command {
    pub id: CommandId,
    pub title: String,
    pub help: String,
    /// For this platform, in the order given (the first is shown in menus).
    pub shortcuts: Vec<Shortcut>,
    /// (key label, bar label) on the F-key bar.
    pub fkey: Option<(&'static str, &'static str)>,
    /// Label on the right end of the F-key bar, with the first shortcut.
    pub footer_right: Option<&'static str>,
    pub run: Handler,
    /// A check mark in menus.
    pub checked: Option<Predicate>,
    /// Greyed out in menus / skipped by dispatch when false.
    pub enabled: Option<Predicate>,
    pub role: MenuRole,
    /// Listed in the command palette (default true).
    pub in_palette: bool,
    /// Runs even while a dialog is open (e.g. Quit).
    pub global: bool,
    pub(crate) menu: Option<(Menu, &'static str)>,
    /// The label in menus if not the title (e.g. "Files" in the Left menu).
    pub menu_label: Option<String>,
}

impl Command {
    pub fn new(id: CommandId, title: impl Into<String>, run: impl Fn(&mut App) + 'static) -> Self {
        Command {
            id,
            title: title.into(),
            help: String::new(),
            shortcuts: Vec::new(),
            fkey: None,
            footer_right: None,
            run: Rc::new(run),
            checked: None,
            enabled: None,
            role: MenuRole::None,
            in_palette: true,
            global: false,
            menu: None,
            menu_label: None,
        }
    }

    pub fn help(mut self, help: impl Into<String>) -> Self {
        self.help = help.into();
        self
    }

    /// Shortcuts on every platform.
    pub fn keys(mut self, specs: &[&str]) -> Self {
        self.shortcuts.extend(specs.iter().map(|s| parse(s)));
        self
    }

    /// Shortcuts only on macOS.
    pub fn mac_keys(self, specs: &[&str]) -> Self {
        if cfg!(target_os = "macos") { self.keys(specs) } else { self }
    }

    /// Shortcuts only on Windows and Linux.
    pub fn other_keys(self, specs: &[&str]) -> Self {
        if cfg!(target_os = "macos") { self } else { self.keys(specs) }
    }

    pub fn fkey(mut self, key: &'static str, label: &'static str) -> Self {
        self.fkey = Some((key, label));
        self
    }

    pub fn footer_right(mut self, label: &'static str) -> Self {
        self.footer_right = Some(label);
        self
    }

    /// Put it in an NC menu, in a section of it (see [`crate::menus`]).
    pub fn menu(mut self, menu: Menu, section: &'static str) -> Self {
        self.menu = Some((menu, section));
        self
    }

    /// The label in menus when it differs from the title.
    pub fn menu_label(mut self, label: impl Into<String>) -> Self {
        self.menu_label = Some(label.into());
        self
    }

    /// What menus show.
    pub fn label_in_menu(&self) -> &str {
        self.menu_label.as_deref().unwrap_or(&self.title)
    }

    pub fn checked(mut self, f: impl Fn(&App) -> bool + 'static) -> Self {
        self.checked = Some(Rc::new(f));
        self
    }

    pub fn enabled(mut self, f: impl Fn(&App) -> bool + 'static) -> Self {
        self.enabled = Some(Rc::new(f));
        self
    }

    pub fn role(mut self, role: MenuRole) -> Self {
        self.role = role;
        self
    }

    pub fn hidden_from_palette(mut self) -> Self {
        self.in_palette = false;
        self
    }

    pub fn global(mut self) -> Self {
        self.global = true;
        self
    }

    /// The first shortcut in the platform's notation ("⌘R"), or "".
    pub fn shortcut_text(&self) -> String {
        self.shortcuts.first().map(Shortcut::display).unwrap_or_default()
    }

    pub fn is_enabled(&self, app: &App) -> bool {
        self.enabled.as_ref().is_none_or(|f| f(app))
    }

    pub fn is_checked(&self, app: &App) -> Option<bool> {
        self.checked.as_ref().map(|f| f(app))
    }
}

fn parse(spec: &str) -> Shortcut {
    Shortcut::parse(spec).unwrap_or_else(|| panic!("bad shortcut {spec:?}"))
}

/// A function that runs once at startup, after the main window exists.
pub type StartHook = fn(&mut App);
/// Runs after settings changed (`App::update_settings`).
pub type SettingsHook = fn(&mut App);
/// A click on a file panel's path (its frame title): `(app, side, col, row)`.
pub type TitleClickHook = fn(&mut App, usize, u16, u16);
/// Extra entries for the drive / location list (⌥F1 / ⌥F2) of `side`.
pub type LocationSource = fn(&App, usize) -> Vec<LocationChoice>;

/// Opens a [`LocationChoice`] on a side.
pub type OpenLocation = Box<dyn FnOnce(&mut App, usize)>;

/// One extra entry in the drive / location list.
pub struct LocationChoice {
    pub label: String,
    /// Shown dimmed (a path, an address).
    pub hint: String,
    /// Opens it on the given side.
    pub open: OpenLocation,
}

/// Everything features contribute: commands, side views, hooks.
#[derive(Default)]
pub struct Registry {
    commands: Vec<Command>,
    by_id: HashMap<CommandId, usize>,
    by_key: HashMap<Shortcut, CommandId>,
    pub(crate) side_views: Vec<SideViewFactory>,
    pub(crate) on_start: Vec<StartHook>,
    pub(crate) on_settings_changed: Vec<SettingsHook>,
    pub(crate) on_title_click: Option<TitleClickHook>,
    pub(crate) location_sources: Vec<LocationSource>,
    pub(crate) on_shutdown: Vec<StartHook>,
}

impl Registry {
    /// A registry filled by each feature's registration function.
    pub fn build(features: &[fn(&mut Registry)]) -> Registry {
        let mut reg = Registry::default();
        for f in features {
            f(&mut reg);
        }
        reg
    }

    /// Add a command. Ids must be unique; a shortcut already taken by an
    /// earlier command stays with that one (and is logged).
    pub fn add(&mut self, cmd: Command) -> &mut Self {
        assert!(!self.by_id.contains_key(cmd.id), "command {} registered twice", cmd.id);
        for s in &cmd.shortcuts {
            match self.by_key.get(s) {
                Some(other) => log::warn!("{} is already the shortcut of {other}; not for {}", s.display(), cmd.id),
                None => {
                    self.by_key.insert(*s, cmd.id);
                }
            }
        }
        self.by_id.insert(cmd.id, self.commands.len());
        self.commands.push(cmd);
        self
    }

    /// A tab next to "Files" on both sides (e.g. Preview).
    pub fn add_side_view(&mut self, factory: SideViewFactory) -> &mut Self {
        self.side_views.push(factory);
        self
    }

    pub fn on_start(&mut self, hook: StartHook) -> &mut Self {
        self.on_start.push(hook);
        self
    }

    pub fn on_settings_changed(&mut self, hook: SettingsHook) -> &mut Self {
        self.on_settings_changed.push(hook);
        self
    }

    /// What a click on a panel's path does (the bookmark menu); one handler.
    pub fn on_panel_title_click(&mut self, hook: TitleClickHook) -> &mut Self {
        self.on_title_click = Some(hook);
        self
    }

    /// Runs in `App::shutdown`, before the process exits: write what is
    /// still pending (bookmarks.json…). Keep it short.
    pub fn on_shutdown(&mut self, hook: StartHook) -> &mut Self {
        self.on_shutdown.push(hook);
        self
    }

    /// Add entries to the drive / location list (bookmarks, connections…).
    pub fn add_location_source(&mut self, source: LocationSource) -> &mut Self {
        self.location_sources.push(source);
        self
    }

    pub fn get(&self, id: &str) -> Option<&Command> {
        self.by_id.get(id).map(|&i| &self.commands[i])
    }

    pub fn commands(&self) -> &[Command] {
        &self.commands
    }

    /// The command bound to this key, if any.
    pub fn for_key(&self, ev: &KeyEvent) -> Option<&Command> {
        let id = self.by_key.get(&Shortcut::new(ev.key, ev.mods))?;
        self.get(id)
    }

    /// Commands on the F-key bar, in key order (F1…F12).
    pub fn fkeys(&self) -> Vec<&Command> {
        let mut v: Vec<&Command> = self.commands.iter().filter(|c| c.fkey.is_some()).collect();
        v.sort_by_key(|c| c.fkey.and_then(|(k, _)| k.trim_start_matches('F').parse::<u8>().ok()).unwrap_or(99));
        v
    }

    pub fn footer_right(&self) -> Vec<&Command> {
        self.commands.iter().filter(|c| c.footer_right.is_some()).collect()
    }

    /// Commands in `menu`'s `section`, in registration order.
    pub fn in_menu_section(&self, menu: Menu, section: &str) -> Vec<&Command> {
        self.commands.iter().filter(|c| c.menu.is_some_and(|(m, s)| m == menu && s == section)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Key, Mods};

    #[test]
    fn lookup_by_key_and_fkey_order() {
        let mut reg = Registry::default();
        reg.add(Command::new("a.quit", "Quit", |_| {}).keys(&["F10", "Primary+Q"]).fkey("F10", "Quit"));
        reg.add(Command::new("a.copy", "Copy", |_| {}).keys(&["F5"]).fkey("F5", "Copy"));
        reg.add(Command::new("a.dup", "Dup", |_| {}).keys(&["F5"]));
        assert_eq!(reg.for_key(&KeyEvent::new(Key::Char('q'), Mods::primary())).unwrap().id, "a.quit");
        assert_eq!(reg.for_key(&KeyEvent::plain(Key::F(5))).unwrap().id, "a.copy", "first registration wins");
        assert!(reg.for_key(&KeyEvent::plain(Key::F(6))).is_none());
        let order: Vec<_> = reg.fkeys().iter().map(|c| c.id).collect();
        assert_eq!(order, ["a.copy", "a.quit"]);
    }

    #[test]
    #[should_panic(expected = "registered twice")]
    fn duplicate_ids_are_a_bug() {
        let mut reg = Registry::default();
        reg.add(Command::new("x", "X", |_| {}));
        reg.add(Command::new("x", "X", |_| {}));
    }
}
