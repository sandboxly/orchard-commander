//! ⌥Enter / Alt+Enter: the item under the cursor in a window of its own —
//! path, type, size, dates, permissions, owner, where a link points. For a
//! folder, what takes up its space: measured in the background (the window
//! fills in as the walk goes on) and drawn as a donut chart in the color
//! scheme's colors, with a legend.
//!
//! * `chart` — the donut as runs of pixels.
//! * `draw` — the window.

mod chart;
mod draw;

pub use draw::facts;

use std::any::Any;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use oc_grid::Frame;
use oc_vfs::{Entry, VPath};

use crate::app::{App, Outcome, PlatformRequest, UiWindow, WinId};
use crate::commands::{Command, Registry};
use crate::input::{Event, Key};
use crate::menus::Menu;
use crate::panel::PARENT;
use crate::tasks::CancelToken;
use crate::widgets::DrawCtx;

/// The window's size when it opens.
pub const COLS: u16 = 84;
pub const ROWS: u16 = 32;

/// Slices in the chart; the rest are one "others" slice.
pub const SLICES: usize = 8;

/// How often a running measurement shows where it got.
const PROGRESS_EVERY: Duration = Duration::from_millis(200);

/// The open info windows and what they show.
#[derive(Default)]
pub struct InfoState {
    pub windows: Vec<(WinId, VPath)>,
}

pub fn register(reg: &mut Registry) {
    reg.add(
        Command::new("files.info", "Show info", show_info)
            .help("Size, dates and permissions of the item under the cursor; for a folder, what takes up its space")
            .keys(&["Alt+Enter"])
            .menu(Menu::Files, "ops"),
    );
}

/// One of a folder's items in the chart: its size with everything inside.
#[derive(Clone, Debug, PartialEq)]
pub struct Part {
    pub name: String,
    pub is_dir: bool,
    pub bytes: u64,
}

/// A folder's space, so far or in full.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    /// The biggest items, largest first (at most [`SLICES`], none empty).
    pub parts: Vec<Part>,
    /// The other items: how many, and their bytes.
    pub others: (u64, u64),
    pub bytes: u64,
    pub files: u64,
    pub dirs: u64,
    /// Items the walk couldn't read.
    pub unreadable: u64,
    pub done: bool,
}

/// The info window.
pub struct InfoWindow {
    pub location: VPath,
    pub name: String,
    /// `None` until the stat is back.
    pub entry: Option<Entry>,
    pub details: Option<oc_local::Details>,
    /// Free space on the folder's volume.
    pub free: Option<u64>,
    pub error: Option<String>,
    /// For folders, once the measurement started.
    pub usage: Option<Usage>,
    token: Option<CancelToken>,
}

impl InfoWindow {
    pub fn new(location: VPath) -> Self {
        let name = location.name();
        InfoWindow { location, name, entry: None, details: None, free: None, error: None, usage: None, token: None }
    }

    /// A folder (not a link to one) whose space is measured.
    pub fn is_folder(&self) -> bool {
        self.entry.as_ref().is_some_and(|e| e.is_dir && !e.is_link)
    }

    pub fn measuring(&self) -> bool {
        self.usage.as_ref().is_some_and(|u| !u.done)
    }
}

/// ⌥Enter: the item under the cursor; on "..", the folder shown.
pub fn show_info(app: &mut App) {
    let side = app.main.files_side();
    let p = &app.main.sides[side].panel;
    let target = match p.cursor_name() {
        Some(name) if name != PARENT => p.target(name),
        _ => p.location.clone(),
    };
    open_info(app, target);
}

/// Open (or bring to the front) the info window for `location`.
pub fn open_info(app: &mut App, location: VPath) -> WinId {
    let existing = app.state::<InfoState>().windows.iter().find(|(_, l)| *l == location).map(|(id, _)| *id);
    if let Some(id) = existing {
        app.request(PlatformRequest::RaiseWindow(id));
        return id;
    }
    let id = app.open_window(Box::new(InfoWindow::new(location.clone())), COLS, ROWS);
    app.state::<InfoState>().windows.push((id, location.clone()));
    app.tasks.run_in_background(
        move |_| {
            let entry = location.stat().map_err(|e| crate::panel::error_reason(&e));
            let details = location.local_path().filter(|_| location.is_local()).and_then(|p| oc_local::details(&p).ok());
            let free = entry.as_ref().ok().filter(|e| e.is_dir).and_then(|_| location.fs.free_space(&location.path));
            (entry, details, free)
        },
        move |app, (entry, details, free)| {
            let Some(w) = app.window_mut::<InfoWindow>(id) else { return };
            match entry {
                Ok(e) => w.entry = Some(e),
                Err(e) => w.error = Some(format!("Can't read {}: {e}", w.location.display())),
            }
            w.details = details;
            w.free = free;
            if w.is_folder() {
                start_measuring(app, id);
            }
        },
    );
    id
}

fn start_measuring(app: &mut App, id: WinId) {
    let Some(w) = app.window_mut::<InfoWindow>(id) else { return };
    w.usage = Some(Usage::default());
    let dir = w.location.clone();
    let tasks = app.tasks.clone();
    let token = app.tasks.run_in_background(
        move |token| {
            measure(&dir, &token.as_fn(), |snapshot| {
                tasks.post(move |app| {
                    if let Some(w) = app.window_mut::<InfoWindow>(id).filter(|w| w.measuring()) {
                        w.usage = Some(snapshot);
                    }
                })
            })
        },
        move |app, usage| {
            if let Some(w) = app.window_mut::<InfoWindow>(id) {
                w.usage = Some(usage);
                w.token = None;
            }
        },
    );
    if let Some(w) = app.window_mut::<InfoWindow>(id) {
        w.token = Some(token);
    }
}

/// Walk `dir`, adding up each item's bytes (folders with everything in
/// them); `progress` gets a snapshot every [`PROGRESS_EVERY`].
pub fn measure(dir: &VPath, cancelled: &dyn Fn() -> bool, mut progress: impl FnMut(Usage)) -> Usage {
    let mut items: HashMap<String, (bool, u64)> = HashMap::new();
    let (mut files, mut dirs, mut unreadable) = (0u64, 0u64, 0u64);
    let mut last = Instant::now();
    match dir.fs.walk(&dir.path) {
        Ok(walk) => {
            for item in walk {
                if cancelled() {
                    break;
                }
                match item {
                    Ok((rel, entry)) => {
                        let Some(top) = rel.first() else { continue };
                        let slot = items.entry(top.clone()).or_insert((false, 0));
                        if rel.len() == 1 {
                            slot.0 = entry.is_dir;
                        }
                        if entry.is_dir {
                            dirs += 1;
                        } else {
                            files += 1;
                            slot.1 += entry.size;
                        }
                    }
                    Err(_) => unreadable += 1,
                }
                if last.elapsed() >= PROGRESS_EVERY {
                    last = Instant::now();
                    progress(summarize(&items, files, dirs, unreadable, false));
                }
            }
        }
        Err(_) => unreadable += 1,
    }
    summarize(&items, files, dirs, unreadable, true)
}

fn summarize(items: &HashMap<String, (bool, u64)>, files: u64, dirs: u64, unreadable: u64, done: bool) -> Usage {
    let mut all: Vec<(&String, &(bool, u64))> = items.iter().collect();
    all.sort_by(|a, b| b.1.1.cmp(&a.1.1).then_with(|| a.0.cmp(b.0)));
    let parts: Vec<Part> = all
        .iter()
        .take(SLICES)
        .filter(|(_, (_, bytes))| *bytes > 0)
        .map(|(name, (is_dir, bytes))| Part { name: (*name).clone(), is_dir: *is_dir, bytes: *bytes })
        .collect();
    let rest = &all[parts.len()..];
    let others = (rest.len() as u64, rest.iter().map(|(_, (_, b))| b).sum());
    let bytes = items.values().map(|(_, b)| b).sum();
    Usage { parts, others, bytes, files, dirs, unreadable, done }
}

fn close(app: &mut App, id: WinId) {
    app.later(move |app| app.close_requested(id));
}

impl UiWindow for InfoWindow {
    fn title(&self) -> String {
        format!("Info: {} — {}", self.name, oc_config::APP_NAME)
    }

    fn draw(&self, _app: &App, f: &mut Frame, ctx: &DrawCtx) {
        draw::draw(self, f, ctx);
    }

    fn handle_event(&mut self, app: &mut App, id: WinId, ev: &Event) -> Outcome {
        let Event::Key(k) = ev else { return Outcome::Ignored };
        let m = k.mods;
        let closes = match k.key {
            Key::Escape | Key::F(10) => m.is_empty(),
            Key::Enter => m.is_empty() || (m.alt && !m.shift && !m.has_primary()),
            Key::Char('w' | 'W') => m.has_primary(),
            Key::F(4) => m.has_primary() && !cfg!(target_os = "macos"),
            _ => false,
        };
        if !closes {
            return Outcome::Ignored;
        }
        close(app, id);
        Outcome::Consumed
    }

    fn close_requested(&mut self, app: &mut App, id: WinId) -> bool {
        if let Some(t) = self.token.take() {
            t.cancel();
        }
        app.state::<InfoState>().windows.retain(|(w, _)| *w != id);
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_each_item_with_what_is_inside() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        std::fs::create_dir_all(d.join("big/deeper")).unwrap();
        std::fs::write(d.join("big/a"), vec![0u8; 3000]).unwrap();
        std::fs::write(d.join("big/deeper/b"), vec![0u8; 1000]).unwrap();
        std::fs::write(d.join("small.txt"), vec![0u8; 10]).unwrap();
        std::fs::write(d.join("empty"), b"").unwrap();
        for i in 0..SLICES {
            std::fs::write(d.join(format!("f{i}")), vec![0u8; 20 + i]).unwrap();
        }
        let usage = measure(&oc_local::local(d), &|| false, |_| {});
        assert!(usage.done);
        assert_eq!(usage.parts.len(), SLICES);
        assert_eq!(usage.parts[0], Part { name: "big".into(), is_dir: true, bytes: 4000 });
        assert_eq!(usage.parts[1].name, format!("f{}", SLICES - 1));
        // small.txt and the empty file don't make the cut.
        assert_eq!(usage.others, (3, 10 + 20));
        assert_eq!(usage.bytes, 4000 + 10 + (0..SLICES as u64).map(|i| 20 + i).sum::<u64>());
        assert_eq!((usage.files, usage.dirs, usage.unreadable), (SLICES as u64 + 4, 2, 0));
        let cancelled = measure(&oc_local::local(d), &|| true, |_| {});
        assert_eq!(cancelled.bytes, 0);
    }
}
