"""Find files: name/content matching, the walk, and results as a virtual folder."""

from __future__ import annotations

import io
import os
import time
import zipfile
from pathlib import Path

import pytest

from orchard_commander import fs, search, vfs
from orchard_commander.search import NameMatcher, Query, SearchResultsFS
from test_vfs import MemoryFS


@pytest.fixture
def project(tmp_path: Path) -> Path:
    """report.pdf, Report-2026.PDF, big-report.zip(docs/inner.txt), notes.md,
    src/main.py, src/util.py, src/deep/deeper/config.py, .hidden/secret.txt"""
    root = tmp_path / "project"
    (root / "src" / "deep" / "deeper").mkdir(parents=True)
    (root / ".hidden").mkdir()
    (root / "report.pdf").write_bytes(b"%PDF-1.4 annual numbers")
    (root / "Report-2026.PDF").write_bytes(b"%PDF-1.4 more numbers" + b"x" * 5000)
    (root / "notes.md").write_text("# Notes\nThe Quick brown fox\nnothing else\n")
    (root / "src" / "main.py").write_text("import os\n\ndef main():\n    print('hello world')\n")
    (root / "src" / "util.py").write_text("def helper():\n    return 42\n")
    (root / "src" / "deep" / "deeper" / "config.py").write_text("DEBUG = True\n")
    (root / ".hidden" / "secret.txt").write_text("hello hidden")
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:  # compressed: text not in the raw bytes
        z.writestr("docs/inner.txt", "hello from inside the zip " * 20)
    (root / "big-report.zip").write_bytes(buf.getvalue())
    return root


def find(root, **options) -> list[str]:
    roots = (vfs.local(root),) if isinstance(root, Path) else (root,)
    hits: list[search.Hit] = []
    search.run(Query(roots=roots, **options), hits.append)
    return sorted(h.relative.replace(os.sep, "/") for h in hits)


# -- names --------------------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("pattern", "name", "expected"),
    [
        ("*.pdf", "Report.PDF", True),  # case-insensitive by default
        ("report", "big-report.zip", True),  # no wildcards: anywhere in the name
        ("*.py;*.md", "notes.md", True),
        ("*.py *.md", "x.txt", False),
        ("*.zip|*big*", "big-report.zip", False),  # "|" excludes
        ("*.zip|*big*", "small.zip", True),
        ("|*.tmp", "file.txt", True),  # only exclusions: everything else
        ("|*.tmp", "file.tmp", False),
        ("report?.pdf", "report1.pdf", True),
    ],
)
def test_total_commander_masks(pattern, name, expected):
    assert NameMatcher(pattern, regex=False, case=False).matches(name) is expected


def test_name_options():
    assert not NameMatcher("*.pdf", regex=False, case=True).matches("A.PDF")
    assert NameMatcher(r"^report-\d{4}", regex=True, case=False).matches("Report-2026.PDF")
    assert not NameMatcher(r"^report-\d{4}", regex=True, case=True).matches("Report-2026.PDF")


def test_walk_options(project: Path):
    assert find(project, names="*.pdf") == ["Report-2026.PDF", "report.pdf"]
    assert find(project, names="*.py") == ["src/deep/deeper/config.py", "src/main.py", "src/util.py"]
    assert find(project, names="*.py", max_depth=1) == ["src/main.py", "src/util.py"]
    assert find(project, names="*.py", recursive=False) == []
    assert find(project, names="*.txt") == [".hidden/secret.txt"]
    assert find(project, names="*.txt", include_hidden=False) == []
    assert find(project, names="d*", kinds="dirs") == ["src/deep", "src/deep/deeper"]
    assert find(project, names="*", kinds="files", min_size=1000) == ["Report-2026.PDF"]
    assert find(project, names="*.pdf", max_size=100) == ["report.pdf"]
    old = time.time() - 86400 * 30
    os.utime(project / "notes.md", (old, old))
    assert find(project, names="*.md", newer_than=time.time() - 86400) == []
    assert find(project, names="*.md", older_than=time.time() - 86400) == ["notes.md"]


# -- contents -----------------------------------------------------------------------------------


def test_content_search(project: Path):
    assert find(project, names="*", text="hello") == [".hidden/secret.txt", "src/main.py"]
    assert find(project, names="*", text="quick") == ["notes.md"]  # case-insensitive
    assert find(project, names="*", text="quick", text_case=True) == []
    assert find(project, names="*.py", text=r"def \w+\(\)", text_regex=True) == ["src/main.py", "src/util.py"]
    assert find(project, names="*.md", text="no", whole_words=True) == []  # "nothing" isn't the word "no"
    assert find(project, names="*.py", text="hello", not_containing=True) == [
        "src/deep/deeper/config.py", "src/util.py"]
    hits: list[search.Hit] = []
    search.run(Query(roots=(vfs.local(project),), names="*.py", text="print"), hits.append)
    assert hits[0].snippet == "print('hello world')"


def test_text_across_chunk_borders(tmp_path: Path):
    (tmp_path / "big.log").write_bytes(b"x" * (search.CHUNK - 3) + b"NEEDLE" + b"y" * 100)
    assert find(tmp_path, names="*.log", text="needle") == ["big.log"]


def test_search_inside_archives(project: Path):
    assert find(project, names="inner.txt") == []
    assert find(project, names="inner.txt", search_archives=True) == ["big-report.zip/docs/inner.txt"]
    hits: list[search.Hit] = []
    search.run(Query(roots=(vfs.local(project),), names="*.txt", text="inside", search_archives=True),
               hits.append)
    assert isinstance(hits[0].location.fs, vfs.ArchiveFS)


def test_search_on_another_file_system():
    mem = MemoryFS()
    mem.mkdir("photos/2026")
    mem.files["photos/2026/beach.jpg"] = b"jpg"
    mem.files["photos/notes.txt"] = b"beach day"
    assert find(vfs.VPath(mem, ""), names="beach") == ["photos/2026/beach.jpg"]
    assert find(vfs.VPath(mem, ""), names="*", text="beach") == ["photos/notes.txt"]


def test_cloud_only_files_skipped_for_content():
    from test_previewers import CloudFS

    cloud = CloudFS()
    cloud.files["a.txt"] = b"needle"
    progress = search.run(Query(roots=(vfs.VPath(cloud, ""),), names="*", text="needle"), lambda h: None)
    assert progress.hits == 0 and progress.skipped_online_only == 1 and cloud.bytes_read == 0
    assert find(vfs.VPath(cloud, ""), names="*", text="needle", include_online_only=True) == ["a.txt"]
    assert find(vfs.VPath(cloud, ""), names="*.txt") == ["a.txt"]  # names alone: no download needed


def test_cancel_and_errors(project: Path):
    hits: list[search.Hit] = []
    calls = {"n": 0}

    def cancel_after_two() -> bool:
        calls["n"] += 1
        return len(hits) >= 2

    search.run(Query(roots=(vfs.local(project),), names="*"), hits.append, is_cancelled=cancel_after_two)
    assert len(hits) == 2
    progress = search.run(Query(roots=(vfs.local(project / "missing"),)), lambda h: None)
    assert progress.errors and "missing" in progress.errors[0]


# -- results as a folder ------------------------------------------------------------------------


def test_results_folder_acts_on_real_files(project: Path, tmp_path: Path):
    query = Query(roots=(vfs.local(project),), names="*.py")
    results = SearchResultsFS(query)
    search.run(query, results.add)
    results.finish()
    root = vfs.VPath(results, "")
    listing = [e.name.replace(os.sep, "/") for e in root.list_dir()]
    assert listing == ["src/deep/deeper/config.py", "src/main.py", "src/util.py"]
    assert root.parent() == vfs.local(project)  # ".." goes back to where the search started
    assert root.display.startswith("Search: *.py in")
    assert "3 hits" in results.status_text

    hit = root.child(f"src{os.sep}util.py")
    assert hit.resolved == vfs.local(project / "src" / "util.py")
    assert hit.materialize().read_text().startswith("def helper")

    # Bulk move of all hits into one folder, then the results forget what moved.
    dest = tmp_path / "collected"
    dest.mkdir()
    sources = [root.child(e.name).resolved for e in root.list_dir()]
    vfs.copy(sources, vfs.local(dest), move=True, on_conflict=lambda s, d: fs.Conflict.CANCEL)
    assert sorted(p.name for p in dest.iterdir()) == ["config.py", "main.py", "util.py"]
    assert results.prune() == 3 and root.list_dir() == []


def test_results_same_relative_path_from_two_roots(tmp_path: Path):
    for side in ("a", "b"):
        (tmp_path / side).mkdir()
        (tmp_path / side / "x.txt").write_text(side)
    query = Query(roots=(vfs.local(tmp_path / "a"), vfs.local(tmp_path / "b")), names="x.txt")
    results = SearchResultsFS(query)
    search.run(query, results.add)
    names = sorted(e.name.replace(os.sep, "/") for e in vfs.VPath(results, "").list_dir())
    assert names == ["a/x.txt", "b/x.txt"]  # several roots: prefixed with the root's name


def test_copy_hits_out_of_many_archives(tmp_path: Path):
    """More archives than the open-archive cache holds: early ones get evicted, but hits must stay readable."""
    from orchard_commander import archives

    music = tmp_path / "music"
    music.mkdir()
    count = archives._OPEN_CACHE_SIZE + 4
    for i in range(count):
        with zipfile.ZipFile(music / f"album{i}.zip", "w") as z:
            z.writestr(f"disc1/cd/track{i}.mp3", f"mp3 {i}")
    query = Query(roots=(vfs.local(music),), names="*.mp3", search_archives=True)
    results = SearchResultsFS(query)
    search.run(query, results.add)
    assert len(results.hits) == count

    dest = tmp_path / "out"
    dest.mkdir()
    vfs.copy([h.location for h in results.hits], vfs.local(dest), move=False,
             on_conflict=lambda s, d: fs.Conflict.CANCEL)
    assert sorted(p.read_text() for p in dest.iterdir()) == sorted(f"mp3 {i}" for i in range(count))
