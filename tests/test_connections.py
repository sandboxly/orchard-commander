from __future__ import annotations

from pathlib import Path

import pytest

from orchard_commander import connections, vfs
from orchard_commander.connections import Connection
from orchard_commander.fsproviders import ConnectContext


def ctx(confirm) -> ConnectContext:
    return ConnectContext(data_dir=connections.data_dir(), confirm=confirm)
from servers import PASSWORD, USER, start_sftp_server


@pytest.mark.parametrize(
    ("url", "expected"),
    [
        ("sftp://alice@example.com", dict(kind="sftp", host="example.com", username="alice", path="")),
        ("sftp://example.com:2222/var/log", dict(port=2222, path="/var/log", username="")),
        ("sftp://bob@host/~/projects/x", dict(path="projects/x")),
        ("smb://nas/media", dict(kind="smb", share="media", path="")),
        ("smb://me%40corp@nas.local:4455/data/a%20b/c", dict(username="me@corp", port=4455, share="data", path="a b/c")),
    ],
)
def test_url_parsing_round_trips(url: str, expected: dict):
    conn = Connection.from_url(url)
    for key, value in expected.items():
        actual = conn.kind if key == "kind" else conn.get(key)
        assert actual == value, key
    assert Connection.from_url(conn.url).url == conn.url


@pytest.mark.parametrize("bad", ["ftp://x", "sftp://", "smb://server", "/local/path"])
def test_bad_urls(bad: str):
    with pytest.raises(ValueError):
        Connection.from_url(bad)


def test_is_url():
    assert connections.is_url("SFTP://host") and connections.is_url(" smb://a/b")
    assert not connections.is_url("/home/me") and not connections.is_url("C:\\Users")


def test_accounts_identify_server_and_share():
    a = Connection.from_url("smb://u@nas/share1/dir")
    assert a.account == Connection.from_url("smb://u@nas:445/share1").account
    assert a.account != Connection.from_url("smb://u@nas/share2").account
    assert Connection.make("sftp", host="h", username="u", port=22).account == "sftp://u@h"


def test_saved_connections_and_passwords(memory_keyring):
    conn = Connection.from_url("sftp://alice@example.com/srv", name="Prod")
    connections.save(conn)
    connections.save(conn.with_changes(name="Production"))  # update, not duplicate
    [loaded] = connections.load_saved()
    assert loaded.name == "Production" and loaded.id == conn.id and loaded.get("path") == "/srv"
    assert connections.get_password(conn) is None
    assert connections.store_password(conn, "pw")
    assert connections.get_password(loaded) == "pw"
    assert "pw" not in connections._store_path().read_text()  # never in the JSON file
    connections.remove(loaded)
    assert connections.load_saved() == [] and connections.get_password(conn) is None


def test_sessions_connect_reuse_and_disconnect(tmp_path: Path):
    root = tmp_path / "remote"
    (root / "home" / USER / "work").mkdir(parents=True)
    server = start_sftp_server(root)
    sessions = connections.Sessions()
    try:
        conn = Connection.make("sftp", host=server.host, username=USER, port=server.port, path="/home/tester/work")
        location = sessions.connect(conn, PASSWORD, ctx(lambda *q: True))
        assert location.path == "/home/tester/work" and location.is_dir()
        again = sessions.connect(conn.with_params(path="/"), None, ctx(lambda *q: pytest.fail("asked")))
        assert again.fs is location.fs and again.path == "/"  # reused, no new login
        nested = vfs.VPath(location.fs, "/")
        assert connections.root_filesystem(nested) is location.fs
        sessions.disconnect(location.fs)
        assert sessions.find(conn) is None
    finally:
        sessions.close_all()
        server.stop()


def test_local_network_hint_on_macos(monkeypatch):
    import errno
    import sys

    from orchard_commander import remote

    unreachable = OSError(errno.EHOSTUNREACH, "No route to host")
    wrapped = ValueError("Failed to connect to '192.168.1.20:445': [Errno 65] No route to host")
    monkeypatch.setattr(sys, "platform", "darwin")
    assert "Local Network" in remote.describe_connect_error("192.168.1.20", 445, unreachable)
    assert "Local Network" in remote.describe_connect_error("192.168.1.20", 445, wrapped)
    assert "Local Network" not in remote.describe_connect_error("8.8.8.8", 445, unreachable)
    assert "Local Network" not in remote.describe_connect_error("192.168.1.2", 445, ConnectionRefusedError(61, "x"))
    monkeypatch.setattr(sys, "platform", "linux")
    assert "Local Network" not in remote.describe_connect_error("192.168.1.20", 445, unreachable)


def test_password_saved_before_rename_is_moved(memory_keyring):
    conn = Connection.from_url("smb://alice@nas/Media")
    memory_keyring.set_password(connections.LEGACY_KEYRING_SERVICE, conn.account, "old-pw")
    assert connections.get_password(conn) == "old-pw"
    assert memory_keyring.data == {(connections.KEYRING_SERVICE, conn.account): "old-pw"}


def test_legacy_config_directory_is_migrated(tmp_path, monkeypatch):
    from orchard_commander import config

    legacy = tmp_path / "fcmd"
    legacy.mkdir()
    (legacy / "settings.json").write_text('{"theme": "nord"}')
    (legacy / "connections.json").write_text("[]")
    (legacy / "known_hosts").write_text("host key")
    monkeypatch.setattr(config, "LEGACY_CONFIG_DIR", legacy)
    monkeypatch.setattr(config, "CONFIG_FILE", tmp_path / "Orchard Commander" / "settings.json")
    assert config.migrate_legacy_config()
    assert config.Settings.load().theme == "nord"
    assert (tmp_path / "Orchard Commander" / "known_hosts").read_text() == "host key"
    assert legacy.exists()  # kept as a backup
    assert not config.migrate_legacy_config()  # only once
