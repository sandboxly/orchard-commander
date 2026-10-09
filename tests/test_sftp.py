from __future__ import annotations

from pathlib import Path

import pytest

from orchard_commander import fs, vfs
from orchard_commander.remote import AuthenticationRequired, HostKeyChanged, HostKeyRejected, sftp
from servers import PASSWORD, USER, start_sftp_server


def never(src, dst):
    raise AssertionError(f"unexpected conflict {src} -> {dst}")


@pytest.fixture
def server(tmp_path: Path):
    root = tmp_path / "remote"
    (root / "docs").mkdir(parents=True)
    (root / "docs" / "readme.txt").write_text("hello from sftp")
    (root / "big.bin").write_bytes(bytes(range(256)) * 4096)  # 1 MiB
    (root / ".profile").write_text("x")
    srv = start_sftp_server(root)
    yield srv
    srv.stop()


@pytest.fixture
def known_hosts(tmp_path: Path) -> Path:
    return tmp_path / "known_hosts"


def connect(server, known_hosts, *, password=PASSWORD, confirm=lambda q: True) -> sftp.SFTPFileSystem:
    return sftp.connect(
        server.host, port=server.port, username=USER, password=password,
        known_hosts=known_hosts, confirm_host_key=confirm,
    )


def test_unknown_host_key_is_confirmed_and_remembered(server, known_hosts):
    asked = []
    remote = connect(server, known_hosts, confirm=lambda q: asked.append(q) or True)
    remote.close()
    assert asked[0].fingerprint == server.host_key_fingerprint
    assert asked[0].port == server.port
    # Second time: known, no question.
    connect(server, known_hosts, confirm=lambda q: pytest.fail("asked again")).close()


def test_rejected_and_changed_host_keys(server, known_hosts):
    with pytest.raises(HostKeyRejected):
        connect(server, known_hosts, confirm=lambda q: False)
    # Pretend we know a different key for this host.
    import paramiko

    keys = paramiko.HostKeys()
    keys.add(f"[{server.host}]:{server.port}", "ssh-ed25519", paramiko.Ed25519Key.from_private_key(
        __import__("io").StringIO(_other_key())))
    keys.save(str(known_hosts))
    with pytest.raises(HostKeyChanged):
        connect(server, known_hosts)


def _other_key() -> str:
    import asyncssh

    return asyncssh.generate_private_key("ssh-ed25519").export_private_key("openssh").decode()


def test_wrong_password(server, known_hosts):
    with pytest.raises(AuthenticationRequired):
        connect(server, known_hosts, password="nope")


def test_browse_read_write(server, known_hosts, tmp_path: Path):
    remote = connect(server, known_hosts)
    try:
        root = vfs.VPath(remote, "/")
        assert root.parent() is None
        assert [e.name for e in root.list_dir()] == ["docs", "big.bin"]
        assert ".profile" in [e.name for e in root.list_dir(show_hidden=True)]
        docs = root.child("docs")
        assert docs.is_dir() and docs.display == f"sftp://{USER}@127.0.0.1:{server.port}/docs"
        readme = docs.child("readme.txt")
        assert readme.stat().size == 15
        assert readme.materialize().read_text() == "hello from sftp"

        # download a tree
        local_out = tmp_path / "down"
        local_out.mkdir()
        progress: list[fs.Progress] = []
        vfs.copy([docs, root.child("big.bin")], vfs.local(local_out), on_conflict=never, on_progress=progress.append)
        assert (local_out / "docs" / "readme.txt").read_text() == "hello from sftp"
        assert (local_out / "big.bin").read_bytes() == (server.root / "big.bin").read_bytes()
        assert progress[-1].bytes_done == progress[-1].bytes_total

        # upload, mkdir, server-side move, delete
        (tmp_path / "up.txt").write_text("uploaded")
        vfs.mkdir(root, "new/nested")
        vfs.copy([vfs.local(tmp_path / "up.txt")], root.child("new", "nested"), on_conflict=never)
        assert (server.root / "new" / "nested" / "up.txt").read_text() == "uploaded"
        vfs.copy([root.child("new", "nested", "up.txt")], root, move=True, on_conflict=never)
        assert (server.root / "up.txt").exists() and not (server.root / "new" / "nested" / "up.txt").exists()
        vfs.delete([root.child("new"), root.child("up.txt")])
        assert not (server.root / "new").exists() and not (server.root / "up.txt").exists()
        assert not remote.can_trash
    finally:
        remote.close()


def test_archive_on_server(server, known_hosts):
    import zipfile

    with zipfile.ZipFile(server.root / "pack.zip", "w") as z:
        z.writestr("inside.txt", "zipped")
    remote = connect(server, known_hosts)
    try:
        pack = vfs.open_archive(vfs.VPath(remote, "/pack.zip"))
        assert [e.name for e in pack.list_dir()] == ["inside.txt"]
        assert pack.child("inside.txt").materialize().read_text() == "zipped"
        assert pack.parent() == vfs.VPath(remote, "/")
        assert pack.local_anchor() is None
    finally:
        remote.close()
