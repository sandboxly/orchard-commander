//! Drawing into a winit window: owns the wgpu surface and keeps it sized.
//! Event handling stays with the caller (oc-ui), which redraws only on change.

use std::sync::Arc;

use winit::window::Window;

use crate::GridError;
use crate::fonts::{FontConfig, FontLibrary};
use crate::frame::Frame;
use crate::renderer::{GridRenderer, PrepareStats};

pub struct WindowRenderer {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    renderer: GridRenderer,
}

impl WindowRenderer {
    /// Set up GPU drawing for `window` with the system fonts. Blocks briefly
    /// while the adapter and device open.
    pub fn new(window: Arc<Window>, font: FontConfig) -> Result<Self, GridError> {
        Self::with_library(window, FontLibrary::system(), font)
    }

    pub fn with_library(
        window: Arc<Window>,
        library: Arc<FontLibrary>,
        font: FontConfig,
    ) -> Result<Self, GridError> {
        let instance = wgpu::Instance::new(
            wgpu::InstanceDescriptor::new_with_display_handle_from_env(Box::new(window.clone())),
        );
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| GridError::Surface(e.to_string()))?;
        let (adapter, device, queue) = crate::headless::open_device(&instance, Some(&surface))?;
        let caps = surface.get_capabilities(&adapter);
        // A non-sRGB format: colors are blended in sRGB space, like Qt.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .or_else(|| caps.formats.first().copied())
            .ok_or_else(|| GridError::Surface("surface supports no formats".into()))?;
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            // Opaque when offered: the grid always paints every pixel, and a
            // compositor must not blend the desktop through text edges.
            alpha_mode: if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
                wgpu::CompositeAlphaMode::Opaque
            } else {
                caps.alpha_modes
                    .first()
                    .copied()
                    .unwrap_or(wgpu::CompositeAlphaMode::Auto)
            },
            view_formats: vec![],
            color_space: Default::default(),
        };
        surface.configure(&device, &config);
        let scale = window.scale_factor() as f32;
        let renderer = GridRenderer::new(device, queue, format, library, font, scale)?;
        Ok(WindowRenderer {
            window,
            surface,
            config,
            renderer,
        })
    }

    pub fn window(&self) -> &Window {
        &self.window
    }

    pub fn renderer(&self) -> &GridRenderer {
        &self.renderer
    }

    pub fn renderer_mut(&mut self) -> &mut GridRenderer {
        &mut self.renderer
    }

    /// Surface size in physical pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// Grid size (columns, rows) that fits the window.
    pub fn grid_size(&self) -> (u16, u16) {
        self.renderer
            .metrics()
            .grid_size(self.config.width, self.config.height)
    }

    /// Call on `WindowEvent::Resized`.
    pub fn resize(&mut self, width: u32, height: u32) {
        let (w, h) = (width.max(1), height.max(1));
        if (w, h) != (self.config.width, self.config.height) {
            self.config.width = w;
            self.config.height = h;
            self.surface.configure(self.renderer.device(), &self.config);
        }
    }

    /// Call on `WindowEvent::ScaleFactorChanged`: re-rasterizes the font.
    pub fn set_scale_factor(&mut self, scale_factor: f64) -> Result<(), GridError> {
        self.renderer.set_scale_factor(scale_factor as f32)
    }

    /// Draw and present `frame`. Returns `Ok(None)` when the frame was
    /// skipped (window occluded, surface being reconfigured) — the caller
    /// should request another redraw.
    pub fn render(&mut self, frame: &Frame) -> Result<Option<PrepareStats>, GridError> {
        let mut reconfigure = false;
        let texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                // Draw this one, reconfigure for the next — after presenting:
                // configuring while a frame is acquired is a wgpu error.
                reconfigure = true;
                t
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(None);
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(self.renderer.device(), &self.config);
                return Ok(None);
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(GridError::Surface(
                    "validation error acquiring the surface texture".into(),
                ));
            }
        };
        let stats = self
            .renderer
            .prepare(frame, (self.config.width, self.config.height));
        let view = texture.texture.create_view(&Default::default());
        let mut encoder = self
            .renderer
            .device()
            .create_command_encoder(&Default::default());
        self.renderer.render(&mut encoder, &view);
        self.renderer.queue().submit([encoder.finish()]);
        self.window.pre_present_notify();
        self.renderer.queue().present(texture);
        if reconfigure {
            self.surface.configure(self.renderer.device(), &self.config);
        }
        Ok(Some(stats))
    }
}
