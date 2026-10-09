"""The virtual file system layer, including a toy third backend."""

from __future__ import annotations

import io
import posixpath
import tarfile
import zipfile
from collections.abc import Iterator
from pathlib import Path
from typing import BinaryIO

import pytest

from orchard_commander import fs, vfs


def never(src, dst):
    raise AssertionError(f"unexpected conflict {src} -> {dst}")


@pytest.fixture
def zipped(tree: Path) -> Path:
    """src/pack.zip: docs/readme.md, top.txt, inner.tar.gz (holding deep.txt)."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as t:
        info = tarfile.TarInfo("deep.txt")
        info.size = 11
        t.addfile(info, io.BytesIO(b"deep inside"))
    path = tree / "src" / "pack.zip"
    with zipfile.ZipFile(path, "w") as z:
        z.writestr("docs/readme.md", "# Hello\n")
        z.writestr("top.txt", "top level")
        z.writestr("inner.tar.gz", buf.getvalue())
    return path


class MemoryFS(vfs.FileSystem):
    """A writable in-memory file system, standing in for e.g. SFTP."""

    type_name = "memory"
    writable = True

    def __init__(self) -> None:
        self.files: dict[str, bytes] = {}
        self.dirs: set[str] = {""}

    def list_dir(self, path: str, **kwargs) -> list[fs.Entry]:
        names = {p for p in self.files.keys() | self.dirs if p and posixpath.dirname(p) == path}
        return fs.sort_entries([self.stat(n) for n in names], **kwargs)

    def stat(self, path: str) -> fs.Entry:
        name = posixpath.basename(path)
        hidden = name.startswith(".")
        if path in self.dirs:
            return fs.Entry(name, True, False, hidden, False, 0, 0.0)
        if path in self.files:
            return fs.Entry(name, False, False, hidden, False, len(self.files[path]), 0.0)
        raise FileNotFoundError(path)

    def parent_path(self, path: str) -> str | None:
        return posixpath.dirname(path) if path else None

    def join(self, path: str, *names: str) -> str:
        return "/".join(p for p in (path, *names) if p)

    def basename(self, path: str) -> str:
        return posixpath.basename(path)

    def display(self, path: str) -> str:
        return f"mem:/{path}"

    def open_read(self, path: str) -> BinaryIO:
        return io.BytesIO(self.files[path])

    def walk(self, path: str) -> Iterator[tuple[vfs.RelPath, fs.Entry]]:
        yield (), self.stat(path)
        prefix = path + "/" if path else ""
        for p in sorted((self.files.keys() | self.dirs) - {path}):
            if p.startswith(prefix):
                yield tuple(p[len(prefix) :].split("/")), self.stat(p)

    def mkdir(self, path: str, *, exist_ok: bool = False) -> None:
        if path in self.dirs:
            if not exist_ok:
                raise FileExistsError(path)
            return
        while path and path not in self.dirs:  # with missing parents
            self.dirs.add(path)
            path = posixpath.dirname(path)

    def remove(self, path: str) -> None:
        prefix = path + "/"
        for p in [f for f in self.files if f == path or f.startswith(prefix)]:
            del self.files[p]
        self.dirs -= {d for d in self.dirs if d == path or d.startswith(prefix)}

    def open_write(self, path: str) -> BinaryIO:
        fs_ = self

        class Writer(io.BytesIO):
            def close(self) -> None:
                fs_.files[path] = self.getvalue()
                super().close()

        return Writer()


# -- navigation -------------------------------------------------------------------


def test_local_navigation(tree: Path):
    src = vfs.local(tree / "src")
    assert src.name == "src" and src.is_dir()
    assert src.parent() == vfs.local(tree)
    assert [e.name for e in src.list_dir()] == ["sub", "a.txt", "b.bin"]
    assert src.child("a.txt").stat().size == 5
    assert src.local_anchor() == tree / "src"


def test_archive_navigation_crosses_back_to_container(zipped: Path):
    pack = vfs.local(zipped)
    assert vfs.is_archive(pack)
    root = vfs.open_archive(pack)
    assert root.name == "pack.zip"
    assert root.display == str(zipped)
    assert root.parent() == vfs.local(zipped.parent)  # up from the root leaves the archive
    docs = root.child("docs")
    assert docs.display == str(zipped / "docs")
    assert docs.parent() == root
    assert [e.name for e in docs.list_dir()] == ["readme.md"]
    assert docs.local_anchor() == zipped.parent
    assert root == vfs.open_archive(vfs.local(zipped))  # same archive, same location


def test_nested_archive(zipped: Path):
    inner = vfs.open_archive(vfs.open_archive(vfs.local(zipped)).child("inner.tar.gz"))
    assert [e.name for e in inner.list_dir()] == ["deep.txt"]
    assert inner.parent() == vfs.open_archive(vfs.local(zipped))
    assert inner.child("deep.txt").materialize().read_text() == "deep inside"
    assert inner.local_anchor() == zipped.parent


# -- capabilities -------------------------------------------------------------------


def test_archives_are_read_only(zipped: Path, tree: Path):
    root = vfs.open_archive(vfs.local(zipped))
    with pytest.raises(vfs.ReadOnlyError, match="pack.zip"):
        vfs.copy([vfs.local(tree / "src" / "a.txt")], root, on_conflict=never)
    with pytest.raises(vfs.ReadOnlyError):
        vfs.copy([root.child("top.txt")], vfs.local(tree / "dst"), move=True, on_conflict=never)
    with pytest.raises(vfs.ReadOnlyError):
        vfs.trash([root.child("top.txt")])
    with pytest.raises(vfs.ReadOnlyError):
        vfs.mkdir(root, "new")
    assert not root.fs.writable and vfs.LOCAL.writable


# -- copying ----------------------------------------------------------------------------


def test_local_copy_uses_native_path(tree: Path, monkeypatch):
    calls = []
    monkeypatch.setattr(fs, "copy_paths", lambda *a, **k: calls.append(a))
    vfs.copy([vfs.local(tree / "src" / "a.txt")], vfs.local(tree / "dst"), on_conflict=never)
    assert calls == [([tree / "src" / "a.txt"], tree / "dst")]


def test_extract_from_nested_archive(zipped: Path, tree: Path):
    pack = vfs.open_archive(vfs.local(zipped))
    inner = vfs.open_archive(pack.child("inner.tar.gz"))
    vfs.copy([inner.child("deep.txt"), pack.child("docs")], vfs.local(tree / "dst"), on_conflict=never)
    assert (tree / "dst" / "deep.txt").read_text() == "deep inside"
    assert (tree / "dst" / "docs" / "readme.md").read_text() == "# Hello\n"


def test_generic_copy_into_another_backend(zipped: Path, tree: Path):
    mem = MemoryFS()
    target = vfs.VPath(mem, "")
    progress: list[fs.Progress] = []
    vfs.copy(
        [vfs.local(tree / "src"), vfs.open_archive(vfs.local(zipped)).child("docs")],
        target,
        on_conflict=never,
        on_progress=progress.append,
    )
    assert mem.files["src/sub/c.txt"] == b"gamma"
    assert mem.files["docs/readme.md"] == b"# Hello\n"
    assert "src/sub" in mem.dirs
    assert progress[-1].files_done == progress[-1].files_total


def test_generic_move_and_conflicts(tree: Path):
    mem = MemoryFS()
    mem.files["a.txt"] = b"old"
    target = vfs.VPath(mem, "")
    vfs.copy([vfs.local(tree / "src" / "a.txt")], target, on_conflict=lambda s, d: fs.Conflict.SKIP)
    assert mem.files["a.txt"] == b"old"
    vfs.copy([vfs.local(tree / "src" / "a.txt")], target, on_conflict=lambda s, d: fs.Conflict.OVERWRITE)
    assert mem.files["a.txt"] == b"alpha"
    vfs.copy([vfs.VPath(mem, "a.txt")], vfs.local(tree / "dst"), move=True, on_conflict=never)
    assert (tree / "dst" / "a.txt").read_text() == "alpha" and "a.txt" not in mem.files


def test_materialize_non_local_file():
    mem = MemoryFS()
    mem.files["x.txt"] = b"payload"
    path = vfs.VPath(mem, "x.txt").materialize()
    assert path.name == "x.txt" and path.read_bytes() == b"payload"


def test_archive_inside_another_backend_can_be_entered(zipped: Path):
    mem = MemoryFS()
    mem.files["remote.zip"] = zipped.read_bytes()
    root = vfs.open_archive(vfs.VPath(mem, "remote.zip"))
    assert [e.name for e in root.list_dir()] == ["docs", "inner.tar.gz", "top.txt"]
    assert root.parent() == vfs.VPath(mem, "")
