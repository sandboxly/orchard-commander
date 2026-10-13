//! What previewers hand to the UI: plain, renderer-agnostic data.
//!
//! Colors are mostly *roles* ([`SyntaxRole`], [`ThemeRole`]) rather than RGB,
//! so a theme switch restyles a preview without loading it again; the UI
//! resolves syntax roles with [`crate::highlight::palette`].

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use oc_vfs::{Entry, ReadStream};

use crate::content;

// ---------------------------------------------------------------------------
// Colors and styles
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb { r, g, b }
    }

    /// 0xRRGGBB
    pub const fn hex(v: u32) -> Self {
        Rgb { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8 }
    }

    /// "#rrggbb"
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// Token classes of highlighted source, roughly Pygments' token types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SyntaxRole {
    Comment,
    Keyword,
    String,
    Number,
    Constant,
    Builtin,
    Function,
    Type,
    Variable,
    Operator,
    Punctuation,
    Tag,
    Attribute,
    Escape,
    Preprocessor,
    Heading,
    Inserted,
    Deleted,
    Error,
}

impl SyntaxRole {
    pub const COUNT: usize = 19;
    pub const ALL: [SyntaxRole; Self::COUNT] = [
        SyntaxRole::Comment,
        SyntaxRole::Keyword,
        SyntaxRole::String,
        SyntaxRole::Number,
        SyntaxRole::Constant,
        SyntaxRole::Builtin,
        SyntaxRole::Function,
        SyntaxRole::Type,
        SyntaxRole::Variable,
        SyntaxRole::Operator,
        SyntaxRole::Punctuation,
        SyntaxRole::Tag,
        SyntaxRole::Attribute,
        SyntaxRole::Escape,
        SyntaxRole::Preprocessor,
        SyntaxRole::Heading,
        SyntaxRole::Inserted,
        SyntaxRole::Deleted,
        SyntaxRole::Error,
    ];

    pub fn from_index(i: u8) -> Option<SyntaxRole> {
        Self::ALL.get(i as usize).copied()
    }
}

/// Colors of the app's color scheme (not of the syntax palette).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ThemeRole {
    /// Secondary text (image alt texts, footnote markers).
    Muted,
    /// Links.
    Link,
    /// Behind inline code and code blocks.
    CodeBackground,
    /// Quote bars, table borders, rules.
    Border,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Color {
    Rgb(Rgb),
    Syntax(SyntaxRole),
    Theme(ThemeRole),
}

/// How a run of text looks. `None` colors mean "the view's default".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TextStyle {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub monospace: bool,
}

impl TextStyle {
    pub fn fg(color: Color) -> Self {
        TextStyle { fg: Some(color), ..TextStyle::default() }
    }

    pub fn is_plain(&self) -> bool {
        *self == TextStyle::default()
    }
}

/// A styled range of a [`StyledLine`]'s text (byte offsets).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyleRun {
    pub start: usize,
    pub end: usize,
    pub style: TextStyle,
    /// Target of a link (Markdown).
    pub link: Option<Box<str>>,
}

/// One line (or paragraph) of text plus style runs. The text is stored once;
/// runs are sorted, don't overlap, and gaps between them are unstyled.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StyledLine {
    pub text: String,
    pub runs: Vec<StyleRun>,
}

impl StyledLine {
    pub fn plain(text: impl Into<String>) -> Self {
        StyledLine { text: text.into(), runs: Vec::new() }
    }

    /// Append `s` in `style`, merging with the previous run when it looks the same.
    pub fn push(&mut self, s: &str, style: TextStyle) {
        self.push_link(s, style, None);
    }

    pub fn push_link(&mut self, s: &str, style: TextStyle, link: Option<&str>) {
        if s.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(s);
        let end = self.text.len();
        if style.is_plain() && link.is_none() {
            return;
        }
        if let Some(last) = self.runs.last_mut()
            && last.end == start && last.style == style && last.link.as_deref() == link {
                last.end = end;
                return;
            }
        self.runs.push(StyleRun { start, end, style, link: link.map(Into::into) });
    }

    /// (text, style, link) pieces covering the whole line, gaps included.
    pub fn segments(&self) -> impl Iterator<Item = (&str, TextStyle, Option<&str>)> + '_ {
        let mut pos = 0;
        let mut runs = self.runs.iter().peekable();
        std::iter::from_fn(move || {
            if pos >= self.text.len() {
                return None;
            }
            match runs.peek() {
                Some(run) if run.start == pos => {
                    let run = runs.next().expect("peeked");
                    pos = run.end;
                    Some((&self.text[run.start..run.end], run.style, run.link.as_deref()))
                }
                Some(run) => {
                    let s = &self.text[pos..run.start];
                    pos = run.start;
                    Some((s, TextStyle::default(), None))
                }
                None => {
                    let s = &self.text[pos..];
                    pos = self.text.len();
                    Some((s, TextStyle::default(), None))
                }
            }
        })
    }
}

/// Text, optionally syntax-highlighted.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StyledText {
    pub lines: Vec<StyledLine>,
    /// The detected language ("Python"), None for plain text.
    pub language: Option<String>,
    /// Soft-wrap long lines (prose) instead of scrolling sideways (code).
    pub wrap: bool,
}

impl StyledText {
    /// Unstyled text, one line per "\n" (a trailing "\n" gives a final empty line).
    pub fn plain(text: &str) -> Self {
        StyledText { lines: text.split('\n').map(StyledLine::plain).collect(), language: None, wrap: false }
    }

    /// The text without styles, lines joined with "\n".
    pub fn text(&self) -> String {
        let mut out = String::with_capacity(self.lines.iter().map(|l| l.text.len() + 1).sum());
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&line.text);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Hex dumps and listings
// ---------------------------------------------------------------------------

/// Facts about a file and a dump of its first bytes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HexData {
    /// ("Name", "x.bin"), ("Size", "1.2 K (1,234 bytes)"), …
    pub facts: Vec<(String, String)>,
    /// Offset of `bytes[0]` in the file.
    pub offset: u64,
    pub bytes: Vec<u8>,
    /// Size of the whole file.
    pub size: u64,
}

impl HexData {
    /// The text the Python version showed: facts, a blank line, the dump;
    /// "(empty file)" for empty files.
    pub fn text(&self) -> String {
        if self.size == 0 && self.bytes.is_empty() {
            return "(empty file)".to_string();
        }
        let mut out = String::new();
        for (i, (k, v)) in self.facts.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&format!("{:<13}{v}", format!("{k}:")));
        }
        out.push_str("\n\n");
        out.push_str(&content::hexdump(&self.bytes, self.offset));
        out
    }
}

/// A folder's or archive's contents.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ListingData {
    pub dirs: usize,
    pub files: usize,
    /// Sum of the file sizes.
    pub total_size: u64,
    /// The first [`content::MAX_LISTING_ENTRIES`] entries, in listing order.
    pub entries: Vec<Entry>,
    /// How many entries were left out.
    pub more: usize,
}

impl ListingData {
    pub fn new(mut entries: Vec<Entry>, max_entries: usize) -> Self {
        let dirs = entries.iter().filter(|e| e.is_dir).count();
        let total_size = entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
        let files = entries.len() - dirs;
        let more = entries.len().saturating_sub(max_entries);
        entries.truncate(max_entries);
        ListingData { dirs, files, total_size, entries, more }
    }

    /// "1 directory, 2 files (12 B)"
    pub fn summary(&self) -> String {
        content::listing_summary(self.dirs, self.files, self.total_size)
    }

    /// Same text as [`content::format_listing`].
    pub fn text(&self) -> String {
        let mut lines = vec![self.summary(), String::new()];
        lines.extend(self.entries.iter().map(content::listing_line));
        if self.more > 0 {
            lines.push(format!("… and {} more", self.more));
        }
        lines.join("\n")
    }
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

/// One decoded frame: straight (not premultiplied) RGBA8, row-major.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    pub rgba: Vec<u8>,
    /// How long to show it (animations); zero for still images.
    pub delay: Duration,
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Frame({} bytes, {:?})", self.rgba.len(), self.delay)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImageData {
    /// Size of the decoded frames (possibly scaled down).
    pub width: u32,
    pub height: u32,
    /// Size of the image in the file.
    pub source_width: u32,
    pub source_height: u32,
    /// "PNG", "JPEG", "SVG", …
    pub format: &'static str,
    /// One frame for still images; all frames (composited) for animations.
    pub frames: Vec<Frame>,
}

impl ImageData {
    pub fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    None,
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MdBlockKind {
    /// Level 1–6.
    Heading { level: u8, line: StyledLine },
    /// Hard line breaks are "\n" in the text; soft ones are spaces.
    Paragraph(StyledLine),
    /// A list item's first paragraph; later ones are paragraphs one level deeper.
    /// `marker` is "•", "3." or a check box ("☐" / "☑").
    ListItem { marker: String, line: StyledLine },
    Code { language: Option<String>, lines: Vec<StyledLine> },
    Rule,
    Table { align: Vec<Align>, header: Vec<StyledLine>, rows: Vec<Vec<StyledLine>> },
}

/// A block of rendered Markdown, with how deep it sits in quotes and lists.
#[derive(Clone, Debug, PartialEq)]
pub struct MdBlock {
    pub kind: MdBlockKind,
    pub quote_depth: u8,
    pub list_depth: u8,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MarkdownData {
    /// The Markdown source (for copying / "view as text").
    pub source: String,
    pub blocks: Vec<MdBlock>,
}

// ---------------------------------------------------------------------------
// Documents and media (rendered/played by the UI)
// ---------------------------------------------------------------------------

/// Where the UI reads a document or media file from: a local path, or an open
/// seekable stream from another file system (so it isn't downloaded first).
pub enum DocumentSource {
    Path(PathBuf),
    Stream(Box<dyn ReadStream>),
}

impl fmt::Debug for DocumentSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DocumentSource::Path(p) => write!(f, "Path({})", p.display()),
            DocumentSource::Stream(_) => f.write_str("Stream(..)"),
        }
    }
}

/// A PDF for the UI's PDF renderer (placeholder until rendering moves here).
#[derive(Debug)]
pub struct PdfData {
    pub source: DocumentSource,
}

pub struct MediaData {
    pub video: bool,
    pub name: String,
    pub size: u64,
    pub source: DocumentSource,
    /// Audio from a stream: a second stream for the waveform (small files only).
    pub waveform_stream: Option<Box<dyn ReadStream>>,
}

impl fmt::Debug for MediaData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MediaData")
            .field("video", &self.video)
            .field("name", &self.name)
            .field("size", &self.size)
            .field("source", &self.source)
            .field("waveform_stream", &self.waveform_stream.is_some())
            .finish()
    }
}

// ---------------------------------------------------------------------------

/// What a previewer produced, for the UI to render.
#[derive(Debug)]
pub enum PreviewData {
    StyledText(StyledText),
    Hex(HexData),
    Listing(ListingData),
    Image(ImageData),
    Markdown(MarkdownData),
    /// A short text shown in place of a preview.
    Message(String),
    Pdf(PdfData),
    Media(MediaData),
}

impl PreviewData {
    /// The kind as a short word, for logs and tests.
    pub fn kind(&self) -> &'static str {
        match self {
            PreviewData::StyledText(_) => "text",
            PreviewData::Hex(_) => "hex",
            PreviewData::Listing(_) => "listing",
            PreviewData::Image(_) => "image",
            PreviewData::Markdown(_) => "markdown",
            PreviewData::Message(_) => "message",
            PreviewData::Pdf(_) => "pdf",
            PreviewData::Media(_) => "media",
        }
    }

    /// A plain-text rendering where one exists (text, hex, listings, messages).
    pub fn plain_text(&self) -> Option<String> {
        Some(match self {
            PreviewData::StyledText(t) => t.text(),
            PreviewData::Hex(h) => h.text(),
            PreviewData::Listing(l) => l.text(),
            PreviewData::Markdown(m) => m.source.clone(),
            PreviewData::Message(m) => m.clone(),
            _ => return None,
        })
    }
}
