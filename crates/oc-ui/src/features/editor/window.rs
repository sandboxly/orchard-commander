//! The editor window (port of `ui/editor.py`'s `EditorWindow`): one
//! top-level window per file, its keys, the find bar, loading, saving,
//! watching the file and the unsaved-changes question.

use std::any::Any;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use oc_grid::Frame;
use oc_preview::highlight::{self as syn, LineHighlighter};
use oc_preview::textfile::{Encoding, TextFile};
use oc_vfs::VPath;

use super::buffer::Buffer;
use super::highlight::{Highlight, MAX_HIGHLIGHT_CHARS};
use super::io::{self, Loaded};
use super::{EditorState, find};
use crate::app::{App, Outcome, PlatformRequest, Severity, UiWindow, WinId, WindowMenu, WindowMenuItem};
use crate::dialogs::{InputBox, MessageBox};
use crate::geom::Rect;
use crate::input::{Event, Key, KeyEvent, MouseButton, MouseKind};
use crate::shortcut::Shortcut;
use crate::widgets::{Checkbox, DrawCtx, TextInput};

/// Debounce for outside changes (1.x: 300 ms).
const RELOAD_DELAY: Duration = Duration::from_millis(300);

/// Window size in cells when it opens (1.x: 100 x 40 characters).
pub const COLS: u16 = 100;
pub const ROWS: u16 = 40;

/// The editor's commands: the keys, and on macOS the window's File/Edit menus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Save,
    Close,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    Find,
    FindNext,
    FindPrevious,
    Replace,
    GoToLine,
    Escape,
}

impl Action {
    pub const ALL: [Action; 14] = [
        Action::Save,
        Action::Close,
        Action::Undo,
        Action::Redo,
        Action::Cut,
        Action::Copy,
        Action::Paste,
        Action::SelectAll,
        Action::Find,
        Action::FindNext,
        Action::FindPrevious,
        Action::Replace,
        Action::GoToLine,
        Action::Escape,
    ];

    /// The menu title (1.x's action titles).
    pub fn title(self) -> &'static str {
        match self {
            Action::Save => "Save",
            Action::Close => "Close",
            Action::Undo => "Undo",
            Action::Redo => "Redo",
            Action::Cut => "Cut",
            Action::Copy => "Copy",
            Action::Paste => "Paste",
            Action::SelectAll => "Select all",
            Action::Find => "Find",
            Action::FindNext => "Find next",
            Action::FindPrevious => "Find previous",
            Action::Replace => "Replace",
            Action::GoToLine => "Go to line",
            Action::Escape => "Escape",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Action::Save => "editor.save",
            Action::Close => "editor.close",
            Action::Undo => "editor.undo",
            Action::Redo => "editor.redo",
            Action::Cut => "editor.cut",
            Action::Copy => "editor.copy",
            Action::Paste => "editor.paste",
            Action::SelectAll => "editor.select_all",
            Action::Find => "editor.find",
            Action::FindNext => "editor.find_next",
            Action::FindPrevious => "editor.find_previous",
            Action::Replace => "editor.replace",
            Action::GoToLine => "editor.go_to_line",
            Action::Escape => "editor.escape",
        }
    }

    /// The platform's keys (Qt's standard keys plus 1.x's extras).
    pub fn shortcuts(self) -> Vec<Shortcut> {
        let mac = cfg!(target_os = "macos");
        let specs: Vec<&str> = match self {
            Action::Save => vec!["Primary+S"],
            Action::Close => vec!["Primary+W"],
            Action::Undo => vec!["Primary+Z"],
            Action::Redo if mac => vec!["Primary+Shift+Z"],
            Action::Redo => vec!["Primary+Y", "Primary+Shift+Z"],
            Action::Cut if mac => vec!["Primary+X"],
            Action::Cut => vec!["Primary+X", "Shift+Delete"],
            Action::Copy if mac => vec!["Primary+C"],
            Action::Copy => vec!["Primary+C", "Primary+Insert"],
            Action::Paste if mac => vec!["Primary+V"],
            Action::Paste => vec!["Primary+V", "Shift+Insert"],
            Action::SelectAll => vec!["Primary+A"],
            Action::Find => vec!["Primary+F"],
            Action::FindNext if mac => vec!["Primary+G", "F3"],
            Action::FindNext => vec!["F3", "Primary+G"],
            Action::FindPrevious if mac => vec!["Primary+Shift+G", "Shift+F3"],
            Action::FindPrevious => vec!["Shift+F3", "Primary+Shift+G"],
            Action::Replace => vec!["Primary+Alt+F"],
            Action::GoToLine => vec!["Primary+L", "Primary+Alt+L"],
            Action::Escape => vec!["Escape"],
        };
        specs.into_iter().filter_map(Shortcut::parse).collect()
    }

    pub fn for_key(k: &KeyEvent) -> Option<Action> {
        Action::ALL.into_iter().find(|a| a.shortcuts().iter().any(|s| s.matches(k)))
    }
}

/// Where the keyboard focus is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Text,
    Find,
    Replace,
    MatchCase,
}

/// The find bar below the text (1.x: hidden until ⌘F).
pub struct FindBar {
    pub visible: bool,
    pub replace: bool,
    pub find: TextInput,
    pub with: TextInput,
    pub match_case: Checkbox,
    pub status: String,
}

impl Default for FindBar {
    fn default() -> Self {
        let mut find = TextInput::new("");
        find.placeholder = "Find".into();
        let mut with = TextInput::new("");
        with.placeholder = "Replace with".into();
        FindBar { visible: false, replace: false, find, with, match_case: Checkbox::new("Match case", false), status: String::new() }
    }
}

/// The parts of the window, computed from its size (drawing and clicks).
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub frame: Rect,
    pub gutter: Rect,
    pub code: Rect,
    pub bar: Option<Rect>,
}

/// The find bar's parts.
pub struct BarLayout {
    pub find: Rect,
    pub with: Option<Rect>,
    pub replace_all: Option<Rect>,
    pub match_case: Rect,
    pub status: Rect,
}

pub const REPLACE_ALL: &str = " Replace all ";

pub struct EditorWindow {
    pub location: VPath,
    pub buf: Buffer,
    pub hl: Highlight,
    /// Encoding, BOM and line endings to write back (`text` unused).
    pub file: TextFile,
    pub language: String,
    pub read_only: bool,
    pub loaded: bool,
    pub load_failed: bool,
    pub saving: bool,
    /// Shown in the status line instead of the position ("loading…").
    pub note: Option<String>,
    /// The file is gone from the disk (shown in the status line until it is
    /// back or saved again).
    pub deleted_on_disk: bool,
    pub top: usize,
    pub left: usize,
    pub focus: Focus,
    pub bar: FindBar,
    pub preedit: String,
    dragging: bool,
    /// Keep the cursor in view after this event.
    follow: bool,
    /// The user chose "Don't save": close without asking again.
    discard: bool,
    /// "Save" in the unsaved-changes question while a save was already
    /// running: close when that one is done.
    close_after_save: bool,
    /// The unsaved-changes question is open.
    asking: bool,
    /// A changed-on-disk question is open.
    asking_reload: bool,
    title_sent: String,
    /// What the native menus' enabled states were last built from.
    menu_state: Vec<bool>,
    /// (mtime, size) after our last load/save, to tell outside changes apart.
    own_signature: Option<(SystemTime, u64)>,
    watch: Option<FileWatch>,
    watch_wanted: bool,
    reload_timer: Option<u64>,
}

/// Watches the file's folder (editors replace files by renaming, so the
/// file itself may be a new inode afterwards).
pub struct FileWatch {
    _watcher: notify::RecommendedWatcher,
}

impl EditorWindow {
    pub fn new(location: VPath) -> Self {
        let read_only = !location.fs.capabilities().writable;
        EditorWindow {
            read_only,
            buf: Buffer::new(""),
            hl: Highlight::new(None, 1),
            file: TextFile { text: String::new(), encoding: Encoding::Utf8, bom: false, newline: Default::default() },
            language: "Plain text".into(),
            loaded: false,
            load_failed: false,
            saving: false,
            note: Some("loading…".into()),
            deleted_on_disk: false,
            top: 0,
            left: 0,
            focus: Focus::Text,
            bar: FindBar::default(),
            preedit: String::new(),
            dragging: false,
            follow: false,
            discard: false,
            close_after_save: false,
            asking: false,
            asking_reload: false,
            title_sent: String::new(),
            menu_state: Vec::new(),
            own_signature: None,
            watch: None,
            watch_wanted: false,
            reload_timer: None,
            location,
        }
    }

    pub fn name(&self) -> String {
        let n = self.location.name();
        if n.is_empty() { self.location.display() } else { n }
    }

    pub fn modified(&self) -> bool {
        self.buf.modified()
    }

    /// The frame's title: "● ~/src/app.py".
    pub fn frame_title(&self) -> String {
        self.frame_title_within(usize::MAX)
    }

    /// The title in at most `max` cells: a long path loses its start, the
    /// unsaved mark stays.
    pub fn frame_title_within(&self, max: usize) -> String {
        let mark = if self.modified() { "● " } else { "" };
        let path = short_display(&self.location);
        format!("{mark}{}", oc_grid::elide_left(&path, max.saturating_sub(oc_grid::str_width(mark))))
    }

    /// The status line: "Ln 14, Col 15 · UTF-8 · LF · Python · read-only".
    pub fn status(&self) -> String {
        if let Some(n) = &self.note {
            return n.clone();
        }
        if !self.loaded {
            return String::new();
        }
        let (line, col) = self.buf.line_col(self.buf.cursor);
        let mut parts = vec![format!("Ln {}, Col {}", line + 1, col + 1), self.file.description(), self.language.clone()];
        if self.read_only {
            parts.push("read-only".into());
        }
        if self.deleted_on_disk {
            parts.push("deleted on disk".into());
        }
        parts.join(" · ")
    }

    pub fn layout(&self, screen: Rect) -> Layout {
        let inner = screen.inset(1, 1);
        let (text, bar) = if self.bar.visible && inner.h > 2 {
            let (t, b) = inner.split_bottom(1);
            (t, Some(b))
        } else {
            (inner, None)
        };
        let digits = self.buf.len_lines().to_string().len().max(3) as u16;
        let gw = (digits + 2).min(text.w / 2);
        let (gutter, rest) = text.split_left(gw);
        // The last column is the scrollbar's.
        let code = Rect::new(rest.x, rest.y, rest.w.saturating_sub(1), rest.h);
        Layout { frame: screen, gutter, code, bar }
    }

    pub fn bar_layout(&self, bar: Rect) -> BarLayout {
        let status_w = 16.min(bar.w / 4);
        let check_w = 14u16;
        let button_w = REPLACE_ALL.len() as u16;
        let replace = self.bar.replace;
        let fixed = check_w + status_w + 2 + if replace { button_w + 2 } else { 0 };
        let fields = bar.w.saturating_sub(fixed);
        let (fw, ww) = if replace { (fields / 2, fields - fields / 2 - 1) } else { (fields, 0) };
        let mut x = bar.x;
        let find = Rect::new(x, bar.y, fw, 1);
        x += fw + 1;
        let (with, replace_all) = if replace {
            let w = Rect::new(x, bar.y, ww, 1);
            x += ww + 1;
            let b = Rect::new(x, bar.y, button_w, 1);
            x += button_w + 2;
            (Some(w), Some(b))
        } else {
            (None, None)
        };
        let match_case = Rect::new(x, bar.y, check_w.min(bar.right().saturating_sub(x)), 1);
        x += check_w + 1;
        let status = Rect::new(x.min(bar.right()), bar.y, bar.right().saturating_sub(x), 1);
        BarLayout { find, with, replace_all, match_case, status }
    }

    // -- after every change ------------------------------------------------------

    /// Highlighting, scrolling, title and menus after an event or a task result.
    fn settle(&mut self, app: &mut App, id: WinId) {
        let changes = self.buf.take_changes();
        if !changes.is_empty() {
            self.hl.apply_changes(&changes, self.buf.len_lines());
        }
        self.kick_highlighting(app, id);
        if std::mem::take(&mut self.follow) {
            self.ensure_visible(app.screen(id));
        }
        self.clamp_scroll(app.screen(id));
        let title = self.title();
        if title != self.title_sent {
            self.title_sent = title.clone();
            app.request(PlatformRequest::SetTitle(id, title));
        }
        let menu_state: Vec<bool> = Action::ALL.iter().map(|a| self.enabled(*a)).collect();
        if menu_state != self.menu_state {
            self.menu_state = menu_state;
            app.request(PlatformRequest::MenusChanged);
        }
    }

    fn kick_highlighting(&mut self, app: &mut App, id: WinId) {
        let tasks = app.tasks.clone();
        self.hl.kick(self.buf.rope(), &tasks, move |app, res| {
            with_editor(app, id, |w, app| {
                w.hl.apply(res);
                w.settle(app, id);
            });
        });
    }

    fn ensure_visible(&mut self, screen: Rect) {
        let l = self.layout(screen);
        let (h, w) = (l.code.h.max(1) as usize, l.code.w.max(1) as usize);
        let line = self.buf.line_of(self.buf.cursor);
        if line < self.top {
            self.top = line;
        } else if line >= self.top + h {
            self.top = line + 1 - h;
        }
        let col = self.buf.display_col(self.buf.cursor);
        if col < self.left {
            self.left = col.saturating_sub(w / 4);
        } else if col >= self.left + w {
            self.left = col + 1 - w + w / 4;
        }
    }

    fn clamp_scroll(&mut self, screen: Rect) {
        let h = self.layout(screen).code.h.max(1) as usize;
        let max_top = self.buf.len_lines().saturating_sub(h);
        self.top = self.top.min(max_top);
    }

    /// Put `line` (0-based) in the middle of the view, cursor at its start.
    pub fn go_to_line(&mut self, screen: Rect, line: usize) {
        let line = line.min(self.buf.len_lines() - 1);
        self.buf.move_to(self.buf.line_start(line), false);
        let h = self.layout(screen).code.h.max(1) as usize;
        self.top = line.saturating_sub(h / 2);
        self.left = 0;
        self.focus = Focus::Text;
    }

    // -- loading -----------------------------------------------------------------

    /// Read the file (again) in the background.
    pub fn load(&mut self, app: &mut App, id: WinId) {
        let location = self.location.clone();
        app.tasks.run_in_background(
            move |_| {
                let loaded = prepare(&location, io::load(&location));
                let sig = location.local_path().and_then(|p| io::signature(&p));
                (loaded, sig)
            },
            move |app, (loaded, sig)| {
                with_editor(app, id, |w, app| {
                    w.loaded_file(app, id, loaded, sig);
                    w.settle(app, id);
                });
            },
        );
    }

    fn loaded_file(&mut self, app: &mut App, id: WinId, loaded: Prepared, sig: Option<(SystemTime, u64)>) {
        let name = self.name();
        self.note = None;
        match loaded {
            Prepared::Text { file, rope, language, hl } => {
                let first = !self.loaded;
                self.buf.set_rope(rope);
                if first {
                    self.language = language;
                    self.hl = Highlight::new(hl, self.buf.len_lines());
                    self.buf.take_changes();
                }
                self.file = file;
                self.loaded = true;
                self.load_failed = false;
                // Again after a failed (re)load, e.g. the file was briefly unreadable.
                self.read_only = !self.location.fs.capabilities().writable;
                self.own_signature = sig;
            }
            Prepared::Binary => self.failed(format!("Can't open {name}: it isn't a text file (try the hex view, F3)")),
            Prepared::Failed(e) => self.failed(format!("Can't open {name}: {e}")),
        }
        self.watch_file(app, id);
        app.redraw();
    }

    fn failed(&mut self, text: String) {
        self.buf.set_text(&text);
        self.buf.take_changes();
        self.hl = Highlight::new(None, self.buf.len_lines());
        self.read_only = true;
        self.load_failed = true;
        self.loaded = false;
    }

    // -- saving ------------------------------------------------------------------

    /// ⌘S. With `then_close`, the window closes once saved (the question).
    pub fn save(&mut self, app: &mut App, id: WinId, then_close: bool) {
        if self.saving {
            // ⌘S, then the close question's "Save" before it finished.
            self.close_after_save |= then_close;
            return;
        }
        if self.read_only || !self.loaded {
            if then_close {
                app.quit_pending = false; // nothing will close: don't quit later by surprise
            }
            return;
        }
        let text = self.buf.text();
        let data = match self.file.encode(Some(&text)) {
            Ok(d) => d,
            Err(e) => {
                let enc = self.file.encoding.display_name();
                let msg = format!(
                    "Some characters can't be written as {enc} ({}).\n\nSave the file as UTF-8 instead?",
                    py_repr(&unencodable_run(&self.file, &text, e.ch))
                );
                let mb = MessageBox::new("Save as UTF-8", &msg, &["Save", "Cancel"], 0).width(60).on_choice(move |app, choice| {
                    with_editor(app, id, |w, app| {
                        if choice == Some(0) {
                            w.file.encoding = Encoding::Utf8;
                            w.file.bom = false;
                            w.save(app, id, then_close);
                        } else if then_close {
                            app.quit_pending = false;
                        }
                        w.settle(app, id);
                    });
                });
                app.push_modal(id, Box::new(mb));
                return;
            }
        };
        self.saving = true;
        self.note = Some("saving…".into());
        self.buf.seal();
        let point = self.buf.save_point();
        let location = self.location.clone();
        app.tasks.run_in_background(
            move |_| {
                io::save(&location, &data)?;
                Ok::<_, String>(location.local_path().and_then(|p| io::signature(&p)))
            },
            move |app, result| {
                let mut close = false;
                let ok = result.is_ok();
                with_editor(app, id, |w, app| {
                    w.saving = false;
                    w.note = None;
                    let want_close = then_close || std::mem::take(&mut w.close_after_save);
                    match result {
                        Ok(sig) => {
                            w.own_signature = sig;
                            w.deleted_on_disk = false;
                            w.buf.mark_clean_at(point);
                            close = want_close && !w.modified();
                            if want_close && !close {
                                // Typed on while saving: the window stays, so does the app.
                                app.quit_pending = false;
                            }
                        }
                        Err(e) => {
                            let text = format!("Couldn't save {}:\n{e}", w.name());
                            app.push_modal(id, Box::new(MessageBox::new("Not saved", &text, &["OK"], 0).severity(Severity::Error)));
                            app.quit_pending = false;
                        }
                    }
                    w.settle(app, id);
                });
                if ok {
                    crate::features::navigation::reload_all(app);
                }
                if close {
                    close_window(app, id);
                }
            },
        );
    }

    // -- watching ------------------------------------------------------------------

    fn watch_file(&mut self, app: &mut App, id: WinId) {
        if self.watch_wanted || !app.watch || !self.location.fs.capabilities().watchable {
            return;
        }
        let Some(path) = self.location.local_path() else {
            return;
        };
        self.watch_wanted = true;
        let tasks = app.tasks.clone();
        app.tasks.run_in_background(
            move |_| start_watch(path, tasks, id),
            move |app, watch| {
                let stale = match app.window_mut::<EditorWindow>(id) {
                    Some(w) => {
                        w.watch = watch;
                        None
                    }
                    None => watch,
                };
                if let Some(stale) = stale {
                    app.tasks.spawn(move || drop(stale), |_, ()| {});
                }
            },
        );
    }

    /// Something changed in the folder: look again after a pause.
    fn file_event(&mut self, app: &mut App, id: WinId) {
        if let Some(t) = self.reload_timer.take() {
            app.cancel_timer(t);
        }
        self.reload_timer = Some(app.schedule(RELOAD_DELAY, move |app| {
            with_editor(app, id, |w, app| {
                w.reload_timer = None;
                w.check_disk(app, id);
            });
        }));
    }

    fn check_disk(&mut self, app: &mut App, id: WinId) {
        let Some(path) = self.location.local_path() else {
            return;
        };
        // A file that failed to load is tried again when it changes.
        if self.saving || !(self.loaded || self.load_failed) {
            return;
        }
        app.tasks.run_in_background(
            move |_| io::signature(&path),
            move |app, sig| {
                with_editor(app, id, |w, app| w.disk_state(app, id, sig));
            },
        );
    }

    fn disk_state(&mut self, app: &mut App, id: WinId, sig: Option<(SystemTime, u64)>) {
        if self.saving {
            return;
        }
        self.deleted_on_disk = sig.is_none();
        let Some(sig) = sig else {
            return;
        };
        if Some(sig) == self.own_signature {
            return; // our own save, or nothing new
        }
        if !self.modified() {
            self.load(app, id);
            return;
        }
        if self.asking_reload {
            return;
        }
        self.asking_reload = true;
        let text = format!("{} was changed by another program.\n\nReload it and lose your changes?", self.name());
        let mb = MessageBox::new("Changed on disk", &text, &["Reload", "Cancel"], 0).width(56).on_choice(move |app, choice| {
            with_editor(app, id, |w, app| {
                w.asking_reload = false;
                if choice == Some(0) {
                    w.load(app, id);
                } else {
                    // Keep the edits; don't ask again for this version.
                    w.own_signature = Some(sig);
                }
            });
        });
        app.push_modal(id, Box::new(mb));
        app.request(PlatformRequest::RaiseWindow(id));
    }

    // -- find and replace ----------------------------------------------------------

    pub fn show_find(&mut self, replace: bool) {
        self.bar.visible = true;
        self.bar.replace = replace && !self.read_only;
        let selected = self.buf.selected_text();
        if !selected.is_empty() && !selected.contains(' ') && !selected.contains('\n') {
            self.bar.find.set_text(&selected);
        }
        self.bar.find.select_all();
        self.focus = Focus::Find;
    }

    /// Find the next (previous) match, wrapping around once. False if none.
    pub fn find_next(&mut self, backward: bool) -> bool {
        let needle = self.bar.find.text.clone();
        if needle.is_empty() {
            self.show_find(self.bar.replace);
            return false;
        }
        let case = self.bar.match_case.checked;
        let sel = self.buf.selection().unwrap_or(self.buf.cursor..self.buf.cursor);
        let from = if backward { sel.start } else { sel.end };
        if let Some(m) = find::find(&self.buf, &needle, from, backward, case) {
            self.select_match(m);
            self.bar.status.clear();
            return true;
        }
        let from = if backward { self.buf.len_chars() } else { 0 };
        match find::find(&self.buf, &needle, from, backward, case) {
            Some(m) => {
                self.select_match(m);
                self.bar.status = "wrapped around".into();
                true
            }
            None => {
                self.bar.status = "not found".into();
                false
            }
        }
    }

    fn select_match(&mut self, m: Range<usize>) {
        self.buf.select(m);
        self.follow = true;
    }

    /// Enter in the replacement field: replace the current match, find the next.
    pub fn replace_one(&mut self) {
        let needle = self.bar.find.text.clone();
        let case = self.bar.match_case.checked;
        if !self.read_only && self.buf.selection().is_some() && find::same(&self.buf.selected_text(), &needle, case) {
            let with = self.bar.with.text.clone();
            self.buf.insert(&with, false);
        }
        self.find_next(false);
    }

    /// Replace every match as one undo step; returns how many.
    pub fn replace_all(&mut self) -> usize {
        let needle = self.bar.find.text.clone();
        if needle.is_empty() || self.read_only {
            return 0;
        }
        let ranges = find::find_all(&self.buf, &needle, self.bar.match_case.checked);
        let with = self.bar.with.text.clone();
        self.buf.replace_ranges(&ranges, &with);
        self.bar.status = format!("{} replaced", ranges.len());
        self.follow = true;
        ranges.len()
    }

    // -- commands ------------------------------------------------------------------

    fn field_mut(&mut self) -> Option<&mut TextInput> {
        match self.focus {
            Focus::Find => Some(&mut self.bar.find),
            Focus::Replace => Some(&mut self.bar.with),
            _ => None,
        }
    }

    pub fn run(&mut self, app: &mut App, id: WinId, action: Action) {
        // Clipboard keys in a find field edit the field.
        if matches!(action, Action::Cut | Action::Copy | Action::Paste | Action::SelectAll)
            && let Some(key) = action.shortcuts().first().map(Shortcut::to_event)
            && let Some(field) = self.field_mut()
        {
            field.handle_key(&key, app.clipboard.as_mut());
            if action != Action::Copy && action != Action::SelectAll {
                self.bar.status.clear();
            }
            return;
        }
        match action {
            Action::Save => self.save(app, id, false),
            Action::Close => close_window(app, id),
            Action::Undo => {
                if !self.read_only && self.buf.undo() {
                    self.follow = true;
                }
            }
            Action::Redo => {
                if !self.read_only && self.buf.redo() {
                    self.follow = true;
                }
            }
            Action::Cut => {
                if let Some(text) = self.buf.selection().map(|_| self.buf.selected_text()) {
                    app.clipboard.set_text(&text);
                    if !self.read_only {
                        self.buf.insert("", false);
                        self.follow = true;
                    }
                }
            }
            Action::Copy => {
                if self.buf.selection().is_some() {
                    let text = self.buf.selected_text();
                    app.clipboard.set_text(&text);
                }
            }
            Action::Paste => {
                if !self.read_only
                    && let Some(text) = app.clipboard.get_text()
                {
                    self.buf.insert(&text, false);
                    self.follow = true;
                }
            }
            Action::SelectAll => self.buf.select_all(),
            Action::Find => self.show_find(false),
            Action::Replace => self.show_find(true),
            Action::FindNext => {
                self.find_next(false);
            }
            Action::FindPrevious => {
                self.find_next(true);
            }
            Action::GoToLine => ask_line(app, id),
            Action::Escape => {
                if self.bar.visible {
                    self.bar.visible = false;
                    self.focus = Focus::Text;
                } else {
                    close_window(app, id);
                }
            }
        }
    }

    // -- keys ----------------------------------------------------------------------

    /// Whether the menu item for `a` is enabled.
    fn enabled(&self, a: Action) -> bool {
        match a {
            Action::Undo => self.buf.can_undo() && !self.read_only,
            Action::Redo => self.buf.can_redo() && !self.read_only,
            Action::Save => !self.read_only && self.loaded,
            Action::Replace => !self.read_only,
            _ => true,
        }
    }

    fn key(&mut self, app: &mut App, id: WinId, k: &KeyEvent) -> Outcome {
        if let Some(action) = Action::for_key(k) {
            self.run(app, id, action);
            return Outcome::Consumed;
        }
        match self.focus {
            Focus::Text => self.text_key(app, id, k),
            Focus::Find | Focus::Replace | Focus::MatchCase => self.bar_key(app, k),
        }
    }

    fn bar_key(&mut self, app: &mut App, k: &KeyEvent) -> Outcome {
        let order: Vec<Focus> =
            if self.bar.replace { vec![Focus::Find, Focus::Replace, Focus::MatchCase] } else { vec![Focus::Find, Focus::MatchCase] };
        match k.key {
            Key::Tab if k.mods.is_empty() || k.mods == crate::input::Mods::SHIFT => {
                let i = order.iter().position(|f| *f == self.focus).unwrap_or(0) as isize;
                let d = if k.mods.shift { -1 } else { 1 };
                self.focus = order[(i + d).rem_euclid(order.len() as isize) as usize];
                return Outcome::Consumed;
            }
            Key::Enter if self.focus == Focus::Find => {
                self.find_next(k.mods.shift);
                return Outcome::Consumed;
            }
            Key::Enter if self.focus == Focus::Replace => {
                self.replace_one();
                return Outcome::Consumed;
            }
            _ => {}
        }
        if self.focus == Focus::MatchCase {
            if self.bar.match_case.handle_key(k, true) {
                self.bar.status.clear();
            }
            return Outcome::Consumed;
        }
        let clip = app.clipboard.as_mut();
        let field = if self.focus == Focus::Find { &mut self.bar.find } else { &mut self.bar.with };
        let before = field.text.clone();
        if field.handle_key(k, clip) {
            if field.text != before && self.focus == Focus::Find {
                self.bar.status.clear();
            }
            return Outcome::Consumed;
        }
        Outcome::Ignored
    }

    fn text_key(&mut self, app: &mut App, id: WinId, k: &KeyEvent) -> Outcome {
        let m = k.mods;
        let mac = cfg!(target_os = "macos");
        let word = if mac { m.alt } else { m.ctrl };
        let line_mod = mac && m.logo;
        let doc_mod = if mac { m.logo } else { m.ctrl };
        let rows = self.layout(app.screen(id)).code.h.max(1) as isize;
        let edit = !self.read_only;
        let b = &mut self.buf;
        match k.key {
            Key::Left => {
                let pos = if line_mod {
                    b.line_start(b.line_of(b.cursor))
                } else if word {
                    b.prev_word(b.cursor)
                } else if !m.shift
                    && let Some(sel) = b.selection()
                {
                    sel.start
                } else {
                    b.prev_pos(b.cursor)
                };
                b.move_to(pos, m.shift);
            }
            Key::Right => {
                let pos = if line_mod {
                    b.line_end(b.line_of(b.cursor))
                } else if word {
                    b.next_word(b.cursor)
                } else if !m.shift
                    && let Some(sel) = b.selection()
                {
                    sel.end
                } else {
                    b.next_pos(b.cursor)
                };
                b.move_to(pos, m.shift);
            }
            Key::Up if doc_mod => b.move_to(0, m.shift),
            Key::Down if doc_mod => b.move_to(b.len_chars(), m.shift),
            Key::Up => b.move_lines(-1, m.shift),
            Key::Down => b.move_lines(1, m.shift),
            Key::Home if doc_mod => b.move_to(0, m.shift),
            Key::End if doc_mod => b.move_to(b.len_chars(), m.shift),
            Key::Home => b.move_to(b.line_start(b.line_of(b.cursor)), m.shift),
            Key::End => b.move_to(b.line_end(b.line_of(b.cursor)), m.shift),
            Key::PageUp => {
                b.move_lines(-rows, m.shift);
                self.top = self.top.saturating_sub(rows as usize);
            }
            Key::PageDown => {
                b.move_lines(rows, m.shift);
                self.top += rows as usize;
            }
            Key::Backspace if edit => {
                if line_mod {
                    b.backspace_line();
                } else {
                    b.backspace(word);
                }
            }
            Key::Delete if edit && (m.is_empty() || word) => b.delete_forward(word),
            Key::Enter if edit && (m.is_empty() || m == crate::input::Mods::SHIFT) => {
                // Enter keeps the line's indentation (1.x).
                let indent = b.indent_of_line(b.line_of(b.cursor));
                b.insert(&format!("\n{indent}"), true);
            }
            Key::Tab if edit && m.is_empty() => b.insert("\t", true),
            Key::Backspace | Key::Delete | Key::Enter | Key::Tab => return Outcome::Consumed,
            _ => match k.typed_text() {
                Some(t) if edit => b.insert(t, true),
                Some(_) => {}
                None => return Outcome::Ignored,
            },
        }
        self.follow = true;
        Outcome::Consumed
    }

    // -- mouse ---------------------------------------------------------------------

    fn pos_at(&self, l: &Layout, col: u16, row: u16) -> usize {
        let line = (self.top + row.saturating_sub(l.code.y) as usize).min(self.buf.len_lines() - 1);
        let x = self.left + col.saturating_sub(l.code.x) as usize;
        self.buf.pos_at_col(line, x, true)
    }

    fn mouse(&mut self, app: &mut App, id: WinId, m: &crate::input::MouseEvent) -> Outcome {
        let l = self.layout(app.screen(id));
        if let Some(bar) = l.bar
            && bar.contains(m.col, m.row)
        {
            if let MouseKind::Down(MouseButton::Left) = m.kind {
                let bl = self.bar_layout(bar);
                if bl.find.contains(m.col, m.row) {
                    self.focus = Focus::Find;
                    self.bar.find.click(bl.find, m.col);
                } else if let Some(w) = bl.with.filter(|r| r.contains(m.col, m.row)) {
                    self.focus = Focus::Replace;
                    self.bar.with.click(w, m.col);
                } else if bl.replace_all.is_some_and(|r| r.contains(m.col, m.row)) {
                    self.replace_all();
                } else if self.bar.match_case.click(bl.match_case, m.col, m.row) {
                    self.bar.status.clear();
                }
            }
            return Outcome::Consumed;
        }
        let text_area = Rect::new(l.gutter.x, l.code.y, l.gutter.w + l.code.w, l.code.h);
        match m.kind {
            MouseKind::Scroll(lines) => {
                let delta = (-lines * 3.0).round() as isize;
                self.top = (self.top as isize + delta).max(0) as usize;
                self.clamp_scroll(app.screen(id));
            }
            MouseKind::Down(MouseButton::Left) if text_area.contains(m.col, m.row) => {
                self.focus = Focus::Text;
                let pos = self.pos_at(&l, m.col.max(l.code.x), m.row);
                self.buf.move_to(pos, m.mods.shift);
                self.dragging = true;
            }
            MouseKind::DoubleClick(MouseButton::Left) if l.code.contains(m.col, m.row) => {
                let pos = self.pos_at(&l, m.col, m.row);
                let word = self.buf.word_at(pos);
                self.buf.select(word);
                self.dragging = false;
            }
            MouseKind::Move if self.dragging => {
                if m.row < l.code.y {
                    self.top = self.top.saturating_sub(1);
                } else if m.row >= l.code.bottom() {
                    self.top += 1;
                    self.clamp_scroll(app.screen(id));
                }
                let row = m.row.clamp(l.code.y, l.code.bottom().saturating_sub(1));
                let pos = self.pos_at(&l, m.col.max(l.code.x), row);
                self.buf.move_to(pos, true);
            }
            MouseKind::Up(MouseButton::Left) => self.dragging = false,
            _ => return Outcome::Ignored,
        }
        Outcome::Consumed
    }
}

impl UiWindow for EditorWindow {
    fn title(&self) -> String {
        let mark = if self.modified() { "● " } else { "" };
        format!("{mark}{} — {}", self.name(), oc_config::APP_NAME)
    }

    fn draw(&self, app: &App, f: &mut Frame, ctx: &DrawCtx) {
        super::draw::draw(self, app, f, ctx);
    }

    fn handle_event(&mut self, app: &mut App, id: WinId, ev: &Event) -> Outcome {
        let outcome = match ev {
            Event::Key(k) => self.key(app, id, k),
            Event::Mouse(m) => self.mouse(app, id, m),
            Event::Preedit(s) => {
                match self.field_mut() {
                    Some(field) => field.preedit = s.clone(),
                    None => self.preedit = s.clone(),
                }
                Outcome::Consumed
            }
            Event::Commit(s) => {
                self.preedit.clear();
                match self.focus {
                    Focus::Text if !self.read_only => {
                        self.buf.insert(s, true);
                        self.follow = true;
                    }
                    Focus::Find | Focus::Replace => {
                        let clip = app.clipboard.as_mut();
                        if let Some(field) = match self.focus {
                            Focus::Find => Some(&mut self.bar.find),
                            Focus::Replace => Some(&mut self.bar.with),
                            _ => None,
                        } {
                            field.handle_event(ev, clip);
                        }
                    }
                    _ => {}
                }
                Outcome::Consumed
            }
            Event::Focus(_) => Outcome::Ignored,
        };
        self.settle(app, id);
        outcome
    }

    fn close_requested(&mut self, app: &mut App, id: WinId) -> bool {
        if self.modified() && !self.read_only && !self.discard {
            if !self.asking {
                self.asking = true;
                let text = format!("Save the changes to {}?", self.name());
                let mb = MessageBox::new("Unsaved changes", &text, &["Save", "Don't save", "Cancel"], 0).width(56).on_choice(
                    move |app, choice| {
                        let mut close = false;
                        with_editor(app, id, |w, app| {
                            w.asking = false;
                            match choice {
                                Some(0) => w.save(app, id, true),
                                Some(1) => {
                                    w.discard = true;
                                    close = true;
                                }
                                _ => app.quit_pending = false,
                            }
                        });
                        if close {
                            close_window(app, id);
                        }
                    },
                );
                app.push_modal(id, Box::new(mb));
            }
            app.request(PlatformRequest::RaiseWindow(id));
            return false;
        }
        self.hl.cancel();
        if let Some(t) = self.reload_timer.take() {
            app.cancel_timer(t);
        }
        if let Some(w) = self.watch.take() {
            app.tasks.spawn(move || drop(w), |_, ()| {});
        }
        app.state::<EditorState>().windows.retain(|(w, _)| *w != id);
        true
    }

    fn wants_text_input(&self) -> bool {
        match self.focus {
            Focus::Text => !self.read_only && self.loaded,
            Focus::Find | Focus::Replace => true,
            Focus::MatchCase => false,
        }
    }

    fn menus(&self) -> Vec<WindowMenu> {
        let item = |a: Action| {
            let enabled = self.enabled(a);
            Some(WindowMenuItem { id: a.id().into(), label: a.title().into(), shortcut: a.shortcuts().first().copied(), enabled })
        };
        use Action::*;
        vec![
            WindowMenu { title: "File".into(), items: vec![item(Save), None, item(Close)] },
            WindowMenu {
                title: "Edit".into(),
                items: vec![
                    item(Undo),
                    item(Redo),
                    None,
                    item(Cut),
                    item(Copy),
                    item(Paste),
                    item(SelectAll),
                    None,
                    item(Find),
                    item(FindNext),
                    item(FindPrevious),
                    item(Replace),
                    None,
                    item(GoToLine),
                ],
            },
        ]
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// A file read for the editor, made ready on the worker: the rope, the
/// language and its highlighter (loading syntaxes takes a moment the first
/// time), so the UI thread only swaps them in.
pub enum Prepared {
    Text { file: TextFile, rope: ropey::Rope, language: String, hl: Option<LineHighlighter> },
    Binary,
    Failed(String),
}

fn prepare(location: &VPath, loaded: Loaded) -> Prepared {
    match loaded {
        Loaded::Text(mut file) => {
            let text = std::mem::take(&mut file.text);
            let (language, syntax_name) = super::language::language_for(&location.name(), &text);
            let syntax = syntax_name.and_then(|n| syn::syntax_set().syntaxes().iter().find(|s| s.name.eq_ignore_ascii_case(n)));
            // Like 1.x, very large texts stay plain (counted in characters).
            let big = text.len() > MAX_HIGHLIGHT_CHARS && text.chars().nth(MAX_HIGHLIGHT_CHARS).is_some();
            let hl = syntax.filter(|_| !big).map(LineHighlighter::new);
            Prepared::Text { file, rope: ropey::Rope::from_str(&text), language, hl }
        }
        Loaded::Binary => Prepared::Binary,
        Loaded::Failed(e) => Prepared::Failed(e),
    }
}

/// Run `f` on editor window `id` if it is still open (task results, dialog answers).
pub fn with_editor<R>(app: &mut App, id: WinId, f: impl FnOnce(&mut EditorWindow, &mut App) -> R) -> Option<R> {
    let mut w = app.take_window(id)?;
    let r = w.as_any_mut().downcast_mut::<EditorWindow>().map(|e| f(e, app));
    app.put_window(id, w);
    r
}

/// Close (or ask to close) window `id` after the current event; carries on
/// quitting if that is what asked.
pub fn close_window(app: &mut App, id: WinId) {
    app.later(move |app| {
        app.close_requested(id);
        let closed = !app.window_ids().contains(&id);
        if closed && app.quit_pending {
            app.later(App::quit);
        }
    });
}

/// The run of characters from the first `first` on that `file`'s encoding
/// can't store: what Python's encode error reported (1.x quoted it).
fn unencodable_run(file: &TextFile, text: &str, first: char) -> String {
    let fails = |c: char| file.encode(Some(c.encode_utf8(&mut [0; 4]))).is_err();
    let start = text.find(first).unwrap_or(0);
    let run: String = text[start..].chars().take_while(|c| fails(*c)).collect();
    if run.is_empty() { first.to_string() } else { run }
}

/// Python's `repr` of a string ('日本', ' '), as 1.x showed it.
pub fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if !py_printable(c) => {
                let n = c as u32;
                out.push_str(&if n < 0x100 {
                    format!("\\x{n:02x}")
                } else if n < 0x10000 {
                    format!("\\u{n:04x}")
                } else {
                    format!("\\U{n:08x}")
                });
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python's `str.isprintable` for one char, roughly: not a control, format,
/// separator (other than space), surrogate or private-use character.
fn py_printable(c: char) -> bool {
    let n = c as u32;
    let format = matches!(n, 0xad | 0x600..=0x605 | 0x61c | 0x6dd | 0x70f | 0x180e | 0x200b..=0x200f | 0x202a..=0x202e
        | 0x2060..=0x2064 | 0x2066..=0x206f | 0xfeff | 0xfff9..=0xfffb | 0x110bd | 0x1d173..=0x1d17a | 0xe0001 | 0xe0020..=0xe007f);
    let private = matches!(n, 0xe000..=0xf8ff | 0xf0000..);
    !(c.is_control() || format || private || (c.is_whitespace() && c != ' '))
}

/// ⌘L: ask for a line number.
fn ask_line(app: &mut App, id: WinId) {
    let ib = InputBox::new("Go to line", "Line number:", "", "Go", move |app, value| {
        let Some(n) = value.and_then(|v| v.trim().parse::<usize>().ok()) else {
            return;
        };
        let screen = app.screen(id);
        with_editor(app, id, |w, app| {
            w.go_to_line(screen, n.saturating_sub(1));
            w.settle(app, id);
        });
    })
    .width(40);
    app.push_modal(id, Box::new(ib));
}

/// The path as the panels show it: the home folder as ~ (1.x `_short_display`).
pub fn short_display(location: &VPath) -> String {
    let text = location.display();
    if !location.is_local() {
        return text;
    }
    let home = oc_local::home_dir().to_string_lossy().into_owned();
    let sep = std::path::MAIN_SEPARATOR;
    if text == home {
        return "~".into();
    }
    match text.strip_prefix(&home) {
        Some(rest) if rest.starts_with(sep) && !home.is_empty() => format!("~{rest}"),
        _ => text,
    }
}

fn start_watch(path: PathBuf, tasks: crate::tasks::Tasks, id: WinId) -> Option<FileWatch> {
    use notify::{EventKind, RecursiveMode, Watcher};
    // A symlink: watch the file it points to (saves write there, too).
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    let dir = path.parent()?.to_path_buf();
    let name = path.file_name()?.to_os_string();
    let pending = Arc::new(AtomicBool::new(false));
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        if matches!(event.kind, EventKind::Access(_)) || !event.paths.iter().any(|p| p.file_name() == Some(name.as_os_str())) {
            return;
        }
        if !pending.swap(true, Ordering::SeqCst) {
            let pending = pending.clone();
            tasks.post(move |app| {
                pending.store(false, Ordering::SeqCst);
                with_editor(app, id, |w, app| w.file_event(app, id));
            });
        }
    });
    let mut watcher = watcher.ok()?;
    watcher.watch(&dir, RecursiveMode::NonRecursive).ok()?;
    Some(FileWatch { _watcher: watcher })
}
