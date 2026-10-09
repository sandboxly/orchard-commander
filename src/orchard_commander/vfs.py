"""Virtual file system: one interface over local directories and archives.

Everything above this layer (panels, previews, file operations) works with
`VPath` values — a file system plus a path in it — and asks the file system
what it can do through capability flags instead of checking what it is.

    LocalFS    the real file system; paths are absolute OS paths
    ArchiveFS  a read-only archive; paths are POSIX paths inside it, "" = root.
               Its `container` is the VPath of the archive file, which may
               itself be inside another archive.

`copy()` moves data between any two file systems, delegating to the native,
well-tested routines in `fs` when both sides are local.
"""

from __future__ import annotations

import hashlib
import os
import posixpath
import shutil
from abc import ABC, abstractmethod
from collections.abc import Iterable, Iterator
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO

from orchard_commander import archives, fs

CHUNK = 1024 * 1024

type RelPath = tuple[str, ...]  # path below a walk's starting point, as parts


class ReadOnlyError(fs.OperationError):
    pass


class FileSystem(ABC):
    """A source of directories and files. Subclasses override what they support."""

    type_name = "file system"
    writable = False  # mkdir, remove, open_write
    can_trash = False  # trash() moves to a recycle bin; otherwise deletion is permanent
    can_rename = False  # rename() within this file system (fast moves)
    watchable = False  # local path that a directory watcher can observe
    container: VPath | None = None  # for nested file systems: where they live

    @property
    def read_only_reason(self) -> str:
        return f"{self.type_name} is read-only"

    # -- reading ------------------------------------------------------------

    @abstractmethod
    def list_dir(
        self, path: str, *, show_hidden: bool = False, sort_key: fs.SortKey = fs.SortKey.NAME, reverse: bool = False
    ) -> list[fs.Entry]: ...

    @abstractmethod
    def stat(self, path: str) -> fs.Entry:
        """Raises FileNotFoundError if `path` doesn't exist."""

    @abstractmethod
    def parent_path(self, path: str) -> str | None:
        """Parent within this file system, or None at its root."""

    @abstractmethod
    def join(self, path: str, *names: str) -> str: ...

    @abstractmethod
    def basename(self, path: str) -> str: ...

    @abstractmethod
    def display(self, path: str) -> str: ...

    @abstractmethod
    def open_read(self, path: str) -> BinaryIO: ...

    @abstractmethod
    def walk(self, path: str) -> Iterator[tuple[RelPath, fs.Entry]]:
        """`path` itself (as ()) and everything below it, directories before their contents."""

    def exists(self, path: str) -> bool:
        try:
            self.stat(path)
            return True
        except FileNotFoundError:
            return False

    def is_dir(self, path: str) -> bool:
        try:
            return self.stat(path).is_dir
        except FileNotFoundError:
            return False

    def local_path(self, path: str) -> Path | None:
        """A real path for this item if one exists without copying."""
        return None

    def materialize(self, path: str) -> Path:
        """A real file with this item's contents (extracted to a temp cache if needed)."""
        local = self.local_path(path)
        if local is not None:
            return local
        entry = self.stat(path)
        key = hashlib.sha1(f"{self.display(path)}|{entry.size}|{entry.mtime}".encode()).hexdigest()[:16]
        target = archives.cache_dir() / "vfs" / key / self.basename(path)
        if not target.exists():
            target.parent.mkdir(parents=True, exist_ok=True)
            partial = target.with_name(target.name + ".part")
            try:
                with self.open_read(path) as src, open(partial, "wb") as dst:
                    shutil.copyfileobj(src, dst, CHUNK)
                os.replace(partial, target)
            finally:
                partial.unlink(missing_ok=True)
        return target

    def free_space(self, path: str) -> int | None:
        return None

    # -- writing (only for writable file systems) ---------------------------------

    def _read_only(self) -> ReadOnlyError:
        return ReadOnlyError(self.read_only_reason)

    def mkdir(self, path: str, *, exist_ok: bool = False) -> None:
        raise self._read_only()

    def remove(self, path: str) -> None:
        """Delete permanently (files or whole directories)."""
        raise self._read_only()

    def trash(self, paths: list[str]) -> None:
        raise self._read_only()

    def rename(self, src: str, dst: str) -> None:
        raise self._read_only()

    def open_write(self, path: str) -> BinaryIO:
        raise self._read_only()

    def set_mtime(self, path: str, mtime: float) -> None:
        pass

    def close(self) -> None:
        """Release connections etc. The file system may not be used afterwards."""


@dataclass(frozen=True, slots=True)
class VPath:
    fs: FileSystem
    path: str

    # -- navigation ------------------------------------------------------------

    @property
    def name(self) -> str:
        if self.fs.parent_path(self.path) is None and self.fs.container is not None:
            return self.fs.container.name  # an archive's root is named after the archive
        return self.fs.basename(self.path)

    def parent(self) -> VPath | None:
        """The containing directory; from an archive's root, the directory holding the archive."""
        parent = self.fs.parent_path(self.path)
        if parent is not None:
            return VPath(self.fs, parent)
        return self.fs.container.parent() if self.fs.container is not None else None

    def child(self, *names: str) -> VPath:
        return VPath(self.fs, self.fs.join(self.path, *names))

    @property
    def display(self) -> str:
        return self.fs.display(self.path)

    @property
    def is_local(self) -> bool:
        return isinstance(self.fs, LocalFS)

    def local_anchor(self) -> Path | None:
        """The nearest real directory: itself if local, else where the outermost
        archive is. None if this isn't backed by the local disk (e.g. a server)."""
        v: VPath = self
        while v.fs.container is not None:
            v = v.fs.container
        if not v.is_local:
            return None
        p = Path(v.path)
        return p if v is self and v.is_dir() else p.parent

    # -- queries -----------------------------------------------------------------

    def stat(self) -> fs.Entry:
        return self.fs.stat(self.path)

    def exists(self) -> bool:
        return self.fs.exists(self.path)

    def is_dir(self) -> bool:
        return self.fs.is_dir(self.path)

    def list_dir(self, **kwargs) -> list[fs.Entry]:
        return self.fs.list_dir(self.path, **kwargs)

    def local_path(self) -> Path | None:
        return self.fs.local_path(self.path)

    def materialize(self) -> Path:
        return self.fs.materialize(self.path)

    def __str__(self) -> str:
        return self.display


# ---------------------------------------------------------------------------
# Local file system
# ---------------------------------------------------------------------------


class LocalFS(FileSystem):
    type_name = "local disk"
    writable = True
    can_trash = True
    can_rename = True
    watchable = True

    def list_dir(self, path: str, **kwargs) -> list[fs.Entry]:
        return fs.list_dir(Path(path), **kwargs)

    def stat(self, path: str) -> fs.Entry:
        return fs.stat_entry(Path(path))

    def exists(self, path: str) -> bool:
        return os.path.lexists(path)

    def is_dir(self, path: str) -> bool:
        return os.path.isdir(path)

    def parent_path(self, path: str) -> str | None:
        parent = fs.parent_of(Path(path))
        return str(parent) if parent is not None else None

    def join(self, path: str, *names: str) -> str:
        return os.path.join(path, *names)

    def basename(self, path: str) -> str:
        return os.path.basename(path.rstrip(os.sep)) or path

    def display(self, path: str) -> str:
        return path

    def open_read(self, path: str) -> BinaryIO:
        return open(path, "rb")

    def walk(self, path: str) -> Iterator[tuple[RelPath, fs.Entry]]:
        root = Path(path)
        yield (), fs.stat_entry(root)
        if not root.is_dir() or root.is_symlink():
            return
        for dirpath, dirs, files in os.walk(root, followlinks=False):
            rel = Path(dirpath).relative_to(root).parts
            for name in sorted(dirs) + sorted(files):
                p = Path(dirpath) / name
                entry = fs.stat_entry(p)
                if entry.is_link and entry.is_dir:  # don't descend; copy the link itself
                    entry = fs.Entry(entry.name, False, True, entry.is_hidden, False, 0, entry.mtime)
                yield (*rel, name), entry

    def local_path(self, path: str) -> Path | None:
        return Path(path)

    def free_space(self, path: str) -> int | None:
        return fs.disk_free(Path(path))

    def mkdir(self, path: str, *, exist_ok: bool = False) -> None:
        Path(path).mkdir(parents=True, exist_ok=exist_ok)

    def remove(self, path: str) -> None:
        p = Path(path)
        if p.is_dir() and not p.is_symlink():
            shutil.rmtree(p)
        else:
            p.unlink()

    def trash(self, paths: list[str]) -> None:
        fs.trash_paths(Path(p) for p in paths)

    def rename(self, src: str, dst: str) -> None:
        os.rename(src, dst)

    def open_write(self, path: str) -> BinaryIO:
        return open(path, "wb")

    def set_mtime(self, path: str, mtime: float) -> None:
        os.utime(path, (mtime, mtime))


LOCAL = LocalFS()


def local(path: str | Path) -> VPath:
    return VPath(LOCAL, os.path.abspath(path))


# ---------------------------------------------------------------------------
# Archives
# ---------------------------------------------------------------------------


class ArchiveFS(FileSystem):
    """Read-only view of an archive. Equal instances wrap the same opened archive."""

    def __init__(self, archive: archives.Archive, container: VPath) -> None:
        self.archive = archive
        self.container = container
        self.type_name = f"{archive.type_name} archive"

    @property
    def read_only_reason(self) -> str:
        return f"{self.container.name if self.container else 'this archive'} is an archive (read-only)"

    def __eq__(self, other: object) -> bool:
        return isinstance(other, ArchiveFS) and other.archive is self.archive and other.container == self.container

    def __hash__(self) -> int:
        return hash((id(self.archive), self.container))

    def list_dir(self, path: str, **kwargs) -> list[fs.Entry]:
        try:
            return self.archive.list_dir(path, **kwargs)
        except archives.ArchiveError as exc:
            raise NotADirectoryError(str(exc)) from exc

    def stat(self, path: str) -> fs.Entry:
        if path == "":
            name = self.container.name if self.container else ""
            return fs.Entry(name, True, False, False, False, 0, self.archive.path.stat().st_mtime)
        member = self.archive.members.get(path)
        if member is None:
            raise FileNotFoundError(path)
        return self.archive.entry(member)

    def exists(self, path: str) -> bool:
        return path == "" or path in self.archive.members

    def is_dir(self, path: str) -> bool:
        return self.archive.is_dir(path)

    def parent_path(self, path: str) -> str | None:
        return posixpath.dirname(path) if path else None

    def join(self, path: str, *names: str) -> str:
        return "/".join(p for p in (path, *names) if p)

    def basename(self, path: str) -> str:
        return posixpath.basename(path)

    def display(self, path: str) -> str:
        base = self.container.display if self.container else self.archive.path.name
        return os.path.join(base, *path.split("/")) if path else base

    def open_read(self, path: str) -> BinaryIO:
        try:
            return self.archive.open(path)
        except archives.ArchiveError as exc:
            raise fs.OperationError(str(exc)) from exc

    def walk(self, path: str) -> Iterator[tuple[RelPath, fs.Entry]]:
        yield (), self.stat(path)
        for member in self.archive.walk(path):
            if member.name == path:
                continue
            rel = member.name[len(path) + 1 :] if path else member.name
            yield tuple(rel.split("/")), self.archive.entry(member)

    def materialize(self, path: str) -> Path:
        try:
            return archives.extract_cached(self.archive, path)
        except archives.ArchiveError as exc:
            raise fs.OperationError(str(exc)) from exc


def open_archive(file: VPath) -> VPath:
    """Enter archive `file` (on any file system); returns the archive's root.

    Raises archives.ArchiveError if it can't be read.
    """
    archive = archives.open_archive(file.materialize())
    return VPath(ArchiveFS(archive, file), "")


def is_archive(path: VPath) -> bool:
    return archives.is_archive(path.name) and not path.is_dir()


# ---------------------------------------------------------------------------
# Operations across file systems
# ---------------------------------------------------------------------------


def _resolve_target(sources: list[VPath], target: VPath) -> list[tuple[VPath, VPath]]:
    if target.is_dir():
        return [(s, target.child(s.name)) for s in sources]
    if len(sources) == 1:
        return [(sources[0], target)]
    raise fs.OperationError(f"Target directory does not exist: {target.display}")


def copy(
    sources: list[VPath],
    target: VPath,
    *,
    move: bool = False,
    on_conflict: fs.ConflictCallback,
    on_progress: fs.ProgressCallback | None = None,
    is_cancelled: fs.CancelCheck | None = None,
) -> None:
    """Copy (or move) `sources` to `target`, across any file systems.

    Target semantics match `fs.copy_paths`: into `target` if it is a
    directory, otherwise a single source is written under that name.
    """
    if not target.fs.writable:
        raise ReadOnlyError(target.fs.read_only_reason)
    if move:
        for s in sources:
            if not s.fs.writable:
                raise ReadOnlyError(s.fs.read_only_reason)
    if target.is_local and all(s.is_local for s in sources):
        native = fs.move_paths if move else fs.copy_paths
        native(
            [Path(s.path) for s in sources],
            Path(target.path),
            on_conflict=on_conflict,
            on_progress=on_progress,
            is_cancelled=is_cancelled,
        )
        return

    pairs = _resolve_target(sources, target)
    if move:
        # Same file system: a server-side rename is instant. Only possible when
        # nothing is in the way; otherwise fall through to copy + delete.
        remaining = []
        for src, dst in pairs:
            if src.fs == dst.fs and src.fs.can_rename and not dst.exists():
                try:
                    src.fs.rename(src.path, dst.path)
                    continue
                except OSError:
                    pass  # some servers refuse renames; copy + delete instead
            remaining.append((src, dst))
        pairs = remaining
    plans = [(src, dst, list(src.fs.walk(src.path))) for src, dst in pairs]
    files = [e for _, _, items in plans for _, e in items if not e.is_dir]
    runner = fs.Transfer(on_conflict, on_progress, is_cancelled, len(files), sum(e.size for e in files))
    for src, dst_root, items in plans:
        _stream_tree(runner, src, dst_root, items)
        if move:
            src.fs.remove(src.path)


def _stream_tree(runner: fs.Transfer, src_root: VPath, dst_root: VPath, items: list[tuple[RelPath, fs.Entry]]) -> None:
    skipped: list[RelPath] = []  # directories the user chose not to overwrite

    def skip(entry: fs.Entry) -> None:
        if not entry.is_dir:
            runner.progress.files_done += 1
            runner.progress.bytes_done += entry.size

    for rel, entry in items:
        if any(rel[: len(d)] == d for d in skipped):
            skip(entry)
            continue
        src, dst = src_root.child(*rel), dst_root.child(*rel)
        runner.check_cancel()
        runner.report(Path(src.display))
        if dst.exists() and not (entry.is_dir and dst.is_dir()):
            if not _confirm_overwrite(runner, src, dst):
                skipped.append(rel)
                skip(entry)
                continue
        if entry.is_dir:
            dst.fs.mkdir(dst.path, exist_ok=True)
            continue
        _stream_file(runner, src, dst)
        if entry.mtime:
            dst.fs.set_mtime(dst.path, entry.mtime)
        runner.progress.files_done += 1
        runner.report()


def _confirm_overwrite(runner: fs.Transfer, src: VPath, dst: VPath) -> bool:
    choice = runner.on_conflict(Path(src.display), dst.local_path() or Path(dst.display))
    if choice is fs.Conflict.CANCEL:
        raise fs.OperationCancelled
    if choice is fs.Conflict.SKIP:
        return False
    dst.fs.remove(dst.path)
    return True


def _stream_file(runner: fs.Transfer, src: VPath, dst: VPath) -> None:
    try:
        with src.fs.open_read(src.path) as reader, dst.fs.open_write(dst.path) as writer:
            while chunk := reader.read(CHUNK):
                writer.write(chunk)
                runner.progress.bytes_done += len(chunk)
                runner.report()
                runner.check_cancel()
    except BaseException:
        if dst.exists():
            dst.fs.remove(dst.path)  # don't leave a partial file behind
        raise


def delete(paths: Iterable[VPath]) -> None:
    """Delete permanently (for file systems without a trash, e.g. servers)."""
    for p in paths:
        if not p.fs.writable:
            raise ReadOnlyError(p.fs.read_only_reason)
        p.fs.remove(p.path)


def trash(paths: Iterable[VPath]) -> None:
    by_fs: dict[FileSystem, list[str]] = {}
    for p in paths:
        if not p.fs.can_trash:
            raise ReadOnlyError(p.fs.read_only_reason)
        by_fs.setdefault(p.fs, []).append(p.path)
    for filesystem, items in by_fs.items():
        filesystem.trash(items)


def mkdir(parent: VPath, name: str) -> VPath:
    path = parent.child(name)
    parent.fs.mkdir(path.path)
    return path
