//! Offscreen rendering: a frame to an RGBA image, no window. Used by tests,
//! screenshots and anything that needs pixels (thumbnails of the UI).

use std::path::Path;
use std::sync::Arc;

use crate::GridError;
use crate::color::Rgba;
use crate::fonts::{FontConfig, FontLibrary};
use crate::frame::Frame;
use crate::renderer::{GridRenderer, PrepareStats};

/// Format of offscreen targets. Not sRGB: we blend in sRGB space like Qt.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// An RGBA8 image, rows top to bottom, no padding.
#[derive(Clone, PartialEq, Eq)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl std::fmt::Debug for RgbaImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RgbaImage({}x{})", self.width, self.height)
    }
}

impl RgbaImage {
    pub fn pixel(&self, x: u32, y: u32) -> Rgba {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        Rgba::new(
            self.data[i],
            self.data[i + 1],
            self.data[i + 2],
            self.data[i + 3],
        )
    }

    /// Write a PNG (for visual checks and screenshots).
    pub fn save_png(&self, path: impl AsRef<Path>) -> Result<(), GridError> {
        let file = std::fs::File::create(path)?;
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), self.width, self.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| GridError::Png(e.to_string()))?;
        writer
            .write_image_data(&self.data)
            .map_err(|e| GridError::Png(e.to_string()))?;
        writer.finish().map_err(|e| GridError::Png(e.to_string()))
    }
}

/// Pick an adapter and open a device. Shared by the headless and window paths.
pub(crate) fn open_device(
    instance: &wgpu::Instance,
    surface: Option<&wgpu::Surface<'_>>,
) -> Result<(wgpu::Adapter, wgpu::Device, wgpu::Queue), GridError> {
    let request = |force_fallback_adapter| {
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            force_fallback_adapter,
            compatible_surface: surface,
            apply_limit_buckets: false,
        }))
    };
    // No GPU (VMs, CI runners, remote desktops): a software adapter — WARP
    // through DX12 on Windows, llvmpipe through Vulkan or GL on Linux.
    let adapter = request(false)
        .or_else(|first| request(true).map_err(|_| first))
        .map_err(|e| GridError::NoAdapter(e.to_string()))?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("oc-grid"),
        required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
        ..Default::default()
    }))
    .map_err(|e| GridError::Device(e.to_string()))?;
    Ok((adapter, device, queue))
}

/// A renderer with its own device and an offscreen target.
pub struct HeadlessRenderer {
    renderer: GridRenderer,
    target: Option<(wgpu::Texture, (u32, u32))>,
    last_stats: PrepareStats,
    adapter: wgpu::AdapterInfo,
}

impl HeadlessRenderer {
    /// With the system fonts.
    pub fn new(font: FontConfig, scale_factor: f32) -> Result<Self, GridError> {
        Self::with_library(FontLibrary::system(), font, scale_factor)
    }

    pub fn with_library(
        library: Arc<FontLibrary>,
        font: FontConfig,
        scale_factor: f32,
    ) -> Result<Self, GridError> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let (adapter, device, queue) = open_device(&instance, None)?;
        let renderer = GridRenderer::new(device, queue, FORMAT, library, font, scale_factor)?;
        Ok(HeadlessRenderer {
            renderer,
            target: None,
            last_stats: PrepareStats::default(),
            adapter: adapter.get_info(),
        })
    }

    /// The GPU (or software renderer) in use, e.g. for `--self-test`.
    pub fn adapter_info(&self) -> &wgpu::AdapterInfo {
        &self.adapter
    }

    pub fn renderer(&mut self) -> &mut GridRenderer {
        &mut self.renderer
    }

    pub fn last_stats(&self) -> PrepareStats {
        self.last_stats
    }

    /// Pixel size of the frame's grid with the current metrics.
    pub fn frame_size(&self, frame: &Frame) -> (u32, u32) {
        let m = self.renderer.metrics();
        (
            frame.cols() as u32 * m.width,
            frame.rows() as u32 * m.height,
        )
    }

    /// Render the frame exactly covering its grid.
    pub fn render(&mut self, frame: &Frame) -> Result<RgbaImage, GridError> {
        let size = self.frame_size(frame);
        self.render_sized(frame, size)
    }

    /// Render and wait for the GPU, without reading pixels back (benchmarks).
    pub fn render_only(&mut self, frame: &Frame, size: (u32, u32)) -> Result<(), GridError> {
        let encoder = self.encode(frame, size)?;
        let index = self.renderer.queue().submit([encoder.finish()]);
        self.renderer
            .device()
            .poll(wgpu::PollType::Wait {
                submission_index: Some(index),
                timeout: None,
            })
            .map_err(|e| GridError::Device(e.to_string()))?;
        Ok(())
    }

    fn encode(
        &mut self,
        frame: &Frame,
        size: (u32, u32),
    ) -> Result<wgpu::CommandEncoder, GridError> {
        let (w, h) = (size.0.max(1), size.1.max(1));
        if self.target.as_ref().is_none_or(|(_, s)| *s != (w, h)) {
            let texture = self
                .renderer
                .device()
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("oc-grid offscreen"),
                    size: wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: FORMAT,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                });
            self.target = Some((texture, (w, h)));
        }
        self.last_stats = self.renderer.prepare(frame, (w, h));
        let (texture, _) = self.target.as_ref().expect("created above");
        let view = texture.create_view(&Default::default());
        let mut encoder = self
            .renderer
            .device()
            .create_command_encoder(&Default::default());
        self.renderer.render(&mut encoder, &view);
        Ok(encoder)
    }

    /// Render into a `size` pixel image (the area beyond the grid shows the background).
    pub fn render_sized(
        &mut self,
        frame: &Frame,
        size: (u32, u32),
    ) -> Result<RgbaImage, GridError> {
        let (w, h) = (size.0.max(1), size.1.max(1));
        let mut encoder = self.encode(frame, (w, h))?;
        let padded = (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let device = self.renderer.device().clone();
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("oc-grid readback"),
            size: padded as u64 * h as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let (texture, _) = self.target.as_ref().expect("created by encode");
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        let index = self.renderer.queue().submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(index),
                timeout: None,
            })
            .map_err(|e| GridError::Device(e.to_string()))?;
        rx.recv()
            .map_err(|e| GridError::Device(e.to_string()))?
            .map_err(|e| GridError::Device(e.to_string()))?;
        let mapped = buffer
            .slice(..)
            .get_mapped_range()
            .map_err(|e| GridError::Device(e.to_string()))?;
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for row in mapped.chunks_exact(padded as usize) {
            data.extend_from_slice(&row[..(w * 4) as usize]);
        }
        drop(mapped);
        buffer.unmap();
        Ok(RgbaImage {
            width: w,
            height: h,
            data,
        })
    }
}

/// One-shot convenience: render `frame` with the system fonts.
pub fn render_to_image(
    frame: &Frame,
    font: FontConfig,
    scale_factor: f32,
) -> Result<RgbaImage, GridError> {
    HeadlessRenderer::new(font, scale_factor)?.render(frame)
}
