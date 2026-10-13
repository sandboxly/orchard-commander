//! The previewers that ship with Orchard Commander.

mod hex;
mod image;
mod listing;
mod media;
#[cfg(feature = "pdf")]
mod pdf;
mod text;

use std::sync::Arc;

use crate::api::Previewer;

pub use hex::HexPreviewer;
pub use image::{ImagePreviewer, MAX_IMAGE_DIMENSION, decode_image, decode_image_as, decode_svg, supported_extensions};
pub use listing::ListingPreviewer;
pub use media::MediaPreviewer;
#[cfg(feature = "pdf")]
pub use pdf::PdfPreviewer;
pub use text::{MarkdownPreviewer, TextPreviewer};

/// Images (and other files without a direct path) are fetched whole only up to this size.
pub const MAX_FETCH_BYTES: u64 = 512 * 1024 * 1024;

/// The built-ins in tie-break order (registration order breaks equal scores).
pub fn builtins() -> Vec<Arc<dyn Previewer>> {
    vec![
        Arc::new(ListingPreviewer),
        Arc::new(ImagePreviewer::default()),
        #[cfg(feature = "pdf")]
        Arc::new(PdfPreviewer),
        Arc::new(MediaPreviewer),
        Arc::new(MarkdownPreviewer),
        Arc::new(TextPreviewer),
        Arc::new(HexPreviewer),
    ]
}
