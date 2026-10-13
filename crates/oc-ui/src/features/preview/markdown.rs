//! Rendered Markdown (1.x showed it in a `QTextBrowser`): headings, wrapped
//! paragraphs, lists, quotes, highlighted code blocks, rules and tables,
//! laid out for the pane's width. Links open in the default app on click.
//!
//! The layout depends only on the width; it is computed when the width
//! changes and cached, and drawing paints only the visible rows.

use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;

use oc_grid::{Flags, Frame};
use oc_preview::{Align, Color, MarkdownData, MdBlock, MdBlockKind, StyledLine, TextStyle, ThemeRole};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::text::{Painter, clamp_top, code_background, paint, scroll_event};
use super::views::{ContentView, Handle};
use crate::app::{App, Outcome};
use crate::geom::Rect;
use crate::input::{Event, MouseButton, MouseKind};
use crate::widgets::{DrawCtx, Scrollbar, fill};

/// A piece of a laid-out row: text, style, link target.
#[derive(Clone, Debug, PartialEq)]
pub struct Piece {
    pub text: String,
    pub style: TextStyle,
    pub link: Option<Box<str>>,
}

/// How a whole row looks underneath its pieces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    Text,
    /// Bold, in the accent color (levels 1–2) or the text color.
    Heading(u8),
    /// The row's background is the code background.
    Code,
}

/// One row of the laid-out document.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub kind: RowKind,
    /// Cells of the row's indentation and quote bars (part of `pieces`)
    /// where the code background starts.
    pub fill_from: usize,
    pub pieces: Vec<Piece>,
}

impl Row {
    fn new(kind: RowKind) -> Row {
        Row { kind, fill_from: 0, pieces: Vec::new() }
    }

    fn push(&mut self, text: &str, style: TextStyle, link: Option<&str>) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.pieces.last_mut()
            && last.style == style
            && last.link.as_deref() == link
        {
            last.text.push_str(text);
            return;
        }
        self.pieces.push(Piece { text: text.to_owned(), style, link: link.map(Into::into) });
    }

    pub fn text(&self) -> String {
        self.pieces.iter().map(|p| p.text.as_str()).collect()
    }
}

fn border() -> TextStyle {
    TextStyle::fg(Color::Theme(ThemeRole::Border))
}

/// Lay out `data` for a pane `width` cells wide.
pub fn layout(data: &MarkdownData, width: usize) -> Vec<Row> {
    let width = width.max(8);
    let mut out: Vec<Row> = Vec::new();
    let mut prev: Option<&MdBlock> = None;
    for b in &data.blocks {
        let tight = matches!((prev.map(|p| &p.kind), &b.kind), (Some(MdBlockKind::ListItem { .. }), MdBlockKind::ListItem { .. }));
        if prev.is_some() && !tight {
            out.push(quote_row(b.quote_depth.min(prev.map_or(0, |p| p.quote_depth)), RowKind::Text));
        }
        prev = Some(b);
        let q = b.quote_depth as usize * 2;
        // Lists: the marker one cell in per level, the text after it.
        let list_indent = if b.list_depth > 0 { 2 * (b.list_depth as usize - 1) + 1 } else { 0 };
        match &b.kind {
            MdBlockKind::Heading { level, line } => {
                let kind = RowKind::Heading(*level);
                for pieces in wrap(line, width.saturating_sub(q)) {
                    let mut row = quote_row(b.quote_depth, kind);
                    row.pieces.extend(pieces);
                    out.push(row);
                }
            }
            MdBlockKind::Paragraph(line) => {
                let indent = q + if b.list_depth > 0 { list_indent + 2 } else { 0 };
                for pieces in wrap(line, width.saturating_sub(indent)) {
                    let mut row = quote_row(b.quote_depth, RowKind::Text);
                    row.push(&" ".repeat(indent - q), TextStyle::default(), None);
                    row.pieces.extend(pieces);
                    out.push(row);
                }
            }
            MdBlockKind::ListItem { marker, line } => {
                let lead = list_indent + marker.width() + 1;
                for (i, pieces) in wrap(line, width.saturating_sub(q + lead)).into_iter().enumerate() {
                    let mut row = quote_row(b.quote_depth, RowKind::Text);
                    if i == 0 {
                        row.push(&" ".repeat(list_indent), TextStyle::default(), None);
                        row.push(marker, TextStyle::default(), None);
                        row.push(" ", TextStyle::default(), None);
                    } else {
                        row.push(&" ".repeat(lead), TextStyle::default(), None);
                    }
                    row.pieces.extend(pieces);
                    out.push(row);
                }
            }
            MdBlockKind::Code { lines, .. } => {
                let indent = q + if b.list_depth > 0 { list_indent + 2 } else { 0 };
                // One cell of padding on each side inside the background.
                let inner = width.saturating_sub(indent + 2).max(1);
                for line in lines {
                    for pieces in break_chars(line, inner) {
                        let mut row = quote_row(b.quote_depth, RowKind::Code);
                        row.push(&" ".repeat(indent - q), TextStyle::default(), None);
                        row.fill_from = indent;
                        row.push(" ", TextStyle::default(), None);
                        row.pieces.extend(pieces);
                        out.push(row);
                    }
                }
            }
            MdBlockKind::Rule => {
                let mut row = quote_row(b.quote_depth, RowKind::Text);
                row.push(&"─".repeat(width.saturating_sub(q)), border(), None);
                out.push(row);
            }
            MdBlockKind::Table { align, header, rows } => {
                for mut row in table(align, header, rows, width.saturating_sub(q)) {
                    let mut r = quote_row(b.quote_depth, RowKind::Text);
                    r.pieces.append(&mut row.pieces);
                    out.push(r);
                }
            }
        }
    }
    out
}

/// A row starting with `depth` quote bars.
fn quote_row(depth: u8, kind: RowKind) -> Row {
    let mut row = Row::new(kind);
    for _ in 0..depth {
        row.push("│ ", border(), None);
    }
    row
}

/// Word-wrap a styled line to rows of at most `width` cells; "\n" breaks
/// rows. Words longer than a row are broken.
pub fn wrap(line: &StyledLine, width: usize) -> Vec<Vec<Piece>> {
    let width = width.max(1);
    let mut rows: Vec<Row> = vec![Row::new(RowKind::Text)];
    let mut w = 0usize;
    for (text, style, link) in line.segments() {
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                rows.push(Row::new(RowKind::Text));
                w = 0;
            }
            for word in part.split_word_bounds() {
                let ww = word.width();
                let blank = word.trim().is_empty();
                if w + ww > width && w > 0 {
                    rows.push(Row::new(RowKind::Text));
                    w = 0;
                    if blank {
                        continue;
                    }
                }
                if ww <= width {
                    rows.last_mut().expect("one row").push(word, style, link);
                    w += ww;
                    continue;
                }
                for g in word.graphemes(true) {
                    let gw = g.width();
                    if w + gw > width && w > 0 {
                        rows.push(Row::new(RowKind::Text));
                        w = 0;
                    }
                    rows.last_mut().expect("one row").push(g, style, link);
                    w += gw;
                }
            }
        }
    }
    // Spaces where a row was broken don't belong to it.
    let rows_len = rows.len();
    rows.into_iter()
        .enumerate()
        .map(|(i, mut r)| {
            if i + 1 < rows_len {
                while let Some(last) = r.pieces.last_mut() {
                    let trimmed = last.text.trim_end().len();
                    last.text.truncate(trimmed);
                    if !last.text.is_empty() {
                        break;
                    }
                    r.pieces.pop();
                }
            }
            r.pieces
        })
        .collect()
}

/// Break a line (code) into rows of `width` cells without looking for words.
fn break_chars(line: &StyledLine, width: usize) -> Vec<Vec<Piece>> {
    let mut rows = vec![Row::new(RowKind::Code)];
    let mut w = 0;
    for (text, style, link) in line.segments() {
        for g in text.graphemes(true) {
            let g = if g == "\t" { "    " } else { g };
            let gw = g.width();
            if w + gw > width && w > 0 {
                rows.push(Row::new(RowKind::Code));
                w = 0;
            }
            rows.last_mut().expect("one row").push(g, style, link);
            w += gw;
        }
    }
    rows.into_iter().map(|r| r.pieces).collect()
}

/// The pieces of `line` cut to `width` cells ("…" marks a cut), and their width.
fn clip(line: &StyledLine, width: usize) -> (Vec<Piece>, usize) {
    let mut row = Row::new(RowKind::Text);
    let total = line.text.replace('\n', " ").width();
    let room = if total > width { width.saturating_sub(1) } else { width };
    let mut w = 0;
    'outer: for (text, style, link) in line.segments() {
        for g in text.graphemes(true) {
            let g = if g == "\n" { " " } else { g };
            let gw = g.width();
            if w + gw > room {
                break 'outer;
            }
            row.push(g, style, link);
            w += gw;
        }
    }
    if total > width && width > 0 {
        row.push("…", TextStyle::default(), None);
        w += 1;
    }
    (row.pieces, w)
}

/// A table drawn with box lines, columns shrunk to fit `width`.
fn table(align: &[Align], header: &[StyledLine], rows: &[Vec<StyledLine>], width: usize) -> Vec<Row> {
    let n = header.len().max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if n == 0 {
        return Vec::new();
    }
    let cell_w = |l: &StyledLine| l.text.replace('\n', " ").width();
    let mut widths: Vec<usize> = (0..n)
        .map(|c| header.get(c).map_or(0, cell_w).max(rows.iter().filter_map(|r| r.get(c)).map(cell_w).max().unwrap_or(0)).max(1))
        .collect();
    let chrome = 3 * n + 1;
    let room = width.saturating_sub(chrome).max(n);
    while widths.iter().sum::<usize>() > room {
        let (i, &max) = widths.iter().enumerate().max_by_key(|(_, w)| **w).expect("n > 0");
        if max <= 1 {
            break;
        }
        widths[i] = max - 1;
    }
    let line = |l: char, m: char, r: char| {
        let mut row = Row::new(RowKind::Text);
        let mut s = String::from(l);
        for (i, w) in widths.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            s.push(if i + 1 < n { m } else { r });
        }
        row.push(&s, border(), None);
        row
    };
    let cells = |cells: &[StyledLine], bold: bool| {
        let mut row = Row::new(RowKind::Text);
        row.push("│", border(), None);
        for (c, w) in widths.iter().enumerate() {
            let empty = StyledLine::default();
            let (mut pieces, used) = clip(cells.get(c).unwrap_or(&empty), *w);
            if bold {
                for p in &mut pieces {
                    p.style.bold = true;
                }
            }
            let pad = w - used.min(*w);
            let (before, after) = match align.get(c).copied().unwrap_or(Align::None) {
                Align::Right => (pad, 0),
                Align::Center => (pad / 2, pad - pad / 2),
                Align::Left | Align::None => (0, pad),
            };
            row.push(&" ".repeat(before + 1), TextStyle::default(), None);
            for p in pieces {
                row.push(&p.text, p.style, p.link.as_deref());
            }
            row.push(&" ".repeat(after + 1), TextStyle::default(), None);
            row.push("│", border(), None);
        }
        row
    };
    let mut out = vec![line('┌', '┬', '┐'), cells(header, true), line('├', '┼', '┤')];
    out.extend(rows.iter().map(|r| cells(r, false)));
    out.push(line('└', '┴', '┘'));
    out
}

// ---------------------------------------------------------------------------

pub struct MarkdownView {
    pub data: MarkdownData,
    pub top: usize,
    /// The layout for a width.
    cache: RefCell<Option<(usize, Rc<Vec<Row>>)>>,
}

impl MarkdownView {
    pub fn new(data: MarkdownData) -> Self {
        MarkdownView { data, top: 0, cache: RefCell::new(None) }
    }

    /// The rows for a pane `width` cells wide (one column is kept for the scrollbar).
    pub fn rows(&self, width: u16) -> Rc<Vec<Row>> {
        let w = width.saturating_sub(1) as usize;
        let mut cache = self.cache.borrow_mut();
        match &*cache {
            Some((cw, rows)) if *cw == w => rows.clone(),
            _ => {
                let rows = Rc::new(layout(&self.data, w));
                *cache = Some((w, rows.clone()));
                rows
            }
        }
    }

    /// The link at a cell, if any.
    fn link_at(&self, area: Rect, col: u16, line: u16) -> Option<String> {
        let rows = self.rows(area.w);
        let top = clamp_top(self.top, rows.len(), area.h as usize);
        let row = rows.get(top + (line.checked_sub(area.y)?) as usize)?;
        let target = (col.checked_sub(area.x)?) as usize;
        let mut x = 0;
        for p in &row.pieces {
            let w = p.text.width();
            if target < x + w {
                return p.link.as_deref().map(str::to_owned);
            }
            x += w;
        }
        None
    }
}

/// Only links that leave the app: web pages and mail.
fn external(link: &str) -> bool {
    let l = link.to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://") || l.starts_with("mailto:")
}

impl ContentView for MarkdownView {
    fn kind(&self) -> &'static str {
        "markdown"
    }

    fn draw(&self, _app: &App, f: &mut Frame, area: Rect, ctx: &DrawCtx, _focused: bool) {
        let rows = self.rows(area.w);
        let painter = Painter::new(ctx);
        let c = &ctx.theme.colors;
        let visible = area.h as usize;
        let top = clamp_top(self.top, rows.len(), visible);
        let text_w = area.w.saturating_sub(1);
        for (i, row) in rows.iter().skip(top).take(visible).enumerate() {
            let y = area.y + i as u16;
            let mut base = ctx.theme.text();
            match row.kind {
                RowKind::Heading(level) => {
                    base = base.with(Flags::BOLD);
                    if level <= 2 {
                        base.fg = c.accent;
                    }
                }
                RowKind::Code => {
                    let x = area.x + row.fill_from as u16;
                    fill(f, Rect::new(x, y, text_w.saturating_sub(row.fill_from as u16), 1), code_background(c));
                }
                RowKind::Text => {}
            }
            let mut col = 0usize;
            let pieces = row.pieces.iter().map(|p| {
                let mut s = painter.style(p.style, base);
                if row.kind == RowKind::Code && col >= row.fill_from && p.style.bg.is_none() {
                    s.bg = code_background(c);
                }
                col += p.text.width();
                (p.text.as_str(), s)
            });
            paint(f, area.x, y, area.x + text_w, 0, pieces);
        }
        Scrollbar { total: rows.len(), visible, top }.draw(f, area, ctx);
    }

    fn handle_event(&mut self, app: &mut App, _h: Handle, ev: &Event, area: Rect) -> Outcome {
        if let Event::Mouse(m) = ev
            && m.kind == MouseKind::Down(MouseButton::Left)
        {
            if let Some(link) = self.link_at(area, m.col, m.row).filter(|l| external(l)) {
                app.tasks.run_in_background(
                    move |_| oc_local::open_with_default_app(std::path::Path::new(&link)).map_err(|e| (link, e)),
                    |app, r| {
                        if let Err((link, e)) = r {
                            app.error(format!("Cannot open {link}: {e}"));
                        }
                    },
                );
            }
            return Outcome::Consumed;
        }
        let total = self.rows(area.w).len();
        if scroll_event(&mut self.top, ev, total, area.h as usize) { Outcome::Consumed } else { Outcome::Ignored }
    }

    /// The rendered text (as laid out 80 cells wide).
    fn text(&self) -> Option<String> {
        Some(layout(&self.data, 80).iter().map(Row::text).collect::<Vec<_>>().join("\n"))
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

    fn md(src: &str, width: usize) -> Vec<String> {
        layout(&oc_preview::markdown::render(src, &|| false), width).iter().map(Row::text).collect()
    }

    #[test]
    fn paragraphs_wrap_at_words() {
        let rows = md("one two three four five six", 12);
        assert_eq!(rows, ["one two", "three four", "five six"]);
        assert_eq!(md("abcdefghijklmnop", 8), ["abcdefgh", "ijklmnop"]);
    }

    #[test]
    fn blocks_lists_quotes_code_rules_tables() {
        let rows = md("# Title\n\n- a\n- b\n  - c\n\n> quote\n\n```\nx = 1\n```\n\n---\n\n| a | b |\n|:--|--:|\n| 1 | 22 |\n", 30);
        assert_eq!(
            rows,
            [
                "Title",
                "",
                " • a",
                " • b",
                "   • c",
                "",
                "│ quote",
                "",
                " x = 1",
                "",
                "──────────────────────────────",
                "",
                "┌───┬────┐",
                "│ a │  b │",
                "├───┼────┤",
                "│ 1 │ 22 │",
                "└───┴────┘",
            ]
        );
    }

    #[test]
    fn wide_tables_shrink_and_elide() {
        let rows = md("| aaaaaaaaaa | bbbbbbbbbb |\n|--|--|\n| x | y |\n", 17);
        assert!(rows.iter().all(|r| r.width() <= 17), "{rows:?}");
        assert!(rows[1].contains('…'));
    }
}
