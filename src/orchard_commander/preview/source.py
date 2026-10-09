"""Data access for previewers, through the generic file system interface.

Previewers never see which file system a file is on. They ask for bytes, a
stream or a path; the file system decides how to provide them (`open_read`,
`local_path`, `materialize` — e.g. reading a disk, extracting from an archive,
downloading from a server).
"""

from __future__ import annotations

import threading
from pathlib import Path
from typing import BinaryIO

from orchard_commander import fs, vfs
from orchard_commander.preview.api import FileInfo, ShowMessage

SNIFF_BYTES = 8192


class PreviewSource:
    """The file being previewed. Reads only what's asked for, and caches it.

    - `head(n)`        the first n bytes (one read, shared by all previewers)
    - `open_stream()`  a stream from the start (seekable on most file systems)
    - `local_file()`   a real path: the file itself, or a downloaded/extracted
                       copy in the temp cache, refused above `max_size`
    """

    def __init__(self, target: vfs.VPath) -> None:
        self.target = target
        self.entry = target.stat()
        self._head = b""
        self._head_complete = False  # whether _head holds the whole file
        self._lock = threading.Lock()

    @property
    def name(self) -> str:
        return self.target.name

    @property
    def size(self) -> int:
        return self.entry.size

    @property
    def direct_path(self) -> Path | None:
        """A real path to the file if the file system has one without copying."""
        return self.target.local_path()

    @property
    def location(self) -> str:
        parent = self.target.parent()
        return parent.display if parent is not None else ""

    def info(self, *, with_head: bool = False) -> FileInfo:
        name = self.target.name
        ext = name.rpartition(".")[2].lower() if "." in name.lstrip(".") else ""
        return FileInfo(
            name=name,
            ext=ext,
            size=self.entry.size,
            mtime=self.entry.mtime,
            is_dir=self.entry.is_dir,
            has_direct_path=self.direct_path is not None,
            head=self.head(SNIFF_BYTES) if with_head and not self.entry.is_dir else None,
        )

    def head(self, n: int) -> bytes:
        """The first `n` bytes (fewer if the file is shorter)."""
        with self._lock:
            if len(self._head) < n and not self._head_complete:
                with self.target.fs.open_read(self.target.path) as f:
                    have = len(self._head)
                    try:  # continue after what's cached instead of fetching it again
                        f.seek(have)
                        self._head += f.read(n - have)
                    except (OSError, ValueError, AttributeError):  # not seekable: read from the start
                        self._head = f.read(n)
                self._head_complete = len(self._head) < n
            return self._head[:n]

    def open_stream(self) -> BinaryIO:
        return self.target.fs.open_read(self.target.path)

    def local_file(self, max_size: int | None = None) -> Path:
        """A local path with the file's contents. Raises ShowMessage above `max_size`
        when the file would have to be fetched from elsewhere."""
        local = self.direct_path
        if local is not None:
            return local
        if max_size is not None and self.entry.size > max_size:
            raise ShowMessage(
                f"{self.name} is {fs.human_size(self.entry.size)}. Previewing it means fetching all of it from "
                f"{self.target.fs.type_name}; copy it with F5 instead."
            )
        return self.target.materialize()
