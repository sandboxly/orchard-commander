//! Images (port of `preview/builtin/image.py`'s view): scaled to fit the
//! pane keeping the aspect ratio, never enlarged beyond one image pixel per
//! screen pixel, centered; animated GIF/WebP/APNG play. Right-click: Open
//! (default app) or Copy (the full-size image to the clipboard).
//!
//! The pixels go to the GPU through the app's image store. A still image
//! larger than the pane is also scaled down on a worker (a smooth filter, as
//! Qt's `SmoothTransformation`); until that is done — and for animations —
//! the GPU scales the full-size pixels.

use std::any::Any;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use oc_grid::{Frame, ImagePlacement, PxRect};
use oc_preview::ImageData;
use oc_vfs::VPath;

use super::views::{ContentView, Handle};
use crate::app::{App, Outcome, Severity};
use crate::dialogs::PopupMenu;
use crate::geom::Rect;
use crate::images::{self, ImageKey};
use crate::input::{Event, MouseButton, MouseKind};
use crate::widgets::{DrawCtx, MenuEntry};

/// Decoded frames, shared with the image store.
pub struct ImagePrep {
    pub width: u32,
    pub height: u32,
    /// Size in the file (the frames may be scaled down).
    pub source: (u32, u32),
    pub frames: Vec<(Arc<Vec<u8>>, Duration)>,
}

impl ImagePrep {
    pub fn new(i: ImageData) -> Self {
        let frames = i.frames.into_iter().map(|f| (Arc::new(f.rgba), f.delay)).collect();
        ImagePrep { width: i.width, height: i.height, source: (i.source_width, i.source_height), frames }
    }
}

/// `w` × `h` fitted into `area` (pixels), never enlarged, centered, whole pixels.
pub fn fit(w: u32, h: u32, area: PxRect, enlarge: bool) -> PxRect {
    if w == 0 || h == 0 || area.is_empty() {
        return PxRect::default();
    }
    let mut scale = (area.w / w as f32).min(area.h / h as f32);
    if !enlarge {
        scale = scale.min(1.0);
    }
    let (dw, dh) = ((w as f32 * scale).round().max(1.0), (h as f32 * scale).round().max(1.0));
    PxRect::new((area.x + (area.w - dw) / 2.0).floor(), (area.y + (area.h - dh) / 2.0).floor(), dw, dh)
}

/// A cell rectangle in pixels.
pub fn area_px(area: Rect, cell_px: (f32, f32)) -> PxRect {
    let (cw, ch) = cell_px;
    PxRect::new(area.x as f32 * cw, area.y as f32 * ch, area.w as f32 * cw, area.h as f32 * ch)
}

pub struct ImageView {
    pub prep: ImagePrep,
    pub path: VPath,
    key: Option<ImageKey>,
    pub frame: usize,
    timer: Option<u64>,
    /// The smooth-scaled copy for the pane: (width, height, key).
    pub scaled: Option<(u32, u32, ImageKey)>,
    /// A scaled copy of this size is being made.
    scaling: Option<(u32, u32)>,
}

impl ImageView {
    pub fn new(prep: ImagePrep, path: VPath) -> Self {
        ImageView { prep, path, key: None, frame: 0, timer: None, scaled: None, scaling: None }
    }

    pub fn is_animated(&self) -> bool {
        self.prep.frames.len() > 1
    }

    fn dest(&self, area: Rect, cell_px: (f32, f32)) -> PxRect {
        fit(self.prep.width, self.prep.height, area_px(area, cell_px), false)
    }

    /// Ask a worker for a copy at the size shown, if that is smaller.
    fn request_scaled(&mut self, app: &mut App, h: Handle, area: Rect) {
        if self.is_animated() {
            return;
        }
        let d = self.dest(area, app.cell_px);
        let (dw, dh) = (d.w as u32, d.h as u32);
        if dw == 0 || dh == 0 || (dw >= self.prep.width && dh >= self.prep.height) {
            return;
        }
        if self.scaled.is_some_and(|(w, h, _)| (w, h) == (dw, dh)) || self.scaling == Some((dw, dh)) {
            return;
        }
        self.scaling = Some((dw, dh));
        let (w, hgt) = (self.prep.width, self.prep.height);
        let pixels = self.prep.frames[0].0.clone();
        app.tasks.run_in_background(
            move |_| {
                let src = image::ImageBuffer::<image::Rgba<u8>, &[u8]>::from_raw(w, hgt, &pixels[..])?;
                Some(image::imageops::resize(&src, dw, dh, image::imageops::FilterType::CatmullRom).into_raw())
            },
            move |app, scaled| {
                h.with::<ImageView, _>(app, |v, app, _| {
                    if v.scaling != Some((dw, dh)) {
                        return;
                    }
                    v.scaling = None;
                    let Some(rgba) = scaled else { return };
                    let store = images::store(app);
                    if let Some((_, _, old)) = v.scaled.take() {
                        store.remove(old);
                    }
                    v.scaled = Some((dw, dh, store.insert(dw, dh, Arc::new(rgba))));
                    app.redraw();
                });
            },
        );
    }

    fn schedule_frame(&mut self, app: &mut App, h: Handle) {
        let delay = self.prep.frames[self.frame].1.max(Duration::from_millis(20));
        self.timer = Some(app.schedule(delay, move |app| {
            h.with::<ImageView, _>(app, |v, app, _| v.next_frame(app, h));
        }));
    }

    fn next_frame(&mut self, app: &mut App, h: Handle) {
        self.frame = (self.frame + 1) % self.prep.frames.len();
        if let Some(key) = self.key {
            let pixels = self.prep.frames[self.frame].0.clone();
            images::store(app).set(key, self.prep.width, self.prep.height, pixels);
        }
        self.schedule_frame(app, h);
    }

    /// Right-click menu: Open.
    pub fn open(&self, app: &mut App) {
        let target = self.path.clone();
        let name = target.name().to_owned();
        if !target.is_local() {
            app.info(format!("Fetching {name}…"));
        }
        let cache = app.cache_dir();
        app.tasks.run_in_background(
            move |token| {
                let local = oc_vfs::ops::materialize_cancellable(&target, &cache, &token.as_fn()).map_err(|e| e.to_string())?;
                oc_local::open_with_default_app(&local).map_err(|e| e.to_string())
            },
            move |app, r| {
                if let Err(e) = r {
                    app.notify(format!("Cannot open {name}: {e}"), Severity::Error);
                }
            },
        );
    }

    /// Right-click menu: Copy the image at full size (animations: the first frame).
    pub fn copy(&self, app: &mut App) {
        let name = self.path.name().to_owned();
        let (w, h) = (self.prep.width, self.prep.height);
        let first = self.prep.frames[0].0.clone();
        if (w, h) == self.prep.source || self.is_animated() {
            copy_to_clipboard(app, &name, w, h, &first);
            return;
        }
        // Shown scaled down: decode the file again at its full size.
        let target = self.path.clone();
        app.tasks.run_in_background(
            move |_| full_size(&target).unwrap_or((w, h, (*first).clone())),
            move |app, (w, h, rgba)| copy_to_clipboard(app, &name, w, h, &rgba),
        );
    }
}

fn copy_to_clipboard(app: &mut App, name: &str, w: u32, h: u32, rgba: &[u8]) {
    match app.clipboard.set_image(w, h, rgba) {
        Ok(()) => app.info(format!("Copied {name} ({w}×{h})")),
        Err(e) => app.error(format!("Cannot copy {name}: {e}")),
    }
}

/// The image in `target` decoded at its full size, EXIF orientation applied.
fn full_size(target: &VPath) -> Option<(u32, u32, Vec<u8>)> {
    let data = oc_preview::PreviewSource::new(target.clone()).ok()?.read_all(None).ok()?;
    let mut decoder = image::ImageReader::new(Cursor::new(data)).with_guessed_format().ok()?.into_decoder().ok()?;
    let orientation = image::ImageDecoder::orientation(&mut decoder).ok();
    let mut img = image::DynamicImage::from_decoder(decoder).ok()?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    let rgba = img.to_rgba8();
    Some((rgba.width(), rgba.height(), rgba.into_raw()))
}

impl ContentView for ImageView {
    fn kind(&self) -> &'static str {
        "image"
    }

    fn draw(&self, _app: &App, f: &mut Frame, area: Rect, ctx: &DrawCtx, _focused: bool) {
        let dest = self.dest(area, ctx.cell_px);
        let key = match self.scaled {
            Some((w, h, key)) if (w, h) == (dest.w as u32, dest.h as u32) => Some(key),
            _ => self.key,
        };
        if let Some(key) = key {
            f.add_image(ImagePlacement::new(key.id(), dest).clipped(area_px(area, ctx.cell_px)));
        }
    }

    fn handle_event(&mut self, app: &mut App, h: Handle, ev: &Event, _area: Rect) -> Outcome {
        let Event::Mouse(m) = ev else { return Outcome::Ignored };
        if m.kind != MouseKind::Down(MouseButton::Right) {
            return Outcome::Ignored;
        }
        let entries = vec![MenuEntry::item("&Open", ""), MenuEntry::item("&Copy", "")];
        app.show(PopupMenu::new(entries, m.col, m.row, move |app, choice| {
            h.with::<ImageView, _>(app, |v, app, _| if choice == 0 { v.open(app) } else { v.copy(app) });
        }));
        Outcome::Consumed
    }

    fn shown(&mut self, app: &mut App, h: Handle, area: Rect) {
        let (w, hgt) = (self.prep.width, self.prep.height);
        self.key = Some(images::store(app).insert(w, hgt, self.prep.frames[0].0.clone()));
        if self.is_animated() {
            self.schedule_frame(app, h);
        }
        self.request_scaled(app, h, area);
    }

    fn resized(&mut self, app: &mut App, h: Handle, area: Rect) {
        self.request_scaled(app, h, area);
    }

    fn clear(&mut self, app: &mut App) {
        if let Some(t) = self.timer.take() {
            app.cancel_timer(t);
        }
        let store = images::store(app);
        if let Some(k) = self.key.take() {
            store.remove(k);
        }
        if let Some((_, _, k)) = self.scaled.take() {
            store.remove(k);
        }
        self.scaling = None;
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fitting_keeps_aspect_and_never_enlarges() {
        let area = PxRect::new(0.0, 0.0, 800.0, 600.0);
        assert_eq!(fit(1600, 1067, area, false), PxRect::new(0.0, 33.0, 800.0, 534.0));
        assert_eq!(fit(100, 50, area, false), PxRect::new(350.0, 275.0, 100.0, 50.0));
        assert_eq!(fit(100, 50, area, true), PxRect::new(0.0, 100.0, 800.0, 400.0));
        assert!(fit(0, 5, area, false).is_empty());
    }
}
