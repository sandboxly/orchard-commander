"""The previewer plugin system: a contract every previewer must meet, the
registry's selection rules, and plugin registration."""

from __future__ import annotations

import io
import os
import zipfile
from pathlib import Path

import pytest
from PySide6.QtGui import QColor, QImage

from orchard_commander import vfs
from orchard_commander.preview import registry
from orchard_commander.preview.api import (
    Access,
    CannotPreview,
    FileInfo,
    LoadContext,
    Loaded,
    Previewer,
    PreviewView,
    Score,
    ShowMessage,
    ViewContext,
)
from orchard_commander.preview.source import PreviewSource
from orchard_commander.themes import get_scheme
from test_vfs import MemoryFS

CTX = LoadContext(colors=get_scheme("monokai"), show_hidden=False)


class CountingFS(MemoryFS):
    """A non-local file system that counts bytes read."""

    type_name = "test server"

    def __init__(self) -> None:
        super().__init__()
        self.bytes_read = 0

    def open_read(self, path):
        fs_, data = self, self.files[path]

        class Reader(io.BytesIO):
            def read(self, size=-1):
                chunk = super().read(size)
                fs_.bytes_read += len(chunk)
                return chunk

        return Reader(data)


def _png_bytes() -> bytes:
    from PySide6.QtCore import QBuffer, QByteArray, QIODevice

    image = QImage(8, 4, QImage.Format.Format_RGB32)
    image.fill(QColor("green"))
    data = QByteArray()
    buffer = QBuffer(data)
    buffer.open(QIODevice.OpenModeFlag.WriteOnly)
    image.save(buffer, "PNG")
    return bytes(data)


def _zip_bytes() -> bytes:
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        z.writestr("inside.txt", "hi")
    return buf.getvalue()


EXTS = ["txt", "md", "py", "png", "gif", "mp4", "mp3", "zip", "tar.gz", "7z", "rar", "pdf", "bin", ""]


def pdf_bytes(pages: int = 3) -> bytes:
    import pypdfium2 as pdfium

    pdf = pdfium.PdfDocument.new()
    for i in range(pages):
        pdf.new_page(595, 842 if i % 2 == 0 else 420)  # A4 portrait, then a landscape-ish page
    buf = io.BytesIO()
    pdf.save(buf)
    pdf.close()
    return buf.getvalue()


def corpus(qapp) -> dict[str, bytes]:
    """Empty, garbage and valid files for each extension previewers care about."""
    files: dict[str, bytes] = {}
    for ext in EXTS:
        suffix = f".{ext}" if ext else ""
        files[f"empty{suffix}"] = b""
        files[f"garbage{suffix}"] = os.urandom(3000)
        files[f"text{suffix}"] = b"plain words\nand more\n"
    files["valid.png"] = _png_bytes()
    files["valid.zip"] = _zip_bytes()
    files["valid.md"] = b"# Title\n\nSome *markdown*.\n"
    files["valid.pdf"] = pdf_bytes()
    files["pdf-without-extension"] = pdf_bytes(1)
    return files


@pytest.fixture(params=["local", "other-fs"])
def sources(request, tmp_path: Path, qapp) -> list[PreviewSource]:
    """The corpus as PreviewSources, on the local disk and on a non-local file system."""
    files = corpus(qapp)
    if request.param == "local":
        for name, data in files.items():
            (tmp_path / name).write_bytes(data)
        (tmp_path / "folder").mkdir()
        return [PreviewSource(vfs.local(tmp_path / n)) for n in [*files, "folder"]]
    mem = CountingFS()
    mem.files.update(files)
    mem.dirs.add("folder")
    return [PreviewSource(vfs.VPath(mem, n)) for n in [*files, "folder"]]


# -- the contract, for every registered previewer ------------------------------------------


@pytest.mark.parametrize("previewer", registry.previewers(), ids=lambda p: p.id)
class TestPreviewerContract:
    def test_identity(self, previewer: Previewer):
        assert previewer.id and previewer.title
        assert isinstance(previewer.access, Access)
        assert registry.get(previewer.id) is previewer

    def test_score_handles_anything(self, previewer: Previewer):
        weird = [
            FileInfo("", "", 0, 0.0, False, True),
            FileInfo(".hidden", "", 10, 0.0, False, False, head=b""),
            FileInfo("dir.zip", "zip", 0, 0.0, True, True),
            FileInfo("x" * 500 + ".PNG", "png", 10**12, 0.0, False, False, head=os.urandom(64)),
            FileInfo("a.md", "md", 5, 0.0, False, True, head=b"\x00\x01"),
        ]
        for info in weird:
            score = previewer.score(info)
            assert isinstance(score, int) and score >= 0

    def test_load_returns_or_refuses_cleanly(self, previewer: Previewer, sources: list[PreviewSource], qtbot):
        """For every file it claims, load() returns a result or raises CannotPreview /
        ShowMessage — nothing else — and the view can show and clear the result."""
        view = previewer.create_view(ViewContext(_themes()))
        qtbot.addWidget(view)
        assert isinstance(view, PreviewView)
        claimed = 0
        for src in sources:
            if previewer.score(src.info(with_head=True)) <= 0:
                continue
            claimed += 1
            try:
                loaded = previewer.load(src, CTX)
            except (CannotPreview, ShowMessage):
                continue
            assert isinstance(loaded, Loaded), src.name
            assert isinstance(loaded.info, str)
            view.show_data(loaded.data)
            view.clear()
            if loaded.close is not None:
                loaded.close()
        assert claimed > 0, f"{previewer.id} claimed no file of the test corpus"


def _themes():
    from orchard_commander.ui.style import ThemeManager

    return ThemeManager()


# -- selection ---------------------------------------------------------------------------------


def ids(src: PreviewSource) -> list[str]:
    return [c.previewer.id for c in registry.candidates(src)]


def test_ranking(tmp_path: Path, qapp):
    (tmp_path / "a.md").write_text("# hi\n")
    (tmp_path / "a.py").write_text("print(1)\n")
    (tmp_path / "a.bin").write_bytes(bytes(range(256)))
    (tmp_path / "e.txt").write_bytes(b"")
    assert ids(PreviewSource(vfs.local(tmp_path / "a.md"))) == ["markdown", "text", "hex"]
    assert ids(PreviewSource(vfs.local(tmp_path / "a.py"))) == ["text", "hex"]
    assert ids(PreviewSource(vfs.local(tmp_path / "a.bin"))) == ["hex"]
    assert ids(PreviewSource(vfs.local(tmp_path / "e.txt"))) == ["hex"]  # shows "(empty file)"
    assert ids(PreviewSource(vfs.local(tmp_path))) == ["listing"]


def test_fallback_when_a_previewer_cannot_show_the_file(tmp_path: Path, qapp):
    (tmp_path / "broken.png").write_bytes(b"not an image at all")
    outcome = registry.preview(vfs.local(tmp_path / "broken.png"), CTX)
    assert [c.previewer.id for c in outcome.candidates] == ["image", "hex"]
    assert outcome.previewer is not None and outcome.previewer.id == "hex"
    assert outcome.error is None


def test_preferred_previewer_wins_if_it_fits(tmp_path: Path, qapp):
    (tmp_path / "a.md").write_text("# hi\n")
    target = vfs.local(tmp_path / "a.md")
    assert registry.preview(target, CTX, preferred="text").previewer.id == "text"
    assert registry.preview(target, CTX, preferred="image").previewer.id == "markdown"  # doesn't fit: ignored


def test_media_is_chosen_by_name_without_reading(qapp):
    mem = CountingFS()
    mem.files["movie.mp4"] = os.urandom(100_000)
    assert ids(PreviewSource(vfs.VPath(mem, "movie.mp4")))[0] == "media"
    assert mem.bytes_read == 0  # the name decided; no sniffing round trip to the server


def test_show_message_stops_the_search(tmp_path: Path, qapp, monkeypatch):
    from orchard_commander.preview.builtin.image import ImagePreviewer

    monkeypatch.setattr(ImagePreviewer, "max_size", 10)
    mem = CountingFS()
    mem.files["big.png"] = _png_bytes()
    outcome = registry.preview(vfs.VPath(mem, "big.png"), CTX)
    assert isinstance(outcome.error, ShowMessage) and "copy it with F5" in str(outcome.error)
    assert outcome.loaded is None and mem.bytes_read == 0  # no hex fallback, nothing fetched


# -- plugins -----------------------------------------------------------------------------------


class DummyView(PreviewView):
    def __init__(self) -> None:
        super().__init__()
        self.shown = None

    def show_data(self, data) -> None:
        self.shown = data


class StlPreviewer(Previewer):
    id = "test-stl"
    title = "3D model"
    access = Access.LOCAL_FILE

    def score(self, f: FileInfo) -> int:
        return Score.FORMAT if f.ext == "stl" else Score.NO

    def load(self, src, ctx) -> Loaded:
        return Loaded(src.local_file().read_bytes()[:5], info="mesh")

    def create_view(self, ctx) -> PreviewView:
        return DummyView()


class MissingDependency(StlPreviewer):
    id = "test-unavailable"

    @classmethod
    def available(cls) -> bool:
        return False


@pytest.fixture
def clean_registry(monkeypatch):
    """A fresh registry (built-ins reloaded), restored afterwards."""
    monkeypatch.setattr(registry, "_previewers", {})
    monkeypatch.setattr(registry, "_initialized", False)
    return registry


def test_register_a_previewer_at_runtime(clean_registry, tmp_path: Path, qapp):
    (tmp_path / "part.stl").write_bytes(b"solid part\n")
    clean_registry.register(StlPreviewer)
    outcome = clean_registry.preview(vfs.local(tmp_path / "part.stl"), CTX)
    assert outcome.previewer.id == "test-stl" and outcome.loaded.data == b"solid"
    with pytest.raises(ValueError):
        clean_registry.register(StlPreviewer)  # ids are unique
    clean_registry.register(MissingDependency)
    assert clean_registry.get("test-unavailable") is None  # skipped: dependency missing


def test_entry_point_plugins(clean_registry, monkeypatch, caplog):
    class EP:
        def __init__(self, name, target):
            self.name, self.value, self._target = name, f"pkg:{name}", target

        def load(self):
            if isinstance(self._target, Exception):
                raise self._target
            return self._target

    eps = [EP("stl", StlPreviewer), EP("broken", ImportError("no module named pdfium")), EP("na", MissingDependency)]
    monkeypatch.setattr(registry, "entry_points", lambda group: eps if group == registry.ENTRY_POINT_GROUP else [])
    found = [p.id for p in clean_registry.previewers()]
    assert "test-stl" in found and "text" in found  # add-on next to the built-ins
    assert "test-unavailable" not in found
    assert "could not load previewer broken" in caplog.text  # a broken add-on is logged, not fatal


# -- PDF ---------------------------------------------------------------------------------------


def test_pdf_ranking_and_sniffing(tmp_path: Path, qapp):
    (tmp_path / "doc.pdf").write_bytes(pdf_bytes())
    (tmp_path / "download").write_bytes(pdf_bytes(1))  # no extension: recognised by "%PDF-"
    assert ids(PreviewSource(vfs.local(tmp_path / "doc.pdf"))) == ["pdf", "hex"]
    assert ids(PreviewSource(vfs.local(tmp_path / "download")))[0] == "pdf"


def test_pdf_load_renders_first_page(tmp_path: Path, qapp):
    (tmp_path / "doc.pdf").write_bytes(pdf_bytes(3))
    outcome = registry.preview(vfs.local(tmp_path / "doc.pdf"), CTX)
    assert outcome.previewer.id == "pdf" and outcome.loaded.info.startswith("3 pages")
    data = outcome.loaded.data
    assert data.first_page.width() == 1200 and not data.first_page.isNull()
    assert data.first_page.pixelColor(10, 10).name() == "#ffffff"  # blank page renders white
    page = data.document.render(1, 300)  # the landscape page: wider than tall
    assert page.width() == 300 and page.height() < 300
    outcome.loaded.close()
    with pytest.raises(CannotPreview):
        data.document.render(0, 100)  # closed


def test_pdf_on_another_file_system_is_streamed(qapp, monkeypatch):
    mem = CountingFS()
    mem.files["remote.pdf"] = pdf_bytes(2)
    monkeypatch.setattr(CountingFS, "materialize", lambda self, path: pytest.fail("PDF was downloaded"))
    outcome = registry.preview(vfs.VPath(mem, "remote.pdf"), CTX)
    assert outcome.previewer.id == "pdf" and outcome.loaded.data.document.page_count == 2
    outcome.loaded.close()


def test_corrupt_pdf_falls_back_to_hex(tmp_path: Path, qapp):
    (tmp_path / "broken.pdf").write_bytes(b"%PDF-1.7\nthis is not really a pdf")
    outcome = registry.preview(vfs.local(tmp_path / "broken.pdf"), CTX)
    assert outcome.previewer.id == "hex" and outcome.error is None


# -- online-only cloud files (Dropbox, iCloud, OneDrive placeholders) ---------------------------


class CloudFS(CountingFS):
    """Files are online-only placeholders: any read would download them."""

    def stat(self, path):
        from dataclasses import replace

        entry = super().stat(path)
        return entry if entry.is_dir else replace(entry, is_online_only=True)


def test_big_online_only_file_is_not_downloaded(qapp):
    from orchard_commander.preview.api import NeedsDownload

    cloud = CloudFS()
    cloud.files["big.pdf"] = pdf_bytes(2) + b"\0" * (30 * 1024**2)
    cloud.files["notes.unknown"] = b"x" * (30 * 1024**2)  # would need sniffing to classify
    for name in ("big.pdf", "notes.unknown"):
        outcome = registry.preview(vfs.VPath(cloud, name), CTX)
        assert isinstance(outcome.error, NeedsDownload) and outcome.loaded is None
    assert cloud.bytes_read == 0  # not even the first bytes for sniffing
    allowed = LoadContext(colors=CTX.colors, show_hidden=False, download_limit=None)
    outcome = registry.preview(vfs.VPath(cloud, "big.pdf"), allowed)  # the user asked for it
    assert outcome.previewer.id == "pdf"
    outcome.loaded.close()


def test_small_online_only_file_previews_automatically(qapp):
    cloud = CloudFS()
    cloud.files["small.txt"] = b"hello from the cloud\n"
    outcome = registry.preview(vfs.VPath(cloud, "small.txt"), CTX)
    assert outcome.previewer.id == "text" and outcome.error is None


def test_online_only_flags_detected(monkeypatch):
    from types import SimpleNamespace

    from orchard_commander import fs

    monkeypatch.setattr(fs, "IS_MACOS", True)
    monkeypatch.setattr(fs, "IS_WINDOWS", False)
    assert fs._is_online_only(SimpleNamespace(st_flags=fs.SF_DATALESS))
    assert not fs._is_online_only(SimpleNamespace(st_flags=0))
    monkeypatch.setattr(fs, "IS_MACOS", False)
    monkeypatch.setattr(fs, "IS_WINDOWS", True)
    assert fs._is_online_only(SimpleNamespace(st_file_attributes=0x00400000))  # RECALL_ON_DATA_ACCESS
    assert not fs._is_online_only(SimpleNamespace(st_file_attributes=0x20))  # ARCHIVE


class StuckCloudFS(CloudFS):
    """Online-only files that arrive only once `deliver` is set (like a stuck Dropbox)."""

    def __init__(self) -> None:
        import threading

        super().__init__()
        self.deliver = threading.Event()
        self.fetches = 0

    def fetch_online_only(self, path, *, timeout, is_cancelled=lambda: False):
        import time as time_module

        self.fetches += 1
        deadline = time_module.monotonic() + timeout
        while not self.deliver.wait(0.05):
            if is_cancelled():
                raise vfs.FetchCancelled(path)
            if time_module.monotonic() >= deadline:
                raise vfs.FetchTimeout(path)


def test_stuck_cloud_download_times_out_with_explanation(qapp, monkeypatch):
    import time as time_module

    from orchard_commander.preview.api import CloudNotDelivered

    monkeypatch.setattr(registry, "fetch_timeout", lambda size: 0.3)
    cloud = StuckCloudFS()
    cloud.files["report.pdf"] = pdf_bytes(1)
    t0 = time_module.monotonic()
    outcome = registry.preview(vfs.VPath(cloud, "report.pdf"), CTX)
    assert time_module.monotonic() - t0 < 2
    assert isinstance(outcome.error, CloudNotDelivered) and "report.pdf hasn't arrived" in str(outcome.error)
    assert [c.previewer.id for c in outcome.candidates] == ["pdf", "hex"]  # by name, without reading
    assert cloud.bytes_read == 0

    cancelled = LoadContext(colors=CTX.colors, show_hidden=False, is_cancelled=lambda: True)
    assert registry.preview(vfs.VPath(cloud, "report.pdf"), cancelled).error is None  # moved on: no message

    cloud.deliver.set()  # the cloud app catches up: "Try again" works
    outcome = registry.preview(vfs.VPath(cloud, "report.pdf"), CTX)
    assert outcome.previewer.id == "pdf"
    outcome.loaded.close()


def test_local_cloud_fetch_uses_one_dedicated_thread(monkeypatch, tmp_path: Path):
    """A stuck placeholder read must not occupy shared workers, and retries reuse it."""
    import threading
    import time as time_module

    release = threading.Event()
    reads: list[str] = []
    fetches = vfs._CloudFetches()

    def slow_read(path, done):
        reads.append(threading.current_thread().name)
        release.wait(10)
        with fetches._lock:
            fetches._running.pop(path, None)
        done.set()

    monkeypatch.setattr(fetches, "_read", slow_read)
    for _ in range(3):  # three attempts while the cloud app is stuck
        with pytest.raises(vfs.FetchTimeout):
            fetches.fetch(str(tmp_path / "x.pdf"), timeout=0.2, is_cancelled=lambda: False)
    assert len(reads) == 1 and reads[0].startswith("cloud-fetch")  # one download, its own thread
    with pytest.raises(vfs.FetchCancelled):
        fetches.fetch(str(tmp_path / "x.pdf"), timeout=5, is_cancelled=lambda: True)
    release.set()
    t0 = time_module.monotonic()
    fetches.fetch(str(tmp_path / "x.pdf"), timeout=5, is_cancelled=lambda: False)  # completes
    assert time_module.monotonic() - t0 < 2
