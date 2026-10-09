"""Theme application: fonts, the Qt style sheet, and change notification."""

from __future__ import annotations

from PySide6.QtCore import QObject, Signal
from PySide6.QtGui import QColor, QFont, QFontDatabase, QPalette
from PySide6.QtWidgets import QApplication

from orchard_commander.themes import Theme, get_theme

MONO_FAMILIES = [
    "JetBrains Mono",
    "Cascadia Mono",
    "Cascadia Code",
    "SF Mono",
    "Menlo",
    "Consolas",
    "DejaVu Sans Mono",
    "Ubuntu Mono",
    "Liberation Mono",
]

DEFAULT_FONT_SIZE = 13


def monospace_font(point_size: int) -> QFont:
    available = set(QFontDatabase.families())
    for family in MONO_FAMILIES:
        if family in available:
            font = QFont(family, point_size)
            break
    else:
        font = QFontDatabase.systemFont(QFontDatabase.SystemFont.FixedFont)
        font.setPointSize(point_size)
    font.setStyleHint(QFont.StyleHint.Monospace)
    font.setFixedPitch(True)
    return font


class ThemeManager(QObject):
    """Holds the active theme and font; widgets listen to `changed` to restyle."""

    changed = Signal()

    def __init__(self, theme_name: str, font_size: int = DEFAULT_FONT_SIZE) -> None:
        super().__init__()
        self.theme: Theme = get_theme(theme_name)
        self.font_size = font_size
        self.font = monospace_font(font_size)

    def set_theme(self, name: str) -> None:
        self.theme = get_theme(name)
        self.apply()

    def set_font_size(self, size: int) -> None:
        self.font_size = max(7, min(size, 40))
        self.font = monospace_font(self.font_size)
        self.apply()

    def apply(self) -> None:
        app = QApplication.instance()
        assert isinstance(app, QApplication)
        app.setFont(self.font)
        app.setPalette(palette(self.theme))
        app.setStyleSheet(stylesheet(self.theme))
        self.changed.emit()


def palette(t: Theme) -> QPalette:
    p = QPalette()
    roles = {
        QPalette.ColorRole.Window: t.background,
        QPalette.ColorRole.WindowText: t.foreground,
        QPalette.ColorRole.Base: t.surface,
        QPalette.ColorRole.AlternateBase: t.surface,
        QPalette.ColorRole.Text: t.foreground,
        QPalette.ColorRole.Button: t.border,
        QPalette.ColorRole.ButtonText: t.foreground,
        QPalette.ColorRole.Highlight: t.cursor_bg,
        QPalette.ColorRole.HighlightedText: t.cursor_fg,
        QPalette.ColorRole.ToolTipBase: t.surface,
        QPalette.ColorRole.ToolTipText: t.foreground,
        QPalette.ColorRole.PlaceholderText: t.muted,
        QPalette.ColorRole.Link: t.dir,
        QPalette.ColorRole.LinkVisited: t.link,
    }
    for role, color in roles.items():
        p.setColor(role, QColor(color))
    return p


def stylesheet(t: Theme) -> str:
    return f"""
    QWidget {{
        color: {t.foreground};
        background: {t.background};
        selection-background-color: {t.cursor_bg};
        selection-color: {t.cursor_fg};
    }}
    QToolTip {{
        color: {t.foreground};
        background: {t.surface};
        border: 1px solid {t.border};
    }}

    /* File lists and text views sit on the panel surface. */
    QTableView, QTextBrowser, QPlainTextEdit, QListWidget, QLabel#previewImage,
    QWidget#previewPage, QVideoWidget {{
        background: {t.surface};
        border: none;
        outline: none;
    }}
    QWidget#previewPage QLabel, QWidget#previewPage QWidget#mediaControls,
    QWidget#dialogBody, QWidget#dialogBody QLabel, QWidget#dialogBody QWidget#buttonRow,
    QWidget#dialogBody QWidget#dialogBody {{
        background: {t.surface};
    }}
    QHeaderView, QHeaderView::section {{
        background: {t.surface};
        color: {t.accent};
        border: none;
        font-weight: bold;
        padding: 0 1ex;
    }}

    QScrollBar:vertical {{
        background: {t.surface};
        width: 1ex;
        margin: 0;
    }}
    QScrollBar:horizontal {{
        background: {t.surface};
        height: 1ex;
        margin: 0;
    }}
    QScrollBar::handle {{
        background: {t.border};
        min-height: 2em;
        min-width: 2em;
    }}
    QScrollBar::handle:hover {{
        background: {t.muted};
    }}
    QScrollBar::add-line, QScrollBar::sub-line,
    QScrollBar::add-page, QScrollBar::sub-page {{
        background: none;
        border: none;
        width: 0;
        height: 0;
    }}

    /* Mode tabs above each panel. */
    QToolButton#modeTab {{
        background: transparent;
        color: {t.muted};
        border: none;
        padding: 0 1ex;
    }}
    QToolButton#modeTab:hover {{
        color: {t.foreground};
    }}
    QToolButton#modeTab:checked {{
        background: {t.border};
        color: {t.foreground};
        font-weight: bold;
    }}
    QWidget#side[active="true"] QToolButton#modeTab:checked {{
        background: {t.border_active};
        color: {t.surface};
    }}

    /* Function-key bar. */
    QWidget#footer, QWidget#footer QLabel {{
        background: {t.footer_bg};
        color: {t.footer_fg};
    }}
    QToolButton#fkey {{
        background: {t.footer_bg};
        color: {t.footer_fg};
        border: none;
        padding: 0 1ex 0 0;
    }}
    QToolButton#fkey:hover {{
        background: {t.border_active};
        color: {t.surface};
    }}

    /* Buttons and inputs in dialogs. */
    QPushButton {{
        background: {t.border};
        color: {t.foreground};
        border: none;
        padding: 0.2em 2ex;
        min-width: 8ex;
    }}
    QPushButton:focus {{
        background: {t.border_active};
        color: {t.surface};
        font-weight: bold;
    }}
    QPushButton:hover {{
        background: {t.muted};
        color: {t.surface};
    }}
    QLineEdit {{
        background: {t.background};
        color: {t.foreground};
        border: 1px solid {t.border};
        padding: 0.2em 0.5ex;
    }}
    QLineEdit:focus {{
        border: 1px solid {t.border_active};
    }}
    QComboBox {{
        background: {t.background};
        color: {t.foreground};
        border: 1px solid {t.border};
        padding: 0.2em 0.5ex;
    }}
    QComboBox:focus {{
        border: 1px solid {t.border_active};
    }}
    QComboBox::drop-down {{
        border: none;
        width: 2ex;
    }}
    QComboBox QAbstractItemView {{
        background: {t.surface};
        color: {t.foreground};
        selection-background-color: {t.cursor_bg};
        selection-color: {t.cursor_fg};
        border: 1px solid {t.border_active};
    }}
    QCheckBox {{
        background: {t.surface};
        color: {t.foreground};
        spacing: 1ex;
    }}
    QCheckBox::indicator {{
        width: 1.6ex;
        height: 1.6ex;
        border: 1px solid {t.border};
        background: {t.background};
    }}
    QCheckBox::indicator:checked {{
        background: {t.border_active};
        border: 1px solid {t.border_active};
    }}
    QCheckBox:focus {{
        color: {t.border_active};
    }}
    QMenu {{
        background: {t.surface};
        color: {t.foreground};
        border: 1px solid {t.border_active};
        padding: 0.3em 0;
    }}
    QMenu::item {{
        padding: 0.2em 2ex 0.2em 1.5ex;
        background: transparent;
    }}
    QMenu::item:selected {{
        background: {t.cursor_bg};
        color: {t.cursor_fg};
    }}
    QMenu::item:disabled {{
        color: {t.muted};
    }}
    QMenu::separator {{
        height: 1px;
        background: {t.border};
        margin: 0.3em 1ex;
    }}
    QTreeWidget {{
        background: {t.surface};
        border: 1px solid {t.border};
        outline: none;
    }}
    QTreeWidget::item {{
        padding: 0.1em 0.5ex;
    }}
    QTreeWidget::item:selected {{
        background: {t.cursor_bg};
        color: {t.cursor_fg};
    }}
    QProgressBar {{
        background: {t.background};
        color: {t.foreground};
        border: none;
        text-align: center;
        max-height: 1em;
    }}
    QProgressBar::chunk {{
        background: {t.border_active};
    }}
    QSlider::groove:horizontal {{
        background: {t.border};
        height: 0.3em;
    }}
    QSlider::sub-page:horizontal {{
        background: {t.border_active};
    }}
    QSlider::handle:horizontal {{
        background: {t.foreground};
        width: 1ex;
        margin: -0.3em 0;
    }}
    QListWidget::item {{
        padding: 0.1em 0.5ex;
    }}
    QListWidget::item:selected {{
        background: {t.cursor_bg};
        color: {t.cursor_fg};
    }}
    QLabel#dialogTitle {{
        color: {t.accent};
        font-weight: bold;
    }}
    QLabel#muted {{
        color: {t.muted};
    }}
    QLabel#toast {{
        background: {t.footer_bg};
        color: {t.foreground};
        border-left: 0.6ex solid {t.border_active};
        padding: 0.5em 2ex;
    }}
    QLabel#toast[severity="error"] {{
        border-left-color: {t.error};
    }}
    QLabel#toast[severity="warning"] {{
        border-left-color: {t.warning};
    }}
    """
