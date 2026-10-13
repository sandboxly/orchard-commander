//! The F4 text editor (port of `ui/editor.py` + `textfile.py`): F4 edits the
//! file under the cursor, Shift+F4 creates a file and edits it. Each file
//! gets its own top-level window ([`EditorWindow`], a `UiWindow`); a second
//! F4 on the same file brings its window to the front.
//!
//! * [`buffer`] — the rope, cursor, selection, undo/redo.
//! * [`highlight`] — incremental syntect highlighting on a worker.
//! * [`find`] — search (case-insensitive unless Match case).
//! * [`io`] — reading/decoding, atomic local saves, `open_write` elsewhere.
//! * [`window`] — keys, find bar, load/save/watch, the unsaved-changes question.
//! * [`draw`] — drawing it.
//!
//! Quitting asks every editor with unsaved changes first (`App::quit` →
//! `close_requested`), like 1.x's `MainWindow._editors`.

pub mod buffer;
pub mod draw;
pub mod find;
pub mod highlight;
pub mod io;
pub mod window;

use oc_vfs::VPath;

use crate::app::{App, PlatformRequest, WinId};
use crate::commands::{Command, Registry};
use crate::dialogs;
use crate::panel::{self, LoadOptions};
pub use window::{Action, EditorWindow, Focus};

/// Above this size F4 asks first (1.x: 20 MB).
pub const ASK_ABOVE_BYTES: u64 = 20 * 1024 * 1024;

/// The open editors: window and file (1.x `MainWindow._editors`).
#[derive(Default)]
pub struct EditorState {
    pub windows: Vec<(WinId, VPath)>,
}

pub fn register(reg: &mut Registry) {
    reg.add(
        Command::new("files.edit", "Edit", edit_file)
            .help("Edit the file under the cursor in the text editor")
            .keys(&["F4"])
            .fkey("F4", "Edit"),
    );
    reg.add(
        Command::new("files.new_file", "New file", new_text_file)
            .help("Create a text file in the active panel and edit it")
            .keys(&["Shift+F4"]),
    );
}

/// F4.
pub fn edit_file(app: &mut App) {
    let side = app.main.files_side();
    let p = &app.main.sides[side].panel;
    let Some(entry) = p.cursor_entry().cloned() else {
        return;
    };
    if entry.name == ".." {
        return;
    }
    if entry.is_dir {
        app.warn("F4 edits files; this is a folder");
        return;
    }
    let Some(target) = p.cursor_target() else {
        return;
    };
    if entry.size > ASK_ABOVE_BYTES {
        let text = format!("{} is {}. Open it in the editor anyway?", target.name(), oc_vfs::human_size(entry.size));
        dialogs::confirm(app, "Large file", &text, "Open", move |app| {
            open_editor(app, target);
        });
        return;
    }
    open_editor(app, target);
}

/// Open `location` in an editor window, or bring its window to the front.
pub fn open_editor(app: &mut App, location: VPath) -> WinId {
    let existing = app.state::<EditorState>().windows.iter().find(|(_, l)| *l == location).map(|(id, _)| *id);
    if let Some(id) = existing {
        app.request(PlatformRequest::RaiseWindow(id));
        return id;
    }
    if !location.fs.capabilities().writable {
        app.warn(format!("{}: the file opens read-only", location.fs.read_only_reason()));
    }
    let id = app.open_window(Box::new(EditorWindow::new(location.clone())), window::COLS, window::ROWS);
    app.state::<EditorState>().windows.push((id, location));
    window::with_editor(app, id, |w, app| w.load(app, id));
    id
}

/// Shift+F4: ask for a name, create the file, list it, edit it.
pub fn new_text_file(app: &mut App) {
    let side = app.main.files_side();
    let folder = app.main.sides[side].panel.location.clone();
    if !folder.fs.capabilities().writable {
        app.warn(format!("Can't create a file here: {}", folder.fs.read_only_reason()));
        return;
    }
    let message = format!("Create in {}:", folder.display());
    dialogs::ask_text(app, "New file", &message, "", "Create", move |app, name| {
        let Some(name) = name else { return };
        let dir = folder.clone();
        app.tasks.run_in_background(
            move |_| io::new_file(&dir, &name).map_err(|e| (name, e)),
            move |app, result| match result {
                Ok(location) => {
                    let name = location.name();
                    if app.main.sides[side].panel.location == folder {
                        panel::load(app, side, folder, LoadOptions { select: Some(name), ..Default::default() });
                    }
                    open_editor(app, location);
                }
                Err((name, e)) => app.warn(format!("Can't create {name}: {e}")),
            },
        );
    });
}

/// The editor windows with unsaved changes (for tests and the quit question).
pub fn unsaved(app: &App) -> Vec<WinId> {
    app.try_state::<EditorState>()
        .map(|s| s.windows.iter().map(|(id, _)| *id).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|id| app.window::<EditorWindow>(*id).is_some_and(|w| w.modified()))
        .collect()
}
