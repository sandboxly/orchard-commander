//! The terminal screen: alacritty_terminal's `Term` plus what the file
//! manager needs on top — the shell's reported folder (OSC 7), the full set
//! of xterm mouse modes, the legacy alternate screens, query replies — and a
//! renderer-friendly [`Snapshot`] with colors already resolved to RGB.
//!
//! Nothing here touches a pty: feed it bytes, read it back. [`crate::Session`]
//! wraps one in a mutex and feeds it from the shell's output.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color, CursorShape as AnsiCursorShape, Handler, NamedColor, Processor, StdSyncHandler,
};

use crate::keys::{self, KeyOptions};
use crate::mouse::{MouseModes, MouseTracking};
use crate::scan::{Intercept, Scanner};
use crate::shell::path_from_osc7;

/// Lines kept above the screen.
pub const SCROLLBACK: usize = 5000;

/// Characters that count as part of a word for double-click selection:
/// paths and URLs select as one word.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || "-_./~:@%+=#".contains(c)
}

// ---------------------------------------------------------------------------
// Colors
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// From `0xRRGGBB`.
    pub const fn hex(v: u32) -> Self {
        Self::new((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }
}

impl From<alacritty_terminal::vte::ansi::Rgb> for Rgb {
    fn from(c: alacritty_terminal::vte::ansi::Rgb) -> Self {
        Self::new(c.r, c.g, c.b)
    }
}

/// xterm's 16 colors (black, red, green, yellow, blue, magenta, cyan, white,
/// then the bright ones), softened a little to read well on dark themes.
pub const DARK_ANSI: [Rgb; 16] = [
    Rgb::hex(0x3b3b3b),
    Rgb::hex(0xe06c75),
    Rgb::hex(0x98c379),
    Rgb::hex(0xe5c07b),
    Rgb::hex(0x61afef),
    Rgb::hex(0xc678dd),
    Rgb::hex(0x56b6c2),
    Rgb::hex(0xd0d0d0),
    Rgb::hex(0x7f848e),
    Rgb::hex(0xff7b86),
    Rgb::hex(0xb5e890),
    Rgb::hex(0xffd68a),
    Rgb::hex(0x80c4ff),
    Rgb::hex(0xde9cf0),
    Rgb::hex(0x7fd8e3),
    Rgb::hex(0xffffff),
];

/// The 16 colors for light themes.
pub const LIGHT_ANSI: [Rgb; 16] = [
    Rgb::hex(0x000000),
    Rgb::hex(0xc4314b),
    Rgb::hex(0x3a7d1c),
    Rgb::hex(0x986801),
    Rgb::hex(0x2763c4),
    Rgb::hex(0xa626a4),
    Rgb::hex(0x0e7f8a),
    Rgb::hex(0xa0a1a7),
    Rgb::hex(0x696c77),
    Rgb::hex(0xe45649),
    Rgb::hex(0x50a14f),
    Rgb::hex(0xc18401),
    Rgb::hex(0x4078f2),
    Rgb::hex(0xc33fc1),
    Rgb::hex(0x0997b3),
    Rgb::hex(0x383a42),
];

/// The colors a snapshot resolves to: the theme's text and background for
/// "default", and the 16 ANSI colors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub foreground: Rgb,
    pub background: Rgb,
    pub ansi: [Rgb; 16],
}

impl Palette {
    /// The theme's text/background with [`DARK_ANSI`] or [`LIGHT_ANSI`].
    pub fn for_theme(foreground: Rgb, background: Rgb, dark: bool) -> Self {
        Self {
            foreground,
            background,
            ansi: if dark { DARK_ANSI } else { LIGHT_ANSI },
        }
    }

    /// One of the 256 xterm colors: the palette's 16, then the 6×6×6 cube,
    /// then 24 grays.
    pub fn indexed(&self, index: u8) -> Rgb {
        let i = index as usize;
        if i < 16 {
            return self.ansi[i];
        }
        if i < 232 {
            let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
            let v = i - 16;
            return Rgb::new(level(v / 36), level((v / 6) % 6), level(v % 6));
        }
        let gray = (8 + (i - 232) * 10) as u8;
        Rgb::new(gray, gray, gray)
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self::for_theme(Rgb::hex(0xd0d0d0), Rgb::hex(0x1e1e1e), true)
    }
}

/// `color` as RGB. Programs may have redefined palette entries (OSC 4/10/11):
/// those win over the theme.
fn resolve(color: Color, palette: &Palette, overrides: &Colors, bold_fg: bool) -> Rgb {
    match color {
        Color::Spec(rgb) => rgb.into(),
        Color::Indexed(i) => overrides[i as usize].map_or_else(|| palette.indexed(i), Rgb::from),
        Color::Named(named) => {
            let index = named as usize;
            if index < 16 {
                // Bold text in one of the 8 basic colors shows in its bright variant.
                let index = if bold_fg && index < 8 {
                    index + 8
                } else {
                    index
                };
                return overrides[index].map_or(palette.ansi[index], Rgb::from);
            }
            match named {
                NamedColor::Background => {
                    overrides[NamedColor::Background].map_or(palette.background, Rgb::from)
                }
                NamedColor::DimBlack
                | NamedColor::DimRed
                | NamedColor::DimGreen
                | NamedColor::DimYellow
                | NamedColor::DimBlue
                | NamedColor::DimMagenta
                | NamedColor::DimCyan
                | NamedColor::DimWhite => {
                    let index = named as usize - NamedColor::DimBlack as usize;
                    overrides[index].map_or(palette.ansi[index], Rgb::from)
                }
                // Foreground, its bright/dim variants, the cursor
                _ => overrides[NamedColor::Foreground].map_or(palette.foreground, Rgb::from),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

bitflags::bitflags! {
    /// How a cell is drawn.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct CellFlags: u16 {
        const BOLD = 1;
        const ITALIC = 1 << 1;
        /// Any underline style.
        const UNDERLINE = 1 << 2;
        /// Swap fg and bg (the renderer does it: selection flips it back).
        const INVERSE = 1 << 3;
        const DIM = 1 << 4;
        const STRIKEOUT = 1 << 5;
        /// Concealed text (SGR 8): draw the background only.
        const HIDDEN = 1 << 6;
        /// A double-width character: it covers this cell and the next.
        const WIDE = 1 << 7;
        /// The second half of a wide character: nothing to draw.
        const WIDE_SPACER = 1 << 8;
        /// Inside the selection.
        const SELECTED = 1 << 9;
        /// Has combining characters (see [`Snapshot::combining`]).
        const COMBINING = 1 << 10;
    }
}

/// One cell, ready to draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Rgb,
    pub bg: Rgb,
    pub flags: CellFlags,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
    HollowBlock,
}

/// Where the cursor is drawn, in viewport cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub line: usize,
    pub column: usize,
    pub shape: CursorShape,
    pub blinking: bool,
    /// On a double-width character: draw it two cells wide.
    pub wide: bool,
}

/// The visible part of the screen at one moment, copied out under the lock
/// so drawing doesn't hold it.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub columns: usize,
    pub lines: usize,
    /// `lines × columns` cells, row by row.
    pub cells: Vec<Cell>,
    /// Combining characters by cell index (ascending), for cells flagged
    /// [`CellFlags::COMBINING`].
    pub combining: Vec<(usize, char)>,
    /// `None` when hidden or scrolled out of view.
    pub cursor: Option<Cursor>,
    /// Lines scrolled back into the history (0 = at the bottom).
    pub display_offset: usize,
    pub history_size: usize,
    /// A full-screen program (vim, less) is on the alternate screen.
    pub alternate: bool,
    /// A program takes the mouse: show an arrow pointer instead of an I-beam.
    pub mouse_tracking: Option<MouseTracking>,
}

impl Snapshot {
    pub fn row(&self, line: usize) -> &[Cell] {
        &self.cells[line * self.columns..(line + 1) * self.columns]
    }

    /// The combining characters on the cell at `index`.
    pub fn combining_at(&self, index: usize) -> impl Iterator<Item = char> + '_ {
        let start = self.combining.partition_point(|&(i, _)| i < index);
        self.combining[start..]
            .iter()
            .take_while(move |&&(i, _)| i == index)
            .map(|&(_, c)| c)
    }

    /// A row as text, trailing blanks removed (wide spacers skipped).
    pub fn row_text(&self, line: usize) -> String {
        let mut s = String::with_capacity(self.columns);
        for (x, cell) in self.row(line).iter().enumerate() {
            if cell.flags.contains(CellFlags::WIDE_SPACER) {
                continue;
            }
            s.push(cell.ch);
            if cell.flags.contains(CellFlags::COMBINING) {
                s.extend(self.combining_at(line * self.columns + x));
            }
        }
        s.truncate(s.trim_end().len());
        s
    }
}

// ---------------------------------------------------------------------------
// Event proxy
// ---------------------------------------------------------------------------

/// What `Term` reports while parsing: replies for the program, title, bell.
#[derive(Default)]
struct ProxyState {
    replies: Vec<u8>,
    title: Option<String>,
    bell: bool,
}

#[derive(Clone, Default)]
struct Proxy(Arc<Mutex<ProxyState>>);

impl Proxy {
    fn with<R>(&self, f: impl FnOnce(&mut ProxyState) -> R) -> R {
        f(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl EventListener for Proxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => self.with(|s| s.replies.extend_from_slice(text.as_bytes())),
            Event::Title(title) => self.with(|s| s.title = Some(title)),
            Event::ResetTitle => self.with(|s| s.title = None),
            Event::Bell => self.with(|s| s.bell = true),
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
struct Size {
    columns: usize,
    lines: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

// ---------------------------------------------------------------------------
// Screen
// ---------------------------------------------------------------------------

/// What a double/triple click selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionKind {
    /// From cell boundary to cell boundary (drag).
    Simple,
    /// The word — or path, or URL — under the cell.
    Word,
    /// Whole lines.
    Line,
}

pub struct Screen {
    term: Term<Proxy>,
    parser: Processor<StdSyncHandler>,
    proxy: Proxy,
    scanner: Scanner,
    intercepts: Vec<Intercept>,
    mouse: MouseModes,
    reported_cwd: Option<PathBuf>,
    prompts: u64,
}

impl Screen {
    /// A blank screen; sizes below 2 are raised to 2.
    pub fn new(columns: usize, lines: usize) -> Self {
        let size = Size {
            columns: columns.max(2),
            lines: lines.max(2),
        };
        let proxy = Proxy::default();
        let config = Config {
            scrolling_history: SCROLLBACK,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, proxy.clone()),
            parser: Processor::new(),
            proxy,
            scanner: Scanner::default(),
            intercepts: Vec::new(),
            mouse: MouseModes::NONE,
            reported_cwd: None,
            prompts: 0,
        }
    }

    /// Process output (from the program, or a message of our own).
    pub fn feed(&mut self, data: &[u8]) {
        let mut events = std::mem::take(&mut self.intercepts);
        events.clear();
        self.scanner.scan(data, &mut events);
        let mut start = 0;
        for event in &events {
            match *event {
                Intercept::PrivateMode { mode, set, end } => {
                    if let Some(flag) = MouseModes::from_private_mode(mode) {
                        self.mouse.set(flag, set);
                    } else if mode == 47 || mode == 1047 {
                        // Legacy alternate screens (alacritty knows only 1049): switch at
                        // exactly this point of the stream.
                        self.parser.advance(&mut self.term, &data[start..end]);
                        start = end;
                        self.switch_screen(set);
                    }
                }
                Intercept::Osc7 { ref url, .. } => {
                    if let Some(path) = path_from_osc7(url) {
                        self.reported_cwd = Some(path);
                    }
                    self.prompts += 1; // a prompt was drawn
                }
                Intercept::Reset { .. } => self.mouse = MouseModes::NONE,
            }
        }
        self.parser.advance(&mut self.term, &data[start..]);
        self.intercepts = events;
        self.flush_sync();
    }

    fn switch_screen(&mut self, alternate: bool) {
        let on = self.alternate();
        if alternate && !on {
            self.term.save_cursor_position();
            self.term.swap_alt();
        } else if !alternate && on {
            self.term.swap_alt();
            self.term.restore_cursor_position();
        }
    }

    /// Apply output a program held back in a synchronized update (`CSI ? 2026 h`)
    /// that never ended, once its timeout passed. Returns whether anything
    /// changed. [`crate::Session`] does this itself; standalone screens need a timer.
    pub fn flush_sync(&mut self) -> bool {
        match self.parser.sync_timeout().sync_timeout() {
            Some(deadline) if Instant::now() >= deadline => {
                self.parser.stop_sync(&mut self.term);
                true
            }
            _ => false,
        }
    }

    /// When output held back by an unfinished synchronized update is due to
    /// be shown ([`Screen::flush_sync`]); `None` when nothing is held back.
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    /// Replies to the program's queries (cursor position, device attributes)
    /// since the last call; the session writes them back to the pty.
    pub fn take_replies(&mut self) -> Vec<u8> {
        self.proxy.with(|s| std::mem::take(&mut s.replies))
    }

    /// The title the program set (OSC 0/2), if any.
    pub fn title(&self) -> Option<String> {
        self.proxy.with(|s| s.title.clone())
    }

    /// Whether the bell rang since the last call.
    pub fn take_bell(&mut self) -> bool {
        self.proxy.with(|s| std::mem::take(&mut s.bell))
    }

    /// The folder the shell last reported (OSC 7).
    pub fn reported_cwd(&self) -> Option<&Path> {
        self.reported_cwd.as_deref()
    }

    /// How many OSC 7 reports (≈ prompts) arrived so far.
    pub fn prompts(&self) -> u64 {
        self.prompts
    }

    pub fn columns(&self) -> usize {
        self.term.columns()
    }

    pub fn lines(&self) -> usize {
        self.term.screen_lines()
    }

    /// Resize (reflowing wrapped lines); sizes below 2 are raised to 2.
    /// Returns whether the size changed.
    pub fn resize(&mut self, columns: usize, lines: usize) -> bool {
        let size = Size {
            columns: columns.max(2),
            lines: lines.max(2),
        };
        if (size.columns, size.lines) == (self.columns(), self.lines()) {
            return false;
        }
        self.term.resize(size);
        true
    }

    fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    pub fn mouse_modes(&self) -> MouseModes {
        self.mouse
    }

    pub fn mouse_tracking(&self) -> Option<MouseTracking> {
        self.mouse.tracking()
    }

    /// DECCKM: arrows send `ESC O A`.
    pub fn app_cursor(&self) -> bool {
        self.mode().contains(TermMode::APP_CURSOR)
    }

    pub fn bracketed_paste(&self) -> bool {
        self.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// The program wants focus in/out reports.
    pub fn focus_events(&self) -> bool {
        self.mode().contains(TermMode::FOCUS_IN_OUT)
    }

    /// A full-screen program is on the alternate screen.
    pub fn alternate(&self) -> bool {
        self.mode().contains(TermMode::ALT_SCREEN)
    }

    pub fn cursor_visible(&self) -> bool {
        self.mode().contains(TermMode::SHOW_CURSOR)
    }

    /// Key encoding options for the current modes (platform Alt convention).
    pub fn key_options(&self) -> KeyOptions {
        KeyOptions::for_platform(self.app_cursor())
    }

    /// Pasted text as it should be sent now.
    pub fn paste_bytes(&self, text: &str) -> String {
        keys::encode_paste(text, self.bracketed_paste())
    }

    /// The focus report to send, if the program asked for them.
    pub fn focus_report(&self, focused: bool) -> Option<&'static [u8]> {
        self.focus_events().then(|| keys::encode_focus(focused))
    }

    // -- scrollback ---------------------------------------------------------

    pub fn history_size(&self) -> usize {
        self.term.history_size()
    }

    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Scroll the view: positive = back into the history. Clamped.
    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// What typing does to the view: back to the bottom, selection gone.
    pub fn reset_view(&mut self) {
        self.scroll_to_bottom();
        self.clear_selection();
    }

    /// A line's text, trailing blanks removed. `line` counts from the top of
    /// the screen (not the viewport); negative values are history lines.
    pub fn line_text(&self, line: i32) -> String {
        let grid = self.term.grid();
        if line < -(self.history_size() as i32) || line >= self.lines() as i32 {
            return String::new();
        }
        let row = &grid[Line(line)];
        let mut s = String::with_capacity(self.columns());
        for x in 0..self.columns() {
            let cell = &row[Column(x)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            s.push(cell.c);
            if let Some(extra) = cell.zerowidth() {
                s.extend(extra);
            }
        }
        s.truncate(s.trim_end().len());
        s
    }

    /// The visible lines (at the current scroll position), joined by `\n`.
    pub fn text(&self) -> String {
        let offset = self.display_offset() as i32;
        (0..self.lines() as i32)
            .map(|y| self.line_text(y - offset))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The history lines, oldest first.
    pub fn history_text(&self) -> impl Iterator<Item = String> + '_ {
        (1..=self.history_size() as i32)
            .rev()
            .map(|n| self.line_text(-n))
    }

    // -- selection ------------------------------------------------------------

    /// A viewport cell → a grid point and side. `column` is a cell boundary
    /// (0…columns): the selection runs from boundary to boundary.
    fn boundary(&self, line: usize, column: usize) -> (Point, Side) {
        let line = Line(line.min(self.lines() - 1) as i32 - self.display_offset() as i32);
        if column >= self.columns() {
            (Point::new(line, Column(self.columns() - 1)), Side::Right)
        } else {
            (Point::new(line, Column(column)), Side::Left)
        }
    }

    /// Start a selection at a viewport cell.
    pub fn select(&mut self, kind: SelectionKind, line: usize, column: usize) {
        let (point, side) = self.boundary(line, column);
        self.term.selection = Some(match kind {
            SelectionKind::Simple => Selection::new(SelectionType::Simple, point, side),
            SelectionKind::Line => Selection::new(SelectionType::Lines, point, Side::Left),
            SelectionKind::Word => {
                let (start, end) = self.word_at(point);
                let mut sel = Selection::new(
                    SelectionType::Simple,
                    Point::new(point.line, start),
                    Side::Left,
                );
                sel.update(Point::new(point.line, end), Side::Right);
                sel
            }
        });
    }

    /// Columns of the word (or path) at `point`, inclusive; just the cell
    /// itself when it isn't part of one.
    fn word_at(&self, point: Point) -> (Column, Column) {
        let row = &self.term.grid()[point.line];
        let columns = self.columns();
        let word = |x: usize| {
            let cell = &row[Column(x)];
            !cell.flags.contains(Flags::WIDE_CHAR_SPACER) && is_word_char(cell.c)
                || cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                    && x > 0
                    && is_word_char(row[Column(x - 1)].c)
        };
        let col = point.column.0.min(columns - 1);
        if !word(col) {
            return (Column(col), Column(col));
        }
        let (mut start, mut end) = (col, col);
        while start > 0 && word(start - 1) {
            start -= 1;
        }
        while end + 1 < columns && word(end + 1) {
            end += 1;
        }
        (Column(start), Column(end))
    }

    /// Move the selection's free end to a viewport cell boundary.
    pub fn extend_selection(&mut self, line: usize, column: usize) {
        let (point, side) = self.boundary(line, column);
        if let Some(sel) = self.term.selection.as_mut() {
            sel.update(point, side);
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// Whether something (non-empty) is selected.
    pub fn has_selection(&self) -> bool {
        self.term.selection.as_ref().is_some_and(|s| !s.is_empty())
    }

    /// The selected text: each line's trailing blanks trimmed, wrapped lines joined.
    pub fn selected_text(&self) -> String {
        if !self.has_selection() {
            return String::new();
        }
        let text = self.term.selection_to_string().unwrap_or_default();
        text.split('\n')
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
    }

    // -- rendering ---------------------------------------------------------------

    pub fn snapshot(&self, palette: &Palette) -> Snapshot {
        let mut snapshot = Snapshot::default();
        self.snapshot_into(palette, &mut snapshot);
        snapshot
    }

    /// Fill `out` (reusing its buffers) with what is visible now.
    pub fn snapshot_into(&self, palette: &Palette, out: &mut Snapshot) {
        let (columns, lines) = (self.columns(), self.lines());
        let offset = self.display_offset();
        let grid = self.term.grid();
        let overrides = self.term.colors();
        let selection = self
            .term
            .selection
            .as_ref()
            .and_then(|s| s.to_range(&self.term));
        out.columns = columns;
        out.lines = lines;
        out.cells.clear();
        out.cells.reserve(columns * lines);
        out.combining.clear();
        for y in 0..lines {
            let line = Line(y as i32 - offset as i32);
            let row = &grid[line];
            for x in 0..columns {
                let cell = &row[Column(x)];
                let f = cell.flags;
                let bold = f.contains(Flags::BOLD);
                let mut flags = CellFlags::empty();
                flags.set(CellFlags::BOLD, bold);
                flags.set(CellFlags::ITALIC, f.contains(Flags::ITALIC));
                flags.set(CellFlags::UNDERLINE, f.intersects(Flags::ALL_UNDERLINES));
                flags.set(CellFlags::INVERSE, f.contains(Flags::INVERSE));
                flags.set(CellFlags::DIM, f.contains(Flags::DIM));
                flags.set(CellFlags::STRIKEOUT, f.contains(Flags::STRIKEOUT));
                flags.set(CellFlags::HIDDEN, f.contains(Flags::HIDDEN));
                flags.set(CellFlags::WIDE, f.contains(Flags::WIDE_CHAR));
                flags.set(CellFlags::WIDE_SPACER, f.contains(Flags::WIDE_CHAR_SPACER));
                if selection
                    .as_ref()
                    .is_some_and(|s| s.contains(Point::new(line, Column(x))))
                {
                    flags |= CellFlags::SELECTED;
                }
                if let Some(extra) = cell.zerowidth() {
                    flags |= CellFlags::COMBINING;
                    let index = out.cells.len();
                    out.combining.extend(extra.iter().map(|&c| (index, c)));
                }
                out.cells.push(Cell {
                    ch: cell.c,
                    fg: resolve(cell.fg, palette, overrides, bold),
                    bg: resolve(cell.bg, palette, overrides, false),
                    flags,
                });
            }
        }
        let content = self.term.renderable_content();
        let cursor = content.cursor;
        let viewport_line = cursor.point.line.0 + offset as i32;
        out.cursor = match cursor.shape {
            AnsiCursorShape::Hidden => None,
            _ if !(0..lines as i32).contains(&viewport_line) => None,
            shape => Some(Cursor {
                line: viewport_line as usize,
                column: cursor.point.column.0,
                shape: match shape {
                    AnsiCursorShape::Underline => CursorShape::Underline,
                    AnsiCursorShape::Beam => CursorShape::Beam,
                    AnsiCursorShape::HollowBlock => CursorShape::HollowBlock,
                    _ => CursorShape::Block,
                },
                blinking: self.term.cursor_style().blinking,
                wide: grid[cursor.point].flags.contains(Flags::WIDE_CHAR),
            }),
        };
        out.display_offset = offset;
        out.history_size = self.history_size();
        out.alternate = self.alternate();
        out.mouse_tracking = self.mouse_tracking();
    }
}

impl std::fmt::Debug for Screen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Screen")
            .field("columns", &self.columns())
            .field("lines", &self.lines())
            .field("mouse", &self.mouse)
            .field("reported_cwd", &self.reported_cwd)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(columns: usize, lines: usize, data: &str) -> Screen {
        let mut s = Screen::new(columns, lines);
        s.feed(data.as_bytes());
        s
    }

    #[test]
    fn replies_to_cursor_position_queries() {
        let mut s = screen(20, 5, "ab\x1b[6n");
        assert_eq!(s.take_replies(), b"\x1b[1;3R");
        assert!(s.take_replies().is_empty());
        s.feed(b"\x1b[c"); // primary device attributes
        assert!(s.take_replies().starts_with(b"\x1b[?"));
    }

    #[test]
    fn alternate_screens_restore_the_shell_output() {
        for mode in [47, 1047, 1049] {
            let mut s = screen(20, 5, "before\r\n");
            s.feed(format!("\x1b[?{mode}hFULL2").as_bytes());
            assert!(s.alternate(), "{mode}");
            assert!(
                s.text().contains("FULL2") && !s.text().contains("before"),
                "{mode}"
            );
            s.feed(format!("\x1b[?{mode}lafter").as_bytes());
            assert!(!s.alternate(), "{mode}");
            assert_eq!(s.line_text(0), "before", "{mode}");
            assert_eq!(s.line_text(1), "after", "{mode}"); // the cursor came back too
            assert!(!s.text().contains("FULL2"));
        }
    }

    #[test]
    fn alternate_screen_has_no_scrollback() {
        let mut s = screen(10, 3, "\x1b[?1049h");
        for i in 0..10 {
            s.feed(format!("x{i}\r\n").as_bytes());
        }
        assert_eq!(s.history_size(), 0);
    }

    #[test]
    fn scrollback_and_scrolling() {
        let mut s = Screen::new(20, 5);
        for i in 0..40 {
            s.feed(format!("line{i}\r\n").as_bytes());
        }
        assert!(s.history_text().any(|h| h.starts_with("line0")));
        assert_eq!(s.history_text().next().unwrap(), "line0");
        assert_eq!(s.history_size(), 36);
        s.scroll(3);
        assert_eq!(s.display_offset(), 3);
        assert!(s.text().starts_with("line33"));
        s.scroll(1000);
        assert_eq!(s.display_offset(), 36);
        assert!(s.text().starts_with("line0\n"));
        s.scroll_to_bottom();
        assert_eq!(s.display_offset(), 0);
    }

    #[test]
    fn resize_reflows_and_clamps() {
        let mut s = screen(10, 4, "0123456789abcdef");
        assert_eq!(s.line_text(0), "0123456789");
        assert!(s.resize(20, 4));
        assert_eq!(s.line_text(0), "0123456789abcdef"); // reflowed: pyte couldn't
        assert!(!s.resize(20, 4));
        assert!(s.resize(1, 0));
        assert_eq!((s.columns(), s.lines()), (2, 2));
    }

    #[test]
    fn osc7_and_mouse_modes() {
        let mut s = screen(20, 5, "\x1b]7;file://host/tmp/a%20b\x07$ ");
        assert_eq!(s.reported_cwd(), Some(Path::new("/tmp/a b")));
        assert_eq!(s.prompts(), 1);
        assert_eq!(s.line_text(0), "$"); // the OSC itself isn't shown
        s.feed(b"\x1b[?1002h\x1b[?1006h");
        assert_eq!(s.mouse_tracking(), Some(MouseTracking::Drags));
        assert!(s.mouse_modes().contains(MouseModes::SGR));
        s.feed(b"\x1b[?9h\x1b[?1015h\x1b[?1002l");
        assert_eq!(s.mouse_tracking(), Some(MouseTracking::X10));
        assert!(s.mouse_modes().contains(MouseModes::URXVT));
        s.feed(b"\x1bc");
        assert_eq!(s.mouse_modes(), MouseModes::NONE);
        s.feed(b"\x1b]7;bogus\x07");
        assert_eq!(s.reported_cwd(), Some(Path::new("/tmp/a b")));
    }

    #[test]
    fn modes() {
        let mut s = Screen::new(10, 3);
        assert!(!s.app_cursor() && !s.bracketed_paste() && !s.focus_events());
        s.feed(b"\x1b[?1h\x1b[?2004h\x1b[?1004h");
        assert!(s.app_cursor() && s.bracketed_paste() && s.focus_events());
        assert!(s.key_options().app_cursor);
        assert_eq!(s.paste_bytes("a\nb"), "\x1b[200~a\rb\x1b[201~");
        assert_eq!(s.focus_report(false), Some(&b"\x1b[O"[..]));
        s.feed(b"\x1b[?1004l\x1b]2;my title\x07\x07");
        assert_eq!(s.focus_report(true), None);
        assert_eq!(s.title().as_deref(), Some("my title"));
        assert!(s.take_bell() && !s.take_bell());
    }

    #[test]
    fn snapshot_resolves_colors() {
        let palette = Palette::for_theme(Rgb::hex(0x111111), Rgb::hex(0x222222), true);
        let s = screen(
            10,
            3,
            "\x1b[31mr\x1b[1mR\x1b[0m\x1b[38;5;196mi\x1b[38;2;1;2;3mt\x1b[0m\x1b[7;4mu\x1b[0m\x1b[48;5;244mg",
        );
        let snap = s.snapshot(&palette);
        let row = snap.row(0);
        assert_eq!(row[0].fg, DARK_ANSI[1]);
        assert_eq!(row[1].fg, DARK_ANSI[9]); // bold red shows bright
        assert!(row[1].flags.contains(CellFlags::BOLD));
        assert_eq!(row[2].fg, Rgb::hex(0xff0000));
        assert_eq!(row[3].fg, Rgb::new(1, 2, 3));
        assert_eq!(row[4].fg, Rgb::hex(0x111111));
        assert_eq!(row[4].bg, Rgb::hex(0x222222));
        assert!(
            row[4]
                .flags
                .contains(CellFlags::INVERSE | CellFlags::UNDERLINE)
        );
        assert_eq!(row[5].bg, Rgb::new(128, 128, 128));
        assert_eq!(snap.row_text(0), "rRitug");
        let cursor = snap.cursor.unwrap();
        assert_eq!(
            (cursor.line, cursor.column, cursor.shape),
            (0, 6, CursorShape::Block)
        );
    }

    #[test]
    fn snapshot_cursor_shapes_and_overrides() {
        let mut s = screen(10, 3, "\x1b[5 q\x1b]4;1;rgb:ff/00/ff\x07\x1b[31mx");
        let snap = s.snapshot(&Palette::default());
        assert_eq!(snap.cursor.unwrap().shape, CursorShape::Beam);
        assert!(snap.cursor.unwrap().blinking);
        assert_eq!(snap.row(0)[0].fg, Rgb::hex(0xff00ff)); // the program redefined red
        s.feed(b"\x1b[?25l");
        assert_eq!(s.snapshot(&Palette::default()).cursor, None);
    }

    #[test]
    fn wide_and_combining_characters() {
        let s = screen(10, 3, "日x e\u{301}");
        let snap = s.snapshot(&Palette::default());
        let row = snap.row(0);
        assert!(row[0].flags.contains(CellFlags::WIDE) && row[0].ch == '日');
        assert!(row[1].flags.contains(CellFlags::WIDE_SPACER));
        assert!(row[4].flags.contains(CellFlags::COMBINING));
        assert_eq!(snap.combining_at(4).collect::<String>(), "\u{301}");
        assert_eq!(snap.row_text(0), "日x e\u{301}");
        assert_eq!(s.line_text(0), "日x e\u{301}");
    }

    #[test]
    fn snapshot_follows_the_scroll_position() {
        let mut s = Screen::new(10, 3);
        for i in 0..10 {
            s.feed(format!("l{i}\r\n").as_bytes());
        }
        s.scroll(2);
        let snap = s.snapshot(&Palette::default());
        assert_eq!(snap.display_offset, 2);
        assert_eq!(snap.row_text(0), "l6");
        assert_eq!(snap.cursor, None); // scrolled out of view
        let mut reused = snap.clone();
        s.scroll_to_bottom();
        s.snapshot_into(&Palette::default(), &mut reused);
        assert_eq!(reused.row_text(0), "l8");
        assert_eq!(reused.cells.len(), 30);
    }

    #[test]
    fn word_line_and_drag_selection() {
        let mut s = screen(40, 4, "\x1b[2J\x1b[Hls ~/src/project.txt done\r\n");
        s.select(SelectionKind::Word, 0, 8);
        assert_eq!(s.selected_text(), "~/src/project.txt"); // a path counts as one word
        s.select(SelectionKind::Line, 0, 1);
        assert_eq!(s.selected_text(), "ls ~/src/project.txt done\n"); // as Python: the line and its break
        s.select(SelectionKind::Simple, 0, 3);
        assert!(!s.has_selection());
        s.extend_selection(0, 8);
        assert_eq!(s.selected_text(), "~/src");
        s.extend_selection(0, 1000); // to the end of the line
        assert_eq!(s.selected_text(), "~/src/project.txt done");
        s.select(SelectionKind::Word, 0, 2); // a blank: just that cell
        assert_eq!(s.selected_text(), "");
        assert!(s.has_selection());
        s.reset_view();
        assert!(!s.has_selection() && s.selected_text().is_empty());
        let snap = s.snapshot(&Palette::default());
        assert!(
            !snap
                .cells
                .iter()
                .any(|c| c.flags.contains(CellFlags::SELECTED))
        );
    }

    #[test]
    fn selection_shows_in_the_snapshot() {
        let mut s = screen(10, 2, "hello");
        s.select(SelectionKind::Simple, 0, 1);
        s.extend_selection(0, 3);
        let snap = s.snapshot(&Palette::default());
        let selected: Vec<bool> = snap.row(0)[..5]
            .iter()
            .map(|c| c.flags.contains(CellFlags::SELECTED))
            .collect();
        assert_eq!(selected, [false, true, true, false, false]);
    }

    #[test]
    fn unfinished_synchronized_update_is_flushed_after_its_timeout() {
        let mut s = screen(10, 3, "\x1b[?2026hheld");
        assert!(!s.text().contains("held"));
        assert!(s.sync_deadline().is_some());
        assert!(!s.flush_sync()); // not due yet
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(s.flush_sync());
        assert_eq!(s.line_text(0), "held");
        assert_eq!(s.sync_deadline(), None);
        s.feed(b"\x1b[?2026h\r\nX\x1b[?2026l"); // ended properly: shown at once
        assert_eq!(s.line_text(1), "X");
        assert_eq!(s.sync_deadline(), None);
    }

    #[test]
    fn palette_cube_and_grays() {
        let p = Palette::default();
        assert_eq!(p.indexed(16), Rgb::new(0, 0, 0));
        assert_eq!(p.indexed(231), Rgb::new(255, 255, 255));
        assert_eq!(p.indexed(67), Rgb::new(95, 135, 175));
        assert_eq!(p.indexed(232), Rgb::new(8, 8, 8));
        assert_eq!(p.indexed(255), Rgb::new(238, 238, 238));
        assert_eq!(p.indexed(3), DARK_ANSI[3]);
        assert_eq!(
            Palette::for_theme(Rgb::default(), Rgb::default(), false).ansi,
            LIGHT_ANSI
        );
    }
}
