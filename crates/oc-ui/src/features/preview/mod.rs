//! The Preview side view (port of `ui/preview_pane.py`): shows the file
//! under the other panel's cursor and follows it.
//!
//! It knows nothing about file types. For each file it asks oc-preview's
//! registry (on a worker, debounced, cancelled when the cursor moves on)
//! which previewers fit, shows the best one's data in a content view
//! ([`views`]) and offers the others as "View as" chips (click, or Shift+F3
//! to cycle; the choice is remembered per extension until quit). The
//! previous content stays until the new one is ready. The title is the file
//! name, the bottom line the previewer's size/type info.
//!
//! Online-only cloud files: up to 25 MB they are fetched with "Downloading
//! … from the cloud" and the seconds waited; bigger ones need a click on
//! "Download and preview"; a fetch that times out offers "Try again".
//!
//! Audio and video play in [`media`]; when a file ends the pane moves the
//! other panel's cursor to the next file it plays and starts it
//! ([`finished_playing`], 1.x's `MainWindow._play_next`).

pub mod image;
pub mod markdown;
pub mod media;
mod media_paint;
pub mod message;
pub mod pdf;
pub mod text;
pub mod views;

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use oc_grid::{Flags, Frame, Style};
use oc_preview::{AUTO_DOWNLOAD_LIMIT, FileInfo, LoadContext, PreviewError, Score, ext_for_name};
use oc_vfs::{Entry, VPath, human_size};

use self::message::MessageView;
use self::views::{ContentView, Handle, Prepared};
use crate::app::{App, Outcome};
use crate::commands::{Command, Registry};
use crate::geom::Rect;
use crate::input::{Event, MouseButton, MouseKind};
use crate::main_window::{self, SideMode, SideView, with_view_as};
use crate::menus::Menu;
use crate::tasks::CancelToken;
use crate::widgets::{BoxFrame, DrawCtx, fill, put, width};

pub const ID: &str = "preview";

/// Cursor movement settles this long before a file is loaded.
pub const DEBOUNCE: Duration = Duration::from_millis(120);
/// Files not on the local disk show "Loading …" if they take longer than this.
pub const SLOW_LOAD: Duration = Duration::from_millis(300);
/// The "View as" label before the chips.
const VIEW_AS: &str = "View as";
/// Padding around a chip label, in cells (like the mode tabs).
const CHIP_PAD: u16 = 3;

pub fn register(reg: &mut Registry) {
    reg.add_side_view(|| Box::new(PreviewView::default()));
    reg.on_settings_changed(media::settings_changed);
    reg.add(
        Command::new("preview.toggle", "Toggle preview", toggle_preview)
            .help("Preview the file under the cursor in the other panel")
            .keys(&["F3"])
            .fkey("F3", "Preview")
            .menu(Menu::Commands, "preview"),
    );
    reg.add(
        Command::new("preview.cycle", "Next preview view", cycle_preview_view)
            .help("Show the previewed file another way (e.g. as text or hex)")
            .keys(&["Shift+F3"])
            .menu(Menu::Commands, "preview"),
    );
    for side in 0..2 {
        let (id, menu, title) =
            if side == 0 { ("left.preview", Menu::Left, "Left: Preview") } else { ("right.preview", Menu::Right, "Right: Preview") };
        reg.add(
            // Shows the preview (choosing it again keeps it, like 1.x's set_mode).
            Command::new(id, title, move |app| show_preview(app, side))
                .help("Preview the other panel's file on this side")
                .menu(menu, "mode")
                .menu_label("Preview")
                .checked(move |app| app.main.sides[side].mode_id() == ID)
                .hidden_from_palette(), // only in the Left/Right menus, like 1.x
        );
    }
}

/// Left / Right menu → Preview.
fn show_preview(app: &mut App, side: usize) {
    if let Some(i) = app.main.sides[side].view_index(ID) {
        main_window::set_mode(app, side, SideMode::View(i));
    }
}

/// F3: preview the active panel's file in the other side, or close the preview.
pub fn toggle_preview(app: &mut App) {
    let active = app.main.active;
    let target = if app.main.sides[active].mode == SideMode::Files { 1 - active } else { active };
    main_window::toggle_view(app, target, ID);
}

/// Shift+F3: the next "View as" option of the shown preview.
pub fn cycle_preview_view(app: &mut App) {
    for side in 0..2 {
        if app.main.sides[side].mode_id() == ID {
            with_view_as::<PreviewView, _>(app, side, ID, |v, app| v.cycle_view(app, side));
        }
    }
}

/// Reload panels (⌘R) also loads the shown preview again.
pub fn refresh_all(app: &mut App) {
    for side in 0..2 {
        if app.main.sides[side].mode_id() == ID {
            with_view_as::<PreviewView, _>(app, side, ID, |v, app| v.reload(app, side));
        }
    }
}

/// A file in `side`'s preview played to its end: select the next file in
/// the other panel that the same previewer plays, and play it (1.x's
/// `MainWindow._play_next`). Nothing happens if the user moved on
/// meanwhile, or after the last playable file.
pub fn finished_playing(app: &mut App, side: usize, finished: &VPath) {
    if app.main.sides[side].mode_id() != ID {
        return;
    }
    let other = 1 - side;
    let panel = &app.main.sides[other].panel;
    if panel.cursor_target().as_ref() != Some(finished) {
        return; // the user moved on meanwhile
    }
    let Some(pane) = main_window::view_as::<PreviewView>(app, side, ID) else { return };
    if pane.path.as_ref() != Some(finished) {
        return;
    }
    let Some(previewer) = pane.current.as_deref().and_then(|id| oc_preview::Registry::global().get(id)) else { return };
    let rows: Vec<Option<&Entry>> = (0..panel.len()).map(|r| panel.entry_at(r)).collect();
    let next = oc_media::next_to_play(&rows, panel.cursor(), |e| e.is_some_and(|e| plays(&*previewer, e)));
    let Some(row) = next else { return };
    app.main.sides[other].panel.set_cursor(row);
    main_window::cursor_moved(app, other);
    let target = app.main.sides[other].panel.cursor_target();
    with_view_as::<PreviewView, _>(app, side, ID, |v, _| v.play_when_shown = target);
}

/// Whether `previewer` shows `entry` (by its name, without reading it).
fn plays(previewer: &dyn oc_preview::Previewer, entry: &Entry) -> bool {
    if entry.is_dir {
        return false;
    }
    let info = FileInfo { mtime: entry.mtime, has_direct_path: true, ..FileInfo::file(entry.name.clone(), entry.size) };
    previewer.score(&info) > Score::NO
}

/// "View as" choices by extension, for this run of the app (1.x's `_choice`).
#[derive(Default)]
struct Choices(HashMap<String, String>);

/// Tunables of the loading (an app state; tests shorten the cloud wait).
pub struct PreviewOptions {
    /// How long an online-only file may take to arrive, by size.
    pub fetch_timeout: fn(u64) -> Duration,
}

impl Default for PreviewOptions {
    fn default() -> Self {
        PreviewOptions { fetch_timeout: oc_preview::fetch_timeout }
    }
}

/// What the worker sends back.
struct Arrived {
    /// (previewer id, title), best first.
    candidates: Vec<(String, String)>,
    previewer: Option<String>,
    /// Taken by [`PreviewView::loaded`]; left here when nobody wants it.
    body: Option<Result<(Prepared, String), PreviewError>>,
}

impl Drop for Arrived {
    /// A result nobody wants (the cursor moved on, the load was cancelled
    /// after the worker finished, the pane closed) may be dropped on the UI
    /// thread. Media data streamed from a server holds open streams whose
    /// closing can wait on the network: close them on a thread of their own.
    fn drop(&mut self) {
        if let Some(Ok((Prepared::Media(m), _))) = &self.body
            && matches!(m.source, oc_preview::DocumentSource::Stream(_))
        {
            let body = self.body.take();
            let _ = std::thread::Builder::new().name("oc-preview drop".into()).spawn(move || drop(body));
        }
    }
}

#[derive(Default)]
pub struct PreviewView {
    /// The file shown (or being loaded).
    pub path: Option<VPath>,
    /// A file to load once the cursor settles (`Some(None)`: nothing).
    pending: Option<Option<VPath>>,
    debounce: Option<u64>,
    slow_timer: Option<u64>,
    ticker: Option<u64>,
    /// Counts loads; a result for an older one is dropped.
    request: u64,
    token: Option<CancelToken>,
    /// Previewer to use for the next load ("View as").
    forced: Option<String>,
    /// The online-only file the user chose to download.
    download_ok: Option<VPath>,
    /// The previewer of the shown data.
    current: Option<String>,
    /// "View as" choices: (previewer id, title).
    pub chips: Vec<(String, String)>,
    pub title: String,
    pub info: String,
    content: Option<Box<dyn ContentView>>,
    /// Counts contents (see [`Handle`]).
    serial: u64,
    /// Start playing this file once it is shown (moving on to the next file).
    pub play_when_shown: Option<VPath>,
}

impl PreviewView {
    /// The previewer shown, None while a message is shown.
    pub fn current_id(&self) -> Option<&str> {
        self.current.as_deref().filter(|_| self.content.as_ref().is_some_and(|c| c.kind() != "message"))
    }

    pub fn content(&self) -> Option<&dyn ContentView> {
        self.content.as_deref()
    }

    /// The content as its own type (tests).
    pub fn content_as<T: ContentView>(&self) -> Option<&T> {
        self.content.as_ref()?.as_any().downcast_ref::<T>()
    }

    pub fn chip_ids(&self) -> Vec<&str> {
        self.chips.iter().map(|(id, _)| id.as_str()).collect()
    }

    pub fn content_mut_as<T: ContentView>(&mut self) -> Option<&mut T> {
        self.content.as_mut()?.as_any_mut().downcast_mut::<T>()
    }

    /// A load is on its way.
    pub fn loading(&self) -> bool {
        self.pending.is_some() || self.token.is_some()
    }

    pub(crate) fn with_content<T: ContentView, R>(
        &mut self,
        app: &mut App,
        h: Handle,
        f: impl FnOnce(&mut T, &mut App, Rect) -> R,
    ) -> Option<R> {
        if h.serial != self.serial {
            return None;
        }
        let area = self.area(app, h.side);
        let content = self.content.as_mut()?.as_any_mut().downcast_mut::<T>()?;
        Some(f(content, app, area))
    }

    fn handle(&self, side: usize) -> Handle {
        Handle { side, serial: self.serial }
    }

    // -- layout -----------------------------------------------------------------

    /// The chips row (if shown) and the content area inside the pane's frame.
    fn split(&self, frame: Rect) -> (Option<Rect>, Rect) {
        let inner = frame.inset(1, 1);
        if self.chips.len() >= 2 && inner.h > 2 {
            let (row, rest) = inner.split_top(1);
            (Some(row), rest)
        } else {
            (None, inner)
        }
    }

    fn area(&self, app: &App, side: usize) -> Rect {
        self.split(main_window::layout(app).frames[side]).1
    }

    /// Where chip `i` is when the pane is drawn into `frame` (clicks in tests).
    pub fn chip_rect(&self, frame: Rect, i: usize) -> Option<Rect> {
        let row = self.split(frame).0?;
        self.chip_rects(row).get(i).copied()
    }

    /// Each chip's cells.
    fn chip_rects(&self, row: Rect) -> Vec<Rect> {
        let mut x = row.x + width(VIEW_AS) + 1;
        self.chips
            .iter()
            .map(|(_, title)| {
                let w = width(title) + 2 * CHIP_PAD;
                let r = Rect::new(x, row.y, w.min(row.right().saturating_sub(x)), 1);
                x = x.saturating_add(w + 1);
                r
            })
            .collect()
    }

    fn draw_chips(&self, f: &mut Frame, row: Rect, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        put(f, row.x, row.y, VIEW_AS, Style::new(c.muted, c.surface), row.w);
        for ((id, title), r) in self.chips.iter().zip(self.chip_rects(row)) {
            if r.is_empty() {
                break;
            }
            let selected = self.current.as_deref() == Some(id);
            let style =
                if selected { Style::new(c.foreground, c.border).with(Flags::BOLD) } else { Style::new(c.muted, c.surface) };
            fill(f, r, style.bg);
            put(f, r.x + CHIP_PAD, r.y, title, style, r.w.saturating_sub(CHIP_PAD));
        }
    }

    // -- loading ----------------------------------------------------------------

    /// Preview `path`, debounced so fast cursor movement stays smooth.
    pub fn show_path(&mut self, app: &mut App, side: usize, path: Option<VPath>) {
        if path == self.path && self.pending.is_none() {
            return;
        }
        if path != self.play_when_shown {
            self.play_when_shown = None; // the user went somewhere else
        }
        let first = self.content.is_none() && self.path.is_none();
        self.pending = Some(path);
        if let Some(t) = self.debounce.take() {
            app.cancel_timer(t);
        }
        if first {
            self.start_load(app, side);
            return;
        }
        self.debounce = Some(app.schedule(DEBOUNCE, move |app| {
            with_view_as::<PreviewView, _>(app, side, ID, |v, app| {
                v.debounce = None;
                v.start_load(app, side);
            });
        }));
    }

    /// Load the file again with the previewer shown (or with `forced`).
    pub fn reload(&mut self, app: &mut App, side: usize) {
        if let Some(path) = self.path.take() {
            self.forced = self.forced.take().or_else(|| self.current.clone());
            self.pending = Some(Some(path));
            self.start_load(app, side);
        }
    }

    /// Show the file with a specific previewer (remembered per extension).
    pub fn view_as(&mut self, app: &mut App, side: usize, previewer: &str) {
        let Some(path) = &self.path else { return };
        app.state::<Choices>().0.insert(ext_for_name(&path.name()), previewer.to_owned());
        self.forced = Some(previewer.to_owned());
        self.reload(app, side);
    }

    /// Switch to the next previewer that fits the file.
    pub fn cycle_view(&mut self, app: &mut App, side: usize) {
        let ids: Vec<String> = self.chips.iter().map(|(id, _)| id.clone()).collect();
        let Some(i) = self.current.as_ref().and_then(|c| ids.iter().position(|id| id == c)) else { return };
        if ids.len() > 1 {
            let next = ids[(i + 1) % ids.len()].clone();
            self.view_as(app, side, &next);
        }
    }

    /// Preview the online-only file even though that means downloading it.
    pub fn download_and_preview(&mut self, app: &mut App, side: usize) {
        if let Some(path) = &self.path {
            self.download_ok = Some(path.clone());
            self.forced = self.forced.take().or_else(|| self.current.clone());
            self.reload(app, side);
        }
    }

    fn stop_timers(&mut self, app: &mut App) {
        for t in [self.debounce.take(), self.slow_timer.take(), self.ticker.take()].into_iter().flatten() {
            app.cancel_timer(t);
        }
    }

    fn start_load(&mut self, app: &mut App, side: usize) {
        let path = self.pending.take().flatten();
        if let Some(t) = self.slow_timer.take() {
            app.cancel_timer(t);
        }
        if let Some(t) = self.token.take() {
            t.cancel();
        }
        self.request += 1;
        let request = self.request;
        if let Some(c) = self.content.as_mut() {
            c.leaving(app);
        }
        self.path = path.clone();
        let Some(path) = path else {
            self.current = None;
            self.chips.clear();
            self.title.clear();
            self.info.clear();
            self.set_content(app, side, Box::new(MessageView::new("No file selected", None)));
            return;
        };
        let name = path.name().to_owned();
        // The panel's entry knows whether this is an online-only cloud file
        // (no stat here: that is I/O, and the listing has it already).
        let panel = &app.main.sides[1 - side].panel;
        let cloud_size =
            panel.cursor_entry().filter(|e| e.is_online_only && panel.cursor_target().as_ref() == Some(&path)).map(|e| e.size);
        let download_ok = self.download_ok.as_ref() == Some(&path);
        if let Some(size) = cloud_size.filter(|&s| download_ok || s <= AUTO_DOWNLOAD_LIMIT) {
            self.show_loading(app, side, &name, format!("Downloading {name} from the cloud…\n{}", human_size(size)));
        } else if !path.is_local() {
            // Servers: only if it takes a moment, to avoid flicker.
            self.slow_timer = Some(app.schedule(SLOW_LOAD, move |app| {
                with_view_as::<PreviewView, _>(app, side, ID, |v, app| {
                    v.slow_timer = None;
                    if v.request == request {
                        let text = format!("Loading {name}…");
                        v.show_loading(app, side, &name, text);
                    }
                });
            }));
        }
        let preferred = self.forced.take().or_else(|| app.try_state::<Choices>().and_then(|c| c.0.get(&ext_for_name(&path.name())).cloned()));
        let mut ctx = LoadContext {
            show_hidden: app.main.sides[1 - side].panel.show_hidden,
            download_limit: if download_ok { None } else { Some(AUTO_DOWNLOAD_LIMIT) },
            syntax_style: app.theme.scheme.pygments_style.clone(),
            cache_dir: Some(app.cache_dir()),
            fetch_timeout: app.state::<PreviewOptions>().fetch_timeout,
            ..LoadContext::default()
        };
        self.token = Some(app.tasks.run_in_background(
            move |token| {
                ctx.is_cancelled = Arc::new(token.as_fn());
                let outcome = oc_preview::Registry::global().preview(&path, &ctx, preferred.as_deref());
                let candidates = outcome.candidates.iter().map(|c| (c.previewer.id().to_owned(), c.previewer.title().to_owned())).collect();
                let previewer = outcome.previewer_id().map(str::to_owned);
                let body = Some(match (outcome.loaded, outcome.error) {
                    (Some(loaded), _) => Ok((views::prepare(loaded.data), loaded.info)),
                    (None, Some(e)) => Err(e),
                    (None, None) => return None, // cancelled
                });
                Some(Arrived { candidates, previewer, body })
            },
            move |app, arrived| {
                with_view_as::<PreviewView, _>(app, side, ID, |v, app| v.loaded(app, side, request, arrived));
            },
        ));
    }

    fn loaded(&mut self, app: &mut App, side: usize, request: u64, arrived: Option<Arrived>) {
        if request != self.request {
            return; // the cursor moved on meanwhile (dropping `arrived` closes its streams off this thread)
        }
        self.token = None;
        if let Some(t) = self.slow_timer.take() {
            app.cancel_timer(t);
        }
        let (Some(mut arrived), Some(path)) = (arrived, self.path.clone()) else { return };
        let Some(body) = arrived.body.take() else { return };
        self.current = arrived.previewer.take();
        self.chips = std::mem::take(&mut arrived.candidates);
        self.title = display_name(&path);
        let content: Box<dyn ContentView> = match body {
            Ok((prepared, info)) => {
                self.info = info;
                views::make(prepared, &path)
            }
            Err(e) => {
                self.info.clear();
                Box::new(match e {
                    PreviewError::NeedsDownload { .. } => MessageView::new(e.to_string(), Some("Download and preview")),
                    PreviewError::CloudNotDelivered { .. } => MessageView::new(e.to_string(), Some("Try again")),
                    PreviewError::ShowMessage(_) | PreviewError::CannotPreview(_) => MessageView::new(e.to_string(), None),
                    e => MessageView::new(format!("Cannot preview:\n{e}"), None),
                })
            }
        };
        self.set_content(app, side, content);
        if self.play_when_shown.is_some() && self.play_when_shown == self.path {
            self.play_when_shown = None;
            let h = self.handle(side);
            if let Some(c) = self.content.as_mut() {
                c.play(app, h);
            }
        }
    }

    /// A message while waiting, counting the seconds.
    fn show_loading(&mut self, app: &mut App, side: usize, name: &str, text: String) {
        self.title = name.to_owned();
        self.info.clear();
        self.chips.clear();
        self.set_content(app, side, Box::new(MessageView::busy(text)));
        self.tick(app, side);
    }

    fn tick(&mut self, app: &mut App, side: usize) {
        let serial = self.serial;
        self.ticker = Some(app.schedule(Duration::from_secs(1), move |app| {
            with_view_as::<PreviewView, _>(app, side, ID, |v, app| {
                v.ticker = None;
                if v.serial == serial {
                    app.redraw();
                    v.tick(app, side);
                }
            });
        }));
    }

    fn set_content(&mut self, app: &mut App, side: usize, mut content: Box<dyn ContentView>) {
        if let Some(t) = self.ticker.take() {
            app.cancel_timer(t);
        }
        if let Some(mut old) = self.content.take() {
            old.clear(app);
        }
        self.serial += 1;
        let area = self.area(app, side);
        content.shown(app, self.handle(side), area);
        self.content = Some(content);
        app.redraw();
    }

    fn clear(&mut self, app: &mut App) {
        self.stop_timers(app);
        if let Some(t) = self.token.take() {
            t.cancel();
        }
        self.request += 1;
        if let Some(mut old) = self.content.take() {
            old.clear(app);
        }
        self.serial += 1;
        self.path = None;
        self.pending = None;
        self.play_when_shown = None;
        self.current = None;
        self.chips.clear();
        self.title.clear();
        self.info.clear();
    }
}

/// The title: the file name (a root's display path).
fn display_name(path: &VPath) -> String {
    let name = path.name();
    if name.is_empty() { path.display() } else { name }
}

impl SideView for PreviewView {
    fn id(&self) -> &'static str {
        ID
    }

    fn title(&self) -> String {
        "Preview".into()
    }

    fn draw(&self, app: &App, _side: usize, f: &mut Frame, frame: Rect, ctx: &DrawCtx, active: bool) {
        BoxFrame { title: &self.title, subtitle: &self.info, active }.draw(f, frame, ctx);
        let (chips, area) = self.split(frame);
        if let Some(row) = chips {
            self.draw_chips(f, row, ctx);
        }
        if let Some(c) = &self.content {
            c.draw(app, f, area, ctx, active);
        }
    }

    fn handle_event(&mut self, app: &mut App, side: usize, ev: &Event, frame: Rect) -> Outcome {
        let (chips, area) = self.split(frame);
        if let (Event::Mouse(m), Some(row)) = (ev, chips)
            && row.contains(m.col, m.row)
        {
            if m.kind == MouseKind::Down(MouseButton::Left) {
                let hit = self.chip_rects(row).iter().position(|r| r.contains(m.col, m.row));
                if let Some(id) = hit.map(|i| self.chips[i].0.clone()) {
                    self.view_as(app, side, &id);
                }
            }
            return Outcome::Consumed;
        }
        let h = self.handle(side);
        let Some(content) = self.content.as_mut() else { return Outcome::Ignored };
        let outcome = content.handle_event(app, h, ev, area);
        let clicked = content.as_any_mut().downcast_mut::<MessageView>().is_some_and(|m| std::mem::take(&mut m.clicked));
        if clicked {
            self.download_and_preview(app, side);
        }
        outcome
    }

    fn target_changed(&mut self, app: &mut App, side: usize, target: Option<VPath>) {
        self.show_path(app, side, target);
    }

    fn deactivated(&mut self, app: &mut App, _side: usize) {
        self.clear(app);
    }

    fn resized(&mut self, app: &mut App, side: usize, frame: Rect) {
        let area = self.split(frame).1;
        let h = self.handle(side);
        if let Some(c) = self.content.as_mut() {
            c.resized(app, h, area);
        }
    }

    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }
}
