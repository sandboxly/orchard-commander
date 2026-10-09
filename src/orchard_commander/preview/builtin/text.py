"""Plain text and source code (syntax-highlighted), and Markdown (rendered)."""

from __future__ import annotations

from pygments import highlight
from pygments.formatters import HtmlFormatter
from pygments.lexers import TextLexer, get_lexer_for_filename
from pygments.util import ClassNotFound

from orchard_commander import fs
from orchard_commander.preview import content
from orchard_commander.preview.api import (
    Access,
    FileInfo,
    LoadContext,
    Loaded,
    Previewer,
    PreviewView,
    Score,
    ViewContext,
)
from orchard_commander.preview.builtin.textview import TextData, TextView
from orchard_commander.preview.source import PreviewSource

MAX_HIGHLIGHT_BYTES = 256 * 1024


def _read(src: PreviewSource) -> tuple[content.TextContent, str]:
    head = src.head(content.MAX_TEXT_BYTES + 1)
    truncated = len(head) > content.MAX_TEXT_BYTES
    text = content.decode_text(head[: content.MAX_TEXT_BYTES], truncated)
    info = fs.human_size(src.size) + (" · first 1 MB" if truncated else "")
    return text, info


class TextPreviewer(Previewer):
    id = "text"
    title = "Text"
    access = Access.HEAD
    needs_head = True

    def score(self, f: FileInfo) -> int:
        if f.is_dir or f.is_empty or f.head is None:
            return Score.NO
        return Score.CONTENT if content.looks_like_text(f.head) else Score.NO

    def load(self, src: PreviewSource, ctx: LoadContext) -> Loaded:
        text, info = _read(src)
        return Loaded(TextData(text.text, html=_highlight(src.name, text.text, ctx.colors.pygments_style)), info)

    def create_view(self, ctx: ViewContext) -> PreviewView:
        return TextView(ctx)


class MarkdownPreviewer(Previewer):
    id = "markdown"
    title = "Markdown"
    access = Access.HEAD
    needs_head = True

    def score(self, f: FileInfo) -> int:
        if f.ext not in content.MARKDOWN_EXTS or f.head is None or f.is_empty:
            return Score.NO
        return Score.SPECIFIC if content.looks_like_text(f.head) else Score.NO

    def load(self, src: PreviewSource, ctx: LoadContext) -> Loaded:
        text, info = _read(src)
        return Loaded(TextData(text.text, markdown=True, wrap=True), f"{info} · markdown")

    def create_view(self, ctx: ViewContext) -> PreviewView:
        return TextView(ctx)


def _highlight(name: str, text: str, style: str) -> str | None:
    if len(text) > MAX_HIGHLIGHT_BYTES:
        return None
    try:
        lexer = get_lexer_for_filename(name, text, stripnl=False)
    except ClassNotFound:
        return None
    if isinstance(lexer, TextLexer):
        return None
    return highlight(text, lexer, HtmlFormatter(style=style, noclasses=True, nobackground=True))
