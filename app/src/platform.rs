//! The winit event loop: windows, GPU surfaces, input conversion, wake-ups
//! from workers, timers. All UI state and logic is in `oc_ui::App`.

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use oc_grid::winit;
use oc_grid::{FontConfig, Frame, Rendered, WindowRenderer};
use oc_ui::input::{from_winit, mods_from_winit};
use oc_ui::{App, AppConfig, Event, MAIN_WINDOW, MouseButton, MouseEvent, MouseKind, PlatformRequest, Pointer, WinId};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{CursorIcon, Fullscreen, Window, WindowId};

use crate::Args;
use crate::geometry::{self, QtGeometry};

/// Messages to the event loop from other threads.
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// Task results are waiting (`App::process_tasks`).
    Wake,
    /// A native menu item was clicked (command id).
    Menu(String),
    /// A native menu item's key equivalent was pressed (command id).
    MenuKey(String),
}

struct Win {
    id: WinId,
    window: Arc<Window>,
    gpu: WindowRenderer,
    frame: Frame,
    font: FontConfig,
    mods: ModifiersState,
    cursor: PhysicalPosition<f64>,
    last_cell: (u16, u16),
    scroll_px: f64,
    /// Horizontal trackpad scrolling, like `scroll_px`.
    scroll_px_x: f64,
    ime_allowed: bool,
    /// The mouse pointer shape set on the window.
    pointer: Pointer,
    /// Which of the app's images this window's renderer has.
    images: oc_ui::images::ImageSync,
    /// The window was hidden at the last redraw: try again at this time
    /// (or as soon as it shows).
    hidden_retry: Option<Instant>,
}

/// How often a hidden window looks whether it can draw again, in case the
/// window system doesn't say when it shows.
const HIDDEN_RETRY: Duration = Duration::from_millis(250);

impl Win {
    fn cell_px(&self) -> (f32, f32) {
        let m = self.gpu.renderer().metrics();
        (m.width as f32, m.height as f32)
    }

    fn cell_at(&self, p: PhysicalPosition<f64>) -> (u16, u16) {
        let (cw, ch) = self.cell_px();
        let col = (p.x.max(0.0) / cw as f64) as u16;
        let row = (p.y.max(0.0) / ch as f64) as u16;
        (col.min(self.frame.cols().saturating_sub(1)), row.min(self.frame.rows().saturating_sub(1)))
    }
}

struct Platform {
    app: Option<App>,
    args: Option<Args>,
    proxy: EventLoopProxy<AppEvent>,
    wake_pending: Arc<AtomicBool>,
    windows: HashMap<WindowId, Win>,
    started: Instant,
    first_frame: bool,
    timing: bool,
    /// `OC_TIMING=exit`: quit right after the first frame (startup benchmarks).
    exit_after_first_frame: bool,
    #[cfg(target_os = "macos")]
    menu: Option<crate::macos::NativeMenu>,
    /// The focused window's own menu bar (an editor's File/Edit), while it is in front.
    #[cfg(target_os = "macos")]
    window_menu: Option<(WinId, crate::macos::WindowMenuBar)>,
    /// The main window's content area while neither maximized nor full
    /// screen (logical): Qt's normal geometry, saved with the window state.
    normal_geometry: Option<geometry::Rect>,
}

pub fn run(args: Args, started: Instant) -> ExitCode {
    // Scanning the system fonts takes ~100 ms: start it now, in parallel
    // with creating the event loop, the app state and the window.
    std::thread::spawn(oc_grid::FontLibrary::system);
    let mut builder = EventLoop::<AppEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::EventLoopBuilderExtMacOS;
        builder.with_default_menu(false);
    }
    let event_loop = match builder.build() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot start the event loop: {e}");
            return ExitCode::FAILURE;
        }
    };
    if std::env::var_os("OC_TIMING").is_some() {
        eprintln!("event loop built at {:.1} ms", started.elapsed().as_secs_f64() * 1e3);
    }
    let mut platform = Platform {
        app: None,
        args: Some(args),
        proxy: event_loop.create_proxy(),
        wake_pending: Arc::new(AtomicBool::new(false)),
        windows: HashMap::new(),
        started,
        first_frame: false,
        timing: std::env::var_os("OC_TIMING").is_some(),
        exit_after_first_frame: std::env::var("OC_TIMING").is_ok_and(|v| v == "exit"),
        #[cfg(target_os = "macos")]
        menu: None,
        #[cfg(target_os = "macos")]
        window_menu: None,
        normal_geometry: None,
    };
    if let Err(e) = event_loop.run_app(&mut platform) {
        eprintln!("event loop error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

impl Platform {
    fn app(&mut self) -> &mut App {
        self.app.as_mut().expect("app created in resumed")
    }

    fn win_for(&self, id: WinId) -> Option<WindowId> {
        self.windows.iter().find(|(_, w)| w.id == id).map(|(k, _)| *k)
    }

    fn create_app(&mut self) -> App {
        let proxy = self.proxy.clone();
        let pending = self.wake_pending.clone();
        let wake = Arc::new(move || {
            if !pending.swap(true, Ordering::SeqCst) {
                let _ = proxy.send_event(AppEvent::Wake);
            }
        });
        let args = self.args.take().expect("args");
        let paths = oc_config::ConfigPaths::platform();
        if let Err(e) = paths.migrate_legacy_config() {
            log::warn!("migrating the old config: {e}");
        }
        let mut cfg = AppConfig::new(paths, wake);
        cfg.left = args.left;
        cfg.right = args.right;
        App::new(cfg)
    }

    fn open_window(&mut self, el: &ActiveEventLoop, id: WinId, title: &str, size: LogicalSize<f64>, pos: Option<LogicalPosition<f64>>) {
        let mut attrs = Window::default_attributes().with_title(title).with_inner_size(size).with_visible(true);
        if let Some(p) = pos {
            attrs = attrs.with_position(p);
        }
        #[cfg(not(target_os = "macos"))]
        {
            attrs = attrs.with_window_icon(window_icon());
        }
        let t_window = Instant::now();
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("cannot open a window: {e}");
                el.exit();
                return;
            }
        };
        let font = self.app().theme.font.clone();
        let t_gpu = Instant::now();
        let gpu = match WindowRenderer::new(window.clone(), font.clone()) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("cannot draw into the window: {e}");
                el.exit();
                return;
            }
        };
        if self.timing {
            eprintln!(
                "window {:.1} ms, GPU + fonts {:.1} ms (at {:.1} ms)",
                (t_gpu - t_window).as_secs_f64() * 1e3,
                t_gpu.elapsed().as_secs_f64() * 1e3,
                self.started.elapsed().as_secs_f64() * 1e3
            );
        }
        let (cols, rows) = gpu.grid_size();
        let bg = self.app().theme.colors.background;
        let win = Win {
            id,
            window: window.clone(),
            gpu,
            frame: Frame::new(cols, rows, bg),
            font,
            mods: ModifiersState::empty(),
            cursor: PhysicalPosition::new(0.0, 0.0),
            last_cell: (u16::MAX, u16::MAX),
            scroll_px: 0.0,
            scroll_px_x: 0.0,
            ime_allowed: false,
            pointer: Pointer::Default,
            images: Default::default(),
            hidden_retry: None,
        };
        let cell = win.cell_px();
        let app = self.app();
        app.cell_px = cell;
        app.resize(id, cols, rows);
        if let Some(theme) = window.theme() {
            app.set_system_dark(theme == winit::window::Theme::Dark);
        }
        window.set_ime_allowed(false);
        self.windows.insert(window.id(), win);
        window.request_redraw();
    }

    fn handle_requests(&mut self, el: &ActiveEventLoop) {
        let requests = self.app().take_requests();
        for r in requests {
            match r {
                PlatformRequest::Quit => {
                    self.save_geometry();
                    self.app().shutdown();
                    el.exit();
                    return;
                }
                PlatformRequest::OpenWindow { id, title, cols, rows } => {
                    let (cw, ch) = self.app().cell_px;
                    let scale = self.windows.values().next().map_or(1.0, |w| w.window.scale_factor());
                    let size = LogicalSize::new(cols as f64 * cw as f64 / scale, rows as f64 * ch as f64 / scale);
                    self.open_window(el, id, &title, size, None);
                }
                PlatformRequest::CloseWindow(id) => {
                    if let Some(k) = self.win_for(id) {
                        self.windows.remove(&k);
                    }
                }
                PlatformRequest::SetTitle(id, title) => {
                    if let Some(k) = self.win_for(id) {
                        self.windows[&k].window.set_title(&title);
                    }
                }
                PlatformRequest::RaiseWindow(id) => {
                    if let Some(k) = self.win_for(id) {
                        let w = &self.windows[&k].window;
                        w.set_minimized(false);
                        w.focus_window();
                    }
                }
                PlatformRequest::ThemeChanged => self.apply_theme(),
                PlatformRequest::MenusChanged => {
                    #[cfg(target_os = "macos")]
                    if let (Some(menu), Some(app)) = (&self.menu, &self.app) {
                        menu.update(app);
                        if let Some((id, bar)) = &self.window_menu {
                            bar.update(&app.window_menus(*id));
                        }
                    }
                }
            }
        }
    }

    /// macOS: a window with its own menus (an editor) gets the menu bar
    /// while it is focused; the main window's comes back afterwards.
    #[cfg(target_os = "macos")]
    fn sync_menu_bar(&mut self) {
        let Some(app) = self.app.as_ref() else { return };
        let focused = app.focused;
        let menus = if focused == MAIN_WINDOW { Vec::new() } else { app.window_menus(focused) };
        let want = (!menus.is_empty()).then_some(focused);
        if want == self.window_menu.as_ref().map(|(id, _)| *id) {
            return;
        }
        self.window_menu = None;
        match want {
            Some(id) => self.window_menu = crate::macos::WindowMenuBar::install(app, &menus).map(|bar| (id, bar)),
            None => {
                if let Some(menu) = &self.menu {
                    menu.activate();
                }
            }
        }
    }

    /// Font or colors changed: new glyphs, maybe a new grid size.
    fn apply_theme(&mut self) {
        let Some(app) = self.app.as_mut() else { return };
        let font = app.theme.font.clone();
        let mut sizes = Vec::new();
        for w in self.windows.values_mut() {
            if w.font != font {
                if let Err(e) = w.gpu.renderer_mut().set_font(font.clone()) {
                    log::warn!("font change failed: {e}");
                }
                w.font = font.clone();
            }
            let (cols, rows) = w.gpu.grid_size();
            sizes.push((w.id, cols, rows, w.cell_px()));
            w.window.request_redraw();
        }
        for (id, cols, rows, cell) in sizes {
            app.cell_px = cell;
            app.resize(id, cols, rows);
        }
    }

    /// 1.x's `saveGeometry()` into `extra["geometry"]` (on quit).
    fn save_geometry(&mut self) {
        let Some(k) = self.win_for(MAIN_WINDOW) else { return };
        let w = &self.windows[&k].window;
        let scale = w.scale_factor();
        let logical = |p: PhysicalPosition<i32>, s: PhysicalSize<u32>| {
            let (p, s) = (p.to_logical::<f64>(scale), s.to_logical::<f64>(scale));
            geometry::Rect::new(p.x.round() as i32, p.y.round() as i32, s.width.round() as i32, s.height.round() as i32)
        };
        let inner_pos = w.inner_position().unwrap_or_default();
        let outer_pos = w.outer_position().unwrap_or(inner_pos);
        let content = logical(inner_pos, w.inner_size());
        // Qt numbers the screens primary first (as `saved_geometry` lists them).
        let primary = w.primary_monitor();
        let monitors: Vec<_> =
            primary.iter().cloned().chain(w.available_monitors().filter(|m| Some(m) != primary.as_ref())).collect();
        let current = w.current_monitor();
        let screen = current.as_ref().and_then(|c| monitors.iter().position(|m| m == c)).unwrap_or(0);
        let screen_width = current.map_or(0, |m| m.size().to_logical::<f64>(m.scale_factor()).width.round() as i32);
        let maximized = w.is_maximized();
        let full_screen = w.fullscreen().is_some();
        let normal = if maximized || full_screen { self.normal_geometry.unwrap_or(content) } else { content };
        let blob = QtGeometry {
            frame: logical(outer_pos, w.outer_size()),
            normal,
            screen: screen as i32,
            maximized,
            full_screen,
            screen_width,
            geometry: content,
        }
        .encode();
        self.app().update_settings(move |s| {
            s.extra.insert(geometry::KEY.into(), blob.clone().into());
        });
    }

    /// The main window's content area, when it is in its normal state.
    fn track_normal_geometry(&mut self, key: WindowId) {
        let Some(w) = self.windows.get(&key).filter(|w| w.id == MAIN_WINDOW) else { return };
        let w = &w.window;
        if w.is_maximized() || w.fullscreen().is_some() || w.is_minimized() == Some(true) {
            return;
        }
        let scale = w.scale_factor();
        let Ok(pos) = w.inner_position() else { return };
        let (p, s) = (pos.to_logical::<f64>(scale), w.inner_size().to_logical::<f64>(scale));
        self.normal_geometry =
            Some(geometry::Rect::new(p.x.round() as i32, p.y.round() as i32, s.width.round() as i32, s.height.round() as i32));
    }

    /// How to open the main window: 1.x's saved geometry (or a 2.0 beta's
    /// `window_geometry`), fitted to the connected screens.
    fn saved_geometry(&mut self, el: &ActiveEventLoop) -> geometry::Restore {
        let extra = &self.app().settings.extra;
        if let Some(g) = extra.get(geometry::KEY).and_then(|v| v.as_str()).and_then(QtGeometry::decode) {
            let mut screens: Vec<geometry::Rect> = Vec::new();
            let primary = el.primary_monitor();
            for m in primary.iter().cloned().chain(el.available_monitors().filter(|m| Some(m) != primary.as_ref())) {
                let scale = m.scale_factor();
                let (p, s) = (m.position().to_logical::<f64>(scale), m.size().to_logical::<f64>(scale));
                screens.push(geometry::Rect::new(p.x as i32, p.y as i32, s.width as i32, s.height as i32));
            }
            return geometry::restore(&g, &screens);
        }
        let geo = extra.get(geometry::LEGACY_KEY);
        let num = |k: &str| geo.and_then(|g| g.get(k)).and_then(|v| v.as_f64());
        let mut r = geometry::Restore::default();
        if let (Some(w), Some(h)) = (num("width"), num("height"))
            && w >= 200.0
            && h >= 150.0
        {
            r.size = (w as i32, h as i32);
        }
        if let (Some(x), Some(y)) = (num("x"), num("y")) {
            r.content_pos = Some((x as i32, y as i32));
        }
        r
    }

    fn redraw(&mut self, key: WindowId) {
        let Some(app) = self.app.as_ref() else { return };
        let Some(w) = self.windows.get_mut(&key) else { return };
        let (cols, rows) = w.gpu.grid_size();
        if (cols, rows) != (w.frame.cols(), w.frame.rows()) {
            w.frame.resize(cols, rows);
        }
        let cell = w.cell_px();
        let t_draw = Instant::now();
        app.draw(w.id, &mut w.frame, cell);
        let t_render = Instant::now();
        w.images.sync(app, &w.frame, w.gpu.renderer_mut());
        let rendered = w.gpu.render(&w.frame);
        if self.timing && !self.first_frame {
            eprintln!(
                "redraw at {:.1} ms: draw {:.2} ms, render {:.1} ms",
                (t_draw - self.started).as_secs_f64() * 1e3,
                (t_render - t_draw).as_secs_f64() * 1e3,
                t_render.elapsed().as_secs_f64() * 1e3
            );
        }
        let shown = match rendered {
            Ok(Rendered::Shown(_)) => {
                w.hidden_retry = None;
                Some("")
            }
            // Not now: when it shows again (Occluded(false)), or a look later
            // in case that never comes. Asking again at once would spin.
            Ok(Rendered::Hidden) => {
                w.hidden_retry.get_or_insert_with(|| Instant::now() + HIDDEN_RETRY);
                // Startup benchmarks on a locked screen or in CI still end.
                self.exit_after_first_frame.then_some(" (drawn; the window is hidden, nothing presented)")
            }
            Ok(Rendered::Retry) => {
                w.window.request_redraw();
                None
            }
            Err(e) => {
                log::error!("render failed: {e}");
                None
            }
        };
        if let Some(note) = shown
            && !self.first_frame
        {
            self.first_frame = true;
            if self.timing {
                eprintln!("first frame after {:.1} ms{note}", self.started.elapsed().as_secs_f64() * 1e3);
            }
            if self.exit_after_first_frame {
                let _ = self.proxy.send_event(AppEvent::Menu("app.quit".into()));
            }
        }
        // The input method's candidate window goes where the text cursor is.
        if let Some(c) = w.frame.cursor {
            let (cw, ch) = cell;
            w.window.set_ime_cursor_area(
                PhysicalPosition::new(c.col as f64 * cw as f64, c.row as f64 * ch as f64),
                PhysicalSize::new(cw as f64, ch as f64),
            );
        }
    }

    fn send(&mut self, key: WindowId, ev: Event) {
        let Some(id) = self.windows.get(&key).map(|w| w.id) else { return };
        self.app().handle_event(id, ev);
    }

    fn mouse(&mut self, key: WindowId, kind: MouseKind) {
        let Some(w) = self.windows.get(&key) else { return };
        let (col, row) = w.cell_at(w.cursor);
        let mods = mods_from_winit(w.mods);
        self.send(key, Event::Mouse(MouseEvent { kind, col, row, mods }));
    }

    /// After every batch of events: timers, requests, redraws, IME, wait.
    fn settle(&mut self, el: &ActiveEventLoop) {
        if self.app.is_none() {
            return;
        }
        self.app().tick(Instant::now());
        self.handle_requests(el);
        if self.app.as_ref().is_some_and(|a| a.quitting) {
            return;
        }
        #[cfg(target_os = "macos")]
        self.sync_menu_bar();
        let redraw = self.app().take_redraw();
        let app = self.app.as_ref().expect("app");
        let now = Instant::now();
        let mut deadline = app.next_deadline();
        for w in self.windows.values_mut() {
            if w.hidden_retry.is_some_and(|t| t <= now) {
                w.hidden_retry = None;
                w.window.request_redraw();
            } else if redraw {
                w.window.request_redraw();
            }
            if let Some(t) = w.hidden_retry {
                deadline = Some(deadline.map_or(t, |d| d.min(t)));
            }
            let ime = app.wants_text_input(w.id);
            if ime != w.ime_allowed {
                w.ime_allowed = ime;
                w.window.set_ime_allowed(ime);
            }
            let pointer = app.pointer(w.id);
            if pointer != w.pointer {
                w.pointer = pointer;
                w.window.set_cursor(match pointer {
                    Pointer::Default => CursorIcon::Default,
                    Pointer::Text => CursorIcon::Text,
                });
            }
        }
        match deadline {
            Some(t) => el.set_control_flow(ControlFlow::WaitUntil(t)),
            None => el.set_control_flow(ControlFlow::Wait),
        }
    }
}

impl ApplicationHandler<AppEvent> for Platform {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.app.is_some() {
            return;
        }
        if self.timing {
            eprintln!("event loop running at {:.1} ms", self.started.elapsed().as_secs_f64() * 1e3);
        }
        let app = self.create_app();
        self.app = Some(app);
        let restore = self.saved_geometry(el);
        let size = LogicalSize::new(restore.size.0 as f64, restore.size.1 as f64);
        // winit places the content on macOS, the whole window elsewhere.
        let pos = if cfg!(target_os = "macos") { restore.content_pos } else { restore.outer_pos() };
        self.open_window(el, MAIN_WINDOW, oc_config::APP_NAME, size, pos.map(|(x, y)| LogicalPosition::new(x as f64, y as f64)));
        if let Some(k) = self.win_for(MAIN_WINDOW) {
            self.track_normal_geometry(k);
            let w = &self.windows[&k].window;
            if restore.full_screen {
                w.set_fullscreen(Some(Fullscreen::Borderless(None)));
            } else if restore.maximized {
                w.set_maximized(true);
            }
        }
        #[cfg(target_os = "macos")]
        {
            crate::macos::set_app_icon();
            let proxy = self.proxy.clone();
            self.menu = crate::macos::NativeMenu::install(self.app(), move |id, by_key| {
                let _ = proxy.send_event(if by_key { AppEvent::MenuKey(id) } else { AppEvent::Menu(id) });
            });
        }
        self.settle(el);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: AppEvent) {
        if self.app.is_none() {
            return;
        }
        match event {
            AppEvent::Wake => {
                self.wake_pending.store(false, Ordering::SeqCst);
                self.app().process_tasks();
            }
            AppEvent::Menu(id) => self.app().menu_activated(&id),
            AppEvent::MenuKey(id) => self.app().menu_key_equivalent(&id),
        }
        self.settle(el);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, key: WindowId, event: WindowEvent) {
        if self.app.is_none() || !self.windows.contains_key(&key) {
            return;
        }
        match event {
            WindowEvent::RedrawRequested => {
                self.redraw(key);
                return;
            }
            WindowEvent::CloseRequested => {
                let id = self.windows[&key].id;
                self.app().close_requested(id);
            }
            WindowEvent::Occluded(false) => {
                let w = self.windows.get_mut(&key).expect("checked");
                if w.hidden_retry.take().is_some() {
                    w.window.request_redraw();
                }
            }
            WindowEvent::Moved(_) => {
                self.track_normal_geometry(key);
                return;
            }
            WindowEvent::Resized(size) => {
                self.track_normal_geometry(key);
                let w = self.windows.get_mut(&key).expect("checked");
                w.gpu.resize(size.width, size.height);
                let (cols, rows) = w.gpu.grid_size();
                let id = w.id;
                w.window.request_redraw();
                self.app().resize(id, cols, rows);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let w = self.windows.get_mut(&key).expect("checked");
                if let Err(e) = w.gpu.set_scale_factor(scale_factor) {
                    log::warn!("scale change failed: {e}");
                }
                let cell = w.cell_px();
                self.app().cell_px = cell;
                self.app().redraw();
            }
            WindowEvent::ModifiersChanged(m) => {
                self.windows.get_mut(&key).expect("checked").mods = m.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    let mods = mods_from_winit(self.windows[&key].mods);
                    let unmodified = event.key_without_modifiers();
                    if let Some(k) = from_winit(&event.logical_key, &unmodified, event.text.as_deref(), mods) {
                        self.send(key, Event::Key(k));
                    }
                }
            }
            WindowEvent::Ime(ime) => match ime {
                Ime::Preedit(text, _) => self.send(key, Event::Preedit(text)),
                Ime::Commit(text) => self.send(key, Event::Commit(text)),
                _ => {}
            },
            WindowEvent::CursorMoved { position, .. } => {
                let w = self.windows.get_mut(&key).expect("checked");
                w.cursor = position;
                let cell = w.cell_at(position);
                if cell != w.last_cell {
                    w.last_cell = cell;
                    self.mouse(key, MouseKind::Move);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let b = match button {
                    winit::event::MouseButton::Left => MouseButton::Left,
                    winit::event::MouseButton::Right => MouseButton::Right,
                    winit::event::MouseButton::Middle => MouseButton::Middle,
                    _ => return,
                };
                let kind = if state == ElementState::Pressed { MouseKind::Down(b) } else { MouseKind::Up(b) };
                self.mouse(key, kind);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let w = self.windows.get_mut(&key).expect("checked");
                let (cols, lines) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, y),
                    MouseScrollDelta::PixelDelta(p) => {
                        // Trackpads: whole rows / columns only, keep the remainder.
                        let (cw, ch) = w.cell_px();
                        let (cw, ch) = (cw as f64 * 3.0, ch as f64 * 3.0);
                        w.scroll_px += p.y;
                        w.scroll_px_x += p.x;
                        let n = (w.scroll_px / ch).trunc();
                        w.scroll_px -= n * ch;
                        let m = (w.scroll_px_x / cw).trunc();
                        w.scroll_px_x -= m * cw;
                        (m as f32, n as f32)
                    }
                };
                if lines != 0.0 {
                    self.mouse(key, MouseKind::Scroll(lines));
                }
                // Sideways only when that is the gesture (a vertical swipe drifts a little).
                if cols != 0.0 && cols.abs() > lines.abs() {
                    self.mouse(key, MouseKind::HScroll(cols));
                }
            }
            WindowEvent::Focused(f) => self.send(key, Event::Focus(f)),
            WindowEvent::ThemeChanged(t) => self.app().set_system_dark(t == winit::window::Theme::Dark),
            _ => return,
        }
        self.settle(el);
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        self.settle(el);
    }

    fn exiting(&mut self, _el: &ActiveEventLoop) {
        if let Some(app) = self.app.as_mut()
            && !app.quitting
        {
            app.shutdown();
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn window_icon() -> Option<winit::window::Icon> {
    let png = include_bytes!("../resources/icon.png");
    let decoder = png::Decoder::new(std::io::Cursor::new(&png[..]));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba {
        return None;
    }
    winit::window::Icon::from_rgba(buf[..info.buffer_size()].to_vec(), info.width, info.height).ok()
}
