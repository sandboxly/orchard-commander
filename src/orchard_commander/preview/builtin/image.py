"""Images in every format Qt can decode; animated GIF/WebP play. Right-click: open
with the default app, or copy the image."""

from __future__ import annotations

import functools
from dataclasses import dataclass
from pathlib import Path

from PySide6.QtCore import QSize, Qt
from PySide6.QtGui import QContextMenuEvent, QGuiApplication, QImage, QImageReader, QMovie, QPixmap, QResizeEvent
from PySide6.QtWidgets import QLabel, QMenu, QSizePolicy, QVBoxLayout

from orchard_commander import fs
from orchard_commander.preview import content
from orchard_commander.preview.api import (
    Access,
    CannotPreview,
    FileInfo,
    LoadContext,
    Loaded,
    Previewer,
    PreviewView,
    Score,
    ViewContext,
)
from orchard_commander.preview.source import PreviewSource

MAX_IMAGE_DIMENSION = 4096  # decode huge images scaled down
MAX_FETCH_BYTES = 512 * 1024**2  # images without a direct path are fetched whole


@functools.cache
def supported_extensions() -> frozenset[str]:
    """Every image format this Qt build decodes (plugins included, e.g. HEIC on macOS)."""
    formats = {bytes(f).decode().lower() for f in QImageReader.supportedImageFormats()}
    return frozenset(formats or content.IMAGE_EXTS)


@dataclass(slots=True)
class ImageData:
    path: Path
    image: QImage | None  # None: animated, played from `path`


class ImagePreviewer(Previewer):
    id = "image"
    title = "Image"
    access = Access.LOCAL_FILE
    max_size = MAX_FETCH_BYTES

    def score(self, f: FileInfo) -> int:
        return Score.FORMAT if not f.is_dir and f.ext in supported_extensions() else Score.NO

    def load(self, src: PreviewSource, ctx: LoadContext) -> Loaded:
        path = src.local_file(self.max_size)
        reader = QImageReader(str(path))
        reader.setAutoTransform(True)  # honor EXIF orientation
        size = reader.size()
        fmt = bytes(reader.format()).decode().upper()
        dims = f"{size.width()}×{size.height()}" if size.isValid() else ""
        info = " · ".join(p for p in (dims, fmt, fs.human_size(src.size)) if p)
        if reader.supportsAnimation() and reader.imageCount() != 1:
            return Loaded(ImageData(path, None), info + " · animated")
        if size.isValid() and max(size.width(), size.height()) > MAX_IMAGE_DIMENSION:
            reader.setScaledSize(size.scaled(MAX_IMAGE_DIMENSION, MAX_IMAGE_DIMENSION, Qt.AspectRatioMode.KeepAspectRatio))
        image = reader.read()
        if image.isNull():
            raise CannotPreview(f"Not a readable image: {reader.errorString()}")
        return Loaded(ImageData(path, image), info)

    def create_view(self, ctx: ViewContext) -> PreviewView:
        return ImageView()


class _ScaledLabel(QLabel):
    """Shows an image scaled to fit (never enlarged), or an animation."""

    def __init__(self) -> None:
        super().__init__()
        self.setObjectName("previewImage")
        self.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self.setSizePolicy(QSizePolicy.Policy.Ignored, QSizePolicy.Policy.Ignored)
        self._image: QImage | None = None
        self._movie: QMovie | None = None
        self._movie_size = QSize()

    def show_image(self, image: QImage) -> None:
        self.clear_media()
        self._image = image
        self._rescale()

    def show_animation(self, path: Path) -> None:
        self.clear_media()
        self._movie = QMovie(str(path), parent=self)
        self._movie_size = QImageReader(str(path)).size()
        self.setMovie(self._movie)
        self._rescale()
        self._movie.start()

    def clear_media(self) -> None:
        if self._movie is not None:
            self._movie.stop()
            self._movie.deleteLater()
            self._movie = None
        self._image = None
        self.clear()

    def resizeEvent(self, event: QResizeEvent) -> None:
        super().resizeEvent(event)
        self._rescale()

    def _fit(self, size: QSize) -> QSize:
        if size.width() <= self.width() and size.height() <= self.height():
            return size
        return size.scaled(self.size(), Qt.AspectRatioMode.KeepAspectRatio)

    def _rescale(self) -> None:
        if self._movie is not None and self._movie_size.isValid():
            self._movie.setScaledSize(self._fit(self._movie_size))
        elif self._image is not None:
            dpr = self.devicePixelRatioF()
            target = self._fit(self._image.size() / dpr if dpr > 1 else self._image.size())
            pixmap = QPixmap.fromImage(
                self._image.scaled(
                    target * dpr, Qt.AspectRatioMode.KeepAspectRatio, Qt.TransformationMode.SmoothTransformation
                )
            )
            pixmap.setDevicePixelRatio(dpr)
            self.setPixmap(pixmap)


class ImageView(PreviewView):
    def __init__(self) -> None:
        super().__init__()
        self.label = _ScaledLabel()
        self.data: ImageData | None = None
        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.addWidget(self.label)

    def show_data(self, data: ImageData) -> None:
        self.data = data
        if data.image is None:
            self.label.show_animation(data.path)
        else:
            self.label.show_image(data.image)

    def clear(self) -> None:
        self.data = None
        self.label.clear_media()

    # -- context menu ------------------------------------------------------

    def context_menu(self) -> QMenu:
        menu = QMenu(self)
        menu.addAction("Open", self.open_file).setEnabled(self.data is not None)
        menu.addAction("Copy", self.copy_image).setEnabled(self.data is not None)
        return menu

    def contextMenuEvent(self, event: QContextMenuEvent) -> None:
        self.context_menu().exec(event.globalPos())

    def open_file(self) -> None:
        """Open with the system's default app (a server's file: the downloaded copy)."""
        if self.data is None:
            return
        try:
            fs.open_with_default_app(self.data.path)
        except OSError as exc:
            self.notice.emit(f"Cannot open {self.data.path.name}: {exc}", "error")

    def copy_image(self) -> None:
        """Copy the image at full size (the preview may be scaled down); animations: the first frame."""
        if self.data is None:
            return
        reader = QImageReader(str(self.data.path))
        reader.setAutoTransform(True)
        image = reader.read()
        if image.isNull():
            image = self.data.image or QImage()
        if image.isNull():
            self.notice.emit(f"Cannot copy {self.data.path.name}: {reader.errorString()}", "error")
            return
        QGuiApplication.clipboard().setImage(image)
        self.notice.emit(f"Copied {self.data.path.name} ({image.width()}×{image.height()})", "info")
