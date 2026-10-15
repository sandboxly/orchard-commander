//! Pixels on screen: an app-wide image store that features fill (preview
//! images, PDF pages, video frames) and the platform copies into each
//! window's GPU renderer.
//!
//! ```text
//! feature:  let key = images::store(app).insert(w, h, Arc::new(rgba));
//! draw:     f.add_image(ImagePlacement::new(key.id(), PxRect::new(..)));
//! platform: win.images.sync(&app, &frame, renderer)   // before rendering a frame
//! later:    images::store(app).set(key, w, h, next_frame);   images::store(app).remove(key);
//! ```
//!
//! The UI never touches the GPU: drawing only names images
//! ([`ImageKey::id`] is an [`orchard_grid::ImageId::external`]). [`ImageSync`]
//! uploads an image when a frame shows it and its pixels changed since the
//! last upload (each change bumps a generation), and frees textures whose
//! images were removed from the store. Pixels are shared (`Arc`), never
//! copied on the UI thread.

use std::collections::HashMap;
use std::sync::Arc;

use orchard_grid::{Frame, GridRenderer, ImageId};

use crate::app::App;

/// Straight (not premultiplied) RGBA8, `width * height * 4` bytes, rows top
/// to bottom. Implemented for `Vec<u8>`, rendered PDF pages and video frames, so their
/// buffers go into the store as they are.
pub trait Pixels: Send + Sync {
    fn rgba(&self) -> &[u8];
}

impl Pixels for Vec<u8> {
    fn rgba(&self) -> &[u8] {
        self
    }
}

impl Pixels for oc_preview::RenderedPage {
    fn rgba(&self) -> &[u8] {
        &self.rgba
    }
}

impl Pixels for oc_media::VideoFrame {
    fn rgba(&self) -> &[u8] {
        &self.rgba
    }
}

/// An image in the store.
#[derive(Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub pixels: Arc<dyn Pixels>,
    generation: u64,
}

/// Names an image in the [`Images`] store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImageKey(u64);

impl ImageKey {
    /// What a frame's [`orchard_grid::ImagePlacement`] refers to.
    pub fn id(self) -> ImageId {
        ImageId::external(self.0)
    }
}

/// The app's images (an [`App::state`]; reach it with [`store`]).
#[derive(Default)]
pub struct Images {
    map: HashMap<u64, Image>,
    next_key: u64,
    generation: u64,
}

impl Images {
    /// Add an image; remove it with [`remove`](Self::remove) when no longer shown.
    pub fn insert(&mut self, width: u32, height: u32, pixels: Arc<dyn Pixels>) -> ImageKey {
        self.next_key += 1;
        let key = ImageKey(self.next_key);
        self.set(key, width, height, pixels);
        key
    }

    /// Replace an image's pixels (the next animation or video frame); the
    /// size may change.
    pub fn set(&mut self, key: ImageKey, width: u32, height: u32, pixels: Arc<dyn Pixels>) {
        self.generation += 1;
        self.map.insert(key.0, Image { width, height, pixels, generation: self.generation });
    }

    pub fn remove(&mut self, key: ImageKey) {
        self.map.remove(&key.0);
    }

    pub fn get(&self, key: ImageKey) -> Option<&Image> {
        self.map.get(&key.0)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// The app's image store.
pub fn store(app: &mut App) -> &mut Images {
    app.state::<Images>()
}

/// Keeps one renderer's textures in step with the store (the platform has
/// one per window).
#[derive(Default)]
pub struct ImageSync {
    /// Store key → generation uploaded.
    uploaded: HashMap<u64, u64>,
}

impl ImageSync {
    /// Before rendering `frame`: upload the images it shows that are new or
    /// changed, free the ones removed from the store.
    pub fn sync(&mut self, app: &App, frame: &Frame, renderer: &mut GridRenderer) {
        let store = app.try_state::<Images>();
        self.uploaded.retain(|key, _| {
            let keep = store.is_some_and(|s| s.map.contains_key(key));
            if !keep {
                renderer.remove_image(ImageId::external(*key));
            }
            keep
        });
        let Some(store) = store else { return };
        for p in &frame.images {
            let Some(key) = p.image.external_key() else { continue };
            let Some(img) = store.map.get(&key) else { continue };
            if self.uploaded.get(&key) == Some(&img.generation) {
                continue;
            }
            if let Err(e) = renderer.set_image(p.image, img.width, img.height, img.pixels.rgba()) {
                log::warn!("cannot upload an image ({}x{}): {e}", img.width, img.height);
            }
            // Also after a failure: don't retry the same pixels every frame.
            self.uploaded.insert(key, img.generation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_generations() {
        let mut s = Images::default();
        let a = s.insert(1, 1, Arc::new(vec![0u8; 4]));
        let b = s.insert(2, 1, Arc::new(vec![0u8; 8]));
        assert_ne!(a, b);
        assert_eq!(a.id().external_key(), Some(a.0));
        let g = s.get(a).unwrap().generation;
        s.set(a, 1, 1, Arc::new(vec![1u8; 4]));
        assert!(s.get(a).unwrap().generation > g);
        s.remove(a);
        assert!(s.get(a).is_none() && s.len() == 1);
    }
}
