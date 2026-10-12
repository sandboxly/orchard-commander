"""The terminal panel: draws a `terminal.Session`'s screen and sends it keystrokes.

One `TerminalView` shows whichever session is current (the active side's);
the sessions themselves keep running in the background.
"""

from __future__ import annotations

import sys
from collections.abc import Callable
from pathlib import Path

from PySide6.QtCore import QEvent, QObject, QPoint, QRectF, Qt, QTimer, Signal
from PySide6.QtGui import (
    QColor,
    QFont,
    QFontMetricsF,
    QGuiApplication,
    QKeyEvent,
    QMouseEvent,
    QPainter,
    QPaintEvent,
    QResizeEvent,
    QWheelEvent,
)
from PySide6.QtWidgets import QWidget

from orchard_commander import terminal
from orchard_commander.themes import ColorScheme
from orchard_commander.ui import term_glyphs
from orchard_commander.ui.boxframe import BoxFrame
from orchard_commander.ui.style import ThemeManager

IS_MACOS = sys.platform == "darwin"
# On macOS Qt calls ⌘ "Control" and the Control key "Meta".
CTRL = Qt.KeyboardModifier.MetaModifier if IS_MACOS else Qt.KeyboardModifier.ControlModifier
CMD = Qt.KeyboardModifier.ControlModifier if IS_MACOS else Qt.KeyboardModifier.NoModifier
ALT = Qt.KeyboardModifier.AltModifier
SHIFT = Qt.KeyboardModifier.ShiftModifier
TOGGLE_KEY = "Meta+`" if IS_MACOS else "Ctrl+`"  # the Control key on every platform, as in VS Code

# xterm's 16 colors, softened a little so they read well on dark and light themes.
DARK_ANSI = {
    "black": "#3b3b3b", "red": "#e06c75", "green": "#98c379", "brown": "#e5c07b", "blue": "#61afef",
    "magenta": "#c678dd", "cyan": "#56b6c2", "white": "#d0d0d0",
    "brightblack": "#7f848e", "brightred": "#ff7b86", "brightgreen": "#b5e890", "brightbrown": "#ffd68a",
    "brightblue": "#80c4ff", "brightmagenta": "#de9cf0", "brightcyan": "#7fd8e3", "brightwhite": "#ffffff",
}
LIGHT_ANSI = {
    "black": "#000000", "red": "#c4314b", "green": "#3a7d1c", "brown": "#986801", "blue": "#2763c4",
    "magenta": "#a626a4", "cyan": "#0e7f8a", "white": "#a0a1a7",
    "brightblack": "#696c77", "brightred": "#e45649", "brightgreen": "#50a14f", "brightbrown": "#c18401",
    "brightblue": "#4078f2", "brightmagenta": "#c33fc1", "brightcyan": "#0997b3", "brightwhite": "#383a42",
}
pyte_aliases = {"yellow": "brown", "brightyellow": "brightbrown"}

KEYS = {  # keys that send escape sequences (normal cursor-key mode)
    Qt.Key.Key_Up: "\x1b[A", Qt.Key.Key_Down: "\x1b[B", Qt.Key.Key_Right: "\x1b[C", Qt.Key.Key_Left: "\x1b[D",
    Qt.Key.Key_Home: "\x1b[H", Qt.Key.Key_End: "\x1b[F", Qt.Key.Key_PageUp: "\x1b[5~",
    Qt.Key.Key_PageDown: "\x1b[6~", Qt.Key.Key_Insert: "\x1b[2~", Qt.Key.Key_Delete: "\x1b[3~",
    Qt.Key.Key_Return: "\r", Qt.Key.Key_Enter: "\r", Qt.Key.Key_Backspace: "\x7f", Qt.Key.Key_Tab: "\t",
    Qt.Key.Key_Backtab: "\x1b[Z", Qt.Key.Key_Escape: "\x1b",
    Qt.Key.Key_F1: "\x1bOP", Qt.Key.Key_F2: "\x1bOQ", Qt.Key.Key_F3: "\x1bOR", Qt.Key.Key_F4: "\x1bOS",
    Qt.Key.Key_F5: "\x1b[15~", Qt.Key.Key_F6: "\x1b[17~", Qt.Key.Key_F7: "\x1b[18~", Qt.Key.Key_F8: "\x1b[19~",
    Qt.Key.Key_F9: "\x1b[20~", Qt.Key.Key_F10: "\x1b[21~", Qt.Key.Key_F11: "\x1b[23~", Qt.Key.Key_F12: "\x1b[24~",
}
APP_CURSOR = {Qt.Key.Key_Up: "\x1bOA", Qt.Key.Key_Down: "\x1bOB", Qt.Key.Key_Right: "\x1bOC",
              Qt.Key.Key_Left: "\x1bOD", Qt.Key.Key_Home: "\x1bOH", Qt.Key.Key_End: "\x1bOF"}
CTRL_SYMBOLS = {Qt.Key.Key_Space: "\x00", Qt.Key.Key_BracketLeft: "\x1b", Qt.Key.Key_Backslash: "\x1c",
                Qt.Key.Key_BracketRight: "\x1d", Qt.Key.Key_AsciiCircum: "\x1e", Qt.Key.Key_Underscore: "\x1f",
                Qt.Key.Key_Minus: "\x1f", Qt.Key.Key_At: "\x00", Qt.Key.Key_2: "\x00"}
WORD_KEYS = {Qt.Key.Key_Left: "\x1bb", Qt.Key.Key_Right: "\x1bf", Qt.Key.Key_Backspace: "\x1b\x7f"}


_nerd_fonts: list[str] | None = None


def nerd_fonts() -> list[str]:
    """Installed fonts with Nerd Font icons (MesloLGS NF for powerlevel10k, …), monospaced first."""
    global _nerd_fonts
    if _nerd_fonts is None:
        from PySide6.QtGui import QFontDatabase

        found = [f for f in QFontDatabase.families()
                 if "Nerd Font" in f or f.endswith((" NF", " NFM", " NFP")) or " NF " in f]
        _nerd_fonts = sorted(found, key=lambda f: (not QFontDatabase.isFixedPitch(f), "Mono" not in f, f))
    return _nerd_fonts


def key_to_text(event: QKeyEvent, *, app_cursor: bool) -> str:
    """What a key press sends to the shell ("" for nothing)."""
    key, mods = event.key(), event.modifiers() & ~Qt.KeyboardModifier.KeypadModifier
    if mods & CTRL:
        if Qt.Key.Key_A <= key <= Qt.Key.Key_Z:
            return chr(key - Qt.Key.Key_A + 1)  # Ctrl+C → ETX, …
        if key in CTRL_SYMBOLS:
            return CTRL_SYMBOLS[key]
    if mods & ALT and key in WORD_KEYS:  # ⌥←/⌥→/⌥⌫: by word, as in Terminal.app and iTerm
        return WORD_KEYS[key]
    if app_cursor and key in APP_CURSOR and not mods & (CTRL | ALT):
        return APP_CURSOR[key]
    if key in KEYS:
        if mods & SHIFT and key == Qt.Key.Key_Tab:
            return KEYS[Qt.Key.Key_Backtab]
        return KEYS[key]
    text = event.text()
    if text and mods & ALT and not IS_MACOS:
        return "\x1b" + text  # Alt as Meta (on macOS, ⌥ types characters like ø)
    return text


class TerminalView(QWidget):
    """Draws the current session's screen; keeps the keyboard while it has focus."""

    toggle_requested = Signal()  # the terminal toggle key, pressed inside the terminal

    def __init__(self, themes: ThemeManager) -> None:
        super().__init__()
        self.themes = themes
        self.session: terminal.Session | None = None
        self.on_restart: Callable[[], None] | None = None  # Enter after the shell exited
        self.setFocusPolicy(Qt.FocusPolicy.StrongFocus)
        self.setAttribute(Qt.WidgetAttribute.WA_InputMethodEnabled, True)
        self.setAttribute(Qt.WidgetAttribute.WA_OpaquePaintEvent, True)
        self.setCursor(Qt.CursorShape.IBeamCursor)
        self._scroll = 0  # lines scrolled back into the history
        self._selection: tuple[tuple[int, int], tuple[int, int]] | None = None  # (line, col) absolute
        self._selecting = False
        self._pressed: int | None = None  # the button reported as held to the program
        self._last_cell: tuple[int, int] | None = None
        self._last_click: tuple[int, QPoint] | None = None
        self._clicks = 0
        self._wheel_rest = 0
        self.setMouseTracking(True)  # motion without a button, for programs that want all motion
        self._repaint = QTimer(self, singleShot=True, interval=16)  # at most ~60 frames a second
        self._repaint.timeout.connect(self.update)
        self._resize = QTimer(self, singleShot=True, interval=60)
        self._resize.timeout.connect(self._apply_size)
        self._restyle()
        themes.changed.connect(self._restyle)

    # -- sessions ----------------------------------------------------------------

    def set_session(self, session: terminal.Session | None) -> None:
        self.session = session
        self._scroll = 0
        self._selection = None
        self._apply_size()
        self.update()

    def output_arrived(self, session: terminal.Session) -> None:
        """Called (on the GUI thread) when `session` printed something."""
        if session is not self.session:
            return
        if not self._repaint.isActive():
            self._repaint.start()
        # An arrow pointer while a program takes the mouse, a text cursor for selecting otherwise.
        reporting = terminal.mouse_tracking(session.screen.mode) is not None
        shape = Qt.CursorShape.ArrowCursor if reporting else Qt.CursorShape.IBeamCursor
        if self.cursor().shape() != shape:
            self.setCursor(shape)

    def grid_size(self) -> tuple[int, int]:
        """(columns, lines) that fit."""
        return max(int(self.width() / self._cw), 2), max(int(self.height() / self._ch), 2)

    def _apply_size(self) -> None:
        if self.session is not None and self.width() > 0 and self.height() > 0:
            self.session.resize(*self.grid_size())
            self.update()

    def resizeEvent(self, event: QResizeEvent) -> None:
        super().resizeEvent(event)
        self._resize.start()

    # -- looks -------------------------------------------------------------------

    def _restyle(self) -> None:
        self._font = QFont(self.themes.font)
        # Prompt themes (powerlevel10k, starship, oh-my-posh) use icons only Nerd Fonts have:
        # fall back to an installed one for them, keeping the theme font for everything else.
        self._font.setFamilies([self._font.family(), *nerd_fonts()])
        self._bold = QFont(self._font)
        self._bold.setBold(True)
        self._italic = QFont(self._font)
        self._italic.setItalic(True)
        fm = QFontMetricsF(self._font)
        self._cw = fm.horizontalAdvance("M")
        self._ch = fm.height()
        self._ascent = fm.ascent()
        c: ColorScheme = self.themes.colors
        self._colors = c
        self._ansi = DARK_ANSI if c.dark else LIGHT_ANSI
        self._resize.start()
        self.update()

    def _color(self, name: str, default: str) -> QColor:
        if name == "default":
            return QColor(default)
        name = pyte_aliases.get(name, name)
        if name in self._ansi:
            return QColor(self._ansi[name])
        return QColor(f"#{name}") if len(name) == 6 else QColor(default)

    # -- drawing -----------------------------------------------------------------

    def _visible_lines(self) -> tuple[list, int, int, bool]:
        """(lines to draw, index of the first in absolute numbering, history length, cursor visible)."""
        assert self.session is not None
        screen = self.session.screen
        history = screen.history_lines()
        scroll = min(self._scroll, len(history))
        rows = screen.lines
        top = list(history)[len(history) - scroll:] if scroll else []
        lines = top + [screen.buffer[y] for y in range(rows - len(top))]
        return lines, len(history) - scroll, len(history), scroll == 0 and not screen.cursor.hidden

    def paintEvent(self, event: QPaintEvent) -> None:
        c = self._colors
        p = QPainter(self)
        p.fillRect(self.rect(), QColor(c.surface))
        if self.session is None:
            return
        with self.session.lock:
            screen = self.session.screen
            lines, first, history_len, show_cursor = self._visible_lines()
            columns = screen.columns
            cursor = (screen.cursor.y, screen.cursor.x)
            lines = [[line[x] for x in range(columns)] for line in lines]  # copy under the lock
        cw, ch = self._cw, self._ch
        sel = self._normalized_selection()
        default_fg, default_bg = c.foreground, c.surface
        for row, cells in enumerate(lines):
            y = row * ch
            absolute = first + row
            x = 0
            while x < columns:
                char = cells[x]
                selected = sel is not None and _in_selection(sel, absolute, x)
                key = (char.fg, char.bg, char.bold, char.italics, char.underscore, char.reverse, selected)
                end = x + 1
                while end < columns:
                    nxt = cells[end]
                    nsel = sel is not None and _in_selection(sel, absolute, end)
                    if (nxt.fg, nxt.bg, nxt.bold, nxt.italics, nxt.underscore, nxt.reverse, nsel) != key:
                        break
                    end += 1
                fg = self._color(char.fg, default_fg)
                bg = self._color(char.bg, default_bg)
                if char.bold and char.fg in self._ansi and not char.fg.startswith("bright"):
                    fg = self._color("bright" + char.fg, default_fg)
                if char.reverse != selected:
                    fg, bg = bg, fg
                rect = QRectF(x * cw, y, (end - x) * cw, ch)
                if bg != QColor(default_bg):
                    p.fillRect(rect, bg)
                text = "".join(cell.data for cell in cells[x:end])
                if any(term_glyphs.handles(cell.data) for cell in cells[x:end]):
                    # separators, blocks, box lines: drawn to fill their cells exactly
                    rest = []
                    for i, cell in enumerate(cells[x:end]):
                        if term_glyphs.handles(cell.data):
                            term_glyphs.draw(p, cell.data, QRectF((x + i) * cw, y, cw, ch), fg)
                            rest.append(" ")
                        else:
                            rest.append(cell.data)
                    text = "".join(rest)
                if text.strip():
                    p.setFont(self._bold if char.bold else self._italic if char.italics else self._font)
                    p.setPen(fg)
                    if text.isascii():
                        p.drawText(QRectF(x * cw, y, (end - x) * cw + cw, ch), Qt.AlignmentFlag.AlignLeft
                                   | Qt.AlignmentFlag.AlignVCenter, text)
                    else:  # wide characters, emoji: put each on its own cell
                        for i, cell in enumerate(cells[x:end]):
                            if cell.data:
                                p.drawText(QRectF((x + i) * cw, y, cw * 2, ch),
                                           Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignVCenter, cell.data)
                    if char.underscore:
                        p.drawLine(int(x * cw), int(y + ch - 1), int(end * cw), int(y + ch - 1))
                x = end
        if show_cursor:
            cy, cx = cursor
            rect = QRectF(cx * cw, cy * ch, cw, ch)
            if self.hasFocus():
                p.fillRect(rect, QColor(c.cursor_bg))
                data = lines[cy][cx].data if cy < len(lines) and cx < columns else ""
                if data.strip():
                    p.setFont(self._font)
                    p.setPen(QColor(c.cursor_fg))
                    p.drawText(rect, Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignVCenter, data)
            else:
                p.setPen(QColor(c.cursor_bg))
                p.drawRect(rect.adjusted(0.5, 0.5, -0.5, -0.5))

    # -- keyboard ----------------------------------------------------------------

    def event(self, event: QEvent) -> bool:
        if event.type() == QEvent.Type.ShortcutOverride and isinstance(event, QKeyEvent):
            # While the terminal has focus it gets the keys — Esc, Ctrl+C, F-keys, Tab —
            # except the terminal toggle and (on macOS) ⌘ shortcuts, which stay with the app.
            if not self._reserved(event):
                event.accept()
                return True
        if event.type() == QEvent.Type.KeyPress and isinstance(event, QKeyEvent) and event.key() in (
            Qt.Key.Key_Tab, Qt.Key.Key_Backtab
        ):
            self.keyPressEvent(event)  # not focus navigation: shell completion
            return True
        return super().event(event)

    @staticmethod
    def _reserved(event: QKeyEvent) -> bool:
        mods = event.modifiers() & ~Qt.KeyboardModifier.KeypadModifier
        if event.key() in (Qt.Key.Key_QuoteLeft, Qt.Key.Key_AsciiTilde) and mods & CTRL:
            return True  # the toggle
        if IS_MACOS and mods & CMD:
            return True
        return False

    def keyPressEvent(self, event: QKeyEvent) -> None:
        mods = event.modifiers() & ~Qt.KeyboardModifier.KeypadModifier
        if event.key() in (Qt.Key.Key_QuoteLeft, Qt.Key.Key_AsciiTilde) and mods & CTRL:
            self.toggle_requested.emit()
            return
        copy_paste = (CMD if IS_MACOS else (Qt.KeyboardModifier.ControlModifier | SHIFT))
        if mods == copy_paste and event.key() == Qt.Key.Key_C:
            self.copy()
            return
        if mods == copy_paste and event.key() == Qt.Key.Key_V:
            self.paste()
            return
        if IS_MACOS and mods & CMD:
            super().keyPressEvent(event)
            return
        session = self.session
        if session is None:
            return
        if not session.alive:
            if event.key() in (Qt.Key.Key_Return, Qt.Key.Key_Enter) and self.on_restart is not None:
                self.on_restart()
            return
        text = key_to_text(event, app_cursor=bool(session.screen.mode & {terminal.APP_CURSOR_KEYS}))
        if text:
            self._scroll = 0
            self._selection = None
            session.write(text)
            self.update()

    def inputMethodEvent(self, event) -> None:  # dead keys, composed characters
        if self.session is not None and event.commitString():
            self.session.write(event.commitString())

    def copy(self) -> None:
        text = self.selected_text()
        if text:
            QGuiApplication.clipboard().setText(text)

    def paste(self) -> None:
        text = QGuiApplication.clipboard().text()
        if not text or self.session is None:
            return
        text = text.replace("\r\n", "\r").replace("\n", "\r")
        if terminal.BRACKETED_PASTE in self.session.screen.mode:
            text = f"\x1b[200~{text}\x1b[201~"  # the shell won't run pasted lines one by one
        self._scroll = 0
        self.session.write(text)

    # -- mouse: reported to programs that ask (vim, htop, tmux, mc); else scrollback and selection

    def _reporting(self, event: QMouseEvent | QWheelEvent) -> bool:
        """Whether this event goes to the program. Shift keeps the mouse for selecting, as in xterm."""
        if self.session is None or event.modifiers() & SHIFT:
            return False
        return terminal.mouse_tracking(self.session.screen.mode) is not None

    def _screen_cell(self, pos: QPoint) -> tuple[int, int]:
        """(column, line) on the screen, 0-based and clamped."""
        assert self.session is not None
        screen = self.session.screen
        column = max(min(int(pos.x() / self._cw), screen.columns - 1), 0)
        line = max(min(int(pos.y() / self._ch), screen.lines - 1), 0)
        return column, line

    def _report(self, event: QMouseEvent | QWheelEvent, button: int, *, press: bool = True,
                motion: bool = False) -> None:
        assert self.session is not None
        column, line = self._screen_cell(event.position().toPoint())
        mods = event.modifiers()
        data = terminal.encode_mouse(
            self.session.screen.mode, button, column, line, press=press, motion=motion,
            alt=bool(mods & ALT), ctrl=bool(mods & CTRL),
        )
        if data:
            self.session.write(data)

    @staticmethod
    def _button(qt_button: Qt.MouseButton) -> int | None:
        return {Qt.MouseButton.LeftButton: terminal.LEFT, Qt.MouseButton.MiddleButton: terminal.MIDDLE,
                Qt.MouseButton.RightButton: terminal.RIGHT}.get(qt_button)

    def wheelEvent(self, event: QWheelEvent) -> None:
        if self.session is None:
            return
        delta = event.angleDelta()
        vertical = abs(delta.y()) >= abs(delta.x())
        amount = delta.y() if vertical else delta.x()
        self._wheel_rest += amount
        steps = int(self._wheel_rest / 40)  # trackpads send many small deltas
        if not steps:
            return
        self._wheel_rest -= steps * 40
        screen = self.session.screen
        if self._reporting(event):
            if vertical:
                button = terminal.WHEEL_UP if steps > 0 else terminal.WHEEL_DOWN
            else:
                button = terminal.WHEEL_LEFT if steps > 0 else terminal.WHEEL_RIGHT
            for _ in range(abs(steps)):
                self._report(event, button)
            return
        if not vertical:
            return
        if screen.alternate:  # full-screen programs without mouse support: scroll like the arrow keys
            key = Qt.Key.Key_Up if steps > 0 else Qt.Key.Key_Down
            app = bool(screen.mode & {terminal.APP_CURSOR_KEYS})
            self.session.write((APP_CURSOR if app else KEYS)[key] * abs(steps))
            return
        with self.session.lock:
            limit = len(screen.history_lines())
        self._scroll = max(0, min(self._scroll + steps, limit))
        self.update()

    def _cell_at(self, pos: QPoint) -> tuple[int, int]:
        """(absolute line incl. history, column) for selections."""
        assert self.session is not None
        with self.session.lock:
            history = len(self.session.screen.history_lines())
            columns = self.session.screen.columns
        row = max(int(pos.y() / self._ch), 0)
        col = max(min(int(pos.x() / self._cw), columns), 0)
        return history - min(self._scroll, history) + row, col

    def mousePressEvent(self, event: QMouseEvent) -> None:
        self.setFocus()
        if self.session is None:
            return
        button = self._button(event.button())
        if button is not None and self._reporting(event):
            self._pressed = button
            self._last_cell = self._screen_cell(event.position().toPoint())
            self._report(event, button)
            return
        if event.button() == Qt.MouseButton.LeftButton:
            cell = self._cell_at(event.position().toPoint())
            clicks = self._click_count(event)
            if clicks == 2:
                self._selection = self._word_at(cell)
            elif clicks >= 3:
                self._selection = ((cell[0], 0), (cell[0] + 1, 0))  # the whole line
            else:
                self._selection = (cell, cell)
                self._selecting = True
            self.update()
        elif event.button() == Qt.MouseButton.MiddleButton:
            self.paste()

    def mouseDoubleClickEvent(self, event: QMouseEvent) -> None:
        # Counted in _click_count; Qt doesn't always deliver the first click's press before this.
        self._clicks = max(self._clicks, 1)
        self._last_click = (event.timestamp(), event.position().toPoint())
        self.mousePressEvent(event)

    def mouseMoveEvent(self, event: QMouseEvent) -> None:
        if self.session is None:
            return
        if self._reporting(event) and not self._selecting:
            cell = self._screen_cell(event.position().toPoint())
            if cell != self._last_cell:  # one report per cell, not per pixel
                self._last_cell = cell
                button = self._pressed if self._pressed is not None else terminal.NO_BUTTON
                self._report(event, button, motion=True)
            return
        if self._selecting and self._selection is not None:
            self._selection = (self._selection[0], self._cell_at(event.position().toPoint()))
            self.update()

    def mouseReleaseEvent(self, event: QMouseEvent) -> None:
        button = self._button(event.button())
        if self._pressed is not None and button == self._pressed and self.session is not None:
            self._pressed = None
            self._report(event, button, press=False)
            return
        self._selecting = False
        if self._selection is not None and self._selection[0] == self._selection[1]:
            self._selection = None
            self.update()

    def _click_count(self, event: QMouseEvent) -> int:
        """1, 2, 3… for single, double, triple clicks on (about) the same spot."""
        now = event.timestamp()
        pos = event.position().toPoint()
        interval = QGuiApplication.styleHints().mouseDoubleClickInterval()
        near = self._last_click is not None and (pos - self._last_click[1]).manhattanLength() < 6
        if near and now - self._last_click[0] <= interval:
            self._clicks += 1
        else:
            self._clicks = 1
        self._last_click = (now, pos)
        return self._clicks

    def _word_at(self, cell: tuple[int, int]) -> tuple[tuple[int, int], tuple[int, int]]:
        """The selection covering the word (or path) under `cell`."""
        line_no, col = cell
        assert self.session is not None
        with self.session.lock:
            screen = self.session.screen
            lines = list(screen.history_lines()) + [screen.buffer[y] for y in range(screen.lines)]
            columns = screen.columns
            if not 0 <= line_no < len(lines):
                return cell, cell
            text = "".join(lines[line_no][x].data or " " for x in range(columns))
        word = lambda ch: ch.isalnum() or ch in "-_./~:@%+=#"  # noqa: E731 (paths and URLs count as words)
        if col >= len(text) or not word(text[col]):
            return (line_no, col), (line_no, col + 1)
        start, end = col, col
        while start > 0 and word(text[start - 1]):
            start -= 1
        while end < len(text) and word(text[end]):
            end += 1
        return (line_no, start), (line_no, end)

    # -- focus reporting (vim reloads changed files on focus, …) ---------------------

    def focusInEvent(self, event) -> None:
        super().focusInEvent(event)
        if self.session is not None and terminal.FOCUS_EVENTS in self.session.screen.mode:
            self.session.write("\x1b[I")
        self.update()

    def focusOutEvent(self, event) -> None:
        super().focusOutEvent(event)
        if self.session is not None and terminal.FOCUS_EVENTS in self.session.screen.mode:
            self.session.write("\x1b[O")
        self.update()

    def _normalized_selection(self) -> tuple[tuple[int, int], tuple[int, int]] | None:
        if self._selection is None:
            return None
        a, b = self._selection
        return (a, b) if a <= b else (b, a)

    def selected_text(self) -> str:
        sel = self._normalized_selection()
        if sel is None or self.session is None:
            return ""
        (l1, c1), (l2, c2) = sel
        with self.session.lock:
            screen = self.session.screen
            history = list(screen.history_lines())
            columns = screen.columns
            all_lines = history + [screen.buffer[y] for y in range(screen.lines)]
            out = []
            for n in range(l1, min(l2, len(all_lines) - 1) + 1):
                line = all_lines[n]
                start = c1 if n == l1 else 0
                end = c2 if n == l2 else columns
                out.append("".join(line[x].data for x in range(start, end)).rstrip())
        return "\n".join(out)


def _in_selection(sel: tuple[tuple[int, int], tuple[int, int]], line: int, col: int) -> bool:
    (l1, c1), (l2, c2) = sel
    return (l1, c1) <= (line, col) < (l2, c2)


class _Relay(QObject):
    """Brings reader-thread notifications to the GUI thread."""

    output = Signal(object)  # session
    exited = Signal(object)


class TerminalPanel(BoxFrame):
    """The bottom panel: one shell per side, showing the active side's."""

    def __init__(self, themes: ThemeManager, shell: Callable[[], list[str]]) -> None:
        super().__init__(themes)
        self._shell = shell
        self.view = TerminalView(themes)
        self.view.on_restart = self.restart
        self.body.addWidget(self.view)
        self.setFocusProxy(self.view)
        self.setMinimumHeight(self.fontMetrics().height() * 6)  # a few lines, at least
        self.sessions: dict[str, terminal.Session] = {}
        self._starting_dirs: dict[str, Path] = {}
        self.side: str | None = None
        self._scheduled: set[str] = set()
        self._relay = _Relay()
        self._relay.output.connect(lambda s: self.view.output_arrived(s))
        self._relay.exited.connect(self._exited)

    def show_side(self, side: str, cwd: Path) -> None:
        """Show `side`'s session (started in `cwd` the first time)."""
        self.side = side
        self._starting_dirs.setdefault(side, cwd)
        if side not in self.sessions and self.isVisible():
            self._start_soon(side)
        self.view.set_session(self.sessions.get(side))
        self._update_title()

    def showEvent(self, event) -> None:
        super().showEvent(event)
        if self.side is not None and self.side not in self.sessions:
            self._start_soon(self.side)

    def _start_soon(self, side: str) -> None:
        """Start once the layout has settled: a shell started at one size and resized right away
        redraws its prompt (zsh themes print it again), leaving copies behind."""
        if side in self._scheduled:
            return
        self._scheduled.add(side)

        def start() -> None:
            self._scheduled.discard(side)
            if side in self.sessions or not self.isVisible():
                return
            self._start(side)
            if self.side == side:
                self.view.set_session(self.sessions[side])
                self._update_title()

        QTimer.singleShot(50, self, start)  # with self as context: dropped if the panel is destroyed

    def _start(self, side: str) -> None:
        columns, lines = self.view.grid_size()
        relay = self._relay
        holder: list[terminal.Session] = []
        session = terminal.Session(
            self._shell(), self._starting_dirs[side], columns=columns, lines=lines,
            on_output=lambda: relay.output.emit(holder[0]),
            on_exit=lambda: relay.exited.emit(holder[0]),
        )
        holder.append(session)
        self.sessions[side] = session
        try:
            session.start()
        except Exception as exc:  # no such shell, …
            session.feed(f"Cannot start {' '.join(session.argv)}: {exc}\r\n")

    def _exited(self, session: terminal.Session) -> None:
        if session in self.sessions.values():
            status = f" (exit status {session.exit_status})" if session.exit_status else ""
            session.feed(f"\r\n\x1b[2m[{session.title} ended{status} — press Enter for a new one]\x1b[0m\r\n")

    def restart(self) -> None:
        if self.side is None:
            return
        old = self.sessions.pop(self.side, None)
        if old is not None:
            old.close()
        self._start(self.side)
        self.view.set_session(self.sessions[self.side])
        self._update_title()

    def _update_title(self) -> None:
        session = self.sessions.get(self.side or "")
        shell = session.title if session is not None else ""
        arrow = {"left": "◀ left", "right": "right ▶"}.get(self.side or "", "")
        self.set_title(" · ".join(x for x in ("Terminal", shell, arrow) if x))

    def run_in(self, side: str, folder: Path) -> None:
        """Make `side`'s shell change to `folder` (typed for the user, as if they did)."""
        session = self.sessions.get(side)
        if session is None or not session.alive:
            self._starting_dirs[side] = folder
            return
        if terminal.IS_WINDOWS:
            session.write(f"Set-Location -LiteralPath '{str(folder).replace(chr(39), chr(39) * 2)}'\r")
        else:
            import shlex

            session.write(f"\x15cd {shlex.quote(str(folder))}\r")  # Ctrl+U: clear what was typed first

    def close_all(self) -> None:
        for session in self.sessions.values():
            session.close()
        self.sessions.clear()
