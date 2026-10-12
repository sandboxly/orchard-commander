"""Batch renaming: TC mask placeholders, search & replace, planning checks, safe execution, undo."""

from __future__ import annotations

import datetime as dt
import sys
from pathlib import Path

import pytest

from orchard_commander import rename, vfs
from orchard_commander.rename import Item, MaskError, Rule

WHEN = dt.datetime(2026, 3, 7, 9, 5, 4).timestamp()


def item(name: str, *, is_dir: bool = False, parent: str = "Holiday", grandparent: str = "Photos") -> Item:
    return Item(vfs.local(Path("/nowhere") / name), name, is_dir, WHEN, parent, grandparent)


def new(name: str, index: int = 0, **rule) -> str:
    return rename.new_name(item(name), index, Rule(**rule))


@pytest.mark.parametrize(
    ("mask", "expected"),
    [
        ("[N]", "IMG_20260307_beach"),
        ("[N1-3]", "IMG"),
        ("[N5,8]", "20260307"),
        ("[N14-]", "beach"),
        ("[N-5-]", "beach"),
        ("[N-5,2]", "be"),
        ("[N-5-4]", "be"),
        ("[N5--7]", "20260307"),
        ("[N1]", "I"),
        ("[2-4]", "MG_"),  # whole name incl. extension
        ("[P] - [N14-]", "Holiday - beach"),
        ("[G]_[P]", "Photos_Holiday"),
        ("[Y]-[M]-[D] [h].[m].[s]", "2026-03-07 09.05.04"),
        ("[d] [t]", "2026-03-07 09.05.04"),
        ("[U][N14-][n]_[N1-3]", "BEACH_IMG"),
        ("[L][N1-3]", "img"),
        ("[F]my holiday [N14-]", "My Holiday Beach"),
        ("[[]x[]]", "[x]"),
        ("photo", "photo"),
    ],
)
def test_name_mask(mask, expected):
    assert new("IMG_20260307_beach.JPG", name_mask=mask, ext_mask="") == expected


def test_extension_mask_and_folders():
    assert new("a.tar.gz") == "a.tar.gz"
    assert new("a.JPG", ext_mask="[L][E]") == "a.jpg"
    assert new("a.jpeg", ext_mask="[E1-2]") == "a.jp"
    assert new(".bashrc") == ".bashrc"  # a dot file has no extension
    assert new("no_ext", ext_mask="txt") == "no_ext.txt"
    folder = item("v1.2", is_dir=True)
    assert rename.new_name(folder, 0, Rule(name_mask="[N]-old", ext_mask="[E]")) == "v1.2-old"  # no ext for dirs


def test_counter():
    assert [new("x.jpg", i, name_mask="[C]") for i in range(3)] == ["1.jpg", "2.jpg", "3.jpg"]
    assert [new("x.jpg", i, name_mask="[C10+5:3]") for i in range(3)] == ["010.jpg", "015.jpg", "020.jpg"]
    assert new("x.jpg", 2, name_mask="Img[C:4]", counter_start=7) == "Img0009.jpg"
    assert new("x.jpg", 1, name_mask="[C]", counter_start=100, counter_step=10, counter_digits=4) == "0110.jpg"


@pytest.mark.parametrize("mask", ["[N", "[X]", "[N0-3]", "[Cabc]", "a]b", "[N2,]"])
def test_bad_masks(mask):
    with pytest.raises(MaskError):
        new("a.txt", name_mask=mask)


def test_search_and_replace():
    assert new("My File copy.txt", search="copy", replace="") == "My File .txt"
    assert new("a-b_c.txt", search="-|_", replace=" ") == "a b c.txt"  # several searches, one replacement
    assert new("a-b_c.txt", search="-|_", replace="+|=") == "a+b=c.txt"  # pairs
    assert new("ABC.txt", search="b", replace="x") == "AxC.txt"  # case-insensitive by default
    assert new("ABC.txt", search="b", replace="x", case_sensitive=True) == "ABC.txt"
    assert new("2026-03-07 trip.jpg", search=r"(\d+)-(\d+)-(\d+)", replace="$3.$2.$1", regex=True) == \
        "07.03.2026 trip.jpg"
    assert new("a.b.c", search=".", replace="_") == "a_b_c"  # plain text: "." is a dot, not "any"


def test_case_conversion():
    assert new("hello WORLD.TXT", case="lower") == "hello world.txt"
    assert new("hello WORLD.txt", case="upper") == "HELLO WORLD.TXT"
    assert new("hello WORLD.txt", case="first") == "Hello world.txt"
    assert new("hello WORLD-again.txt", case="words") == "Hello World-Again.Txt"


# -- planning ------------------------------------------------------------------------------------


def items_in(folder: Path, *names: str) -> list[Item]:
    root = vfs.local(folder)
    return [rename.item_for(root.child(n), root.child(n).stat()) for n in names]


def test_plan_finds_problems(tmp_path: Path):
    for n in ("a.txt", "b.txt", "keep.txt"):
        (tmp_path / n).write_text(n)
    items = items_in(tmp_path, "a.txt", "b.txt")
    assert rename.plan(items, Rule(name_mask="same")).problems[0].error.startswith("same new name")
    assert not rename.plan(items, Rule(name_mask="same")).can_run
    clash = rename.plan(items[:1], Rule(name_mask="keep"))
    assert clash.problems[0].error == "a file with this name already exists"
    bad = rename.plan(items, Rule(name_mask="[N"))
    assert "never closed" in bad.mask_error and not bad.can_run
    assert "Search" in rename.plan(items, Rule(search="(", regex=True)).mask_error
    assert rename.plan(items, Rule(name_mask="x/[N]")).problems[0].error == "contains “/”"
    nothing = rename.plan(items, Rule())
    assert not nothing.can_run and not nothing.problems and nothing.to_rename == []


def test_plan_target_taken_by_unchanged_item(tmp_path: Path):
    (tmp_path / "a.txt").write_text("a")
    (tmp_path / "b.txt").write_text("b")
    items = items_in(tmp_path, "a.txt", "b.txt")
    plan = rename.plan(items, Rule(search="a", replace="b"))  # a→b, b stays b
    assert not plan.can_run and plan.problems[0].item.name == "a.txt"


def test_swap_and_rotate(tmp_path: Path):
    for n in ("1.txt", "2.txt", "3.txt"):
        (tmp_path / n).write_text(f"content {n}")
    items = items_in(tmp_path, "1.txt", "2.txt", "3.txt")
    plan = rename.plan(items, Rule(name_mask="[C2]"))  # 1→2, 2→3, 3→4
    assert plan.can_run
    done = rename.execute(plan)
    assert not done.errors
    assert {p.name: p.read_text() for p in tmp_path.iterdir()} == {
        "2.txt": "content 1.txt", "3.txt": "content 2.txt", "4.txt": "content 3.txt"}
    undone = rename.undo(done)
    assert not undone.errors
    assert {p.name: p.read_text() for p in tmp_path.iterdir()} == {
        "1.txt": "content 1.txt", "2.txt": "content 2.txt", "3.txt": "content 3.txt"}


def test_case_only_change(tmp_path: Path):
    (tmp_path / "photo.JPG").write_text("x")
    plan = rename.plan(items_in(tmp_path, "photo.JPG"), Rule(ext_mask="[L][E]"))
    assert plan.can_run, plan.problems
    rename.execute(plan)
    assert [p.name for p in tmp_path.iterdir()] == ["photo.jpg"]


def test_folder_and_its_contents(tmp_path: Path):
    (tmp_path / "docs").mkdir()
    (tmp_path / "docs" / "one.txt").write_text("1")
    root = vfs.local(tmp_path)
    items = [rename.item_for(p, p.stat()) for p in (root.child("docs"), root.child("docs", "one.txt"))]
    plan = rename.plan(items, Rule(name_mask="[N]_x"))
    assert "renamed too" in plan.problems[0].error and plan.problems[0].item.name == "one.txt"


def test_failure_keeps_old_name(tmp_path: Path, monkeypatch):
    (tmp_path / "a.txt").write_text("a")
    plan = rename.plan(items_in(tmp_path, "a.txt"), Rule(name_mask="b"))
    real = vfs.LocalFS.rename
    calls = []

    def flaky(self, src, dst):
        calls.append(dst)
        if len(calls) == 2:  # the final step fails
            raise PermissionError("nope")
        real(self, src, dst)

    monkeypatch.setattr(vfs.LocalFS, "rename", flaky)
    done = rename.execute(plan)
    assert done.errors and "nope" in done.errors[0] and done.renames == []
    assert [p.name for p in tmp_path.iterdir()] == ["a.txt"]


def test_works_on_other_file_systems():
    from test_vfs import MemoryFS

    class RenamingFS(MemoryFS):
        can_rename = True

        def rename(self, src: str, dst: str) -> None:
            if dst in self.files:
                raise FileExistsError(dst)
            self.files[dst] = self.files.pop(src)

    mem = RenamingFS()
    mem.files["x.txt"] = b"x"
    loc = vfs.VPath(mem, "x.txt")
    plan = rename.plan([rename.item_for(loc, loc.stat())], Rule(name_mask="y"))
    rename.execute(plan)
    assert "y.txt" in mem.files and "x.txt" not in mem.files


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX names")
def test_name_problems():
    assert rename.name_problem("") == "empty name"
    assert rename.name_problem("a" * 300) == "name too long"
    assert rename.name_problem("fine name.txt") is None
