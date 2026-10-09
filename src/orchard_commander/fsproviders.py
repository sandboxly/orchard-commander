"""Connection types for file systems: SFTP, SMB, and whatever add-ons provide.

A `FileSystemProvider` describes one kind of connection: its address scheme
(`sftp://`), the fields of its connection dialog, how to parse and format
addresses, and how to connect. Connecting returns a `vfs.FileSystem`; from
then on the rest of the app only uses that generic interface.

To add a connection type, subclass `FileSystemProvider` and register it —
built-ins in `_builtin_providers()`, add-ons through the
`orchard_commander.filesystems` entry point:

    class WebDavProvider(FileSystemProvider):
        scheme = "webdav"
        title = "WebDAV"
        fields = (Field("host", "Server", required=True), Field("username", "User"),
                  Field("path", "Start folder"))

        def parse_url(self, url): ...            # webdav://user@host/path -> params
        def format_url(self, params): ...
        def connect(self, params, secret, ctx):  # worker thread; may call ctx.confirm()
            return WebDavFileSystem(...)
"""

from __future__ import annotations

import logging
import threading
from abc import ABC, abstractmethod
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from importlib.metadata import entry_points
from pathlib import Path
from typing import ClassVar
from urllib.parse import quote, unquote, urlsplit

from orchard_commander import vfs

ENTRY_POINT_GROUP = "orchard_commander.filesystems"

log = logging.getLogger(__name__)

type Params = Mapping[str, str | int]


class AuthenticationRequired(Exception):
    """The server rejected the credentials (or needs a secret we don't have)."""


class ConnectionCancelled(Exception):
    """The user declined a question while connecting (e.g. didn't trust a host key)."""


@dataclass(frozen=True, slots=True)
class Field:
    """One input in the connection dialog. Values are stored as connection params."""

    key: str
    label: str
    kind: str = "text"  # "text" | "int" | "path" (a local file, e.g. a key)
    required: bool = False
    placeholder: str = ""
    default: str | int | None = None


@dataclass(frozen=True, slots=True)
class ConnectContext:
    """What a provider may use while connecting (on a worker thread)."""

    data_dir: Path  # where the provider may keep files (e.g. trusted host keys)
    confirm: Callable[[str, str, str], bool]  # (title, message, ok label) -> user agreed; Cancel is the default


class FileSystemProvider(ABC):
    scheme: ClassVar[str]  # address scheme, also the stored connection type
    title: ClassVar[str]  # shown in the connection dialog
    fields: ClassVar[tuple[Field, ...]] = ()
    secret_label: ClassVar[str] = "Password"  # the optional secret (stored in the keychain)
    secret_hint: ClassVar[str] = "empty: ask when connecting"

    @classmethod
    def available(cls) -> bool:
        """False if an optional dependency is missing; the provider is then skipped."""
        return True

    # -- addresses -------------------------------------------------------------------

    @abstractmethod
    def parse_url(self, url: str) -> dict[str, str | int]:
        """`scheme://…` -> params. Raises ValueError for malformed addresses."""

    @abstractmethod
    def format_url(self, params: Params) -> str: ...

    def account(self, params: Params) -> str:
        """Identifies the server account: which sessions are shared and under which
        name the secret is stored. Default: the address without the start folder."""
        return self.format_url({**params, "path": ""})

    def describe(self, params: Params) -> str:
        """Short human description, e.g. for password prompts: "root@server"."""
        user, host = params.get("username"), params.get("host", "")
        return f"{user}@{host}" if user else str(host)

    def validate(self, params: Params) -> str | None:
        """An error message for the connection dialog, or None if the params are fine."""
        for f in self.fields:
            if f.required and not str(params.get(f.key, "")).strip():
                return f"Enter {f.label.lower()}."
            if f.kind == "int" and params.get(f.key) not in (None, "") and not str(params[f.key]).isdigit():
                return f"{f.label} must be a number."
        return None

    # -- connecting --------------------------------------------------------------------

    @abstractmethod
    def connect(self, params: Params, secret: str | None, ctx: ConnectContext) -> vfs.FileSystem:
        """Open a file system (worker thread). Raises AuthenticationRequired if a
        (different) secret is needed, ConnectionError/OSError otherwise."""

    def start_path(self, params: Params, filesystem: vfs.FileSystem) -> str:
        """Where to start browsing after connecting."""
        return str(params.get("path") or "/")

    def is_connected(self, filesystem: vfs.FileSystem) -> bool:
        """Whether an open file system can still be used (else it's reconnected)."""
        return bool(getattr(filesystem, "connected", True))


# ---------------------------------------------------------------------------
# Helpers for typical server URLs: scheme://[user@]host[:port]/path
# ---------------------------------------------------------------------------


def split_server_url(url: str, scheme: str) -> tuple[dict[str, str | int], list[str]]:
    """Parse the common parts; returns (params, path segments)."""
    parts = urlsplit(url.strip())
    if parts.scheme.lower() != scheme:
        raise ValueError(f"Not a {scheme}:// address: {url}")
    if not parts.hostname:
        raise ValueError(f"No server in address: {url}")
    params: dict[str, str | int] = {"host": parts.hostname}
    if parts.username:
        params["username"] = unquote(parts.username)
    if parts.port:
        params["port"] = parts.port
    return params, [unquote(s) for s in parts.path.split("/") if s]


def join_server_url(scheme: str, params: Params, default_port: int | None, path: str = "") -> str:
    user = f"{quote(str(params['username']))}@" if params.get("username") else ""
    port = params.get("port")
    port_part = f":{port}" if port and int(port) != default_port else ""
    return f"{scheme}://{user}{params.get('host', '')}{port_part}{path}"


# ---------------------------------------------------------------------------
# Registry
# ---------------------------------------------------------------------------

_lock = threading.Lock()
_providers: dict[str, FileSystemProvider] = {}
_initialized = False


def _builtin_providers() -> list[type[FileSystemProvider]]:
    from orchard_commander.remote.providers import SFTPProvider, SMBProvider

    return [SFTPProvider, SMBProvider]


def register(provider: FileSystemProvider | type[FileSystemProvider], *, replace: bool = False) -> None:
    instance = provider() if isinstance(provider, type) else provider
    if not type(instance).available():
        log.info("file system provider %s unavailable (missing dependency?)", instance.scheme)
        return
    with _lock:
        if instance.scheme in _providers and not replace:
            raise ValueError(f"A provider for {instance.scheme}:// is already registered")
        _providers[instance.scheme] = instance


def unregister(scheme: str) -> None:
    with _lock:
        _providers.pop(scheme, None)


def _ensure_initialized() -> None:
    global _initialized
    with _lock:
        if _initialized:
            return
        _initialized = True
    for cls in _builtin_providers():
        register(cls)
    for ep in entry_points(group=ENTRY_POINT_GROUP):
        try:
            register(ep.load(), replace=True)  # add-ons may override built-ins
        except Exception:  # a broken add-on must not break the app
            log.exception("could not load file system provider %s from %s", ep.name, ep.value)


def providers() -> list[FileSystemProvider]:
    _ensure_initialized()
    with _lock:
        return list(_providers.values())


def get(scheme: str) -> FileSystemProvider | None:
    _ensure_initialized()
    with _lock:
        return _providers.get(scheme.lower())


def scheme_of(url: str) -> str | None:
    """The registered scheme `url` starts with, if any."""
    head, sep, _ = url.strip().partition("://")
    return head.lower() if sep and get(head) is not None else None
