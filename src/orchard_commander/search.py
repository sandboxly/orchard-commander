"""Find files — after Total Commander's Find Files (Alt+F7) and "Feed to listbox".

`run()` walks one or more folders on any file system (local, servers, inside
archives) and reports hits as it finds them. `SearchResultsFS` shows the hits
as a virtual folder: each entry stands for the real file (`real_location`), so
copy/move/trash/preview/open act on the real files.

Name patterns follow Total Commander: masks separated by spaces or ";", one
"|" separates included from excluded masks ("*.zip|*big*.zip"), and a mask
without wildcards matches anywhere in the name ("report" = "*report*").
"""

from __future__ import annotations

import fnmatch
import os
import re
import threading
import time
from collections.abc import Callable, Iterator
from dataclasses import dataclass, field, replace
from typing import BinaryIO

from orchard_commander import archives, fs, vfs

CHUNK = 1024 * 1024
OVERLAP = 4096  # bytes kept between chunks so matches across chunk borders are found
SNIPPET_CHARS = 160


@dataclass(frozen=True)
class Query:
    roots: tuple[vfs.VPath, ...]
    names: str = "*"
    name_regex: bool = False
    name_case: bool = False
    recursive: bool = True
    max_depth: int | None = None  # None: unlimited; 0: only the root folders themselves
    include_hidden: bool = True
    kinds: str = "both"  # "files" | "dirs" | "both"
    text: str = ""
    text_regex: bool = False
    text_case: bool = False
    whole_words: bool = False
    not_containing: bool = False  # files that do NOT contain the text
    min_size: int | None = None
    max_size: int | None = None
    newer_than: float | None = None  # mtime >=
    older_than: float | None = None  # mtime <=
    search_archives: bool = False
    include_online_only: bool = False  # content search downloads cloud placeholders only if True

    def describe(self) -> str:
        what = self.names.strip() or "*"
        if self.text:
            what += f" {'without' if self.not_containing else 'containing'} “{self.text}”"
        where = ", ".join(r.name or r.display for r in self.roots)
        return f"{what} in {where}"


@dataclass(frozen=True)
class Hit:
    location: vfs.VPath  # the real file or folder
    entry: fs.Entry
    relative: str  # path below the search root, for display
    snippet: str = ""  # the first matching line, for content searches


@dataclass
class Progress:
    folders: int = 0
    files: int = 0
    hits: int = 0
    current: str = ""
    skipped_online_only: int = 0
    errors: list[str] = field(default_factory=list)


# ---------------------------------------------------------------------------
# Name matching
# ---------------------------------------------------------------------------


class NameMatcher:
    def __init__(self, pattern: str, *, regex: bool, case: bool) -> None:
        self.regex = regex
        if regex:
            self._re = re.compile(pattern, 0 if case else re.IGNORECASE) if pattern.strip() else None
            return
        include, _, exclude = pattern.partition("|")
        self._include = [self._mask(m, case) for m in self._split(include)] or [self._mask("*", case)]
        self._exclude = [self._mask(m, case) for m in self._split(exclude)]

    @staticmethod
    def _split(masks: str) -> list[str]:
        return [m for m in re.split(r"[;\s]+", masks.strip()) if m]

    @staticmethod
    def _mask(mask: str, case: bool) -> re.Pattern[str]:
        if not any(c in mask for c in "*?["):
            mask = f"*{mask}*"  # TC: plain text matches anywhere in the name
        return re.compile(fnmatch.translate(mask), 0 if case else re.IGNORECASE)

    def matches(self, name: str) -> bool:
        if self.regex:
            return self._re is None or self._re.search(name) is not None
        return any(m.match(name) for m in self._include) and not any(m.match(name) for m in self._exclude)


# ---------------------------------------------------------------------------
# Content matching
# ---------------------------------------------------------------------------


class TextMatcher:
    """Finds text in a stream, chunk by chunk (with overlap), decoding as UTF-8."""

    def __init__(self, text: str, *, regex: bool, case: bool, whole_words: bool) -> None:
        pattern = text if regex else re.escape(text)
        if whole_words:
            pattern = rf"\b(?:{pattern})\b"
        self._re = re.compile(pattern, (0 if case else re.IGNORECASE) | re.MULTILINE)

    def search(self, stream: BinaryIO, is_cancelled: Callable[[], bool]) -> str | None:
        """The first matching line, or None."""
        tail = ""
        while chunk := stream.read(CHUNK):
            if is_cancelled():
                return None
            text = tail + chunk.decode("utf-8", errors="replace")
            match = self._re.search(text)
            if match:
                start = text.rfind("\n", 0, match.start()) + 1
                end = text.find("\n", match.end())
                line = text[start : end if end != -1 else len(text)].strip()
                return line[:SNIPPET_CHARS]
            tail = text[-OVERLAP:]
        return None


# ---------------------------------------------------------------------------
# Walking
# ---------------------------------------------------------------------------


def _entry_ok(q: Query, entry: fs.Entry) -> bool:
    if entry.is_dir:
        if q.kinds == "files":
            return False
    elif q.kinds == "dirs":
        return False
    if not entry.is_dir:
        if q.min_size is not None and entry.size < q.min_size:
            return False
        if q.max_size is not None and entry.size > q.max_size:
            return False
    if q.newer_than is not None and entry.mtime < q.newer_than:
        return False
    if q.older_than is not None and entry.mtime > q.older_than:
        return False
    return True


def run(
    query: Query,
    on_hit: Callable[[Hit], None],
    *,
    progress: Progress | None = None,
    is_cancelled: Callable[[], bool] = lambda: False,
) -> Progress:
    """Search (blocking; run it on a worker thread). Hits are reported as found."""
    progress = progress or Progress()
    names = NameMatcher(query.names, regex=query.name_regex, case=query.name_case)
    text = TextMatcher(query.text, regex=query.text_regex, case=query.text_case,
                       whole_words=query.whole_words) if query.text else None

    def consider(location: vfs.VPath, entry: fs.Entry, relative: str) -> None:
        if not names.matches(entry.name) or not _entry_ok(query, entry):
            return
        snippet = ""
        if text is not None:
            if entry.is_dir:
                return  # content criteria apply to files only
            if entry.is_online_only and not query.include_online_only:
                progress.skipped_online_only += 1
                return
            try:
                with location.fs.open_read(location.path) as stream:
                    found = text.search(stream, is_cancelled)
            except (OSError, fs.OperationError) as exc:
                progress.errors.append(f"{relative}: {exc}")
                return
            if (found is not None) == query.not_containing:
                return
            snippet = found or ""
        progress.hits += 1
        on_hit(Hit(location, entry, relative, snippet))

    for root in query.roots:
        for location, entry, relative in _walk(query, root, progress, is_cancelled):
            if is_cancelled():
                return progress
            consider(location, entry, relative)
    return progress


def _walk(
    query: Query, root: vfs.VPath, progress: Progress, is_cancelled: Callable[[], bool]
) -> Iterator[tuple[vfs.VPath, fs.Entry, str]]:
    root_label = "" if len(query.roots) == 1 else (root.name or root.display)
    stack: list[tuple[vfs.VPath, str, int]] = [(root, root_label, 0)]
    while stack:
        if is_cancelled():
            return
        folder, rel_folder, depth = stack.pop()
        progress.folders += 1
        progress.current = folder.display
        try:
            entries = folder.list_dir(show_hidden=query.include_hidden)
        except (OSError, fs.OperationError, archives.ArchiveError) as exc:
            progress.errors.append(f"{folder.display}: {exc}")
            continue
        subfolders = []
        for entry in entries:
            location = folder.child(entry.name)
            relative = f"{rel_folder}{os.sep}{entry.name}" if rel_folder else entry.name
            if not entry.is_dir:
                progress.files += 1
            yield location, entry, relative
            deeper = query.recursive and (query.max_depth is None or depth < query.max_depth)
            if not deeper:
                continue
            if entry.is_dir and not entry.is_link:  # don't follow links: no loops
                subfolders.append((location, relative, depth + 1))
            elif (query.search_archives and not entry.is_dir and archives.is_archive(entry.name)
                  and not (entry.is_online_only and not query.include_online_only)):
                try:
                    subfolders.append((vfs.open_archive(location), relative, depth + 1))
                except (archives.ArchiveError, fs.OperationError, OSError) as exc:
                    progress.errors.append(f"{relative}: {exc}")
        stack.extend(reversed(subfolders))  # depth-first, in listing order


# ---------------------------------------------------------------------------
# Results as a folder
# ---------------------------------------------------------------------------


class SearchResultsFS(vfs.FileSystem):
    """A virtual folder of search hits; each entry stands for the real file.

    Its only directory is the root (""); entries are named by their path below
    the search root. Copy/move/trash/preview act on the real files through
    `real_location`. Thread-safe: the search adds hits while the panel lists.
    """

    writable = False  # nothing can be created *in* the results

    def __init__(self, query: Query) -> None:
        self.query = query
        self._lock = threading.Lock()
        self._hits: dict[str, Hit] = {}
        self.progress = Progress()
        self.running = True
        self.started = time.monotonic()
        self.finished: float | None = None
        self.cancelled = False
        self.changes = 0  # bumped on every new hit / prune; the UI refreshes on change

    # -- filling ----------------------------------------------------------------------

    def add(self, hit: Hit) -> None:
        with self._lock:
            name = hit.relative
            n = 2
            while name in self._hits:  # same relative path from two roots
                name = f"{hit.relative} ({n})"
                n += 1
            self._hits[name] = hit
            self.changes += 1

    def finish(self, *, cancelled: bool = False) -> None:
        self.running = False
        self.cancelled = cancelled
        self.finished = time.monotonic()
        self.changes += 1

    def prune(self) -> int:
        """Forget hits whose files are gone (after a move/trash); returns how many."""
        with self._lock:
            items = list(self._hits.items())
        gone = [name for name, hit in items if not _exists(hit.location)]
        with self._lock:
            for name in gone:
                self._hits.pop(name, None)
            if gone:
                self.changes += 1
        return len(gone)

    def relocate(self, moved: dict[vfs.VPath, vfs.VPath]) -> None:
        """Follow renamed files and folders (also hits inside a renamed folder)."""
        if not moved:
            return
        with self._lock:
            updated: dict[str, Hit] = {}
            for name, hit in self._hits.items():
                below: list[str] = []
                ancestor: vfs.VPath | None = hit.location
                while ancestor is not None and ancestor not in moved:
                    below.insert(0, ancestor.name)
                    ancestor = ancestor.parent()
                if ancestor is None:
                    updated[name] = hit
                    continue
                location = moved[ancestor].child(*below) if below else moved[ancestor]
                parts = hit.relative.split(os.sep)
                index = len(parts) - len(below) - 1
                if index >= 0:
                    parts[index] = moved[ancestor].name
                relative = os.sep.join(parts)
                key = relative if relative not in updated else name
                entry = replace(hit.entry, name=location.name)
                updated[key] = Hit(location, entry, relative, hit.snippet)
            self._hits = updated
            self.changes += 1

    @property
    def hits(self) -> list[Hit]:
        with self._lock:
            return list(self._hits.values())

    # -- presentation ---------------------------------------------------------------

    @property
    def type_name(self) -> str:  # type: ignore[override]
        return "search results"

    @property
    def status_text(self) -> str | None:
        p = self.progress
        hits = f"{len(self._hits):,} hit{'s' * (len(self._hits) != 1)}"
        if self.running:
            return f"searching… {p.folders:,} folders, {p.files:,} files · {hits}"
        extra = []
        if self.cancelled:
            extra.append("stopped")
        if p.skipped_online_only:
            extra.append(f"{p.skipped_online_only:,} cloud-only files not searched")
        if p.errors:
            extra.append(f"{len(p.errors):,} unreadable")
        return " · ".join([hits, *extra])

    def display(self, path: str) -> str:
        return f"Search: {self.query.describe()}" if not path else path

    def exit_to(self) -> vfs.VPath | None:
        return self.query.roots[0] if self.query.roots else None

    # -- the file system interface ------------------------------------------------

    def _hit(self, path: str) -> Hit:
        with self._lock:
            hit = self._hits.get(path)
        if hit is None:
            raise FileNotFoundError(path)
        return hit

    def real_location(self, path: str) -> vfs.VPath | None:
        return self._hit(path).location if path else None

    def list_dir(self, path: str, **kwargs) -> list[fs.Entry]:
        if path:
            raise NotADirectoryError(path)
        with self._lock:
            items = list(self._hits.items())
        kwargs["show_hidden"] = True  # hidden-ness was decided by the query
        return fs.sort_entries([replace(hit.entry, name=name) for name, hit in items], **kwargs)

    def stat(self, path: str) -> fs.Entry:
        if not path:
            return fs.Entry(self.display(""), True, False, False, False, 0, 0.0)
        return self._hit(path).location.stat()

    def exists(self, path: str) -> bool:
        return not path or _exists(self._hit_or_none(path))

    def is_dir(self, path: str) -> bool:
        return not path or (self._hit_or_none(path) is not None and self._hit(path).entry.is_dir)

    def _hit_or_none(self, path: str) -> vfs.VPath | None:
        try:
            return self._hit(path).location
        except FileNotFoundError:
            return None

    def parent_path(self, path: str) -> str | None:
        return None if not path else ""

    def join(self, path: str, *names: str) -> str:
        return names[-1] if names else path

    def basename(self, path: str) -> str:
        return path or "Search results"

    def open_read(self, path: str) -> BinaryIO:
        real = self._hit(path).location
        return real.fs.open_read(real.path)

    def walk(self, path: str):
        real = self._hit(path).location
        return real.fs.walk(real.path)

    def local_path(self, path: str):
        return self._hit(path).location.local_path() if path else None

    def materialize(self, path: str):
        return self._hit(path).location.materialize()


def _exists(location: vfs.VPath | None) -> bool:
    if location is None:
        return False
    try:
        return location.exists()
    except (OSError, fs.OperationError):
        return False
