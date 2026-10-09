"""PDF documents, rendered with PDFium (pypdfium2).

Documents are read from a seekable stream when the file system offers one,
so a PDF on a server is not downloaded first; PDFium fetches only the parts it
needs. Pages render on worker threads at the exact size of the pane.
"""

from __future__ import annotations

import importlib.util
import threading
import time
from collections import OrderedDict
from dataclasses import dataclass
from typing import Any, BinaryIO

from PySide6.QtCore import Qt, QTimer
from PySide6.QtGui import QImage, QKeyEvent, QPixmap, QResizeEvent, QWheelEvent
from PySide6.QtWidgets import QHBoxLayout, QLabel, QSizePolicy, QToolButton, QVBoxLayout, QWidget

from orchard_commander import fs
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
from orchard_commander.ui import tasks

WHEEL_STEP = 120  # one notch of a classic mouse wheel (Qt's angle units)
WHEEL_PAUSE_S = 0.35  # after turning a page, ignore the rest of that flick
MAX_FETCH_BYTES = 512 * 1024**2  # PDFs without a direct path or seekable stream are fetched whole
FIRST_PAGE_WIDTH = 1200  # pixels for the page rendered during load (replaced at pane size)
MAX_RENDER_PIXELS = 6000  # cap on either side of a rendered page
CACHE_PAGES = 8

# PDFium is not thread-safe: every call into it goes through this lock.
_pdfium_lock = threading.RLock()


class PdfDocument:
    """An open PDF. Thread-safe; renders pages to QImages."""

    def __init__(self, source: Any, stream: BinaryIO | None = None) -> None:
        import pypdfium2 as pdfium

        self._stream = stream
        with _pdfium_lock:
            try:
                self._doc = pdfium.PdfDocument(source)
            except pdfium.PdfiumError as exc:
                if stream is not None:
                    stream.close()
                if "password" in str(exc).lower():
                    raise ShowMessage("This PDF is password-protected.") from exc
                raise CannotPreview(f"Not a readable PDF: {exc}") from exc
            self.page_count = len(self._doc)
            meta = self._doc.get_metadata_dict(skip_empty=True)
        self.title = str(meta.get("Title", "")).strip()
        self._closed = False

    def page_size(self, index: int) -> tuple[float, float]:
        with _pdfium_lock:
            self._check_open()
            return self._doc.get_page_size(index)  # points

    def render(self, index: int, width_px: int) -> QImage:
        """Render page `index` (0-based) `width_px` pixels wide."""
        with _pdfium_lock:
            self._check_open()
            page = self._doc[index]
            try:
                width_pt, height_pt = page.get_size()
                scale = min(width_px / width_pt, MAX_RENDER_PIXELS / max(width_pt, height_pt))
                bitmap = page.render(scale=max(scale, 0.05), prefer_bgrx=True, rev_byteorder=True)
                image = QImage(bitmap.buffer, bitmap.width, bitmap.height, bitmap.stride,
                               QImage.Format.Format_RGBX8888).copy()  # copy: the buffer belongs to PDFium
                bitmap.close()
            finally:
                page.close()
        return image

    def close(self) -> None:
        with _pdfium_lock:
            if self._closed:
                return
            self._closed = True
            self._doc.close()
            if self._stream is not None:
                self._stream.close()

    def _check_open(self) -> None:
        if self._closed:
            raise CannotPreview("document closed")


@dataclass(slots=True)
class PdfData:
    document: PdfDocument
    first_page: QImage


def _stream_usable(stream: BinaryIO) -> bool:
    """PDFium needs random access: seek/tell/read/readinto."""
    try:
        return stream.seekable() and all(hasattr(stream, m) for m in ("tell", "read", "readinto"))
    except (AttributeError, OSError, ValueError):
        return False


class PdfPreviewer(Previewer):
    id = "pdf"
    title = "PDF"
    access = Access.STREAM
    max_size = MAX_FETCH_BYTES
    needs_head = True  # recognise PDFs without a .pdf extension

    @classmethod
    def available(cls) -> bool:
        return importlib.util.find_spec("pypdfium2") is not None

    def score(self, f: FileInfo) -> int:
        if f.is_dir or f.is_empty:
            return Score.NO
        if f.ext == "pdf" or (f.head is not None and f.head.startswith(b"%PDF-")):
            return Score.FORMAT
        return Score.NO

    def load(self, src: PreviewSource, ctx: LoadContext) -> Loaded:
        if (path := src.direct_path) is not None:
            document = PdfDocument(str(path))
        else:
            stream = src.open_stream()
            if _stream_usable(stream):
                document = PdfDocument(stream, stream)  # PDFium reads only the parts it needs
            else:
                stream.close()
                document = PdfDocument(str(src.local_file(self.max_size)))
        try:
            if document.page_count == 0:
                raise CannotPreview("PDF without pages")
            first = document.render(0, FIRST_PAGE_WIDTH)
        except BaseException:
            document.close()
            raise
        pages = f"{document.page_count} page{'s' * (document.page_count != 1)}"
        info = " · ".join(p for p in (pages, fs.human_size(src.size), document.title) if p)
        return Loaded(PdfData(document, first), info, close=document.close)

    def create_view(self, ctx: ViewContext) -> PreviewView:
        return PdfView()


class _Page(QLabel):
    def __init__(self) -> None:
        super().__init__()
        self.setObjectName("previewImage")
        self.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self.setSizePolicy(QSizePolicy.Policy.Ignored, QSizePolicy.Policy.Ignored)


class PdfView(PreviewView):
    """One page at a time, fitted to the pane.

    ←/→, PgUp/PgDn, Space and the scroll wheel turn pages; Home/End jump.
    """

    def __init__(self) -> None:
        super().__init__()
        self.setObjectName("previewPage")
        self.setFocusPolicy(Qt.FocusPolicy.StrongFocus)
        self._doc: PdfDocument | None = None
        self._index = 0
        self._shown: QImage | None = None
        self._cache: OrderedDict[tuple[int, int], QImage] = OrderedDict()
        self._generation = 0  # invalidates renders of a previous document
        self._wheel_accum = 0
        self._wheel_paused_until = 0.0

        self.page = _Page()
        self.prev_button = self._button("◀", lambda: self.go(self._index - 1))
        self.next_button = self._button("▶", lambda: self.go(self._index + 1))
        self.counter = QLabel("")
        self.counter.setObjectName("muted")
        controls = QWidget()
        controls.setObjectName("mediaControls")
        row = QHBoxLayout(controls)
        row.setContentsMargins(0, 4, 0, 0)
        row.addStretch(1)
        row.addWidget(self.prev_button)
        row.addWidget(self.counter)
        row.addWidget(self.next_button)
        row.addStretch(1)
        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.addWidget(self.page, 1)
        layout.addWidget(controls)

        self._resize_timer = QTimer(self, singleShot=True, interval=150)
        self._resize_timer.timeout.connect(self._request_render)

    def _button(self, text: str, slot) -> QToolButton:
        b = QToolButton()
        b.setText(text)
        b.setObjectName("fkey")
        b.setFocusPolicy(Qt.FocusPolicy.NoFocus)
        b.clicked.connect(slot)
        return b

    # -- PreviewView ---------------------------------------------------------------

    def show_data(self, data: PdfData) -> None:
        self.clear()
        self._doc = data.document
        self._index = 0
        self._show(data.first_page)
        self._update_controls()
        self._request_render()

    def clear(self) -> None:
        self._generation += 1
        self._resize_timer.stop()
        self._cache.clear()
        self._shown = None
        self.page.clear()
        if self._doc is not None:
            doc, self._doc = self._doc, None
            tasks.run_in_background(doc.close, on_error=lambda exc: None)  # may wait for a render

    # -- navigation -----------------------------------------------------------------

    @property
    def page_index(self) -> int:
        return self._index

    @property
    def page_count(self) -> int:
        return self._doc.page_count if self._doc else 0

    def go(self, index: int) -> None:
        if self._doc is None:
            return
        index = max(0, min(index, self._doc.page_count - 1))
        if index == self._index:
            return
        self._index = index
        self._update_controls()
        cached = self._cache.get((index, self._target_width()))
        if cached is not None:
            self._show(cached)
        self._request_render()

    def _update_controls(self) -> None:
        count = self.page_count
        self.counter.setText(f"{self._index + 1} / {count}" if count else "")
        self.prev_button.setEnabled(self._index > 0)
        self.next_button.setEnabled(self._index < count - 1)

    def keyPressEvent(self, event: QKeyEvent) -> None:
        match event.key():
            case Qt.Key.Key_Right | Qt.Key.Key_PageDown | Qt.Key.Key_Space | Qt.Key.Key_Down:
                self.go(self._index + 1)
            case Qt.Key.Key_Left | Qt.Key.Key_PageUp | Qt.Key.Key_Backspace | Qt.Key.Key_Up:
                self.go(self._index - 1)
            case Qt.Key.Key_Home:
                self.go(0)
            case Qt.Key.Key_End:
                self.go(self.page_count - 1)
            case _:
                super().keyPressEvent(event)

    def wheelEvent(self, event: QWheelEvent) -> None:
        """One page per deliberate scroll step.

        Trackpads and smooth-scrolling mice send many small events (and keep
        sending "momentum" events after the fingers lift); they are summed up
        to one wheel notch, momentum is ignored, and after a page turn the rest
        of the same flick is ignored for a moment.
        """
        event.accept()
        if event.phase() in (Qt.ScrollPhase.ScrollMomentum, Qt.ScrollPhase.ScrollBegin):
            self._wheel_accum = 0
            return
        now = time.monotonic()
        if now < self._wheel_paused_until:
            self._wheel_accum = 0
            return
        self._wheel_accum += event.angleDelta().y()
        if abs(self._wheel_accum) >= WHEEL_STEP:
            self.go(self._index + (1 if self._wheel_accum < 0 else -1))
            self._wheel_accum = 0
            self._wheel_paused_until = now + WHEEL_PAUSE_S

    # -- rendering ------------------------------------------------------------------

    def _target_width(self) -> int:
        """Pixel width that fits the page in the pane (device pixels, for crisp text)."""
        doc, area = self._doc, self.page.size()
        if doc is None or area.width() <= 1 or area.height() <= 1:
            return FIRST_PAGE_WIDTH
        try:
            w, h = doc.page_size(self._index)
        except CannotPreview:
            return FIRST_PAGE_WIDTH
        fit = min(area.width() / w, area.height() / h)
        return max(16, int(w * fit * self.devicePixelRatioF()))

    def _request_render(self) -> None:
        doc = self._doc
        if doc is None:
            return
        index, width, generation = self._index, self._target_width(), self._generation
        key = (index, width)
        if key in self._cache:
            self._show(self._cache[key])
            self._prefetch(index + 1, width, generation)
            return

        def done(image: QImage) -> None:
            self._remember(key, image)
            if generation == self._generation and index == self._index:
                self._show(image)
                self._prefetch(index + 1, width, generation)

        tasks.run_in_background(lambda: doc.render(index, width), done, lambda exc: None, owner=self)

    def _prefetch(self, index: int, width: int, generation: int) -> None:
        doc = self._doc
        if doc is None or index >= doc.page_count or (index, width) in self._cache:
            return
        tasks.run_in_background(
            lambda: doc.render(index, width),
            lambda image: generation == self._generation and self._remember((index, width), image),
            lambda exc: None,
            owner=self,
        )

    def _remember(self, key: tuple[int, int], image: QImage) -> None:
        self._cache[key] = image
        self._cache.move_to_end(key)
        while len(self._cache) > CACHE_PAGES:
            self._cache.popitem(last=False)

    def _show(self, image: QImage) -> None:
        self._shown = image
        dpr = self.devicePixelRatioF()
        logical = image.size() / dpr
        area = self.page.size()
        if logical.width() > area.width() or logical.height() > area.height():  # until the exact render lands
            image = image.scaled(area * dpr, Qt.AspectRatioMode.KeepAspectRatio,
                                 Qt.TransformationMode.SmoothTransformation)
        pixmap = QPixmap.fromImage(image)
        pixmap.setDevicePixelRatio(dpr)
        self.page.setPixmap(pixmap)

    def resizeEvent(self, event: QResizeEvent) -> None:
        super().resizeEvent(event)
        if self._shown is not None:
            self._show(self._shown)
        self._resize_timer.start()

