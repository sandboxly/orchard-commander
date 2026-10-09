import bz2
import gzip
import lzma
import shutil
import tarfile
import zipfile
from pathlib import Path

import py7zr
import pytest

from orchard_commander import archives, fs, vfs

DATA = Path(__file__).parent / "data"


def never(src, dst):
    raise AssertionError(f"unexpected conflict {src} -> {dst}")


@pytest.fixture
def content(tmp_path: Path) -> Path:
    """docs/readme.txt, docs/deep/x.txt, top.txt, .hidden"""
    root = tmp_path / "content"
    (root / "docs" / "deep").mkdir(parents=True)
    (root / "docs" / "readme.txt").write_text("read me")
    (root / "docs" / "deep" / "x.txt").write_text("x" * 5000)
    (root / "top.txt").write_text("top")
    (root / ".hidden").write_text("h")
    return root


def make(kind: str, content: Path, dest: Path) -> Path:
    files = sorted(p for p in content.rglob("*") if p.is_file())
    if kind == "zip":
        out = dest / "a.zip"
        with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
            for f in files:  # no explicit directory entries: they must be inferred
                z.write(f, f.relative_to(content).as_posix())
    elif kind in ("tar.gz", "tar.bz2", "tar.xz"):
        out = dest / f"a.{kind}"
        with tarfile.open(out, f"w:{kind.split('.')[1]}") as t:
            t.add(content, arcname=".")
    elif kind == "7z":
        out = dest / "a.7z"
        with py7zr.SevenZipFile(out, "w") as z:
            for f in files:
                z.write(f, arcname=f.relative_to(content).as_posix())
    else:
        raise ValueError(kind)
    return out


@pytest.mark.parametrize("kind", ["zip", "tar.gz", "tar.bz2", "tar.xz", "7z"])
def test_browse_and_extract(kind: str, content: Path, tmp_path: Path):
    archive = archives.open_archive(make(kind, content, tmp_path))
    root = archive.list_dir("")
    assert [e.name for e in root] == ["docs", "top.txt"]  # dirs first, hidden filtered
    assert ".hidden" in [e.name for e in archive.list_dir("", show_hidden=True)]
    docs = archive.list_dir("docs")
    assert [(e.name, e.is_dir) for e in docs] == [("deep", True), ("readme.txt", False)]
    assert archive.list_dir("docs/deep")[0].size == 5000

    out = tmp_path / "out"
    out.mkdir()
    progress: list[fs.Progress] = []
    root = vfs.open_archive(vfs.local(archive.path))
    vfs.copy([root.child("docs"), root.child("top.txt")], vfs.local(out), on_conflict=never, on_progress=progress.append)
    assert (out / "docs" / "deep" / "x.txt").read_text() == "x" * 5000
    assert (out / "docs" / "readme.txt").read_text() == "read me"
    assert (out / "top.txt").read_text() == "top"
    assert progress[-1].files_done == progress[-1].files_total == 3


def test_extract_single_dir_under_new_name(content: Path, tmp_path: Path):
    root = vfs.open_archive(vfs.local(make("zip", content, tmp_path)))
    vfs.copy([root.child("docs")], vfs.local(tmp_path / "renamed"), on_conflict=never)
    assert (tmp_path / "renamed" / "deep" / "x.txt").exists()


def test_extract_conflicts(content: Path, tmp_path: Path):
    top = vfs.open_archive(vfs.local(make("zip", content, tmp_path))).child("top.txt")
    out = tmp_path / "out"
    out.mkdir()
    (out / "top.txt").write_text("mine")
    vfs.copy([top], vfs.local(out), on_conflict=lambda s, d: fs.Conflict.SKIP)
    assert (out / "top.txt").read_text() == "mine"
    vfs.copy([top], vfs.local(out), on_conflict=lambda s, d: fs.Conflict.OVERWRITE)
    assert (out / "top.txt").read_text() == "top"


def test_extract_cancel_leaves_no_partial_file(content: Path, tmp_path: Path):
    root = vfs.open_archive(vfs.local(make("zip", content, tmp_path)))
    out = tmp_path / "out"
    out.mkdir()
    with pytest.raises(fs.OperationCancelled):
        vfs.copy([root.child("docs")], vfs.local(out), on_conflict=never, is_cancelled=lambda: True)
    assert not list(out.glob("**/*.txt"))


@pytest.mark.parametrize(("suffix", "opener"), [(".gz", gzip.open), (".bz2", bz2.open), (".xz", lzma.open)])
def test_single_compressed_file(suffix, opener, tmp_path: Path):
    path = tmp_path / f"notes.txt{suffix}"
    with opener(path, "wb") as f:
        f.write(b"hello world")
    archive = archives.open_archive(path)
    [entry] = archive.list_dir("")
    assert entry.name == "notes.txt"
    target = archives.extract_cached(archive, "notes.txt")
    assert target.name == "notes.txt" and target.read_bytes() == b"hello world"


def test_zip_slip_names_are_dropped(tmp_path: Path):
    path = tmp_path / "evil.zip"
    with zipfile.ZipFile(path, "w") as z:
        z.writestr("../escape.txt", "x")
        z.writestr("ok/./fine.txt", "y")
    archive = archives.open_archive(path)
    assert set(archive.members) == {"ok", "ok/fine.txt"}


def test_corrupt_archive_raises_archive_error(tmp_path: Path):
    bad = tmp_path / "bad.zip"
    bad.write_bytes(b"not a zip")
    with pytest.raises(archives.ArchiveError):
        archives.open_archive(bad)


def test_archive_type_detection():
    assert archives.archive_type("x.tar.gz") == "tar"
    assert archives.archive_type("X.TGZ") == "tar"
    assert archives.archive_type("x.gz") == "gz"
    assert archives.archive_type("x.zip") == "zip"
    assert archives.archive_type(".zip") is None
    assert archives.archive_type("x.txt") is None


def test_rar_listing_is_pure_python():
    archive = archives.open_archive(DATA / "rar3-solid.rar")
    assert [e.name for e in archive.list_dir("")] == ["stest1.txt", "stest2.txt"]


@pytest.mark.skipif(
    not any(shutil.which(t) for t in ("unrar", "7z", "7zz", "7za", "bsdtar")), reason="no RAR extraction tool"
)
@pytest.mark.parametrize("name", ["rar5-solid.rar", "seektest.rar"])
def test_rar_extract(name: str, tmp_path: Path):
    root = vfs.open_archive(vfs.local(DATA / name))
    vfs.copy([root.child("stest1.txt"), root.child("stest2.txt")], vfs.local(tmp_path), on_conflict=never)
    data = (tmp_path / "stest1.txt").read_bytes()
    assert len(data) == 2048 and data.startswith(b"000\n001\n")
