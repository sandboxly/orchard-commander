//! What the UI draws: a grid of character cells plus a few overlays (colored
//! rectangles, images, a cursor). Pure data — no GPU — so the UI can build a
//! frame on any thread and tests can inspect it.

use std::borrow::Cow;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::color::Rgba;

// ---------------------------------------------------------------------------
// Cells
// ---------------------------------------------------------------------------

/// Text attributes of a cell. A plain bit set (no dependency) so cells stay `Copy`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Flags(u16);

impl Flags {
    pub const NONE: Flags = Flags(0);
    pub const BOLD: Flags = Flags(1);
    pub const ITALIC: Flags = Flags(1 << 1);
    pub const UNDERLINE: Flags = Flags(1 << 2);
    pub const STRIKETHROUGH: Flags = Flags(1 << 3);
    /// Drawn at half intensity (the foreground blended halfway into the background).
    pub const DIM: Flags = Flags(1 << 4);
    /// Foreground and background swapped (selection, terminal reverse video).
    pub const INVERSE: Flags = Flags(1 << 5);
    /// The first half of a double-width character; the next cell is its [`Self::WIDE_CONT`].
    pub const WIDE: Flags = Flags(1 << 6);
    /// The second half of a double-width character: draws its background, never a glyph.
    pub const WIDE_CONT: Flags = Flags(1 << 7);

    pub const fn bits(self) -> u16 {
        self.0
    }

    pub const fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Flags) -> Flags {
        Flags(self.0 | other.0)
    }

    pub fn set(&mut self, other: Flags, on: bool) {
        if on {
            self.0 |= other.0
        } else {
            self.0 &= !other.0
        }
    }
}

impl std::ops::BitOr for Flags {
    type Output = Flags;
    fn bitor(self, rhs: Flags) -> Flags {
        self.union(rhs)
    }
}

impl std::ops::BitOrAssign for Flags {
    fn bitor_assign(&mut self, rhs: Flags) {
        self.0 |= rhs.0
    }
}

impl std::fmt::Debug for Flags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const NAMES: [&str; 8] = [
            "BOLD",
            "ITALIC",
            "UNDERLINE",
            "STRIKE",
            "DIM",
            "INVERSE",
            "WIDE",
            "WIDE_CONT",
        ];
        let names: Vec<&str> = (0..8)
            .filter(|i| self.0 & (1 << i) != 0)
            .map(|i| NAMES[i])
            .collect();
        write!(f, "Flags({})", names.join("|"))
    }
}

/// Colors and attributes applied to text written with [`Frame::put_str`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Style {
    pub fg: Rgba,
    /// [`Rgba::TRANSPARENT`] keeps whatever background the frame has there.
    pub bg: Rgba,
    pub flags: Flags,
}

impl Style {
    pub const fn new(fg: Rgba, bg: Rgba) -> Self {
        Style {
            fg,
            bg,
            flags: Flags::NONE,
        }
    }

    /// Foreground only, transparent background.
    pub const fn fg(fg: Rgba) -> Self {
        Style {
            fg,
            bg: Rgba::TRANSPARENT,
            flags: Flags::NONE,
        }
    }

    pub const fn with(self, flags: Flags) -> Self {
        Style {
            flags: self.flags.union(flags),
            ..self
        }
    }

    pub const fn bold(self) -> Self {
        self.with(Flags::BOLD)
    }
}

impl Default for Style {
    fn default() -> Self {
        Style::fg(Rgba::WHITE)
    }
}

/// One character cell (20 bytes, `Copy`, so a 240x70 grid is a flat 330 KB).
///
/// Multi-code-point graphemes (combining accents, emoji sequences) live in
/// the frame's cluster pool; `ch` then holds their first code point.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: Rgba,
    pub bg: Rgba,
    pub flags: Flags,
    /// 1-based index into [`Frame`]'s cluster pool; 0 when `ch` is the whole grapheme.
    cluster: u32,
}

impl Cell {
    pub const BLANK: Cell = Cell {
        ch: ' ',
        fg: Rgba::WHITE,
        bg: Rgba::TRANSPARENT,
        flags: Flags::NONE,
        cluster: 0,
    };

    pub const fn new(ch: char, style: Style) -> Self {
        Cell {
            ch,
            fg: style.fg,
            bg: style.bg,
            flags: style.flags,
            cluster: 0,
        }
    }

    pub const fn style(&self) -> Style {
        Style {
            fg: self.fg,
            bg: self.bg,
            flags: self.flags,
        }
    }

    /// Whether the glyph pass can skip this cell.
    pub fn is_blank(&self) -> bool {
        self.cluster == 0 && (self.ch == ' ' || self.flags.contains(Flags::WIDE_CONT))
    }

    pub const fn has_cluster(&self) -> bool {
        self.cluster != 0
    }
}

impl Default for Cell {
    fn default() -> Self {
        Cell::BLANK
    }
}

// ---------------------------------------------------------------------------
// Overlays
// ---------------------------------------------------------------------------

/// A rectangle in physical pixels, relative to the grid's top-left corner.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct PxRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl PxRect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        PxRect { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    /// The overlap of two rectangles (empty when they don't touch).
    pub fn intersect(&self, other: &PxRect) -> PxRect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let w = (self.right().min(other.right()) - x).max(0.0);
        let h = (self.bottom().min(other.bottom()) - y).max(0.0);
        PxRect { x, y, w, h }
    }
}

/// A rectangle in cells (column, row, width, height).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CellRect {
    pub col: u16,
    pub row: u16,
    pub cols: u16,
    pub rows: u16,
}

impl CellRect {
    pub const fn new(col: u16, row: u16, cols: u16, rows: u16) -> Self {
        CellRect {
            col,
            row,
            cols,
            rows,
        }
    }
}

/// Where an overlay goes: pixel-exact (scrollbar thumbs, image previews) or
/// snapped to cells (selection bars), resolved with the renderer's metrics.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Area {
    Px(PxRect),
    Cells(CellRect),
}

impl From<PxRect> for Area {
    fn from(r: PxRect) -> Self {
        Area::Px(r)
    }
}

impl From<CellRect> for Area {
    fn from(r: CellRect) -> Self {
        Area::Cells(r)
    }
}

/// Whether an overlay is drawn under the text (selection bars) or over it
/// (scrollbars, focus outlines).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Layer {
    #[default]
    BelowText,
    AboveText,
}

/// A solid colored rectangle.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct FillRect {
    pub area: Area,
    pub color: Rgba,
    pub layer: Layer,
}

/// Handle of an image uploaded to a renderer (see `GridRenderer::upload_image`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ImageId(pub(crate) u64);

/// An uploaded image drawn into a rectangle, optionally clipped (a preview
/// scrolled partly out of its panel). Drawn above cell backgrounds, below text.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ImagePlacement {
    pub image: ImageId,
    pub dest: Area,
    pub clip: Option<Area>,
    /// 0.0–1.0, multiplies the image's own alpha.
    pub opacity: f32,
}

impl ImagePlacement {
    pub fn new(image: ImageId, dest: impl Into<Area>) -> Self {
        ImagePlacement {
            image,
            dest: dest.into(),
            clip: None,
            opacity: 1.0,
        }
    }

    pub fn clipped(self, clip: impl Into<Area>) -> Self {
        ImagePlacement {
            clip: Some(clip.into()),
            ..self
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CursorShape {
    /// Filled cell; the character under it is redrawn in `text`.
    #[default]
    Block,
    /// Outline of the cell — the unfocused terminal cursor.
    HollowBlock,
    /// Thin vertical bar at the cell's left edge (text input).
    Beam,
    Underline,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cursor {
    pub col: u16,
    pub row: u16,
    pub shape: CursorShape,
    pub color: Rgba,
    /// Color of the character under a block cursor; `None` uses the cell background.
    pub text: Option<Rgba>,
}

// ---------------------------------------------------------------------------
// Frame
// ---------------------------------------------------------------------------

/// A full screen of cells plus overlays. Build one per redraw (or keep one and
/// rewrite it); the renderer only reads it.
#[derive(Clone, Debug)]
pub struct Frame {
    cols: u16,
    rows: u16,
    cells: Vec<Cell>,
    clusters: Vec<Box<str>>,
    /// Shows wherever a cell background is transparent, and beyond the grid.
    pub background: Rgba,
    pub rects: Vec<FillRect>,
    pub images: Vec<ImagePlacement>,
    pub cursor: Option<Cursor>,
}

impl Frame {
    pub fn new(cols: u16, rows: u16, background: Rgba) -> Self {
        Frame {
            cols,
            rows,
            cells: vec![Cell::BLANK; cols as usize * rows as usize],
            clusters: Vec::new(),
            background,
            rects: Vec::new(),
            images: Vec::new(),
            cursor: None,
        }
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Change the grid size; contents are cleared (the UI redraws on resize anyway).
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.cells.clear();
        self.cells
            .resize(cols as usize * rows as usize, Cell::BLANK);
        self.clear(self.background);
    }

    /// Blank every cell and drop all overlays, keeping the allocation.
    pub fn clear(&mut self, background: Rgba) {
        self.background = background;
        self.cells.fill(Cell::BLANK);
        self.clusters.clear();
        self.rects.clear();
        self.images.clear();
        self.cursor = None;
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// One row of cells. Panics if `row` is outside the grid.
    pub fn row(&self, row: u16) -> &[Cell] {
        let start = row as usize * self.cols as usize;
        &self.cells[start..start + self.cols as usize]
    }

    fn index(&self, col: u16, row: u16) -> Option<usize> {
        (col < self.cols && row < self.rows)
            .then(|| row as usize * self.cols as usize + col as usize)
    }

    pub fn cell(&self, col: u16, row: u16) -> Option<&Cell> {
        self.index(col, row).map(|i| &self.cells[i])
    }

    pub fn cell_mut(&mut self, col: u16, row: u16) -> Option<&mut Cell> {
        self.index(col, row).map(|i| &mut self.cells[i])
    }

    /// The full grapheme of a cell (its char, or its cluster).
    pub fn grapheme<'a>(&'a self, cell: &Cell) -> Cow<'a, str> {
        if cell.cluster != 0
            && let Some(s) = self.clusters.get(cell.cluster as usize - 1)
        {
            return Cow::Borrowed(s);
        }
        let mut buf = [0u8; 4];
        Cow::Owned(cell.ch.encode_utf8(&mut buf).to_owned())
    }

    pub(crate) fn cluster_str(&self, cell: &Cell) -> Option<&str> {
        (cell.cluster != 0)
            .then(|| self.clusters.get(cell.cluster as usize - 1).map(|s| &**s))
            .flatten()
    }

    /// Put one character. Double-width characters take this cell and the next;
    /// one that doesn't fit at the right edge becomes a space (like terminals).
    /// Returns the column after it.
    pub fn put_char(&mut self, col: u16, row: u16, ch: char, style: Style) -> u16 {
        let width = ch.width().unwrap_or(0);
        self.put_grapheme(col, row, ch, None, width, style)
    }

    fn put_grapheme(
        &mut self,
        col: u16,
        row: u16,
        ch: char,
        cluster: Option<&str>,
        width: usize,
        style: Style,
    ) -> u16 {
        if width == 0 || col >= self.cols || row >= self.rows {
            return col;
        }
        // WIDE/WIDE_CONT describe the grid layout, not a style: only we set them.
        let style = Style {
            flags: Flags(style.flags.0 & !(Flags::WIDE.0 | Flags::WIDE_CONT.0)),
            ..style
        };
        self.unsplit(col, row);
        let i = row as usize * self.cols as usize + col as usize;
        if width >= 2 {
            if col + 1 >= self.cols {
                self.cells[i] = Cell::new(' ', style);
                return col + 1;
            }
            self.unsplit(col + 1, row);
            let cluster = cluster.map_or(0, |s| self.intern(s));
            self.cells[i] = Cell {
                ch,
                cluster,
                ..Cell::new(ch, style.with(Flags::WIDE))
            };
            self.cells[i + 1] = Cell::new(' ', style.with(Flags::WIDE_CONT));
            return col + 2;
        }
        let cluster = cluster.map_or(0, |s| self.intern(s));
        self.cells[i] = Cell {
            cluster,
            ..Cell::new(ch, style)
        };
        col + 1
    }

    /// Overwriting half of a wide character blanks the other half, so no
    /// orphaned glyph or continuation remains.
    fn unsplit(&mut self, col: u16, row: u16) {
        let i = row as usize * self.cols as usize + col as usize;
        let flags = self.cells[i].flags;
        if flags.contains(Flags::WIDE) && col + 1 < self.cols {
            let next = &mut self.cells[i + 1];
            next.flags.set(Flags::WIDE_CONT, false);
            next.ch = ' ';
        } else if flags.contains(Flags::WIDE_CONT) && col > 0 {
            let prev = &mut self.cells[i - 1];
            prev.flags.set(Flags::WIDE, false);
            prev.ch = ' ';
            prev.cluster = 0;
        }
    }

    fn intern(&mut self, s: &str) -> u32 {
        // Overwritten cells leave their clusters behind; a frame that is kept
        // and rewritten every redraw (instead of cleared) would grow without
        // bound. Compacting once the pool outgrows the grid keeps it at most
        // ~2x the live clusters, amortized O(1) per write.
        if self.clusters.len() >= (2 * self.cells.len()).max(4096) {
            self.compact_clusters();
        }
        self.clusters.push(s.into());
        self.clusters.len() as u32
    }

    /// Drop clusters no cell refers to any more, renumbering the cells.
    fn compact_clusters(&mut self) {
        let mut remap = vec![0u32; self.clusters.len()];
        let mut live: Vec<Box<str>> = Vec::new();
        for cell in &mut self.cells {
            if cell.cluster == 0 {
                continue;
            }
            let old = cell.cluster as usize - 1;
            let Some(slot) = remap.get_mut(old) else {
                cell.cluster = 0; // stale index (a cell copied from another frame)
                continue;
            };
            if *slot == 0 {
                live.push(std::mem::take(&mut self.clusters[old]));
                *slot = live.len() as u32;
            }
            cell.cluster = *slot;
        }
        self.clusters = live;
    }

    /// Write `text` from (`col`, `row`), one grapheme per cell (two for wide
    /// ones), clipped at the right edge. Control characters and stray
    /// combining marks are skipped. Returns the column after the text.
    pub fn put_str(&mut self, col: u16, row: u16, text: &str, style: Style) -> u16 {
        self.put_str_clipped(col, row, text, style, self.cols)
    }

    /// Like [`Self::put_str`] but stops before column `end` (a panel's edge).
    pub fn put_str_clipped(
        &mut self,
        mut col: u16,
        row: u16,
        text: &str,
        style: Style,
        end: u16,
    ) -> u16 {
        let end = end.min(self.cols);
        if row >= self.rows {
            return col;
        }
        if text.is_ascii() {
            // Fast path: every byte is one cell.
            for b in text.bytes() {
                if col >= end {
                    break;
                }
                if b >= 0x20 && b != 0x7f {
                    col = self.put_grapheme(col, row, b as char, None, 1, style);
                }
            }
            return col;
        }
        for g in text.graphemes(true) {
            if col >= end {
                break;
            }
            let width = g.width();
            let mut chars = g.chars();
            let first = chars.next().unwrap_or(' ');
            if width == 0 || first.is_control() {
                continue;
            }
            if width >= 2 && col + 1 >= end {
                // Wide grapheme that doesn't fit before the clip edge.
                self.put_grapheme(col, row, ' ', None, 1, style);
                col += 1;
                break;
            }
            let cluster = chars.next().is_some().then_some(g);
            col = self.put_grapheme(col, row, first, cluster, width.min(2), style);
        }
        col
    }

    /// Fill a rectangle of cells with `ch` in `style`.
    pub fn fill(&mut self, rect: CellRect, ch: char, style: Style) {
        let cols = rect.col.saturating_add(rect.cols).min(self.cols);
        let rows = rect.row.saturating_add(rect.rows).min(self.rows);
        let width = ch.width().unwrap_or(0) as u16;
        if width == 0 {
            return;
        }
        for row in rect.row..rows {
            let mut col = rect.col;
            while col < cols {
                if width >= 2 && col + 1 >= cols {
                    // Half a wide char left over at the rect's edge.
                    self.put_char(col, row, ' ', style);
                    break;
                }
                col = self.put_char(col, row, ch, style);
            }
        }
    }

    /// Recolor a rectangle's background, keeping its text (selection bars).
    pub fn set_bg(&mut self, rect: CellRect, bg: Rgba) {
        self.update(rect, |c| c.bg = bg);
    }

    /// Recolor a rectangle's text and background, keeping the characters.
    pub fn set_style(&mut self, rect: CellRect, style: Style) {
        self.update(rect, |c| {
            c.fg = style.fg;
            c.bg = style.bg;
            let keep = c.flags.0 & (Flags::WIDE.0 | Flags::WIDE_CONT.0);
            c.flags = Flags(style.flags.0 & !(Flags::WIDE.0 | Flags::WIDE_CONT.0) | keep);
        });
    }

    fn update(&mut self, rect: CellRect, mut f: impl FnMut(&mut Cell)) {
        let cols = rect.col.saturating_add(rect.cols).min(self.cols);
        let rows = rect.row.saturating_add(rect.rows).min(self.rows);
        if rect.col >= cols {
            return; // starts right of the grid
        }
        for row in rect.row..rows {
            let start = row as usize * self.cols as usize;
            for cell in &mut self.cells[start + rect.col as usize..start + cols as usize] {
                f(cell);
            }
        }
    }

    /// Draw a box border with box-drawing characters; the renderer draws
    /// these geometrically so they join across cells at any line spacing.
    pub fn draw_box(&mut self, rect: CellRect, kind: BoxStyle, style: Style) {
        if rect.cols < 2 || rect.rows < 2 {
            return;
        }
        let [tl, tr, bl, br, h, v] = kind.chars();
        let (l, t) = (rect.col, rect.row);
        let r = rect.col.saturating_add(rect.cols - 1);
        let b = rect.row.saturating_add(rect.rows - 1);
        for col in l + 1..r {
            self.put_char(col, t, h, style);
            self.put_char(col, b, h, style);
        }
        for row in t + 1..b {
            self.put_char(l, row, v, style);
            self.put_char(r, row, v, style);
        }
        self.put_char(l, t, tl, style);
        self.put_char(r, t, tr, style);
        self.put_char(l, b, bl, style);
        self.put_char(r, b, br, style);
    }

    /// Add a solid rectangle overlay.
    pub fn add_rect(&mut self, area: impl Into<Area>, color: Rgba, layer: Layer) {
        self.rects.push(FillRect {
            area: area.into(),
            color,
            layer,
        });
    }

    pub fn add_image(&mut self, placement: ImagePlacement) {
        self.images.push(placement);
    }

    /// A row's text (wide continuations skipped), for tests and accessibility.
    /// Panics if `row` is outside the grid.
    pub fn row_text(&self, row: u16) -> String {
        let mut out = String::with_capacity(self.cols as usize);
        for cell in self.row(row) {
            if cell.flags.contains(Flags::WIDE_CONT) {
                continue;
            }
            match self.cluster_str(cell) {
                Some(s) => out.push_str(s),
                None => out.push(cell.ch),
            }
        }
        out
    }
}

/// Border styles, named as in the Python settings (`FRAME_STYLES`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BoxStyle {
    #[default]
    Rounded,
    Single,
    Double,
    Heavy,
}

impl BoxStyle {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "rounded" => BoxStyle::Rounded,
            "single" => BoxStyle::Single,
            "double" => BoxStyle::Double,
            "heavy" => BoxStyle::Heavy,
            _ => return None,
        })
    }

    /// Top-left, top-right, bottom-left, bottom-right, horizontal, vertical.
    pub const fn chars(self) -> [char; 6] {
        match self {
            BoxStyle::Rounded => ['╭', '╮', '╰', '╯', '─', '│'],
            BoxStyle::Single => ['┌', '┐', '└', '┘', '─', '│'],
            BoxStyle::Double => ['╔', '╗', '╚', '╝', '═', '║'],
            BoxStyle::Heavy => ['┏', '┓', '┗', '┛', '━', '┃'],
        }
    }
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

/// Display width of `text` in cells.
pub fn str_width(text: &str) -> usize {
    text.width()
}

/// Shorten `text` from the left to at most `max` cells, marking the cut with
/// "…" (paths keep their informative tail). Port of `BoxFrame._elide_left`.
pub fn elide_left(text: &str, max: usize) -> Cow<'_, str> {
    if text.width() <= max {
        return Cow::Borrowed(text);
    }
    if max == 0 {
        return Cow::Borrowed("");
    }
    let mut width = 1; // the ellipsis
    let mut start = text.len();
    for (i, g) in text.grapheme_indices(true).rev() {
        let w = g.width();
        if width + w > max {
            break;
        }
        width += w;
        start = i;
    }
    Cow::Owned(format!("…{}", &text[start..]))
}

/// Shorten `text` from the right to at most `max` cells, ending in "…".
pub fn elide_right(text: &str, max: usize) -> Cow<'_, str> {
    if text.width() <= max {
        return Cow::Borrowed(text);
    }
    if max == 0 {
        return Cow::Borrowed("");
    }
    let mut width = 1;
    let mut end = 0;
    for (i, g) in text.grapheme_indices(true) {
        let w = g.width();
        if width + w > max {
            break;
        }
        width += w;
        end = i + g.len();
    }
    Cow::Owned(format!("{}…", &text[..end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Style = Style::fg(Rgba::WHITE);

    #[test]
    fn put_str_writes_cells_and_clips() {
        let mut f = Frame::new(5, 2, Rgba::BLACK);
        assert_eq!(f.put_str(1, 0, "abcdef", S), 5);
        assert_eq!(f.row_text(0), " abcd");
        assert_eq!(f.put_str(0, 5, "x", S), 0, "rows past the grid are ignored");
        assert_eq!(f.put_str_clipped(0, 1, "hello", S, 3), 3);
        assert_eq!(f.row_text(1), "hel  ");
    }

    #[test]
    fn wide_chars_take_two_cells() {
        let mut f = Frame::new(6, 1, Rgba::BLACK);
        assert_eq!(f.put_str(0, 0, "日本x", S), 5);
        assert!(f.cell(0, 0).unwrap().flags.contains(Flags::WIDE));
        assert!(f.cell(1, 0).unwrap().flags.contains(Flags::WIDE_CONT));
        assert_eq!(f.row_text(0), "日本x ");
    }

    #[test]
    fn wide_char_at_right_edge_becomes_space() {
        let mut f = Frame::new(3, 1, Rgba::BLACK);
        assert_eq!(f.put_str(0, 0, "ab日", S), 3);
        assert_eq!(f.row_text(0), "ab ");
        assert!(!f.cell(2, 0).unwrap().flags.contains(Flags::WIDE));
    }

    #[test]
    fn overwriting_half_a_wide_char_blanks_the_other_half() {
        let mut f = Frame::new(4, 1, Rgba::BLACK);
        f.put_str(0, 0, "日", S);
        f.put_char(1, 0, 'x', S);
        assert_eq!(f.row_text(0), " x  ");
        assert!(!f.cell(0, 0).unwrap().flags.contains(Flags::WIDE));
        f.put_str(1, 0, "日", S);
        f.put_char(1, 0, 'y', S);
        assert_eq!(f.row_text(0), " y  ");
        assert!(!f.cell(2, 0).unwrap().flags.contains(Flags::WIDE_CONT));
    }

    #[test]
    fn clusters_keep_the_whole_grapheme() {
        let mut f = Frame::new(4, 1, Rgba::BLACK);
        let end = f.put_str(0, 0, "e\u{301}👍🏽", S);
        assert_eq!(end, 3);
        let c0 = *f.cell(0, 0).unwrap();
        assert!(c0.has_cluster());
        assert_eq!(f.grapheme(&c0), "e\u{301}");
        assert_eq!(f.row_text(0), "e\u{301}👍🏽 ");
    }

    #[test]
    fn control_chars_are_skipped() {
        let mut f = Frame::new(4, 1, Rgba::BLACK);
        assert_eq!(f.put_str(0, 0, "a\tb\u{7}", S), 2);
        assert_eq!(f.row_text(0), "ab  ");
    }

    #[test]
    fn draw_box_uses_box_chars() {
        let mut f = Frame::new(4, 3, Rgba::BLACK);
        f.draw_box(CellRect::new(0, 0, 4, 3), BoxStyle::Rounded, S);
        assert_eq!(f.row_text(0), "╭──╮");
        assert_eq!(f.row_text(1), "│  │");
        assert_eq!(f.row_text(2), "╰──╯");
        f.draw_box(CellRect::new(0, 0, 4, 3), BoxStyle::Double, S);
        assert_eq!(f.row_text(0), "╔══╗");
        assert_eq!(BoxStyle::from_name("heavy"), Some(BoxStyle::Heavy));
        assert_eq!(BoxStyle::from_name("dotted"), None);
    }

    #[test]
    fn set_style_keeps_text_and_wide_flags() {
        let mut f = Frame::new(4, 1, Rgba::BLACK);
        f.put_str(0, 0, "日a", S);
        f.set_style(
            CellRect::new(0, 0, 4, 1),
            Style::new(Rgba::BLACK, Rgba::WHITE).bold(),
        );
        let c = f.cell(0, 0).unwrap();
        assert!(c.flags.contains(Flags::WIDE | Flags::BOLD));
        assert_eq!(c.bg, Rgba::WHITE);
        assert_eq!(f.row_text(0), "日a ");
    }

    #[test]
    fn clear_and_resize_reset() {
        let mut f = Frame::new(2, 2, Rgba::BLACK);
        f.put_str(0, 0, "ab", S);
        f.add_rect(CellRect::new(0, 0, 1, 1), Rgba::WHITE, Layer::BelowText);
        f.resize(3, 1);
        assert_eq!(f.cells().len(), 3);
        assert_eq!(f.row_text(0), "   ");
        assert!(f.rects.is_empty());
    }

    #[test]
    fn elide_left_keeps_the_tail() {
        assert_eq!(elide_left("~/Downloads/photos", 30), "~/Downloads/photos");
        assert_eq!(elide_left("~/Downloads/photos", 8), "…/photos");
        assert_eq!(elide_left("日本語", 4), "…語");
        assert_eq!(elide_left("abc", 0), "");
        assert_eq!(elide_left("abc", 1), "…");
    }

    #[test]
    fn elide_right_keeps_the_head() {
        assert_eq!(elide_right("IMG_4501.JPG", 8), "IMG_450…");
        assert_eq!(elide_right("short", 8), "short");
        assert_eq!(str_width("日本"), 4);
    }

    #[test]
    fn rects_outside_the_grid_are_ignored() {
        // A panel laid out for a wider window than the frame (resize race)
        // must not panic.
        let mut f = Frame::new(5, 2, Rgba::BLACK);
        f.set_bg(CellRect::new(10, 0, 4, 2), Rgba::WHITE);
        f.set_style(CellRect::new(5, 0, 1, 1), S);
        f.fill(CellRect::new(7, 1, 3, 3), 'x', S);
        f.set_bg(CellRect::new(3, 1, u16::MAX, u16::MAX), Rgba::WHITE);
        assert_eq!(f.cell(4, 1).unwrap().bg, Rgba::WHITE);
        assert_eq!(f.cell(2, 1).unwrap().bg, Rgba::TRANSPARENT);
        f.draw_box(CellRect::new(3, 1, 10, 10), BoxStyle::Single, S);
        assert_eq!(f.row_text(1), "   ┌─");
    }

    #[test]
    fn fill_with_wide_chars_steps_by_two() {
        let mut f = Frame::new(5, 1, Rgba::BLACK);
        f.fill(CellRect::new(0, 0, 5, 1), '日', S);
        assert_eq!(f.row_text(0), "日日 ");
        assert!(f.cell(2, 0).unwrap().flags.contains(Flags::WIDE));
        assert!(f.cell(3, 0).unwrap().flags.contains(Flags::WIDE_CONT));
        assert!(!f.cell(4, 0).unwrap().flags.contains(Flags::WIDE));
    }

    #[test]
    fn layout_flags_in_a_style_are_ignored() {
        let mut f = Frame::new(3, 1, Rgba::BLACK);
        let bogus = S.with(Flags::WIDE | Flags::WIDE_CONT | Flags::BOLD);
        f.put_str(0, 0, "ab", bogus);
        let c = f.cell(0, 0).unwrap();
        assert!(c.flags.contains(Flags::BOLD));
        assert!(!c.flags.contains(Flags::WIDE) && !c.flags.contains(Flags::WIDE_CONT));
        assert!(!c.is_blank());
        f.put_str(2, 0, "日", bogus); // doesn't fit: a plain space
        assert_eq!(f.cell(2, 0).unwrap().flags, Flags::BOLD);
    }

    #[test]
    fn rewriting_a_kept_frame_does_not_grow_the_cluster_pool() {
        let mut f = Frame::new(10, 2, Rgba::BLACK);
        for i in 0..20_000 {
            f.put_str(0, 0, "e\u{301}a\u{308}", S);
            f.put_str(0, 1, if i % 2 == 0 { "👍🏽x" } else { "o\u{302}y" }, S);
        }
        assert!(f.clusters.len() <= 4096, "{}", f.clusters.len());
        assert_eq!(f.row_text(0), "e\u{301}a\u{308}        ");
        assert_eq!(
            f.row_text(1),
            "o\u{302}yx       ",
            "the x from the emoji row stays"
        );
        let c = *f.cell(0, 0).unwrap();
        assert_eq!(f.grapheme(&c), "e\u{301}");
    }

    #[test]
    fn elide_handles_combining_and_wide_text() {
        // Never cuts inside a grapheme; the result never exceeds `max` cells.
        for max in 0..12 {
            for text in [
                "héllo wörld",
                "e\u{301}e\u{301}e\u{301}e\u{301}",
                "日本語のファイル名",
                "a日b本c",
            ] {
                let l = elide_left(text, max);
                let r = elide_right(text, max);
                assert!(str_width(&l) <= max, "{text:?} {max} -> {l:?}");
                assert!(str_width(&r) <= max, "{text:?} {max} -> {r:?}");
                if str_width(text) > max && max > 0 {
                    assert!(l.starts_with('…') && r.ends_with('…'));
                }
            }
        }
        assert_eq!(elide_left("e\u{301}e\u{301}e\u{301}", 2), "…e\u{301}");
        assert_eq!(elide_right("日本語", 4), "日…");
    }

    #[test]
    fn px_rect_intersection() {
        let a = PxRect::new(0.0, 0.0, 10.0, 10.0);
        let b = PxRect::new(5.0, 5.0, 10.0, 10.0);
        assert_eq!(a.intersect(&b), PxRect::new(5.0, 5.0, 5.0, 5.0));
        assert!(a.intersect(&PxRect::new(20.0, 0.0, 1.0, 1.0)).is_empty());
    }
}
