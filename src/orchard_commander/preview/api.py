"""The previewer plugin interface.

To support a new file format, subclass `Previewer`:

    class PdfPreviewer(Previewer):
        id = "pdf"
        title = "PDF"
        access = Access.LOCAL_FILE
        max_size = 200 * 1024**2          # don't download bigger PDFs just to preview them

        def score(self, f: FileInfo) -> int:
            return Score.FORMAT if f.ext == "pdf" else Score.NO

        def load(self, src: PreviewSource, ctx: LoadContext) -> Loaded:   # worker thread
            pages = render_pages(src.local_file())
            return Loaded(pages, info=f"{len(pages)} pages")

        def create_view(self, ctx: ViewContext) -> PreviewView:           # GUI thread, once
            return PdfView(ctx)

and register it (built-ins in `builtin/__init__.py`, add-ons via the
`orchard_commander.previewers` entry point).

Threading: `score()` and `load()` run on a worker thread and must not touch
widgets (QImage and other plain data are fine). Views live on the GUI thread
and are reused for every file their previewer shows.
"""

from __future__ import annotations

import enum
from abc import ABC, abstractmethod
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, ClassVar

from PySide6.QtCore import Signal
from PySide6.QtWidgets import QWidget

if TYPE_CHECKING:
    from orchard_commander.preview.source import PreviewSource
    from orchard_commander.themes import ColorScheme
    from orchard_commander.ui.style import ThemeManager


class Access(enum.Enum):
    """What a previewer reads. `PreviewSource` gets it through the generic
    `vfs.FileSystem` interface, so previewers work on every file system,
    including ones added later."""

    METADATA = enum.auto()  # name/size/date, listings — no file contents
    HEAD = enum.auto()  # the first bytes (text, hex dumps)
    STREAM = enum.auto()  # a stream, ideally seekable (video, audio)
    LOCAL_FILE = enum.auto()  # a real file on disk; fetched/extracted if needed (images, documents, 3D)


class Score(enum.IntEnum):
    """Conventional scores; any int works, highest wins."""

    NO = 0
    FALLBACK = 1  # can show anything, e.g. a hex dump
    CONTENT = 50  # recognised from the contents (e.g. "looks like text")
    FORMAT = 100  # the file's format, by extension or signature
    SPECIFIC = 150  # a more specific variant of a format (e.g. Markdown among text)


@dataclass(frozen=True, slots=True)
class FileInfo:
    """What `score()` may look at. No I/O needed to compute a score."""

    name: str
    ext: str  # lower case, without the dot; "" if none
    size: int
    mtime: float
    is_dir: bool
    has_direct_path: bool  # the file system can hand out a real path without copying
    head: bytes | None = None  # first bytes, only if some previewer set `needs_head`

    @property
    def is_empty(self) -> bool:
        return not self.is_dir and self.size == 0


class CannotPreview(Exception):
    """Raised by `load()` when the file can't be shown after all (e.g. a corrupt
    image); the next best previewer is tried instead."""


class ShowMessage(Exception):
    """Raised by `load()` to show its text instead of a preview, without trying
    other previewers (e.g. "too large to fetch", "needs unrar")."""


class NeedsDownload(ShowMessage):
    """The file is an online-only cloud placeholder bigger than the automatic
    download limit; previewing it means downloading it first."""

    def __init__(self, size: int) -> None:
        from orchard_commander import fs

        super().__init__(
            f"This file is stored online only ({fs.human_size(size)}).\n\n"
            "Previewing it means downloading it from the cloud first."
        )
        self.size = size


class CloudNotDelivered(ShowMessage):
    """An online-only file didn't arrive in time; its download goes on in the background."""

    def __init__(self, name: str, waited: float) -> None:
        super().__init__(
            f"{name} hasn't arrived from the cloud yet (waited {waited:.0f} s).\n\n"
            "Check that the cloud app (Dropbox, iCloud Drive, OneDrive) is running, signed in and "
            "online. The download continues in the background."
        )


def fetch_timeout(size: int) -> float:
    """How long a preview waits for an online-only file: 20 s plus 1 s per MB."""
    return 20 + size / 1024**2


@dataclass(slots=True)
class Loaded:
    """The result of `load()`: data for the view, plus the status line text."""

    data: Any
    info: str = ""
    close: Callable[[], None] | None = None  # releases resources if the result is discarded unseen


AUTO_DOWNLOAD_LIMIT = 25 * 1024**2  # online-only cloud files up to this size are fetched for previews


@dataclass(frozen=True, slots=True)
class LoadContext:
    colors: ColorScheme  # the color scheme in use
    show_hidden: bool
    is_cancelled: Callable[[], bool] = lambda: False
    download_limit: int | None = AUTO_DOWNLOAD_LIMIT  # for online-only files; None: always download


class PreviewStore:
    """Small values a previewer keeps between files and app runs (e.g. "muted").

    Values must be JSON-serializable. The app stores them in its settings; this
    base class only keeps them in memory (tests, embedding).
    """

    def __init__(self) -> None:
        self._values: dict[str, Any] = {}

    def get(self, key: str, default: Any = None) -> Any:
        return self._values.get(key, default)

    def set(self, key: str, value: Any) -> None:
        self._values[key] = value


@dataclass(frozen=True, slots=True)
class ViewContext:
    themes: ThemeManager  # current theme, font, and the `changed` signal
    store: PreviewStore = field(default_factory=PreviewStore)  # remembered per previewer


class PreviewView(QWidget):
    """A previewer's widget. One instance per previewer, reused across files."""

    message = Signal(str)  # ask the pane to show a message instead (e.g. playback failed)
    notice = Signal(str, str)  # a passing notification (text, "info" | "warning" | "error"), e.g. "Copied"
    finished = Signal()  # playback reached the end; the host may move on to the next file

    def play(self) -> None:
        """Start playback (media views). The host calls this after `show_data()` when it moved
        on to this file automatically. Default: nothing to play."""

    def set_continuous(self, on: bool) -> None:
        """Whether the host plays the next file when this one ends (so e.g. don't loop)."""

    def store_changed(self) -> None:
        """Values in `ViewContext.store` were changed elsewhere (e.g. in Settings): show them."""

    @abstractmethod
    def show_data(self, data: Any) -> None:
        """Display what `load()` returned."""

    def clear(self) -> None:
        """Release what's shown (stop playback, free memory). Called before every
        switch to another file or previewer, and when the pane is hidden."""

    def focus_widget(self) -> QWidget:
        """The widget that should get keyboard focus when the user tabs to the preview."""
        return self


class Previewer(ABC):
    id: ClassVar[str]
    title: ClassVar[str]  # shown in the "View as" switcher
    access: ClassVar[Access] = Access.HEAD
    max_size: ClassVar[int | None] = None  # for LOCAL_FILE from other file systems: download limit
    needs_head: ClassVar[bool] = False  # whether score() wants FileInfo.head
    # Load again when the color scheme changes (load() used LoadContext.colors). Previewers whose
    # views restyle themselves (ViewContext.themes.changed) set False — e.g. so playback goes on.
    reload_on_theme_change: ClassVar[bool] = True

    @classmethod
    def available(cls) -> bool:
        """False if an optional dependency is missing; the previewer is then skipped."""
        return True

    @abstractmethod
    def score(self, f: FileInfo) -> int:
        """How well this previewer fits `f`: `Score.NO` (0) for not at all."""

    @abstractmethod
    def load(self, src: PreviewSource, ctx: LoadContext) -> Loaded:
        """Read what's needed and prepare data for the view. Worker thread."""

    @abstractmethod
    def create_view(self, ctx: ViewContext) -> PreviewView:
        """Create the view widget. GUI thread; called once per preview pane."""


@dataclass(slots=True)
class Candidate:
    previewer: Previewer
    score: int


@dataclass(slots=True)
class Outcome:
    """What the pane receives from a background preview job."""

    candidates: list[Candidate] = field(default_factory=list)
    previewer: Previewer | None = None
    loaded: Loaded | None = None
    error: BaseException | None = None
