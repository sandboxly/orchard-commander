//! Plain text and source code (syntax-highlighted), and Markdown (rendered).

use oc_vfs::human_size;

use crate::api::{Access, FileInfo, LoadContext, Loaded, PreviewError, Previewer, Score};
use crate::content::{self, MARKDOWN_EXTS, MAX_TEXT_BYTES, TextContent};
use crate::data::{PreviewData, StyledLine, StyledText};
use crate::highlight::{highlight_lines, syntax_for_file, within_highlight_limit};
use crate::markdown;
use crate::source::PreviewSource;

/// The first 1 MB as text, and the status line ("1.2 M · first 1 MB").
fn read(src: &PreviewSource) -> Result<(TextContent, String), PreviewError> {
    let text = src.with_head(MAX_TEXT_BYTES + 1, |head| {
        let truncated = head.len() > MAX_TEXT_BYTES;
        content::decode_text(&head[..head.len().min(MAX_TEXT_BYTES)], truncated)
    })?;
    let info = format!("{}{}", human_size(src.size()), if text.truncated { " · first 1 MB" } else { "" });
    Ok((text, info))
}

pub struct TextPreviewer;

impl Previewer for TextPreviewer {
    fn id(&self) -> &str {
        "text"
    }

    fn title(&self) -> &str {
        "Text"
    }

    fn access(&self) -> Access {
        Access::Head
    }

    fn needs_head(&self) -> bool {
        true
    }

    /// Colors are roles the view resolves itself.
    fn reload_on_theme_change(&self) -> bool {
        false
    }

    fn score(&self, f: &FileInfo) -> i32 {
        match &f.head {
            Some(head) if !f.is_dir && !f.is_empty() && content::looks_like_text(head) => Score::CONTENT,
            _ => Score::NO,
        }
    }

    fn load(&self, src: &PreviewSource, ctx: &LoadContext) -> Result<Loaded, PreviewError> {
        let (text, info) = read(src)?;
        Ok(Loaded::new(PreviewData::StyledText(highlight(src.name(), &text.text, ctx)), info))
    }
}

/// Highlighted if the name says which language and the text is small enough.
pub(crate) fn highlight(name: &str, text: &str, ctx: &LoadContext) -> StyledText {
    let syntax = within_highlight_limit(text).then(|| syntax_for_file(name)).flatten();
    if let Some(syntax) = syntax
        && let Some(lines) = highlight_lines(text, syntax, &*ctx.is_cancelled) {
            return StyledText { lines, language: Some(syntax.name.clone()), wrap: false };
        }
    StyledText { lines: text.split('\n').map(StyledLine::plain).collect(), language: None, wrap: false }
}

pub struct MarkdownPreviewer;

impl Previewer for MarkdownPreviewer {
    fn id(&self) -> &str {
        "markdown"
    }

    fn title(&self) -> &str {
        "Markdown"
    }

    fn access(&self) -> Access {
        Access::Head
    }

    fn needs_head(&self) -> bool {
        true
    }

    fn reload_on_theme_change(&self) -> bool {
        false
    }

    fn score(&self, f: &FileInfo) -> i32 {
        if !MARKDOWN_EXTS.contains(&f.ext.as_str()) || f.is_empty() {
            return Score::NO;
        }
        match &f.head {
            Some(head) if content::looks_like_text(head) => Score::SPECIFIC,
            _ => Score::NO,
        }
    }

    fn load(&self, src: &PreviewSource, ctx: &LoadContext) -> Result<Loaded, PreviewError> {
        let (text, info) = read(src)?;
        let data = markdown::render(&text.text, &*ctx.is_cancelled);
        Ok(Loaded::new(PreviewData::Markdown(data), format!("{info} · markdown")))
    }
}
