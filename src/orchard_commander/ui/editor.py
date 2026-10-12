"""The text editor (Total Commander's F4): one window per file, on any file system.

Highlighting comes from Pygments in the app's color scheme. Files are read and
written through the VFS on a worker thread; local files are saved to a
temporary name first and then swapped in.
"""

from __future__ import annotations

import os
import tempfile
from collections.abc import Callable
from pathlib import Path

from PySide6.QtCore import QFileSystemWatcher, QRect, QSize, Qt, QTimer, Signal
from PySide6.QtGui import (
    QColor,
    QFont,
    QKeySequence,
    QPainter,
    QPaintEvent,
    QResizeEvent,
    QShortcut,
    QSyntaxHighlighter,
    QTextCharFormat,
    QTextCursor,
    QTextDocument,
    QTextFormat,
)
from PySide6.QtWidgets import (
    QCheckBox,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QPlainTextEdit,
    QPushButton,
    QTextEdit,
    QVBoxLayout,
    QWidget,
)

from orchard_commander import textfile, vfs
from orchard_commander.ui import dialogs, tasks
from orchard_commander.ui.boxframe import BoxFrame
from orchard_commander.ui.style import ThemeManager

MAX_HIGHLIGHT_CHARS = 2_000_000  # beyond this, plain text (re-lexing would lag)
ASK_ABOVE_BYTES = 20 * 1024**2


# ---------------------------------------------------------------------------
# Syntax highlighting
# ---------------------------------------------------------------------------


class PygmentsHighlighter(QSyntaxHighlighter):
    """Lexes the whole document (after a short pause in typing) and colors each line from that."""

    def __init__(self, document: QTextDocument, file_name: str, style_name: str) -> None:
        super().__init__(document)
        self._lexer = _lexer_for(file_name, document.toPlainText())
        self._formats: dict = {}
        self._lines: dict[int, list[tuple[int, int, QTextCharFormat]]] = {}
        self.set_style(style_name)
        self._timer = QTimer(self, singleShot=True, interval=200)
        self._timer.timeout.connect(self.relex)
        document.contentsChange.connect(lambda *_: self._timer.start())
        self.relex()

    @property
    def language(self) -> str:
        return self._lexer.name if self._lexer is not None else "Plain text"

    def set_style(self, style_name: str) -> None:
        from pygments.styles import get_style_by_name
        from pygments.util import ClassNotFound

        try:
            self._style = get_style_by_name(style_name)
        except ClassNotFound:
            self._style = get_style_by_name("default")
        self._formats.clear()

    def _format(self, token) -> QTextCharFormat:
        fmt = self._formats.get(token)
        if fmt is None:
            spec = self._style.style_for_token(token)
            fmt = QTextCharFormat()
            if spec["color"]:
                fmt.setForeground(QColor(f"#{spec['color']}"))
            if spec["bold"]:
                fmt.setFontWeight(QFont.Weight.Bold)
            if spec["italic"]:
                fmt.setFontItalic(True)
            if spec["underline"]:
                fmt.setFontUnderline(True)
            self._formats[token] = fmt
        return fmt

    def relex(self) -> None:
        self._lines = {}
        text = self.document().toPlainText()
        if self._lexer is not None and len(text) <= MAX_HIGHLIGHT_CHARS:
            line, column = 0, 0
            for token, value in self._lexer.get_tokens(text):
                fmt = self._format(token)
                parts = value.split("\n")
                for i, part in enumerate(parts):
                    if i:
                        line, column = line + 1, 0
                    width = len(part.encode("utf-16-le")) // 2  # Qt counts UTF-16 code units
                    styled = (fmt.foreground().style() != Qt.BrushStyle.NoBrush or fmt.fontWeight() > 400
                              or fmt.fontItalic())
                    if width and styled:
                        self._lines.setdefault(line, []).append((column, width, fmt))
                    column += width
        self.rehighlight()

    def highlightBlock(self, text: str) -> None:
        for start, length, fmt in self._lines.get(self.currentBlock().blockNumber(), ()):
            self.setFormat(start, length, fmt)


def _lexer_for(name: str, text: str):
    from pygments.lexers import TextLexer, get_lexer_for_filename, guess_lexer
    from pygments.util import ClassNotFound

    try:
        lexer = get_lexer_for_filename(name, text[:4096], stripnl=False, ensurenl=False)
    except ClassNotFound:
        if not text.startswith("#!"):
            return None
        try:  # scripts without an extension: the shebang line tells
            lexer = guess_lexer(text[:4096], stripnl=False, ensurenl=False)
        except ClassNotFound:
            return None
    return None if isinstance(lexer, TextLexer) else lexer


# ---------------------------------------------------------------------------
# The text area with line numbers
# ---------------------------------------------------------------------------


class _Gutter(QWidget):
    def __init__(self, editor: CodeEditor) -> None:
        super().__init__(editor)
        self.editor = editor

    def sizeHint(self) -> QSize:
        return QSize(self.editor.gutter_width(), 0)

    def paintEvent(self, event: QPaintEvent) -> None:
        self.editor.paint_gutter(event)


class CodeEditor(QPlainTextEdit):
    def __init__(self, themes: ThemeManager) -> None:
        super().__init__()
        self.themes = themes
        self.setLineWrapMode(QPlainTextEdit.LineWrapMode.NoWrap)
        self._gutter = _Gutter(self)
        self.blockCountChanged.connect(lambda _: self._update_margins())
        self.updateRequest.connect(self._scroll_gutter)
        self.cursorPositionChanged.connect(self._highlight_line)
        self.restyle()

    def restyle(self) -> None:
        c = self.themes.colors
        font = QFont(self.themes.font)
        self.setFont(font)
        self.setTabStopDistance(self.fontMetrics().horizontalAdvance(" ") * 4)
        self.setStyleSheet(
            f"QPlainTextEdit {{ background: {c.surface}; color: {c.foreground}; border: none;"
            f" selection-background-color: {c.cursor_bg}; selection-color: {c.cursor_fg}; }}"
        )
        self._update_margins()
        self._highlight_line()

    def gutter_width(self) -> int:
        digits = max(3, len(str(self.blockCount())))
        return self.fontMetrics().horizontalAdvance("9") * (digits + 2)

    def _update_margins(self) -> None:
        self.setViewportMargins(self.gutter_width(), 0, 0, 0)

    def _scroll_gutter(self, rect: QRect, dy: int) -> None:
        if dy:
            self._gutter.scroll(0, dy)
        else:
            self._gutter.update(0, rect.y(), self._gutter.width(), rect.height())

    def resizeEvent(self, event: QResizeEvent) -> None:
        super().resizeEvent(event)
        r = self.contentsRect()
        self._gutter.setGeometry(QRect(r.left(), r.top(), self.gutter_width(), r.height()))

    def paint_gutter(self, event: QPaintEvent) -> None:
        c = self.themes.colors
        p = QPainter(self._gutter)
        p.fillRect(event.rect(), QColor(c.surface))
        block = self.firstVisibleBlock()
        top = round(self.blockBoundingGeometry(block).translated(self.contentOffset()).top())
        current = self.textCursor().blockNumber()
        width = self._gutter.width() - self.fontMetrics().horizontalAdvance("9")
        while block.isValid() and top <= event.rect().bottom():
            bottom = top + round(self.blockBoundingRect(block).height())
            if block.isVisible() and bottom >= event.rect().top():
                p.setPen(QColor(c.foreground if block.blockNumber() == current else c.muted))
                p.drawText(0, top, width, self.fontMetrics().height(), Qt.AlignmentFlag.AlignRight,
                           str(block.blockNumber() + 1))
            block = block.next()
            top = bottom

    def _highlight_line(self) -> None:
        line = QTextEdit.ExtraSelection()
        line.format.setBackground(QColor(self.themes.colors.cursor_inactive_bg))
        line.format.setProperty(QTextFormat.Property.FullWidthSelection, True)
        line.cursor = self.textCursor()
        line.cursor.clearSelection()
        self.setExtraSelections([line])
        self._gutter.update()

    def keyPressEvent(self, event) -> None:
        # Enter keeps the current line's indentation.
        if event.key() in (Qt.Key.Key_Return, Qt.Key.Key_Enter) and not event.modifiers():
            line = self.textCursor().block().text()
            indent = line[: len(line) - len(line.lstrip(" \t"))]
            super().keyPressEvent(event)
            self.insertPlainText(indent)
            return
        super().keyPressEvent(event)


# ---------------------------------------------------------------------------
# The editor window
# ---------------------------------------------------------------------------


class EditorWindow(QWidget):
    """Edits one file. `closed` is emitted when the window goes away."""

    closed = Signal(object)  # self
    saved = Signal(object)  # vfs.VPath

    def __init__(self, themes: ThemeManager, location: vfs.VPath, *, new: bool = False) -> None:
        super().__init__(None, Qt.WindowType.Window)
        self.setAttribute(Qt.WidgetAttribute.WA_DeleteOnClose)
        self.themes = themes
        self.location = location
        self.file = textfile.TextFile("")
        self.read_only = not location.fs.writable
        self._new = new
        self._loaded = new
        self.load_failed = False
        self._saving = False

        self.frame = BoxFrame(themes, heavy=True)
        self.frame.set_active(True)
        outer = QVBoxLayout(self)
        outer.setContentsMargins(0, 0, 0, 0)
        outer.addWidget(self.frame)
        self.editor = CodeEditor(themes)
        self.editor.setReadOnly(True)
        self.frame.body.addWidget(self.editor, 1)
        self.find_bar = self._build_find_bar()
        self.frame.body.addWidget(self.find_bar)
        self.find_bar.hide()
        self.highlighter: PygmentsHighlighter | None = None

        self.editor.document().modificationChanged.connect(lambda _: self._update_titles())
        self.editor.cursorPositionChanged.connect(self._update_titles)
        themes.changed.connect(self._restyle)
        self._watcher = QFileSystemWatcher(self)
        self._watcher.fileChanged.connect(lambda _: self._reload_timer.start())
        self._reload_timer = QTimer(self, singleShot=True, interval=300)
        self._reload_timer.timeout.connect(self._changed_on_disk)
        self._shortcuts()

        cw = self.fontMetrics().horizontalAdvance("M")
        self.resize(cw * 100, self.fontMetrics().height() * 40)
        self._update_titles()
        if new:
            self._start_editing(textfile.TextFile(""))
        else:
            self.editor.setPlainText("")
            self.frame.set_subtitle("loading…")
            self.load()

    # -- keys ----------------------------------------------------------------------

    def _shortcuts(self) -> None:
        def key(sequence, slot: Callable[[], None]) -> None:
            shortcut = QShortcut(QKeySequence(sequence), self)
            shortcut.setContext(Qt.ShortcutContext.WindowShortcut)
            shortcut.activated.connect(slot)

        key(QKeySequence.StandardKey.Save, self.save)
        key(QKeySequence.StandardKey.Close, self.close)
        key(QKeySequence.StandardKey.Find, self.show_find)
        key(QKeySequence.StandardKey.FindNext, lambda: self.find_next(backward=False))
        key(QKeySequence.StandardKey.FindPrevious, lambda: self.find_next(backward=True))
        key("Ctrl+Alt+F", self.show_replace)
        key("Ctrl+L", self.go_to_line)
        key("Escape", self._escape)
        key("F3", lambda: self.find_next(backward=False))

    def _escape(self) -> None:
        if self.find_bar.isVisible():
            self.find_bar.hide()
            self.editor.setFocus()
        else:
            self.close()

    # -- find and replace ----------------------------------------------------------

    def _build_find_bar(self) -> QWidget:
        bar = QWidget()
        bar.setObjectName("dialogBody")
        row = QHBoxLayout(bar)
        row.setContentsMargins(4, 4, 4, 4)
        self.find_field = QLineEdit()
        self.find_field.setPlaceholderText("Find")
        self.replace_field = QLineEdit()
        self.replace_field.setPlaceholderText("Replace with")
        self.match_case = QCheckBox("Match case")
        self.find_status = QLabel("")
        self.find_status.setObjectName("muted")
        row.addWidget(self.find_field, 2)
        row.addWidget(self.replace_field, 2)
        self.replace_all_button = QPushButton("Replace all")
        self.replace_all_button.setAutoDefault(False)
        self.replace_all_button.clicked.connect(self.replace_all)
        row.addWidget(self.replace_all_button)
        row.addWidget(self.match_case)
        row.addWidget(self.find_status, 1)
        self.find_field.returnPressed.connect(lambda: self.find_next(backward=False))
        self.find_field.textChanged.connect(lambda _: self.find_status.setText(""))
        self.replace_field.returnPressed.connect(self.replace_one)
        return bar

    def show_find(self, *, replace: bool = False) -> None:
        self.find_bar.show()
        for widget in (self.replace_field, self.replace_all_button):
            widget.setVisible(replace and not self.read_only)
        selected = self.editor.textCursor().selectedText()
        if selected and " " not in selected:
            self.find_field.setText(selected)
        self.find_field.selectAll()
        self.find_field.setFocus()

    def show_replace(self) -> None:
        self.show_find(replace=True)

    def find_next(self, *, backward: bool = False) -> bool:
        needle = self.find_field.text()
        if not needle:
            self.show_find()
            return False
        flags = QTextDocument.FindFlag(0)
        if backward:
            flags |= QTextDocument.FindFlag.FindBackward
        if self.match_case.isChecked():
            flags |= QTextDocument.FindFlag.FindCaseSensitively
        if self.editor.find(needle, flags):
            self.find_status.setText("")
            return True
        cursor = self.editor.textCursor()  # wrap around
        cursor.movePosition(QTextCursor.MoveOperation.End if backward else QTextCursor.MoveOperation.Start)
        self.editor.setTextCursor(cursor)
        found = self.editor.find(needle, flags)
        self.find_status.setText("wrapped around" if found else "not found")
        return found

    def replace_one(self) -> None:
        cursor = self.editor.textCursor()
        needle = self.find_field.text()
        same = (cursor.selectedText() == needle if self.match_case.isChecked()
                else cursor.selectedText().casefold() == needle.casefold())
        if cursor.hasSelection() and same and not self.read_only:
            cursor.insertText(self.replace_field.text())
        self.find_next()

    def replace_all(self) -> int:
        """Replace every match (one undo step); returns how many."""
        needle, replacement = self.find_field.text(), self.replace_field.text()
        if not needle or self.read_only:
            return 0
        flags = QTextDocument.FindFlag.FindCaseSensitively if self.match_case.isChecked() else QTextDocument.FindFlag(0)
        doc = self.editor.document()
        cursor = QTextCursor(doc)
        edit = QTextCursor(doc)
        edit.beginEditBlock()
        count = 0
        while True:
            cursor = doc.find(needle, cursor, flags)
            if cursor.isNull():
                break
            cursor.insertText(replacement)
            count += 1
        edit.endEditBlock()
        self.find_status.setText(f"{count} replaced")
        return count

    def go_to_line(self) -> None:
        value = dialogs.ask_text(self, self.themes, "Go to line", "Line number:", "", "Go")
        if value and value.strip().isdigit():
            block = self.editor.document().findBlockByNumber(max(int(value) - 1, 0))
            cursor = QTextCursor(block)
            self.editor.setTextCursor(cursor)
            self.editor.centerCursor()

    # -- loading and saving --------------------------------------------------------

    def load(self) -> None:
        location = self.location

        def read() -> textfile.TextFile:
            with location.fs.open_read(location.path) as f:
                return textfile.decode(f.read())

        def done(file: textfile.TextFile) -> None:
            self._loaded = True
            self._start_editing(file)

        def failed(exc: BaseException) -> None:
            reason = "it isn't a text file (try the hex view, F3)" if isinstance(exc, textfile.BinaryFile) else str(exc)
            self.frame.set_subtitle("")
            self.editor.setPlainText(f"Can't open {location.name}: {reason}")
            self.read_only = True
            self.load_failed = True

        tasks.run_in_background(read, done, failed, owner=self)

    def _start_editing(self, file: textfile.TextFile) -> None:
        position = self.editor.textCursor().position()
        self.file = file
        self.editor.setPlainText(file.text)
        self.editor.document().setModified(False)
        cursor = self.editor.textCursor()
        cursor.setPosition(min(position, len(file.text)))
        self.editor.setTextCursor(cursor)
        self.editor.setReadOnly(self.read_only)
        if self.highlighter is None:
            self.highlighter = PygmentsHighlighter(self.editor.document(), self.location.name,
                                                   self.themes.colors.pygments_style)
        self._watch()
        self._update_titles()

    def _watch(self) -> None:
        if self._watcher.files():
            self._watcher.removePaths(self._watcher.files())
        local = self.location.local_path()
        if local is not None and local.exists():
            self._watcher.addPath(str(local))

    @property
    def modified(self) -> bool:
        return self.editor.document().isModified()

    def save(self, *, then: Callable[[], None] | None = None) -> None:
        if self.read_only or self._saving or not self._loaded:
            return
        try:
            data = self.file.encode(self.editor.toPlainText())
        except UnicodeEncodeError as exc:  # e.g. an emoji in a Latin-1 file
            if not dialogs.confirm(self, self.themes, "Save as UTF-8",
                                   f"Some characters can't be written as {self.file.description.split(' · ')[0]} "
                                   f"({exc.object[exc.start:exc.end]!r}).\n\nSave the file as UTF-8 instead?", "Save"):
                return
            self.file = textfile.TextFile(self.file.text, "utf-8", False, self.file.newline)
            data = self.file.encode(self.editor.toPlainText())
        location = self.location
        self._saving = True
        self._watcher.removePaths(self._watcher.files())  # our own write isn't an outside change

        def write() -> None:
            local = location.local_path()
            if local is not None:
                _write_atomically(local, data)
            else:
                with location.fs.open_write(location.path) as f:
                    f.write(data)

        def done(_) -> None:
            self._saving = False
            self._new = False
            self.editor.document().setModified(False)
            self._watch()
            self._update_titles()
            self.saved.emit(location)
            if then is not None:
                then()

        def failed(exc: BaseException) -> None:
            self._saving = False
            self._watch()
            dialogs.confirm(self, self.themes, "Not saved", f"Couldn't save {location.name}:\n{exc}", "OK")

        self.frame.set_subtitle("saving…")
        tasks.run_in_background(write, done, failed, owner=self)

    def _changed_on_disk(self) -> None:
        local = self.location.local_path()
        if local is None or self._saving:
            return
        if not local.exists():
            self.frame.set_subtitle("deleted on disk")
            return
        if self.modified and not dialogs.confirm(
            self, self.themes, "Changed on disk",
            f"{self.location.name} was changed by another program.\n\nReload it and lose your changes?", "Reload",
        ):
            self._watch()
            return
        self.load()

    # -- titles and closing --------------------------------------------------------

    def _update_titles(self) -> None:
        mark = "● " if self.modified else ""
        name = self.location.name or self.location.display
        self.setWindowTitle(f"{mark}{name} — Orchard Commander")
        self.frame.set_title(f"{mark}{_short_display(self.location)}")
        if not self._loaded:
            return
        cursor = self.editor.textCursor()
        parts = [f"Ln {cursor.blockNumber() + 1}, Col {cursor.positionInBlock() + 1}", self.file.description]
        if self.highlighter is not None:
            parts.append(self.highlighter.language)
        if self.read_only:
            parts.append("read-only")
        self.frame.set_subtitle(" · ".join(parts))

    def _restyle(self) -> None:
        self.editor.restyle()
        if self.highlighter is not None:
            self.highlighter.set_style(self.themes.colors.pygments_style)
            self.highlighter.relex()

    def closeEvent(self, event) -> None:
        if self.modified and not self.read_only:
            choice = ask_save(self, self.themes, self.location.name)
            if choice == "cancel":
                event.ignore()
                return
            if choice == "save":
                event.ignore()
                self.save(then=self._close_saved)
                return
        self.closed.emit(self)
        super().closeEvent(event)

    def _close_saved(self) -> None:
        self.editor.document().setModified(False)
        self.close()


def _short_display(location: vfs.VPath) -> str:
    """The path as the panels show it: the home folder as ~."""
    text = location.display
    home = str(Path.home())
    if text == home or text.startswith(home + os.sep):
        text = "~" + text[len(home):]
    return text


def ask_save(parent: QWidget, themes: ThemeManager, name: str) -> str:
    """"save", "discard" or "cancel"."""
    from PySide6.QtWidgets import QDialog

    d = dialogs.Dialog(parent, themes, "Unsaved changes", width_chars=56)
    d.add_label(f"Save the changes to {name}?")
    result = {"choice": "cancel"}

    def pick(choice: str) -> Callable[[], None]:
        def done() -> None:
            result["choice"] = choice
            d.accept()
        return done

    d.add_buttons(("Save", pick("save")), ("Don't save", pick("discard")), ("Cancel", d.reject))
    d.exec()
    return result["choice"] if d.result() == QDialog.DialogCode.Accepted else "cancel"


def _write_atomically(path: Path, data: bytes) -> None:
    """Write next to the file, then swap it in: a crash can't leave half a file. Keeps the permissions."""
    mode = path.stat().st_mode if path.exists() else None
    fd, temp = tempfile.mkstemp(prefix=f".{path.name}.", suffix=".tmp", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as f:
            f.write(data)
            f.flush()
            os.fsync(f.fileno())
        if mode is not None:
            os.chmod(temp, mode & 0o7777)
        os.replace(temp, path)
    except BaseException:
        Path(temp).unlink(missing_ok=True)
        raise


def new_file(folder: vfs.VPath, name: str) -> vfs.VPath:
    """Create an empty file (blocking); raises FileExistsError if the name is taken."""
    location = folder.child(name)
    if location.exists():
        raise FileExistsError(name)
    with location.fs.open_write(location.path) as f:
        f.write(b"")
    return location


__all__ = ["EditorWindow", "CodeEditor", "PygmentsHighlighter", "new_file"]
