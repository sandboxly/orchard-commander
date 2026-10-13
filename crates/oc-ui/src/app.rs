//! The UI's root state: settings, theme, windows, dialogs, background tasks,
//! timers. The platform layer (the app binary's winit loop, or a test)
//! feeds it events and draws its windows; everything else happens here.
//!
//! Flow per platform event:
//!
//! ```text
//! winit event ──► App::handle_event(win, Event) ──► modal / main window / other window
//! worker done ──► EventLoopProxy wake ──► App::process_tasks() ──► on_done(app, result)
//! timer due   ──► App::tick(now)
//! then: if App::take_redraw() → request_redraw → App::draw(win, &mut Frame, cell_px)
//!       App::take_requests() → open/close windows, quit, re-apply the font
//! ```

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oc_config::{ConfigPaths, Settings};
use oc_grid::Frame;

use crate::clipboard::Clipboard;
use crate::commands::{CommandId, Registry};
use crate::geom::Rect;
use crate::input::{Event, MouseButton, MouseKind};
use crate::main_window::MainWindow;
use crate::modal::{Modal, ModalEntry, ModalId, ModalOutcome};
use crate::tasks::{self, CancelToken, TaskQueue, Tasks, Waker};
use crate::theme::{SchemeCache, Theme, default_scheme};
use oc_config::ColorScheme;
use crate::widgets::DrawCtx;

/// A window of the app. The main window is [`MAIN_WINDOW`]; others (editors…)
/// are [`UiWindow`]s opened with [`App::open_window`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WinId(pub u32);

pub const MAIN_WINDOW: WinId = WinId(0);

/// What the platform layer has to do after an event.
#[derive(Clone, Debug, PartialEq)]
pub enum PlatformRequest {
    Quit,
    OpenWindow {
        id: WinId,
        title: String,
        cols: u16,
        rows: u16,
    },
    CloseWindow(WinId),
    SetTitle(WinId, String),
    /// Bring a window to the front and focus it (e.g. an editor that is
    /// already open for the file, or one asking about unsaved changes).
    RaiseWindow(WinId),
    /// Font, colors or frame style changed: re-read [`App::theme`].
    ThemeChanged,
    /// Check marks / enabled states may have changed (native menus).
    MenusChanged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

/// A short message in the bottom-right corner.
#[derive(Clone, Debug)]
pub struct Toast {
    pub text: String,
    pub severity: Severity,
    pub until: Instant,
}

/// Whether a handler used an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ignored,
    Consumed,
}

/// A menu of a window's own native menu bar (macOS shows it while the
/// window is focused, instead of the main window's menus).
#[derive(Clone, Debug, PartialEq)]
pub struct WindowMenu {
    pub title: String,
    /// `None` is a separator.
    pub items: Vec<Option<WindowMenuItem>>,
}

/// An item of a [`WindowMenu`]. Choosing it sends `shortcut` to the window
/// as a key press (so the window handles menu and keyboard alike).
#[derive(Clone, Debug, PartialEq)]
pub struct WindowMenuItem {
    pub id: String,
    pub label: String,
    pub shortcut: Option<crate::shortcut::Shortcut>,
    pub enabled: bool,
}

/// A top-level window other than the main one (e.g. an editor).
pub trait UiWindow: Any {
    fn title(&self) -> String;
    fn draw(&self, app: &App, f: &mut Frame, ctx: &DrawCtx);
    fn handle_event(&mut self, app: &mut App, id: WinId, ev: &Event) -> Outcome;
    /// The user asked to close it; false keeps it open (e.g. to ask about unsaved changes).
    fn close_requested(&mut self, _app: &mut App, _id: WinId) -> bool {
        true
    }
    fn wants_text_input(&self) -> bool {
        false
    }
    /// Its own native menus (macOS), shown while it is focused; empty keeps the main ones.
    fn menus(&self) -> Vec<WindowMenu> {
        Vec::new()
    }
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

type Later = Box<dyn FnOnce(&mut App)>;

struct Timer {
    id: u64,
    at: Instant,
    f: Later,
}

/// How the app is set up (the binary and tests differ only here).
pub struct AppConfig {
    pub paths: ConfigPaths,
    pub left: Option<PathBuf>,
    pub right: Option<PathBuf>,
    /// Wakes the event loop when a task result arrives.
    pub wake: Waker,
    pub clipboard: Box<dyn Clipboard>,
    pub system_dark: bool,
    /// Watch local folders for changes.
    pub watch: bool,
    /// Write settings changes to disk.
    pub persist: bool,
    /// The features to register (normally [`crate::features::ALL`]).
    pub features: &'static [fn(&mut Registry)],
}

impl AppConfig {
    pub fn new(paths: ConfigPaths, wake: Waker) -> Self {
        AppConfig {
            paths,
            left: None,
            right: None,
            wake,
            clipboard: Box::new(crate::clipboard::SystemClipboard::default()),
            system_dark: true,
            watch: true,
            persist: true,
            features: crate::features::ALL,
        }
    }
}

type SettingsChange = Arc<dyn Fn(&mut Settings) + Send + Sync>;

enum WriterMsg {
    Change(SettingsChange),
    Flush(mpsc::SyncSender<()>),
}

/// Writes settings on its own thread so the UI never waits for the disk.
/// Like `Settings::update`, each change is applied to a fresh read of the
/// file, so other running instances' settings survive.
struct SettingsWriter {
    tx: mpsc::Sender<WriterMsg>,
}

impl SettingsWriter {
    fn start(path: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel::<WriterMsg>();
        let _ = std::thread::Builder::new().name("oc-settings".into()).spawn(move || {
            for msg in rx {
                match msg {
                    WriterMsg::Change(change) => {
                        let mut on_disk = Settings::load(&path);
                        change(&mut on_disk);
                        if let Err(e) = on_disk.save(&path) {
                            log::warn!("cannot save settings: {e}");
                        }
                    }
                    WriterMsg::Flush(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        SettingsWriter { tx }
    }
}

/// Tracks presses to report double clicks.
#[derive(Default)]
struct Clicks {
    last: Option<(Instant, u16, u16, MouseButton)>,
}

const DOUBLE_CLICK: Duration = Duration::from_millis(450);

/// How long quitting waits for a cancelled file operation (or a running
/// trash job) to stop.
pub(crate) const OPERATION_QUIT_WAIT: Duration = Duration::from_secs(3);

pub struct App {
    pub settings: Settings,
    pub paths: ConfigPaths,
    pub theme: Theme,
    pub system_dark: bool,
    pub tasks: Tasks,
    queue: TaskQueue,
    pub registry: Rc<Registry>,
    pub main: MainWindow,
    windows: Vec<(WinId, Option<Box<dyn UiWindow>>)>,
    modals: Vec<ModalEntry>,
    pub toast: Option<Toast>,
    timers: Vec<Timer>,
    later: Vec<Later>,
    pub clipboard: Box<dyn Clipboard>,
    requests: Vec<PlatformRequest>,
    redraw: bool,
    /// Grid size of each window, as last drawn / resized.
    sizes: Vec<(WinId, (u16, u16))>,
    pub cell_px: (f32, f32),
    pub focused: WinId,
    pub window_focused: bool,
    /// The wheel event being handled is trackpad momentum (the fingers have
    /// lifted; set by the platform layer). The PDF view ignores those, as
    /// 1.x did, so one flick turns one page.
    pub wheel_momentum: bool,
    next_id: u64,
    writer: Option<SettingsWriter>,
    pub watch: bool,
    clicks: Clicks,
    cache_dir: Mutex<Option<PathBuf>>,
    /// The running copy/move/delete, if any (one at a time, like Python).
    pub operation: Option<CancelToken>,
    /// True while the operation's worker thread is still running (it may
    /// outlive `operation`, which is cleared on the UI thread).
    pub(crate) operation_running: Option<Arc<AtomicBool>>,
    pub quitting: bool,
    /// Set while [`quit`](App::quit) asks the other windows to close. A
    /// window that refused in order to ask the user (unsaved changes) calls
    /// `quit()` again once it has closed, or clears this if the user
    /// cancelled.
    pub quit_pending: bool,
    /// Feature state by type (see [`App::state`]).
    states: HashMap<TypeId, Box<dyn Any>>,
}

impl App {
    /// Build the app state and start listing both panels. Cheap: no I/O but
    /// reading the settings file (listings run in the background).
    pub fn new(cfg: AppConfig) -> App {
        let settings = Settings::load(&cfg.paths.settings_file());
        // A built-in scheme needs no disk; a user scheme is read now (the
        // first frame needs it), every other scheme in the background.
        let appearance = settings.appearance();
        let mut schemes = SchemeCache::default();
        if !oc_config::themes::is_builtin(appearance.scheme_name(cfg.system_dark)) {
            schemes.all = Some(oc_config::themes::all_schemes(&cfg.paths.color_schemes_dir()));
        }
        let theme = Theme::with_scheme(&appearance, schemes.get(appearance.scheme_name(cfg.system_dark)).unwrap_or_else(default_scheme));
        let (tasks, queue) = tasks::channel(cfg.wake);
        let registry = Rc::new(Registry::build(cfg.features));
        let main = MainWindow::new(&settings, &registry, cfg.left, cfg.right);
        let writer = cfg.persist.then(|| SettingsWriter::start(cfg.paths.settings_file()));
        let mut app = App {
            settings,
            paths: cfg.paths,
            theme,
            system_dark: cfg.system_dark,
            tasks,
            queue,
            registry,
            main,
            windows: Vec::new(),
            modals: Vec::new(),
            toast: None,
            timers: Vec::new(),
            later: Vec::new(),
            clipboard: cfg.clipboard,
            requests: Vec::new(),
            redraw: true,
            sizes: vec![(MAIN_WINDOW, (120, 40))],
            cell_px: (8.0, 16.0),
            focused: MAIN_WINDOW,
            window_focused: true,
            wheel_momentum: false,
            next_id: 1,
            writer,
            watch: cfg.watch,
            clicks: Clicks::default(),
            cache_dir: Mutex::new(None),
            operation: None,
            operation_running: None,
            quitting: false,
            quit_pending: false,
            states: HashMap::new(),
        };
        let loaded = schemes.all.is_some();
        app.put_state(schemes);
        if !loaded {
            app.refresh_color_schemes(|_| {});
        }
        crate::main_window::start(&mut app);
        let hooks = app.registry.on_start.clone();
        for hook in hooks {
            hook(&mut app);
        }
        app
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    // -- feature state ----------------------------------------------------------

    /// A feature's app-wide state, made with `Default` on first use. Each
    /// feature keeps its own type (e.g. `struct Bookmarks { items: … }` in
    /// its module), so adding one never edits `App`:
    ///
    /// ```ignore
    /// #[derive(Default)]
    /// struct Bookmarks { items: Vec<Bookmark> }
    /// app.state::<Bookmarks>().items.push(b);
    /// ```
    pub fn state<T: Default + 'static>(&mut self) -> &mut T {
        self.states
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Box::new(T::default()))
            .downcast_mut::<T>()
            .expect("state stored under its own type")
    }

    /// The state of type `T`, if a feature made it (drawing, predicates).
    pub fn try_state<T: 'static>(&self) -> Option<&T> {
        self.states.get(&TypeId::of::<T>()).and_then(|b| b.downcast_ref::<T>())
    }

    /// Take a feature's state out (to use it together with `&mut App`); put
    /// it back with [`put_state`](Self::put_state).
    pub fn take_state<T: Default + 'static>(&mut self) -> T {
        std::mem::take(self.state::<T>())
    }

    pub fn put_state<T: 'static>(&mut self, value: T) {
        self.states.insert(TypeId::of::<T>(), Box::new(value));
    }

    // -- the event loop's side ----------------------------------------------

    /// Run task results and deferred work that arrived. Call when woken.
    pub fn process_tasks(&mut self) {
        let jobs = self.queue.drain();
        if !jobs.is_empty() {
            self.redraw = true;
        }
        for job in jobs {
            job(self);
            self.run_later();
        }
        self.run_later();
    }

    /// Run due timers.
    pub fn tick(&mut self, now: Instant) {
        while let Some(i) = self.timers.iter().position(|t| t.at <= now) {
            let timer = self.timers.swap_remove(i);
            (timer.f)(self);
            self.redraw = true;
            self.run_later();
        }
    }

    /// When [`tick`](Self::tick) should be called next.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.timers.iter().map(|t| t.at).min()
    }

    /// Run `f` after the current event is handled (e.g. a command chosen in
    /// a dialog that is closing).
    pub fn later(&mut self, f: impl FnOnce(&mut App) + 'static) {
        self.later.push(Box::new(f));
    }

    fn run_later(&mut self) {
        for _ in 0..64 {
            if self.later.is_empty() {
                return;
            }
            for f in std::mem::take(&mut self.later) {
                f(self);
            }
            self.redraw = true;
        }
    }

    /// Run `f` after `delay`; returns an id for [`cancel_timer`](Self::cancel_timer).
    pub fn schedule(&mut self, delay: Duration, f: impl FnOnce(&mut App) + 'static) -> u64 {
        let id = self.next_id();
        self.timers.push(Timer { id, at: Instant::now() + delay, f: Box::new(f) });
        id
    }

    pub fn cancel_timer(&mut self, id: u64) {
        self.timers.retain(|t| t.id != id);
    }

    /// Something changed: draw again.
    pub fn redraw(&mut self) {
        self.redraw = true;
    }

    /// Whether anything changed since the last call.
    pub fn take_redraw(&mut self) -> bool {
        std::mem::take(&mut self.redraw)
    }

    pub fn take_requests(&mut self) -> Vec<PlatformRequest> {
        std::mem::take(&mut self.requests)
    }

    pub fn request(&mut self, r: PlatformRequest) {
        if !self.requests.contains(&r) {
            self.requests.push(r);
        }
    }

    /// A window's grid size changed (also tells panels how many rows they show).
    pub fn resize(&mut self, win: WinId, cols: u16, rows: u16) {
        match self.sizes.iter_mut().find(|(w, _)| *w == win) {
            Some((_, s)) => *s = (cols, rows),
            None => self.sizes.push((win, (cols, rows))),
        }
        if win == MAIN_WINDOW {
            crate::main_window::resized(self, cols, rows);
        }
        self.redraw = true;
    }

    pub fn size_of(&self, win: WinId) -> (u16, u16) {
        self.sizes.iter().find(|(w, _)| *w == win).map_or((120, 40), |(_, s)| *s)
    }

    pub fn screen(&self, win: WinId) -> Rect {
        let (c, r) = self.size_of(win);
        Rect::screen(c, r)
    }

    /// Feed one input event for window `win`.
    pub fn handle_event(&mut self, win: WinId, ev: Event) {
        let ev = self.track_clicks(ev);
        if let Event::Focus(focused) = ev {
            self.window_focused = focused;
            if focused {
                self.focused = win;
                self.request(PlatformRequest::MenusChanged);
            }
            self.redraw = true;
            return;
        }
        if matches!(ev, Event::Key(_)) {
            self.focused = win;
        }
        let used = if let Some(i) = self.modals.iter().rposition(|m| m.window == win) {
            self.modal_event(i, &ev)
        } else if win == MAIN_WINDOW {
            crate::main_window::handle_event(self, &ev)
        } else {
            self.window_event(win, &ev)
        };
        if used == Outcome::Consumed {
            self.redraw = true;
        }
        self.run_later();
    }

    fn track_clicks(&mut self, ev: Event) -> Event {
        if let Event::Mouse(mut m) = ev
            && let MouseKind::Down(b) = m.kind
        {
            let now = Instant::now();
            let double = self.clicks.last.is_some_and(|(t, c, r, lb)| lb == b && c == m.col && r == m.row && now - t < DOUBLE_CLICK);
            self.clicks.last = if double { None } else { Some((now, m.col, m.row, b)) };
            if double {
                m.kind = MouseKind::DoubleClick(b);
                return Event::Mouse(m);
            }
        }
        ev
    }

    fn modal_event(&mut self, i: usize, ev: &Event) -> Outcome {
        let Some(mut modal) = self.modals[i].modal.take() else {
            return Outcome::Ignored;
        };
        let (id, win) = (self.modals[i].id, self.modals[i].window);
        let screen = self.screen(win);
        let outcome = modal.handle_event(self, ev, screen);
        // The stack may have changed meanwhile (dialogs pushed or closed).
        if let Some(pos) = self.modals.iter().position(|m| m.id == id) {
            if outcome == ModalOutcome::Close {
                self.modals.remove(pos);
            } else {
                self.modals[pos].modal = Some(modal);
            }
        }
        match outcome {
            ModalOutcome::Ignored => Outcome::Ignored,
            _ => {
                self.request(PlatformRequest::MenusChanged);
                Outcome::Consumed
            }
        }
    }

    fn window_event(&mut self, win: WinId, ev: &Event) -> Outcome {
        let Some(slot) = self.windows.iter_mut().find(|(id, _)| *id == win) else {
            return Outcome::Ignored;
        };
        let Some(mut w) = slot.1.take() else {
            return Outcome::Ignored;
        };
        let outcome = w.handle_event(self, win, ev);
        if let Some(slot) = self.windows.iter_mut().find(|(id, _)| *id == win) {
            slot.1 = Some(w);
        }
        outcome
    }

    /// Draw window `win` into `f` (already sized to the window's grid).
    pub fn draw(&self, win: WinId, f: &mut Frame, cell_px: (f32, f32)) {
        let ctx = DrawCtx { theme: &self.theme, cell_px, window_focused: self.window_focused };
        f.clear(self.theme.colors.background);
        f.cursor = None;
        let screen = Rect::screen(f.cols(), f.rows());
        if win == MAIN_WINDOW {
            crate::main_window::draw(self, f, &ctx);
        } else if let Some((_, Some(w))) = self.windows.iter().find(|(id, _)| *id == win) {
            w.draw(self, f, &ctx);
        }
        for m in self.modals.iter().filter(|m| m.window == win) {
            if let Some(modal) = &m.modal {
                // A dialog hides the cursor of what's below it.
                f.cursor = None;
                modal.draw(self, f, screen, &ctx);
            }
        }
        if win == MAIN_WINDOW {
            crate::main_window::draw_toast(self, f, &ctx);
        }
    }

    // -- dialogs ------------------------------------------------------------

    pub fn push_modal(&mut self, win: WinId, modal: Box<dyn Modal>) -> ModalId {
        let id = ModalId(self.next_id());
        self.modals.push(ModalEntry { id, window: win, modal: Some(modal) });
        self.redraw = true;
        self.request(PlatformRequest::MenusChanged);
        id
    }

    /// Push a dialog on the main window.
    pub fn show(&mut self, modal: impl Modal + 'static) -> ModalId {
        self.push_modal(MAIN_WINDOW, Box::new(modal))
    }

    pub fn close_modal(&mut self, id: ModalId) {
        self.modals.retain(|m| m.id != id);
        self.redraw = true;
        self.request(PlatformRequest::MenusChanged);
    }

    pub fn has_modal(&self, win: WinId) -> bool {
        self.modals.iter().any(|m| m.window == win)
    }

    /// Titles of the open dialogs, bottom to top (tests).
    pub fn modal_titles(&self) -> Vec<String> {
        self.modals.iter().filter_map(|m| m.modal.as_ref().map(|d| d.title().to_owned())).collect()
    }

    /// The top dialog's text (tests).
    pub fn top_modal_text(&self) -> Option<String> {
        self.modals.last().and_then(|m| m.modal.as_ref()).map(|d| d.text())
    }

    /// Whether the platform should enable the input method for `win`.
    pub fn wants_text_input(&self, win: WinId) -> bool {
        if let Some(m) = self.modals.iter().rev().find(|m| m.window == win) {
            return m.modal.as_ref().is_some_and(|d| d.wants_text_input());
        }
        if win == MAIN_WINDOW {
            // The terminal takes composed text (dead keys, accents, CJK) for the shell.
            return crate::features::terminal::has_focus(self);
        }
        self.windows.iter().find(|(id, _)| *id == win).and_then(|(_, w)| w.as_ref()).is_some_and(|w| w.wants_text_input())
    }

    /// The mouse pointer's shape for `win` (the platform sets it when it changes).
    pub fn pointer(&self, win: WinId) -> crate::input::Pointer {
        if win != MAIN_WINDOW || self.has_modal(win) {
            return crate::input::Pointer::Default;
        }
        crate::features::terminal::pointer(self)
    }

    // -- windows --------------------------------------------------------------

    /// Open another top-level window.
    pub fn open_window(&mut self, window: Box<dyn UiWindow>, cols: u16, rows: u16) -> WinId {
        let id = WinId(self.next_id() as u32);
        let title = window.title();
        self.windows.push((id, Some(window)));
        self.sizes.push((id, (cols, rows)));
        self.request(PlatformRequest::OpenWindow { id, title, cols, rows });
        id
    }

    /// The platform's close button (or ⌘W) on `win`.
    pub fn close_requested(&mut self, win: WinId) {
        if win == MAIN_WINDOW {
            self.quit();
            return;
        }
        let Some(pos) = self.windows.iter().position(|(id, _)| *id == win) else {
            return;
        };
        let Some(mut w) = self.windows[pos].1.take() else {
            return;
        };
        let close = w.close_requested(self, win);
        if let Some(pos) = self.windows.iter().position(|(id, _)| *id == win) {
            if close {
                self.windows.remove(pos);
                self.modals.retain(|m| m.window != win);
                self.sizes.retain(|(w, _)| *w != win);
                if self.focused == win {
                    self.focused = MAIN_WINDOW;
                }
                self.request(PlatformRequest::CloseWindow(win));
            } else {
                self.windows[pos].1 = Some(w);
            }
        }
    }

    pub fn window_ids(&self) -> Vec<WinId> {
        std::iter::once(MAIN_WINDOW).chain(self.windows.iter().map(|(id, _)| *id)).collect()
    }

    /// Take window `id` out to use it together with `&mut App` (task
    /// results, dialog answers); put it back with [`put_window`](Self::put_window).
    /// Meanwhile the window is skipped by drawing, events and closing.
    pub fn take_window(&mut self, id: WinId) -> Option<Box<dyn UiWindow>> {
        self.windows.iter_mut().find(|(w, _)| *w == id)?.1.take()
    }

    pub fn put_window(&mut self, id: WinId, window: Box<dyn UiWindow>) {
        if let Some(slot) = self.windows.iter_mut().find(|(w, _)| *w == id) {
            slot.1 = Some(window);
        }
    }

    /// A window's state, if it is a `T` (drawing, predicates, tests).
    pub fn window<T: UiWindow>(&self, id: WinId) -> Option<&T> {
        self.windows.iter().find(|(w, _)| *w == id)?.1.as_ref()?.as_any().downcast_ref::<T>()
    }

    /// A window's state, if it is a `T` (for feature commands acting on the focused window).
    pub fn window_mut<T: UiWindow>(&mut self, id: WinId) -> Option<&mut T> {
        self.windows.iter_mut().find(|(w, _)| *w == id)?.1.as_mut()?.as_any_mut().downcast_mut::<T>()
    }

    /// The native menus of window `id` (empty for the main window: it uses the registry's).
    pub fn window_menus(&self, id: WinId) -> Vec<WindowMenu> {
        self.windows.iter().find(|(w, _)| *w == id).and_then(|(_, w)| w.as_ref()).map(|w| w.menus()).unwrap_or_default()
    }

    pub fn window_title(&self, id: WinId) -> String {
        if id == MAIN_WINDOW {
            return oc_config::APP_NAME.to_owned();
        }
        self.windows.iter().find(|(w, _)| *w == id).and_then(|(_, w)| w.as_ref()).map(|w| w.title()).unwrap_or_default()
    }

    // -- commands -------------------------------------------------------------

    /// Run a registered command (no-op for unknown or disabled ones).
    pub fn run_command(&mut self, id: &str) {
        let Some(cmd) = self.registry.get(id) else {
            log::warn!("unknown command {id}");
            return;
        };
        if !cmd.is_enabled(self) {
            return;
        }
        let run = cmd.run.clone();
        run(self);
        self.redraw = true;
        self.request(PlatformRequest::MenusChanged);
        self.run_later();
    }

    /// A native menu item was clicked. While a dialog is open only global
    /// commands (Quit) run, like Qt's menus under a modal dialog: a click on
    /// "Copy" must not reach the dialog as F5.
    pub fn menu_activated(&mut self, id: &str) {
        self.menu_item(id, false);
    }

    /// A native menu item's key equivalent was pressed (macOS hands those to
    /// the menu, not the window). With a dialog open the key goes to the
    /// dialog as the key it is (⌘A selects the text in a field rather than
    /// marking all files); otherwise the command runs.
    pub fn menu_key_equivalent(&mut self, id: &str) {
        self.menu_item(id, true);
    }

    fn menu_item(&mut self, id: &str, by_key: bool) {
        let win = self.focused;
        let Some(cmd) = self.registry.get(id) else {
            // A window's own menu item (the editor's): its shortcut, as a key press.
            let item = self.window_menus(win).into_iter().flat_map(|m| m.items).flatten().find(|i| i.id == id);
            if let Some(s) = item.and_then(|i| i.shortcut) {
                self.handle_event(win, Event::Key(s.to_event()));
            }
            return;
        };
        // A key equivalent without ⌘ (F5, ⌥F7, ⌃`) is a key press like any
        // other: the focused view (the terminal, a dialog) may want it.
        if by_key && let Some(s) = cmd.menu_shortcut().filter(|s| !s.mods.logo) {
            self.handle_event(win, Event::Key(s.to_event()));
            return;
        }
        if !cmd.global {
            if self.has_modal(win) && !by_key {
                return;
            }
            // Main window commands reach dialogs and other windows as their keys.
            if self.has_modal(win) || win != MAIN_WINDOW {
                if let Some(s) = cmd.shortcuts.iter().find(|s| s.mods.logo).or(cmd.shortcuts.first()).copied() {
                    self.handle_event(win, Event::Key(s.to_event()));
                }
                return;
            }
        }
        let id: CommandId = cmd.id;
        self.run_command(id);
    }

    // -- messages ---------------------------------------------------------------

    /// Show a toast (errors stay 8 s, others 3.5 s).
    pub fn notify(&mut self, text: impl Into<String>, severity: Severity) {
        let secs = if severity == Severity::Error { 8.0 } else { 3.5 };
        let until = Instant::now() + Duration::from_secs_f64(secs);
        self.toast = Some(Toast { text: text.into(), severity, until });
        self.schedule(Duration::from_secs_f64(secs), |app| {
            if app.toast.as_ref().is_some_and(|t| t.until <= Instant::now()) {
                app.toast = None;
            }
        });
        self.redraw = true;
    }

    pub fn info(&mut self, text: impl Into<String>) {
        self.notify(text, Severity::Info);
    }

    pub fn warn(&mut self, text: impl Into<String>) {
        self.notify(text, Severity::Warning);
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.notify(text, Severity::Error);
    }

    // -- settings ----------------------------------------------------------------

    /// Change settings: applied now, written to disk in the background (only
    /// what `change` touches, on top of the file's current contents), then
    /// the app is brought up to date (theme, hooks).
    pub fn update_settings(&mut self, change: impl Fn(&mut Settings) + Send + Sync + 'static) {
        let before = self.settings.appearance();
        change(&mut self.settings);
        if let Some(w) = &self.writer {
            let _ = w.tx.send(WriterMsg::Change(Arc::new(change)));
        }
        if self.settings.appearance() != before {
            self.reload_theme();
        }
        let hooks = self.registry.on_settings_changed.clone();
        for hook in hooks {
            hook(self);
        }
        self.redraw = true;
        self.request(PlatformRequest::MenusChanged);
    }

    /// Whether changes are written to disk (false for `--screenshot`).
    pub fn persists(&self) -> bool {
        self.writer.is_some()
    }

    /// Wait until all settings changes are on disk (tests, quitting).
    pub fn flush_settings(&self) {
        if let Some(w) = &self.writer {
            let (tx, rx) = mpsc::sync_channel(1);
            if w.tx.send(WriterMsg::Flush(tx)).is_ok() {
                let _ = rx.recv_timeout(Duration::from_secs(5));
            }
        }
    }

    /// Re-resolve the theme from the settings (schemes from the cache; see
    /// [`SchemeCache`]).
    pub fn reload_theme(&mut self) {
        let appearance = self.settings.appearance();
        let scheme = self.color_scheme(appearance.scheme_name(self.system_dark));
        self.theme = Theme::with_scheme(&appearance, scheme);
        self.request(PlatformRequest::ThemeChanged);
        self.redraw = true;
    }

    /// The color scheme called `name` (the default if there is none).
    pub fn color_scheme(&mut self, name: &str) -> ColorScheme {
        if let Some(s) = self.state::<SchemeCache>().get(name) {
            return s;
        }
        // A user scheme before the background read finished: read it now.
        let all = oc_config::themes::all_schemes(&self.paths.color_schemes_dir());
        self.state::<SchemeCache>().all = Some(all);
        self.state::<SchemeCache>().get(name).unwrap_or_else(default_scheme)
    }

    /// Every color scheme as last read (built-ins only until the user's
    /// folder has been read).
    pub fn color_schemes(&self) -> Vec<ColorScheme> {
        match self.try_state::<SchemeCache>().and_then(|c| c.all.clone()) {
            Some(all) => all,
            None => oc_config::themes::builtin_schemes().to_vec(),
        }
    }

    /// Re-read the user's color schemes in the background, then run `then`.
    pub fn refresh_color_schemes(&mut self, then: impl FnOnce(&mut App) + Send + 'static) {
        let dir = self.paths.color_schemes_dir();
        self.tasks.run_in_background(
            move |_| oc_config::themes::all_schemes(&dir),
            move |app, all| {
                app.state::<SchemeCache>().all = Some(all);
                then(app);
            },
        );
    }

    /// The OS switched between light and dark mode.
    pub fn set_system_dark(&mut self, dark: bool) {
        if dark != self.system_dark {
            self.system_dark = dark;
            if self.settings.appearance().follow_system_appearance {
                self.reload_theme();
            }
        }
    }

    /// Where copies of non-local files go (opening with the default app);
    /// a temp folder made on first use, removed by [`shutdown`](Self::shutdown).
    pub fn cache_dir(&self) -> PathBuf {
        let mut dir = self.cache_dir.lock().unwrap_or_else(|p| p.into_inner());
        dir.get_or_insert_with(|| {
            tempfile_dir().unwrap_or_else(|| std::env::temp_dir().join(format!("orchard-commander-{}", std::process::id())))
        })
        .clone()
    }

    // -- quitting ------------------------------------------------------------------

    /// Quit: other windows may refuse (unsaved changes); the running file
    /// operation is cancelled.
    pub fn quit(&mut self) {
        self.quit_pending = true;
        for id in self.window_ids().into_iter().skip(1) {
            self.close_requested(id);
            if self.windows.iter().any(|(w, _)| *w == id) {
                return; // it refused (asking first): one question at a time, like 1.x
            }
        }
        self.quit_pending = false;
        if let Some(op) = &self.operation {
            op.cancel();
        }
        self.quitting = true;
        self.request(PlatformRequest::Quit);
    }

    /// Last things before the process exits: the running file operation
    /// wound down (bounded, like 1.x's `wait(3000)`: a cancelled copy removes
    /// its partial file instead of leaving it truncated), settings on disk,
    /// temp files gone.
    pub fn shutdown(&mut self) {
        self.modals.clear(); // unanswered prompts: workers see a dropped reply
        if let Some(op) = &self.operation {
            op.cancel();
        }
        if let Some(running) = self.operation_running.take() {
            let end = Instant::now() + OPERATION_QUIT_WAIT;
            while running.load(Ordering::SeqCst) && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let hooks = self.registry.on_shutdown.clone();
        for hook in hooks {
            hook(self);
        }
        self.flush_settings();
        if let Some(dir) = self.cache_dir.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn tempfile_dir() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("orchard-commander-ui-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}
