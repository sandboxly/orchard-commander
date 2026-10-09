"""Discoverable connection types (FileSystemProvider) and provider-agnostic connections."""

from __future__ import annotations

import json

import pytest

from orchard_commander import connections, fsproviders, vfs
from orchard_commander.connections import Connection
from orchard_commander.fsproviders import (
    AuthenticationRequired,
    ConnectContext,
    Field,
    FileSystemProvider,
    join_server_url,
    split_server_url,
)
from test_vfs import MemoryFS


class MemProvider(FileSystemProvider):
    """An add-on connection type: mem://[user@]bucket/path, password "letmein"."""

    scheme = "mem"
    title = "In-memory test storage"
    fields = (Field("host", "Bucket", required=True), Field("username", "User"), Field("path", "Start folder"))
    secret_label = "Access key"
    store: dict[str, MemoryFS] = {}

    def parse_url(self, url):
        params, segments = split_server_url(url, self.scheme)
        if segments:
            params["path"] = "/".join(segments)
        return params

    def format_url(self, params):
        path = str(params.get("path") or "").strip("/")
        return join_server_url(self.scheme, params, None, f"/{path}" if path else "")

    def connect(self, params, secret, ctx):
        if secret != "letmein":
            raise AuthenticationRequired("wrong access key")
        if not ctx.confirm("New bucket", f"Use bucket {params['host']}?", "Use"):
            raise fsproviders.ConnectionCancelled("declined")
        filesystem = self.store.setdefault(str(params["host"]), MemoryFS())
        filesystem.mkdir("projects", exist_ok=True)
        return filesystem

    def start_path(self, params, filesystem):
        return str(params.get("path") or "")


class Unavailable(MemProvider):
    scheme = "nope"

    @classmethod
    def available(cls):
        return False


@pytest.fixture
def clean_registry(monkeypatch):
    monkeypatch.setattr(fsproviders, "_providers", {})
    monkeypatch.setattr(fsproviders, "_initialized", False)
    return fsproviders


@pytest.fixture
def with_mem_addon(clean_registry, monkeypatch):
    class EP:
        def __init__(self, name, target):
            self.name, self.value, self._target = name, f"addon:{name}", target

        def load(self):
            if isinstance(self._target, Exception):
                raise self._target
            return self._target

    eps = [EP("mem", MemProvider), EP("broken", ImportError("boto3 missing")), EP("nope", Unavailable)]
    monkeypatch.setattr(fsproviders, "entry_points", lambda group: eps if group == fsproviders.ENTRY_POINT_GROUP else [])
    MemProvider.store = {}
    return clean_registry


def ctx(answer: bool = True, asked: list | None = None) -> ConnectContext:
    def confirm(title, message, ok):
        if asked is not None:
            asked.append(title)
        return answer

    return ConnectContext(data_dir=connections.data_dir(), confirm=confirm)


def test_builtins_registered(clean_registry):
    assert [p.scheme for p in clean_registry.providers()] == ["sftp", "smb", "webdavs", "webdav"]
    assert clean_registry.scheme_of("SMB://nas/x") == "smb"
    assert clean_registry.scheme_of("ftp://x") is None and clean_registry.scheme_of("/local") is None


def test_entry_point_addon_is_discovered(with_mem_addon, caplog):
    schemes = [p.scheme for p in with_mem_addon.providers()]
    assert schemes == ["sftp", "smb", "webdavs", "webdav", "mem"]  # broken and unavailable add-ons skipped
    assert "could not load file system provider broken" in caplog.text
    assert connections.is_url("mem://bucket/projects")


def test_addon_connection_end_to_end(with_mem_addon):
    conn = Connection.from_url("mem://alice@photos/projects", name="Photos")
    assert conn.kind == "mem" and conn.get("host") == "photos" and conn.url == "mem://alice@photos/projects"
    assert conn.account == "mem://alice@photos" and conn.description == "alice@photos"

    sessions = connections.Sessions()
    with pytest.raises(AuthenticationRequired):
        sessions.connect(conn, "wrong", ctx())
    with pytest.raises(fsproviders.ConnectionCancelled):
        sessions.connect(conn, "letmein", ctx(answer=False))
    asked: list = []
    location = sessions.connect(conn, "letmein", ctx(asked=asked))
    assert asked == ["New bucket"] and location.path == "projects" and location.is_dir()
    again = sessions.connect(conn.with_params(path=""), None, ctx(asked=asked))  # session reused
    assert again.fs is location.fs and asked == ["New bucket"]

    vfs.mkdir(location, "2026")  # it's a regular file system from here on
    assert [e.name for e in location.list_dir()] == ["2026"]


def test_provider_validation(with_mem_addon):
    provider = with_mem_addon.get("mem")
    assert provider.validate({}) == "Enter bucket."
    assert provider.validate({"host": "b"}) is None
    sftp = with_mem_addon.get("sftp")
    assert sftp.validate({"host": "h", "port": "22a"}) == "Port must be a number."


def test_saved_connections_old_format_migrated(clean_registry):
    old = [
        {"kind": "smb", "host": "192.168.1.20", "username": "alice", "port": None, "share": "Media",
         "path": "", "key_file": "", "name": "", "id": "a1"},
        {"kind": "sftp", "host": "203.0.113.10", "username": "root", "port": None, "share": "",
         "path": "", "key_file": "/Users/me/.ssh/id_ed25519", "name": "Web server", "id": "b2"},
    ]
    connections._store_path().parent.mkdir(parents=True, exist_ok=True)
    connections._store_path().write_text(json.dumps(old))
    smb, sftp = connections.load_saved()
    assert smb.params == {"host": "192.168.1.20", "username": "alice", "share": "Media"}
    assert smb.url == "smb://alice@192.168.1.20/Media" and smb.id == "a1"
    assert sftp.get("key_file") == "/Users/me/.ssh/id_ed25519" and sftp.name == "Web server"
    assert sftp.account == "sftp://root@203.0.113.10"  # same key as before: stored passwords still match
    connections.save(smb)  # written back in the new format
    assert json.loads(connections._store_path().read_text())[1]["params"]["share"] == "Media"


def test_connection_of_uninstalled_addon_is_kept(clean_registry):
    connections.save(Connection("ftp", {"host": "ftp.example.com"}, name="Old add-on"))
    [conn] = connections.load_saved()
    assert conn.label == "Old add-on" and conn.url == "ftp://"
    with pytest.raises(ValueError, match="add-on not installed"):
        connections.Sessions().connect(conn, None, ctx())
