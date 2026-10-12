"""The settings window and the color scheme editor.

Settings apply and are saved as they change (no OK button): the window behind
the dialog is the preview. The scheme editor previews its colors live and
restores the scheme in use when cancelled.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import replace

from PySide6.QtCore import QRectF, QSize, Qt
from PySide6.QtGui import QColor, QFont, QFontDatabase, QFontMetricsF, QPainter, QPaintEvent
from PySide6.QtWidgets import (
    QCheckBox,
    QColorDialog,
    QComboBox,
    QDialog,
    QDoubleSpinBox,
    QGridLayout,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QListWidget,
    QPushButton,
    QSpinBox,
    QStackedWidget,
    QVBoxLayout,
    QWidget,
)

from orchard_commander import terminal, themes
from orchard_commander.config import Settings
from orchard_commander.ui.boxframe import BoxFrame
from orchard_commander.ui.dialogs import Dialog, confirm
from orchard_commander.ui.style import ThemeManager, default_monospace_font

Apply = Callable[..., None]  # apply(**settings changes): save them and update the app


def _body(widget: QWidget) -> QWidget:
    widget.setObjectName("dialogBody")  # dialog surface color (see the style sheet)
    return widget


def _heading(text: str, *, space_above: bool = False) -> QLabel:
    label = QLabel(text)
    label.setObjectName("dialogTitle")
    if space_above:
        label.setContentsMargins(0, label.fontMetrics().height() // 2, 0, 0)
    return label


class SettingsDialog(Dialog):
    """Settings by section; a list on the left picks the page."""

    def __init__(self, parent: QWidget, themes_: ThemeManager, settings: Settings, apply: Apply) -> None:
        super().__init__(parent, themes_, "Settings", width_chars=108)
        cw = self.fontMetrics().horizontalAdvance("M")
        row = QHBoxLayout()
        row.setSpacing(2 * cw)
        self.sections = QListWidget()
        self.sections.setFixedWidth(16 * cw)
        self.pages = _body(QStackedWidget())
        self.appearance = AppearancePage(themes_, settings, apply)
        self.preview = PreviewPage(settings, apply)
        self.terminal = TerminalPage(settings, apply)
        for title, page in (("Appearance", self.appearance), ("Preview", self.preview), ("Terminal", self.terminal)):
            self.sections.addItem(title)
            self.pages.addWidget(page)
        self.sections.currentRowChanged.connect(self.pages.setCurrentIndex)
        self.sections.setCurrentRow(0)
        row.addWidget(self.sections)
        row.addWidget(self.pages, 1)
        self.content.addLayout(row)
        close, = self.add_buttons(("Close", self.accept))
        close.setFocus()


# ---------------------------------------------------------------------------
# Preview
# ---------------------------------------------------------------------------


class PreviewPage(QWidget):
    def __init__(self, settings: Settings, apply: Apply) -> None:
        super().__init__()
        _body(self)
        self.settings = settings
        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.addWidget(_heading("Audio and video"))
        self.autoplay = QCheckBox("Start playing as soon as a file is selected (autoplay)")
        self.autoplay.setToolTip("Also in the player: the [x] Autoplay button, or the A key")
        self.autoplay.setChecked(settings.autoplay_enabled)
        self.autoplay.toggled.connect(
            lambda on: apply(extra={**self.settings.extra, "preview.media.autoplay": on}))
        layout.addWidget(self.autoplay)
        self.play_next = QCheckBox("Play the next file when one ends")
        self.play_next.setToolTip("Selects the next audio or video file in the panel and plays it. "
                                  "Off: videos loop, audio stops at the end.")
        self.play_next.setChecked(settings.play_next_enabled)
        self.play_next.toggled.connect(lambda on: apply(play_next=on))
        layout.addWidget(self.play_next)
        layout.addStretch(1)


# ---------------------------------------------------------------------------
# Terminal
# ---------------------------------------------------------------------------


class TerminalPage(QWidget):
    def __init__(self, settings: Settings, apply: Apply) -> None:
        super().__init__()
        _body(self)
        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.addWidget(_heading("Shell"))
        self.shell = QLineEdit(settings.terminal_shell)
        self.shell.setPlaceholderText(" ".join(terminal.default_shell()) + "   (your default)")
        self.shell.editingFinished.connect(lambda: apply(terminal_shell=self.shell.text().strip()))
        layout.addWidget(self.shell)
        hint = QLabel("Used for new terminal sessions. Empty: your login shell (PowerShell on Windows).")
        hint.setObjectName("muted")
        hint.setWordWrap(True)
        layout.addWidget(hint)
        layout.addWidget(_heading("Panels", space_above=True))
        self.link = QCheckBox("Link each side's terminal and panel folders")
        self.link.setToolTip("cd in the terminal moves the panel; moving the panel types a cd in the shell "
                             "(once it's back at its prompt)")
        self.link.setChecked(settings.terminal_link_enabled)
        self.link.toggled.connect(lambda on: apply(terminal_link=on))
        layout.addWidget(self.link)
        layout.addStretch(1)


# ---------------------------------------------------------------------------
# Appearance
# ---------------------------------------------------------------------------


class AppearancePage(QWidget):
    def __init__(self, themes_: ThemeManager, settings: Settings, apply: Apply) -> None:
        super().__init__()
        _body(self)
        self.themes = themes_
        self.settings = settings
        self._apply = apply
        self._families: dict[bool, list[str]] = {}
        cw = self.fontMetrics().horizontalAdvance("M")

        form = QGridLayout()
        form.setHorizontalSpacing(cw)
        form.setVerticalSpacing(self.fontMetrics().height() // 4)
        form.setColumnStretch(1, 1)
        r = 0

        def add(label: str, *widgets: QWidget) -> None:
            nonlocal r
            form.addWidget(QLabel(label), r, 0)
            for i, w in enumerate(widgets):
                form.addWidget(w, r, 1 + i)
            r += 1

        def heading(text: str) -> None:
            nonlocal r
            form.addWidget(_heading(text, space_above=r > 0), r, 0, 1, 3)
            r += 1

        heading("Color scheme")
        self.follow_system = QCheckBox("Follow the system's light / dark mode")
        self.follow_system.toggled.connect(lambda on: self.apply(follow_system_appearance=on))
        form.addWidget(self.follow_system, r, 0, 1, 3)
        r += 1
        self.scheme_rows: dict[str, tuple[QLabel, QComboBox, QPushButton]] = {}
        for key, label in (("color_scheme", "Scheme"), ("light_scheme", "Light mode"), ("dark_scheme", "Dark mode")):
            combo = _combo()
            combo.activated.connect(lambda _=0, k=key, c=combo: self.apply(**{k: c.currentData()}))
            edit = QPushButton("Edit…")
            edit.setAutoDefault(False)
            edit.clicked.connect(lambda _=False, k=key: self.edit_scheme(k))
            text = QLabel(label)
            form.addWidget(text, r, 0)
            form.addWidget(combo, r, 1)
            form.addWidget(edit, r, 2)
            self.scheme_rows[key] = (text, combo, edit)
            r += 1

        heading("Font")
        self.family = _combo()
        self.family.activated.connect(lambda _=0: self.apply(font_family=self.family.currentData(), font_style=""))
        add("Typeface", self.family)
        self.all_fonts = QCheckBox("Include proportional fonts")
        self.all_fonts.setToolTip("Proportional fonts don't keep columns aligned")
        self.all_fonts.toggled.connect(lambda _: self.refresh())
        form.addWidget(self.all_fonts, r, 1, 1, 2)
        r += 1
        self.style_combo = _combo()
        self.style_combo.activated.connect(lambda _=0: self.apply(font_style=self.style_combo.currentText()))
        add("Style", self.style_combo)
        self.size = QSpinBox()
        self.size.setRange(*themes.FONT_SIZE_RANGE)
        self.size.setSuffix(" pt")
        self.size.valueChanged.connect(lambda v: self.apply(font_size=v))
        add("Size", self.size)
        self.spacing = QDoubleSpinBox()
        self.spacing.setRange(*themes.LINE_SPACING_RANGE)
        self.spacing.setSingleStep(0.05)
        self.spacing.setDecimals(2)
        self.spacing.setSuffix(" ×")
        self.spacing.valueChanged.connect(lambda v: self.apply(line_spacing=round(v, 2)))
        add("Line spacing", self.spacing)

        heading("Window")
        self.frame = _combo()
        for key, label in themes.FRAME_STYLES.items():
            self.frame.addItem(label, key)
        self.frame.activated.connect(lambda _=0: self.apply(frame_style=self.frame.currentData()))
        add("Frames", self.frame)
        self.menu_bar = QCheckBox("Always show the menu bar")
        self.menu_bar.setToolTip("Otherwise F9 shows it")
        self.menu_bar.toggled.connect(lambda on: self.apply(menu_bar_visible=on))
        form.addWidget(self.menu_bar, r, 0, 1, 3)

        for w in (self.size, self.spacing):  # don't let them stretch across the column
            w.setFixedWidth(12 * cw)
        reset = QPushButton("Restore defaults")
        reset.setAutoDefault(False)
        reset.clicked.connect(self.restore_defaults)

        side = QVBoxLayout()
        side.addWidget(_heading("Sample"))
        side.addWidget(SamplePanel(themes_))
        side.addStretch(1)
        side.addWidget(reset, 0, Qt.AlignmentFlag.AlignRight)
        layout = QHBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(2 * cw)
        layout.addLayout(_top_aligned(form), 1)
        layout.addLayout(side)
        self.refresh()

    def apply(self, **changes: object) -> None:
        self._apply(**changes)
        self.refresh()

    # -- showing the current values ----------------------------------------

    def refresh(self) -> None:
        """Show the saved settings (after any change, so dependent widgets follow)."""
        a = self.settings.appearance()
        widgets = [self.follow_system, self.family, self.all_fonts, self.style_combo, self.size, self.spacing,
                   self.frame, self.menu_bar] + [combo for _, combo, _ in self.scheme_rows.values()]
        for w in widgets:
            w.blockSignals(True)
        try:
            self.follow_system.setChecked(a.follow_system_appearance)
            names = list(themes.all_schemes())
            builtin = [n for n in names if themes.is_builtin(n)]
            custom = [n for n in names if not themes.is_builtin(n)]
            for key, (label, combo, edit) in self.scheme_rows.items():
                visible = (key == "color_scheme") != a.follow_system_appearance
                for w in (label, combo, edit):
                    w.setVisible(visible)
                _fill(combo, [(n, n) for n in builtin] + ([None] if custom else []) + [(n, n) for n in custom],
                      getattr(a, key))
            self._fill_fonts(a)
            self.size.setValue(a.font_size)
            self.spacing.setValue(a.line_spacing)
            self.frame.setCurrentIndex(max(0, self.frame.findData(a.frame_style)))
            self.menu_bar.setChecked(bool(self.settings.menu_bar_visible))
        finally:
            for w in widgets:
                w.blockSignals(False)

    def _fill_fonts(self, a: themes.Appearance) -> None:
        show_all = self.all_fonts.isChecked()
        if show_all not in self._families:
            self._families[show_all] = [
                f for f in QFontDatabase.families()
                if not QFontDatabase.isPrivateFamily(f) and (show_all or QFontDatabase.isFixedPitch(f))
            ]
        families = self._families[show_all]
        if a.font_family and a.font_family not in families and a.font_family in QFontDatabase.families():
            families = [*families, a.font_family]  # a proportional font chosen earlier
        automatic = default_monospace_font(a.font_size).family()
        _fill(self.family, [(f"Automatic ({automatic})", "")] + [(f, f) for f in families], a.font_family)
        family = a.font_family if a.font_family in families else automatic
        styles = QFontDatabase.styles(family) or ["Regular"]
        current = a.font_style if a.font_style in styles else self.themes.font.styleName() or styles[0]
        _fill(self.style_combo, [(s, s) for s in styles], current)

    def restore_defaults(self) -> None:
        self.apply(color_scheme="", follow_system_appearance=None, light_scheme="", dark_scheme="", font_family="",
                   font_style="", font_size=None, line_spacing=None, frame_style="", menu_bar_visible=None)

    # -- color schemes -----------------------------------------------------

    def edit_scheme(self, key: str) -> None:
        name = getattr(self.settings.appearance(), key)
        result = SchemeEditor(self.window(), self.themes, themes.get_scheme(name)).edit()
        if result is None:
            return
        action, new_name = result
        if action == "saved":
            self.apply(**{key: new_name})
        else:  # deleted: whatever used it goes back to the default
            used = [k for k in self.scheme_rows if getattr(self.settings, k) == new_name]
            self.apply(**{k: "" for k in used})
            self.refresh()


def _combo() -> QComboBox:
    combo = QComboBox()
    combo.setMaxVisibleItems(20)
    combo.setSizeAdjustPolicy(QComboBox.SizeAdjustPolicy.AdjustToMinimumContentsLengthWithIcon)
    combo.setMinimumContentsLength(18)
    return combo


def _top_aligned(grid: QGridLayout) -> QVBoxLayout:
    """`grid` at its natural height (a stretch on an empty grid row has no effect)."""
    box = QVBoxLayout()
    box.addLayout(grid)
    box.addStretch(1)
    return box


def _fill(combo: QComboBox, items: list[tuple[str, str] | None], current: str) -> None:
    """Replace the items (None: a separator) and select the one whose data is `current`."""
    combo.clear()
    for item in items:
        if item is None:
            combo.insertSeparator(combo.count())
        else:
            combo.addItem(*item)
    combo.setCurrentIndex(max(0, combo.findData(current)))


# ---------------------------------------------------------------------------
# Sample panel
# ---------------------------------------------------------------------------

# (name, kind, size): kinds pick the color as in the file panel.
SAMPLE_ROWS = [
    ("..", "dir", "<UP>"),
    ("src", "dir", "<DIR>"),
    ("build.sh", "exec", "1.2K"),
    ("latest", "link", "<LNK>"),
    (".env", "hidden", "312"),
    ("notes.md", "marked", "4.0K"),
    ("README.md", "file", "8.1K"),
    ("setup.cfg", "file", "640"),
]
SAMPLE_CURSOR = 6
SAMPLE_KEYS = [("F5", "Copy"), ("F6", "Move"), ("F7", "MkDir")]


class SamplePanel(QWidget):
    """A small fake file panel and key bar in the current appearance."""

    def __init__(self, themes_: ThemeManager) -> None:
        super().__init__()
        _body(self)
        self.frame = BoxFrame(themes_)
        self.frame.set_title("~/projects/orchard")
        self.frame.set_subtitle("1 marked")
        self.frame.set_active(True)
        self.frame.body.addWidget(SampleRows(themes_))
        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(0)
        layout.addWidget(self.frame)
        layout.addWidget(SampleKeyBar(themes_))


class _Sample(QWidget):
    def __init__(self, themes_: ThemeManager) -> None:
        super().__init__()
        self.themes = themes_
        themes_.changed.connect(self._restyle)

    def _restyle(self) -> None:
        self.updateGeometry()
        self.update()

    def _cell(self) -> tuple[float, float]:
        fm = QFontMetricsF(self.themes.font)
        return fm.horizontalAdvance("M"), fm.height()


class SampleRows(_Sample):
    def sizeHint(self) -> QSize:
        cw, _ = self._cell()
        return QSize(round(30 * cw), (len(SAMPLE_ROWS) + 1) * (self.themes.row_height() + 2))

    def minimumSizeHint(self) -> QSize:
        return self.sizeHint()

    def paintEvent(self, event: QPaintEvent) -> None:
        t = self.themes.colors
        cw, _ = self._cell()
        row_h = self.themes.row_height() + 2
        p = QPainter(self)
        p.fillRect(self.rect(), QColor(t.surface))
        header = QFont(self.themes.font)
        header.setBold(True)
        p.setFont(header)
        p.setPen(QColor(t.accent))
        line = QRectF(cw / 2, 0, self.width() - cw, row_h)
        p.drawText(line, Qt.AlignmentFlag.AlignVCenter | Qt.AlignmentFlag.AlignLeft, "Name")
        p.drawText(line, Qt.AlignmentFlag.AlignVCenter | Qt.AlignmentFlag.AlignRight, "Size")
        for i, (name, kind, size) in enumerate(SAMPLE_ROWS):
            line = QRectF(0, (i + 1) * row_h, self.width(), row_h)
            color = {"file": t.foreground}.get(kind) or getattr(t, kind)
            if i == SAMPLE_CURSOR:
                p.fillRect(line, QColor(t.cursor_bg))
                color = t.cursor_fg
            font = QFont(self.themes.font)
            font.setBold(kind in ("dir", "marked"))
            font.setItalic(kind == "link")
            p.setFont(font)
            p.setPen(QColor(color))
            text = line.adjusted(cw / 2, 0, -cw / 2, 0)
            mark = "✓" if kind == "marked" else " "
            p.drawText(text, Qt.AlignmentFlag.AlignVCenter | Qt.AlignmentFlag.AlignLeft, mark + name)
            p.drawText(text, Qt.AlignmentFlag.AlignVCenter | Qt.AlignmentFlag.AlignRight, size)
        p.end()


class SampleKeyBar(_Sample):
    def sizeHint(self) -> QSize:
        cw, ch = self._cell()
        return QSize(round(30 * cw), round(ch) + 4)

    def minimumSizeHint(self) -> QSize:
        return self.sizeHint()

    def paintEvent(self, event: QPaintEvent) -> None:
        t = self.themes.colors
        cw, _ = self._cell()
        p = QPainter(self)
        p.fillRect(self.rect(), QColor(t.footer_bg))
        bold = QFont(self.themes.font)
        bold.setBold(True)
        x = cw
        for key, label in SAMPLE_KEYS:
            for text, color, font in ((key, t.footer_key, bold), (label, t.footer_fg, self.themes.font)):
                p.setFont(font)
                p.setPen(QColor(color))
                width = QFontMetricsF(font).horizontalAdvance(text)
                p.drawText(QRectF(x, 0, width, self.height()), Qt.AlignmentFlag.AlignVCenter, text)
                x += width + cw
            x += cw
        p.end()


# ---------------------------------------------------------------------------
# Color scheme editor
# ---------------------------------------------------------------------------


class SchemeEditor(Dialog):
    """Edit a scheme's colors with a live preview. Built-in schemes are saved as a copy."""

    def __init__(self, parent: QWidget, themes_: ThemeManager, scheme: themes.ColorScheme) -> None:
        super().__init__(parent, themes_, "Edit color scheme", width_chars=96)
        self.builtin = themes.is_builtin(scheme.name)
        self.original = None if self.builtin else scheme.name
        self.scheme = scheme
        self.result_: tuple[str, str] | None = None
        cw = self.fontMetrics().horizontalAdvance("M")
        if self.builtin:
            self.add_label(f"'{scheme.name}' is built in: your changes are saved as a new color scheme.", muted=True)

        top = _body(QWidget())
        grid = QGridLayout(top)
        grid.setContentsMargins(0, 0, 0, 0)
        grid.setHorizontalSpacing(cw)
        self.name = QLineEdit(f"{scheme.name} (custom)" if self.builtin else scheme.name)
        grid.addWidget(QLabel("Name"), 0, 0)
        grid.addWidget(self.name, 0, 1, 1, 3)
        self.dark = QCheckBox("Dark scheme")
        self.dark.setChecked(scheme.dark)
        self.dark.toggled.connect(lambda on: self._set(dark=on))
        grid.addWidget(self.dark, 1, 1)
        self.syntax = _combo()
        from pygments.styles import get_all_styles

        self.syntax.addItems(sorted(get_all_styles()))
        self.syntax.setCurrentText(scheme.pygments_style)
        self.syntax.activated.connect(lambda _=0: self._set(pygments_style=self.syntax.currentText()))
        grid.addWidget(QLabel("Syntax colors"), 1, 2, Qt.AlignmentFlag.AlignRight)
        grid.addWidget(self.syntax, 1, 3)
        grid.setColumnStretch(1, 1)
        self.content.addWidget(top)

        colors = _body(QWidget())
        columns = QHBoxLayout(colors)
        columns.setContentsMargins(0, self.fontMetrics().height() // 2, 0, 0)
        columns.setSpacing(3 * cw)
        self.swatches: dict[str, tuple[QPushButton, QLineEdit]] = {}
        groups: dict[str, list[tuple[str, str]]] = {}
        for role, label, group in themes.COLOR_ROLES:
            groups.setdefault(group, []).append((role, label))
        # Two columns: General + Cursor | Files + Messages + Key bar.
        for column_groups in (("General", "Cursor"), ("Files", "Messages", "Key bar")):
            col = QGridLayout()
            col.setHorizontalSpacing(cw)
            col.setVerticalSpacing(2)
            row = 0
            for group in column_groups:
                col.addWidget(_heading(group, space_above=row > 0), row, 0, 1, 3)
                row += 1
                for role, label in groups[group]:
                    swatch = QPushButton()
                    swatch.setAutoDefault(False)
                    swatch.setFixedWidth(5 * cw)
                    swatch.clicked.connect(lambda _=False, r=role, lb=label: self._pick(r, lb))
                    hex_edit = QLineEdit(getattr(scheme, role))
                    hex_edit.setFixedWidth(10 * cw)
                    hex_edit.setMaxLength(7)
                    hex_edit.textEdited.connect(lambda text, r=role: self._typed(r, text))
                    col.addWidget(QLabel(label), row, 0)
                    col.addWidget(swatch, row, 1)
                    col.addWidget(hex_edit, row, 2)
                    self.swatches[role] = (swatch, hex_edit)
                    row += 1
            col.setColumnStretch(0, 1)
            columns.addLayout(_top_aligned(col), 1)
        self.content.addWidget(colors)

        self.error = self.add_label("")
        self.error.hide()
        buttons = [("Save", self.save)]
        if not self.builtin:
            buttons.append(("Delete", self.delete))
        buttons.append(("Cancel", self.reject))
        self.add_buttons(*buttons)
        self._paint_swatches()
        self.name.setFocus()
        self.name.selectAll()

    # -- editing -----------------------------------------------------------

    def _set(self, **changes: object) -> None:
        self.scheme = replace(self.scheme, **changes)
        self.themes.preview_colors(self.scheme)
        self._paint_swatches()

    def _typed(self, role: str, text: str) -> None:
        if themes.is_color(text):
            self._set(**{role: text.lower()})

    def _pick(self, role: str, label: str) -> None:
        color = QColorDialog.getColor(QColor(getattr(self.scheme, role)), self, label)
        if color.isValid():
            self.swatches[role][1].setText(color.name())
            self._set(**{role: color.name()})

    def _paint_swatches(self) -> None:
        t = self.scheme
        for role, (swatch, _) in self.swatches.items():
            swatch.setStyleSheet(
                f"QPushButton {{ background: {getattr(t, role)}; border: 1px solid {t.muted}; }}"
                f"QPushButton:focus {{ border: 2px solid {t.border_active}; }}"
            )

    # -- results -----------------------------------------------------------

    def save(self) -> None:
        scheme = replace(self.scheme, name=self.name.text().strip())
        try:
            themes.save_user_scheme(scheme, replaces=self.original)
        except (ValueError, OSError) as exc:
            self.error.setText(str(exc))
            self.error.show()
            return
        self.result_ = ("saved", scheme.name)
        self.accept()

    def delete(self) -> None:
        assert self.original is not None
        if confirm(self, self.themes, "Delete color scheme", f"Delete '{self.original}'?", "Delete"):
            themes.delete_user_scheme(self.original)
            self.result_ = ("deleted", self.original)
            self.accept()

    def done(self, code: int) -> None:
        self.themes.preview_colors(None)
        super().done(code)

    def edit(self) -> tuple[str, str] | None:
        """("saved", name), ("deleted", name), or None when cancelled."""
        self.themes.preview_colors(self.scheme)
        return self.result_ if self.exec() == QDialog.DialogCode.Accepted else None

