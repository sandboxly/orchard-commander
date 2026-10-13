//! Character → rasterized glyph in the atlas, cached.
//!
//! Lookups are the hot path of every frame (one per non-blank cell), so
//! printable ASCII goes through a flat table and everything else through an
//! Fx hash map; rasterization happens once per (character, style).

use rustc_hash::FxHashMap;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::shape::ShapeContext;
use swash::zeno::{Angle, Format, Transform};
use unicode_width::UnicodeWidthChar;

use crate::atlas::{Atlas, AtlasFull};
use crate::fonts::{BOLD, FaceIdx, FontSet, ITALIC, STRONG, StyleBits};
use crate::special;

/// Hinting sharpens stems at small sizes on Windows/Linux; macOS renders
/// unhinted, and so do we there, to look like the native apps.
const HINT: bool = !cfg!(target_os = "macos");

/// Italic synthesized by shearing, like CoreText/Qt do for fonts without an italic.
const SYNTH_ITALIC_DEGREES: f32 = 14.0;

/// A glyph image in one of the atlases, and where to draw it relative to its
/// cell's top-left corner (physical pixels).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AtlasGlyph {
    /// x, y, w, h in atlas pixels.
    pub uv: [u16; 4],
    /// x, y, w, h relative to the cell origin; w/h differ from the uv size
    /// when the glyph was scaled to fit (color emoji).
    pub dest: [f32; 4],
    /// From the color atlas (emoji) instead of the coverage atlas.
    pub color: bool,
}

#[derive(Clone, Copy)]
enum Slot {
    Unknown,
    Empty,
    Glyph(AtlasGlyph),
}

pub(crate) struct GlyphCache {
    pub fonts: FontSet,
    scale: ScaleContext,
    shape: ShapeContext,
    pub mask: Atlas,
    pub color: Atlas,
    /// Printable ASCII × 4 styles.
    ascii: Box<[Slot; 512]>,
    chars: FxHashMap<(char, StyleBits), Option<AtlasGlyph>>,
    /// Per style, so lookups borrow the `&str` instead of allocating a key.
    clusters: [FxHashMap<Box<str>, Box<[AtlasGlyph]>>; 4],
}

impl GlyphCache {
    pub fn new(fonts: FontSet, mask_size: u32, color_size: u32) -> Self {
        GlyphCache {
            fonts,
            scale: ScaleContext::new(),
            shape: ShapeContext::new(),
            mask: Atlas::new(mask_size, 1),
            color: Atlas::new(color_size, 4),
            ascii: Box::new([Slot::Unknown; 512]),
            chars: FxHashMap::default(),
            clusters: Default::default(),
        }
    }

    /// Forget all glyphs (font/size change, or an atlas was grown).
    pub fn clear(&mut self, mask_size: u32, color_size: u32) {
        self.mask.reset(mask_size);
        self.color.reset(color_size);
        self.ascii.fill(Slot::Unknown);
        self.chars.clear();
        self.clusters.iter_mut().for_each(FxHashMap::clear);
    }

    pub fn replace_fonts(&mut self, fonts: FontSet) {
        self.fonts = fonts;
        let (m, c) = (self.mask.size, self.color.size);
        self.clear(m, c);
    }

    /// The glyph for a single character, `None` for blank ones (space, or
    /// nothing in any font).
    #[inline]
    pub fn glyph(&mut self, ch: char, style: StyleBits) -> Result<Option<AtlasGlyph>, AtlasFull> {
        if ch.is_ascii() {
            let style = style & 3;
            let i = (style as usize) << 7 | ch as usize;
            return match self.ascii[i] {
                Slot::Glyph(g) => Ok(Some(g)),
                Slot::Empty => Ok(None),
                Slot::Unknown => {
                    let g = self.rasterize_char(ch, style)?;
                    self.ascii[i] = g.map_or(Slot::Empty, Slot::Glyph);
                    Ok(g)
                }
            };
        }
        // STRONG only matters for box-drawing lines.
        let style = if style & STRONG != 0 && special::is_line(ch) { (style & 3) | STRONG } else { style & 3 };
        if let Some(&g) = self.chars.get(&(ch, style)) {
            return Ok(g);
        }
        let g = self.rasterize_char(ch, style)?;
        self.chars.insert((ch, style), g);
        Ok(g)
    }

    /// Glyphs of a multi-code-point grapheme (combining marks, emoji
    /// sequences), shaped together so marks sit on their base.
    pub fn cluster(&mut self, text: &str, style: StyleBits) -> Result<&[AtlasGlyph], AtlasFull> {
        let style = style & 3;
        // Two-step lookup: the borrow checker can't see the early return releases the borrow.
        if !self.clusters[style as usize].contains_key(text) {
            let glyphs = self.shape_cluster(text, style)?;
            self.clusters[style as usize].insert(text.into(), glyphs);
        }
        Ok(self.clusters[style as usize]
            .get(text)
            .map(|g| &**g)
            .unwrap_or(&[]))
    }

    fn rasterize_char(
        &mut self,
        ch: char,
        style: StyleBits,
    ) -> Result<Option<AtlasGlyph>, AtlasFull> {
        let (cw, chh) = (self.fonts.metrics.width, self.fonts.metrics.height);
        let strong = style & STRONG;
        if special::handles(ch) && style != strong {
            // Drawn geometrically, the same in bold and italic: share one atlas entry.
            return self.glyph(ch, strong);
        }
        if special::handles(ch) {
            let mask = special::rasterize_strong(ch, cw, chh, strong != 0).expect("handled");
            return self.insert_cell_mask(mask).map(Some);
        }
        if ch == ' ' || ch.is_control() {
            return Ok(None);
        }
        let Some((face, gid)) = self.fonts.resolve(ch, style) else {
            // No font has it: a few characters the UI relies on are drawn.
            return match special::rasterize_fallback(ch, cw, chh) {
                Some(mask) => self.insert_cell_mask(mask).map(Some),
                None => Ok(None),
            };
        };
        let span = ch.width().unwrap_or(1).clamp(1, 2) as u32;
        self.rasterize_glyph(face, gid, style, span, 0.0, 0.0)
    }

    /// A coverage mask exactly one cell in size, into the atlas.
    fn insert_cell_mask(&mut self, mask: Vec<u8>) -> Result<AtlasGlyph, AtlasFull> {
        let (cw, chh) = (self.fonts.metrics.width, self.fonts.metrics.height);
        let (x, y) = self.mask.insert(cw, chh, mask)?;
        Ok(AtlasGlyph {
            uv: [x as u16, y as u16, cw as u16, chh as u16],
            dest: [0.0, 0.0, cw as f32, chh as f32],
            color: false,
        })
    }

    /// Rasterize one glyph; `dx`/`dy` offset it from the pen position (cluster shaping).
    fn rasterize_glyph(
        &mut self,
        face: FaceIdx,
        gid: u16,
        style: StyleBits,
        span: u32,
        dx: f32,
        dy: f32,
    ) -> Result<Option<AtlasGlyph>, AtlasFull> {
        let m = self.fonts.metrics;
        let avail_w = (span * m.width) as f32;
        let (synth_bold, synth_italic) = self.fonts.synth(face, style);
        let mut px = m.font_px;
        let mut image = None;
        // At most one retry: a glyph much wider than its cells (proportional
        // Nerd Font icons, fallback symbols) is re-rendered at a size that fits.
        for _ in 0..2 {
            let font = self.fonts.faces[face as usize].font();
            let mut scaler = self.scale.builder(font).size(px).hint(HINT).build();
            let sources = [
                Source::ColorOutline(0),
                Source::ColorBitmap(StrikeWith::BestFit),
                Source::Outline,
            ];
            let mut render = Render::new(&sources);
            render.format(Format::Alpha);
            if synth_bold {
                render.embolden((px / 28.0).max(0.5));
            }
            if synth_italic {
                render.transform(Some(Transform::skew(
                    Angle::from_degrees(SYNTH_ITALIC_DEGREES),
                    Angle::ZERO,
                )));
            }
            let Some(img) = render.render(&mut scaler, gid) else {
                return Ok(None);
            };
            let is_color = matches!(img.content, swash::scale::image::Content::Color);
            let too_wide = img.placement.width as f32 > avail_w * 1.15 + 1.0;
            if !is_color && too_wide && px == m.font_px && span == 1 && self.is_icon_like(face) {
                px *= avail_w / img.placement.width as f32;
                continue;
            }
            image = Some(img);
            break;
        }
        let Some(img) = image else { return Ok(None) };
        let p = img.placement;
        if p.width == 0 || p.height == 0 {
            return Ok(None);
        }
        let is_color = matches!(img.content, swash::scale::image::Content::Color);
        let mut dest = [
            p.left as f32 + dx,
            m.baseline as f32 - p.top as f32 - dy,
            p.width as f32,
            p.height as f32,
        ];
        if is_color {
            // Color glyphs (emoji bitmaps) come at the strike's proportions:
            // scale into the cells, centred, keeping the aspect ratio.
            let avail_h = m.height as f32;
            let scale = (avail_w / p.width as f32)
                .min(avail_h / p.height as f32)
                .min(1.0);
            if scale < 1.0 || dest[0] < 0.0 || dest[0] + dest[2] > avail_w {
                let (w, h) = (p.width as f32 * scale, p.height as f32 * scale);
                dest = [
                    ((avail_w - w) / 2.0).round(),
                    ((avail_h - h) / 2.0).round(),
                    w,
                    h,
                ];
            }
        }
        let atlas = if is_color {
            &mut self.color
        } else {
            &mut self.mask
        };
        let data = match img.content {
            swash::scale::image::Content::SubpixelMask => {
                img.data.chunks_exact(4).map(|p| p[3]).collect()
            }
            _ => img.data,
        };
        let (x, y) = atlas.insert(p.width, p.height, data)?;
        Ok(Some(AtlasGlyph {
            uv: [x as u16, y as u16, p.width as u16, p.height as u16],
            dest,
            color: is_color,
        }))
    }

    /// Faces other than the configured one: candidates for shrinking an
    /// oversized glyph. The main font's own wide glyphs (ligature pieces)
    /// are left alone.
    fn is_icon_like(&self, face: FaceIdx) -> bool {
        face != self.fonts.primary(0)
            && face != self.fonts.primary(BOLD)
            && face != self.fonts.primary(ITALIC)
    }

    fn shape_cluster(
        &mut self,
        text: &str,
        style: StyleBits,
    ) -> Result<Box<[AtlasGlyph]>, AtlasFull> {
        let mut chars = text.chars();
        let Some(first) = chars.next() else {
            return Ok(Box::new([]));
        };
        // The face is chosen by the base character; marks it lacks fall back individually.
        let Some((face, _)) = self.fonts.resolve(first, style) else {
            return Ok(Box::new([]));
        };
        let all_covered = text.chars().all(|c| {
            c == '\u{200d}'
                || ('\u{fe00}'..='\u{fe0f}').contains(&c)
                || self.fonts.faces[face as usize].glyph(c) != 0
        });
        let span = unicode_width::UnicodeWidthStr::width(text).clamp(1, 2) as u32;
        if !all_covered {
            let mut out = Vec::new();
            for c in text.chars() {
                if let Some(g) = self.glyph(c, style)? {
                    out.push(g);
                }
            }
            return Ok(out.into_boxed_slice());
        }
        let px = self.fonts.metrics.font_px;
        let mut placed: Vec<(u16, f32, f32)> = Vec::new();
        {
            let font = self.fonts.faces[face as usize].font();
            let mut shaper = self.shape.builder(font).size(px).build();
            shaper.add_str(text);
            let mut pen = 0.0;
            shaper.shape_with(|cluster| {
                for g in cluster.glyphs {
                    placed.push((g.id, pen + g.x, g.y));
                    pen += g.advance;
                }
            });
        }
        let mut out = Vec::with_capacity(placed.len());
        for (gid, x, y) in placed {
            if let Some(g) = self.rasterize_glyph(face, gid, style, span, x.round(), y.round())? {
                out.push(g);
            }
        }
        Ok(out.into_boxed_slice())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::fonts::{FontConfig, FontLibrary};

    /// A cache over a single-face font (no bold, no italic), so both styles
    /// are synthesized. `None` when the font isn't installed.
    fn single_face_cache() -> Option<GlyphCache> {
        let path = if cfg!(target_os = "macos") {
            "/System/Library/Fonts/Supplemental/Andale Mono.ttf"
        } else if cfg!(target_os = "windows") {
            "C:\\Windows\\Fonts\\lucon.ttf"
        } else {
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
        };
        let mut db = fontdb::Database::new();
        db.load_font_file(path).ok()?;
        let lib = Arc::new(FontLibrary::from_database(db));
        let fonts = crate::fonts::FontSet::new(lib, &FontConfig::new("", 24.0), 1.0)?;
        Some(GlyphCache::new(fonts, 1024, 256))
    }

    /// The last uploaded coverage mask and its size.
    fn last_mask(cache: &GlyphCache) -> (u32, u32, Vec<u8>) {
        let up = cache.mask.pending.last().expect("a glyph was rasterized");
        (up.w, up.h, up.data.clone())
    }

    #[test]
    fn missing_styles_are_synthesized() {
        let Some(mut cache) = single_face_cache() else {
            return;
        };
        let face = cache.fonts.primary(0);
        assert_eq!(cache.fonts.synth(face, BOLD | ITALIC), (true, true));
        assert_eq!(
            cache.fonts.primary(BOLD),
            face,
            "no bold face: regular + embolden"
        );

        cache.glyph('I', 0).unwrap().unwrap();
        let (w, h, regular) = last_mask(&cache);
        cache.glyph('I', BOLD).unwrap().unwrap();
        let (_, _, bold) = last_mask(&cache);
        let ink = |m: &[u8]| m.iter().map(|&a| a as u32).sum::<u32>();
        assert!(ink(&bold) > ink(&regular) * 11 / 10, "bold is heavier");

        cache.glyph('I', ITALIC).unwrap().unwrap();
        let (iw, ih, italic) = last_mask(&cache);
        // The ink's centre moves right from the bottom row to the top row.
        let centre = |m: &[u8], w: u32, y: u32| {
            let row = &m[(y * w) as usize..((y + 1) * w) as usize];
            let n: u32 = row.iter().map(|&a| a as u32).sum();
            row.iter()
                .enumerate()
                .map(|(x, &a)| x as u32 * a as u32)
                .sum::<u32>() as f32
                / n.max(1) as f32
        };
        let lean = centre(&italic, iw, 1) - centre(&italic, iw, ih - 2);
        let upright = centre(&regular, w, 1) - centre(&regular, w, h - 2);
        assert!(upright.abs() < 1.0, "{upright}");
        assert!(lean > 2.0, "synthesized italic leans right ({lean})");
    }

    #[test]
    fn special_glyphs_fill_the_cell_and_are_cached() {
        let Some(mut cache) = single_face_cache() else {
            return;
        };
        let m = cache.fonts.metrics;
        let g = cache.glyph('─', 0).unwrap().unwrap();
        assert_eq!(g.dest, [0.0, 0.0, m.width as f32, m.height as f32]);
        let uploads = cache.mask.pending.len();
        assert_eq!(cache.glyph('─', 0).unwrap(), Some(g));
        assert_eq!(
            cache.mask.pending.len(),
            uploads,
            "cached, not rasterized again"
        );
        assert_eq!(
            cache.glyph('─', BOLD | ITALIC).unwrap(),
            Some(g),
            "style-independent"
        );
        assert_eq!(cache.mask.pending.len(), uploads);
        assert_eq!(cache.glyph(' ', 0).unwrap(), None);
        assert_eq!(cache.glyph('\u{7}', 0).unwrap(), None);
    }
}
