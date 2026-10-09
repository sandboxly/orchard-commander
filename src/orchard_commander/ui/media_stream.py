"""Feed media from any file system to QMediaPlayer without downloading it first."""

from __future__ import annotations

import threading
from typing import BinaryIO

from PySide6.QtCore import QIODevice

READ_AHEAD = 1024 * 1024  # bytes fetched per request; players ask for small pieces


class StreamDevice(QIODevice):
    """A random-access QIODevice over a seekable stream (e.g. an SMB or SFTP file).

    The player's demuxer reads from its own thread; reads are served from a
    read-ahead block so a 32 KB request doesn't cost a network round trip.
    """

    def __init__(self, stream: BinaryIO, size: int) -> None:
        super().__init__()
        self._stream = stream
        self._size = size
        self._lock = threading.Lock()
        self._block_start = 0
        self._block = b""
        self.open(QIODevice.OpenModeFlag.ReadOnly | QIODevice.OpenModeFlag.Unbuffered)

    def isSequential(self) -> bool:
        return False

    def size(self) -> int:
        return self._size

    def bytesAvailable(self) -> int:
        return max(0, self._size - self.pos()) + super().bytesAvailable()

    def readData(self, maxlen: int) -> bytes:
        pos = self.pos()
        if pos >= self._size or maxlen <= 0:
            return b""
        with self._lock:
            offset = pos - self._block_start
            if not 0 <= offset < len(self._block):
                try:
                    self._stream.seek(pos)
                    self._block = self._stream.read(max(READ_AHEAD, maxlen))
                except (OSError, ValueError):
                    return b""
                self._block_start, offset = pos, 0
            return self._block[offset : offset + maxlen]

    def writeData(self, data: bytes) -> int:
        return -1

    def close(self) -> None:
        with self._lock:
            try:
                self._stream.close()
            except Exception:
                pass
            self._block = b""
        super().close()
