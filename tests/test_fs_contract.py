"""Every file system implementation must pass `orchard_commander.testing.check_filesystem`."""

from __future__ import annotations

import io
import zipfile
from pathlib import Path

import pytest

from orchard_commander import vfs
from orchard_commander.testing import SAMPLE_TREE, check_filesystem
from servers import PASSWORD, USER, docker_available, start_samba_container, start_sftp_server
from test_vfs import MemoryFS


def test_local_disk(tmp_path: Path):
    (tmp_path / "root").mkdir()
    check_filesystem(vfs.local(tmp_path / "root"))


def test_memory_example_backend():
    check_filesystem(vfs.VPath(MemoryFS(), ""))


@pytest.mark.parametrize("nested", [False, True], ids=["zip", "zip-in-memory-fs"])
def test_archive(tmp_path: Path, nested: bool):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        for rel, data in SAMPLE_TREE.items():
            z.writestr(rel, data)
    if nested:  # an archive stored on another (non-local) file system
        mem = MemoryFS()
        mem.files["sample.zip"] = buf.getvalue()
        archive = vfs.VPath(mem, "sample.zip")
    else:
        (tmp_path / "sample.zip").write_bytes(buf.getvalue())
        archive = vfs.local(tmp_path / "sample.zip")
    check_filesystem(vfs.open_archive(archive), writable=False)


def test_sftp(tmp_path: Path):
    from orchard_commander.remote import sftp

    (tmp_path / "server").mkdir()
    server = start_sftp_server(tmp_path / "server")
    remote = sftp.connect(server.host, port=server.port, username=USER, password=PASSWORD,
                          known_hosts=tmp_path / "known_hosts", confirm_host_key=lambda q: True)
    try:
        check_filesystem(vfs.VPath(remote, "/"))
    finally:
        remote.close()
        server.stop()


@pytest.fixture(scope="module")
def samba():
    if not docker_available():
        pytest.skip("Docker not available for the Samba test server")
    server, stop = start_samba_container()
    yield server
    stop()


def test_smb(samba):
    from orchard_commander.remote import smb

    remote = smb.connect(samba.host, samba.share, port=samba.port, username=USER, password=PASSWORD)
    try:
        check_filesystem(vfs.VPath(remote, "/"))
    finally:
        remote.close()
