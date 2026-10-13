//! PDF documents for previews, rendered with PDFium (port of the document
//! part of `preview/builtin/pdf.py`).
//!
//! PDFium is a C++ library loaded at run time (see [`library`] for where it
//! is looked for; `scripts/fetch-pdfium.sh` downloads it). It is not
//! thread-safe, so everything runs on one dedicated thread; [`Document`] is a
//! thread-safe handle that sends it requests.
//!
//! ```no_run
//! # fn main() -> Result<(), oc_pdf::PdfError> {
//! let doc = oc_pdf::Document::open_path("manual.pdf".as_ref())?;
//! let first = doc.render_width(0, oc_pdf::FIRST_PAGE_WIDTH)?; // fast first look
//! let fitted = doc.render(1, 800, 600)?; // page 2 fitted into the pane
//! # Ok(()) }
//! ```
//!
//! Documents are read through any `Read + Seek` stream, so a PDF on a server
//! isn't downloaded first: PDFium fetches only the parts it needs.

mod document;
mod engine;
mod error;
pub mod library;
#[cfg(feature = "testing")]
pub mod testing;

pub use document::{CACHE_PAGES, Document, Page, Priority, RenderCallback};
pub use error::PdfError;

/// Width of the page rendered while loading (replaced at the pane's size).
pub const FIRST_PAGE_WIDTH: u32 = 1200;

/// Cap on either side of a rendered page, in pixels.
pub const MAX_RENDER_PIXELS: u32 = 6000;

/// Whether PDFium could be loaded. The first call loads it (a few ms).
pub fn available() -> bool {
    engine::ensure_started().is_ok()
}

/// Where PDFium was loaded from, or why it couldn't be.
pub fn library_path() -> Result<std::path::PathBuf, PdfError> {
    engine::ensure_started()
}

/// Load PDFium and draw a blank page (`--self-test`: catches a library
/// missing from the bundle or refused by code signing).
pub fn self_test() -> Result<(), PdfError> {
    engine::ensure_started()?;
    engine::call(|e| e.self_test())?.map_err(PdfError::Render)
}

/// The pixel size of a `width_pt` × `height_pt` page drawn as large as fits
/// into `max_width` × `max_height`, keeping its aspect ratio, each side at
/// least 1 and at most [`MAX_RENDER_PIXELS`]. None for pages without a
/// usable size.
pub fn fit_size(width_pt: f32, height_pt: f32, max_width: u32, max_height: u32) -> Option<(u32, u32)> {
    let ok = |v: f32| v.is_finite() && v > 0.0;
    if !ok(width_pt) || !ok(height_pt) {
        return None;
    }
    let (w, h) = (f64::from(width_pt), f64::from(height_pt));
    let cap = f64::from(MAX_RENDER_PIXELS);
    let scale = (f64::from(max_width.max(1)) / w).min(f64::from(max_height.max(1)) / h).min(cap / w.max(h));
    let side = |v: f64| (v * scale).round().clamp(1.0, cap) as u32;
    Some((side(w), side(h)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_size_keeps_aspect_and_limits() {
        assert_eq!(fit_size(595.0, 842.0, 1200, u32::MAX), Some((1200, 1698)));
        assert_eq!(fit_size(595.0, 420.0, 300, u32::MAX), Some((300, 212)));
        assert_eq!(fit_size(595.0, 842.0, 1000, 500), Some((353, 500)));
        assert_eq!(fit_size(595.0, 842.0, u32::MAX, u32::MAX), Some((4240, 6000)));
        assert_eq!(fit_size(10_000.0, 1.0, 50, 50), Some((50, 1))); // never 0
        assert_eq!(fit_size(595.0, 842.0, 0, 0), Some((1, 1)));
        assert_eq!(fit_size(0.0, 842.0, 100, 100), None);
        assert_eq!(fit_size(f32::NAN, 842.0, 100, 100), None);
        assert_eq!(fit_size(595.0, f32::INFINITY, 100, 100), None);
    }
}
