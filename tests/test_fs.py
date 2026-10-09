from pathlib import Path

import pytest

from orchard_commander import fs


def never(src, dst):
    raise AssertionError(f"unexpected conflict {src} -> {dst}")


def test_list_dir_dirs_first_and_hidden(tree: Path):
    names = [e.name for e in fs.list_dir(tree / "src")]
    assert names == ["sub", "a.txt", "b.bin"]
    names = [e.name for e in fs.list_dir(tree / "src", show_hidden=True)]
    assert ".hidden" in names


def test_list_dir_sort_by_size_reverse(tree: Path):
    files = [e.name for e in fs.list_dir(tree / "src", sort_key=fs.SortKey.SIZE, reverse=True) if not e.is_dir]
    assert files == ["b.bin", "a.txt"]


def test_copy_into_directory(tree: Path):
    src, dst = tree / "src", tree / "dst"
    progress: list[fs.Progress] = []
    fs.copy_paths([src / "a.txt", src / "sub"], dst, on_conflict=never, on_progress=progress.append)
    assert (dst / "a.txt").read_text() == "alpha"
    assert (dst / "sub" / "c.txt").read_text() == "gamma"
    assert (src / "a.txt").exists()
    assert progress[-1].files_done == progress[-1].files_total == 2


def test_copy_single_file_to_new_name(tree: Path):
    fs.copy_paths([tree / "src" / "a.txt"], tree / "dst" / "renamed.txt", on_conflict=never)
    assert (tree / "dst" / "renamed.txt").read_text() == "alpha"


def test_copy_conflict_skip_and_overwrite(tree: Path):
    src, dst = tree / "src", tree / "dst"
    (dst / "a.txt").write_text("old")
    fs.copy_paths([src / "a.txt"], dst, on_conflict=lambda s, d: fs.Conflict.SKIP)
    assert (dst / "a.txt").read_text() == "old"
    fs.copy_paths([src / "a.txt"], dst, on_conflict=lambda s, d: fs.Conflict.OVERWRITE)
    assert (dst / "a.txt").read_text() == "alpha"


def test_copy_conflict_cancel(tree: Path):
    (tree / "dst" / "a.txt").write_text("old")
    with pytest.raises(fs.OperationCancelled):
        fs.copy_paths([tree / "src" / "a.txt"], tree / "dst", on_conflict=lambda s, d: fs.Conflict.CANCEL)


def test_copy_cancel_removes_partial_file(tree: Path):
    with pytest.raises(fs.OperationCancelled):
        fs.copy_paths([tree / "src" / "b.bin"], tree / "dst", on_conflict=never, is_cancelled=lambda: True)
    assert not (tree / "dst" / "b.bin").exists()


def test_copy_dir_into_itself_rejected(tree: Path):
    with pytest.raises(fs.OperationError):
        fs.copy_paths([tree / "src"], tree / "src" / "sub", on_conflict=never)


def test_copy_multiple_to_missing_dir_rejected(tree: Path):
    with pytest.raises(fs.OperationError):
        fs.copy_paths([tree / "src" / "a.txt", tree / "src" / "b.bin"], tree / "nope", on_conflict=never)


def test_copy_preserves_symlink(tree: Path):
    link = tree / "src" / "link"
    link.symlink_to("a.txt")
    fs.copy_paths([link], tree / "dst", on_conflict=never)
    assert (tree / "dst" / "link").is_symlink()


def test_move_and_rename(tree: Path):
    src, dst = tree / "src", tree / "dst"
    fs.move_paths([src / "sub"], dst, on_conflict=never)
    assert (dst / "sub" / "c.txt").exists() and not (src / "sub").exists()
    fs.move_paths([src / "a.txt"], src / "z.txt", on_conflict=never)
    assert (src / "z.txt").read_text() == "alpha" and not (src / "a.txt").exists()


def test_move_merges_directories(tree: Path):
    src, dst = tree / "src", tree / "dst"
    (dst / "sub").mkdir()
    (dst / "sub" / "keep.txt").write_text("k")
    fs.move_paths([src / "sub"], dst, on_conflict=never)
    assert {p.name for p in (dst / "sub").iterdir()} == {"keep.txt", "c.txt"}
    assert not (src / "sub").exists()


def test_move_conflict_skip_keeps_source(tree: Path):
    (tree / "dst" / "a.txt").write_text("old")
    fs.move_paths([tree / "src" / "a.txt"], tree / "dst", on_conflict=lambda s, d: fs.Conflict.SKIP)
    assert (tree / "src" / "a.txt").exists()
    assert (tree / "dst" / "a.txt").read_text() == "old"


def test_trash(tree: Path, monkeypatch):
    trashed = []
    monkeypatch.setattr(fs, "send2trash", trashed.append)
    fs.trash_paths([tree / "src" / "a.txt"])
    assert trashed == [tree / "src" / "a.txt"]


def test_human_size():
    assert fs.human_size(512) == "512 B"
    assert fs.human_size(2048) == "2.0 K"
    assert fs.human_size(5 * 1024**3) == "5.0 G"
