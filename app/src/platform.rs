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
use oc_ui::{App, AppConfig, Event, MAIN_WINDOW, MouseButton, MouseEvent, MouseKind, PlatformRequest, WinId};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{Window, WindowId};

use crate::{Args, geometry};

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
    ime_allowed: bool,
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
            ime_allowed: false,
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

    /// The saved 1.x geometry (Qt's blob, see `geometry`), as of now.
    fn previous_geometry(&mut self) -> Option<geometry::Geometry> {
        self.app().settings.extra.get("geometry").and_then(|v| v.as_str()).and_then(geometry::decode)
    }

    fn save_geometry(&mut self) {
        let Some(k) = self.win_for(MAIN_WINDOW) else { return };
        let w = &self.windows[&k].window;
        let scale = w.scale_factor();
        let rect = |pos: Option<PhysicalPosition<i32>>, size: PhysicalSize<u32>| {
            let p = pos.unwrap_or_default().to_logical::<f64>(scale);
            let s = size.to_logical::<f64>(scale);
            geometry::Rect { x: p.x.round() as i32, y: p.y.round() as i32, w: s.width.round() as i32, h: s.height.round() as i32 }
        };
        let frame = rect(w.outer_position().ok(), w.outer_size());
        let client = rect(w.inner_position().ok(), w.inner_size());
        let (maximized, full_screen) = (w.is_maximized(), w.fullscreen().is_some());
        let screen_width = w.current_monitor().map_or(0, |m| m.size().to_logical::<f64>(m.scale_factor()).width.round() as i32);
        // Maximized: the size to go back to is the one saved before.
        let normal = match self.previous_geometry() {
            Some(old) if maximized || full_screen => old.normal,
            _ => client,
        };
        let value = geometry::encode(&geometry::Geometry { frame, normal, client, screen_width, maximized, full_screen });
        self.app().update_settings(move |s| {
            s.extra.insert("geometry".into(), value.clone().into());
        });
    }

    /// Size, position and whether to maximize / go full screen, from the
    /// geometry 1.x (or we) saved; 1280×800 where the system likes without.
    fn saved_geometry(&mut self) -> (LogicalSize<f64>, Option<LogicalPosition<f64>>, bool, bool) {
        let Some(g) = self.previous_geometry() else {
            return (LogicalSize::new(1280.0, 800.0), None, false, false);
        };
        let zoomed = g.maximized || g.full_screen;
        let content = if zoomed { g.normal } else { g.client };
        // The position is the frame's (title bar included), the size the content's.
        let (dx, dy) = (g.client.x - g.frame.x, g.client.y - g.frame.y);
        let size = if content.w >= 200 && content.h >= 150 {
            LogicalSize::new(content.w as f64, content.h as f64)
        } else {
            LogicalSize::new(1280.0, 800.0)
        };
        let pos = LogicalPosition::new((content.x - dx) as f64, (content.y - dy) as f64);
        (size, Some(pos), g.maximized, g.full_screen)
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
        }
        match deadline {
            Some(t) => el.set_control_flow(ControlFlow::WaitUntil(t)),
            None => el.set_control_flow(ControlFlow::Wait),
        }
    }
}

/// Whether the title bar of a window at `pos` (logical, frame) would be on
/// one of the screens. Without a list of screens: assume so.
fn on_a_screen(el: &ActiveEventLoop, pos: LogicalPosition<f64>, size: LogicalSize<f64>) -> bool {
    let (x, y) = (pos.x + size.width.min(200.0) / 2.0, pos.y + 10.0);
    let mut monitors = el.available_monitors().peekable();
    monitors.peek().is_none()
        || monitors.any(|m| {
            let scale = m.scale_factor();
            let (at, extent) = (m.position().to_logical::<f64>(scale), m.size().to_logical::<f64>(scale));
            (at.x..at.x + extent.width).contains(&x) && (at.y..at.y + extent.height).contains(&y)
        })
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
        let (size, pos, maximized, full_screen) = self.saved_geometry();
        // Like Qt's restoreGeometry: not off screen (e.g. a monitor since unplugged).
        let pos = pos.filter(|&p| on_a_screen(el, p, size));
        self.open_window(el, MAIN_WINDOW, oc_config::APP_NAME, size, pos);
        if let Some(k) = self.win_for(MAIN_WINDOW) {
            let w = &self.windows[&k].window;
            if maximized {
                w.set_maximized(true);
            }
            if full_screen {
                w.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
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
            WindowEvent::Resized(size) => {
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
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => {
                        // Trackpads: whole rows only, keep the remainder.
                        let ch = w.cell_px().1 as f64 * 3.0;
                        w.scroll_px += p.y;
                        let n = (w.scroll_px / ch).trunc();
                        w.scroll_px -= n * ch;
                        n as f32
                    }
                };
                if lines != 0.0 {
                    self.mouse(key, MouseKind::Scroll(lines));
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
