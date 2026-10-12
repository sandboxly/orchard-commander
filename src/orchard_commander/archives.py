"""Read-only access to archives as if they were directories.

Supported: zip, tar (plain/gz/bz2/xz), single-file gzip/bzip2/xz, 7z (py7zr)
and rar (rarfile; listing is pure Python, decompression uses whichever of
unrar / unar / 7z / bsdtar is installed — bsdtar ships with macOS and, as
tar.exe, with Windows 10+).

Inside an archive, paths are POSIX strings relative to the archive root, with
"" meaning the root. Archive objects are shared between the UI and worker
threads; member listings are immutable after opening and all reads take the
archive's lock. The rest of the app reaches archives through `vfs.ArchiveFS`.
"""

from __future__ import annotations

import atexit
import bz2
import datetime as dt
import gzip
import hashlib
import io
import lzma
import os
import posixpath
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import zipfile
from abc import ABC, abstractmethod
from collections.abc import Callable, Iterator
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO

from orchard_commander import fs

CHUNK = 1024 * 1024

# Longest suffixes first so "x.tar.gz" is a tarball, not a gzip file.
SUFFIXES: list[tuple[str, str]] = [
    (".tar.gz", "tar"), (".tar.bz2", "tar"), (".tar.xz", "tar"), (".tgz", "tar"), (".tbz", "tar"),
    (".tbz2", "tar"), (".txz", "tar"), (".tar", "tar"),
    (".zip", "zip"), (".7z", "7z"), (".rar", "rar"),
    (".gz", "gz"), (".bz2", "bz2"), (".xz", "xz"), (".lzma", "xz"),
]


class ArchiveError(Exception):
    pass


def archive_type(name: str) -> str | None:
    lower = name.lower()
    for suffix, kind in SUFFIXES:
        if lower.endswith(suffix) and len(lower) > len(suffix):
            return kind
    return None


def is_archive(name: str | Path) -> bool:
    return archive_type(str(name)) is not None


@dataclass(frozen=True, slots=True)
class Member:
    name: str  # normalized POSIX path, no leading/trailing slash
    is_dir: bool
    size: int
    mtime: float
    is_link: bool = False


def _normalize(name: str) -> str | None:
    """Clean a member name; None for names that would escape the archive root."""
    name = name.replace("\\", "/")
    parts = [p for p in name.split("/") if p not in ("", ".")]
    if any(p == ".." for p in parts):
        return None
    return "/".join(parts)


class Archive(ABC):
    """An opened archive with an index of its members."""

    type_name = "archive"

    def __init__(self, path: Path) -> None:
        self.path = path
        self._lock = threading.RLock()
        self.members: dict[str, Member] = {}
        self.children: dict[str, list[str]] = {"": []}
        try:
            raw = list(self._read_members())
        except ArchiveError:
            raise
        except Exception as exc:  # corrupt/encrypted/unsupported: one error type for the UI
            raise ArchiveError(f"Cannot read {path.name}: {exc}") from exc
        self._index(raw)

    # -- subclass API ---------------------------------------------------------

    @abstractmethod
    def _read_members(self) -> Iterator[Member]: ...

    @abstractmethod
    def _open(self, name: str) -> BinaryIO:
        """Stream a file member's contents. Called with the lock held."""

    def _close_handle(self) -> None:
        """Release the open file handle (if the subclass keeps one)."""

    def _reopen_handle(self) -> None:
        """Open the handle again after `_close_handle()`."""

    _closed = False

    def close(self, *, wait: bool = True) -> bool:
        """Release the file handle; the archive stays usable and reopens it when read.

        With wait=False, an archive that is being read right now is left open
        (returns False) instead of blocking until the reader finishes.
        """
        if not self._lock.acquire(blocking=wait):
            return False
        try:
            if not self._closed:
                self._close_handle()
                self._closed = True
            return True
        finally:
            self._lock.release()

    def _ensure_open(self) -> None:
        """Called with the lock held, before touching the handle."""
        if self._closed:
            self._reopen_handle()
            self._closed = False

    # -- index ----------------------------------------------------------------

    def _index(self, raw: list[Member]) -> None:
        mtime = self.path.stat().st_mtime
        for m in raw:
            self._add(m)
            parent = posixpath.dirname(m.name)
            while parent and parent not in self.members:  # implicit directories
                self._add(Member(parent, True, 0, mtime))
                parent = posixpath.dirname(parent)

    def _add(self, m: Member) -> None:
        if not m.name or (m.name in self.members and not m.is_dir):
            return
        existed = m.name in self.members
        self.members[m.name] = m
        if not existed:
            self.children.setdefault(posixpath.dirname(m.name), []).append(m.name)
        if m.is_dir:
            self.children.setdefault(m.name, [])

    def is_dir(self, inner: str) -> bool:
        return inner == "" or (inner in self.members and self.members[inner].is_dir)

    def entry(self, m: Member) -> fs.Entry:
        name = posixpath.basename(m.name)
        return fs.Entry(
            name=name,
            is_dir=m.is_dir,
            is_link=m.is_link,
            is_hidden=name.startswith("."),
            is_executable=False,
            size=m.size,
            mtime=m.mtime,
        )

    def list_dir(
        self, inner: str, *, show_hidden: bool = False, sort_key: fs.SortKey = fs.SortKey.NAME, reverse: bool = False
    ) -> list[fs.Entry]:
        if not self.is_dir(inner):
            raise ArchiveError(f"Not a directory in {self.path.name}: {inner}")
        entries = [self.entry(self.members[n]) for n in self.children.get(inner, [])]
        return fs.sort_entries(entries, show_hidden=show_hidden, sort_key=sort_key, reverse=reverse)

    def walk(self, inner: str) -> Iterator[Member]:
        """`inner` itself and everything below it, parents before children."""
        if inner:
            yield self.members[inner]
        for child in self.children.get(inner, []):
            m = self.members[child]
            if m.is_dir:
                yield from self.walk(child)
            else:
                yield m

    # -- reading --------------------------------------------------------------

    def open(self, name: str) -> BinaryIO:
        """Stream file member `name`. Holds the archive's lock until closed."""
        self._lock.acquire()
        try:
            self._ensure_open()
            stream = self._open(name)
        except BaseException:
            self._lock.release()
            raise
        return _LockedReader(stream, self._lock)  # type: ignore[return-value]

    def extract_file(self, name: str, dst: Path, on_chunk: Callable[[int], None] | None = None) -> None:
        """Write file member `name` to `dst`. `on_chunk` may raise to abort."""
        tmp = dst.with_name(dst.name + ".part")
        try:
            with self._lock:
                self._ensure_open()
            with self._lock, self._open(name) as src, open(tmp, "wb") as out:
                while chunk := src.read(CHUNK):
                    out.write(chunk)
                    if on_chunk:
                        on_chunk(len(chunk))
            os.replace(tmp, dst)
        except BaseException:
            tmp.unlink(missing_ok=True)
            raise
        mtime = self.members[name].mtime
        if mtime:
            os.utime(dst, (mtime, mtime))


class _LockedReader(io.RawIOBase):
    def __init__(self, f: BinaryIO, lock: threading.RLock) -> None:
        super().__init__()
        self._f = f
        self._lock: threading.RLock | None = lock

    def readable(self) -> bool:
        return True

    def read(self, size: int = -1) -> bytes:
        return self._f.read(size)

    def close(self) -> None:
        if self._lock is not None:
            try:
                self._f.close()
            finally:
                self._lock.release()
                self._lock = None
        super().close()


class ZipArchive(Archive):
    type_name = "zip"

    def _read_members(self) -> Iterator[Member]:
        self._zip = zipfile.ZipFile(self.path)
        self._infos: dict[str, zipfile.ZipInfo] = {}
        for info in self._zip.infolist():
            name = _normalize(info.filename)
            if not name:
                continue
            self._infos[name] = info
            try:
                mtime = time.mktime((*info.date_time, 0, 0, -1))
            except (OverflowError, ValueError):
                mtime = 0.0
            yield Member(name, info.is_dir(), info.file_size, mtime)

    def _open(self, name: str) -> BinaryIO:
        info = self._infos[name]
        if info.flag_bits & 0x1:
            raise ArchiveError("Encrypted zip entries are not supported yet")
        return self._zip.open(info)  # type: ignore[return-value]

    def _close_handle(self) -> None:
        self._zip.close()

    def _reopen_handle(self) -> None:
        self._zip = zipfile.ZipFile(self.path)


class TarArchive(Archive):
    type_name = "tar"

    def _read_members(self) -> Iterator[Member]:
        self._tar = tarfile.open(self.path, "r:*")
        self._infos: dict[str, tarfile.TarInfo] = {}
        for info in self._tar.getmembers():
            name = _normalize(info.name)
            if not name:
                continue
            self._infos[name] = info
            yield Member(name, info.isdir(), info.size if info.isreg() else 0, float(info.mtime), info.issym())

    def _open(self, name: str) -> BinaryIO:
        f = self._tar.extractfile(self._infos[name])
        return f if f is not None else io.BytesIO()  # type: ignore[return-value]

    def _close_handle(self) -> None:
        self._tar.close()

    def _reopen_handle(self) -> None:
        self._tar = tarfile.open(self.path, "r:*")


class CompressedFile(Archive):
    """A single compressed file (foo.txt.gz), shown as a folder holding foo.txt."""

    OPENERS: dict[str, Callable[[Path], BinaryIO]] = {
        "gz": lambda p: gzip.open(p, "rb"),  # type: ignore[dict-item]
        "bz2": lambda p: bz2.open(p, "rb"),  # type: ignore[dict-item]
        "xz": lambda p: lzma.open(p, "rb"),  # type: ignore[dict-item]
    }

    def __init__(self, path: Path, kind: str) -> None:
        self.kind = kind
        self.type_name = kind
        super().__init__(path)

    def _read_members(self) -> Iterator[Member]:
        stem = self.path.name.rsplit(".", 1)[0] or "data"
        size = 0
        if self.kind == "gz":  # gzip stores the size (mod 4 GiB) in its trailer
            with open(self.path, "rb") as f:
                f.seek(-4, os.SEEK_END)
                size = struct.unpack("<I", f.read(4))[0]
        with self.OPENERS[self.kind](self.path) as f:  # validate the format up front
            f.read(1)
        yield Member(stem, False, size, self.path.stat().st_mtime)

    def _open(self, name: str) -> BinaryIO:
        return self.OPENERS[self.kind](self.path)


class SevenZipArchive(Archive):
    type_name = "7z"

    def _read_members(self) -> Iterator[Member]:
        import py7zr

        self._sz = py7zr.SevenZipFile(self.path, "r")
        if self._sz.needs_password():
            raise ArchiveError("Encrypted 7z archives are not supported yet")
        for info in self._sz.list():
            name = _normalize(info.filename)
            if not name:
                continue
            when = info.creationtime
            mtime = when.timestamp() if isinstance(when, dt.datetime) else 0.0
            size = info.uncompressed or 0
            yield Member(name, info.is_directory, 0 if info.is_directory else size, mtime)

    def _extract_to_dir(self, name: str, tmpdir: Path) -> Path:
        self._sz.reset()
        self._sz.extract(path=tmpdir, targets=[name])
        out = tmpdir / name
        if not out.exists():
            raise ArchiveError(f"Could not extract {name}")
        return out

    def _open(self, name: str) -> BinaryIO:
        # py7zr can't stream a single member; extract to a temp dir and read that.
        tmpdir = Path(tempfile.mkdtemp(prefix="7z-", dir=cache_dir()))
        f = open(self._extract_to_dir(name, tmpdir), "rb")
        return _DeletingReader(f, tmpdir)  # type: ignore[return-value]

    def _close_handle(self) -> None:
        self._sz.close()

    def _reopen_handle(self) -> None:
        import py7zr

        self._sz = py7zr.SevenZipFile(self.path, "r")


class _DeletingReader(io.RawIOBase):
    """Reads a temp file and removes its directory when closed."""

    def __init__(self, f: BinaryIO, tmpdir: Path) -> None:
        super().__init__()
        self._f = f
        self._tmpdir = tmpdir

    def readable(self) -> bool:
        return True

    def read(self, size: int = -1) -> bytes:
        return self._f.read(size)

    def close(self) -> None:
        self._f.close()
        shutil.rmtree(self._tmpdir, ignore_errors=True)
        super().close()


class RarArchive(Archive):
    """Listing via rarfile (pure Python); decompression via an external tool."""

    type_name = "rar"

    def _read_members(self) -> Iterator[Member]:
        import rarfile

        self._rar = rarfile.RarFile(self.path)
        if self._rar.needs_password():
            raise ArchiveError("Encrypted rar archives are not supported yet")
        self._names: dict[str, str] = {}  # normalized -> name as stored
        for info in self._rar.infolist():
            name = _normalize(info.filename)
            if not name:
                continue
            self._names[name] = info.filename
            mtime = 0.0
            if isinstance(getattr(info, "mtime", None), dt.datetime):  # RAR5: precise, tz-aware
                mtime = info.mtime.timestamp()
            elif info.date_time:  # RAR4: local time tuple
                try:
                    mtime = time.mktime((*info.date_time, 0, 0, -1))
                except (OverflowError, ValueError, TypeError):
                    pass
            yield Member(name, info.is_dir(), info.file_size, mtime, info.is_symlink())

    def _open(self, name: str) -> BinaryIO:
        return _ToolReader(rar_command(self.path, self._names[name]))  # type: ignore[return-value]

    def _close_handle(self) -> None:
        self._rar.close()

    def _reopen_handle(self) -> None:
        import rarfile

        self._rar = rarfile.RarFile(self.path)


RAR_TOOL_HINT = "Install unrar or 7-Zip to extract this RAR archive"


_rar_support: dict[str, bool] = {}


def _seven_zip_reads_rar(exe: str) -> bool:
    """Some 7-Zip builds leave out RAR (p7zip, Homebrew's sevenzip): ask its format list, once."""
    if exe not in _rar_support:
        try:
            out = subprocess.run([exe, "i"], capture_output=True, text=True, timeout=10).stdout
            _rar_support[exe] = any(line.split()[1:2] == ["Rar5"] or " Rar5 " in line for line in out.splitlines())
        except (OSError, subprocess.SubprocessError):
            _rar_support[exe] = False
    return _rar_support[exe]


def rar_command(archive: Path, member: str) -> list[str]:
    """A command that writes `member` to stdout, using the best available tool."""
    member = member.replace("\\", "/")
    if exe := shutil.which("unrar"):
        return [exe, "p", "-inul", "-y", "--", str(archive), member]
    for name in ("7zz", "7z", "7za"):
        if (exe := shutil.which(name)) and _seven_zip_reads_rar(exe):
            return [exe, "x", "-so", "-bd", "-y", "--", str(archive), member]
    # bsdtar ships with macOS, and with Windows 10+ as tar.exe.
    exe = shutil.which("bsdtar") or (shutil.which("tar") if sys.platform == "win32" else None)
    if exe:
        return [exe, "-x", "-O", "-f", str(archive), member]
    raise ArchiveError(RAR_TOOL_HINT)


class _ToolReader(io.RawIOBase):
    """Streams an external extractor's stdout; raises ArchiveError if it fails."""

    def __init__(self, cmd: list[str]) -> None:
        super().__init__()
        flags = subprocess.CREATE_NO_WINDOW if sys.platform == "win32" else 0  # type: ignore[attr-defined]
        self._proc = subprocess.Popen(
            cmd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, creationflags=flags
        )
        self._tool = Path(cmd[0]).stem

    def readable(self) -> bool:
        return True

    def read(self, size: int = -1) -> bytes:
        assert self._proc.stdout is not None
        data = self._proc.stdout.read(size) if size >= 0 else self._proc.stdout.read()
        if not data:
            self._check()
        return data

    def _check(self) -> None:
        assert self._proc.stderr is not None
        if self._proc.wait() != 0:
            err = self._proc.stderr.read().decode(errors="replace").strip().splitlines()
            detail = err[0].removeprefix(f"{self._tool}: ").rstrip(".") if err else f"exit code {self._proc.returncode}"
            raise ArchiveError(f"{detail}. {RAR_TOOL_HINT}")

    def close(self) -> None:
        if self._proc.poll() is None:
            self._proc.kill()
        self._proc.wait()
        for stream in (self._proc.stdout, self._proc.stderr):
            if stream:
                stream.close()
        super().close()


# ---------------------------------------------------------------------------
# Opening with a small cache
# ---------------------------------------------------------------------------

_open_lock = threading.Lock()
_open_cache: dict[tuple[str, float, int], Archive] = {}
_OPEN_CACHE_SIZE = 8


def open_archive(path: Path) -> Archive:
    """Open (or reuse an already opened) archive. Raises ArchiveError."""
    kind = archive_type(path.name)
    if kind is None:
        raise ArchiveError(f"Not a supported archive: {path.name}")
    try:
        st = path.stat()
    except OSError as exc:
        raise ArchiveError(str(exc)) from exc
    key = (str(path), st.st_mtime, st.st_size)
    with _open_lock:
        if key in _open_cache:
            return _open_cache[key]
    match kind:
        case "zip":
            archive: Archive = ZipArchive(path)
        case "tar":
            archive = TarArchive(path)
        case "7z":
            archive = SevenZipArchive(path)
        case "rar":
            archive = RarArchive(path)
        case _:
            archive = CompressedFile(path, kind)
    with _open_lock:
        _open_cache[key] = archive
        evicted = []
        while len(_open_cache) > _OPEN_CACHE_SIZE:
            evicted.append(_open_cache.pop(next(iter(_open_cache))))
    # Evicted archives may still be in use (open panels, search hits): close()
    # only releases the handle, they reopen it when read again.
    for old in evicted:
        old.close(wait=False)
    return archive


# ---------------------------------------------------------------------------
# Temp cache for previewing / opening members
# ---------------------------------------------------------------------------

_cache_dir: Path | None = None


def cache_dir() -> Path:
    global _cache_dir
    if _cache_dir is None:
        _cache_dir = Path(tempfile.mkdtemp(prefix="orchard-commander-"))
        atexit.register(shutil.rmtree, _cache_dir, ignore_errors=True)
    return _cache_dir


def extract_cached(archive: Archive, name: str) -> Path:
    """Extract a file member to the temp cache (once) and return its path.

    The file keeps its original name, so its extension still identifies it.
    """
    key = hashlib.sha1(f"{archive.path}|{archive.path.stat().st_mtime}|{name}".encode()).hexdigest()[:16]
    target = cache_dir() / key / posixpath.basename(name)
    if not target.exists():
        target.parent.mkdir(parents=True, exist_ok=True)
        archive.extract_file(name, target)
    return target
