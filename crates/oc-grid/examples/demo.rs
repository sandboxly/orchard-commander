//! A fake two-panel file manager in a window, to see and time the renderer.
//!
//! Keys: ↑/↓, PageUp/PageDown, Home/End move the cursor bar through 100 000
//! rows; F3 toggles the image preview; +/- change the font size; L cycles
//! the line spacing; Esc or F10 quits. Redraws only after input (no busy loop)
//! and prints frame time statistics every 10 frames.
//!
//! `cargo run -p oc-grid --release --example demo [-- "Font Family"]`

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_grid::winit::application::ApplicationHandler;
use oc_grid::winit::dpi::LogicalSize;
use oc_grid::winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use oc_grid::winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use oc_grid::winit::keyboard::{Key, NamedKey};
use oc_grid::winit::window::{Window, WindowId};
use oc_grid::{FontConfig, Frame, Rendered, WindowRenderer};
use support::FakeFm;

struct App {
    font: FontConfig,
    window: Option<Arc<Window>>,
    gpu: Option<WindowRenderer>,
    frame: Frame,
    fm: FakeFm,
    show_image: bool,
    times: Vec<(Duration, Duration)>,
}

impl App {
    fn redraw(&mut self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn draw(&mut self) {
        let Some(gpu) = self.gpu.as_mut() else { return };
        let t0 = Instant::now();
        let (cols, rows) = gpu.grid_size();
        if (cols, rows) != (self.frame.cols(), self.frame.rows()) {
            self.frame.resize(cols, rows);
        }
        if self.show_image && self.fm.preview.is_none() {
            let (w, h) = (640, 400);
            self.fm.preview = gpu
                .renderer_mut()
                .upload_image(w, h, &support::landscape(w, h))
                .ok();
        } else if !self.show_image
            && let Some(id) = self.fm.preview.take()
        {
            gpu.renderer_mut().remove_image(id);
        }
        let m = gpu.renderer().metrics();
        self.fm.draw(&mut self.frame, (m.width, m.height));
        let t1 = Instant::now();
        match gpu.render(&self.frame) {
            Ok(Rendered::Shown(_)) => {}
            // Drawn again on the next event (showing the window resizes or focuses it).
            Ok(Rendered::Hidden) => return,
            Ok(Rendered::Retry) => {
                self.redraw();
                return;
            }
            Err(e) => {
                eprintln!("render failed: {e}");
                return;
            }
        }
        self.times.push((t1 - t0, t1.elapsed()));
        if self.times.len() == 10 {
            let n = self.times.len() as u32;
            let build: Duration = self.times.iter().map(|t| t.0).sum::<Duration>() / n;
            let render: Duration = self.times.iter().map(|t| t.1).sum::<Duration>() / n;
            let worst = self
                .times
                .iter()
                .map(|t| t.0 + t.1)
                .max()
                .unwrap_or_default();
            println!(
                "{}x{} cells ({}x{} px, {}): frame build {build:?}, render+present {render:?}, worst {worst:?} (avg of {n})",
                cols,
                rows,
                m.width,
                m.height,
                gpu.renderer().font_family(),
            );
            self.times.clear();
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Orchard Commander — oc-grid demo")
            .with_inner_size(LogicalSize::new(1280.0, 800.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        match WindowRenderer::new(window.clone(), self.font.clone()) {
            Ok(gpu) => {
                println!(
                    "font: {} ({:?})",
                    gpu.renderer().font_family(),
                    gpu.renderer().metrics()
                );
                self.gpu = Some(gpu);
            }
            Err(e) => {
                eprintln!("cannot start renderer: {e}");
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);
        self.redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.resize(size.width, size.height);
                }
                self.redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(gpu) = self.gpu.as_mut() {
                    let _ = gpu.set_scale_factor(scale_factor);
                }
                self.redraw();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -y.round() as isize * 3,
                    MouseScrollDelta::PixelDelta(p) => -(p.y / 20.0).round() as isize,
                };
                if lines != 0 {
                    self.fm.move_by(lines);
                    self.redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let page = self.fm.page as isize;
                match event.logical_key.as_ref() {
                    Key::Named(NamedKey::ArrowDown) => self.fm.move_by(1),
                    Key::Named(NamedKey::ArrowUp) => self.fm.move_by(-1),
                    Key::Named(NamedKey::PageDown) => self.fm.move_by(page),
                    Key::Named(NamedKey::PageUp) => self.fm.move_by(-page),
                    Key::Named(NamedKey::Home) => self.fm.move_by(isize::MIN / 2),
                    Key::Named(NamedKey::End) => self.fm.move_by(isize::MAX / 2),
                    Key::Named(NamedKey::F3) => self.show_image = !self.show_image,
                    Key::Named(NamedKey::Escape | NamedKey::F10) => {
                        event_loop.exit();
                        return;
                    }
                    Key::Character(c) if c == "+" || c == "=" || c == "-" || c == "l" => {
                        if let Some(gpu) = self.gpu.as_mut() {
                            let mut font = gpu.renderer().font_config().clone();
                            match c {
                                "-" => font.size -= 1.0,
                                "l" => {
                                    font.line_spacing = if font.line_spacing >= 1.9 {
                                        0.8
                                    } else {
                                        font.line_spacing + 0.1
                                    }
                                }
                                _ => font.size += 1.0,
                            }
                            if let Err(e) = gpu.renderer_mut().set_font(font) {
                                eprintln!("font change failed: {e}");
                            }
                        }
                    }
                    _ => return,
                }
                self.redraw();
            }
            WindowEvent::RedrawRequested => self.draw(),
            _ => {}
        }
    }
}

fn main() {
    let family = std::env::args().nth(1).unwrap_or_default();
    let event_loop = EventLoop::new().expect("event loop");
    // Sleep until an event arrives: redraws happen only on change.
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App {
        font: FontConfig {
            family,
            ..FontConfig::default()
        },
        window: None,
        gpu: None,
        frame: Frame::new(1, 1, support::BG),
        fm: FakeFm::new(100_000),
        show_image: false,
        times: Vec::new(),
    };
    event_loop.run_app(&mut app).expect("run");
}
