"""Shell sessions for the terminal panel: a pseudo-terminal plus a VT100/xterm screen.

`Session` runs the user's shell (their login shell; PowerShell on Windows)
in a pseudo-terminal — ptyprocess on macOS/Linux, ConPTY via pywinpty on
Windows — and feeds its output into a pyte screen on a reader thread. The UI
(`ui/terminal_view.py`) only reads the screen (under `Session.lock`) and
writes keystrokes; nothing here imports Qt.
"""

from __future__ import annotations

import codecs
import os
import re
import shutil
import sys
import threading
from collections import deque
from collections.abc import Callable
from pathlib import Path

import pyte

IS_WINDOWS = sys.platform == "win32"
SCROLLBACK = 5000  # lines kept above the screen

# pyte keeps private modes (CSI ? n h) shifted by 5 bits in `screen.mode`.
APP_CURSOR_KEYS = 1 << 5  # DECCKM: arrows send ESC O A instead of ESC [ A
BRACKETED_PASTE = 2004 << 5
ALT_SCREENS = (47, 1047, 1049)  # full-screen programs (vim, less, htop) draw on a second screen
# Mouse reporting (xterm): which events, and how they're encoded.
MOUSE_X10, MOUSE_CLICKS, MOUSE_DRAGS, MOUSE_MOTION = (9 << 5, 1000 << 5, 1002 << 5, 1003 << 5)
FOCUS_EVENTS = 1004 << 5  # "\x1b[I" / "\x1b[O" when the terminal gains / loses focus (vim)
MOUSE_UTF8, MOUSE_SGR, MOUSE_URXVT = 1005 << 5, 1006 << 5, 1015 << 5

LEFT, MIDDLE, RIGHT, NO_BUTTON = 0, 1, 2, 3
WHEEL_UP, WHEEL_DOWN, WHEEL_LEFT, WHEEL_RIGHT = 64, 65, 66, 67


def mouse_tracking(mode: set[int]) -> str | None:
    """What the program wants reported: "motion", "drags", "clicks", "x10" — or None."""
    for flag, name in ((MOUSE_MOTION, "motion"), (MOUSE_DRAGS, "drags"), (MOUSE_CLICKS, "clicks"), (MOUSE_X10, "x10")):
        if flag in mode:
            return name
    return None


def encode_mouse(mode: set[int], button: int, column: int, line: int, *, press: bool = True,
                 motion: bool = False, shift: bool = False, alt: bool = False, ctrl: bool = False) -> bytes | None:
    """The bytes reporting a mouse event at (column, line) — 0-based — or None if not to be sent.

    `button`: LEFT/MIDDLE/RIGHT, NO_BUTTON (motion without a button) or a WHEEL_* value.
    """
    tracking = mouse_tracking(mode)
    if tracking is None:
        return None
    wheel = button >= WHEEL_UP
    if tracking == "x10":  # presses only, no modifiers
        if not press or motion:
            return None
        code = button
    else:
        if motion and (tracking == "clicks" or (tracking == "drags" and button == NO_BUTTON)):
            return None
        if wheel and not press:
            return None  # wheels have no release
        code = button + (32 if motion else 0) + (4 if shift else 0) + (8 if alt else 0) + (16 if ctrl else 0)
    x, y = column + 1, line + 1
    if MOUSE_SGR in mode:  # modern: any size, releases keep the button
        return f"\x1b[<{code};{x};{y}{'M' if press else 'm'}".encode()
    if not press:
        code = (code & ~3) | 3  # legacy: a release doesn't say which button
    if MOUSE_URXVT in mode:
        return f"\x1b[{code + 32};{x};{y}M".encode()
    if MOUSE_UTF8 in mode:
        return ("\x1b[M" + "".join(chr(v + 32) for v in (code, x, y))).encode()
    if x + 32 > 255 or y + 32 > 255:
        return None  # the classic encoding can't express it
    return b"\x1b[M" + bytes((code + 32, x + 32, y + 32))


# -- where the shell is ---------------------------------------------------------------------------

_OSC7 = re.compile(r"\x1b\]7;([^\x07\x1b]*)(?:\x07|\x1b\\)")

# PowerShell's Set-Location doesn't change the process's directory: its prompt reports it instead
# (OSC 7, which some POSIX shell setups send too).
POWERSHELL_INTEGRATION = (
    "$__orchardPrompt = $function:prompt; "
    "function global:prompt { "
    "$p = (Get-Location).ProviderPath -replace '\\\\', '/'; "
    "[Console]::Write([char]27 + ']7;file:///' + $p + [char]7); & $__orchardPrompt }"
)


def is_powershell(argv: list[str]) -> bool:
    return Path(argv[0]).stem.lower() in ("pwsh", "powershell")


def with_integration(argv: list[str]) -> list[str]:
    """`argv` plus what makes the shell report its folder, where needed (PowerShell)."""
    if is_powershell(argv) and "-Command" not in argv:
        return [*argv, "-NoExit", "-Command", POWERSHELL_INTEGRATION]
    return argv


def path_from_osc7(value: str) -> Path | None:
    """file://host/path (percent-encoded) -> Path."""
    from urllib.parse import unquote, urlparse

    url = urlparse(value)
    if url.scheme != "file" or not url.path:
        return None
    path = unquote(url.path)
    if IS_WINDOWS and re.match(r"^/[A-Za-z]:", path):
        path = path[1:]
    return Path(path)


def process_cwd(pid: int) -> Path | None:
    """A process's working directory (macOS, Linux); None where it can't be read."""
    try:
        if sys.platform == "darwin":
            import ctypes

            global _libproc
            if _libproc is None:
                _libproc = ctypes.CDLL("/usr/lib/libproc.dylib")
            # struct proc_vnodepathinfo: the cwd's vnode_info (152 bytes), then its path (1024)
            buf = ctypes.create_string_buffer(2352)
            if _libproc.proc_pidinfo(pid, 9, 0, buf, 2352) <= 0:  # PROC_PIDVNODEPATHINFO
                return None
            path = buf.raw[152:152 + 1024].split(b"\0", 1)[0]
            return Path(os.fsdecode(path)) if path else None
        if sys.platform.startswith("linux"):
            return Path(os.readlink(f"/proc/{pid}/cwd"))
    except (OSError, AttributeError):
        return None
    return None


_libproc = None


def cd_command(argv: list[str], folder: Path) -> str:
    """What to type so the shell changes to `folder` (leading space: kept out of most histories)."""
    if is_powershell(argv):
        quoted = "'" + str(folder).replace("'", "''") + "'"
        return f"\x1b Set-Location -LiteralPath {quoted}\r"  # Esc clears the line in PSReadLine
    if Path(argv[0]).stem.lower() == "cmd":
        return f'\x1b cd /d "{folder}"\r'
    import shlex

    return f"\x15 cd {shlex.quote(str(folder))}\r"  # Ctrl+U: clear what was typed first


def default_shell() -> list[str]:
    """The user's shell as a command line: their login shell, or PowerShell on Windows."""
    if IS_WINDOWS:
        for name in ("pwsh.exe", "powershell.exe"):
            if path := shutil.which(name):
                return [path, "-NoLogo"]
        return [os.environ.get("COMSPEC", "cmd.exe")]
    shell = os.environ.get("SHELL", "")
    if not shell:
        try:
            import pwd

            shell = pwd.getpwuid(os.getuid()).pw_shell
        except (ImportError, KeyError):
            shell = ""
    shell = shell if shell and Path(shell).exists() else "/bin/sh"
    # A login shell reads the user's profile: apps started from the Dock get a minimal PATH otherwise.
    return [shell, "-l"]


def shell_command(configured: str) -> list[str]:
    """The command for a configured shell ("" = the default), split like a command line."""
    if not configured.strip():
        return default_shell()
    if IS_WINDOWS:
        return [configured.strip()]
    import shlex

    return shlex.split(configured)


def shell_environment() -> dict[str, str]:
    env = dict(os.environ)
    env.update(TERM="xterm-256color", COLORTERM="truecolor", TERM_PROGRAM="OrchardCommander")
    if not IS_WINDOWS and not any(env.get(k) for k in ("LC_ALL", "LC_CTYPE", "LANG")):
        env["LANG"] = "en_US.UTF-8"  # apps from the Dock have no locale: shells would fall back to ASCII
    return env


class Screen(pyte.HistoryScreen):
    """pyte's screen plus what full-screen programs and prompts expect.

    - an alternate screen (vim/less leave the shell's screen as it was),
    - replies to queries (cursor position, device attributes) go back to the program.
    """

    def __init__(self, columns: int, lines: int, respond: Callable[[str], None]) -> None:
        super().__init__(columns, lines, history=SCROLLBACK, ratio=0.5)
        self._respond = respond
        self._saved: tuple[dict[int, dict], tuple[int, int]] | None = None

    @property
    def alternate(self) -> bool:
        return self._saved is not None

    def write_process_input(self, data: str) -> None:
        self._respond(data)

    def set_mode(self, *modes: int, **kwargs) -> None:
        if kwargs.get("private") and any(m in ALT_SCREENS for m in modes) and self._saved is None:
            self._saved = ({y: dict(line) for y, line in self.buffer.items()}, (self.cursor.x, self.cursor.y))
            self.erase_in_display(2)
        super().set_mode(*modes, **kwargs)

    def reset_mode(self, *modes: int, **kwargs) -> None:
        if kwargs.get("private") and any(m in ALT_SCREENS for m in modes) and self._saved is not None:
            lines, (x, y) = self._saved
            self._saved = None
            self.buffer.clear()
            for row, line in lines.items():
                if row < self.lines:
                    self.buffer[row].update(line)
            self.cursor_position(y + 1, x + 1)
        super().reset_mode(*modes, **kwargs)

    def index(self) -> None:
        if self._saved is not None:  # the alternate screen has no scrollback
            pyte.Screen.index(self)
        else:
            super().index()

    def history_lines(self) -> deque:
        return self.history.top

    def resize(self, lines: int | None = None, columns: int | None = None) -> None:
        super().resize(lines, columns)
        # pyte keeps the cursor where it was; keep it on the screen.
        self.cursor.y = min(self.cursor.y, self.lines - 1)
        self.cursor.x = min(self.cursor.x, self.columns - 1)


class Session:
    """One shell in a pseudo-terminal. Thread-safe: read the screen under `lock`."""

    def __init__(self, argv: list[str], cwd: Path, *, columns: int = 80, lines: int = 24,
                 on_output: Callable[[], None] = lambda: None,
                 on_exit: Callable[[], None] = lambda: None) -> None:
        self.argv = argv
        self.cwd = cwd
        self.lock = threading.RLock()
        self.screen = Screen(columns, lines, self.write)
        self._stream = pyte.Stream(self.screen)
        self._on_output = on_output
        self._on_exit = on_exit
        self._process = None
        self.exit_status: int | None = None
        self._decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
        self.reported_cwd: Path | None = None  # from OSC 7, if the shell sends it
        self._osc_tail = ""
        self._busy = False  # Windows: a command runs (Enter typed, no prompt since)
        self.commands = 0  # counts Enter presses: the window refreshes panels when a command finished

    @property
    def title(self) -> str:
        return Path(self.argv[0]).stem

    @property
    def alive(self) -> bool:
        return self._process is not None and self._process.isalive()

    def start(self) -> None:
        cwd = str(self.cwd if self.cwd.is_dir() else Path.home())
        size = (self.screen.lines, self.screen.columns)
        env = shell_environment()
        if IS_WINDOWS:
            from winpty import PtyProcess  # pywinpty (ConPTY)

            self._process = PtyProcess.spawn(with_integration(self.argv), cwd=cwd, env=env, dimensions=size)
        else:
            import warnings

            from ptyprocess import PtyProcess

            with warnings.catch_warnings():
                # forkpty() in a threaded process: the child only execs the shell (as pexpect does).
                warnings.simplefilter("ignore", DeprecationWarning)
                self._process = PtyProcess.spawn(with_integration(self.argv), cwd=cwd, env=env, dimensions=size)
        threading.Thread(target=self._read_loop, daemon=True, name=f"terminal {self.title}").start()

    def _read_loop(self) -> None:
        process = self._process
        while True:
            try:
                data = process.read(65536)
            except (EOFError, OSError):
                break
            text = data if isinstance(data, str) else self._decoder.decode(data)
            if text:
                self._scan_osc7(text)
                self.feed(text)
        try:
            process.wait()
            self.exit_status = process.exitstatus
        except Exception:  # already reaped, or Windows without a status
            pass
        self._on_exit()

    def _scan_osc7(self, text: str) -> None:
        scan = self._osc_tail + text
        matches = list(_OSC7.finditer(scan))
        if matches:
            path = path_from_osc7(matches[-1].group(1))
            if path is not None:
                self.reported_cwd = path
            self._busy = False  # a prompt was drawn
        start = scan.rfind("\x1b]7;")
        self._osc_tail = scan[start:][-1024:] if start != -1 and (not matches or start > matches[-1].start()) else ""

    @property
    def pid(self) -> int | None:
        return self._process.pid if self._process is not None else None

    def current_dir(self) -> Path | None:
        """Where the shell is now: what it reported (OSC 7), else its process's directory."""
        if self.reported_cwd is not None:
            return self.reported_cwd
        pid = self.pid
        return process_cwd(pid) if pid is not None and self.alive else None

    def at_prompt(self) -> bool:
        """Whether the shell itself is in the foreground (no program running), so typing is safe."""
        if not self.alive:
            return False
        if IS_WINDOWS:
            return not self._busy
        try:
            return os.tcgetpgrp(self._process.fd) == self._process.pid
        except OSError:
            return False

    def change_dir(self, folder: Path) -> None:
        """Type a cd into the shell (check `at_prompt()` first)."""
        self.write(cd_command(self.argv, folder))

    def feed(self, text: str) -> None:
        """Process output (from the shell, or a message of our own)."""
        with self.lock:
            self._stream.feed(text)
        self._on_output()

    def write(self, text: str | bytes) -> None:
        """Send keystrokes / pasted text (or raw bytes, e.g. mouse reports) to the shell."""
        if self._process is None or not text:
            return
        if isinstance(text, str) and "\r" in text:
            self.commands += 1
        if IS_WINDOWS:
            data = text if isinstance(text, str) else text.decode("latin-1")
            if "\r" in data:
                self._busy = True  # until the next prompt reports in
        else:
            data = text if isinstance(text, bytes) else text.encode("utf-8")
        try:
            self._process.write(data)
        except (OSError, EOFError):
            pass

    def resize(self, columns: int, lines: int) -> None:
        columns, lines = max(columns, 2), max(lines, 2)
        with self.lock:
            if (columns, lines) == (self.screen.columns, self.screen.lines):
                return
            self.screen.resize(lines, columns)
        if self._process is not None:
            try:
                self._process.setwinsize(lines, columns)
            except (OSError, EOFError):
                pass

    def close(self) -> None:
        process, self._process = self._process, None
        if process is not None and process.isalive():
            try:
                process.terminate(force=True)
            except Exception:
                pass
