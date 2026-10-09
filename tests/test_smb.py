"""SMB file system against two real servers:

- impacket's pure-Python SMB2 server (always available; read-only use, since it
  answers file creation with values smbprotocol rejects)
- Samba in Docker (full read/write; skipped without Docker)
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from orchard_commander import vfs
from orchard_commander.remote import AuthenticationRequired, smb
from servers import PASSWORD, USER, docker_available, start_samba_container, start_smb_server


def never(src, dst):
    raise AssertionError(f"unexpected conflict {src} -> {dst}")


def connect(server, password=PASSWORD) -> smb.SMBFileSystem:
    return smb.connect(server.host, server.share, port=server.port, username=USER, password=password, timeout=10)


# -- impacket: listing and reading ------------------------------------------------------


@pytest.fixture(scope="module")
def impacket_server(tmp_path_factory):
    root = tmp_path_factory.mktemp("share")
    (root / "docs").mkdir()
    (root / "docs" / "readme.txt").write_text("hello from smb")
    (root / "big.bin").write_bytes(bytes(range(256)) * 4096)
    return start_smb_server(root)


def test_wrong_password(impacket_server):
    with pytest.raises(AuthenticationRequired):
        connect(impacket_server, password="nope")


def test_browse_and_download(impacket_server, tmp_path: Path):
    server = impacket_server
    remote = connect(server)
    try:
        root = vfs.VPath(remote, "/")
        assert root.parent() is None
        assert [e.name for e in root.list_dir()] == ["docs", "big.bin"]
        big = root.list_dir()[1]
        assert big.size == 1024 * 1024
        assert abs(big.mtime - os.stat(server.root / "big.bin").st_mtime) < 2  # no timezone shift
        docs = root.child("docs")
        assert docs.is_dir() and docs.display == f"smb://127.0.0.1:{server.port}/SHARE/docs"
        assert not root.child("missing.txt").exists()
        assert docs.child("readme.txt").materialize().read_text() == "hello from smb"
        out = tmp_path / "out"
        out.mkdir()
        vfs.copy([docs, root.child("big.bin")], vfs.local(out), on_conflict=never)
        assert (out / "docs" / "readme.txt").read_text() == "hello from smb"
        assert (out / "big.bin").read_bytes() == (server.root / "big.bin").read_bytes()
    finally:
        remote.close()


# -- Samba: full read/write ---------------------------------------------------------------


@pytest.fixture(scope="module")
def samba():
    if not docker_available():
        pytest.skip("Docker not available for the Samba test server")
    server, stop = start_samba_container()
    yield server
    stop()


def test_samba_write_operations(samba, tmp_path: Path):
    remote = connect(samba)
    try:
        root = vfs.VPath(remote, "/")
        (tmp_path / "up.txt").write_text("uploaded")
        vfs.mkdir(root, "new/nested")
        vfs.copy([vfs.local(tmp_path / "up.txt")], root.child("new", "nested"), on_conflict=never)
        uploaded = root.child("new", "nested", "up.txt")
        assert uploaded.stat().size == 8
        assert abs(uploaded.stat().mtime - (tmp_path / "up.txt").stat().st_mtime) < 2  # mtime preserved

        vfs.copy([uploaded], root, move=True, on_conflict=never)  # server-side rename
        assert root.child("up.txt").exists() and not uploaded.exists()
        assert root.child("up.txt").materialize().read_text() == "uploaded"

        vfs.delete([root.child("new"), root.child("up.txt")])
        assert root.list_dir(show_hidden=True) == []
    finally:
        remote.close()


@pytest.fixture(scope="module")
def stingy_samba():
    """Samba granting few SMB2 credits, like many NAS boxes."""
    if not docker_available():
        pytest.skip("Docker not available for the Samba test server")
    server, stop = start_samba_container(max_credits=16)
    yield server
    stop()


def test_concurrent_use_with_few_credits(stingy_samba):
    """Listing while files are read (preview + copy of the same file) must not run out of credits."""
    import threading

    remote = connect(stingy_samba)
    try:
        data = os.urandom(8 * 1024 * 1024)
        with remote.open_write("/movie.mp4") as f:
            f.write(data)
        errors: list[BaseException] = []
        results: list[bytes] = []

        def read_all() -> None:
            try:
                with remote.open_read("/movie.mp4") as f:  # two readers of one file at once
                    results.append(f.read())
            except BaseException as exc:
                errors.append(exc)

        def list_repeatedly() -> None:
            try:
                for _ in range(50):
                    remote.list_dir("/")
                    remote.stat("/movie.mp4")
            except BaseException as exc:
                errors.append(exc)

        threads = [threading.Thread(target=t) for t in (read_all, read_all, list_repeatedly, list_repeatedly)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(120)
        assert errors == []
        assert results == [data, data]
    finally:
        remote.close()
