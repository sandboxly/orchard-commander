//! Rendered Markdown (1.x showed it in a `QTextBrowser`): headings, wrapped
//! paragraphs, lists, quotes, highlighted code blocks, rules and tables,
//! laid out for the pane's width. Links open in the default app on click;
//! text can be selected and copied ([`super::select`]).
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

use super::select::{self, MenuChoice, Pos, RowText, Selection};
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
    /// Cells of indentation, quote bars and list markers before the text
    /// (not copied, as Qt didn't copy them).
    pub lead: usize,
    /// What a copy puts between the previous row and this one: "\n", or
    /// " " / "" where a paragraph or code line was wrapped.
    pub joiner: &'static str,
}

impl Row {
    fn new(kind: RowKind) -> Row {
        Row { kind, fill_from: 0, pieces: Vec::new(), lead: 0, joiner: "\n" }
    }

    /// Width in cells.
    pub fn width(&self) -> usize {
        self.pieces.iter().map(|p| p.text.width()).sum()
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
                for (pieces, joiner) in wrap_joined(line, width.saturating_sub(q)) {
                    let mut row = quote_row(b.quote_depth, kind);
                    row.pieces.extend(pieces);
                    row.joiner = joiner;
                    out.push(row);
                }
            }
            MdBlockKind::Paragraph(line) => {
                let indent = q + if b.list_depth > 0 { list_indent + 2 } else { 0 };
                for (pieces, joiner) in wrap_joined(line, width.saturating_sub(indent)) {
                    let mut row = quote_row(b.quote_depth, RowKind::Text);
                    row.push(&" ".repeat(indent - q), TextStyle::default(), None);
                    row.pieces.extend(pieces);
                    (row.lead, row.joiner) = (indent, joiner);
                    out.push(row);
                }
            }
            MdBlockKind::ListItem { marker, line } => {
                let lead = list_indent + marker.width() + 1;
                for (i, (pieces, joiner)) in wrap_joined(line, width.saturating_sub(q + lead)).into_iter().enumerate() {
                    let mut row = quote_row(b.quote_depth, RowKind::Text);
                    if i == 0 {
                        row.push(&" ".repeat(list_indent), TextStyle::default(), None);
                        row.push(marker, TextStyle::default(), None);
                        row.push(" ", TextStyle::default(), None);
                    } else {
                        row.push(&" ".repeat(lead), TextStyle::default(), None);
                    }
                    row.pieces.extend(pieces);
                    (row.lead, row.joiner) = (q + lead, joiner);
                    out.push(row);
                }
            }
            MdBlockKind::Code { lines, .. } => {
                let indent = q + if b.list_depth > 0 { list_indent + 2 } else { 0 };
                // One cell of padding on each side inside the background.
                let inner = width.saturating_sub(indent + 2).max(1);
                for line in lines {
                    for (i, pieces) in break_chars(line, inner).into_iter().enumerate() {
                        let mut row = quote_row(b.quote_depth, RowKind::Code);
                        row.push(&" ".repeat(indent - q), TextStyle::default(), None);
                        row.fill_from = indent;
                        row.push(" ", TextStyle::default(), None);
                        row.pieces.extend(pieces);
                        (row.lead, row.joiner) = (indent + 1, if i == 0 { "\n" } else { "" });
                        out.push(row);
                    }
                }
            }
            MdBlockKind::Rule => {
                let mut row = quote_row(b.quote_depth, RowKind::Text);
                row.push(&"─".repeat(width.saturating_sub(q)), border(), None);
                row.lead = usize::MAX; // a line, not text
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
    row.lead = depth as usize * 2;
    row
}

/// Word-wrap a styled line to rows of at most `width` cells; "\n" breaks
/// rows. Words longer than a row are broken.
pub fn wrap(line: &StyledLine, width: usize) -> Vec<Vec<Piece>> {
    wrap_joined(line, width).into_iter().map(|(pieces, _)| pieces).collect()
}

/// [`wrap`], each row with what joins it to the row before when copied:
/// "\n" after a line break, " " where spaces were wrapped away, "" where a
/// word was broken.
fn wrap_joined(line: &StyledLine, width: usize) -> Vec<(Vec<Piece>, &'static str)> {
    let width = width.max(1);
    let soft = || Row { joiner: "", ..Row::new(RowKind::Text) };
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
                    rows.push(soft());
                    w = 0;
                    if blank {
                        rows.last_mut().expect("one row").joiner = " ";
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
                        rows.push(soft());
                        w = 0;
                    }
                    rows.last_mut().expect("one row").push(g, style, link);
                    w += gw;
                }
            }
        }
    }
    // Spaces where a row was broken don't belong to it (a copy puts one back).
    for i in 0..rows.len().saturating_sub(1) {
        let r = &mut rows[i];
        let mut trimmed = false;
        while let Some(last) = r.pieces.last_mut() {
            let keep = last.text.trim_end().len();
            trimmed |= keep < last.text.len();
            last.text.truncate(keep);
            if !last.text.is_empty() {
                break;
            }
            r.pieces.pop();
        }
        if trimmed && rows[i + 1].joiner.is_empty() {
            rows[i + 1].joiner = " ";
        }
    }
    rows.into_iter().map(|r| (r.pieces, r.joiner)).collect()
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
    pub selection: Selection,
    /// The link the left button went down on (opened on release, unless
    /// the press became a selection).
    pressed_link: Option<String>,
    /// The layout for a width.
    cache: RefCell<Option<(usize, Rc<Vec<Row>>)>>,
}

impl MarkdownView {
    pub fn new(data: MarkdownData) -> Self {
        MarkdownView { data, top: 0, selection: Selection::default(), pressed_link: None, cache: RefCell::new(None) }
    }

    /// The selected text as laid out for the last width drawn.
    pub fn selected_text(&self) -> String {
        let cache = self.cache.borrow();
        let Some((_, rows)) = cache.as_ref() else { return String::new() };
        self.selection.text(|r| rows.get(r).map(|row| row_text(row)))
    }

    /// Selection, links and the context menu; None if the event is none of those.
    fn mouse(&mut self, app: &mut App, h: Handle, ev: &Event, area: Rect) -> Option<Outcome> {
        let Event::Mouse(m) = ev else { return None };
        let text_area = Rect { w: area.w.saturating_sub(1), ..area };
        let inside = text_area.contains(m.col, m.row);
        let link = self.link_at(area, m.col, m.row);
        match m.kind {
            MouseKind::Down(MouseButton::Right) | MouseKind::DoubleClick(MouseButton::Right) if inside => {
                let has_selection = !self.selection.is_empty();
                select::context_menu(app, m, has_selection, link.is_some(), move |app, choice| match choice {
                    MenuChoice::CopyLink => select::copy(app, link.as_deref().unwrap_or_default()),
                    MenuChoice::Copy => {
                        h.with::<MarkdownView, _>(app, |v, app, _| select::copy(app, &v.selected_text()));
                    }
                    MenuChoice::SelectAll => {
                        h.with::<MarkdownView, _>(app, |v, _, area| {
                            let n = v.rows(area.w).len();
                            v.selection.select_all(n);
                        });
                    }
                });
                Some(Outcome::Consumed)
            }
            MouseKind::Down(MouseButton::Left) | MouseKind::DoubleClick(MouseButton::Left) if !inside => None,
            MouseKind::Down(MouseButton::Left)
            | MouseKind::DoubleClick(MouseButton::Left)
            | MouseKind::Up(MouseButton::Left)
            | MouseKind::Move => {
                let rows = self.rows(area.w);
                let dragging = self.selection.is_dragging();
                let top = clamp_top(self.top, rows.len(), area.h as usize);
                let pos = select::pos_at(m, text_area, top, 0);
                let word = |(r, c): Pos| rows.get(r).and_then(|row| select::word_at(&row.text(), c));
                let changed = self.selection.mouse(m, pos, word);
                match m.kind {
                    MouseKind::Down(_) => self.pressed_link = link.filter(|l| external(l)),
                    MouseKind::Up(_) => {
                        let pressed = self.pressed_link.take();
                        if let Some(l) = pressed.filter(|l| self.selection.is_empty() && link.as_ref() == Some(l)) {
                            open_link(app, l);
                        }
                    }
                    _ => {}
                }
                // Hovering changes nothing on screen.
                let mine = changed || dragging || m.kind != MouseKind::Move;
                Some(if mine { Outcome::Consumed } else { Outcome::Ignored })
            }
            _ => None,
        }
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

/// What copying sees of a row.
fn row_text(row: &Row) -> RowText<'static> {
    RowText { text: row.text().into(), lead: row.lead, joiner: row.joiner }
}

fn open_link(app: &mut App, link: String) {
    app.tasks.run_in_background(
        move |_| oc_local::open_with_default_app(std::path::Path::new(&link)).map_err(|e| (link, e)),
        |app, r| {
            if let Err((link, e)) = r {
                app.error(format!("Cannot open {link}: {e}"));
            }
        },
    );
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
            if let Some(cells) = self.selection.cells_in_row(top + i, row.width()) {
                select::paint(f, ctx, cells, area.x, y, 0, area.x + text_w);
            }
        }
        Scrollbar { total: rows.len(), visible, top }.draw(f, area, ctx);
    }

    fn handle_event(&mut self, app: &mut App, h: Handle, ev: &Event, area: Rect) -> Outcome {
        if let Some(outcome) = self.mouse(app, h, ev, area) {
            return outcome;
        }
        if let Event::Key(k) = ev
            && select::is_copy(k)
        {
            select::copy(app, &self.selected_text());
            return Outcome::Consumed;
        }
        let total = self.rows(area.w).len();
        if scroll_event(&mut self.top, ev, total, area.h as usize) { Outcome::Consumed } else { Outcome::Ignored }
    }

    fn resized(&mut self, _app: &mut App, _h: Handle, _area: Rect) {
        self.selection.clear(); // rows are laid out anew
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
    fn copying_rejoins_wrapped_rows_and_skips_markers() {
        let data = oc_preview::markdown::render("one two three\n\n- item\n\n> quoted\n\n---\n\nabcdefghij", &|| false);
        let rows = layout(&data, 8);
        let mut s = Selection::default();
        s.select_all(rows.len());
        assert_eq!(s.text(|r| rows.get(r).map(row_text)), "one two three\n\nitem\n\nquoted\n\n\n\nabcdefghij");
    }

    #[test]
    fn wide_tables_shrink_and_elide() {
        let rows = md("| aaaaaaaaaa | bbbbbbbbbb |\n|--|--|\n| x | y |\n", 17);
        assert!(rows.iter().all(|r| r.width() <= 17), "{rows:?}");
        assert!(rows[1].contains('…'));
    }
}
