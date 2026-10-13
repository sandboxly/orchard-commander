//! Driving an [`App`] without a window: for tests, `--screenshot` and
//! benchmarks. The app's tasks are processed by polling.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_config::ConfigPaths;

use crate::app::{App, AppConfig, MAIN_WINDOW};
use crate::clipboard::MemoryClipboard;
use crate::input::{Event, Key, KeyEvent, Mods};

/// An app on `left` and `right` with its config in `config_dir`, no folder
/// watching and an in-memory clipboard.
pub fn headless_app(config_dir: &Path, left: &Path, right: &Path) -> App {
    let mut cfg = AppConfig::new(ConfigPaths::new(config_dir), Arc::new(|| {}));
    cfg.left = Some(left.to_path_buf());
    cfg.right = Some(right.to_path_buf());
    cfg.clipboard = Box::new(MemoryClipboard::default());
    cfg.watch = false;
    let mut app = App::new(cfg);
    // Never the real keychain from tests.
    crate::connections::use_credential_store(&mut app, Arc::new(oc_config::MemoryCredentials::new()));
    app
}

/// Process tasks and timers until `done(app)` or `timeout` (then panics
/// with `what`).
pub fn wait_until(app: &mut App, what: &str, timeout: Duration, mut done: impl FnMut(&App) -> bool) {
    let end = Instant::now() + timeout;
    loop {
        app.process_tasks();
        app.tick(Instant::now());
        if done(app) {
            return;
        }
        if Instant::now() > end {
            panic!("timed out waiting for {what}; dialogs: {:?}", app.modal_titles());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Wait until both panels have their listings and no background job is pending.
pub fn settle(app: &mut App) {
    wait_until(app, "listings", Duration::from_secs(20), |app| {
        app.tasks.pending() == 0 && app.main.sides.iter().all(|s| !s.panel.loading())
    });
}

pub fn press(app: &mut App, key: Key) {
    app.handle_event(MAIN_WINDOW, Event::Key(KeyEvent::plain(key)));
}

pub fn press_with(app: &mut App, key: Key, mods: Mods) {
    app.handle_event(MAIN_WINDOW, Event::Key(KeyEvent::new(key, mods)));
}

pub fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_event(MAIN_WINDOW, Event::Key(KeyEvent::typed(&c.to_string())));
    }
}
