"""The terminal panel: shell sessions in a pseudo-terminal, key translation, one session per side."""

from __future__ import annotations

import sys
import threading
import time
from pathlib import Path

import pytest
from PySide6.QtCore import QEvent, Qt
from PySide6.QtGui import QKeyEvent

from orchard_commander import terminal
from orchard_commander.ui.terminal_view import IS_MACOS, key_to_text
from test_ui import settle, wait_until

pytestmark = pytest.mark.skipif(sys.platform == "win32", reason="POSIX shell tests (ConPTY on Windows)")


def started(w, side: str) -> terminal.Session:
    """The side's session, once the terminal has started it (after the layout settled)."""
    wait_until(lambda: side in w.terminal.sessions)
    return w.terminal.sessions[side]


def screen_text(session: terminal.Session) -> str:
    with session.lock:
        return "\n".join(line.rstrip() for line in session.screen.display)


def run(session: terminal.Session, command: str) -> None:
    session.write(command + "\r")


@pytest.fixture
def sh(tmp_path: Path):
    exited = threading.Event()
    session = terminal.Session(["/bin/sh"], tmp_path, columns=60, lines=12, on_exit=exited.set)
    session.start()
    session.exited = exited
    yield session
    session.close()


def eventually(predicate, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while not predicate():
        if time.monotonic() > deadline:
            raise AssertionError("condition not met in time")
        time.sleep(0.02)


# -- sessions ------------------------------------------------------------------------------------


def test_shell_runs_in_folder_and_resizes(sh: terminal.Session, tmp_path: Path):
    run(sh, "pwd; echo sum=$((40+2))")
    eventually(lambda: "sum=42" in screen_text(sh))
    assert str(tmp_path.resolve()) in screen_text(sh).replace("\n", "") or tmp_path.name in screen_text(sh)
    sh.resize(40, 8)
    run(sh, "tput cols; tput lines")
    eventually(lambda: "\n40\n8" in screen_text(sh))


def test_alternate_screen_restores_shell_output(sh: terminal.Session):
    run(sh, "echo before")
    eventually(lambda: "before" in screen_text(sh))
    run(sh, r"printf '\033[?1049h'; echo FULL$((1+1)); sleep 0.3; printf '\033[?1049l'; echo af$((0))ter")
    eventually(lambda: "af0ter" in screen_text(sh))
    assert "FULL2" not in screen_text(sh) and "before" in screen_text(sh)  # like vim / less


def test_replies_to_cursor_position_queries():
    replies = []
    screen = terminal.Screen(20, 5, replies.append)
    import pyte

    pyte.Stream(screen).feed("ab\x1b[6n")
    assert replies == ["\x1b[1;3R"]


def test_scrollback(sh: terminal.Session):
    run(sh, "i=0; while [ $i -lt 40 ]; do echo line$i; i=$((i+1)); done")
    eventually(lambda: "line39" in screen_text(sh))
    with sh.lock:
        history = ["".join(c.data for c in line.values()).strip() for line in sh.screen.history_lines()]
    assert any(h.startswith("line0") for h in history)


def test_exit(sh: terminal.Session):
    run(sh, "exit 3")
    assert sh.exited.wait(5) and not sh.alive and sh.exit_status == 3


def test_default_shell():
    argv = terminal.default_shell()
    assert Path(argv[0]).exists() and argv[1:] == ["-l"]  # a login shell: the user's PATH
    assert terminal.shell_command("/bin/zsh -f") == ["/bin/zsh", "-f"]
    assert terminal.shell_command("") == argv
    env = terminal.shell_environment()
    assert env["TERM"] == "xterm-256color"


# -- keys ----------------------------------------------------------------------------------------

CTRL = Qt.KeyboardModifier.MetaModifier if IS_MACOS else Qt.KeyboardModifier.ControlModifier


def key(k, mods=Qt.KeyboardModifier.NoModifier, text: str = "") -> QKeyEvent:
    return QKeyEvent(QEvent.Type.KeyPress, k, mods, text)


@pytest.mark.parametrize(
    ("event", "app_cursor", "expected"),
    [
        (lambda: key(Qt.Key.Key_C, CTRL), False, "\x03"),
        (lambda: key(Qt.Key.Key_D, CTRL), False, "\x04"),
        (lambda: key(Qt.Key.Key_Up), False, "\x1b[A"),
        (lambda: key(Qt.Key.Key_Up), True, "\x1bOA"),  # vim, less: application cursor keys
        (lambda: key(Qt.Key.Key_Return), False, "\r"),
        (lambda: key(Qt.Key.Key_Backspace), False, "\x7f"),
        (lambda: key(Qt.Key.Key_Escape), False, "\x1b"),
        (lambda: key(Qt.Key.Key_Tab), False, "\t"),
        (lambda: key(Qt.Key.Key_Tab, Qt.KeyboardModifier.ShiftModifier), False, "\x1b[Z"),
        (lambda: key(Qt.Key.Key_Left, Qt.KeyboardModifier.AltModifier), False, "\x1bb"),  # a word back
        (lambda: key(Qt.Key.Key_F5), False, "\x1b[15~"),
        (lambda: key(Qt.Key.Key_A, text="a"), False, "a"),
    ],
)
def test_key_translation(qapp, event, app_cursor, expected):
    assert key_to_text(event(), app_cursor=app_cursor) == expected


# -- the panel in the window -----------------------------------------------------------------------


@pytest.fixture
def term_window(qtbot, tree: Path):
    from orchard_commander.config import Settings
    from orchard_commander.ui.main_window import MainWindow
    from orchard_commander.ui.style import ThemeManager

    themes = ThemeManager()
    themes.apply()
    # /bin/sh without a user profile: fast and predictable
    w = MainWindow(Settings(show_hidden=False, terminal_shell="/bin/sh"), themes, tree / "src", tree / "dst")
    qtbot.addWidget(w)
    w.show()
    w.left.panel.setFocus()
    settle(w)
    yield w
    w.terminal.close_all()


def test_one_shell_per_side(qtbot, term_window, tree: Path):
    w = term_window
    w.toggle_terminal()
    assert w.terminal.isVisible() and w.terminal.view.hasFocus()
    left = started(w, "left")
    run(left, "pwd; echo LEFT-$((1+1))")  # wait for the output, not the echoed command line
    wait_until(lambda: "LEFT-2" in screen_text(left))
    assert str((tree / "src").resolve()) in screen_text(left).replace("\n", "")  # the left panel's folder

    w.right.panel.setFocus()  # switching the active side swaps the session shown
    settle(w)
    right = started(w, "right")
    assert w.terminal.view.session is right and right is not left
    run(right, "echo RIGHT-$((1+1))")
    wait_until(lambda: "RIGHT-2" in screen_text(right))
    assert "RIGHT-" not in screen_text(left) and left.alive  # the other keeps running
    w.left.panel.setFocus()
    settle(w)
    assert w.terminal.view.session is left


def test_terminal_keeps_keys_from_app_shortcuts(qtbot, term_window, tree: Path, monkeypatch):
    w = term_window
    w.toggle_terminal()
    sent = []
    monkeypatch.setattr(started(w, "left"), "write", sent.append)
    view = w.terminal.view
    for k in (Qt.Key.Key_Escape, Qt.Key.Key_Tab, Qt.Key.Key_F5):  # Stop search, switch panel, Copy
        qtbot.keyClick(view, k)
    assert sent == ["\x1b", "\t", "\x1b[15~"] and view.hasFocus()


def test_toggle_and_go_to_panel_folder(qtbot, term_window, tree: Path):
    w = term_window
    w.toggle_terminal()  # show + focus
    w.left.panel.setFocus()
    w.toggle_terminal()  # visible, but the panel had focus: focus the terminal
    assert w.terminal.isVisible() and w.terminal.view.hasFocus()
    session = started(w, "left")
    w.left.panel.load(w.left.panel.location.child("sub"))
    settle(w)
    w.terminal_to_panel_folder()
    wait_until(lambda: "sub" in screen_text(session))
    run(session, "pwd")
    wait_until(lambda: str((tree / "src" / "sub").resolve()) in screen_text(session).replace("\n", ""))
    w.toggle_terminal()  # from inside: hide, back to the panels
    assert not w.terminal.isVisible() and w.left.panel.table.hasFocus()


def test_restart_after_exit(qtbot, term_window, tree: Path):
    w = term_window
    w.toggle_terminal()
    first = started(w, "left")
    run(first, "exit")
    wait_until(lambda: "press Enter" in screen_text(first))
    qtbot.keyClick(w.terminal.view, Qt.Key.Key_Return)
    second = started(w, "left")
    assert second is not first and second.alive


@pytest.mark.skipif(not IS_MACOS, reason="⌘J is the macOS alternative")
def test_cmd_j_toggles_from_inside(qtbot, term_window):
    w = term_window
    qtbot.keyClick(w.left.panel.table, Qt.Key.Key_J, Qt.KeyboardModifier.ControlModifier)  # ⌘J
    assert w.terminal.isVisible() and w.terminal.view.hasFocus()
    qtbot.keyClick(w.terminal.view, Qt.Key.Key_J, Qt.KeyboardModifier.ControlModifier)
    assert not w.terminal.isVisible() and w.left.panel.table.hasFocus()


# -- mouse -----------------------------------------------------------------------------------------

SGR_DRAGS = {terminal.MOUSE_DRAGS, terminal.MOUSE_SGR}


@pytest.mark.parametrize(
    ("mode", "kwargs", "expected"),
    [
        (SGR_DRAGS, dict(button=terminal.LEFT, column=4, line=2), b"\x1b[<0;5;3M"),
        (SGR_DRAGS, dict(button=terminal.LEFT, column=4, line=2, press=False), b"\x1b[<0;5;3m"),
        (SGR_DRAGS, dict(button=terminal.RIGHT, column=0, line=0, ctrl=True), b"\x1b[<18;1;1M"),
        (SGR_DRAGS, dict(button=terminal.LEFT, column=7, line=1, motion=True), b"\x1b[<32;8;2M"),
        (SGR_DRAGS, dict(button=terminal.NO_BUTTON, column=7, line=1, motion=True), None),  # drags only
        ({terminal.MOUSE_MOTION, terminal.MOUSE_SGR},
         dict(button=terminal.NO_BUTTON, column=7, line=1, motion=True), b"\x1b[<35;8;2M"),
        (SGR_DRAGS, dict(button=terminal.WHEEL_DOWN, column=1, line=1), b"\x1b[<65;2;2M"),
        (SGR_DRAGS, dict(button=terminal.WHEEL_UP, column=1, line=1, press=False), None),
        (SGR_DRAGS, dict(button=terminal.LEFT, column=400, line=90), b"\x1b[<0;401;91M"),  # any size
        ({terminal.MOUSE_CLICKS}, dict(button=terminal.LEFT, column=4, line=2), b"\x1b[M %#"),  # legacy
        ({terminal.MOUSE_CLICKS}, dict(button=terminal.MIDDLE, column=4, line=2, press=False), b"\x1b[M#%#"),
        ({terminal.MOUSE_CLICKS}, dict(button=terminal.LEFT, column=300, line=2), None),  # too wide for it
        ({terminal.MOUSE_CLICKS, terminal.MOUSE_URXVT}, dict(button=terminal.LEFT, column=300, line=2),
         b"\x1b[32;301;3M"),
        ({terminal.MOUSE_CLICKS}, dict(button=terminal.LEFT, column=4, line=2, motion=True), None),
        ({terminal.MOUSE_X10}, dict(button=terminal.LEFT, column=4, line=2, press=False), None),
        (set(), dict(button=terminal.LEFT, column=4, line=2), None),  # nobody asked
    ],
)
def test_mouse_encoding(mode, kwargs, expected):
    assert terminal.encode_mouse(mode, **kwargs) == expected


def cell_point(view, column: int, line: int):
    from PySide6.QtCore import QPoint

    return QPoint(int((column + 0.5) * view._cw), int((line + 0.5) * view._ch))


@pytest.fixture
def mouse_term(qtbot, term_window, monkeypatch):
    term_window.toggle_terminal()
    session = started(term_window, "left")
    sent: list = []
    monkeypatch.setattr(session, "write", sent.append)
    return term_window.terminal.view, session, sent


def test_clicks_drags_and_wheel_go_to_the_program(qtbot, mouse_term):
    view, session, sent = mouse_term
    session.feed("\x1b[?1002h\x1b[?1006h")  # what vim/htop send to ask for the mouse
    left = Qt.MouseButton.LeftButton
    qtbot.mousePress(view, left, pos=cell_point(view, 2, 1))
    from PySide6.QtCore import QPointF
    from PySide6.QtGui import QMouseEvent

    move = QMouseEvent(QEvent.Type.MouseMove, QPointF(cell_point(view, 5, 1)), QPointF(), Qt.MouseButton.NoButton,
                       left, Qt.KeyboardModifier.NoModifier)
    view.mouseMoveEvent(move)
    qtbot.mouseRelease(view, left, pos=cell_point(view, 5, 1))
    assert sent == [b"\x1b[<0;3;2M", b"\x1b[<32;6;2M", b"\x1b[<0;6;2m"]
    assert view._selection is None  # the program got it: no local selection

    sent.clear()
    from PySide6.QtCore import QPoint
    from PySide6.QtGui import QWheelEvent

    wheel = QWheelEvent(QPointF(cell_point(view, 1, 1)), QPointF(), QPoint(), QPoint(0, -120),
                        Qt.MouseButton.NoButton, Qt.KeyboardModifier.NoModifier, Qt.ScrollPhase.NoScrollPhase, False)
    view.wheelEvent(wheel)
    assert sent == [b"\x1b[<65;2;2M"] * 3  # three steps down


def test_shift_selects_even_when_the_program_has_the_mouse(qtbot, mouse_term):
    view, session, sent = mouse_term
    session.feed("hello world\r\n\x1b[?1000h\x1b[?1006h")
    shift = Qt.KeyboardModifier.ShiftModifier
    qtbot.mousePress(view, Qt.MouseButton.LeftButton, shift, cell_point(view, 0, 0))
    qtbot.mouseRelease(view, Qt.MouseButton.LeftButton, shift, cell_point(view, 0, 0))
    assert sent == []


def test_double_and_triple_click_select(qtbot, mouse_term):
    view, session, sent = mouse_term
    session.feed("\x1b[2J\x1b[Hls ~/src/project.txt done\r\n")
    with session.lock:
        line = len(session.screen.history_lines())
    qtbot.mouseDClick(view, Qt.MouseButton.LeftButton, pos=cell_point(view, 8, 0))
    assert view.selected_text() == "~/src/project.txt"  # a path counts as one word
    view._selection = None
    view._last_click = None
    qtbot.mouseClick(view, Qt.MouseButton.LeftButton, pos=cell_point(view, 1, 0))
    qtbot.mouseDClick(view, Qt.MouseButton.LeftButton, pos=cell_point(view, 1, 0))
    qtbot.mousePress(view, Qt.MouseButton.LeftButton, pos=cell_point(view, 1, 0))  # third click
    assert view.selected_text().strip() == "ls ~/src/project.txt done" and line >= 0
    assert sent == []


def test_focus_reports(qtbot, mouse_term, term_window):
    view, session, sent = mouse_term
    session.feed("\x1b[?1004h")
    term_window.left.panel.setFocus()
    view.setFocus()
    assert sent == ["\x1b[O", "\x1b[I"]


def test_a_real_program_receives_clicks(qtbot, term_window, tmp_path: Path):
    """End to end: a program in the pseudo-terminal turns on SGR mouse mode and reads a click."""
    script = tmp_path / "mouse.py"
    script.write_text(
        "import sys, termios, tty\n"
        "tty.setraw(0)\n"
        "sys.stdout.write('\\x1b[?1000h\\x1b[?1006hREADY\\r\\n'); sys.stdout.flush()\n"
        "data = b''\n"
        "while not data.endswith(b'm'):\n"
        "    data += __import__('os').read(0, 64)\n"
        "sys.stdout.write('GOT ' + repr(data) + '\\r\\n'); sys.stdout.flush()\n"
    )
    w = term_window
    w.settings.terminal_shell = f"{sys.executable} {script}"
    w.toggle_terminal()
    session = started(w, "left")
    wait_until(lambda: "READY" in screen_text(session))
    view = w.terminal.view
    qtbot.mouseClick(view, Qt.MouseButton.LeftButton, pos=cell_point(view, 9, 4))
    wait_until(lambda: "GOT" in screen_text(session))
    assert r"GOT b'\x1b[<0;10;5M\x1b[<0;10;5m'" in screen_text(session)


# -- linked folders --------------------------------------------------------------------------------


def shell_dir(session: terminal.Session) -> Path | None:
    current = session.current_dir()
    return current.resolve() if current is not None else None


@pytest.fixture
def linked(qtbot, term_window):
    term_window.toggle_terminal()
    session = started(term_window, "left")
    wait_until(lambda: session.at_prompt() and term_window._shell_dirs.get("left") is not None)
    return term_window, session


def test_cd_in_the_terminal_moves_the_panel(qtbot, linked, tree: Path):
    w, session = linked
    run(session, "cd sub")
    wait_until(lambda: w.left.panel.location == vfs_local(tree / "src" / "sub"))
    run(session, "cd ..")
    wait_until(lambda: w.left.panel.location == vfs_local(tree / "src"))
    assert w.right.panel.location == vfs_local(tree / "dst")  # the other side keeps its own


def test_moving_the_panel_moves_the_shell(qtbot, linked, tree: Path):
    w, session = linked
    w.left.panel.load(vfs_local(tree / "dst"))
    wait_until(lambda: shell_dir(session) == (tree / "dst").resolve())
    settle(w)
    assert w.left.panel.location == vfs_local(tree / "dst")  # and it stays there (no ping-pong)


def test_no_typing_into_a_running_program(qtbot, linked, tree: Path):
    w, session = linked
    run(session, "sleep 1.5; echo SLEPT")
    wait_until(lambda: not session.at_prompt())
    w.left.panel.load(vfs_local(tree / "src" / "sub"))
    settle(w)
    time.sleep(0.4)
    assert shell_dir(session) == (tree / "src").resolve()  # sleep is still running: nothing typed
    wait_until(lambda: "SLEPT" in screen_text(session))
    wait_until(lambda: shell_dir(session) == (tree / "src" / "sub").resolve())  # caught up afterwards


def test_panels_refresh_after_each_command(qtbot, linked, tree: Path):
    w, session = linked
    run(session, "rm a.txt")
    wait_until(lambda: "a.txt" not in [e.name for e in w.left.panel.entries])
    size = next(e.size for e in w.left.panel.entries if e.name == "b.bin")
    run(session, "echo more >> b.bin")  # same folder entries, only a size changes: folder watching misses it
    wait_until(lambda: next(e.size for e in w.left.panel.entries if e.name == "b.bin") == size + 5)


def test_link_can_be_switched_off(qtbot, linked, tree: Path):
    w, session = linked
    w.apply_settings(terminal_link=False)
    run(session, "cd sub")
    wait_until(lambda: shell_dir(session) == (tree / "src" / "sub").resolve())
    time.sleep(0.5)
    settle(w)
    assert w.left.panel.location == vfs_local(tree / "src")


def vfs_local(path: Path):
    from orchard_commander import vfs

    return vfs.local(path)


# -- glyphs ----------------------------------------------------------------------------------------


def render_cell(char: str):
    from PySide6.QtCore import QRectF
    from PySide6.QtGui import QColor, QImage, QPainter

    from orchard_commander.ui import term_glyphs

    image = QImage(10, 20, QImage.Format.Format_RGB32)
    image.fill(QColor("black"))
    p = QPainter(image)
    term_glyphs.draw(p, char, QRectF(0, 0, 10, 20), QColor("red"))
    p.end()
    return lambda x, y: image.pixelColor(x, y).red() > 40  # ░ is drawn at 25 %


@pytest.mark.parametrize("char", ["", "", "", "─", "│", "┼", "╭", "═", "█", "▄", "░"])
def test_cell_glyphs_are_drawn(qapp, char):
    from orchard_commander.ui import term_glyphs

    assert term_glyphs.handles(char)
    red = render_cell(char)
    painted = sum(red(x, y) for x in range(10) for y in range(20))
    assert painted > 0


def test_powerline_separator_fills_to_the_cell_edges(qapp):
    red = render_cell("")  # a right-pointing triangle: the whole left edge, the tip at the right
    assert all(red(0, y) for y in range(1, 19)) and red(9, 10) and not red(9, 1)
    full = render_cell("█")
    assert all(full(x, y) for x in range(10) for y in range(20))
    line = render_cell("─")  # a horizontal line runs from edge to edge, so neighbours touch
    assert red_row(line, 10)


def red_row(red, y: int) -> bool:
    return all(red(x, y) or red(x, y - 1) for x in range(10))


def test_nerd_font_fallback(qapp):
    from orchard_commander.ui import terminal_view

    terminal_view._nerd_fonts = None
    fonts = terminal_view.nerd_fonts()
    assert all("Nerd" in f or " NF" in f for f in fonts)  # only Nerd Fonts (whatever is installed here)


def test_shell_starts_once_at_its_final_size(qtbot, term_window):
    w = term_window
    w.toggle_terminal()
    session = started(w, "left")
    with session.lock:
        size = (session.screen.columns, session.screen.lines)
    assert size == w.terminal.view.grid_size()  # no resize right after starting (it'd redraw prompts)
