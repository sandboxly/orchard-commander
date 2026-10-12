"""The terminal on Windows: ConPTY sessions and PowerShell reporting its folder (CI runs these)."""

from __future__ import annotations

import sys
import threading
import time
from pathlib import Path

import pytest

from orchard_commander import terminal

pytestmark = pytest.mark.skipif(sys.platform != "win32", reason="Windows only (ConPTY, PowerShell)")


def screen_text(session: terminal.Session) -> str:
    with session.lock:
        return "\n".join(line.rstrip() for line in session.screen.display)


def eventually(predicate, timeout: float = 20.0) -> None:
    deadline = time.monotonic() + timeout
    while not predicate():
        if time.monotonic() > deadline:
            raise AssertionError("condition not met in time")
        time.sleep(0.05)


def test_default_shell_is_powershell():
    argv = terminal.default_shell()
    assert terminal.is_powershell(argv) and Path(argv[0]).exists()


def test_conpty_session_runs_a_command(tmp_path: Path):
    done = threading.Event()
    session = terminal.Session(["cmd.exe", "/c", "echo terminal-ok"], tmp_path, on_exit=done.set)
    session.start()
    assert done.wait(20)
    assert "terminal-ok" in screen_text(session)


def test_powershell_reports_its_folder(tmp_path: Path):
    (tmp_path / "sub").mkdir()
    session = terminal.Session(terminal.default_shell(), tmp_path, columns=100, lines=30)
    session.start()
    try:
        eventually(lambda: session.reported_cwd is not None)  # the prompt hook sends OSC 7
        assert session.reported_cwd.resolve() == tmp_path.resolve()
        assert session.at_prompt()
        session.change_dir(tmp_path / "sub")
        eventually(lambda: session.reported_cwd is not None and session.reported_cwd.name == "sub")
        assert session.current_dir().resolve() == (tmp_path / "sub").resolve()
    finally:
        session.close()
