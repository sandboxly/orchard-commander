"""Filesystem layer: directory listing and file operations.

Nothing in here knows about Textual, so it can be unit-tested on its own and
reused by any frontend. Long-running operations report progress and ask about
conflicts through callbacks supplied by the caller.
"""

from __future__ import annotations

import enum
import errno
import os
import shutil
import stat
import string
import subprocess
import sys
from collections.abc import Callable, Iterable, Iterator
from dataclasses import dataclass
from pathlib import Path

from send2trash import send2trash

IS_WINDOWS = sys.platform == "win32"
IS_MACOS = sys.platform == "darwin"

COPY_CHUNK = 1024 * 1024


# ---------------------------------------------------------------------------
# Listing
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class Entry:
    """A single item in a directory listing (on any file system; see vfs.py)."""

    name: str
    is_dir: bool
    is_link: bool
    is_hidden: bool
    is_executable: bool
    size: int
    mtime: float
    is_online_only: bool = False  # a cloud placeholder (Dropbox, iCloud, OneDrive): reading downloads it

    @property
    def ext(self) -> str:
        if self.is_dir:
            return ""
        _, dot, ext = self.name.rpartition(".")
        return ext.lower() if dot and _ else ""


class SortKey(enum.Enum):
    NAME = "name"
    EXT = "ext"
    SIZE = "size"
    MTIME = "mtime"


def _is_hidden(name: str, st: os.stat_result | None) -> bool:
    if name.startswith("."):
        return True
    if IS_WINDOWS and st is not None:
        attrs = getattr(st, "st_file_attributes", 0)
        return bool(attrs & (stat.FILE_ATTRIBUTE_HIDDEN | stat.FILE_ATTRIBUTE_SYSTEM))
    if IS_MACOS and st is not None:
        return bool(getattr(st, "st_flags", 0) & stat.UF_HIDDEN)
    return False


def _is_executable(name: str, st: os.stat_result) -> bool:
    if IS_WINDOWS:
        return os.path.splitext(name)[1].lower() in {".exe", ".bat", ".cmd", ".com", ".ps1"}
    return bool(st.st_mode & (stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH))


# Cloud placeholders: the file's data is not on disk yet; reading it downloads it.
SF_DATALESS = 0x40000000  # macOS (APFS): File Provider placeholders (Dropbox, iCloud Drive, OneDrive)
WINDOWS_CLOUD_ATTRIBUTES = 0x00400000 | 0x00040000 | 0x00001000  # RECALL_ON_DATA_ACCESS | RECALL_ON_OPEN | OFFLINE


def _is_online_only(st: os.stat_result) -> bool:
    if IS_MACOS:
        return bool(getattr(st, "st_flags", 0) & SF_DATALESS)
    if IS_WINDOWS:
        return bool(getattr(st, "st_file_attributes", 0) & WINDOWS_CLOUD_ATTRIBUTES)
    return False


def _entry(name: str, lstat: os.stat_result, st: os.stat_result | None) -> Entry:
    """Build an Entry from lstat and (link-following) stat; st is None for dangling links."""
    is_link = stat.S_ISLNK(lstat.st_mode)
    eff = st or lstat
    is_dir = stat.S_ISDIR(eff.st_mode)
    return Entry(
        name=name,
        is_dir=is_dir,
        is_link=is_link,
        is_hidden=_is_hidden(name, eff),
        is_executable=not is_dir and _is_executable(name, eff),
        size=0 if is_dir else eff.st_size,
        mtime=eff.st_mtime,
        is_online_only=not is_dir and _is_online_only(eff),
    )


def _entry_from_dirent(de: os.DirEntry[str]) -> Entry:
    try:
        lst = de.stat(follow_symlinks=False)
        try:
            st = de.stat()  # follows symlinks
        except OSError:
            st = None  # dangling link
    except OSError:
        return Entry(de.name, False, False, de.name.startswith("."), False, 0, 0.0)
    return _entry(de.name, lst, st)


def stat_entry(path: Path) -> Entry:
    """Entry for a single path. Raises OSError if it doesn't exist."""
    lst = path.lstat()
    try:
        st = path.stat()
    except OSError:
        st = None
    return _entry(path.name or str(path), lst, st)


def list_dir(
    path: Path,
    *,
    show_hidden: bool = False,
    sort_key: SortKey = SortKey.NAME,
    reverse: bool = False,
) -> list[Entry]:
    """List `path`, directories first, then files, each group sorted by `sort_key`.

    Raises OSError if the directory cannot be read.
    """
    with os.scandir(path) as it:
        entries = [_entry_from_dirent(de) for de in it]
    return sort_entries(entries, show_hidden=show_hidden, sort_key=sort_key, reverse=reverse)


def sort_entries(
    entries: Iterable[Entry],
    *,
    show_hidden: bool = False,
    sort_key: SortKey = SortKey.NAME,
    reverse: bool = False,
) -> list[Entry]:
    """Filter hidden entries and sort: directories first, then files, each by `sort_key`."""
    if not show_hidden:
        entries = [e for e in entries if not e.is_hidden]

    def key(e: Entry) -> tuple:
        name = e.name.casefold()
        match sort_key:
            case SortKey.NAME:
                return (name,)
            case SortKey.EXT:
                return (e.ext, name)
            case SortKey.SIZE:
                return (e.size, name)
            case SortKey.MTIME:
                return (e.mtime, name)

    dirs = sorted((e for e in entries if e.is_dir), key=key, reverse=reverse)
    files = sorted((e for e in entries if not e.is_dir), key=key, reverse=reverse)
    return dirs + files


def parent_of(path: Path) -> Path | None:
    """Return the parent directory, or None when already at a root."""
    parent = path.parent
    return None if parent == path else parent


def locations() -> list[tuple[str, Path]]:
    """Places worth jumping to: drives on Windows, volumes/mounts elsewhere."""
    result: list[tuple[str, Path]] = []
    home = Path.home()
    if IS_WINDOWS:
        if hasattr(os, "listdrives"):  # Python 3.12+ on Windows
            drives = os.listdrives()
        else:
            drives = [f"{c}:\\" for c in string.ascii_uppercase if os.path.exists(f"{c}:\\")]
        result += [(d.rstrip("\\"), Path(d)) for d in drives]
        result.append(("Home", home))
        return result

    result += [("/", Path("/")), ("Home", home)]
    if IS_MACOS:
        mount_roots = [Path("/Volumes")]
    else:
        user = os.environ.get("USER", "")
        mount_roots = [Path("/media") / user, Path("/run/media") / user, Path("/mnt")]
    for root in mount_roots:
        try:
            for child in sorted(root.iterdir()):
                if child.is_dir():
                    result.append((child.name, child))
        except OSError:
            pass
    return result


def disk_free(path: Path) -> int | None:
    try:
        return shutil.disk_usage(path).free
    except OSError:
        return None


def human_size(n: int) -> str:
    if n < 1024:
        return f"{n} B"
    size = float(n)
    for unit in ("K", "M", "G", "T", "P"):
        size /= 1024
        if size < 1024 or unit == "P":
            return f"{size:.1f} {unit}" if size < 100 else f"{size:.0f} {unit}"
    raise AssertionError("unreachable")


# ---------------------------------------------------------------------------
# Opening files
# ---------------------------------------------------------------------------


def open_with_default_app(path: Path) -> None:
    """Open `path` with the OS default application, without blocking."""
    if IS_WINDOWS:
        os.startfile(path)  # type: ignore[attr-defined]
        return
    cmd = ["open", str(path)] if IS_MACOS else ["xdg-open", str(path)]
    subprocess.Popen(
        cmd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )


# ---------------------------------------------------------------------------
# Copy / move / delete
# ---------------------------------------------------------------------------


class Conflict(enum.Enum):
    OVERWRITE = "overwrite"
    SKIP = "skip"
    CANCEL = "cancel"


class OperationCancelled(Exception):
    pass


class OperationError(Exception):
    pass


@dataclass(slots=True)
class Progress:
    """Snapshot handed to the progress callback."""

    current: Path
    files_done: int
    files_total: int
    bytes_done: int
    bytes_total: int


ConflictCallback = Callable[[Path, Path], Conflict]
"""Called with (source, destination) when destination exists."""

ProgressCallback = Callable[[Progress], None]
CancelCheck = Callable[[], bool]


def _walk_files(path: Path) -> Iterator[Path]:
    """Yield every non-directory below `path` (or `path` itself), not following links."""
    if path.is_dir() and not path.is_symlink():
        for root, dirs, files in os.walk(path, followlinks=False):
            r = Path(root)
            for d in dirs:
                if (r / d).is_symlink():
                    yield r / d
            for f in files:
                yield r / f
    else:
        yield path


def measure(paths: Iterable[Path]) -> tuple[int, int]:
    """Return (file count, total bytes) for everything below `paths`."""
    count = total = 0
    for p in paths:
        for f in _walk_files(p):
            count += 1
            try:
                total += f.lstat().st_size
            except OSError:
                pass
    return count, total


def resolve_target(sources: list[Path], target: Path) -> list[tuple[Path, Path]]:
    """Map each source to its destination path.

    If `target` is an existing directory, sources go inside it. Otherwise a
    single source is renamed to `target`; multiple sources require a directory.
    """
    if target.is_dir():
        return [(s, target / s.name) for s in sources]
    if len(sources) == 1:
        return [(sources[0], target)]
    raise OperationError(f"Target directory does not exist: {target}")


def _check_not_into_self(src: Path, dst: Path) -> None:
    if not src.is_dir() or src.is_symlink():
        return
    s, d = os.path.normcase(src.resolve()), os.path.normcase(dst.resolve())
    if d == s or d.startswith(s.rstrip(os.sep) + os.sep):
        raise OperationError(f"Cannot copy or move '{src.name}' into itself")


def _same_file(a: Path, b: Path) -> bool:
    try:
        return os.path.samefile(a, b)
    except OSError:
        return False


class Transfer:
    """Shared state for one copy/move/extract operation: progress, conflicts, cancel."""

    def __init__(
        self,
        on_conflict: ConflictCallback,
        on_progress: ProgressCallback | None,
        is_cancelled: CancelCheck | None,
        files_total: int,
        bytes_total: int,
    ) -> None:
        self.on_conflict = on_conflict
        self.on_progress = on_progress
        self.is_cancelled = is_cancelled or (lambda: False)
        self.progress = Progress(Path(), 0, files_total, 0, bytes_total)

    def check_cancel(self) -> None:
        if self.is_cancelled():
            raise OperationCancelled

    def report(self, current: Path | None = None) -> None:
        if current is not None:
            self.progress.current = current
        if self.on_progress:
            self.on_progress(self.progress)

    def resolve_conflict(self, src: Path, dst: Path) -> bool:
        """Return True if `dst` may be written. Removes an existing file at `dst`."""
        if not os.path.lexists(dst):
            return True
        if dst.is_dir() and not dst.is_symlink() and src.is_dir() and not src.is_symlink():
            return True  # directories merge
        if _same_file(src, dst):
            raise OperationError(f"Source and destination are the same file: {dst}")
        choice = self.on_conflict(src, dst)
        if choice is Conflict.CANCEL:
            raise OperationCancelled
        if choice is Conflict.SKIP:
            return False
        if dst.is_dir() and not dst.is_symlink():
            shutil.rmtree(dst)
        else:
            dst.unlink()
        return True

    def copy_file(self, src: Path, dst: Path) -> None:
        self.check_cancel()
        self.report(src)
        if not self.resolve_conflict(src, dst):
            self.progress.files_done += 1
            self.progress.bytes_done += _lsize(src)
            return
        if src.is_symlink():
            os.symlink(os.readlink(src), dst, target_is_directory=src.is_dir())
            self.progress.bytes_done += _lsize(src)
        else:
            try:
                with open(src, "rb") as fsrc, open(dst, "wb") as fdst:
                    while chunk := fsrc.read(COPY_CHUNK):
                        fdst.write(chunk)
                        self.progress.bytes_done += len(chunk)
                        self.report()
                        self.check_cancel()
            except OperationCancelled:
                dst.unlink(missing_ok=True)  # don't leave a truncated file behind
                raise
            shutil.copystat(src, dst)
        self.progress.files_done += 1
        self.report()

    def copy_tree(self, src: Path, dst: Path) -> None:
        if src.is_symlink() or not src.is_dir():
            self.copy_file(src, dst)
            return
        self.check_cancel()
        if not self.resolve_conflict(src, dst):
            return
        dst.mkdir(exist_ok=True)
        with os.scandir(src) as it:
            children = sorted(it, key=lambda de: de.name)
        for child in children:
            self.copy_tree(Path(child.path), dst / child.name)
        shutil.copystat(src, dst)

    def move(self, src: Path, dst: Path) -> None:
        self.check_cancel()
        self.report(src)
        if os.path.lexists(dst):
            if src.is_dir() and not src.is_symlink() and dst.is_dir() and not dst.is_symlink():
                # Merge: move children individually, then drop the emptied source.
                with os.scandir(src) as it:
                    children = [Path(de.path) for de in it]
                for child in children:
                    self.move(child, dst / child.name)
                try:
                    src.rmdir()
                except OSError:
                    pass  # something was skipped
                return
            if not self.resolve_conflict(src, dst):
                n, size = measure([src])
                self.progress.files_done += n
                self.progress.bytes_done += size
                return
        try:
            os.rename(src, dst)
            n, size = measure([dst])
            self.progress.files_done += n
            self.progress.bytes_done += size
            self.report()
        except OSError as exc:
            if exc.errno != errno.EXDEV:
                raise
            # Different filesystem: copy then delete the source.
            self.copy_tree(src, dst)
            if src.is_dir() and not src.is_symlink():
                shutil.rmtree(src)
            else:
                src.unlink()


def _lsize(p: Path) -> int:
    try:
        return p.lstat().st_size
    except OSError:
        return 0


def copy_paths(
    sources: list[Path],
    target: Path,
    *,
    on_conflict: ConflictCallback,
    on_progress: ProgressCallback | None = None,
    is_cancelled: CancelCheck | None = None,
) -> None:
    """Copy `sources` to `target` (see `resolve_target` for the semantics)."""
    pairs = resolve_target(sources, target)
    for src, dst in pairs:
        _check_not_into_self(src, dst)
    files, size = measure(sources)
    runner = Transfer(on_conflict, on_progress, is_cancelled, files, size)
    for src, dst in pairs:
        runner.copy_tree(src, dst)


def move_paths(
    sources: list[Path],
    target: Path,
    *,
    on_conflict: ConflictCallback,
    on_progress: ProgressCallback | None = None,
    is_cancelled: CancelCheck | None = None,
) -> None:
    """Move/rename `sources` to `target` (see `resolve_target` for the semantics)."""
    pairs = resolve_target(sources, target)
    for src, dst in pairs:
        _check_not_into_self(src, dst)
        if src == dst:
            raise OperationError(f"Source and destination are the same: {dst}")
    files, size = measure(sources)
    runner = Transfer(on_conflict, on_progress, is_cancelled, files, size)
    for src, dst in pairs:
        runner.move(src, dst)


def trash_paths(paths: Iterable[Path]) -> None:
    """Move `paths` to the OS recycle bin / trash."""
    for p in paths:
        send2trash(p)
