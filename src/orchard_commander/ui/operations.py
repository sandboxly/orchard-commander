"""Run copy/move/trash on a worker thread, reporting back via Qt signals."""

from __future__ import annotations

import threading
import time
from pathlib import Path

from PySide6.QtCore import QThread, Signal

from orchard_commander import fs, vfs

PROGRESS_INTERVAL = 0.05  # seconds between progress signals


class FileOperation(QThread):
    """One copy/move/trash job between any file systems (see `vfs`).

    `conflict` is emitted when a destination exists; the receiver (on the GUI
    thread) must call `answer_conflict`. The worker blocks until it does.
    """

    progress = Signal(object)  # fs.Progress
    conflict = Signal(object, object)  # src Path, dst Path (for display)
    succeeded = Signal(str)
    failed = Signal(str)
    cancelled = Signal(str)

    def __init__(self, kind: str, sources: list[vfs.VPath], target: vfs.VPath | None = None) -> None:
        super().__init__()
        self.kind = kind
        self.sources = sources
        self.target = target
        self.cancel_requested = False
        self._answer: tuple[fs.Conflict, bool] | None = None
        self._answered = threading.Event()
        self._sticky: fs.Conflict | None = None
        self._last_progress = 0.0

    @property
    def title(self) -> str:
        if self.kind == "copy" and any(not s.is_local for s in self.sources):
            return "Extracting"
        return {"copy": "Copying", "move": "Moving", "trash": "Moving to trash", "delete": "Deleting"}[self.kind]

    def answer_conflict(self, choice: fs.Conflict, apply_to_all: bool) -> None:
        self._answer = (choice, apply_to_all)
        self._answered.set()

    def _on_conflict(self, src: Path, dst: Path) -> fs.Conflict:
        if self._sticky is not None:
            return self._sticky
        self._answered.clear()
        self.conflict.emit(src, dst)
        self._answered.wait()
        assert self._answer is not None
        choice, apply_all = self._answer
        if apply_all:
            self._sticky = choice
        return choice

    def _on_progress(self, p: fs.Progress) -> None:
        now = time.monotonic()
        if now - self._last_progress >= PROGRESS_INTERVAL or p.files_done == p.files_total:
            self._last_progress = now
            self.progress.emit(fs.Progress(p.current, p.files_done, p.files_total, p.bytes_done, p.bytes_total))

    def run(self) -> None:
        try:
            if self.kind == "trash":
                vfs.trash(self.sources)
            elif self.kind == "delete":
                vfs.delete(self.sources)
            else:
                assert self.target is not None
                vfs.copy(
                    self.sources,
                    self.target,
                    move=self.kind == "move",
                    on_conflict=self._on_conflict,
                    on_progress=self._on_progress,
                    is_cancelled=lambda: self.cancel_requested,
                )
        except fs.OperationCancelled:
            self.cancelled.emit(f"{self.title} cancelled")
        except Exception as exc:  # OSError, OperationError, send2trash's platform errors
            self.failed.emit(str(exc))
        else:
            self.succeeded.emit(f"{self.title} finished")
