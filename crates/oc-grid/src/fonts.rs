//! Font discovery, selection, fallback and cell metrics.
//!
//! The system font list is scanned once per process ([`FontLibrary::system`])
//! and shared; each renderer owns a [`FontSet`] for its configured family and
//! size, which resolves characters to faces (primary font first, then
//! fallbacks — configured, Nerd Fonts, platform symbol/emoji/CJK fonts, and
//! finally any installed font) and caches the answer.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use rustc_hash::FxHashMap;
use swash::{CacheKey, FontRef, StringId};

/// Monospace families tried in order when none is configured (or the
/// configured one isn't installed). Same list as the Python version.
pub const MONO_FAMILIES: &[&str] = &[
    "JetBrains Mono",
    "Cascadia Mono",
    "Cascadia Code",
    "SF Mono",
    "Menlo",
    "Consolas",
    "DejaVu Sans Mono",
    "Ubuntu Mono",
    "Liberation Mono",
];

/// Families that carry Nerd Font icons, preferred for private-use characters.
const NERD_FAMILIES: &[&str] = &["Symbols Nerd Font Mono", "Symbols Nerd Font"];

/// Whether a family is a Nerd Font (patched with the prompt icons), by the
/// names Nerd Fonts and powerlevel10k use (as 1.x's `nerd_fonts()`).
pub(crate) fn is_nerd_family(name: &str) -> bool {
    name.contains("Nerd Font") || name.ends_with(" NF") || name.ends_with(" NFM") || name.ends_with(" NFP") || name.contains(" NF ")
}

/// Per-platform fallbacks for symbols, emoji and CJK before scanning every font.
#[cfg(target_os = "macos")]
const PLATFORM_FALLBACKS: &[&str] = &[
    "Menlo",
    "Apple Symbols",
    "Apple Color Emoji",
    "PingFang SC",
    "Hiragino Sans",
    "Apple SD Gothic Neo",
    "Arial Unicode MS",
    "STIX Two Math",
];
#[cfg(target_os = "windows")]
const PLATFORM_FALLBACKS: &[&str] = &[
    "Consolas",
    "Segoe UI Symbol",
    "Segoe UI Emoji",
    "MS Gothic",
    "Microsoft YaHei",
    "Malgun Gothic",
    "SimSun",
];
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const PLATFORM_FALLBACKS: &[&str] = &[
    "DejaVu Sans Mono",
    "Noto Sans Mono",
    "Noto Sans Symbols 2",
    "Noto Color Emoji",
    "Noto Sans CJK SC",
    "Noto Sans Mono CJK SC",
    "Symbola",
];

/// Logical pixels per typographic point. Matches Qt: on macOS a 13 pt font is
/// 13 logical pixels, elsewhere point sizes are at 96 dpi.
pub const fn px_per_pt() -> f32 {
    if cfg!(target_os = "macos") {
        1.0
    } else {
        96.0 / 72.0
    }
}

pub type FontData = Arc<dyn AsRef<[u8]> + Send + Sync>;

// ---------------------------------------------------------------------------
// Library
// ---------------------------------------------------------------------------

/// The installed fonts (a `fontdb` database) plus memory-mapped font files,
/// shared by every renderer in the process.
pub struct FontLibrary {
    db: fontdb::Database,
    files: Mutex<HashMap<PathBuf, FontData>>,
}

impl std::fmt::Debug for FontLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FontLibrary")
            .field("faces", &self.db.len())
            .finish()
    }
}

impl FontLibrary {
    /// The system fonts, scanned on first use (≈100 ms) and shared afterwards.
    pub fn system() -> Arc<FontLibrary> {
        static SYSTEM: OnceLock<Arc<FontLibrary>> = OnceLock::new();
        SYSTEM
            .get_or_init(|| {
                let mut db = fontdb::Database::new();
                db.load_system_fonts();
                Arc::new(FontLibrary::from_database(db))
            })
            .clone()
    }

    /// A library over a prepared database (tests, bundled fonts).
    pub fn from_database(db: fontdb::Database) -> Self {
        FontLibrary {
            db,
            files: Mutex::new(HashMap::new()),
        }
    }

    pub fn database(&self) -> &fontdb::Database {
        &self.db
    }

    /// Sorted, de-duplicated family names (for a font picker).
    pub fn families(&self, monospace_only: bool) -> Vec<String> {
        let mut names: Vec<String> = self
            .db
            .faces()
            .filter(|f| !monospace_only || f.monospaced)
            .filter_map(|f| f.families.first().map(|(n, _)| n.clone()))
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort_unstable_by_key(|n| n.to_lowercase());
        names.dedup();
        names
    }

    pub fn has_family(&self, family: &str) -> bool {
        !family.is_empty()
            && self.db.faces().any(|f| {
                f.families
                    .iter()
                    .any(|(n, _)| n.eq_ignore_ascii_case(family))
            })
    }

    /// The family a renderer uses for `requested`: itself if installed, else
    /// the first installed of [`MONO_FAMILIES`], else any monospace font.
    /// Port of `resolve_font` / `default_monospace_font`.
    pub fn resolve_family(&self, requested: &str) -> Option<String> {
        if self.has_family(requested) {
            return self.canonical_family(requested);
        }
        MONO_FAMILIES
            .iter()
            .find(|f| self.has_family(f))
            .and_then(|f| self.canonical_family(f))
            .or_else(|| {
                self.db
                    .faces()
                    .find(|f| {
                        f.monospaced && f.families.first().is_some_and(|(n, _)| !n.starts_with('.'))
                    })
                    .or_else(|| self.db.faces().next())
                    .and_then(|f| f.families.first().map(|(n, _)| n.clone()))
            })
    }

    /// Style names of a family's faces ("Regular", "Light", "Bold Italic"),
    /// lightest first, for the settings' style picker (Qt's `QFontDatabase.styles`).
    pub fn styles(&self, family: &str) -> Vec<String> {
        let mut faces: Vec<(u16, bool, String)> = self
            .family_faces(family)
            .filter_map(|f| {
                let name = self.load(f.id).and_then(|face| face.style_name())?;
                Some((f.weight.0, f.style != fontdb::Style::Normal, name))
            })
            .collect();
        faces.sort();
        let mut names: Vec<String> = Vec::with_capacity(faces.len());
        for (_, _, name) in faces {
            if !names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
                names.push(name);
            }
        }
        names
    }

    fn family_faces<'a>(&'a self, family: &'a str) -> impl Iterator<Item = &'a fontdb::FaceInfo> {
        self.db.faces().filter(move |f| {
            f.families
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case(family))
        })
    }

    /// The face of `family` whose style name is `style` (case-insensitive).
    fn query_style(&self, family: &str, style: &str) -> Option<fontdb::ID> {
        if style.is_empty() {
            return None;
        }
        self.family_faces(family).find_map(|f| {
            let name = self.load(f.id)?.style_name()?;
            name.eq_ignore_ascii_case(style).then_some(f.id)
        })
    }

    fn canonical_family(&self, name: &str) -> Option<String> {
        self.db.faces().find_map(|f| {
            f.families
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(n, _)| n.clone())
        })
    }

    fn query(&self, family: &str, bold: bool, italic: bool) -> Option<fontdb::ID> {
        self.query_weight(family, fontdb::Weight::NORMAL, bold, italic)
    }

    /// Like [`Self::query`] relative to a base weight (a "Light" style's
    /// italic is Light Italic; its bold is at least Bold).
    fn query_weight(
        &self,
        family: &str,
        base: fontdb::Weight,
        bold: bool,
        italic: bool,
    ) -> Option<fontdb::ID> {
        let families = [fontdb::Family::Name(family)];
        self.db.query(&fontdb::Query {
            families: &families,
            weight: if bold {
                fontdb::Weight(base.0.max(fontdb::Weight::BOLD.0))
            } else {
                base
            },
            stretch: fontdb::Stretch::Normal,
            style: if italic {
                fontdb::Style::Italic
            } else {
                fontdb::Style::Normal
            },
        })
    }

    /// Load a face's bytes (memory-mapped files are shared between faces of a collection).
    fn load(&self, id: fontdb::ID) -> Option<LoadedFace> {
        let info = self.db.face(id)?;
        let data: FontData = match &info.source {
            fontdb::Source::Binary(data) => data.clone(),
            fontdb::Source::SharedFile(_, data) => data.clone(),
            fontdb::Source::File(path) => {
                let mut files = self.files.lock().unwrap_or_else(|e| e.into_inner());
                match files.get(path) {
                    Some(data) => data.clone(),
                    None => {
                        let file = std::fs::File::open(path).ok()?;
                        // SAFETY: font files are read-only system/user files; like
                        // fontdb's own shared mode we accept the (theoretical) risk
                        // of the file changing underneath the mapping.
                        let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
                        let data: FontData = Arc::new(map);
                        files.insert(path.clone(), data.clone());
                        data
                    }
                }
            }
        };
        let font = FontRef::from_index((*data).as_ref(), info.index as usize)?;
        Some(LoadedFace {
            id,
            offset: font.offset,
            key: font.key,
            data,
            bold: info.weight.0 >= 600,
            italic: info.style != fontdb::Style::Normal,
        })
    }
}

/// A face ready for swash.
pub(crate) struct LoadedFace {
    pub id: fontdb::ID,
    data: FontData,
    offset: u32,
    key: CacheKey,
    /// The face itself is bold/italic (otherwise those styles are synthesized).
    pub bold: bool,
    pub italic: bool,
}

impl LoadedFace {
    pub fn font(&self) -> FontRef<'_> {
        FontRef {
            data: (*self.data).as_ref(),
            offset: self.offset,
            key: self.key,
        }
    }

    pub fn glyph(&self, ch: char) -> u16 {
        self.font().charmap().map(ch)
    }

    /// The style name Qt shows (typographic subfamily, else subfamily), English preferred.
    fn style_name(&self) -> Option<String> {
        let strings = self.font().localized_strings();
        [StringId::TypographicSubFamily, StringId::SubFamily]
            .into_iter()
            .find_map(|id| {
                strings
                    .find_by_id(id, Some("en"))
                    .or_else(|| strings.find_by_id(id, None))
            })
            .map(|s| s.chars().collect::<String>())
            .filter(|s| !s.trim().is_empty())
    }
}

// ---------------------------------------------------------------------------
// Configuration and metrics
// ---------------------------------------------------------------------------

/// The user's font settings.
#[derive(Clone, Debug, PartialEq)]
pub struct FontConfig {
    /// Family name; empty or not installed uses the default monospace font.
    pub family: String,
    /// Style name within the family ("Light", "Medium", as listed by
    /// [`FontLibrary::styles`]); empty or unknown uses the regular face.
    /// Python's `Appearance.font_style`.
    pub style: String,
    /// Size in points (see [`px_per_pt`]).
    pub size: f32,
    /// Row height as a multiple of the font's height (Python's `line_spacing`, 0.8–2.0).
    pub line_spacing: f32,
    /// Families tried before the built-in fallbacks for characters the main font lacks.
    pub fallback: Vec<String>,
}

impl Default for FontConfig {
    fn default() -> Self {
        FontConfig {
            family: String::new(),
            style: String::new(),
            size: 13.0,
            line_spacing: 1.0,
            fallback: Vec::new(),
        }
    }
}

impl FontConfig {
    pub fn new(family: impl Into<String>, size: f32) -> Self {
        FontConfig {
            family: family.into(),
            size,
            ..Self::default()
        }
    }

    /// Clamp to the ranges the settings allow (Python `LINE_SPACING_RANGE`).
    pub fn sanitized(mut self) -> Self {
        if !self.size.is_finite() || self.size <= 0.0 {
            self.size = 13.0;
        }
        self.size = self.size.clamp(4.0, 200.0);
        self.line_spacing = if self.line_spacing.is_finite() {
            self.line_spacing.clamp(0.8, 2.0)
        } else {
            1.0
        };
        self
    }
}

/// Cell geometry in physical pixels. Integers on purpose: every cell starts on
/// a pixel boundary, so lines and backgrounds never blur or leave seams.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    pub width: u32,
    pub height: u32,
    /// Baseline, from the cell top.
    pub baseline: u32,
    /// Top of the underline, from the cell top.
    pub underline_y: u32,
    pub underline_thickness: u32,
    /// Top of the strikethrough line, from the cell top.
    pub strike_y: u32,
    /// Font size in physical pixels (ppem).
    pub font_px: f32,
}

impl CellMetrics {
    fn compute(face: &LoadedFace, px: f32, line_spacing: f32) -> Self {
        let font = face.font();
        let m = font.metrics(&[]).scale(px);
        let gid = face.glyph('M');
        let advance = font.glyph_metrics(&[]).scale(px).advance_width(gid);
        let advance = if advance > 0.0 { advance } else { px * 0.6 };
        let ascent = m.ascent.abs();
        let descent = m.descent.abs();
        let natural = ascent + descent;
        let width = (advance.round() as u32).max(1);
        let height = ((natural * line_spacing).round() as u32).max(1);
        let baseline =
            (((height as f32 - natural) / 2.0 + ascent).round().max(1.0) as u32).min(height);
        // At least one pixel per 14 px of font size, so lines scale with HiDPI
        // like Qt's (fonts often specify hairline strokes).
        let thickness = (m.stroke_size.max(px / 14.0).round() as u32).clamp(1, (height / 8).max(1));
        // swash reports the underline offset below the baseline as negative.
        let below = (-m.underline_offset).round().max(1.0) as u32;
        let underline_y = (baseline + below).min(height.saturating_sub(thickness));
        let strike_up = if m.strikeout_offset > 0.0 {
            m.strikeout_offset
        } else {
            m.x_height / 2.0
        };
        let strike_y = baseline.saturating_sub((strike_up.round() as u32).max(1));
        CellMetrics {
            width,
            height,
            baseline,
            underline_y,
            underline_thickness: thickness,
            strike_y,
            font_px: px,
        }
    }

    /// How many whole cells fit into a `w`×`h` pixel area (at least 1×1).
    pub fn grid_size(&self, w: u32, h: u32) -> (u16, u16) {
        (
            (w / self.width).clamp(1, u16::MAX as u32) as u16,
            (h / self.height).clamp(1, u16::MAX as u32) as u16,
        )
    }
}

// ---------------------------------------------------------------------------
// Face set
// ---------------------------------------------------------------------------

/// Style bits: 1 = bold, 2 = italic.
pub(crate) type StyleBits = u8;
pub(crate) const BOLD: StyleBits = 1;
pub(crate) const ITALIC: StyleBits = 2;
/// Box-drawing lines thicker ([`crate::Flags::STRONG`]); only geometric glyphs use it.
pub(crate) const STRONG: StyleBits = 4;

pub(crate) type FaceIdx = u16;

/// Faces of one configured font, plus the fallback chain.
pub(crate) struct FontSet {
    lib: Arc<FontLibrary>,
    pub faces: Vec<LoadedFace>,
    loaded: FxHashMap<fontdb::ID, FaceIdx>,
    /// Face per style bits (regular, bold, italic, bold italic).
    primary: [FaceIdx; 4],
    /// Named fallbacks in order; the full scan list is built only when needed.
    fallbacks: Vec<fontdb::ID>,
    scan: Option<Vec<fontdb::ID>>,
    fallback_cache: FxHashMap<char, Option<FaceIdx>>,
    pub family: String,
    pub metrics: CellMetrics,
}

impl FontSet {
    pub fn new(lib: Arc<FontLibrary>, config: &FontConfig, scale_factor: f32) -> Option<Self> {
        let config = config.clone().sanitized();
        let family = lib.resolve_family(&config.family)?;
        let mut set = FontSet {
            lib: lib.clone(),
            faces: Vec::new(),
            loaded: FxHashMap::default(),
            primary: [0; 4],
            fallbacks: Vec::new(),
            scan: None,
            fallback_cache: FxHashMap::default(),
            family: family.clone(),
            metrics: CellMetrics {
                width: 1,
                height: 1,
                baseline: 1,
                underline_y: 0,
                underline_thickness: 1,
                strike_y: 0,
                font_px: 1.0,
            },
        };
        // The configured style (e.g. "Light") is the face for plain text;
        // bold/italic are looked up relative to its weight.
        let regular = lib
            .query_style(&family, &config.style)
            .or_else(|| lib.query(&family, false, false))?;
        let base = lib
            .db
            .face(regular)
            .map_or(fontdb::Weight::NORMAL, |f| f.weight);
        let regular = set.face_for(regular)?;
        set.primary[0] = regular;
        for bits in 1..4u8 {
            let id = lib.query_weight(&family, base, bits & BOLD != 0, bits & ITALIC != 0);
            set.primary[bits as usize] = id.and_then(|id| set.face_for(id)).unwrap_or(regular);
        }
        // Only keep a styled face if it really is that style (fontdb returns
        // the closest match); otherwise style is synthesized from regular.
        for bits in 1..4u8 {
            let f = &set.faces[set.primary[bits as usize] as usize];
            if (bits & BOLD != 0 && !f.bold) || (bits & ITALIC != 0 && !f.italic) {
                set.primary[bits as usize] =
                    if bits == BOLD | ITALIC && set.faces[set.primary[1] as usize].bold {
                        set.primary[1]
                    } else {
                        regular
                    };
            }
        }
        let mut seen = std::collections::HashSet::new();
        seen.insert(set.faces[regular as usize].id);
        let add =
            |set: &mut FontSet, seen: &mut std::collections::HashSet<fontdb::ID>, name: &str| {
                if let Some(id) = lib.query(name, false, false)
                    && seen.insert(id)
                {
                    set.fallbacks.push(id);
                }
            };
        for name in config
            .fallback
            .iter()
            .map(String::as_str)
            .chain(NERD_FAMILIES.iter().copied())
        {
            add(&mut set, &mut seen, name);
        }
        // Any installed Nerd Font ("FiraCode Nerd Font Mono", powerlevel10k's
        // "MesloLGS NF") also carries the icons: right after the configured and
        // Symbols Nerd fonts, before the platform ones; monospaced ones first.
        if let Some(face) = lib
            .db
            .faces()
            .filter(|face| {
                face.families.first().is_some_and(|(n, _)| is_nerd_family(n))
                    && face.weight == fontdb::Weight::NORMAL
                    && face.style == fontdb::Style::Normal
                    && !seen.contains(&face.id)
            })
            .min_by_key(|face| {
                let name = face.families.first().map_or("", |(n, _)| n.as_str());
                (!face.monospaced, !name.contains("Mono"), name.to_owned())
            })
        {
            seen.insert(face.id);
            set.fallbacks.push(face.id);
        }
        for name in PLATFORM_FALLBACKS {
            add(&mut set, &mut seen, name);
        }
        let px = config.size * px_per_pt() * scale_factor.max(0.1);
        set.metrics = CellMetrics::compute(&set.faces[regular as usize], px, config.line_spacing);
        Some(set)
    }

    fn face_for(&mut self, id: fontdb::ID) -> Option<FaceIdx> {
        if let Some(&idx) = self.loaded.get(&id) {
            return Some(idx);
        }
        let face = self.lib.load(id)?;
        let idx = self.faces.len() as FaceIdx;
        self.faces.push(face);
        self.loaded.insert(id, idx);
        Some(idx)
    }

    pub fn primary(&self, style: StyleBits) -> FaceIdx {
        self.primary[(style & 3) as usize]
    }

    /// The face and glyph for `ch` in `style`; `None` if no installed font has it.
    pub fn resolve(&mut self, ch: char, style: StyleBits) -> Option<(FaceIdx, u16)> {
        let primary = self.primary(style);
        let gid = self.faces[primary as usize].glyph(ch);
        if gid != 0 {
            return Some((primary, gid));
        }
        let face = self.fallback_face(ch)?;
        Some((face, self.faces[face as usize].glyph(ch)))
    }

    /// Whether the synthesized bold/italic applies for `face` in `style`.
    pub fn synth(&self, face: FaceIdx, style: StyleBits) -> (bool, bool) {
        let f = &self.faces[face as usize];
        (
            style & BOLD != 0 && !f.bold,
            style & ITALIC != 0 && !f.italic,
        )
    }

    fn fallback_face(&mut self, ch: char) -> Option<FaceIdx> {
        if let Some(&hit) = self.fallback_cache.get(&ch) {
            return hit;
        }
        let regular = self.primary[0];
        let mut found = (self.faces[regular as usize].glyph(ch) != 0).then_some(regular);
        if found.is_none() {
            found = self.search(ch, false);
        }
        if found.is_none() {
            found = self.search(ch, true);
        }
        self.fallback_cache.insert(ch, found);
        found
    }

    fn search(&mut self, ch: char, full_scan: bool) -> Option<FaceIdx> {
        // Taken out (and put back below) instead of cloned: the full scan
        // list has an entry per installed face.
        let ids = if full_scan {
            match self.scan.take() {
                Some(ids) => ids,
                None => {
                    // Monospace and regular faces first, so a fallback looks as close as possible.
                    let mut faces: Vec<&fontdb::FaceInfo> = self
                        .lib
                        .db
                        .faces()
                        .filter(|f| !self.fallbacks.contains(&f.id))
                        .collect();
                    // Apple's LastResort maps every code point to a placeholder: only as a last resort.
                    faces.sort_by_key(|f| {
                        let last = f.families.iter().any(|(n, _)| n.contains("LastResort"));
                        (
                            last,
                            !f.monospaced,
                            f.style != fontdb::Style::Normal,
                            f.weight.0.abs_diff(400),
                        )
                    });
                    faces.into_iter().map(|f| f.id).collect()
                }
            }
        } else {
            std::mem::take(&mut self.fallbacks)
        };
        let found = self.search_in(&ids, ch);
        if full_scan {
            self.scan = Some(ids);
        } else {
            self.fallbacks = ids;
        }
        found
    }

    fn search_in(&mut self, ids: &[fontdb::ID], ch: char) -> Option<FaceIdx> {
        for &id in ids {
            if let Some(&idx) = self.loaded.get(&id) {
                if self.faces[idx as usize].glyph(ch) != 0 {
                    return Some(idx);
                }
                continue;
            }
            // Check coverage before keeping the face loaded.
            let Some(face) = self.lib.load(id) else {
                continue;
            };
            if face.glyph(ch) != 0 {
                let idx = self.faces.len() as FaceIdx;
                self.faces.push(face);
                self.loaded.insert(id, idx);
                return Some(idx);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nerd_font_names() {
        for name in ["MesloLGS NF", "FiraCode Nerd Font Mono", "Hack Nerd Font", "JetBrainsMono NFM", "Iosevka NF Light"] {
            assert!(is_nerd_family(name), "{name}");
        }
        for name in ["Menlo", "SF Mono", "Fira Code", "NFL Sans"] {
            assert!(!is_nerd_family(name), "{name}");
        }
    }

    #[test]
    fn config_is_sanitized() {
        let c = FontConfig {
            size: -3.0,
            line_spacing: 9.0,
            ..FontConfig::default()
        }
        .sanitized();
        assert_eq!(c.size, 13.0);
        assert_eq!(c.line_spacing, 2.0);
        let c = FontConfig {
            line_spacing: f32::NAN,
            ..FontConfig::default()
        }
        .sanitized();
        assert_eq!(c.line_spacing, 1.0);
    }

    #[test]
    fn empty_library_has_no_family() {
        let lib = FontLibrary::from_database(fontdb::Database::new());
        assert_eq!(lib.resolve_family("Menlo"), None);
        assert!(!lib.has_family(""));
        assert!(FontSet::new(Arc::new(lib), &FontConfig::default(), 1.0).is_none());
    }

    #[test]
    fn system_font_resolves_and_has_sane_metrics() {
        let lib = FontLibrary::system();
        if lib.database().is_empty() {
            return; // no fonts on this machine (minimal CI container)
        }
        let set = FontSet::new(lib.clone(), &FontConfig::default(), 1.0).expect("a monospace font");
        let m = set.metrics;
        assert!(m.width >= 5 && m.width <= 12, "{m:?}");
        assert!(m.height >= 12 && m.height <= 24, "{m:?}");
        assert!(m.baseline > m.height / 2 && m.baseline < m.height, "{m:?}");
        assert!(
            m.underline_y >= m.baseline && m.underline_y < m.height,
            "{m:?}"
        );
        assert!(m.strike_y < m.baseline, "{m:?}");

        // Unknown family: falls back to the default monospace family.
        let other =
            FontSet::new(lib.clone(), &FontConfig::new("No Such Font 123", 13.0), 1.0).unwrap();
        assert_eq!(other.family, set.family);

        // HiDPI doubles the pixel metrics (within rounding).
        let hi = FontSet::new(lib.clone(), &FontConfig::default(), 2.0).unwrap();
        assert!(hi.metrics.width.abs_diff(2 * m.width) <= 1);
        assert!(hi.metrics.height.abs_diff(2 * m.height) <= 1);

        // Line spacing stretches the row, not the column.
        let tall = FontSet::new(
            lib,
            &FontConfig {
                line_spacing: 1.5,
                ..FontConfig::default()
            },
            1.0,
        )
        .unwrap();
        assert_eq!(tall.metrics.width, m.width);
        assert!(tall.metrics.height as f32 >= m.height as f32 * 1.4);
    }

    #[test]
    fn configured_style_picks_that_face() {
        // Port of Python's resolve_font(family, style, size): a listed style
        // is used for plain text, an unknown one falls back to regular.
        let lib = FontLibrary::system();
        let Some(family) = lib.resolve_family("") else {
            return;
        };
        let styles = lib.styles(&family);
        assert!(!styles.is_empty(), "{family} lists no styles");
        let plain = FontSet::new(lib.clone(), &FontConfig::new(&*family, 13.0), 1.0).unwrap();
        let regular_id = plain.faces[plain.primary(0) as usize].id;
        let unknown = FontSet::new(
            lib.clone(),
            &FontConfig {
                style: "No Such Style".into(),
                ..FontConfig::new(&*family, 13.0)
            },
            1.0,
        )
        .unwrap();
        assert_eq!(unknown.faces[unknown.primary(0) as usize].id, regular_id);
        // A bold style (every common monospace family has one) becomes the base face.
        if let Some(bold) = styles.iter().find(|s| s.eq_ignore_ascii_case("bold")) {
            let set = FontSet::new(
                lib.clone(),
                &FontConfig {
                    style: bold.to_uppercase(),
                    ..FontConfig::new(&*family, 13.0)
                },
                1.0,
            )
            .unwrap();
            let base = &set.faces[set.primary(0) as usize];
            assert!(base.bold, "style {bold} of {family}");
            assert_ne!(base.id, regular_id);
            assert_eq!(set.synth(set.primary(BOLD), BOLD), (false, false));
        }
    }

    #[test]
    fn nerd_fonts_come_before_platform_fallbacks() {
        let lib = FontLibrary::system();
        if lib.database().is_empty() {
            return;
        }
        let set = FontSet::new(lib.clone(), &FontConfig::default(), 1.0).unwrap();
        let is_nerd = |id: &fontdb::ID| {
            lib.database()
                .face(*id)
                .is_some_and(|f| f.families.iter().any(|(n, _)| n.contains("Nerd Font")))
        };
        if let Some(first_platform) = set.fallbacks.iter().position(|id| !is_nerd(id)) {
            assert!(
                !set.fallbacks[first_platform..].iter().any(is_nerd),
                "a Nerd Font after a platform fallback"
            );
        }
    }

    #[test]
    fn fallback_finds_characters_the_main_font_lacks() {
        let lib = FontLibrary::system();
        if lib.database().is_empty() {
            return;
        }
        let mut set = FontSet::new(lib, &FontConfig::default(), 1.0).unwrap();
        let (face, gid) = set.resolve('A', 0).unwrap();
        assert_eq!(face, set.primary(0));
        assert_ne!(gid, 0);
        if cfg!(target_os = "macos") {
            let (face, _) = set.resolve('日', 0).expect("CJK fallback");
            assert_ne!(face, set.primary(0));
            assert!(set.resolve('😀', 0).is_some(), "emoji fallback");
        }
        // The answer for a code point few fonts define (maybe only a
        // last-resort placeholder font) is cached either way.
        let first = set.resolve('\u{10FFFD}', 0);
        assert!(set.fallback_cache.contains_key(&'\u{10FFFD}'));
        assert_eq!(set.resolve('\u{10FFFD}', 0), first);
    }
}
