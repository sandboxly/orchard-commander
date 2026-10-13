//! Shelf packer for the glyph atlas textures. CPU-side only: it hands out
//! rectangles and queues pixel uploads; the renderer flushes them to the GPU.

/// Gap between entries so linear sampling of a scaled glyph never bleeds
/// into its neighbour.
const PAD: u32 = 1;

pub(crate) struct Upload {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

struct Shelf {
    y: u32,
    h: u32,
    x: u32,
}

/// The atlas is full; the renderer grows the texture and starts over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AtlasFull;

pub(crate) struct Atlas {
    pub size: u32,
    /// Bytes per pixel (1 for coverage masks, 4 for color glyphs).
    pub bpp: u32,
    shelves: Vec<Shelf>,
    next_y: u32,
    pub pending: Vec<Upload>,
    /// An allocation failed since the last reset: this atlas needs to grow
    /// (the other one may be fine and should keep its size).
    pub full: bool,
}

impl Atlas {
    pub fn new(size: u32, bpp: u32) -> Self {
        Atlas {
            size,
            bpp,
            shelves: Vec::new(),
            next_y: 0,
            pending: Vec::new(),
            full: false,
        }
    }

    /// Forget every entry (after a resize of the texture or a font change).
    pub fn reset(&mut self, size: u32) {
        self.size = size;
        self.shelves.clear();
        self.next_y = 0;
        self.pending.clear();
        self.full = false;
    }

    /// Reserve a `w`×`h` rectangle; returns its top-left corner.
    pub fn alloc(&mut self, w: u32, h: u32) -> Result<(u32, u32), AtlasFull> {
        let pos = self.try_alloc(w, h);
        self.full |= pos.is_err();
        pos
    }

    fn try_alloc(&mut self, w: u32, h: u32) -> Result<(u32, u32), AtlasFull> {
        let (pw, ph) = (w + PAD, h + PAD);
        if pw > self.size || ph > self.size {
            return Err(AtlasFull);
        }
        // Best-fitting shelf: the lowest one tall enough without wasting more than a third.
        let mut best: Option<usize> = None;
        for (i, s) in self.shelves.iter().enumerate() {
            if s.h >= ph
                && s.h * 2 <= ph * 3
                && s.x + pw <= self.size
                && best.is_none_or(|b| self.shelves[b].h > s.h)
            {
                best = Some(i);
            }
        }
        let shelf = match best {
            Some(i) => &mut self.shelves[i],
            None => {
                if self.next_y + ph > self.size {
                    return Err(AtlasFull);
                }
                self.shelves.push(Shelf {
                    y: self.next_y,
                    h: ph,
                    x: 0,
                });
                self.next_y += ph;
                self.shelves.last_mut().expect("just pushed")
            }
        };
        let pos = (shelf.x, shelf.y);
        shelf.x += pw;
        Ok(pos)
    }

    /// Reserve space for an image and queue its pixels for upload.
    pub fn insert(&mut self, w: u32, h: u32, data: Vec<u8>) -> Result<(u32, u32), AtlasFull> {
        debug_assert_eq!(data.len(), (w * h * self.bpp) as usize);
        let (x, y) = self.alloc(w, h)?;
        if w > 0 && h > 0 {
            self.pending.push(Upload { x, y, w, h, data });
        }
        Ok((x, y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_without_overlap() {
        let mut a = Atlas::new(64, 1);
        let mut rects = Vec::new();
        for i in 0..40 {
            let (w, h) = (3 + i % 7, 5 + i % 4);
            match a.alloc(w, h) {
                Ok((x, y)) => rects.push((x, y, w, h)),
                Err(AtlasFull) => break,
            }
        }
        assert!(rects.len() > 20);
        for (i, a) in rects.iter().enumerate() {
            assert!(a.0 + a.2 <= 64 && a.1 + a.3 <= 64);
            for b in &rects[i + 1..] {
                let overlap =
                    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3;
                assert!(!overlap, "{a:?} {b:?}");
            }
        }
    }

    #[test]
    fn reports_full_and_resets() {
        let mut a = Atlas::new(16, 4);
        assert_eq!(a.alloc(20, 2), Err(AtlasFull));
        while a.alloc(7, 7).is_ok() {}
        assert_eq!(a.alloc(7, 7), Err(AtlasFull));
        assert!(a.full);
        a.reset(32);
        assert!(!a.full);
        assert_eq!(a.alloc(7, 7), Ok((0, 0)));
        a.insert(2, 2, vec![0; 16]).unwrap();
        assert_eq!(a.pending.len(), 1);
    }
}
