//! macOS: the native menu bar (built from the command registry with muda),
//! other windows' own menu bars (the editor's File/Edit, swapped in while
//! such a window is focused) and the Dock icon.

use muda::accelerator::{Key as MKey, KeyAccelerator, Modifiers, NamedKey};
use muda::{CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use oc_ui::menus::{self, Menu as NcMenu};
use oc_ui::{App, Command, Key, MenuRole, Shortcut, WindowMenu};

/// The installed menu bar; keeps the items to update check marks.
pub struct NativeMenu {
    _menu: Menu,
    window_menu: Submenu,
    checks: Vec<(&'static str, CheckMenuItem)>,
    items: Vec<(&'static str, MenuItem)>,
}

fn accelerator(s: &Shortcut) -> Option<KeyAccelerator> {
    let mut mods = Modifiers::empty();
    if s.mods.logo {
        mods |= Modifiers::META;
    }
    if s.mods.ctrl {
        mods |= Modifiers::CONTROL;
    }
    if s.mods.alt {
        mods |= Modifiers::ALT;
    }
    if s.mods.shift {
        mods |= Modifiers::SHIFT;
    }
    let key = match s.key {
        Key::Char(c) => MKey::Character(c.to_string()),
        Key::Enter => MKey::Named(NamedKey::Enter),
        Key::Escape => MKey::Named(NamedKey::Escape),
        Key::Tab => MKey::Named(NamedKey::Tab),
        Key::Backspace => MKey::Named(NamedKey::Backspace),
        Key::Delete => MKey::Named(NamedKey::Delete),
        Key::Insert => MKey::Named(NamedKey::Insert),
        Key::Home => MKey::Named(NamedKey::Home),
        Key::End => MKey::Named(NamedKey::End),
        Key::PageUp => MKey::Named(NamedKey::PageUp),
        Key::PageDown => MKey::Named(NamedKey::PageDown),
        Key::Up => MKey::Named(NamedKey::ArrowUp),
        Key::Down => MKey::Named(NamedKey::ArrowDown),
        Key::Left => MKey::Named(NamedKey::ArrowLeft),
        Key::Right => MKey::Named(NamedKey::ArrowRight),
        Key::Space => MKey::Character(" ".into()),
        Key::F(n) => MKey::Named(match n {
            1 => NamedKey::F1,
            2 => NamedKey::F2,
            3 => NamedKey::F3,
            4 => NamedKey::F4,
            5 => NamedKey::F5,
            6 => NamedKey::F6,
            7 => NamedKey::F7,
            8 => NamedKey::F8,
            9 => NamedKey::F9,
            10 => NamedKey::F10,
            11 => NamedKey::F11,
            12 => NamedKey::F12,
            _ => return None,
        }),
        Key::Other => return None,
    };
    Some(KeyAccelerator::new(mods, key))
}

/// Native menu items forward their command; with an accelerator, macOS
/// handles the key itself (`App::menu_key_equivalent` routes it like a key
/// press; a click is `App::menu_activated`).
/// The item shows the command's first shortcut, as 1.x's Qt menus did (F5,
/// ⌥F7, ⌃`); one without ⌘ reaches the window as the key it is, so a
/// focused view (the terminal) still gets it.
fn native_accelerator(cmd: &Command) -> Option<KeyAccelerator> {
    cmd.menu_shortcut().as_ref().and_then(accelerator)
}

impl NativeMenu {
    /// `on_command(id, by_key)`: a menu item was chosen, by its key
    /// equivalent (`by_key`) or with the mouse.
    pub fn install(app: &App, on_command: impl Fn(String, bool) + Send + Sync + 'static) -> Option<NativeMenu> {
        let menu = Menu::new();
        let mut checks = Vec::new();
        let mut items = Vec::new();
        let reg = app.registry.clone();

        let mut make = |cmd: &Command| -> Box<dyn IsMenuItem> {
            let label = cmd.label_in_menu().replace('&', "&&");
            let accel = native_accelerator(cmd);
            match cmd.is_checked(app) {
                Some(checked) => {
                    let item = CheckMenuItem::with_id(cmd.id, &label, cmd.is_enabled(app), checked, None);
                    let _ = item.set_key_accelerator(accel);
                    checks.push((cmd.id, item.clone()));
                    Box::new(item)
                }
                None => {
                    let item = MenuItem::with_id(cmd.id, &label, cmd.is_enabled(app), None);
                    let _ = item.set_key_accelerator(accel);
                    items.push((cmd.id, item.clone()));
                    Box::new(item)
                }
            }
        };

        // The application menu: About, Settings, Services, Hide…, Quit.
        let app_menu = Submenu::new(oc_config::APP_NAME, true);
        // Ours lists the licenses too; AppKit's panel only if there is none.
        match reg.commands().iter().find(|c| c.role == MenuRole::About) {
            Some(about) => {
                let _ = app_menu.append(make(about).as_ref());
            }
            None => {
                let _ = app_menu.append(&PredefinedMenuItem::about(None, None));
            }
        }
        let prefs: Vec<&Command> = reg.commands().iter().filter(|c| c.role == MenuRole::Preferences).collect();
        if !prefs.is_empty() {
            let _ = app_menu.append(&PredefinedMenuItem::separator());
            for c in prefs {
                let _ = app_menu.append(make(c).as_ref());
            }
        }
        for item in [
            &PredefinedMenuItem::separator() as &dyn IsMenuItem,
            &PredefinedMenuItem::services(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
        ] {
            let _ = app_menu.append(item);
        }
        if let Some(quit) = reg.commands().iter().find(|c| c.role == MenuRole::Quit) {
            let _ = app_menu.append(make(quit).as_ref());
        }
        let _ = menu.append(&app_menu);

        for nc in NcMenu::ALL {
            let title = oc_ui::widgets::parse_hotkey(nc.title()).0;
            let sub = Submenu::new(title, true);
            for id in menus::menu_items(app, nc) {
                match id.and_then(|id| reg.get(id)) {
                    // macOS shows Quit/Settings in the app menu only.
                    Some(cmd) if cmd.role != MenuRole::None => {}
                    Some(cmd) => {
                        let _ = sub.append(make(cmd).as_ref());
                    }
                    None => {
                        let _ = sub.append(&PredefinedMenuItem::separator());
                    }
                }
            }
            let _ = menu.append(&sub);
        }
        let window_menu = Submenu::new("Window", true);
        let _ = window_menu.append(&PredefinedMenuItem::minimize(None));
        let _ = window_menu.append(&PredefinedMenuItem::fullscreen(None));
        let _ = menu.append(&window_menu);
        window_menu.set_as_windows_menu_for_nsapp();

        menu.init_for_nsapp();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| on_command(e.id.0.clone(), triggered_by_key())));
        Some(NativeMenu { _menu: menu, window_menu, checks, items })
    }

    /// Make it the menu bar again (the main window got the focus back).
    pub fn activate(&self) {
        self._menu.init_for_nsapp();
        self.window_menu.set_as_windows_menu_for_nsapp();
    }

    /// Refresh check marks and enabled states.
    pub fn update(&self, app: &App) {
        for (id, item) in &self.checks {
            if let Some(cmd) = app.registry.get(id) {
                item.set_checked(cmd.is_checked(app).unwrap_or(false));
                item.set_enabled(cmd.is_enabled(app));
            }
        }
        for (id, item) in &self.items {
            if let Some(cmd) = app.registry.get(id) {
                item.set_enabled(cmd.is_enabled(app));
            }
        }
    }
}

/// The menu bar of a window with its own menus (1.x: the editor's
/// QMenuBar): the application menu, the window's menus, Window. Its items
/// reach the window as their shortcuts (`App::menu_activated`), and since
/// the main window's menus are gone meanwhile, keys like ⌘Z or ⌘F stay
/// with the editor.
pub struct WindowMenuBar {
    _menu: Menu,
    items: Vec<(String, MenuItem)>,
}

impl WindowMenuBar {
    pub fn install(app: &App, menus: &[WindowMenu]) -> Option<WindowMenuBar> {
        let menu = Menu::new();
        let app_menu = Submenu::new(oc_config::APP_NAME, true);
        for item in [
            &PredefinedMenuItem::about(None, None) as &dyn IsMenuItem,
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::services(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
        ] {
            let _ = app_menu.append(item);
        }
        if let Some(quit) = app.registry.commands().iter().find(|c| c.role == MenuRole::Quit) {
            let item = MenuItem::with_id(quit.id, quit.label_in_menu(), true, None);
            let _ = item.set_key_accelerator(native_accelerator(quit));
            let _ = app_menu.append(&item);
        }
        let _ = menu.append(&app_menu);
        let mut items = Vec::new();
        for m in menus {
            let sub = Submenu::new(&m.title, true);
            for entry in &m.items {
                match entry {
                    Some(it) => {
                        let item = MenuItem::with_id(it.id.as_str(), it.label.replace('&', "&&"), it.enabled, None);
                        let accel = it.shortcut.filter(|s| s.mods.logo).as_ref().and_then(accelerator);
                        let _ = item.set_key_accelerator(accel);
                        let _ = sub.append(&item);
                        items.push((it.id.clone(), item));
                    }
                    None => {
                        let _ = sub.append(&PredefinedMenuItem::separator());
                    }
                }
            }
            let _ = menu.append(&sub);
        }
        let window_menu = Submenu::new("Window", true);
        let _ = window_menu.append(&PredefinedMenuItem::minimize(None));
        let _ = window_menu.append(&PredefinedMenuItem::fullscreen(None));
        let _ = menu.append(&window_menu);
        window_menu.set_as_windows_menu_for_nsapp();
        menu.init_for_nsapp();
        Some(WindowMenuBar { _menu: menu, items })
    }

    /// Refresh enabled states (Undo/Redo…).
    pub fn update(&self, menus: &[WindowMenu]) {
        for it in menus.iter().flat_map(|m| m.items.iter()).flatten() {
            if let Some((_, item)) = self.items.iter().find(|(id, _)| *id == it.id) {
                item.set_enabled(it.enabled);
            }
        }
    }
}

/// Whether the menu action being handled comes from a key equivalent (the
/// event AppKit is dispatching is a key press) rather than a click.
fn triggered_by_key() -> bool {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSEventType};

    let Some(mtm) = MainThreadMarker::new() else { return false };
    let app = NSApplication::sharedApplication(mtm);
    app.currentEvent().is_some_and(|e| e.r#type() == NSEventType::KeyDown)
}

/// Whether the wheel event AppKit is dispatching is trackpad momentum (the
/// fingers have lifted). winit reports its phase only as Started/Moved/Ended,
/// the same as for the fingers' own scrolling.
pub fn scroll_is_momentum() -> bool {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSEventType};

    let Some(mtm) = MainThreadMarker::new() else { return false };
    let app = NSApplication::sharedApplication(mtm);
    app.currentEvent().is_some_and(|e| e.r#type() == NSEventType::ScrollWheel && e.momentumPhase().0 != 0)
}

/// The Dock icon (a bare binary has none; a bundle gets it from Info.plist,
/// this covers `cargo run` and keeps the 1.x look).
pub fn set_app_icon() {
    use objc2::AllocAnyThread;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let Some(mtm) = MainThreadMarker::new() else { return };
    let bytes = include_bytes!("../resources/icon-macos.png");
    let data = NSData::with_bytes(bytes);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else { return };
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: called on the main thread with a valid image.
    unsafe { app.setApplicationIconImage(Some(&image)) };
}
