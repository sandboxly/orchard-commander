"""Location bookmarks, after Total Commander's Directory Hotlist.

A bookmark has a title (an "&" marks its hotkey letter, as in TC), a location
for the active panel and optionally one for the other panel (TC's "target").
Bookmarks can be grouped in folders (TC's submenus) and separated by lines.
Locations are `Spec`s (see `locations.py`), so bookmarks can point into
archives and onto servers, not just local folders.

Stored as JSON in the config directory (`bookmarks.json`).
"""

from __future__ import annotations

import json
import uuid
from collections.abc import Iterator
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Union

from orchard_commander import config
from orchard_commander.locations import Spec, same_location

FORMAT_VERSION = 1


@dataclass
class Bookmark:
    title: str
    location: Spec
    target: Spec | None = None  # opened in the other panel at the same time
    id: str = field(default_factory=lambda: uuid.uuid4().hex)


@dataclass
class Folder:
    title: str
    items: list[Item] = field(default_factory=list)
    id: str = field(default_factory=lambda: uuid.uuid4().hex)


@dataclass
class Separator:
    id: str = field(default_factory=lambda: uuid.uuid4().hex)


Item = Union[Bookmark, Folder, Separator]


# ---------------------------------------------------------------------------
# Titles and hotkeys ("&Projects": P is the hotkey, "&&" is a literal "&")
# ---------------------------------------------------------------------------


def hotkey(title: str) -> str | None:
    """The hotkey letter of a title (lower case), if it has one."""
    i = 0
    while i < len(title) - 1:
        if title[i] == "&":
            if title[i + 1] == "&":
                i += 2
                continue
            return title[i + 1].lower()
        i += 1
    return None


def plain(title: str) -> str:
    """The title as shown: without hotkey markers."""
    out, i = [], 0
    while i < len(title):
        if title[i] == "&" and i + 1 < len(title):
            out.append(title[i + 1])
            i += 2
            continue
        out.append(title[i])
        i += 1
    return "".join(out)


# ---------------------------------------------------------------------------
# Tree helpers
# ---------------------------------------------------------------------------


def walk(items: list[Item], trail: tuple[str, ...] = ()) -> Iterator[tuple[tuple[str, ...], Item]]:
    """Every item with the folder titles leading to it."""
    for item in items:
        yield trail, item
        if isinstance(item, Folder):
            yield from walk(item.items, (*trail, plain(item.title)))


def bookmarks(items: list[Item]) -> list[tuple[tuple[str, ...], Bookmark]]:
    return [(trail, item) for trail, item in walk(items) if isinstance(item, Bookmark)]


def folders(items: list[Item]) -> list[tuple[tuple[str, ...], Folder]]:
    return [(trail, item) for trail, item in walk(items) if isinstance(item, Folder)]


def find_location(items: list[Item], spec: Spec) -> Bookmark | None:
    """The first bookmark whose (active panel) location is `spec`."""
    return next((b for _, b in bookmarks(items) if same_location(b.location, spec)), None)


def remove(items: list[Item], item_id: str) -> bool:
    """Remove the item (anywhere in the tree) with this id."""
    for i, item in enumerate(items):
        if item.id == item_id:
            del items[i]
            return True
        if isinstance(item, Folder) and remove(item.items, item_id):
            return True
    return False


def container_of(items: list[Item], folder_id: str | None) -> list[Item]:
    """The item list of a folder (by id); the top level for None or unknown ids."""
    if folder_id is not None:
        for _, folder in folders(items):
            if folder.id == folder_id:
                return folder.items
    return items


# ---------------------------------------------------------------------------
# Storage
# ---------------------------------------------------------------------------


def _store_path() -> Path:
    return config.CONFIG_FILE.parent / "bookmarks.json"


def _to_json(item: Item) -> dict[str, Any]:
    if isinstance(item, Bookmark):
        data: dict[str, Any] = {"type": "bookmark", "id": item.id, "title": item.title, "location": item.location}
        if item.target is not None:
            data["target"] = item.target
        return data
    if isinstance(item, Folder):
        return {"type": "folder", "id": item.id, "title": item.title, "items": [_to_json(i) for i in item.items]}
    return {"type": "separator", "id": item.id}


def _from_json(data: Any) -> Item | None:
    if not isinstance(data, dict):
        return None
    kind, item_id = data.get("type"), data.get("id") or uuid.uuid4().hex
    if kind == "bookmark" and isinstance(data.get("location"), dict):
        target = data.get("target") if isinstance(data.get("target"), dict) else None
        return Bookmark(str(data.get("title", "")), data["location"], target, item_id)
    if kind == "folder":
        children = [c for c in (_from_json(d) for d in data.get("items", [])) if c is not None]
        return Folder(str(data.get("title", "")), children, item_id)
    if kind == "separator":
        return Separator(item_id)
    return None


def load() -> list[Item]:
    try:
        data = json.loads(_store_path().read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []
    items = data.get("items", []) if isinstance(data, dict) else []
    return [i for i in (_from_json(d) for d in items) if i is not None]


def save(items: list[Item]) -> None:
    path = _store_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    data = {"version": FORMAT_VERSION, "items": [_to_json(i) for i in items]}
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(data, indent=2, ensure_ascii=False), encoding="utf-8")
    tmp.replace(path)  # never leave a half-written file behind
