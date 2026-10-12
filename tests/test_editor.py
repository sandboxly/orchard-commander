"""The text editor: encodings and line endings, highlighting, F4 / Shift+F4, saving on any file system."""

from __future__ import annotations

import os
import stat
import zipfile
from pathlib import Path

import pytest
from PySide6.QtCore import Qt

from orchard_commander import textfile, vfs
from orchard_commander.ui import dialogs
from orchard_commander.ui import editor as editor_ui
from test_ui import settle, wait_until


@pytest.fixture(autouse=True)
def discard_on_close(monkeypatch):
    """Editors left with changes are closed when the test's window closes: don't ask (no one answers)."""
    monkeypatch.setattr(editor_ui, "ask_save", lambda *a: "discard")


# -- text files ----------------------------------------------------------------------------------

@pytest.mark.parametrize(
    ("data", "encoding", "bom", "newline", "text"),
    [
        (b"a\nb\n", "utf-8", False, "\n", "a\nb\n"),
        (b"a\r\nb\r\n", "utf-8", False, "\r\n", "a\nb\n"),
        ("grüße\n".encode(), "utf-8", False, "\n", "grüße\n"),
        (b"\xef\xbb\xbfhi\n", "utf-8", True, "\n", "hi\n"),
        ("hi\r\n".encode("utf-16-le"), None, None, None, None),  # NUL bytes without a BOM: binary
        (b"\xff\xfe" + "hé\n".encode("utf-16-le"), "utf-16-le", True, "\n", "hé\n"),
        ("café\n".encode("cp1252"), "cp1252", False, "\n", "café\n"),
        (b"", "utf-8", False, "\n", ""),
    ],
)
def test_decode(data, encoding, bom, newline, text):
    if encoding is None:
        with pytest.raises(textfile.BinaryFile):
            textfile.decode(data)
        return
    file = textfile.decode(data)
    assert (file.encoding, file.bom, file.newline, file.text) == (encoding, bom, newline, text)
    assert file.encode() == data  # unchanged text writes back the same bytes


def test_binary_and_descriptions():
    with pytest.raises(textfile.BinaryFile):
        textfile.decode(b"\x89PNG\r\n\x1a\n\0\0\0")
    assert textfile.decode(b"x\r\n").description == "UTF-8 · CRLF"
    assert textfile.decode(b"\xef\xbb\xbfx").description == "UTF-8 with BOM · LF"


# -- highlighting --------------------------------------------------------------------------------

def test_python_is_highlighted(qapp):
    from PySide6.QtGui import QTextDocument

    doc = QTextDocument('def f():\n    return "😀 text"  # note\n')
    h = editor_ui.PygmentsHighlighter(doc, "x.py", "monokai")
    assert h.language == "Python"
    first = doc.findBlockByNumber(0).layout().formats()
    assert any(r.start == 0 and r.length == 3 for r in first)  # "def" is colored
    second = doc.findBlockByNumber(1).layout().formats()
    comment = [r for r in second if r.format.fontItalic() or r.start > 20]
    assert comment and max(r.start + r.length for r in second) <= doc.findBlockByNumber(1).length()
    plain = editor_ui.PygmentsHighlighter(QTextDocument("just text"), "notes.txt", "monokai")
    assert plain.language == "Plain text"


# -- the editor in the app -----------------------------------------------------------------------

def editor_for(window, name: str) -> editor_ui.EditorWindow:
    wait_until(lambda: any(e.location.name == name for e in window._editors))
    editor = next(e for e in window._editors if e.location.name == name)
    wait_until(lambda: editor._loaded or editor.load_failed)
    return editor


def select(window, name: str) -> None:
    panel = window.active_panel
    settle(window)
    panel.table.set_current_row(panel.model.row_of(name))


def test_f4_edits_and_saves(qtbot, window, tree: Path):
    path = tree / "src" / "a.txt"
    path.write_bytes(b"alpha\r\nbeta\r\n")
    path.chmod(0o640)
    select(window, "a.txt")
    qtbot.keyClick(window.left.panel.table, Qt.Key.Key_F4)  # Total Commander's F4
    editor = editor_for(window, "a.txt")
    assert editor.editor.toPlainText() == "alpha\nbeta\n" and "CRLF" in editor.frame._subtitle
    editor.editor.moveCursor(editor.editor.textCursor().MoveOperation.End)
    editor.editor.insertPlainText("gamma\n")
    assert editor.modified
    editor.save()
    wait_until(lambda: not editor.modified)
    assert path.read_bytes() == b"alpha\r\nbeta\r\ngamma\r\n"  # line endings kept
    if os.name != "nt":  # Windows has no POSIX permissions
        assert stat.S_IMODE(path.stat().st_mode) == 0o640  # permissions kept
    assert not list(path.parent.glob(".a.txt.*"))  # no temporary files left


def test_unsaved_changes_ask(qtbot, window, tree: Path, monkeypatch):
    editor = window.open_editor(vfs.local(tree / "src" / "a.txt"))
    editor = editor_for(window, "a.txt")
    editor.editor.insertPlainText("changed ")
    monkeypatch.setattr(editor_ui, "ask_save", lambda *a: "cancel")
    assert not editor.close() and editor.isVisible()
    monkeypatch.setattr(editor_ui, "ask_save", lambda *a: "save")
    editor.close()
    wait_until(lambda: (tree / "src" / "a.txt").read_text().startswith("changed "))
    wait_until(lambda: not window._editors)


def test_quitting_asks_about_unsaved_editors(qtbot, window, tree: Path, monkeypatch):
    window.open_editor(vfs.local(tree / "src" / "a.txt"))
    editor = editor_for(window, "a.txt")
    editor.editor.insertPlainText("x")
    monkeypatch.setattr(editor_ui, "ask_save", lambda *a: "cancel")
    assert not window.close() and window.isVisible()
    monkeypatch.setattr(editor_ui, "ask_save", lambda *a: "discard")
    editor.close()


def test_shift_f4_creates_a_file(qtbot, window, tree: Path, monkeypatch):
    monkeypatch.setattr(dialogs, "ask_text", lambda *a, **k: "notes.md")
    qtbot.keyClick(window.left.panel.table, Qt.Key.Key_F4, Qt.KeyboardModifier.ShiftModifier)
    editor = editor_for(window, "notes.md")
    assert (tree / "src" / "notes.md").exists() and editor.highlighter.language == "Markdown"
    editor.editor.insertPlainText("# Hello\n")
    editor.save()
    wait_until(lambda: (tree / "src" / "notes.md").read_text() == "# Hello\n")
    wait_until(lambda: window.left.panel.cursor_name == "notes.md")


def test_find_and_replace(qtbot, window, tree: Path):
    (tree / "src" / "a.txt").write_text("cat dog cat\nCat\n")
    window.open_editor(vfs.local(tree / "src" / "a.txt"))
    editor = editor_for(window, "a.txt")
    editor.show_replace()
    editor.find_field.setText("cat")
    editor.replace_field.setText("bird")
    assert editor.find_next()
    assert editor.replace_all() == 3  # case-insensitive by default
    assert editor.editor.toPlainText() == "bird dog bird\nbird\n"
    editor.editor.document().undo()  # one undo step for all of them
    assert editor.editor.toPlainText() == "cat dog cat\nCat\n"
    editor.match_case.setChecked(True)
    assert editor.replace_all() == 2


def test_read_only_and_binary(qtbot, window, tree: Path):
    with zipfile.ZipFile(tree / "src" / "pack.zip", "w") as z:
        z.writestr("inner.txt", "inside")
    inner = vfs.open_archive(vfs.local(tree / "src" / "pack.zip")).child("inner.txt")
    window.open_editor(inner)
    editor = editor_for(window, "inner.txt")
    assert editor.read_only and editor.editor.isReadOnly() and editor.editor.toPlainText() == "inside"
    (tree / "src" / "image.bin").write_bytes(b"\0\1\2binary")
    window.open_editor(vfs.local(tree / "src" / "image.bin"))
    binary = editor_for(window, "image.bin")
    assert binary.read_only and "isn't a text file" in binary.editor.toPlainText()


def test_edit_on_another_file_system(qtbot, window):
    from test_vfs import MemoryFS

    mem = MemoryFS()
    mem.files["remote.py"] = b"x = 1\n"
    window.open_editor(vfs.VPath(mem, "remote.py"))
    editor = editor_for(window, "remote.py")
    editor.editor.insertPlainText("y = 2\n")
    editor.save()
    wait_until(lambda: mem.files["remote.py"] == b"y = 2\nx = 1\n")


def test_reloads_when_changed_on_disk(qtbot, window, tree: Path):
    path = tree / "src" / "a.txt"
    window.open_editor(vfs.local(path))
    editor = editor_for(window, "a.txt")
    path.write_text("changed elsewhere\n")
    wait_until(lambda: editor.editor.toPlainText() == "changed elsewhere\n")


def test_f4_is_in_the_function_key_bar(qtbot, window):
    edit = next(c for c in window.commands if c.title == "Edit")
    assert edit.shortcuts == ["F4"] and edit.fkey == ("F4", "Edit")


def test_edit_a_file_over_sftp(qtbot, window, tmp_path: Path):
    """F4 on a server: read over SFTP, save back over SFTP."""
    from orchard_commander.remote import sftp
    from servers import PASSWORD, USER, start_sftp_server

    root = tmp_path / "server"
    root.mkdir()
    (root / "config.ini").write_bytes(b"[app]\r\nmode = old\r\n")
    server = start_sftp_server(root)
    try:
        remote = sftp.connect(server.host, port=server.port, username=USER, password=PASSWORD,
                              known_hosts=tmp_path / "known_hosts", confirm_host_key=lambda q: True)
        location = vfs.VPath(remote, remote.join(remote.home, "config.ini"))
        window.open_editor(location)
        editor = editor_for(window, "config.ini")
        assert not editor.read_only and editor.highlighter.language == "INI"
        editor.show_replace()
        editor.find_field.setText("old")
        editor.replace_field.setText("new")
        assert editor.replace_all() == 1
        editor.save()
        wait_until(lambda: not editor.modified)
        assert (root / "config.ini").read_bytes() == b"[app]\r\nmode = new\r\n"  # CRLF kept, on the server
        editor.close()
        remote.close()
    finally:
        server.stop()


def test_title_shortens_the_home_folder(qtbot, window, tree: Path, monkeypatch):
    monkeypatch.setenv("HOME", str(tree))
    window.open_editor(vfs.local(tree / "src" / "a.txt"))
    editor = editor_for(window, "a.txt")
    assert editor.frame._title.endswith(os.path.join("~", "src", "a.txt"))
