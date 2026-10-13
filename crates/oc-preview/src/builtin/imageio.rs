//! macOS ImageIO for the formats the `image` crate lacks (HEIC/HEIF, ICNS,
//! JPEG 2000) — Qt decodes them through its macOS plugins, so 1.x previews
//! them. Plain C calls into the system frameworks; no extra crates.

use std::ffi::c_void;
use std::time::Duration;

use crate::data::{Frame, ImageData};

use super::image::MAX_IMAGE_DIMENSION;

type CFTypeRef = *const c_void;
type CFIndex = isize;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

const K_CF_NUMBER_SINT64: CFIndex = 4;
/// kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big: RGBA in memory.
const RGBA_PREMULTIPLIED: u32 = 1 | (4 << 12);

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFRelease(cf: CFTypeRef);
    fn CFDataCreate(alloc: CFTypeRef, bytes: *const u8, len: CFIndex) -> CFTypeRef;
    fn CFNumberCreate(alloc: CFTypeRef, kind: CFIndex, value: *const c_void) -> CFTypeRef;
    fn CFNumberGetValue(number: CFTypeRef, kind: CFIndex, value: *mut c_void) -> u8;
    fn CFDictionaryCreate(
        alloc: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: CFIndex,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFTypeRef;
    fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    static kCGImageSourceCreateThumbnailFromImageAlways: CFTypeRef;
    static kCGImageSourceCreateThumbnailWithTransform: CFTypeRef;
    static kCGImageSourceThumbnailMaxPixelSize: CFTypeRef;
    static kCGImageSourceShouldCacheImmediately: CFTypeRef;
    static kCGImagePropertyPixelWidth: CFTypeRef;
    static kCGImagePropertyPixelHeight: CFTypeRef;
    static kCGImagePropertyOrientation: CFTypeRef;
    fn CGImageSourceCreateWithData(data: CFTypeRef, options: CFTypeRef) -> CFTypeRef;
    fn CGImageSourceGetCount(src: CFTypeRef) -> usize;
    fn CGImageSourceCopyPropertiesAtIndex(src: CFTypeRef, index: usize, options: CFTypeRef) -> CFTypeRef;
    fn CGImageSourceCreateThumbnailAtIndex(src: CFTypeRef, index: usize, options: CFTypeRef) -> CFTypeRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGImageGetWidth(image: CFTypeRef) -> usize;
    fn CGImageGetHeight(image: CFTypeRef) -> usize;
    fn CGColorSpaceCreateDeviceRGB() -> CFTypeRef;
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: CFTypeRef,
        bitmap_info: u32,
    ) -> CFTypeRef;
    fn CGContextDrawImage(ctx: CFTypeRef, rect: CGRect, image: CFTypeRef);
}

/// Releases a Core Foundation object when dropped.
struct Owned(CFTypeRef);

impl Owned {
    fn new(r: CFTypeRef) -> Option<Owned> {
        (!r.is_null()).then_some(Owned(r))
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: a +1 reference from a Create/Copy call, released once.
        unsafe { CFRelease(self.0) }
    }
}

fn number(n: i64) -> Option<Owned> {
    // SAFETY: CFNumberCreate copies the i64 it is pointed at.
    Owned::new(unsafe { CFNumberCreate(std::ptr::null(), K_CF_NUMBER_SINT64, (&raw const n).cast()) })
}

fn dict(pairs: &[(CFTypeRef, CFTypeRef)]) -> Option<Owned> {
    let keys: Vec<CFTypeRef> = pairs.iter().map(|p| p.0).collect();
    let values: Vec<CFTypeRef> = pairs.iter().map(|p| p.1).collect();
    // SAFETY: valid CF objects; the dictionary retains keys and values.
    Owned::new(unsafe {
        CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            pairs.len() as CFIndex,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
    })
}

/// An integer property of image `index`, if present.
fn property(src: &Owned, index: usize, key: CFTypeRef) -> Option<i64> {
    // SAFETY: `src` is a live image source; the properties dictionary is
    // released by `Owned`, the value inside it is borrowed only meanwhile.
    unsafe {
        let props = Owned::new(CGImageSourceCopyPropertiesAtIndex(src.0, index, std::ptr::null()))?;
        let value = CFDictionaryGetValue(props.0, key);
        let mut out: i64 = 0;
        (!value.is_null() && CFNumberGetValue(value, K_CF_NUMBER_SINT64, (&raw mut out).cast()) != 0).then_some(out)
    }
}

/// Decode `data` with ImageIO: the largest image in the file (ICNS holds one
/// per icon size), EXIF orientation applied, scaled down to fit
/// [`MAX_IMAGE_DIMENSION`].
pub fn decode(data: &[u8], format: &'static str) -> Result<ImageData, String> {
    let unreadable = || "unsupported image format".to_string();
    // SAFETY: plain Core Foundation / ImageIO / Core Graphics calls on
    // objects created here; each is released by `Owned`, and the bitmap
    // context draws into `rgba`, which outlives it.
    unsafe {
        let cf_data = Owned::new(CFDataCreate(std::ptr::null(), data.as_ptr(), data.len() as CFIndex)).ok_or_else(unreadable)?;
        let src = Owned::new(CGImageSourceCreateWithData(cf_data.0, std::ptr::null())).ok_or_else(unreadable)?;
        let count = CGImageSourceGetCount(src.0);
        let size = |i: usize| {
            let w = property(&src, i, kCGImagePropertyPixelWidth).unwrap_or(0);
            let h = property(&src, i, kCGImagePropertyPixelHeight).unwrap_or(0);
            (w.max(0) as u64, h.max(0) as u64)
        };
        let index = (0..count).max_by_key(|&i| {
            let (w, h) = size(i);
            w * h
        });
        let index = index.ok_or_else(unreadable)?;
        let (mut sw, mut sh) = size(index);
        // Orientations 5–8 turn the picture by 90°.
        if matches!(property(&src, index, kCGImagePropertyOrientation), Some(5..=8)) {
            std::mem::swap(&mut sw, &mut sh);
        }
        let longest = sw.max(sh).clamp(1, MAX_IMAGE_DIMENSION as u64) as i64;
        let max_size = number(longest).ok_or_else(unreadable)?;
        let options = dict(&[
            (kCGImageSourceCreateThumbnailFromImageAlways, kCFBooleanTrue),
            (kCGImageSourceCreateThumbnailWithTransform, kCFBooleanTrue),
            (kCGImageSourceShouldCacheImmediately, kCFBooleanTrue),
            (kCGImageSourceThumbnailMaxPixelSize, max_size.0),
        ])
        .ok_or_else(unreadable)?;
        let image = Owned::new(CGImageSourceCreateThumbnailAtIndex(src.0, index, options.0)).ok_or_else(unreadable)?;
        let (w, h) = (CGImageGetWidth(image.0), CGImageGetHeight(image.0));
        if w == 0 || h == 0 || w > MAX_IMAGE_DIMENSION as usize || h > MAX_IMAGE_DIMENSION as usize {
            return Err(unreadable());
        }
        let mut rgba = vec![0u8; w * h * 4];
        let space = Owned::new(CGColorSpaceCreateDeviceRGB()).ok_or_else(unreadable)?;
        let ctx = Owned::new(CGBitmapContextCreate(rgba.as_mut_ptr().cast(), w, h, 8, w * 4, space.0, RGBA_PREMULTIPLIED))
            .ok_or_else(unreadable)?;
        let rect = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: w as f64, height: h as f64 } };
        CGContextDrawImage(ctx.0, rect, image.0);
        drop(ctx);
        unpremultiply(&mut rgba);
        let (sw, sh) = if sw == 0 || sh == 0 { (w as u32, h as u32) } else { (sw as u32, sh as u32) };
        Ok(ImageData {
            width: w as u32,
            height: h as u32,
            source_width: sw,
            source_height: sh,
            format,
            frames: vec![Frame { rgba, delay: Duration::ZERO }],
        })
    }
}

fn unpremultiply(rgba: &mut [u8]) {
    for p in rgba.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}
