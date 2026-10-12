from __future__ import annotations

import time
from pathlib import Path

import pytest
from PySide6.QtCore import QEventLoop, Qt, QTimer
from PySide6.QtWidgets import QApplication

from orchard_commander import fs, vfs
from orchard_commander.config import Settings
from orchard_commander.ui import dialogs
from orchard_commander.ui.main_window import FILES, PREVIEW, MainWindow
from orchard_commander.ui.preview_pane import PreviewPane
from orchard_commander.ui.style import ThemeManager


def wait(ms: int) -> None:
    # A real event loop: QTest.qWait doesn't release the GIL, which starves worker threads.
    loop = QEventLoop()
    QTimer.singleShot(ms, loop.quit)
    loop.exec()


def wait_until(predicate, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while not predicate():
        if time.monotonic() > deadline:
            raise AssertionError("condition not met in time")
        wait(20)


@pytest.fixture
def window(qtbot, tree: Path) -> MainWindow:
    themes = ThemeManager()
    themes.apply()
    # Hidden files off: tests navigate by key presses through a known listing.
    w = MainWindow(Settings(show_hidden=False), themes, tree / "src", tree / "dst")
    qtbot.addWidget(w)
    w.show()
    w.left.panel.setFocus()
    settle(w)
    return w


def shown_text(pane: PreviewPane) -> str:
    """Text of the preview currently shown (text, listing, hex), or the message."""
    view = pane.current_view
    if view is not None and hasattr(view, "plain_text"):
        return view.plain_text()
    return pane.message_label.text() if view is None else ""


def settle(window: MainWindow) -> None:
    """Wait for background directory listings to land."""
    wait_until(lambda: not window.left.panel.loading and not window.right.panel.loading)


def press(qtbot, window: MainWindow, *keys: Qt.Key) -> None:
    for key in keys:
        target = QApplication.focusWidget() or window
        qtbot.keyClick(target, key)
        settle(window)


def select(window: MainWindow, name: str) -> None:
    settle(window)
    panel = window.left.panel
    row = panel.model.row_of(name)
    assert row is not None, name
    panel.table.set_current_row(row)


def test_browse_into_dir_and_back(qtbot, window: MainWindow, tree: Path):
    panel = window.left.panel
    assert panel.cursor_name == ".."
    press(qtbot, window, Qt.Key.Key_Down)
    assert panel.cursor_name == "sub"
    press(qtbot, window, Qt.Key.Key_Return)
    assert panel.location == vfs.local(tree / "src" / "sub")
    press(qtbot, window, Qt.Key.Key_Backspace)
    assert panel.location == vfs.local(tree / "src")
    assert panel.cursor_name == "sub"


def test_tab_switches_sides(qtbot, window: MainWindow):
    assert window.active_panel is window.left.panel
    press(qtbot, window, Qt.Key.Key_Tab)
    assert window.active_panel is window.right.panel
    assert window.right.panel.active and not window.left.panel.active


def test_marks(qtbot, window: MainWindow):
    press(qtbot, window, Qt.Key.Key_Down, Qt.Key.Key_Down, Qt.Key.Key_Insert, Qt.Key.Key_Space)
    assert window.left.panel.marked == {"a.txt", "b.bin"}
    qtbot.keyClicks(window.left.panel.table, "*")
    assert window.left.panel.marked == {"sub"}


def test_copy_marked_files(qtbot, window: MainWindow, tree: Path, monkeypatch):
    monkeypatch.setattr(dialogs, "ask_text", lambda *a, **k: str(tree / "dst"))
    window.left.panel.model.marked = {"a.txt", "b.bin"}
    window.copy()
    wait_until(lambda: {e.name for e in window.right.panel.entries} == {"a.txt", "b.bin"})
    assert (tree / "dst" / "b.bin").stat().st_size == 3000
    wait_until(lambda: not window.left.panel.marked)


def test_copy_conflict_asks(qtbot, window: MainWindow, tree: Path, monkeypatch):
    (tree / "dst" / "a.txt").write_text("old")
    asked = []
    monkeypatch.setattr(dialogs, "ask_text", lambda *a, **k: str(tree / "dst"))
    monkeypatch.setattr(
        dialogs, "ask_conflict", lambda parent, themes, s, d: asked.append(d) or (fs.Conflict.OVERWRITE, False)
    )
    select(window, "a.txt")
    window.copy()
    wait_until(lambda: (tree / "dst" / "a.txt").read_text() == "alpha")
    assert asked == [tree / "dst" / "a.txt"]


def test_move_rename(qtbot, window: MainWindow, tree: Path, monkeypatch):
    monkeypatch.setattr(dialogs, "ask_text", lambda *a, **k: "renamed.txt")
    select(window, "a.txt")
    window.move()
    wait_until(lambda: (tree / "src" / "renamed.txt").exists())
    assert not (tree / "src" / "a.txt").exists()


def test_trash(qtbot, window: MainWindow, tree: Path, monkeypatch):
    trashed: list[Path] = []
    monkeypatch.setattr(dialogs, "confirm", lambda *a, **k: True)
    monkeypatch.setattr(fs, "send2trash", lambda p: (trashed.append(p), p.unlink()))
    select(window, "a.txt")
    window.trash()
    wait_until(lambda: "a.txt" not in {e.name for e in window.left.panel.entries})
    assert trashed == [tree / "src" / "a.txt"]


def test_open_with_default_app(qtbot, window: MainWindow, tree: Path, monkeypatch):
    opened: list[Path] = []
    monkeypatch.setattr(fs, "open_with_default_app", opened.append)
    select(window, "a.txt")
    press(qtbot, window, Qt.Key.Key_Return)
    assert opened == [tree / "src" / "a.txt"]


def test_hidden_files_shown_by_default(qtbot, tree: Path):
    themes = ThemeManager()
    window = MainWindow(Settings(), themes, tree / "src", tree / "dst")
    qtbot.addWidget(window)
    settle(window)
    assert ".hidden" in {e.name for e in window.left.panel.entries}
    window.toggle_hidden()
    settle(window)
    assert ".hidden" not in {e.name for e in window.left.panel.entries}
    assert Settings.load().show_hidden is False  # an explicit choice is remembered


def test_hidden_toggle_and_color_scheme_persist(qtbot, window: MainWindow, monkeypatch):
    window.toggle_hidden()
    settle(window)
    assert ".hidden" in {e.name for e in window.left.panel.entries}
    monkeypatch.setattr(dialogs.Picker, "choose", lambda self: "nord")
    window.choose_color_scheme()
    assert window.themes.colors.name == "nord"
    saved = Settings.load()
    assert saved.show_hidden and saved.color_scheme == "nord"


def test_preview_follows_cursor_in_other_panel(qtbot, window: MainWindow, tree: Path):
    window.set_mode(window.right, PREVIEW)
    assert window.right.mode == PREVIEW
    assert window.active_panel is window.left.panel  # focus stays with the file list
    pane = window.right.preview

    select(window, "a.txt")
    wait_until(lambda: pane.current_id == "text" and shown_text(pane) == "alpha")
    select(window, "sub")
    wait_until(lambda: "c.txt" in shown_text(pane))
    select(window, "b.bin")
    wait_until(lambda: "00000000  00 00" in shown_text(pane))


def test_image_preview(qtbot, window: MainWindow, tree: Path):
    from PySide6.QtGui import QColor, QImage

    img = QImage(64, 32, QImage.Format.Format_RGB32)
    img.fill(QColor("red"))
    img.save(str(tree / "src" / "pic.png"))
    window.left.panel.reload()
    settle(window)
    window.set_mode(window.right, PREVIEW)
    select(window, "pic.png")
    pane = window.right.preview
    wait_until(lambda: pane.current_id == "image")
    assert "64×32" in pane._subtitle


def test_image_context_menu_opens_and_copies(qtbot, window: MainWindow, tree: Path, monkeypatch):
    from PySide6.QtGui import QColor, QGuiApplication, QImage

    from orchard_commander.preview.builtin import image as image_previewer

    big = QImage(5000, 300, QImage.Format.Format_RGB32)  # wider than previews decode
    big.fill(QColor("blue"))
    big.save(str(tree / "src" / "wide.png"))
    window.left.panel.reload()
    settle(window)
    window.set_mode(window.right, PREVIEW)
    select(window, "wide.png")
    pane = window.right.preview
    wait_until(lambda: pane.current_id == "image")
    view = pane.current_view
    assert view.data.image.width() < 5000  # the preview is scaled down

    menu = view.context_menu()
    actions = {a.text(): a for a in menu.actions()}
    assert list(actions) == ["Open", "Copy"] and all(a.isEnabled() for a in actions.values())

    opened = []
    monkeypatch.setattr(image_previewer.fs, "open_with_default_app", opened.append)
    actions["Open"].trigger()
    assert opened == [tree / "src" / "wide.png"]

    actions["Copy"].trigger()
    copied = QGuiApplication.clipboard().image()
    assert (copied.width(), copied.height()) == (5000, 300)  # full size, not the preview's
    assert window.toast.isVisible() and "Copied wide.png" in window.toast.text()

    view.clear()
    assert not any(a.isEnabled() for a in view.context_menu().actions())


def test_only_one_side_previews(qtbot, window: MainWindow):
    window.set_mode(window.right, PREVIEW)
    window.set_mode(window.left, PREVIEW)
    assert window.left.mode == PREVIEW and window.right.mode == FILES
    assert window.active_panel is window.right.panel


def test_f3_toggles_quick_view(qtbot, window: MainWindow):
    press(qtbot, window, Qt.Key.Key_F3)
    assert window.right.mode == PREVIEW
    press(qtbot, window, Qt.Key.Key_F3)
    assert window.right.mode == FILES


@pytest.mark.skipif(not __import__("shutil").which("ffmpeg"), reason="needs ffmpeg to create a sample video")
def test_video_preview_decodes_frames(qtbot, window: MainWindow, tree: Path):
    import subprocess

    subprocess.run(
        ["ffmpeg", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=25", "-t", "2",
         "-pix_fmt", "yuv420p", str(tree / "src" / "clip.mp4")],
        check=True,
    )
    window.left.panel.reload()
    settle(window)
    window.set_mode(window.right, PREVIEW)
    select(window, "clip.mp4")
    pane = window.right.preview
    wait_until(lambda: pane.current_id == "media")
    frames = []
    pane._views["media"].video.videoSink().videoFrameChanged.connect(lambda f: frames.append(f.size()))
    wait_until(lambda: len(frames) >= 5)
    assert frames[-1].width() == 160
    assert window.active_panel is window.left.panel  # video surface didn't steal focus
    window.set_mode(window.right, FILES)  # leaving preview stops playback
    from PySide6.QtMultimedia import QMediaPlayer

    assert pane._views["media"].player.playbackState() == QMediaPlayer.PlaybackState.StoppedState


# -- archives -------------------------------------------------------------------


@pytest.fixture
def zipped(tree: Path) -> Path:
    """src/pack.zip: docs/readme.md, top.txt, inner.tar.gz (holding deep.txt)."""
    import io
    import tarfile
    import zipfile

    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as t:
        data = b"deep inside"
        info = tarfile.TarInfo("deep.txt")
        info.size = len(data)
        t.addfile(info, io.BytesIO(data))
    path = tree / "src" / "pack.zip"
    with zipfile.ZipFile(path, "w") as z:
        z.writestr("docs/readme.md", "# Hello\n")
        z.writestr("top.txt", "top level")
        z.writestr("inner.tar.gz", buf.getvalue())
    return path


def test_enter_and_leave_archive(qtbot, window: MainWindow, zipped: Path, tree: Path):
    panel = window.left.panel
    panel.reload()
    settle(window)
    select(window, "pack.zip")
    press(qtbot, window, Qt.Key.Key_Return)
    assert isinstance(panel.location.fs, vfs.ArchiveFS)
    assert [e.name for e in panel.entries] == ["docs", "inner.tar.gz", "top.txt"]
    assert panel._display_path().endswith("pack.zip")
    select(window, "docs")
    press(qtbot, window, Qt.Key.Key_Return)
    assert [e.name for e in panel.entries] == ["readme.md"]
    press(qtbot, window, Qt.Key.Key_Backspace)
    assert panel.cursor_name == "docs"
    press(qtbot, window, Qt.Key.Key_Backspace)
    assert panel.location == vfs.local(tree / "src")
    assert panel.cursor_name == "pack.zip"


def test_nested_archive(qtbot, window: MainWindow, zipped: Path):
    panel = window.left.panel
    panel.reload()
    settle(window)
    select(window, "pack.zip")
    press(qtbot, window, Qt.Key.Key_Return)
    select(window, "inner.tar.gz")
    press(qtbot, window, Qt.Key.Key_Return)
    assert [e.name for e in panel.entries] == ["deep.txt"]
    assert panel.location.parent() == vfs.open_archive(vfs.local(zipped))
    press(qtbot, window, Qt.Key.Key_Backspace)
    assert panel.location == vfs.open_archive(vfs.local(zipped)) and panel.cursor_name == "inner.tar.gz"


def test_archive_preview(qtbot, window: MainWindow, zipped: Path):
    window.left.panel.reload()
    settle(window)
    window.set_mode(window.right, PREVIEW)
    pane = window.right.preview
    select(window, "pack.zip")  # an archive previews like a folder
    wait_until(lambda: "docs" in shown_text(pane) and "top.txt" in shown_text(pane))
    press(qtbot, window, Qt.Key.Key_Return)
    select(window, "top.txt")  # a file inside previews like any file
    wait_until(lambda: shown_text(pane) == "top level")
    select(window, "docs")
    wait_until(lambda: "readme.md" in shown_text(pane))


def test_copy_out_of_archive_extracts(qtbot, window: MainWindow, zipped: Path, tree: Path, monkeypatch):
    monkeypatch.setattr(dialogs, "ask_text", lambda *a, **k: str(tree / "dst"))
    panel = window.left.panel
    panel.reload()
    settle(window)
    select(window, "pack.zip")
    press(qtbot, window, Qt.Key.Key_Return)
    panel.model.marked = {"docs", "top.txt"}
    window.copy()
    wait_until(lambda: (tree / "dst" / "top.txt").exists() and (tree / "dst" / "docs" / "readme.md").exists())
    assert (tree / "dst" / "docs" / "readme.md").read_text() == "# Hello\n"


def test_archives_are_read_only(qtbot, window: MainWindow, zipped: Path, monkeypatch):
    messages: list[str] = []
    monkeypatch.setattr(window, "notify", lambda text, severity="info": messages.append(text))
    asked = []
    monkeypatch.setattr(dialogs, "ask_text", lambda *a, **k: asked.append(a) or None)
    monkeypatch.setattr(dialogs, "confirm", lambda *a, **k: asked.append(a) or False)
    panel = window.left.panel
    panel.reload()
    settle(window)
    select(window, "pack.zip")
    press(qtbot, window, Qt.Key.Key_Return)
    select(window, "top.txt")
    window.move()
    window.trash()
    window.mkdir()
    assert len(messages) == 3 and all("read-only" in m for m in messages)
    assert not asked  # refused before any dialog
    # Copying *into* an archive shown in the other panel is refused too.
    window.right.panel.load(panel.location)
    settle(window)
    window.left.panel.load(vfs.local(zipped.parent))
    settle(window)
    select(window, "a.txt")
    window.copy()
    assert "read-only" in messages[-1] and not asked


def test_shift_enter_opens_archive_externally(qtbot, window: MainWindow, zipped: Path, monkeypatch):
    opened: list[Path] = []
    monkeypatch.setattr(fs, "open_with_default_app", opened.append)
    window.left.panel.reload()
    settle(window)
    select(window, "pack.zip")
    qtbot.keyClick(window.left.panel.table, Qt.Key.Key_Return, Qt.KeyboardModifier.ShiftModifier)
    settle(window)
    assert opened == [zipped] and window.left.panel.location.is_local


def test_open_file_inside_archive_extracts_first(qtbot, window: MainWindow, zipped: Path, monkeypatch):
    opened: list[Path] = []
    monkeypatch.setattr(fs, "open_with_default_app", opened.append)
    window.left.panel.reload()
    settle(window)
    select(window, "pack.zip")
    press(qtbot, window, Qt.Key.Key_Return)
    select(window, "top.txt")
    press(qtbot, window, Qt.Key.Key_Return)
    assert len(opened) == 1 and opened[0].name == "top.txt" and opened[0].read_text() == "top level"


# -- servers ------------------------------------------------------------------------


@pytest.fixture
def sftp_server(tmp_path: Path):
    from servers import start_sftp_server

    root = tmp_path / "server"
    (root / "pub").mkdir(parents=True)
    (root / "pub" / "hello.txt").write_text("remote hello")
    (root / "pub" / "old.txt").write_text("delete me")
    server = start_sftp_server(root)
    yield server
    server.stop()


def connect_left(window: MainWindow, server, monkeypatch, memory_keyring) -> list:
    """Connect the left panel via Go to + an sftp:// address, answering the prompts."""
    from orchard_commander.ui import main_window as mw
    from orchard_commander.ui.connect_dialogs import Credentials
    from servers import PASSWORD, USER

    prompts: list = []
    monkeypatch.setattr(dialogs, "ask_text", lambda *a, **k: f"sftp://{USER}@{server.host}:{server.port}/pub")
    monkeypatch.setattr(mw, "confirm_question", lambda parent, themes, title, msg, ok: prompts.append(msg) or True)
    monkeypatch.setattr(mw, "ask_password", lambda *a: prompts.append("password") or Credentials(PASSWORD, True))
    window.goto_path()
    wait_until(lambda: not window.left.panel.location.is_local and not window.left.panel.loading, timeout=10)
    return prompts


def test_sftp_browse_preview_copy_delete(qtbot, window: MainWindow, sftp_server, tree: Path, monkeypatch,
                                        memory_keyring):
    prompts = connect_left(window, sftp_server, monkeypatch, memory_keyring)
    panel = window.left.panel
    assert sftp_server.host_key_fingerprint in prompts[0] and prompts[1] == "password"
    assert [e.name for e in panel.entries] == ["hello.txt", "old.txt"]
    assert panel.location.display.endswith(f":{sftp_server.port}/pub")
    assert any("s3cret" == v for v in memory_keyring.data.values())  # "remember" stored it

    # Preview of a remote file in the right side.
    window.set_mode(window.right, PREVIEW)
    select(window, "hello.txt")
    pane = window.right.preview
    wait_until(lambda: shown_text(pane) == "remote hello")
    window.set_mode(window.right, FILES)
    window.left.panel.setFocus()
    settle(window)
    assert window.active_panel is window.left.panel

    # Download to the local right panel (default target = other panel).
    monkeypatch.setattr(dialogs, "ask_text", lambda parent, themes, title, message, value, ok: value)
    select(window, "hello.txt")
    window.copy()
    wait_until(lambda: (tree / "dst" / "hello.txt").exists())
    assert (tree / "dst" / "hello.txt").read_text() == "remote hello"

    # No trash on servers: F8 deletes permanently after confirmation.
    confirmations = []
    monkeypatch.setattr(dialogs, "confirm", lambda parent, themes, title, msg, ok: confirmations.append(title) or True)
    select(window, "old.txt")
    window.trash()
    wait_until(lambda: not (sftp_server.root / "pub" / "old.txt").exists())
    assert confirmations == ["Delete permanently"]

    # Disconnect returns the panel to the local disk.
    window.left.panel.setFocus()
    window.disconnect()
    settle(window)
    assert window.left.panel.location.is_local


def test_reconnect_reuses_stored_password(qtbot, window: MainWindow, sftp_server, monkeypatch, memory_keyring):
    connect_left(window, sftp_server, monkeypatch, memory_keyring)
    window.disconnect()
    settle(window)
    prompts = connect_left(window, sftp_server, monkeypatch, memory_keyring)
    assert prompts == []  # host key known, password from the keychain


def test_saved_connections_in_location_picker(qtbot, window: MainWindow, sftp_server, monkeypatch, memory_keyring):
    from orchard_commander import connections
    from servers import PASSWORD, USER

    conn = connections.Connection.make(
        "sftp", host=sftp_server.host, username=USER, port=sftp_server.port, path="/pub", name="test box"
    )
    connections.save(conn)
    connections.store_password(conn, PASSWORD)
    from orchard_commander.ui import main_window as mw

    monkeypatch.setattr(mw, "confirm_question", lambda *a: True)
    offered = []

    def choose(self):
        offered.extend(self.list.item(i).text() for i in range(self.list.count()))
        return next(self.list.item(i).data(Qt.ItemDataRole.UserRole) for i in range(self.list.count())
                    if isinstance(self.list.item(i).data(Qt.ItemDataRole.UserRole), connections.Connection))

    monkeypatch.setattr(dialogs.Picker, "choose", choose)
    window.choose_location(window.right)
    wait_until(lambda: not window.right.panel.location.is_local and not window.right.panel.loading, timeout=10)
    assert any("test box" in t for t in offered)
    assert [e.name for e in window.right.panel.entries] == ["hello.txt", "old.txt"]


# -- previews of files on other file systems (servers) --------------------------------------


@pytest.fixture
def remote_fs(monkeypatch):
    from test_vfs import MemoryFS

    class Remote(MemoryFS):
        type_name = "test server"

        def __init__(self) -> None:
            super().__init__()
            self.bytes_read = 0

        def open_read(self, path):
            import io

            remote = self
            data = self.files[path]

            class Reader(io.BytesIO):
                def read(self, size=-1):
                    chunk = super().read(size)
                    remote.bytes_read += len(chunk)
                    return chunk

            return Reader(data)

        def materialize(self, path):
            if self.basename(path).endswith((".mp4", ".txt", ".bin")):
                raise AssertionError(f"{path} should not be downloaded for a preview")
            return super().materialize(path)

    return Remote()


def show_remote(window: MainWindow, remote, name: str) -> PreviewPane:
    window.left.panel.load(vfs.VPath(remote, ""))
    settle(window)
    window.set_mode(window.right, PREVIEW)
    select(window, name)
    return window.right.preview


def test_remote_text_reads_only_the_beginning(qtbot, window: MainWindow, remote_fs):
    remote_fs.files["log.txt"] = b"line\n" * 1_000_000  # 5 MB
    pane = show_remote(window, remote_fs, "log.txt")
    wait_until(lambda: pane.current_id == "text" and shown_text(pane))
    assert remote_fs.bytes_read <= 1024 * 1024 + 1
    assert "first 1 MB" in pane._subtitle


def test_remote_binary_reads_only_the_beginning(qtbot, window: MainWindow, remote_fs):
    remote_fs.files["disk.bin"] = bytes(range(256)) * 40_000  # 10 MB
    pane = show_remote(window, remote_fs, "disk.bin")
    wait_until(lambda: "00000000  00 01 02" in shown_text(pane))
    assert remote_fs.bytes_read <= 1024 * 1024 + 1


def test_huge_remote_image_is_not_downloaded(qtbot, window: MainWindow, remote_fs, monkeypatch):
    from orchard_commander.preview.builtin.image import ImagePreviewer

    monkeypatch.setattr(ImagePreviewer, "max_size", 1000)
    remote_fs.files["huge.png"] = b"\x89PNG" + b"\0" * 5000
    pane = show_remote(window, remote_fs, "huge.png")
    wait_until(lambda: "copy it with F5" in shown_text(pane))
    assert remote_fs.bytes_read == 0


@pytest.mark.skipif(not __import__("shutil").which("ffmpeg"), reason="needs ffmpeg to create a sample video")
def test_remote_video_streams_without_download(qtbot, window: MainWindow, remote_fs, tmp_path: Path):
    import subprocess

    clip = tmp_path / "clip.mp4"
    subprocess.run(
        ["ffmpeg", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=25", "-t", "4",
         "-pix_fmt", "yuv420p", str(clip)],
        check=True,
    )
    remote_fs.files["clip.mp4"] = clip.read_bytes()
    pane = show_remote(window, remote_fs, "clip.mp4")
    wait_until(lambda: pane.current_id == "media")
    frames = []
    pane._views["media"].video.videoSink().videoFrameChanged.connect(lambda f: frames.append(f.size()))
    wait_until(lambda: len(frames) >= 5)
    assert frames[-1].width() == 160
    assert "streaming" in pane._subtitle
    window.set_mode(window.right, FILES)  # stops playback and closes the stream
    assert pane._views["media"]._device is None


# -- "View as" -----------------------------------------------------------------------------


def chip_titles(pane: PreviewPane) -> list[str]:
    return [b.text().strip() for b in pane._chip_buttons()] if pane.chips.isVisible() else []


def test_view_as_chips_and_memory(qtbot, window: MainWindow, tree: Path):
    (tree / "src" / "notes.md").write_text("# Heading\n\n*emphasis*\n")
    (tree / "src" / "more.md").write_text("# Other\n")
    window.left.panel.reload()
    settle(window)
    window.set_mode(window.right, PREVIEW)
    pane = window.right.preview

    select(window, "notes.md")
    wait_until(lambda: pane.current_id == "markdown")
    assert chip_titles(pane) == ["Markdown", "Text", "Hex"]
    assert "Heading" in shown_text(pane) and "#" not in shown_text(pane)  # rendered

    text_chip = next(b for b in pane._chip_buttons() if b.text().strip() == "Text")
    text_chip.click()
    wait_until(lambda: pane.current_id == "text")
    assert shown_text(pane).startswith("# Heading")  # the source

    select(window, "more.md")  # the choice is remembered for .md files
    wait_until(lambda: pane.current_id == "text" and shown_text(pane).startswith("# Other"))

    window.set_mode(window.right, FILES)  # close the preview…
    window.left.panel.setFocus()
    window.set_mode(window.right, PREVIEW)  # …and open it again: still remembered
    wait_until(lambda: pane.current_id == "text")
    window.cycle_preview_view()  # Shift+F3: text -> hex
    wait_until(lambda: pane.current_id == "hex")
    window.cycle_preview_view()  # hex -> markdown (wraps around)
    wait_until(lambda: pane.current_id == "markdown")


def test_single_candidate_shows_no_chips(qtbot, window: MainWindow, tree: Path):
    window.set_mode(window.right, PREVIEW)
    pane = window.right.preview
    select(window, "sub")
    wait_until(lambda: pane.current_id == "listing")
    assert chip_titles(pane) == []


def test_pdf_preview_pages(qtbot, window: MainWindow, tree: Path):
    from test_previewers import pdf_bytes

    (tree / "src" / "manual.pdf").write_bytes(pdf_bytes(3))
    window.left.panel.reload()
    settle(window)
    window.set_mode(window.right, PREVIEW)
    select(window, "manual.pdf")
    pane = window.right.preview
    wait_until(lambda: pane.current_id == "pdf")
    view = pane.current_view
    assert view.page_count == 3 and view.page_index == 0 and view.counter.text() == "1 / 3"
    assert "3 pages" in pane._subtitle
    wait_until(lambda: view.page.pixmap() is not None and not view.page.pixmap().isNull())
    qtbot.keyClick(view, Qt.Key.Key_PageDown)
    assert view.page_index == 1 and view.counter.text() == "2 / 3"
    qtbot.keyClick(view, Qt.Key.Key_End)
    assert view.page_index == 2 and not view.next_button.isEnabled()
    qtbot.keyClick(view, Qt.Key.Key_Home)
    assert view.page_index == 0 and not view.prev_button.isEnabled()
    wait_until(lambda: (0, view._target_width()) in view._cache)  # rendered at the pane's exact size
    select(window, "a.txt")  # switching files closes the document
    wait_until(lambda: pane.current_id == "text")
    assert view._doc is None


def test_online_only_file_needs_explicit_download(qtbot, window: MainWindow, monkeypatch):
    from test_previewers import CloudFS, pdf_bytes

    cloud = CloudFS()
    cloud.files["report.pdf"] = pdf_bytes(1) + b"\0" * (30 * 1024**2)
    window.left.panel.load(vfs.VPath(cloud, ""))
    settle(window)
    model = window.left.panel.model
    assert model.data(model.index(model.row_of("report.pdf"), 1)).startswith("☁")  # size column
    window.set_mode(window.right, PREVIEW)
    select(window, "report.pdf")
    pane = window.right.preview
    wait_until(lambda: pane.download_button.isVisible())
    assert "stored online only" in pane.message_label.text() and cloud.bytes_read == 0
    pane.download_button.click()
    wait_until(lambda: pane.current_id == "pdf")
    assert not pane.download_button.isVisible()


def test_pdf_wheel_turns_one_page_per_step(qtbot, window: MainWindow, tree: Path, monkeypatch):
    from PySide6.QtCore import QPoint, QPointF
    from PySide6.QtGui import QWheelEvent

    from orchard_commander.preview.builtin import pdf as pdf_module
    from test_previewers import pdf_bytes

    clock = [1000.0]
    monkeypatch.setattr(pdf_module.time, "monotonic", lambda: clock[0])
    (tree / "src" / "long.pdf").write_bytes(pdf_bytes(10))
    window.left.panel.reload()
    settle(window)
    window.set_mode(window.right, PREVIEW)
    select(window, "long.pdf")
    pane = window.right.preview
    wait_until(lambda: pane.current_id == "pdf")
    view = pane.current_view

    def wheel(dy: int, phase=Qt.ScrollPhase.ScrollUpdate) -> None:
        event = QWheelEvent(QPointF(10, 10), QPointF(10, 10), QPoint(0, dy // 4), QPoint(0, dy),
                            Qt.MouseButton.NoButton, Qt.KeyboardModifier.NoModifier, phase, False)
        QApplication.sendEvent(view, event)

    # A trackpad flick: lots of small deltas, then momentum after lifting the fingers.
    wheel(-8, Qt.ScrollPhase.ScrollBegin)
    for _ in range(40):
        wheel(-15)
        clock[0] += 0.01
    for _ in range(30):
        wheel(-30, Qt.ScrollPhase.ScrollMomentum)
    assert view.page_index == 1  # one flick, one page

    clock[0] += 1.0  # a classic mouse wheel: whole notches, a moment apart
    wheel(-120, Qt.ScrollPhase.NoScrollPhase)
    clock[0] += 1.0
    wheel(-120, Qt.ScrollPhase.NoScrollPhase)
    assert view.page_index == 3
    clock[0] += 1.0
    wheel(120, Qt.ScrollPhase.NoScrollPhase)
    assert view.page_index == 2
    wheel(-40, Qt.ScrollPhase.NoScrollPhase)  # a fraction of a notch: nothing yet
    assert view.page_index == 2


def test_loading_indicator_for_cloud_and_slow_servers(qtbot, window: MainWindow, tree: Path, monkeypatch):
    import time as time_module
    from dataclasses import replace

    from orchard_commander import fs as fs_module
    from orchard_commander.preview import registry

    real_preview = registry.preview
    monkeypatch.setattr(registry, "preview", lambda *a, **k: (time_module.sleep(0.6), real_preview(*a, **k))[1])
    pane = window.right.preview
    window.set_mode(window.right, PREVIEW)

    # A local online-only placeholder (e.g. in Dropbox): announced at once, in the middle of the pane.
    real_stat = fs_module.stat_entry
    monkeypatch.setattr(fs_module, "stat_entry", lambda p: replace(real_stat(p), is_online_only=p.name == "a.txt"))
    select(window, "a.txt")
    wait_until(lambda: pane.busy.isVisible())
    assert pane.message_label.text().startswith("Downloading a.txt from the cloud…")
    wait_until(lambda: pane.current_id == "text")
    assert not pane.busy.isVisible()

    # A file on a server: only if it takes longer than a moment.
    remote = MemoryFSForUI()
    remote.files["far.txt"] = b"far away"
    window.left.panel.load(vfs.VPath(remote, ""))
    settle(window)
    select(window, "far.txt")
    wait_until(lambda: pane.busy.isVisible())
    assert pane.message_label.text() == "Loading far.txt…"
    wait_until(lambda: pane.current_id == "text" and shown_text(pane) == "far away")


def MemoryFSForUI():
    from test_vfs import MemoryFS

    return MemoryFS()


# -- bookmarks -------------------------------------------------------------------------------


def menu_actions(menu) -> dict[str, object]:
    """Visible action texts (hotkey column and title, without the detail) -> action."""
    return {a.text().split("\t")[0].strip(): a for a in menu.actions() if not a.isSeparator()}


def test_bookmark_menu_and_hotkeys(qtbot, window: MainWindow, tree: Path):
    from orchard_commander import bookmarks
    from orchard_commander.bookmarks import Bookmark, Folder, Separator
    from orchard_commander.ui import bookmarks_ui

    items = [
        Bookmark("&Src", {"kind": "local", "path": str(tree / "src")}),
        Bookmark("&Dst", {"kind": "local", "path": str(tree / "dst")}),
        Bookmark("&Sub", {"kind": "local", "path": str(tree / "src" / "sub")}),
        Separator(),
        Folder("&Work", [Bookmark("&Inside", {"kind": "local", "path": str(tree)})]),
    ]
    opened: list[str] = []
    menu = bookmarks_ui.build_menu(
        window, items, on_open=lambda b: opened.append(bookmarks.plain(b.title)),
        current={"kind": "local", "path": str(tree / "src")},
        on_add=lambda: opened.append("add"), on_remove=lambda b: opened.append("remove"),
        on_organize=lambda: opened.append("organize"),
    )
    qtbot.addWidget(menu)
    texts = list(menu_actions(menu))
    assert texts[:4] == ["S  Src", "D  Dst", "S  Sub", "W  Work"]
    assert "-  Remove current folder (Src)" in texts and "+  Add current folder…" not in texts
    assert "*  Organize bookmarks…" in texts

    qtbot.keyClick(menu, Qt.Key.Key_D)  # unique hotkey: opens right away
    assert opened == ["Dst"]
    qtbot.keyClick(menu, Qt.Key.Key_S)  # shared by Src and Sub: moves between them
    assert menu.activeAction().text().startswith("S  Src")
    qtbot.keyClick(menu, Qt.Key.Key_S)
    assert menu.activeAction().text().startswith("S  Sub") and opened == ["Dst"]


def test_add_and_open_bookmark_with_other_panel(qtbot, window: MainWindow, tree: Path, monkeypatch):
    from orchard_commander import bookmarks
    from orchard_commander.ui import bookmarks_ui

    monkeypatch.setattr(bookmarks_ui.AddBookmarkDialog, "ask", lambda self: ("&Pair", None, True))
    window.add_bookmark()
    [saved] = bookmarks.load()  # persisted
    assert saved.location == {"kind": "local", "path": str(tree / "src")}
    assert saved.target == {"kind": "local", "path": str(tree / "dst")}

    window.left.panel.load(vfs.local(tree))
    window.right.panel.load(vfs.local(tree))
    settle(window)
    window.open_bookmark(window.bookmark_items[0], window.left)
    settle(window)
    assert window.left.panel.location == vfs.local(tree / "src")
    assert window.right.panel.location == vfs.local(tree / "dst")  # the other panel too


def test_bookmark_inside_archive(qtbot, window: MainWindow, zipped: Path):
    from orchard_commander.bookmarks import Bookmark

    spec = {"kind": "archive", "archive": {"kind": "local", "path": str(zipped)}, "path": "docs"}
    window.open_bookmark(Bookmark("&Docs", spec), window.left)
    wait_until(lambda: isinstance(window.left.panel.location.fs, vfs.ArchiveFS) and not window.left.panel.loading)
    assert [e.name for e in window.left.panel.entries] == ["readme.md"]
    assert window.location_spec(window.left) == spec  # and it round-trips


def test_bookmark_on_server_reconnects(qtbot, window: MainWindow, sftp_server, monkeypatch, memory_keyring):
    from orchard_commander import connections
    from orchard_commander.bookmarks import Bookmark
    from orchard_commander.ui import main_window as mw
    from servers import PASSWORD, USER

    conn = connections.Connection.make("sftp", host=sftp_server.host, username=USER, port=sftp_server.port)
    connections.save(conn)
    connections.store_password(conn, PASSWORD)
    monkeypatch.setattr(mw, "confirm_question", lambda *a: True)
    window.open_bookmark(Bookmark("&Pub", {"kind": "server", "account": conn.account, "path": "/pub"}), window.left)
    wait_until(lambda: not window.left.panel.location.is_local and not window.left.panel.loading, timeout=10)
    assert window.left.panel.location.path == "/pub"
    assert [e.name for e in window.left.panel.entries] == ["hello.txt", "old.txt"]
    spec = window.location_spec(window.left)  # a server folder can be bookmarked
    assert spec == {"kind": "server", "account": conn.account, "path": "/pub"}


def test_go_to_bookmark_search(qtbot, window: MainWindow, tree: Path, monkeypatch):
    from orchard_commander.bookmarks import Bookmark, Folder

    window.bookmark_items = [Folder("&Work", [Bookmark("&Sub", {"kind": "local", "path": str(tree / "src" / "sub")})])]
    offered = []

    def choose(self):
        offered.extend(self.list.item(i).text() for i in range(self.list.count()))
        return self.list.item(0).data(Qt.ItemDataRole.UserRole)

    monkeypatch.setattr(dialogs.Picker, "choose", choose)
    window.go_to_bookmark()
    settle(window)
    assert offered[0].startswith("Work › Sub")
    assert window.left.panel.location == vfs.local(tree / "src" / "sub")


def test_organizer_folders_order_delete(qtbot, window: MainWindow, tree: Path, monkeypatch):
    from orchard_commander.bookmarks import Bookmark, Folder, Separator
    from orchard_commander.ui import bookmarks_ui, dialogs as dialogs_module

    a = Bookmark("&A", {"kind": "local", "path": str(tree)})
    b = Bookmark("&B", {"kind": "local", "path": str(tree / "src")})
    organizer = bookmarks_ui.BookmarkOrganizer(window, window.themes, [a, b], None, None)
    qtbot.addWidget(organizer)
    tree_widget = organizer.tree
    tree_widget.setCurrentItem(tree_widget.topLevelItem(1))  # B
    organizer.move(-1)  # B above A
    monkeypatch.setattr(dialogs_module, "ask_text", lambda *args, **kw: "&Group")
    organizer.add_folder()  # inserted after B
    organizer.add_separator()  # current is the new folder: goes inside it
    tree_widget.setCurrentItem(tree_widget.topLevelItem(2))  # A
    organizer.delete()
    items = organizer.items()
    assert [type(i).__name__ for i in items] == ["Bookmark", "Folder"]
    assert items[0].id == b.id and isinstance(items[1], Folder) and items[1].title == "&Group"
    assert isinstance(items[1].items[0], Separator)


def test_click_on_panel_path_opens_bookmarks(qtbot, window: MainWindow, monkeypatch):
    calls = []
    monkeypatch.setattr(window, "bookmark_menu", lambda side=None, pos=None: calls.append(side))
    for side in (window.left, window.right):  # reconnect to the patched method
        side.panel.title_clicked.disconnect()
        side.panel.title_clicked.connect(lambda pos, s=side: window.bookmark_menu(s, pos))
    panel = window.right.panel
    panel.repaint()
    qtbot.mouseClick(panel, Qt.MouseButton.LeftButton, pos=panel._title_rect.center().toPoint())
    assert calls == [window.right]


# -- Total Commander panel commands -------------------------------------------------------------


def ctrl(qtbot, window: MainWindow, key: Qt.Key) -> None:
    """Ctrl+key as Qt sees it (⌘ on macOS), sent to the focused widget."""
    qtbot.keyClick(QApplication.focusWidget() or window, key, Qt.KeyboardModifier.ControlModifier)
    settle(window)


def test_transfer_left_and_right(qtbot, window: MainWindow, tree: Path, zipped: Path):
    left, right = window.left.panel, window.right.panel
    (tree / "dst" / "inbox").mkdir()
    left.reload()
    right.reload()
    settle(window)

    select(window, "sub")  # left panel, cursor on a folder
    ctrl(qtbot, window, Qt.Key.Key_Right)
    assert right.location == vfs.local(tree / "src" / "sub") and left.location == vfs.local(tree / "src")
    assert window.active_panel is left  # focus stays where it was

    select(window, "a.txt")  # cursor on a file: the folder itself
    ctrl(qtbot, window, Qt.Key.Key_Right)
    assert right.location == vfs.local(tree / "src")

    select(window, "pack.zip")  # cursor on an archive: opened in the other panel
    ctrl(qtbot, window, Qt.Key.Key_Right)
    assert isinstance(right.location.fs, vfs.ArchiveFS) and right.location.name == "pack.zip"

    right.load(vfs.local(tree / "dst"))
    settle(window)
    right.setFocus()  # in the right panel, Ctrl+Right makes it match the left
    ctrl(qtbot, window, Qt.Key.Key_Right)
    assert right.location == vfs.local(tree / "src")

    right.load(vfs.local(tree / "dst"))
    settle(window)
    right.setFocus()
    right.table.set_current_row(right.model.row_of("inbox"))
    ctrl(qtbot, window, Qt.Key.Key_Left)  # mirror image: the left panel opens "inbox"
    assert left.location == vfs.local(tree / "dst" / "inbox")
    left.setFocus()
    ctrl(qtbot, window, Qt.Key.Key_Left)  # in the left panel, Ctrl+Left makes it match the right
    assert left.location == vfs.local(tree / "dst")


def test_swap_and_same_directory(qtbot, window: MainWindow, tree: Path):
    left, right = window.left.panel, window.right.panel
    ctrl(qtbot, window, Qt.Key.Key_U)  # swap
    assert left.location == vfs.local(tree / "dst") and right.location == vfs.local(tree / "src")
    ctrl(qtbot, window, Qt.Key.Key_E)  # the other panel shows the active panel's folder
    assert left.location == right.location == vfs.local(tree / "dst")


def test_folder_changes_dont_cancel_navigation(qtbot, window: MainWindow, tree: Path):
    # The watched folder changing while the next one is still being listed (e.g. a
    # server) must not reload it over the navigation: with a busy folder (home),
    # connecting to a server silently stayed local.
    panel = window.left.panel
    panel.load(vfs.local(tree / "dst"))
    panel._directory_changed()
    settle(window)
    assert panel.location == vfs.local(tree / "dst")


def test_bookmark_to_slow_folder_while_current_folder_keeps_changing(qtbot, window: MainWindow, tree: Path,
                                                                      monkeypatch):
    # The real situation: the panel shows a busy folder (home) whose watcher keeps
    # firing, and the bookmark's folder takes a moment to list (a server).
    from orchard_commander import bookmarks

    slow = tree / "dst"
    original = vfs.VPath.list_dir

    def list_dir(self, **options):
        if self.local_path() == slow:
            time.sleep(0.8)
        return original(self, **options)

    monkeypatch.setattr(vfs.VPath, "list_dir", list_dir)
    churn = QTimer(interval=50)
    counter = iter(range(10**6))
    churn.timeout.connect(lambda: (tree / "src" / f"tmp{next(counter)}").write_text("x"))
    churn.start()
    try:
        wait(400)  # the watcher is firing now
        window.open_bookmark(bookmarks.Bookmark("dst", {"kind": "local", "path": str(slow)}), window.left)
        wait_until(lambda: window.left.panel.location == vfs.local(slow), timeout=5)
    finally:
        churn.stop()


@pytest.mark.parametrize("key", [Qt.Key.Key_Return, Qt.Key.Key_Enter])
def test_enter_presses_the_highlighted_button_not_the_default(qtbot, window: MainWindow, key):
    # Enter with Cancel highlighted used to press the default button: files were trashed.
    from PySide6.QtWidgets import QPushButton

    def answer(highlight: str) -> bool:
        def act() -> None:
            dialog = QApplication.activeModalWidget()
            button = next(b for b in dialog.findChildren(QPushButton) if b.text() == highlight)
            button.setFocus()
            qtbot.keyClick(button, key)

        QTimer.singleShot(50, act)
        return dialogs.confirm(window, window.themes, "Move to trash", "Move 'a.txt' to the trash?", "Trash")

    assert answer("Cancel") is False
    assert answer("Trash") is True


def test_cancelled_trash_keeps_the_file(qtbot, window: MainWindow, tree: Path):
    from PySide6.QtWidgets import QPushButton

    select(window, "a.txt")

    def cancel_with_keyboard() -> None:
        dialog = QApplication.activeModalWidget()
        qtbot.keyClick(QApplication.focusWidget(), Qt.Key.Key_Tab)  # Trash → Cancel
        assert isinstance(QApplication.focusWidget(), QPushButton) and QApplication.focusWidget().text() == "Cancel"
        qtbot.keyClick(QApplication.focusWidget(), Qt.Key.Key_Return)
        assert not dialog.isVisible()

    QTimer.singleShot(50, cancel_with_keyboard)
    window.trash()
    settle(window)
    wait(200)
    assert (tree / "src" / "a.txt").exists()
