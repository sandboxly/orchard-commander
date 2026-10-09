"""A read-only text view shared by the text-like previewers."""

from __future__ import annotations

from dataclasses import dataclass

from PySide6.QtWidgets import QTextBrowser, QVBoxLayout, QWidget

from orchard_commander.preview.api import PreviewView, ViewContext


@dataclass(slots=True)
class TextData:
    text: str
    html: str | None = None  # rich version (e.g. syntax-highlighted), preferred if set
    markdown: bool = False
    wrap: bool = False


class TextView(PreviewView):
    def __init__(self, ctx: ViewContext) -> None:
        super().__init__()
        self.themes = ctx.themes
        self.browser = QTextBrowser()
        self.browser.setOpenExternalLinks(True)
        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.addWidget(self.browser)

    def show_data(self, data: TextData) -> None:
        mode = QTextBrowser.LineWrapMode.WidgetWidth if data.wrap else QTextBrowser.LineWrapMode.NoWrap
        self.browser.setLineWrapMode(mode)
        if data.markdown:
            self.browser.setMarkdown(data.text)
            # Qt renders code spans in a generic 'monospace' that may resolve to a
            # proportional font; use the app's monospace font instead.
            family = self.themes.font.family()
            self.browser.setHtml(self.browser.toHtml().replace("'monospace'", f"'{family}'"))
        elif data.html:
            self.browser.setHtml(data.html)
        else:
            self.browser.setPlainText(data.text)

    def clear(self) -> None:
        self.browser.clear()

    def focus_widget(self) -> QWidget:
        return self.browser

    def plain_text(self) -> str:
        return self.browser.toPlainText()
