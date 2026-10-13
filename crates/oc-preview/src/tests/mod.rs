//! Crate tests: content helpers, the previewer contract (every registered
//! previewer against a corpus of empty, garbage, valid and huge files on a
//! local and a non-local file system), registry selection rules, cloud
//! placeholders, and the built-in loaders.

mod contract;
mod fs;
mod loaders;
#[cfg(feature = "pdf")]
mod pdf;
mod registry;
mod review;

use std::io::Cursor;

use image::codecs::gif::GifEncoder;
use image::{Delay, ImageFormat, Rgba, RgbaImage};

pub fn png_bytes(w: u32, h: u32) -> Vec<u8> {
    let img = RgbaImage::from_pixel(w, h, Rgba([0, 128, 0, 255]));
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

pub fn webp_bytes(w: u32, h: u32) -> Vec<u8> {
    let img = RgbaImage::from_pixel(w, h, Rgba([200, 10, 10, 255]));
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::WebP).unwrap();
    out.into_inner()
}

pub fn gif_bytes(frames: usize) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = GifEncoder::new(&mut out);
        enc.set_repeat(image::codecs::gif::Repeat::Infinite).unwrap();
        for i in 0..frames {
            let img = RgbaImage::from_pixel(6, 3, Rgba([(i * 80) as u8, 0, 255, 255]));
            enc.encode_frame(image::Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(70, 1))).unwrap();
        }
    }
    out
}

pub const SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="20" height="10" fill="#0000ff"/></svg>"##;

/// A valid one-page PDF.
pub fn pdf_bytes() -> Vec<u8> {
    oc_pdf::testing::pdf_bytes(1)
}

/// Whether the PDF previewer is registered: built with the `pdf` feature and
/// the PDFium library found (scripts/fetch-pdfium.sh). Tests that need it
/// skip with a note otherwise.
pub fn pdf_available() -> bool {
    let ok = cfg!(feature = "pdf") && oc_pdf::available();
    if !ok {
        eprintln!("PDF previewer unavailable (no `pdf` feature or no PDFium library): skipping PDF checks");
    }
    ok
}

/// Deterministic pseudo-random bytes (no rand dependency).
pub fn garbage(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}
