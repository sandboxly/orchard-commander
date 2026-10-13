//! Raster images (PNG, JPEG, GIF, WebP, BMP, TIFF, ICO, …) via the `image`
//! crate, animated GIF/WebP/APNG as frame lists, and SVG via resvg.

use std::io::Cursor;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, RgbaImage};
use oc_vfs::human_size;
use resvg::{tiny_skia, usvg};

use crate::api::{Access, FileInfo, LoadContext, Loaded, PreviewError, Previewer, Score};
use crate::data::{Frame, ImageData, PreviewData};
use crate::source::PreviewSource;

use super::MAX_FETCH_BYTES;

/// Huge images are scaled down to fit this on decode.
pub const MAX_IMAGE_DIMENSION: u32 = 4096;
/// Animations keep frames up to this many decoded bytes (the rest is dropped).
const MAX_ANIMATION_BYTES: usize = 256 * 1024 * 1024;
/// Decoder working-memory limit; bigger images fail and fall back to hex.
const MAX_DECODE_ALLOC: u64 = 1024 * 1024 * 1024;
/// Local files above this aren't read into memory for a preview.
const MAX_LOCAL_BYTES: u64 = 1024 * 1024 * 1024;

/// Extensions this build decodes.
pub fn supported_extensions() -> &'static [&'static str] {
    &[
        "png", "jpg", "jpeg", "jpe", "jfif", "gif", "bmp", "webp", "tif", "tiff", "ico", "svg", "svgz", "tga", "pbm",
        "pgm", "ppm", "pnm", "qoi", "apng",
    ]
}

pub struct ImagePreviewer {
    /// Download limit for images without a direct path.
    pub max_size: u64,
}

impl Default for ImagePreviewer {
    fn default() -> Self {
        ImagePreviewer { max_size: MAX_FETCH_BYTES }
    }
}

impl Previewer for ImagePreviewer {
    fn id(&self) -> &str {
        "image"
    }

    fn title(&self) -> &str {
        "Image"
    }

    fn access(&self) -> Access {
        Access::LocalFile
    }

    fn max_size(&self) -> Option<u64> {
        Some(self.max_size)
    }

    fn reload_on_theme_change(&self) -> bool {
        false
    }

    fn score(&self, f: &FileInfo) -> i32 {
        if !f.is_dir && supported_extensions().contains(&f.ext.as_str()) { Score::FORMAT } else { Score::NO }
    }

    fn load(&self, src: &PreviewSource, _ctx: &LoadContext) -> Result<Loaded, PreviewError> {
        if src.direct_path().is_some() && src.size() > MAX_LOCAL_BYTES {
            return Err(PreviewError::cannot("Image too large to preview"));
        }
        let data = src.read_all(Some(self.max_size))?;
        let ext = crate::api::ext_for_name(src.name());
        // The name picks the decoder; a misnamed file still decodes by its
        // contents (Qt sniffed them too).
        let image = if matches!(ext.as_str(), "svg" | "svgz") {
            decode_svg(&data).or_else(|e| decode_image(&data).map_err(|_| e))
        } else {
            decode_image_as(&data, ImageFormat::from_extension(&ext))
                .or_else(|e| if looks_like_svg(&data) { decode_svg(&data) } else { Err(e) })
        }
        .map_err(|e| PreviewError::CannotPreview(format!("Not a readable image: {e}")))?;
        let mut info = format!(
            "{}×{} · {} · {}",
            image.source_width,
            image.source_height,
            image.format,
            human_size(src.size())
        );
        if image.is_animated() {
            info.push_str(" · animated");
        }
        Ok(Loaded::new(PreviewData::Image(image), info))
    }
}

fn format_name(f: ImageFormat) -> &'static str {
    match f {
        ImageFormat::Png => "PNG",
        ImageFormat::Jpeg => "JPEG",
        ImageFormat::Gif => "GIF",
        ImageFormat::WebP => "WEBP",
        ImageFormat::Bmp => "BMP",
        ImageFormat::Tiff => "TIFF",
        ImageFormat::Ico => "ICO",
        ImageFormat::Tga => "TGA",
        ImageFormat::Pnm => "PNM",
        ImageFormat::Qoi => "QOI",
        _ => "IMAGE",
    }
}

/// An SVG (or SVGZ) by its first bytes.
fn looks_like_svg(data: &[u8]) -> bool {
    let head = &data[..data.len().min(4096)];
    head.starts_with(&[0x1f, 0x8b]) || head.windows(4).any(|w| w == b"<svg")
}

/// Refuse images whose decoded pixels alone would exceed the memory limit,
/// before anything is allocated: a tiny file can claim 65535×65535 pixels,
/// and not every decoder checks `Limits::max_alloc` itself.
fn check_size(w: u32, h: u32, bytes_per_pixel: u64) -> Result<(), String> {
    if w as u64 * h as u64 * bytes_per_pixel > MAX_DECODE_ALLOC {
        return Err(format!("{w}×{h} is too large to preview"));
    }
    Ok(())
}

fn limits() -> Limits {
    let mut l = Limits::default();
    l.max_alloc = Some(MAX_DECODE_ALLOC);
    l
}

/// Scaled to fit [`MAX_IMAGE_DIMENSION`], keeping the aspect ratio.
fn fit(w: u32, h: u32) -> (u32, u32) {
    let longest = w.max(h);
    if longest <= MAX_IMAGE_DIMENSION {
        return (w, h);
    }
    let scale = MAX_IMAGE_DIMENSION as f64 / longest as f64;
    (((w as f64 * scale).round() as u32).max(1), ((h as f64 * scale).round() as u32).max(1))
}

fn shrink(img: RgbaImage) -> RgbaImage {
    let (w, h) = fit(img.width(), img.height());
    if (w, h) == img.dimensions() { img } else { image::imageops::thumbnail(&img, w, h) }
}

/// GIF/WebP/APNG frames, composited to full canvases; None if not animated.
fn animation<'a>(frames: image::Frames<'a>, format: &'static str, dims: (u32, u32)) -> Result<Option<ImageData>, String> {
    let mut out: Vec<Frame> = Vec::new();
    let mut bytes = 0usize;
    let mut size = (0, 0);
    for frame in frames {
        let frame = match frame {
            Ok(f) => f,
            Err(e) if out.is_empty() => return Err(e.to_string()),
            Err(_) => break, // a broken tail: keep what decoded
        };
        let (num, den) = frame.delay().numer_denom_ms();
        let ms = num.checked_div(den).unwrap_or(0);
        // Like browsers: tiny delays mean "as fast as allowed", shown at 10 fps.
        let delay = Duration::from_millis(if ms <= 10 { 100 } else { ms as u64 });
        let buffer = shrink(frame.into_buffer());
        size = buffer.dimensions();
        bytes += buffer.as_raw().len();
        out.push(Frame { rgba: buffer.into_raw(), delay });
        if bytes > MAX_ANIMATION_BYTES {
            break;
        }
    }
    if out.is_empty() {
        return Err("no frames".into());
    }
    if out.len() == 1 {
        out[0].delay = Duration::ZERO; // a single-frame GIF is a still image
    }
    Ok(Some(ImageData { width: size.0, height: size.1, source_width: dims.0, source_height: dims.1, format, frames: out }))
}

/// Decode a raster image from memory: EXIF orientation applied, huge images
/// scaled down, animations as frame lists.
pub fn decode_image(data: &[u8]) -> Result<ImageData, String> {
    decode_image_as(data, None)
}

/// Like [`decode_image`], with the format to assume when the contents don't
/// tell (TGA has no signature; its extension is all there is).
pub fn decode_image_as(data: &[u8], fallback: Option<ImageFormat>) -> Result<ImageData, String> {
    let mut reader = ImageReader::new(Cursor::new(data)).with_guessed_format().map_err(|e| e.to_string())?;
    if reader.format().is_none()
        && let Some(f) = fallback
    {
        reader.set_format(f);
    }
    let format = reader.format().ok_or("unsupported image format")?;
    let name = format_name(format);
    let err = |e: image::ImageError| e.to_string();
    match format {
        ImageFormat::Gif => {
            let mut dec = GifDecoder::new(Cursor::new(data)).map_err(err)?;
            dec.set_limits(limits()).map_err(err)?;
            let dims = dec.dimensions();
            check_size(dims.0, dims.1, 4)?;
            if let Some(img) = animation(dec.into_frames(), name, dims)? {
                return Ok(img);
            }
        }
        ImageFormat::WebP => {
            let dec = WebPDecoder::new(Cursor::new(data)).map_err(err)?;
            if dec.has_animation() {
                let dims = dec.dimensions();
                check_size(dims.0, dims.1, 4)?;
                if let Some(img) = animation(dec.into_frames(), name, dims)? {
                    return Ok(img);
                }
            }
        }
        ImageFormat::Png => {
            let dec = PngDecoder::with_limits(Cursor::new(data), limits()).map_err(err)?;
            if dec.is_apng().unwrap_or(false) {
                let dims = dec.dimensions();
                check_size(dims.0, dims.1, 4)?;
                if let Some(img) = animation(dec.apng().map_err(err)?.into_frames(), name, dims)? {
                    return Ok(img);
                }
            }
        }
        _ => {}
    }
    reader.limits(limits());
    let mut dec = reader.into_decoder().map_err(err)?;
    let (w, h) = dec.dimensions();
    check_size(w, h, dec.total_bytes() / (w as u64 * h as u64).max(1))?;
    let orientation = dec.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(dec).map_err(err)?;
    img.apply_orientation(orientation);
    let (sw, sh) = (img.width(), img.height());
    let (w, h) = fit(sw, sh);
    let img = if (w, h) == (sw, sh) { img } else { img.thumbnail_exact(w, h) };
    let rgba = img.into_rgba8();
    Ok(ImageData {
        width: rgba.width(),
        height: rgba.height(),
        source_width: sw,
        source_height: sh,
        format: name,
        frames: vec![Frame { rgba: rgba.into_raw(), delay: Duration::ZERO }],
    })
}

/// System fonts for SVG text, loaded once and only when an SVG has text.
fn fonts() -> Arc<usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        })
        .clone()
}

/// Render an SVG (or gzipped SVGZ) at its own size, scaled down to fit
/// [`MAX_IMAGE_DIMENSION`]. External files it references are not loaded.
pub fn decode_svg(data: &[u8]) -> Result<ImageData, String> {
    let mut opt = usvg::Options::default();
    // Embedded (data:) images only. usvg would otherwise read any path an
    // SVG names, relative to the current directory — e.g. "/dev/zero",
    // which never ends — even for SVGs from servers or archives.
    opt.image_href_resolver.resolve_string = Box::new(|_, _| None);
    let gzipped = data.starts_with(&[0x1f, 0x8b]);
    if gzipped || data.windows(5).any(|w| w == b"<text") {
        opt.fontdb = fonts();
    }
    let tree = usvg::Tree::from_data(data, &opt).map_err(|e| e.to_string())?;
    let size = tree.size();
    let (sw, sh) = (size.width().ceil(), size.height().ceil());
    if !(sw >= 1.0 && sh >= 1.0) || sw > 1e6 || sh > 1e6 {
        return Err("invalid SVG size".into());
    }
    let (sw, sh) = (sw as u32, sh as u32);
    let (w, h) = fit(sw, sh);
    let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or("invalid SVG size")?;
    let transform = tiny_skia::Transform::from_scale(w as f32 / size.width(), h as f32 / size.height());
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for p in pixmap.pixels() {
        let c = p.demultiply();
        rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    Ok(ImageData {
        width: w,
        height: h,
        source_width: sw,
        source_height: sh,
        format: "SVG",
        frames: vec![Frame { rgba, delay: Duration::ZERO }],
    })
}
