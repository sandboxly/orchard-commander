//! The GPU side: turns a [`Frame`] into a handful of instanced draw calls.
//!
//! Per frame: one draw for cell backgrounds and under-text overlays, one per
//! visible image, one for all glyphs, one for decorations (underline,
//! strikethrough, thin cursors, over-text overlays). Instance data is rebuilt
//! from the frame every time — at ~30 bytes per cell that is far cheaper than
//! diffing — and glyph rasterization is cached in two atlas textures.

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use rustc_hash::FxHashMap;

use crate::GridError;
use crate::atlas::AtlasFull;
use crate::color::Rgba;
use crate::fonts::{BOLD, CellMetrics, FontConfig, FontLibrary, FontSet, ITALIC, STRONG, StyleBits};
use crate::frame::{Area, CellRect, CursorShape, Flags, Frame, ImageId, Layer, PxRect};
use crate::glyph_cache::GlyphCache;

const INITIAL_MASK_ATLAS: u32 = 1024;
const INITIAL_COLOR_ATLAS: u32 = 512;
/// The atlases grow up to this (or the device limit), then start over.
const MAX_ATLAS: u32 = 8192;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct QuadInstance {
    rect: [f32; 4],
    color: Rgba,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GlyphInstance {
    rect: [f32; 4],
    uv: [u16; 4],
    color: Rgba,
    /// 0 = coverage mask tinted with `color`, 1 = color glyph.
    kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ImageInstance {
    rect: [f32; 4],
    uv: [f32; 4],
    opacity: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    viewport: [f32; 2],
    mask_size: [f32; 2],
    color_size: [f32; 2],
    pad: [f32; 2],
}

/// Counts from the last [`GridRenderer::prepare`], for profiling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PrepareStats {
    pub quads: usize,
    pub glyphs: usize,
    pub images: usize,
    /// Glyphs rasterized and uploaded this frame (0 once the cache is warm).
    pub uploads: usize,
}

/// A vertex buffer that grows (doubling) to fit the instances.
struct InstanceBuffer {
    buffer: wgpu::Buffer,
    capacity: u64,
    label: &'static str,
}

impl InstanceBuffer {
    fn new(device: &wgpu::Device, label: &'static str) -> Self {
        let capacity = 64 * 1024;
        InstanceBuffer {
            buffer: Self::create(device, label, capacity),
            capacity,
            label,
        }
    }

    fn create(device: &wgpu::Device, label: &'static str, size: u64) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn write(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let needed = bytes.len() as u64;
        if needed > self.capacity {
            self.capacity = needed.next_power_of_two();
            self.buffer = Self::create(device, self.label, self.capacity);
        }
        queue.write_buffer(&self.buffer, 0, bytes);
    }
}

struct GpuImage {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    size: (u32, u32),
}

/// Renders frames into any color target of the format it was created for:
/// a window surface ([`crate::WindowRenderer`]) or an offscreen texture
/// ([`crate::HeadlessRenderer`]).
pub struct GridRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    format: wgpu::TextureFormat,
    quad_pipeline: wgpu::RenderPipeline,
    glyph_pipeline: wgpu::RenderPipeline,
    image_pipeline: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    main_layout: wgpu::BindGroupLayout,
    image_layout: wgpu::BindGroupLayout,
    main_group: wgpu::BindGroup,
    sampler: wgpu::Sampler,
    mask_tex: wgpu::Texture,
    color_tex: wgpu::Texture,
    max_atlas: u32,
    quad_buf: InstanceBuffer,
    glyph_buf: InstanceBuffer,
    image_buf: InstanceBuffer,
    cache: GlyphCache,
    library: Arc<FontLibrary>,
    font: FontConfig,
    scale_factor: f32,
    images: FxHashMap<u64, GpuImage>,
    next_image: u64,
    // Rebuilt by every prepare(); kept to reuse their allocations.
    quads: Vec<QuadInstance>,
    above: Vec<QuadInstance>,
    glyphs: Vec<GlyphInstance>,
    image_instances: Vec<ImageInstance>,
    image_draws: Vec<u64>,
    below_count: u32,
    clear: Rgba,
    viewport: (u32, u32),
}

impl GridRenderer {
    /// Create a renderer drawing into `format` targets. `scale_factor` is the
    /// display's HiDPI factor: the font size is in points, cells in physical pixels.
    pub fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        format: wgpu::TextureFormat,
        library: Arc<FontLibrary>,
        font: FontConfig,
        scale_factor: f32,
    ) -> Result<Self, GridError> {
        let font = font.sanitized();
        let fonts = FontSet::new(library.clone(), &font, scale_factor).ok_or(GridError::NoFont)?;
        let max_atlas = device.limits().max_texture_dimension_2d.min(MAX_ATLAS);
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("oc-grid shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let main_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("oc-grid globals"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                texture_entry(2),
                texture_entry(3),
            ],
        });
        let image_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("oc-grid image"),
            entries: &[texture_entry(0)],
        });
        let main_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("oc-grid main"),
            bind_group_layouts: &[Some(&main_layout)],
            immediate_size: 0,
        });
        let image_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("oc-grid images"),
            bind_group_layouts: &[Some(&main_layout), Some(&image_layout)],
            immediate_size: 0,
        });
        let quad_attrs = wgpu::vertex_attr_array![0 => Float32x4, 1 => Unorm8x4];
        let glyph_attrs =
            wgpu::vertex_attr_array![0 => Float32x4, 1 => Uint16x4, 2 => Unorm8x4, 3 => Uint32];
        let image_attrs = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32];
        let pipeline = |label,
                        layout: &wgpu::PipelineLayout,
                        vs,
                        fs,
                        stride: usize,
                        attrs: &[wgpu::VertexAttribute]| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: stride as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: attrs,
                    })],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let quad_pipeline = pipeline(
            "oc-grid quads",
            &main_pl,
            "vs_quad",
            "fs_quad",
            size_of::<QuadInstance>(),
            &quad_attrs,
        );
        let glyph_pipeline = pipeline(
            "oc-grid glyphs",
            &main_pl,
            "vs_glyph",
            "fs_glyph",
            size_of::<GlyphInstance>(),
            &glyph_attrs,
        );
        let image_pipeline = pipeline(
            "oc-grid images",
            &image_pl,
            "vs_image",
            "fs_image",
            size_of::<ImageInstance>(),
            &image_attrs,
        );
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("oc-grid globals"),
            size: size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("oc-grid sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let mask_size = INITIAL_MASK_ATLAS.min(max_atlas);
        let color_size = INITIAL_COLOR_ATLAS.min(max_atlas);
        let mask_tex = atlas_texture(&device, mask_size, wgpu::TextureFormat::R8Unorm);
        let color_tex = atlas_texture(&device, color_size, wgpu::TextureFormat::Rgba8Unorm);
        let main_group = main_bind_group(
            &device,
            &main_layout,
            &globals,
            &sampler,
            &mask_tex,
            &color_tex,
        );
        let quad_buf = InstanceBuffer::new(&device, "oc-grid quad instances");
        let glyph_buf = InstanceBuffer::new(&device, "oc-grid glyph instances");
        let image_buf = InstanceBuffer::new(&device, "oc-grid image instances");
        Ok(GridRenderer {
            cache: GlyphCache::new(fonts, mask_size, color_size),
            device,
            queue,
            format,
            quad_pipeline,
            glyph_pipeline,
            image_pipeline,
            globals,
            main_layout,
            image_layout,
            main_group,
            sampler,
            mask_tex,
            color_tex,
            max_atlas,
            quad_buf,
            glyph_buf,
            image_buf,
            library,
            font,
            scale_factor,
            images: FxHashMap::default(),
            next_image: 1,
            quads: Vec::new(),
            above: Vec::new(),
            glyphs: Vec::new(),
            image_instances: Vec::new(),
            image_draws: Vec::new(),
            below_count: 0,
            clear: Rgba::BLACK,
            viewport: (1, 1),
        })
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    pub fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    /// Cell size, baseline etc. in physical pixels.
    pub fn metrics(&self) -> CellMetrics {
        self.cache.fonts.metrics
    }

    /// The family actually used (the configured one, or the fallback default).
    pub fn font_family(&self) -> &str {
        &self.cache.fonts.family
    }

    pub fn font_config(&self) -> &FontConfig {
        &self.font
    }

    pub fn scale_factor(&self) -> f32 {
        self.scale_factor
    }

    /// Change font family/size/spacing; clears the glyph cache.
    pub fn set_font(&mut self, font: FontConfig) -> Result<(), GridError> {
        let font = font.sanitized();
        let fonts = FontSet::new(self.library.clone(), &font, self.scale_factor)
            .ok_or(GridError::NoFont)?;
        self.cache.replace_fonts(fonts);
        self.font = font;
        Ok(())
    }

    /// The window moved to a display with another HiDPI factor.
    pub fn set_scale_factor(&mut self, scale_factor: f32) -> Result<(), GridError> {
        if (scale_factor - self.scale_factor).abs() < f32::EPSILON {
            return Ok(());
        }
        let fonts = FontSet::new(self.library.clone(), &self.font, scale_factor)
            .ok_or(GridError::NoFont)?;
        self.cache.replace_fonts(fonts);
        self.scale_factor = scale_factor;
        Ok(())
    }

    /// Current (coverage, color) glyph atlas sizes in pixels — for diagnostics.
    pub fn atlas_sizes(&self) -> (u32, u32) {
        (self.cache.mask.size, self.cache.color.size)
    }

    /// Pixel rectangle of a cell rectangle.
    pub fn cell_rect_px(&self, r: CellRect) -> PxRect {
        let m = self.metrics();
        PxRect::new(
            r.col as f32 * m.width as f32,
            r.row as f32 * m.height as f32,
            r.cols as f32 * m.width as f32,
            r.rows as f32 * m.height as f32,
        )
    }

    fn area_px(&self, area: Area) -> PxRect {
        match area {
            Area::Px(r) => r,
            Area::Cells(r) => self.cell_rect_px(r),
        }
    }

    // -- images -----------------------------------------------------------

    /// Upload straight-alpha RGBA8 pixels (`width * height * 4` bytes).
    pub fn upload_image(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<ImageId, GridError> {
        let id = self.next_image;
        self.next_image += 1;
        self.store_image(id, width, height, rgba)?;
        Ok(ImageId(id))
    }

    /// Replace an image's pixels (a new video frame); the size may change.
    pub fn update_image(
        &mut self,
        id: ImageId,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<(), GridError> {
        if !self.images.contains_key(&id.0) {
            return Err(GridError::UnknownImage);
        }
        self.store_image(id.0, width, height, rgba)
    }

    pub fn remove_image(&mut self, id: ImageId) {
        if let Some(img) = self.images.remove(&id.0) {
            img.texture.destroy();
        }
    }

    fn store_image(
        &mut self,
        id: u64,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<(), GridError> {
        let max = self.device.limits().max_texture_dimension_2d;
        if width == 0 || height == 0 || width > max || height > max {
            return Err(GridError::ImageSize { width, height });
        }
        if rgba.len() != width as usize * height as usize * 4 {
            return Err(GridError::ImageData {
                expected: width as usize * height as usize * 4,
                got: rgba.len(),
            });
        }
        let reuse = self
            .images
            .get(&id)
            .is_some_and(|img| img.size == (width, height));
        if !reuse {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("oc-grid image"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("oc-grid image"),
                layout: &self.image_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                }],
            });
            if let Some(old) = self.images.insert(
                id,
                GpuImage {
                    texture,
                    bind_group,
                    size: (width, height),
                },
            ) {
                old.texture.destroy();
            }
        }
        let img = &self.images[&id];
        write_texture(&self.queue, &img.texture, 0, 0, width, height, 4, rgba);
        Ok(())
    }

    // -- frame building ---------------------------------------------------

    /// Build and upload everything needed to draw `frame` into a target of
    /// `size` physical pixels. Call before [`Self::render`] / [`Self::draw`].
    pub fn prepare(&mut self, frame: &Frame, size: (u32, u32)) -> PrepareStats {
        self.viewport = (size.0.max(1), size.1.max(1));
        let mut uploads = 0;
        for attempt in 0..3 {
            let last = attempt == 2;
            match self.build(frame, last) {
                Ok(()) => break,
                Err(AtlasFull) => {
                    // Grow the atlases (or, at the limit, start them over) and rebuild.
                    self.grow_atlases();
                }
            }
        }
        for (atlas, tex, bpp) in [
            (&mut self.cache.mask, &self.mask_tex, 1),
            (&mut self.cache.color, &self.color_tex, 4),
        ] {
            uploads += atlas.pending.len();
            for up in atlas.pending.drain(..) {
                write_texture(&self.queue, tex, up.x, up.y, up.w, up.h, bpp, &up.data);
            }
        }
        let globals = Globals {
            viewport: [self.viewport.0 as f32, self.viewport.1 as f32],
            mask_size: [self.cache.mask.size as f32; 2],
            color_size: [self.cache.color.size as f32; 2],
            pad: [0.0; 2],
        };
        self.queue
            .write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));
        self.quad_buf
            .write(&self.device, &self.queue, bytemuck::cast_slice(&self.quads));
        self.glyph_buf.write(
            &self.device,
            &self.queue,
            bytemuck::cast_slice(&self.glyphs),
        );
        self.image_buf.write(
            &self.device,
            &self.queue,
            bytemuck::cast_slice(&self.image_instances),
        );
        self.clear = frame.background;
        PrepareStats {
            quads: self.quads.len(),
            glyphs: self.glyphs.len(),
            images: self.image_draws.len(),
            uploads,
        }
    }

    /// Grow the atlas that ran out (only that one: an 8192² RGBA atlas is
    /// 256 MB of VRAM, not to be spent because the mask atlas filled up).
    fn grow_atlases(&mut self) {
        let (mask_full, color_full) = match (self.cache.mask.full, self.cache.color.full) {
            (false, false) => (true, true),
            flags => flags,
        };
        let grow = |size: u32, full: bool| {
            if full {
                (size * 2).min(self.max_atlas)
            } else {
                size
            }
        };
        let mask = grow(self.cache.mask.size, mask_full);
        let color = grow(self.cache.color.size, color_full);
        if mask != self.cache.mask.size {
            self.mask_tex.destroy();
            self.mask_tex = atlas_texture(&self.device, mask, wgpu::TextureFormat::R8Unorm);
        }
        if color != self.cache.color.size {
            self.color_tex.destroy();
            self.color_tex = atlas_texture(&self.device, color, wgpu::TextureFormat::Rgba8Unorm);
        }
        self.cache.clear(mask, color);
        self.main_group = main_bind_group(
            &self.device,
            &self.main_layout,
            &self.globals,
            &self.sampler,
            &self.mask_tex,
            &self.color_tex,
        );
    }

    /// Fill the instance lists from the frame. With `tolerate_full`, glyphs
    /// that don't fit the atlas are skipped instead of failing.
    fn build(&mut self, frame: &Frame, tolerate_full: bool) -> Result<(), AtlasFull> {
        let m = self.cache.fonts.metrics;
        let (cw, ch) = (m.width as f32, m.height as f32);
        self.quads.clear();
        self.above.clear();
        self.glyphs.clear();
        self.image_instances.clear();
        self.image_draws.clear();

        let block_cursor = frame.cursor.filter(|c| {
            c.shape == CursorShape::Block && c.col < frame.cols() && c.row < frame.rows()
        });
        let bg_default = frame.background;

        for row in 0..frame.rows() {
            let y = row as f32 * ch;
            let cells = frame.row(row);
            // Backgrounds: runs of equal color become one quad.
            let mut run: Option<(u16, Rgba)> = None;
            for (col, cell) in cells.iter().enumerate() {
                let col = col as u16;
                let mut bg = if cell.flags.contains(Flags::INVERSE) {
                    if cell.fg.is_transparent() {
                        bg_default
                    } else {
                        cell.fg
                    }
                } else {
                    cell.bg
                };
                // A block cursor on a wide char covers both of its cells.
                if let Some(c) = block_cursor
                    && c.row == row
                    && (c.col == col
                        || (c.col + 1 == col
                            && cell.flags.contains(Flags::WIDE_CONT)
                            && cells[c.col as usize].flags.contains(Flags::WIDE)))
                {
                    bg = c.color;
                }
                let visible = !bg.is_transparent() && bg != bg_default;
                if !matches!(run, Some((_, color)) if visible && color == bg) {
                    if let Some((start, color)) = run.take() {
                        push_quad(
                            &mut self.quads,
                            start as f32 * cw,
                            y,
                            (col - start) as f32 * cw,
                            ch,
                            color,
                        );
                    }
                    if visible {
                        run = Some((col, bg));
                    }
                }
            }
            if let Some((start, color)) = run {
                push_quad(
                    &mut self.quads,
                    start as f32 * cw,
                    y,
                    (frame.cols() - start) as f32 * cw,
                    ch,
                    color,
                );
            }
        }

        for r in &frame.rects {
            let px = self.area_px(r.area);
            let list = if r.layer == Layer::BelowText {
                &mut self.quads
            } else {
                &mut self.above
            };
            push_quad(list, px.x, px.y, px.w, px.h, r.color);
        }

        // Images go between backgrounds and text.
        let viewport = PxRect::new(0.0, 0.0, self.viewport.0 as f32, self.viewport.1 as f32);
        for p in &frame.images {
            if !self.images.contains_key(&p.image.0) {
                continue;
            }
            let dest = self.area_px(p.dest);
            if dest.is_empty() {
                continue;
            }
            let clip = p
                .clip
                .map_or(viewport, |c| self.area_px(c).intersect(&viewport));
            let vis = dest.intersect(&clip);
            if vis.is_empty() {
                continue;
            }
            let uv = [
                (vis.x - dest.x) / dest.w,
                (vis.y - dest.y) / dest.h,
                vis.w / dest.w,
                vis.h / dest.h,
            ];
            self.image_instances.push(ImageInstance {
                rect: [vis.x, vis.y, vis.w, vis.h],
                uv,
                opacity: p.opacity.clamp(0.0, 1.0),
            });
            self.image_draws.push(p.image.0);
        }

        // Glyphs and decorations.
        for row in 0..frame.rows() {
            let y = row as f32 * ch;
            for (col, cell) in frame.row(row).iter().enumerate() {
                let x = col as f32 * cw;
                let flags = cell.flags;
                // A wide char's continuation is decorated by the char itself
                // (span 2); drawing it again would double translucent lines.
                let decorated = (flags.contains(Flags::UNDERLINE)
                    || flags.contains(Flags::STRIKETHROUGH))
                    && !flags.contains(Flags::WIDE_CONT);
                if cell.is_blank() && !decorated {
                    continue;
                }
                let mut fg = if flags.contains(Flags::INVERSE) {
                    if cell.bg.is_transparent() {
                        bg_default
                    } else {
                        cell.bg
                    }
                } else {
                    cell.fg
                };
                if let Some(c) = block_cursor
                    && c.row == row
                    && c.col == col as u16
                {
                    fg = c.text.unwrap_or(if cell.bg.is_transparent() {
                        bg_default
                    } else {
                        cell.bg
                    });
                }
                if flags.contains(Flags::DIM) {
                    fg.a /= 2;
                }
                let span = if flags.contains(Flags::WIDE) {
                    2.0
                } else {
                    1.0
                };
                if decorated {
                    let t = m.underline_thickness as f32;
                    if flags.contains(Flags::UNDERLINE) {
                        push_quad(
                            &mut self.above,
                            x,
                            y + m.underline_y as f32,
                            cw * span,
                            t,
                            fg,
                        );
                    }
                    if flags.contains(Flags::STRIKETHROUGH) {
                        push_quad(&mut self.above, x, y + m.strike_y as f32, cw * span, t, fg);
                    }
                }
                if cell.is_blank() {
                    continue;
                }
                let style: StyleBits = (if flags.contains(Flags::BOLD) { BOLD } else { 0 })
                    | (if flags.contains(Flags::ITALIC) {
                        ITALIC
                    } else {
                        0
                    })
                    | (if flags.contains(Flags::STRONG) {
                        STRONG
                    } else {
                        0
                    });
                match frame.cluster_str(cell) {
                    None => match self.cache.glyph(cell.ch, style) {
                        Ok(Some(g)) => push_glyph(&mut self.glyphs, x, y, &g, fg),
                        Ok(None) => {}
                        Err(e) if !tolerate_full => return Err(e),
                        Err(_) => {}
                    },
                    Some(text) => match self.cache.cluster(text, style) {
                        Ok(gs) => {
                            for g in gs {
                                push_glyph(&mut self.glyphs, x, y, g, fg);
                            }
                        }
                        Err(e) if !tolerate_full => return Err(e),
                        Err(_) => {}
                    },
                }
            }
        }

        if let Some(c) = frame
            .cursor
            .filter(|c| c.col < frame.cols() && c.row < frame.rows())
        {
            let (x, y) = (c.col as f32 * cw, c.row as f32 * ch);
            let t = crate::special::thin(m.width) as f32;
            match c.shape {
                CursorShape::Block => {}
                CursorShape::Beam => push_quad(&mut self.above, x, y, t.max(2.0), ch, c.color),
                CursorShape::Underline => {
                    let h = (ch / 10.0).round().max(2.0);
                    push_quad(&mut self.above, x, y + ch - h, cw, h, c.color);
                }
                CursorShape::HollowBlock => {
                    push_quad(&mut self.above, x, y, cw, t, c.color);
                    push_quad(&mut self.above, x, y + ch - t, cw, t, c.color);
                    push_quad(&mut self.above, x, y + t, t, ch - 2.0 * t, c.color);
                    push_quad(&mut self.above, x + cw - t, y + t, t, ch - 2.0 * t, c.color);
                }
            }
        }

        self.below_count = self.quads.len() as u32;
        self.quads.append(&mut self.above);
        Ok(())
    }

    // -- drawing ----------------------------------------------------------

    /// Record the prepared frame into an existing render pass (the caller
    /// cleared the target, e.g. to [`Self::clear_color`]).
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        let quads = self.quads.len() as u32;
        pass.set_bind_group(0, &self.main_group, &[]);
        if self.below_count > 0 {
            pass.set_pipeline(&self.quad_pipeline);
            pass.set_vertex_buffer(0, self.quad_buf.buffer.slice(..));
            pass.draw(0..4, 0..self.below_count);
        }
        if !self.image_draws.is_empty() {
            pass.set_pipeline(&self.image_pipeline);
            pass.set_vertex_buffer(0, self.image_buf.buffer.slice(..));
            for (i, id) in self.image_draws.iter().enumerate() {
                if let Some(img) = self.images.get(id) {
                    pass.set_bind_group(1, &img.bind_group, &[]);
                    pass.draw(0..4, i as u32..i as u32 + 1);
                }
            }
        }
        if !self.glyphs.is_empty() {
            pass.set_pipeline(&self.glyph_pipeline);
            pass.set_vertex_buffer(0, self.glyph_buf.buffer.slice(..));
            pass.draw(0..4, 0..self.glyphs.len() as u32);
        }
        if quads > self.below_count {
            pass.set_pipeline(&self.quad_pipeline);
            pass.set_vertex_buffer(0, self.quad_buf.buffer.slice(..));
            pass.draw(0..4, self.below_count..quads);
        }
    }

    /// The prepared frame's background, as a wgpu clear color.
    pub fn clear_color(&self) -> wgpu::Color {
        let c = self.clear;
        wgpu::Color {
            r: c.r as f64 / 255.0,
            g: c.g as f64 / 255.0,
            b: c.b as f64 / 255.0,
            a: c.a as f64 / 255.0,
        }
    }

    /// Clear `view` to the frame background and draw the prepared frame.
    pub fn render(&self, encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("oc-grid"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(self.clear_color()),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        self.draw(&mut pass);
    }
}

fn push_quad(list: &mut Vec<QuadInstance>, x: f32, y: f32, w: f32, h: f32, color: Rgba) {
    if w > 0.0 && h > 0.0 && !color.is_transparent() {
        list.push(QuadInstance {
            rect: [x, y, w, h],
            color,
        });
    }
}

#[inline]
fn push_glyph(
    list: &mut Vec<GlyphInstance>,
    x: f32,
    y: f32,
    g: &crate::glyph_cache::AtlasGlyph,
    fg: Rgba,
) {
    let color = if g.color {
        Rgba::WHITE.with_alpha(fg.a)
    } else {
        fg
    };
    list.push(GlyphInstance {
        rect: [x + g.dest[0], y + g.dest[1], g.dest[2], g.dest[3]],
        uv: g.uv,
        color,
        kind: g.color as u32,
    });
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn atlas_texture(device: &wgpu::Device, size: u32, format: wgpu::TextureFormat) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("oc-grid atlas"),
        size: wgpu::Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn main_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    globals: &wgpu::Buffer,
    sampler: &wgpu::Sampler,
    mask: &wgpu::Texture,
    color: &wgpu::Texture,
) -> wgpu::BindGroup {
    let mask_view = mask.create_view(&Default::default());
    let color_view = color.create_view(&Default::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("oc-grid globals"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(&mask_view),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(&color_view),
            },
        ],
    })
}

#[allow(clippy::too_many_arguments)]
fn write_texture(
    queue: &wgpu::Queue,
    tex: &wgpu::Texture,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    bpp: u32,
    data: &[u8],
) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d { x, y, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * bpp),
            rows_per_image: Some(h),
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
}
