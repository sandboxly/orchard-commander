"""Batch renaming — after Total Commander's Multi-Rename Tool (Ctrl+M).

A `Rule` turns each file's old name into a new one in three steps, as in TC:

1. the **masks** — name and extension — with placeholders in brackets:
   [N] name, [E] extension, ranges like [N2-5] [N2,5] [N2-] [N-8,5] [N-8-5]
   [N2--5] [N-5-], [2-5] over the whole name, [P] parent and [G] grandparent
   folder, [C] counter ([C10+5:3]: start 10, step 5, 3 digits), [d] [t] date
   and time of last modification, [Y] [M] [D] [h] [m] [s], case switches
   [U] [L] [F] [n] that apply to what follows, and [[] []] for brackets;
2. **search & replace** on the result — several pairs separated by "|",
   plain or RegEx ($1 / \\1 for groups), case-insensitive unless asked;
3. a **case conversion** of the whole name.

`plan()` checks the outcome (invalid names, duplicates, existing files) and
`execute()` renames safely: swaps and case-only changes go through temporary
names, so nothing is ever overwritten. Everything works through the generic
file system interface.
"""

from __future__ import annotations

import datetime as dt
import re
import sys
import uuid
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field

from orchard_commander import fs, metadata, vfs

CASES = ("unchanged", "lower", "upper", "first", "words")  # whole-name case conversion


class MetadataPending(Exception):
    """A [=field] placeholder needs metadata that hasn't been read yet."""


class MissingMetadata(Exception):
    """The file has no value for a [=field] placeholder (and the placeholder no fallback)."""


class MaskError(ValueError):
    """A mask can't be read; `position` is where in the mask."""

    def __init__(self, message: str, position: int) -> None:
        super().__init__(message)
        self.position = position


@dataclass(frozen=True)
class Rule:
    name_mask: str = "[N]"
    ext_mask: str = "[E]"
    search: str = ""
    replace: str = ""
    regex: bool = False
    case_sensitive: bool = False
    case: str = "unchanged"  # one of CASES
    counter_start: int = 1
    counter_step: int = 1
    counter_digits: int = 1


@dataclass(frozen=True)
class Item:
    """One file or folder to rename (only what the rule needs)."""

    location: vfs.VPath
    name: str
    is_dir: bool
    mtime: float
    parent: str = ""  # parent folder's name
    grandparent: str = ""
    # File metadata for [=field] placeholders: None until read (see metadata.read)
    meta: Mapping[str, str] | None = field(default=None, compare=False)
    meta_error: str = field(default="", compare=False)
    online_only: bool = False  # a cloud placeholder: reading metadata would download it


def item_for(location: vfs.VPath, entry: fs.Entry) -> Item:
    parent = location.parent()
    grand = parent.parent() if parent is not None else None
    # location.name, not entry.name: search results list entries by their relative path
    return Item(location, location.name, entry.is_dir, entry.mtime,
                parent.name if parent is not None else "", grand.name if grand is not None else "",
                online_only=entry.is_online_only)


def split_name(name: str, is_dir: bool) -> tuple[str, str]:
    """('report.final', 'pdf') for files; folders and dot-files have no extension."""
    if is_dir:
        return name, ""
    stem, dot, ext = name.rpartition(".")
    return (stem, ext) if dot and stem else (name, "")


# ---------------------------------------------------------------------------
# Masks
# ---------------------------------------------------------------------------

_RANGE = re.compile(r"(-?\d+)(?:(-|,)(-?\d+)?)?")
_COUNTER = re.compile(r"C(\d+)?(?:\+(-?\d+))?(?::(\d+))?")


def _slice(text: str, spec: str, position: int) -> str:
    """TC ranges: '2-5', '2,5', '2-', '-8,5', '-8-5', '2--5', '-5-', '3' (one char)."""
    if not spec:
        return text
    m = _RANGE.fullmatch(spec)
    if not m:
        raise MaskError(f"Unknown range “{spec}”", position)
    start, sep, end = int(m.group(1)), m.group(2), m.group(3)
    if start == 0:
        raise MaskError("Ranges start at 1", position)
    i = start - 1 if start > 0 else len(text) + start
    i = max(i, 0)
    if sep is None:
        return text[i : i + 1]
    if sep == ",":
        if end is None:
            raise MaskError(f"Missing length in “{spec}”", position)
        return text[i : i + max(int(end), 0)]
    if end is None:
        return text[i:]
    stop = int(end)
    if start < 0 and stop > 0:  # [N-8-5]: both counted from the end
        stop = -stop
    return text[i:stop] if stop > 0 else text[i : len(text) + stop + 1]


def _words_upper(text: str) -> str:
    return re.sub(r"[^\W_]+", lambda m: m.group(0)[:1].upper() + m.group(0)[1:].lower(), text)


_CASE_FUNCS: dict[str, Callable[[str], str]] = {
    "unchanged": lambda s: s,
    "lower": str.lower,
    "upper": str.upper,
    "first": lambda s: s[:1].upper() + s[1:].lower(),
    "words": _words_upper,
}
_SWITCHES = {"U": "upper", "L": "lower", "F": "words", "n": "unchanged"}


def expand(mask: str, item: Item, index: int, rule: Rule) -> str:
    """Apply one mask (name or extension) to `item`; `index` counts from 0 in the batch."""
    stem, ext = split_name(item.name, item.is_dir)
    when = dt.datetime.fromtimestamp(item.mtime) if item.mtime else dt.datetime.now()
    out: list[str] = []
    case = "unchanged"
    pos = 0
    while pos < len(mask):
        if mask[pos] != "[":
            if mask[pos] == "]":
                raise MaskError("“]” without “[” (write []] for a bracket)", pos)
            nxt = min((i for i in (mask.find("[", pos), mask.find("]", pos)) if i != -1), default=len(mask))
            out.append(_CASE_FUNCS[case](mask[pos:nxt]))
            pos = nxt
            continue
        if mask.startswith("[[]", pos) or mask.startswith("[]]", pos):
            out.append(mask[pos + 1])
            pos += 3
            continue
        close = mask.find("]", pos + 1)
        if close == -1:
            raise MaskError("“[” is never closed", pos)
        cmd = mask[pos + 1 : close]
        value = _placeholder(cmd, item, stem, ext, when, index, rule, pos)
        if value is None:  # a case switch
            case = _SWITCHES[cmd]
        else:
            out.append(_CASE_FUNCS[case](value))
        pos = close + 1
    return "".join(out)


_FIELD = re.compile(r"=(\w+)(?::([-,\d]*))?(?:\|(.*))?", re.DOTALL)


def _metadata_field(cmd: str, item: Item, pos: int) -> str:
    """[=artist], [=artist:1-3] (range as for [N]), [=artist|Unknown] (fallback when missing)."""
    m = _FIELD.fullmatch(cmd)
    if not m:
        raise MaskError(f"Unknown placeholder “[{cmd}]” (example: [=artist])", pos)
    name, spec, fallback = m.groups()
    if name not in metadata.all_fields():
        raise MaskError(f"Unknown field “{name}” (see Insert… for the list)", pos)
    if spec:
        _slice("", spec, pos)  # check the range syntax even before metadata arrives
    if item.meta is None:
        raise MetadataPending()
    value = item.meta.get(name, "")
    if not value:
        if fallback is None:
            raise MissingMetadata(item.meta_error or f"no {name}")
        return fallback
    return _slice(value, spec or "", pos)


def needs_metadata(rule: Rule) -> bool:
    return "[=" in rule.name_mask or "[=" in rule.ext_mask


def _placeholder(cmd: str, item: Item, stem: str, ext: str, when: dt.datetime, index: int, rule: Rule,
                 pos: int) -> str | None:
    if cmd in _SWITCHES:
        return None
    if cmd.startswith("="):
        return _metadata_field(cmd, item, pos)
    head, spec = cmd[:1], cmd[1:]
    if head == "N":
        return _slice(stem, spec, pos)
    if head == "E":
        return _slice(ext, spec, pos)
    if head == "P":
        return _slice(item.parent, spec, pos)
    if head == "G":
        return _slice(item.grandparent, spec, pos)
    if cmd and (cmd[0].isdigit() or cmd[0] == "-"):  # [2-5]: whole name with extension
        return _slice(item.name, cmd, pos)
    if head == "C":
        m = _COUNTER.fullmatch(cmd)
        if not m:
            raise MaskError(f"Unknown counter “[{cmd}]” (example: [C10+5:3])", pos)
        start = int(m.group(1)) if m.group(1) else rule.counter_start
        step = int(m.group(2)) if m.group(2) else rule.counter_step
        digits = int(m.group(3)) if m.group(3) else rule.counter_digits
        return str(start + index * step).zfill(max(digits, 1))
    simple = {
        "d": when.strftime("%Y-%m-%d"),
        "t": when.strftime("%H.%M.%S"),
        "Y": when.strftime("%Y"),
        "M": when.strftime("%m"),
        "D": when.strftime("%d"),
        "h": when.strftime("%H"),
        "m": when.strftime("%M"),
        "s": when.strftime("%S"),
    }
    if cmd in simple:
        return simple[cmd]
    raise MaskError(f"Unknown placeholder “[{cmd}]”", pos)


# ---------------------------------------------------------------------------
# Search & replace, case
# ---------------------------------------------------------------------------


def _pairs(rule: Rule) -> list[tuple[str, str]]:
    searches = rule.search.split("|")
    replaces = rule.replace.split("|")
    return [(s, replaces[i] if i < len(replaces) else (replaces[-1] if len(replaces) > 1 else replaces[0]))
            for i, s in enumerate(searches) if s]


def search_replace(name: str, rule: Rule) -> str:
    """Raises re.error for a bad regular expression."""
    if not rule.search:
        return name
    flags = 0 if rule.case_sensitive else re.IGNORECASE
    for search, replacement in _pairs(rule):
        if rule.regex:
            template = re.sub(r"\$(\d+)", r"\\\1", replacement)  # $1 like TC, \1 like Python
            name = re.sub(search, template, name, flags=flags)
        else:
            name = re.sub(re.escape(search), lambda _m, r=replacement: r, name, flags=flags)
    return name


def new_name(item: Item, index: int, rule: Rule) -> str:
    """The new name for `item` (raises MaskError / re.error for a bad rule)."""
    stem = expand(rule.name_mask, item, index, rule)
    ext = expand(rule.ext_mask, item, index, rule) if not item.is_dir else ""
    name = f"{stem}.{ext}" if ext else stem
    return _CASE_FUNCS[rule.case](search_replace(name, rule))


# ---------------------------------------------------------------------------
# Planning and executing
# ---------------------------------------------------------------------------

WINDOWS_BAD = set('<>:"/\\|?*')
WINDOWS_RESERVED = {"con", "prn", "aux", "nul", *(f"com{i}" for i in range(1, 10)), *(f"lpt{i}" for i in range(1, 10))}


def name_problem(name: str) -> str | None:
    if not name or name in (".", ".."):
        return "empty name"
    if "/" in name or "\0" in name:
        return "contains “/”"
    if sys.platform == "win32":
        if any(c in WINDOWS_BAD for c in name):
            return "contains a character Windows doesn't allow"
        if name.endswith((" ", ".")):
            return "ends with a space or dot"
        if name.split(".")[0].lower() in WINDOWS_RESERVED:
            return "reserved name on Windows"
    if len(name.encode("utf-8")) > 255:
        return "name too long"
    return None


@dataclass
class Change:
    item: Item
    new: str
    error: str = ""  # blocks the batch
    note: str = ""  # left unchanged, e.g. "skipped: no artist"
    pending: bool = False  # waiting for metadata

    @property
    def changed(self) -> bool:
        return self.new != self.item.name

    @property
    def ok(self) -> bool:
        return not self.error


@dataclass
class Plan:
    changes: list[Change]
    mask_error: str = ""  # the rule itself is broken

    @property
    def to_rename(self) -> list[Change]:
        return [c for c in self.changes if c.changed and c.ok]

    @property
    def problems(self) -> list[Change]:
        return [c for c in self.changes if not c.ok]

    @property
    def pending(self) -> int:
        return sum(c.pending for c in self.changes)

    @property
    def skipped(self) -> list[Change]:
        return [c for c in self.changes if c.note and not c.pending]

    @property
    def can_run(self) -> bool:
        return not self.mask_error and not self.problems and not self.pending and bool(self.to_rename)


def _folds_case(location: vfs.VPath) -> bool:
    """Whether names in this folder are case-insensitive (typical for macOS and Windows disks)."""
    return location.is_local and sys.platform in ("darwin", "win32")


def plan(items: list[Item], rule: Rule, *, exists: Callable[[vfs.VPath], bool] | None = None) -> Plan:
    """New names for `items` plus everything that would go wrong.

    `exists` checks whether a name is already taken (default: ask the file system).
    """
    exists = exists or (lambda location: location.exists())
    changes: list[Change] = []
    for index, item in enumerate(items):
        try:
            changes.append(Change(item, new_name(item, index, rule)))
        except MetadataPending:
            changes.append(Change(item, item.name, note="reading metadata…", pending=True))
        except MissingMetadata as exc:
            changes.append(Change(item, item.name, note=f"skipped: {exc}"))
        except MaskError as exc:
            return Plan([Change(i, i.name) for i in items], mask_error=f"{exc} (at position {exc.position + 1})")
        except re.error as exc:
            return Plan([Change(i, i.name) for i in items], mask_error=f"Search: {exc}")

    def key(location: vfs.VPath, name: str) -> tuple[object, str]:
        folder = location.parent()
        return (folder, name.casefold() if _folds_case(location) else name)

    # Names that are freed by this batch (unchanged items keep theirs).
    old_names = {key(c.item.location, c.item.name) for c in changes if c.changed}
    renamed_dirs = {c.item.location for c in changes if c.changed and c.item.is_dir}
    seen: dict[tuple[object, str], Change] = {}
    for change in changes:
        if not change.changed:
            continue
        problem = name_problem(change.new)
        if problem:
            change.error = problem
            continue
        outer = change.item.location.parent()
        while outer is not None and outer not in renamed_dirs:
            outer = outer.parent()
        if outer is not None:
            change.error = f"inside {outer.name}, which is renamed too (rename separately)"
            continue
        k = key(change.item.location, change.new)
        if k in seen:
            change.error = f"same new name as {seen[k].item.name}"
            seen[k].error = seen[k].error or f"same new name as {change.item.name}"
            continue
        seen[k] = change
        folder = change.item.location.parent()
        target = folder.child(change.new) if folder is not None else None
        same_file = key(change.item.location, change.item.name) == k  # case-only change
        if not same_file and k not in old_names and target is not None and exists(target):
            change.error = "a file with this name already exists"
    return Plan(changes)


@dataclass
class Done:
    """A finished batch, for undo: (folder, new name, old name) in rename order."""

    renames: list[tuple[vfs.VPath, str, str]] = field(default_factory=list)
    errors: list[str] = field(default_factory=list)


def execute(the_plan: Plan, *, on_progress: Callable[[int, int], None] | None = None) -> Done:
    """Rename everything in `the_plan` (which must be `can_run`).

    Two phases: first every file moves to a temporary name in its folder, then
    to its final name — so swaps (a→b, b→a) and case-only changes work and no
    file is overwritten. A file that fails keeps (or gets back) its old name.
    """
    done = Done()
    work = the_plan.to_rename
    tag = uuid.uuid4().hex[:8]
    staged: list[tuple[vfs.VPath, Change, str]] = []
    for i, change in enumerate(work):
        folder = change.item.location.parent()
        if folder is None:
            done.errors.append(f"{change.item.name}: no folder")
            continue
        temp = f".orchard-rename-{tag}-{i}"
        try:
            folder.fs.rename(change.item.location.path, folder.child(temp).path)
            staged.append((folder, change, temp))
        except (OSError, fs.OperationError) as exc:
            done.errors.append(f"{change.item.name}: {exc}")
    for n, (folder, change, temp) in enumerate(staged):
        try:
            folder.fs.rename(folder.child(temp).path, folder.child(change.new).path)
            done.renames.append((folder, change.new, change.item.name))
        except (OSError, fs.OperationError) as exc:
            done.errors.append(f"{change.item.name}: {exc}")
            try:  # put it back
                folder.fs.rename(folder.child(temp).path, change.item.location.path)
            except (OSError, fs.OperationError):
                done.errors.append(f"{change.item.name} is now called {temp}")
        if on_progress:
            on_progress(n + 1, len(staged))
    return done


def undo(done: Done) -> Done:
    """Reverse a finished batch (also two-phase). Returns what the undo did."""
    items = [Item(folder.child(new), new, False, 0.0) for folder, new, _ in done.renames]
    changes = [Change(item, old) for item, (_, _, old) in zip(items, done.renames)]
    return execute(Plan(changes))


def existing_names(items: list[Item]) -> Callable[[vfs.VPath], bool]:
    """An `exists` check for `plan()` that lists each folder once (cheap on servers).

    Blocking: call it on a worker thread.
    """
    taken: dict[vfs.VPath, set[str]] = {}
    for folder in {i.location.parent() for i in items}:
        if folder is None:
            continue
        try:
            names = [e.name for e in folder.list_dir(show_hidden=True)]
        except (OSError, fs.OperationError):
            continue  # fall back to asking per name
        taken[folder] = {n.casefold() for n in names} if _folds_case(folder) else set(names)

    def exists(location: vfs.VPath) -> bool:
        folder = location.parent()
        if folder not in taken:
            return location.exists()
        name = location.name
        return (name.casefold() if _folds_case(location) else name) in taken[folder]

    return exists


def renamed_locations(done: Done) -> dict[vfs.VPath, vfs.VPath]:
    """old location -> new location for a finished batch."""
    return {folder.child(old): folder.child(new) for folder, new, old in done.renames}


def rename_supported(location: vfs.VPath) -> bool:
    return location.fs.writable and location.fs.can_rename


__all__ = ["Rule", "Item", "Plan", "Change", "Done", "MaskError", "item_for", "plan", "execute", "undo",
           "new_name", "expand", "split_name", "CASES", "rename_supported", "existing_names", "needs_metadata",
           "renamed_locations"]
