//! Markdown rendered to styled blocks (GitHub flavour: tables, strikethrough,
//! task lists, footnotes), with highlighted code blocks.
//!
//! Raw HTML is dropped except `<br>`: the UI has no HTML engine, and showing
//! tags verbatim reads worse than leaving them out.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use crate::data::{Align, Color, MarkdownData, MdBlock, MdBlockKind, StyledLine, TextStyle, ThemeRole};
use crate::highlight::{highlight_lines, syntax_for_token, within_highlight_limit};

enum Pending {
    Paragraph,
    Heading(u8),
    ListItem(String),
}

#[derive(Default)]
struct Table {
    align: Vec<Align>,
    header: Vec<StyledLine>,
    rows: Vec<Vec<StyledLine>>,
    row: Vec<StyledLine>,
    cell: Option<StyledLine>,
}

struct Renderer<'c> {
    blocks: Vec<MdBlock>,
    current: Option<(Pending, StyledLine)>,
    quote: u8,
    /// Next number of each open list (None: bullets).
    lists: Vec<Option<u64>>,
    bold: u32,
    italic: u32,
    strike: u32,
    links: Vec<String>,
    images: u32,
    code: Option<(Option<String>, String)>,
    table: Option<Table>,
    footnote: Option<String>,
    html_block: bool,
    is_cancelled: &'c dyn Fn() -> bool,
}

/// Render `source` into blocks. Bounded work: code blocks above
/// [`crate::highlight::MAX_HIGHLIGHT_BYTES`] stay plain.
pub fn render(source: &str, is_cancelled: &dyn Fn() -> bool) -> MarkdownData {
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_GFM;
    let mut r = Renderer {
        blocks: Vec::new(),
        current: None,
        quote: 0,
        lists: Vec::new(),
        bold: 0,
        italic: 0,
        strike: 0,
        links: Vec::new(),
        images: 0,
        code: None,
        table: None,
        footnote: None,
        html_block: false,
        is_cancelled,
    };
    for event in Parser::new_ext(source, opts) {
        r.event(event);
    }
    r.flush();
    MarkdownData { source: source.to_owned(), blocks: r.blocks }
}

impl Renderer<'_> {
    fn depth(&self) -> u8 {
        self.lists.len().min(u8::MAX as usize) as u8
    }

    fn push_block(&mut self, kind: MdBlockKind) {
        self.blocks.push(MdBlock { kind, quote_depth: self.quote, list_depth: self.depth() });
    }

    fn flush(&mut self) {
        if let Some((pending, line)) = self.current.take() {
            let kind = match pending {
                Pending::Paragraph if line.text.trim().is_empty() => return,
                Pending::Paragraph => MdBlockKind::Paragraph(line),
                Pending::Heading(level) => MdBlockKind::Heading { level, line },
                Pending::ListItem(marker) => MdBlockKind::ListItem { marker, line },
            };
            self.push_block(kind);
        }
    }

    fn start(&mut self, pending: Pending) {
        self.flush();
        let mut line = StyledLine::default();
        if let Some(label) = self.footnote.take() {
            line.push(&format!("[{label}] "), TextStyle::fg(Color::Theme(ThemeRole::Muted)));
        }
        self.current = Some((pending, line));
    }

    fn style(&self) -> TextStyle {
        let mut s = TextStyle {
            bold: self.bold > 0,
            italic: self.italic > 0 || self.images > 0,
            strikethrough: self.strike > 0,
            ..TextStyle::default()
        };
        if self.images > 0 {
            s.fg = Some(Color::Theme(ThemeRole::Muted));
        } else if !self.links.is_empty() {
            s.fg = Some(Color::Theme(ThemeRole::Link));
            s.underline = true;
        }
        s
    }

    /// Where inline content goes: a table cell or the current block.
    fn line(&mut self) -> &mut StyledLine {
        if self.table.as_ref().is_some_and(|t| t.cell.is_some()) {
            return self.table.as_mut().and_then(|t| t.cell.as_mut()).expect("checked");
        }
        if self.current.is_none() {
            self.start(Pending::Paragraph);
        }
        &mut self.current.as_mut().expect("just started").1
    }

    fn text(&mut self, s: &str, style: TextStyle) {
        let link = if self.images > 0 { None } else { self.links.last().cloned() };
        self.line().push_link(s, style, link.as_deref());
    }

    fn event(&mut self, event: Event<'_>) {
        if let Some((_, buf)) = self.code.as_mut() {
            match event {
                Event::Text(t) => buf.push_str(&t),
                Event::End(TagEnd::CodeBlock) => self.end_code(),
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(t) => {
                if !self.html_block {
                    let style = self.style();
                    self.text(&t, style);
                }
            }
            Event::Code(t) => {
                let style = TextStyle {
                    monospace: true,
                    bg: Some(Color::Theme(ThemeRole::CodeBackground)),
                    ..self.style()
                };
                self.text(&t, style);
            }
            Event::SoftBreak => {
                let style = self.style();
                self.text(" ", style);
            }
            Event::HardBreak => self.text("\n", TextStyle::default()),
            Event::InlineHtml(h) => {
                let tag = h.trim().to_ascii_lowercase();
                if matches!(tag.as_str(), "<br>" | "<br/>" | "<br />") {
                    self.text("\n", TextStyle::default());
                }
            }
            Event::Html(_) => {}
            Event::FootnoteReference(label) => {
                let style = TextStyle { fg: Some(Color::Theme(ThemeRole::Muted)), ..TextStyle::default() };
                self.text(&format!("[{label}]"), style);
            }
            Event::Rule => {
                self.flush();
                self.push_block(MdBlockKind::Rule);
            }
            Event::TaskListMarker(checked) => {
                if let Some((Pending::ListItem(marker), _)) = self.current.as_mut() {
                    *marker = if checked { "☑" } else { "☐" }.to_string();
                }
            }
            Event::InlineMath(t) | Event::DisplayMath(t) => {
                let style = TextStyle { monospace: true, ..self.style() };
                self.text(&t, style);
            }
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                // A loose list's item text arrives in a paragraph: fill the item.
                let reuse = matches!(&self.current, Some((Pending::ListItem(_), l)) if l.text.is_empty());
                if !reuse && self.table.as_ref().is_none_or(|t| t.cell.is_none()) {
                    self.start(Pending::Paragraph);
                }
            }
            Tag::Heading { level, .. } => self.start(Pending::Heading(level as u8)),
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_add(1);
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.split([' ', ',', '{']).next().filter(|s| !s.is_empty()).map(str::to_owned),
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((lang, String::new()));
            }
            Tag::HtmlBlock => {
                self.flush();
                self.html_block = true;
            }
            Tag::List(start) => {
                self.flush();
                self.lists.push(start);
            }
            Tag::Item => {
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}.");
                        *n += 1;
                        m
                    }
                    _ => "•".to_string(),
                };
                self.start(Pending::ListItem(marker));
            }
            Tag::FootnoteDefinition(label) => {
                self.flush();
                self.footnote = Some(label.to_string());
            }
            Tag::Table(align) => {
                self.flush();
                let align = align
                    .into_iter()
                    .map(|a| match a {
                        Alignment::None => Align::None,
                        Alignment::Left => Align::Left,
                        Alignment::Center => Align::Center,
                        Alignment::Right => Align::Right,
                    })
                    .collect();
                self.table = Some(Table { align, ..Table::default() });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    t.row.clear();
                }
            }
            Tag::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    t.cell = Some(StyledLine::default());
                }
            }
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => self.links.push(dest_url.into_string()),
            Tag::Image { .. } => self.images += 1,
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                if !matches!(&self.current, Some((Pending::ListItem(_), _))) {
                    self.flush();
                }
            }
            TagEnd::Heading(_) | TagEnd::Item => self.flush(),
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::HtmlBlock => self.html_block = false,
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
            }
            TagEnd::FootnoteDefinition => {
                self.flush();
                self.footnote = None;
            }
            TagEnd::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    let cell = t.cell.take().unwrap_or_default();
                    t.row.push(cell);
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    t.header = std::mem::take(&mut t.row);
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    let row = std::mem::take(&mut t.row);
                    t.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.push_block(MdBlockKind::Table { align: t.align, header: t.header, rows: t.rows });
                }
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                self.links.pop();
            }
            TagEnd::Image => self.images = self.images.saturating_sub(1),
            _ => {}
        }
    }

    fn end_code(&mut self) {
        let Some((language, mut text)) = self.code.take() else { return };
        if text.ends_with('\n') {
            text.pop();
        }
        let syntax = language.as_deref().and_then(syntax_for_token);
        let cancelled = self.is_cancelled;
        let lines = syntax
            .filter(|_| within_highlight_limit(&text))
            .and_then(|s| highlight_lines(&text, s, cancelled))
            .map(|mut lines| {
                if lines.len() > 1 && lines.last().is_some_and(|l| l.text.is_empty()) && !text.ends_with('\n') {
                    lines.pop();
                }
                lines
            })
            .unwrap_or_else(|| text.split('\n').map(StyledLine::plain).collect());
        self.push_block(MdBlockKind::Code { language, lines });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::SyntaxRole;

    fn md(s: &str) -> Vec<MdBlock> {
        render(s, &|| false).blocks
    }

    #[test]
    fn headings_paragraphs_and_inline_styles() {
        let blocks = md("# Title\n\nSome *markdown* and **bold** `code`,\nsoft break.\n");
        assert_eq!(blocks.len(), 2);
        let MdBlockKind::Heading { level: 1, line } = &blocks[0].kind else { panic!("{:?}", blocks[0]) };
        assert_eq!(line.text, "Title");
        let MdBlockKind::Paragraph(p) = &blocks[1].kind else { panic!() };
        assert_eq!(p.text, "Some markdown and bold code, soft break.");
        let seg: Vec<_> = p.segments().collect();
        assert!(seg.iter().any(|(t, s, _)| *t == "markdown" && s.italic));
        assert!(seg.iter().any(|(t, s, _)| *t == "bold" && s.bold));
        assert!(seg.iter().any(|(t, s, _)| *t == "code" && s.monospace));
    }

    #[test]
    fn lists_tight_loose_ordered_and_tasks() {
        let blocks = md("- a\n- b\n  - nested\n\n3. three\n4. four\n\n- [x] done\n- [ ] todo\n\n* loose\n\n* items\n");
        let items: Vec<(String, String, u8)> = blocks
            .iter()
            .filter_map(|b| match &b.kind {
                MdBlockKind::ListItem { marker, line } => Some((marker.clone(), line.text.clone(), b.list_depth)),
                _ => None,
            })
            .collect();
        let expect = [
            ("•", "a", 1),
            ("•", "b", 1),
            ("•", "nested", 2),
            ("3.", "three", 1),
            ("4.", "four", 1),
            ("☑", "done", 1),
            ("☐", "todo", 1),
            ("•", "loose", 1),
            ("•", "items", 1),
        ];
        let expect: Vec<_> = expect.iter().map(|(m, t, d)| (m.to_string(), t.to_string(), *d)).collect();
        assert_eq!(items, expect);
    }

    #[test]
    fn code_blocks_are_highlighted() {
        let blocks = md("```python\ndef f():\n    pass\n```\n\n    indented\n");
        let MdBlockKind::Code { language, lines } = &blocks[0].kind else { panic!() };
        assert_eq!(language.as_deref(), Some("python"));
        assert_eq!(lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["def f():", "    pass"]);
        assert!(lines[0].runs.iter().any(|r| r.style.fg == Some(Color::Syntax(SyntaxRole::Keyword))));
        let MdBlockKind::Code { language: None, lines } = &blocks[1].kind else { panic!() };
        assert_eq!(lines[0].text, "indented");
    }

    #[test]
    fn links_quotes_rules_tables_html() {
        let blocks = md(
            "> quoted [link](https://x.org)\n\n---\n\n| a | b |\n|:--|--:|\n| 1 | **2** |\n\n<div>html</div>\n\nline<br>two\n",
        );
        let MdBlockKind::Paragraph(p) = &blocks[0].kind else { panic!() };
        assert_eq!(blocks[0].quote_depth, 1);
        let link = p.runs.iter().find(|r| r.link.is_some()).unwrap();
        assert_eq!((&p.text[link.start..link.end], link.link.as_deref()), ("link", Some("https://x.org")));
        assert_eq!(blocks[1].kind, MdBlockKind::Rule);
        let MdBlockKind::Table { align, header, rows } = &blocks[2].kind else { panic!("{:?}", blocks[2]) };
        assert_eq!(align, &[Align::Left, Align::Right]);
        assert_eq!(header.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(rows[0][1].text, "2");
        let MdBlockKind::Paragraph(last) = &blocks[3].kind else { panic!("{:?}", blocks[3]) };
        assert_eq!(last.text, "line\ntwo");
        assert_eq!(blocks.len(), 4);
    }

    #[test]
    fn garbage_and_empty_input() {
        assert!(md("").is_empty());
        let bytes: Vec<u8> = (0..3000u32).map(|i| (i * 7919 % 251) as u8).collect();
        let text = String::from_utf8_lossy(&bytes);
        let _ = md(&text);
        let _ = md(&"[".repeat(10_000));
        let _ = md(&"> ".repeat(500));
    }
}
