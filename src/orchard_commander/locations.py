"""Serializable descriptions of places ("specs"), for bookmarks and the like.

A live location is a `vfs.VPath`, which holds open file systems and can't be
saved. A spec describes how to get there again:

    {"kind": "local", "path": "/Users/me/Projects"}
    {"kind": "server", "account": "sftp://root@example.com", "path": "/var/www"}
    {"kind": "archive", "archive": <spec of the archive file>, "path": "docs/2026"}

`account` is the connection's account (see `connections.Connection.account`),
so a saved connection with its keychain password is found again, and any
connection type (including add-ons) works. Archives nest: the archive file's
spec may itself be inside another archive or on a server.
"""

from __future__ import annotations

import os
import posixpath
from collections.abc import Callable
from pathlib import Path
from typing import Any

from orchard_commander import vfs

type Spec = dict[str, Any]


def spec_of(location: vfs.VPath, account_of: Callable[[vfs.FileSystem], str | None]) -> Spec | None:
    """A spec for `location`, or None if it can't be described (e.g. a server
    connection that `account_of` doesn't know)."""
    filesystem = location.fs
    if isinstance(filesystem, vfs.ArchiveFS) and filesystem.container is not None:
        inner = spec_of(filesystem.container, account_of)
        return None if inner is None else {"kind": "archive", "archive": inner, "path": location.path}
    if location.is_local:
        return {"kind": "local", "path": location.path}
    account = account_of(filesystem)
    return None if account is None else {"kind": "server", "account": account, "path": location.path}


def describe(spec: Spec) -> str:
    """Human-readable: ~/Projects, sftp://root@host/var/www, ~/a.zip/docs."""
    kind = spec.get("kind")
    if kind == "local":
        text = str(spec.get("path", ""))
        home = str(Path.home())
        return "~" + text[len(home):] if text == home or text.startswith(home + os.sep) else text
    if kind == "server":
        path = str(spec.get("path", "")).rstrip("/")
        return str(spec.get("account", "")) + (path if path.startswith("/") else f"/{path}" if path else "")
    if kind == "archive":
        inner = str(spec.get("path", "")).strip("/")
        return describe(spec.get("archive") or {}) + (f"/{inner}" if inner else "")
    return "?"


def name_of(spec: Spec) -> str:
    """A default title: the folder's (or archive's) name."""
    kind = spec.get("kind")
    path = str(spec.get("path", ""))
    if kind == "local":
        return Path(path).name or path
    if kind == "archive":
        return posixpath.basename(path.strip("/")) or name_of(spec.get("archive") or {})
    if kind == "server":
        return posixpath.basename(path.rstrip("/")) or str(spec.get("account", "")).split("://", 1)[-1]
    return "?"


def _normal(spec: Spec | None) -> Any:
    if not spec:
        return None
    kind = spec.get("kind")
    if kind == "local":
        return ("local", os.path.normcase(os.path.normpath(str(spec.get("path", "")))))
    if kind == "server":
        return ("server", str(spec.get("account", "")).lower(), posixpath.normpath("/" + str(spec.get("path", ""))))
    if kind == "archive":
        return ("archive", _normal(spec.get("archive")), str(spec.get("path", "")).strip("/"))
    return ("?", repr(sorted(spec.items())))


def same_location(a: Spec | None, b: Spec | None) -> bool:
    return a is not None and b is not None and _normal(a) == _normal(b)


def is_valid(spec: Any) -> bool:
    if not isinstance(spec, dict):
        return False
    kind = spec.get("kind")
    if kind == "local":
        return isinstance(spec.get("path"), str)
    if kind == "server":
        return isinstance(spec.get("account"), str) and "://" in spec["account"]
    if kind == "archive":
        return is_valid(spec.get("archive")) and isinstance(spec.get("path"), str)
    return False
