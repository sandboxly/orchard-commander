"""Modal dialogs, drawn as terminal-style boxes."""

from __future__ import annotations

import datetime as dt
from collections.abc import Callable
from pathlib import Path

from PySide6.QtCore import QEvent, QObject, Qt, Signal
from PySide6.QtGui import QFontMetrics, QKeyEvent, QKeySequence, QShortcut
from PySide6.QtWidgets import (
    QDialog,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QListWidget,
    QListWidgetItem,
    QProgressBar,
    QPushButton,
    QVBoxLayout,
    QWidget,
)

from orchard_commander.fs import Conflict, Progress, human_size
from orchard_commander.ui.boxframe import BoxFrame
from orchard_commander.ui.style import ThemeManager


class Dialog(QDialog):
    def __init__(self, parent: QWidget, themes: ThemeManager, title: str, width_chars: int = 64) -> None:
        super().__init__(parent, Qt.WindowType.Dialog | Qt.WindowType.FramelessWindowHint)
        self.setModal(True)
        self.themes = themes
        self.frame = BoxFrame(themes, heavy=True)
        self.frame.set_active(True)
        self.frame.set_title(title)
        outer = QVBoxLayout(self)
        outer.setContentsMargins(0, 0, 0, 0)
        outer.addWidget(self.frame)
        body = QWidget()
        body.setObjectName("dialogBody")
        self.frame.body.addWidget(body)
        self.content = QVBoxLayout(body)
        self.content.setContentsMargins(0, 0, 0, 0)
        self.width_chars = width_chars
        self._fit_width()
        themes.changed.connect(self._fit_width)  # the font may change while open (settings)

    def _fit_width(self) -> None:
        cw = QFontMetrics(self.themes.font).horizontalAdvance("M")
        self.setMinimumWidth(cw * self.width_chars)

    def keyPressEvent(self, event: QKeyEvent) -> None:
        # Enter presses the highlighted button. Buttons aren't auto-default (Enter in
        # a text field means OK), so QDialog would press the default button instead:
        # "Trash" with Cancel highlighted.
        if event.key() in (Qt.Key.Key_Return, Qt.Key.Key_Enter):
            focused = self.focusWidget()
            if isinstance(focused, QPushButton):
                if focused.isEnabled():
                    focused.click()
                return
        super().keyPressEvent(event)

    def add_label(self, text: str, *, muted: bool = False) -> QLabel:
        label = QLabel(text)
        label.setTextFormat(Qt.TextFormat.PlainText)
        label.setWordWrap(True)
        if muted:
            label.setObjectName("muted")
        self.content.addWidget(label)
        return label

    def add_buttons(self, *buttons: tuple[str, Callable[[], None]], default: int = 0) -> list[QPushButton]:
        row_widget = QWidget()
        row_widget.setObjectName("buttonRow")
        row = QHBoxLayout(row_widget)
        row.setContentsMargins(0, self.fontMetrics().height(), 0, 0)
        row.addStretch(1)
        result = []
        for i, (text, slot) in enumerate(buttons):
            b = QPushButton(text)
            b.setAutoDefault(False)
            b.setDefault(i == default)
            b.clicked.connect(slot)
            row.addWidget(b)
            result.append(b)
        row.addStretch(1)
        self.content.addWidget(row_widget)
        return result


def ask_text(
    parent: QWidget, themes: ThemeManager, title: str, message: str, value: str = "", ok_label: str = "OK"
) -> str | None:
    d = Dialog(parent, themes, title)
    d.add_label(message)
    edit = QLineEdit(value)
    edit.selectAll()
    d.content.addWidget(edit)
    d.add_buttons((ok_label, d.accept), ("Cancel", d.reject))
    edit.returnPressed.connect(d.accept)
    edit.setFocus()
    if d.exec() != QDialog.DialogCode.Accepted:
        return None
    return edit.text().strip() or None


def confirm(parent: QWidget, themes: ThemeManager, title: str, message: str, ok_label: str = "OK") -> bool:
    d = Dialog(parent, themes, title, width_chars=52)
    d.add_label(message)
    ok, _ = d.add_buttons((ok_label, d.accept), ("Cancel", d.reject))
    ok.setFocus()
    return d.exec() == QDialog.DialogCode.Accepted


def _describe_file(p: Path) -> str:
    try:
        st = p.stat()
    except OSError:
        return str(p)
    when = dt.datetime.fromtimestamp(st.st_mtime).strftime("%Y-%m-%d %H:%M")
    size = "<DIR>" if p.is_dir() else human_size(st.st_size)
    return f"{p}\n    {size}, {when}"


def ask_conflict(parent: QWidget, themes: ThemeManager, src: Path, dst: Path) -> tuple[Conflict, bool]:
    """Destination exists. Returns (choice, apply_to_all)."""
    d = Dialog(parent, themes, "File exists", width_chars=72)
    d.add_label(f"Overwrite\n  {_describe_file(dst)}\nwith\n  {_describe_file(src)}?")
    answer: list[tuple[Conflict, bool]] = [(Conflict.CANCEL, False)]

    def choose(choice: Conflict, all_: bool) -> Callable[[], None]:
        def slot() -> None:
            answer[0] = (choice, all_)
            d.accept()

        return slot

    options = [
        ("&Overwrite", Conflict.OVERWRITE, False, "O"),
        ("&All", Conflict.OVERWRITE, True, "A"),
        ("&Skip", Conflict.SKIP, False, "S"),
        ("S&kip all", Conflict.SKIP, True, "K"),
        ("Cancel", Conflict.CANCEL, False, None),
    ]
    buttons = d.add_buttons(*((label, choose(c, a)) for label, c, a, _ in options), default=2)
    for label, c, a, key in options:
        if key:
            QShortcut(QKeySequence(key), d, activated=choose(c, a))
    buttons[2].setFocus()
    d.exec()
    return answer[0]


class ProgressDialog(Dialog):
    cancel_requested = Signal()

    def __init__(self, parent: QWidget, themes: ThemeManager, title: str) -> None:
        super().__init__(parent, themes, title, width_chars=72)
        self.cancelled = False
        self.current = self.add_label("Preparing…")
        self.bar = QProgressBar()
        self.bar.setTextVisible(False)
        self.content.addWidget(self.bar)
        self.counts = self.add_label("", muted=True)
        self.add_buttons(("Cancel", self.cancel))

    def update_progress(self, p: Progress) -> None:
        fm = self.current.fontMetrics()
        self.current.setText(fm.elidedText(str(p.current), Qt.TextElideMode.ElideMiddle, self.current.width()))
        # QProgressBar is int-based; scale bytes to permille.
        self.bar.setRange(0, 1000)
        self.bar.setValue(int(1000 * p.bytes_done / p.bytes_total) if p.bytes_total else 0)
        self.counts.setText(
            f"{p.files_done}/{p.files_total} files · {human_size(p.bytes_done)} / {human_size(p.bytes_total)}"
        )

    def cancel(self) -> None:
        self.cancelled = True
        self.current.setText("Cancelling…")
        self.cancel_requested.emit()

    def reject(self) -> None:  # Escape
        self.cancel()


class Picker(Dialog):
    """Filterable list. Type to filter, ↑/↓ to move, Enter to choose."""

    def __init__(
        self,
        parent: QWidget,
        themes: ThemeManager,
        title: str,
        items: list[tuple[str, str, object]],
        *,
        current: object = None,
        on_highlight: Callable[[object], None] | None = None,
        width_chars: int = 64,
    ) -> None:
        super().__init__(parent, themes, title, width_chars=width_chars)
        self.filter = QLineEdit()
        self.filter.setPlaceholderText("Type to filter…")
        self.content.addWidget(self.filter)
        self.list = QListWidget()
        self.list.setFocusPolicy(Qt.FocusPolicy.NoFocus)
        self.content.addWidget(self.list)
        fm = self.fontMetrics()
        self.list.setMinimumHeight(fm.height() * min(max(len(items), 4), 16) + 8)
        self._on_highlight = on_highlight
        for label, hint, value in items:
            text = f"{label:<28} {hint}" if hint else label
            item = QListWidgetItem(text)
            item.setData(Qt.ItemDataRole.UserRole, value)
            self.list.addItem(item)
            if value == current:
                self.list.setCurrentItem(item)
        if self.list.currentRow() < 0 and self.list.count():
            self.list.setCurrentRow(0)
        self.filter.textChanged.connect(self._apply_filter)
        self.filter.installEventFilter(self)
        self.list.currentItemChanged.connect(self._highlighted)
        self.list.itemActivated.connect(lambda _: self.accept())
        self.list.itemClicked.connect(lambda _: self.accept())
        self.filter.returnPressed.connect(self.accept)
        self.filter.setFocus()

    def _apply_filter(self, text: str) -> None:
        needle = text.casefold().split()
        first = None
        for i in range(self.list.count()):
            item = self.list.item(i)
            hay = item.text().casefold()
            visible = all(word in hay for word in needle)
            item.setHidden(not visible)
            if visible and first is None:
                first = item
        current = self.list.currentItem()
        if first is not None and (current is None or current.isHidden()):
            self.list.setCurrentItem(first)

    def _highlighted(self, item: QListWidgetItem | None) -> None:
        if item is not None and self._on_highlight:
            self._on_highlight(item.data(Qt.ItemDataRole.UserRole))

    def eventFilter(self, obj: QObject, event: QEvent) -> bool:
        if obj is self.filter and event.type() == QEvent.Type.KeyPress:
            assert isinstance(event, QKeyEvent)
            step = {Qt.Key.Key_Down: 1, Qt.Key.Key_Up: -1, Qt.Key.Key_PageDown: 10, Qt.Key.Key_PageUp: -10}
            if event.key() in step:
                self._move(step[Qt.Key(event.key())])
                return True
        return super().eventFilter(obj, event)

    def _move(self, delta: int) -> None:
        visible = [i for i in range(self.list.count()) if not self.list.item(i).isHidden()]
        if not visible:
            return
        row = self.list.currentRow()
        pos = visible.index(row) if row in visible else 0
        self.list.setCurrentRow(visible[max(0, min(pos + delta, len(visible) - 1))])

    def choose(self) -> object | None:
        if self.exec() != QDialog.DialogCode.Accepted:
            return None
        item = self.list.currentItem()
        return item.data(Qt.ItemDataRole.UserRole) if item is not None and not item.isHidden() else None
