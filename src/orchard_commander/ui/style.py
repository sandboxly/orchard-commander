"""Theme application: fonts, the Qt style sheet, and change notification."""

from __future__ import annotations

from PySide6.QtCore import QObject, Qt, Signal
from PySide6.QtGui import QColor, QFont, QFontDatabase, QFontMetrics, QGuiApplication, QPalette
from PySide6.QtWidgets import QApplication

from orchard_commander.themes import Appearance, ColorScheme, get_scheme

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


def default_monospace_font(point_size: int) -> QFont:
    """The first installed of MONO_FAMILIES, else the system's fixed font."""
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


def resolve_font(family: str, style: str, point_size: int) -> QFont:
    """The chosen font; the default monospace font if `family` isn't installed (any more)."""
    if not family or family not in QFontDatabase.families():
        return default_monospace_font(point_size)
    if style and style in QFontDatabase.styles(family):
        font = QFontDatabase.font(family, style, point_size)
    else:
        font = QFont(family, point_size)
    font.setStyleHint(QFont.StyleHint.Monospace)  # for glyphs the family lacks
    return font


def system_is_dark() -> bool:
    """The OS light/dark mode; dark when the platform doesn't say."""
    return QGuiApplication.styleHints().colorScheme() != Qt.ColorScheme.Light


class ThemeManager(QObject):
    """Holds the active appearance, its colors and font; widgets listen to `changed` to restyle."""

    changed = Signal()

    def __init__(self, appearance: Appearance | None = None) -> None:
        super().__init__()
        self.appearance = appearance or Appearance()
        self._preview: ColorScheme | None = None
        self._resolve()
        QGuiApplication.styleHints().colorSchemeChanged.connect(self._system_mode_changed)

    def _resolve(self) -> None:
        a = self.appearance
        self.colors: ColorScheme = self._preview or get_scheme(a.scheme_name(system_is_dark()))
        self.font: QFont = resolve_font(a.font_family, a.font_style, a.font_size)

    @property
    def font_size(self) -> int:
        return self.appearance.font_size

    @property
    def frame_style(self) -> str:
        return self.appearance.frame_style

    def row_height(self) -> int:
        """Height of a file list row: the font's line height times the line spacing."""
        return round(QFontMetrics(self.font).height() * self.appearance.line_spacing)

    def set_appearance(self, appearance: Appearance) -> None:
        self.appearance = appearance
        self._resolve()
        self.apply()

    def preview_colors(self, scheme: ColorScheme | None) -> None:
        """Show `scheme` until called with None (pickers and the scheme editor)."""
        self._preview = scheme
        self._resolve()
        self.apply()

    def _system_mode_changed(self) -> None:
        if self.appearance.follow_system_appearance and self._preview is None:
            self._resolve()
            self.apply()

    def apply(self) -> None:
        app = QApplication.instance()
        assert isinstance(app, QApplication)
        app.setFont(self.font)
        app.setPalette(palette(self.colors))
        line = QFontMetrics(self.font).height()
        padding = max(0, round(line * 0.1) + (self.row_height() - line) // 2)
        app.setStyleSheet(stylesheet(self.colors, item_padding=padding))
        self.changed.emit()


def palette(t: ColorScheme) -> QPalette:
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


def stylesheet(t: ColorScheme, item_padding: int = 1) -> str:
    """`item_padding`: vertical padding of list items in pixels (line spacing)."""
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
    QToolButton#fkey:checked {{
        color: {t.accent};
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
    QLineEdit, QSpinBox, QDoubleSpinBox {{
        background: {t.background};
        color: {t.foreground};
        border: 1px solid {t.border};
        padding: 0.2em 0.5ex;
    }}
    QLineEdit:focus, QSpinBox:focus, QDoubleSpinBox:focus {{
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
    QMenuBar {{
        background: {t.footer_bg};
        color: {t.footer_fg};
        border: none;
    }}
    QMenuBar::item {{
        background: transparent;
        padding: 0 1ex;
    }}
    QMenuBar::item:selected {{
        background: {t.cursor_bg};
        color: {t.cursor_fg};
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
        padding: {item_padding}px 0.5ex;
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
        padding: {item_padding}px 0.5ex;
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
