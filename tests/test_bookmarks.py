"""Bookmarks model and location specs (no UI)."""

from __future__ import annotations

import io
import json
import zipfile
from pathlib import Path

from orchard_commander import bookmarks, locations, vfs
from orchard_commander.bookmarks import Bookmark, Folder, Separator
from test_vfs import MemoryFS


def local(path) -> dict:
    return {"kind": "local", "path": str(path)}


def test_hotkeys_and_titles():
    assert bookmarks.hotkey("&Projects") == "p"
    assert bookmarks.hotkey("Sound && &Vision") == "v"
    assert bookmarks.hotkey("No hotkey") is None
    assert bookmarks.hotkey("trailing &") is None
    assert bookmarks.plain("Sound && &Vision") == "Sound & Vision"


def test_tree_helpers():
    work = Folder("&Work", [Bookmark("&Repo", local("/src/repo")), Separator()])
    items = [Bookmark("&Home", local("/home/me")), work]
    assert [(t, b.title) for t, b in bookmarks.bookmarks(items)] == [((), "&Home"), (("Work",), "&Repo")]
    assert bookmarks.find_location(items, local("/src/repo/")).title == "&Repo"  # normalised
    assert bookmarks.find_location(items, local("/elsewhere")) is None
    assert bookmarks.container_of(items, work.id) is work.items
    assert bookmarks.container_of(items, "unknown") is items
    assert bookmarks.remove(items, work.items[0].id) and len(work.items) == 1


def test_save_and_load_round_trip(tmp_path: Path):
    target = local(tmp_path / "other")
    items = [
        Bookmark("&Docs", local(tmp_path), target),
        Folder("&Servers", [Bookmark("&Web", {"kind": "server", "account": "sftp://root@web", "path": "/var/www"})]),
        Separator(),
    ]
    bookmarks.save(items)
    loaded = bookmarks.load()
    assert [type(i).__name__ for i in loaded] == ["Bookmark", "Folder", "Separator"]
    assert loaded[0].target == target and loaded[0].id == items[0].id
    assert loaded[1].items[0].location["account"] == "sftp://root@web"
    data = json.loads(bookmarks._store_path().read_text())
    assert data["version"] == 1


def test_damaged_file_is_tolerated():
    bookmarks._store_path().parent.mkdir(parents=True, exist_ok=True)
    bookmarks._store_path().write_text('{"items": [{"type": "bookmark"}, {"type": "separator"}, 42]}')
    assert [type(i).__name__ for i in bookmarks.load()] == ["Separator"]
    bookmarks._store_path().write_text("not json")
    assert bookmarks.load() == []


def test_specs_for_local_archive_and_server(tmp_path: Path):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        z.writestr("docs/readme.md", "#")
    (tmp_path / "pack.zip").write_bytes(buf.getvalue())
    no_accounts = lambda fs: None  # noqa: E731

    assert locations.spec_of(vfs.local(tmp_path), no_accounts) == local(tmp_path)
    inside = vfs.open_archive(vfs.local(tmp_path / "pack.zip")).child("docs")
    spec = locations.spec_of(inside, no_accounts)
    assert spec == {"kind": "archive", "archive": local(tmp_path / "pack.zip"), "path": "docs"}
    assert locations.describe(spec).endswith("pack.zip/docs") and locations.name_of(spec) == "docs"

    server = MemoryFS()
    on_server = vfs.VPath(server, "projects")
    assert locations.spec_of(on_server, no_accounts) is None  # unknown connection: can't be bookmarked
    spec = locations.spec_of(on_server, lambda fs: "mem://me@bucket" if fs is server else None)
    assert spec == {"kind": "server", "account": "mem://me@bucket", "path": "projects"}
    assert locations.describe(spec) == "mem://me@bucket/projects"

    server.files["nested.zip"] = buf.getvalue()  # an archive on a server
    nested = vfs.open_archive(vfs.VPath(server, "nested.zip"))
    spec = locations.spec_of(nested, lambda fs: "mem://me@bucket")
    assert spec["archive"] == {"kind": "server", "account": "mem://me@bucket", "path": "nested.zip"}


def test_same_location_and_validation():
    assert locations.same_location(local("/a/b"), local("/a/b/"))
    assert not locations.same_location(local("/a/b"), local("/a/c"))
    assert locations.same_location({"kind": "server", "account": "SFTP://h", "path": "/x/"},
                                   {"kind": "server", "account": "sftp://h", "path": "/x"})
    assert not locations.same_location(None, local("/a"))
    assert locations.is_valid({"kind": "archive", "archive": local("/a.zip"), "path": ""})
    assert not locations.is_valid({"kind": "server", "account": "nohost", "path": "/"})
    assert not locations.is_valid({"kind": "teleport"})
