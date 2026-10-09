"""Server connections: definitions, persistence, passwords, live sessions.

A `Connection` describes how to reach a server (no secrets): a connection
type (a `fsproviders.FileSystemProvider`, e.g. SFTP) and its params. Saved
connections live in `connections.json` in the config directory; passwords
go to the OS credential store (macOS Keychain, Windows Credential Manager,
Secret Service on Linux) via `keyring`, and only if the user asks for it.
`Sessions` keeps one open file system per server/account and reuses it.
"""

from __future__ import annotations

import json
import threading
import uuid
from dataclasses import asdict, dataclass, field, replace
from pathlib import Path
from typing import Any

from orchard_commander import config, fsproviders, vfs
from orchard_commander.fsproviders import ConnectContext, FileSystemProvider

KEYRING_SERVICE = "Orchard Commander"
LEGACY_KEYRING_SERVICE = "fcmd"  # passwords saved before the rename


@dataclass(frozen=True)
class Connection:
    """How to reach a server: a provider type plus that provider's params (no secrets)."""

    kind: str  # provider scheme, e.g. "sftp"
    params: dict[str, Any] = field(default_factory=dict)
    name: str = ""
    id: str = field(default_factory=lambda: uuid.uuid4().hex)

    @classmethod
    def make(cls, kind: str, *, name: str = "", **params: Any) -> Connection:
        return cls(kind, {k: v for k, v in params.items() if v not in (None, "")}, name)

    @classmethod
    def from_url(cls, url: str, *, name: str = "") -> Connection:
        """Parse an address of any registered provider. Raises ValueError."""
        scheme = fsproviders.scheme_of(url)
        if scheme is None:
            known = ", ".join(f"{p.scheme}://" for p in fsproviders.providers())
            raise ValueError(f"Unsupported address (use {known}): {url}")
        provider = fsproviders.get(scheme)
        assert provider is not None
        return cls(scheme, provider.parse_url(url), name)

    @property
    def provider(self) -> FileSystemProvider:
        provider = fsproviders.get(self.kind)
        if provider is None:
            raise ValueError(f"No support for {self.kind}:// connections (add-on not installed?)")
        return provider

    def get(self, key: str, default: Any = "") -> Any:
        return self.params.get(key, default)

    @property
    def account(self) -> str:
        """Identifies the server account (shared sessions, stored secret)."""
        provider = fsproviders.get(self.kind)
        return provider.account(self.params) if provider else f"{self.kind}://{self.id}"

    @property
    def url(self) -> str:
        provider = fsproviders.get(self.kind)
        return provider.format_url(self.params) if provider else f"{self.kind}://"

    @property
    def label(self) -> str:
        return self.name or self.url

    @property
    def description(self) -> str:
        provider = fsproviders.get(self.kind)
        return provider.describe(self.params) if provider else self.label

    def with_params(self, **changes: Any) -> Connection:
        params = {**self.params, **changes}
        return replace(self, params={k: v for k, v in params.items() if v not in (None, "")})

    def with_changes(self, **changes: Any) -> Connection:
        return replace(self, **changes)


def is_url(text: str) -> bool:
    """Whether `text` is an address of a registered connection type."""
    return fsproviders.scheme_of(text) is not None


# ---------------------------------------------------------------------------
# Saved connections
# ---------------------------------------------------------------------------


def _store_path() -> Path:
    return config.CONFIG_FILE.parent / "connections.json"


def load_saved() -> list[Connection]:
    try:
        data = json.loads(_store_path().read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []
    result = []
    for item in data if isinstance(data, list) else []:
        if not isinstance(item, dict) or "kind" not in item:
            continue
        params = item.get("params")
        if not isinstance(params, dict):  # saved before providers: params were top-level fields
            params = {k: v for k, v in item.items() if k not in ("kind", "name", "id") and v not in ("", None)}
        result.append(Connection(item["kind"], params, item.get("name", ""), item.get("id") or uuid.uuid4().hex))
    return result


def save_all(items: list[Connection]) -> None:
    path = _store_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps([asdict(c) for c in items], indent=2), encoding="utf-8")


def save(conn: Connection) -> None:
    """Add or update (by id)."""
    items = [c for c in load_saved() if c.id != conn.id]
    items.append(conn)
    save_all(items)


def remove(conn: Connection) -> None:
    save_all([c for c in load_saved() if c.id != conn.id])
    forget_password(conn)


# ---------------------------------------------------------------------------
# Passwords (OS credential store; every failure means "no stored password")
# ---------------------------------------------------------------------------


def get_password(conn: Connection) -> str | None:
    try:
        import keyring

        password = keyring.get_password(KEYRING_SERVICE, conn.account)
        if password is None:  # saved under the old name? move it over
            password = keyring.get_password(LEGACY_KEYRING_SERVICE, conn.account)
            if password is not None:
                keyring.set_password(KEYRING_SERVICE, conn.account, password)
                keyring.delete_password(LEGACY_KEYRING_SERVICE, conn.account)
        return password
    except Exception:
        return None


def store_password(conn: Connection, password: str) -> bool:
    try:
        import keyring

        keyring.set_password(KEYRING_SERVICE, conn.account, password)
        return True
    except Exception:
        return False


def forget_password(conn: Connection) -> None:
    try:
        import keyring

        keyring.delete_password(KEYRING_SERVICE, conn.account)
    except Exception:
        pass


# ---------------------------------------------------------------------------
# Live sessions
# ---------------------------------------------------------------------------


def data_dir() -> Path:
    """Where connection providers keep their files (e.g. SFTP's trusted host keys)."""
    return config.CONFIG_FILE.parent


class Sessions:
    """Open file systems, one per server account, reused across panels."""

    def __init__(self) -> None:
        self._open: dict[str, vfs.FileSystem] = {}
        self._lock = threading.Lock()

    def find(self, conn: Connection) -> vfs.FileSystem | None:
        with self._lock:
            filesystem = self._open.get(conn.account)
        if filesystem is not None and conn.provider.is_connected(filesystem):
            return filesystem
        return None

    def connect(self, conn: Connection, secret: str | None, ctx: ConnectContext) -> vfs.VPath:
        """Open (or reuse) a session and return the start directory. Blocking.

        Raises fsproviders.AuthenticationRequired, ValueError (unknown type), or
        whatever the provider raises (ConnectionError, OSError, …).
        """
        provider = conn.provider
        filesystem = self.find(conn)
        if filesystem is None:
            filesystem = provider.connect(conn.params, secret, ctx)
            with self._lock:
                old = self._open.pop(conn.account, None)
                self._open[conn.account] = filesystem
            if old is not None:
                old.close()
        return vfs.VPath(filesystem, provider.start_path(conn.params, filesystem))

    def owner_of(self, filesystem: vfs.FileSystem) -> str | None:
        with self._lock:
            return next((account for account, f in self._open.items() if f is filesystem), None)

    def disconnect(self, filesystem: vfs.FileSystem) -> None:
        with self._lock:
            account = next((a for a, f in self._open.items() if f is filesystem), None)
            if account is not None:
                del self._open[account]
        filesystem.close()

    def close_all(self) -> None:
        with self._lock:
            items = list(self._open.values())
            self._open.clear()
        for filesystem in items:
            filesystem.close()


def root_filesystem(location: vfs.VPath) -> vfs.FileSystem:
    """The outermost file system (e.g. the server a nested archive lives on)."""
    filesystem = location.fs
    while filesystem.container is not None:
        filesystem = filesystem.container.fs
    return filesystem
