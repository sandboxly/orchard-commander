"""The preview pane: hosts previewer views (see `orchard_commander.preview`).

It knows nothing about file types. For each file it asks the registry (in the
background) which previewers fit, shows the best one's view, and offers the
others as "View as" choices.
"""

from __future__ import annotations

from PySide6.QtCore import Qt, QTimer
from PySide6.QtWidgets import (
    QHBoxLayout,
    QLabel,
    QProgressBar,
    QPushButton,
    QStackedWidget,
    QToolButton,
    QVBoxLayout,
    QWidget,
)

from orchard_commander import fs, vfs
from orchard_commander.preview import registry
from orchard_commander.preview.api import (
    AUTO_DOWNLOAD_LIMIT,
    CannotPreview,
    LoadContext,
    NeedsDownload,
    Outcome,
    Previewer,
    PreviewView,
    ShowMessage,
    ViewContext,
)
from orchard_commander.ui import tasks
from orchard_commander.ui.boxframe import BoxFrame
from orchard_commander.ui.style import ThemeManager

DEBOUNCE_MS = 120
SLOW_LOAD_MS = 300  # show a loading indicator if a preview from a server takes longer


class PreviewPane(BoxFrame):
    """Shows whatever `show_path()` is given, loading in the background."""

    def __init__(self, themes: ThemeManager) -> None:
        super().__init__(themes)
        self.show_hidden = False
        self.path: vfs.VPath | None = None
        self._pending: vfs.VPath | None = None
        self._request = 0
        self._views: dict[str, PreviewView] = {}
        self._current: Previewer | None = None
        self._choice: dict[str, str] = {}  # extension -> previewer id picked via "View as" (this session)
        self._forced: str | None = None  # previewer id requested for the next load
        self._download_ok: vfs.VPath | None = None  # online-only file the user chose to download

        # "View as" chips, shown when more than one previewer fits.
        self.chips = QWidget()
        self.chips.setObjectName("previewPage")
        self._chip_row = QHBoxLayout(self.chips)
        self._chip_row.setContentsMargins(0, 0, 0, 4)
        self.chips.hide()

        self.stack = QStackedWidget()
        self.message_label = QLabel()
        self.message_label.setObjectName("muted")
        self.message_label.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self.message_label.setWordWrap(True)
        self._message_page = QWidget()
        self._message_page.setObjectName("previewPage")
        self.download_button = QPushButton("Download and preview")
        self.download_button.clicked.connect(self.download_and_preview)
        self.download_button.hide()
        self.busy = QProgressBar()  # indeterminate: the OS doesn't report download progress
        self.busy.setRange(0, 0)
        self.busy.setTextVisible(False)
        self.busy.setFixedWidth(self.fontMetrics().horizontalAdvance("M") * 24)
        self.busy.hide()
        message_layout = QVBoxLayout(self._message_page)
        message_layout.addStretch(1)
        message_layout.addWidget(self.message_label)
        message_layout.addWidget(self.busy, 0, Qt.AlignmentFlag.AlignHCenter)
        message_layout.addWidget(self.download_button, 0, Qt.AlignmentFlag.AlignHCenter)
        message_layout.addStretch(1)
        self._slow_timer = QTimer(self, singleShot=True, interval=SLOW_LOAD_MS)
        self._slow_timer.timeout.connect(self._show_slow_loading)
        self.stack.addWidget(self._message_page)

        container = QWidget()
        container.setObjectName("previewPage")
        layout = QVBoxLayout(container)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(0)
        layout.addWidget(self.chips)
        layout.addWidget(self.stack, 1)
        self.body.addWidget(container)

        self._debounce = QTimer(self, singleShot=True, interval=DEBOUNCE_MS)
        self._debounce.timeout.connect(self._start_load)
        themes.changed.connect(self.refresh)
        self._show_message("No file selected")

    # -- public API --------------------------------------------------------

    @property
    def current_id(self) -> str | None:
        """The id of the previewer currently shown, if any."""
        return self._current.id if self._current is not None and self.stack.currentWidget() is not \
            self._message_page else None

    @property
    def current_view(self) -> PreviewView | None:
        widget = self.stack.currentWidget()
        return widget if isinstance(widget, PreviewView) else None

    def show_path(self, path: vfs.VPath | None) -> None:
        """Preview `path` (debounced, so fast cursor movement stays smooth)."""
        if path == self.path and self._pending is None:
            return
        self._pending = path
        self._debounce.start()

    def refresh(self) -> None:
        if self.path is not None and self.isVisible():
            self._pending, self.path = self.path, None
            self._forced = self._current.id if self._current else None
            self._start_load()

    def view_as(self, previewer_id: str) -> None:
        """Show the current file with a specific previewer (remembered per extension)."""
        if self.path is None:
            return
        self._choice[self._ext(self.path)] = previewer_id
        self._forced = previewer_id
        self._pending, self.path = self.path, None
        self._start_load()

    def download_and_preview(self) -> None:
        """Preview the current online-only file even though it means downloading it."""
        if self.path is None:
            return
        self._download_ok = self.path
        self._pending, self.path = self.path, None
        self._start_load()

    def cycle_view(self) -> None:
        """Switch to the next previewer that fits the current file."""
        ids = [b.property("previewer") for b in self._chip_buttons()]
        if len(ids) > 1 and self._current is not None and self._current.id in ids:
            self.view_as(ids[(ids.index(self._current.id) + 1) % len(ids)])

    def deactivate(self) -> None:
        """Called when the pane is hidden: stop playback and forget the file."""
        self._debounce.stop()
        self._slow_timer.stop()
        self._request += 1
        self._clear_view()
        self.path = self._pending = None
        self._current = None
        self._set_chips([], None)
        self.set_title("")
        self.set_subtitle("")

    # -- loading -----------------------------------------------------------

    @staticmethod
    def _ext(path: vfs.VPath) -> str:
        name = path.name
        return name.rpartition(".")[2].lower() if "." in name.lstrip(".") else ""

    def _start_load(self) -> None:
        path, self._pending = self._pending, None
        self._slow_timer.stop()
        self._request += 1
        request = self._request
        self._clear_view()
        self.path = path
        if path is None:
            self._current = None
            self._set_chips([], None)
            self.set_title("")
            self.set_subtitle("")
            self._show_message("No file selected")
            return
        self.set_title(path.name or path.display)
        self.set_subtitle("")
        cloud_size = self._online_only_size(path)
        if cloud_size is not None and (path == self._download_ok or cloud_size <= AUTO_DOWNLOAD_LIMIT):
            self._show_loading(f"Downloading {path.name} from the cloud…\n{fs.human_size(cloud_size)}")
        elif path.local_path() is None:
            self._slow_timer.start()  # servers: only if it takes a moment, to avoid flicker
        preferred = self._forced or self._choice.get(self._ext(path))
        self._forced = None
        ctx = LoadContext(
            theme=self.themes.theme,
            show_hidden=self.show_hidden,
            is_cancelled=lambda: request != self._request,
            download_limit=None if path == self._download_ok else AUTO_DOWNLOAD_LIMIT,
        )
        tasks.run_in_background(
            lambda: registry.preview(path, ctx, preferred),
            lambda outcome: self._loaded(request, outcome),
            lambda exc: self._loaded(request, Outcome(error=exc)),
            owner=self,
        )

    def _show_slow_loading(self) -> None:
        if self.path is not None:
            self._show_loading(f"Loading {self.path.name}…")

    def _loaded(self, request: int, outcome: Outcome) -> None:
        if request == self._request:
            self._slow_timer.stop()
        if request != self._request or self.path is None:
            if outcome.loaded is not None and outcome.loaded.close is not None:
                outcome.loaded.close()  # e.g. don't leak an open file on a server
            return  # the cursor moved on meanwhile
        self._current = outcome.previewer
        self._set_chips(outcome.candidates, outcome.previewer)
        if outcome.loaded is None or outcome.previewer is None:
            error = outcome.error
            if isinstance(error, NeedsDownload):
                self._show_message(str(error), download=True)
            elif isinstance(error, (ShowMessage, CannotPreview)):
                self._show_message(str(error))
            else:
                self._show_message(f"Cannot preview:\n{error}")
            self.set_subtitle("")
            return
        view = self._view_for(outcome.previewer)
        view.show_data(outcome.loaded.data)
        self.stack.setCurrentWidget(view)
        self.setFocusProxy(view.focus_widget())
        self.set_subtitle(outcome.loaded.info)

    # -- views ---------------------------------------------------------------

    def _view_for(self, previewer: Previewer) -> PreviewView:
        view = self._views.get(previewer.id)
        if view is None:
            view = previewer.create_view(ViewContext(self.themes))
            view.message.connect(self._view_message)
            self._views[previewer.id] = view
            self.stack.addWidget(view)
        return view

    def _view_message(self, text: str) -> None:
        if self.sender() is self.current_view:
            self._clear_view()
            self._show_message(text)

    def _clear_view(self) -> None:
        for view in self._views.values():
            view.clear()

    @staticmethod
    def _online_only_size(path: vfs.VPath) -> int | None:
        """Size of an online-only cloud file (reading it downloads it), else None."""
        local = path.local_path()
        if local is None:
            return None
        try:  # a local stat is cheap and doesn't download cloud placeholders
            entry = fs.stat_entry(local)
        except OSError:
            return None
        return entry.size if entry.is_online_only else None

    def _show_loading(self, text: str) -> None:
        self._clear_view()
        self._show_message(text, busy=True)

    def _show_message(self, text: str, *, download: bool = False, busy: bool = False) -> None:
        self.download_button.setVisible(download)
        self.busy.setVisible(busy)
        self.message_label.setText(text)
        self.stack.setCurrentWidget(self._message_page)
        self.setFocusProxy(None)

    # -- "View as" chips ---------------------------------------------------------

    def _chip_buttons(self) -> list[QToolButton]:
        return [self._chip_row.itemAt(i).widget() for i in range(self._chip_row.count())
                if isinstance(self._chip_row.itemAt(i).widget(), QToolButton)]

    def _set_chips(self, candidates, current: Previewer | None) -> None:
        while self._chip_row.count():
            item = self._chip_row.takeAt(0)
            if item.widget() is not None:
                item.widget().deleteLater()
        if len(candidates) < 2:
            self.chips.hide()
            return
        label = QLabel("View as")
        label.setObjectName("muted")
        self._chip_row.addWidget(label)
        for candidate in candidates:
            p = candidate.previewer
            b = QToolButton()
            b.setText(f" {p.title} ")
            b.setObjectName("modeTab")
            b.setCheckable(True)
            b.setChecked(current is not None and p.id == current.id)
            b.setFocusPolicy(Qt.FocusPolicy.NoFocus)
            b.setCursor(Qt.CursorShape.PointingHandCursor)
            b.setProperty("previewer", p.id)
            b.setToolTip("Shift+F3 cycles the views")
            b.clicked.connect(lambda _=False, pid=p.id: self.view_as(pid))
            self._chip_row.addWidget(b)
        self._chip_row.addStretch(1)
        self.chips.show()
