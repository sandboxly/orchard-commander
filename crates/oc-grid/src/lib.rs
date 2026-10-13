//! GPU character-grid renderer that the whole Orchard Commander UI draws into.
//!
//! The app looks like a terminal program, so instead of a widget toolkit the
//! UI fills a [`Frame`] — a grid of character cells with colors and
//! attributes, plus colored rectangles, images and a cursor — and a
//! [`GridRenderer`] draws it with wgpu in a few instanced draw calls.
//!
//! * Text comes from system fonts (swash rasterizer, fontdb discovery) with
//!   fallback for missing characters (Nerd Fonts, symbol, emoji and CJK
//!   fonts), cached in glyph atlases.
//! * Box-drawing, block and powerline characters are drawn geometrically
//!   (port of `term_glyphs.py`) so borders join seamlessly at any line spacing.
//! * Cells are whole physical pixels at any HiDPI scale factor, so lines stay crisp.
//! * [`HeadlessRenderer`] renders to an image (tests, screenshots);
//!   [`WindowRenderer`] presents to a winit window.
//!
//! Replaces the Qt painting in `boxframe.py`, `panel.py`, `terminal_view.py`.

mod atlas;
mod color;
mod fonts;
mod frame;
mod glyph_cache;
mod headless;
mod renderer;
pub mod special;
mod window;

pub use color::Rgba;
pub use fonts::{CellMetrics, FontConfig, FontLibrary, MONO_FAMILIES, px_per_pt};
pub use frame::{
    Area, BoxStyle, Cell, CellRect, Cursor, CursorShape, FillRect, Flags, Frame, ImageId,
    ImagePlacement, Layer, PxRect, Style, elide_left, elide_right, str_width,
};
pub use headless::{HeadlessRenderer, RgbaImage, render_to_image};
pub use renderer::{GridRenderer, PrepareStats};
pub use window::{Rendered, WindowRenderer};

/// Re-exported so callers can match the exact wgpu/winit versions.
pub use {wgpu, winit};

#[derive(Debug, thiserror::Error)]
pub enum GridError {
    #[error("no usable font installed")]
    NoFont,
    #[error("no GPU adapter: {0}")]
    NoAdapter(String),
    #[error("GPU device error: {0}")]
    Device(String),
    #[error("window surface error: {0}")]
    Surface(String),
    #[error("unknown image")]
    UnknownImage,
    #[error("invalid image size {width}x{height}")]
    ImageSize { width: u32, height: u32 },
    #[error("image data is {got} bytes, expected {expected}")]
    ImageData { expected: usize, got: usize },
    #[error("PNG encoding failed: {0}")]
    Png(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
