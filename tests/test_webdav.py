"""WebDAV: authentication, certificates, ranged reads, uploads, names, the provider."""

from __future__ import annotations

import io
import json
import zipfile
from pathlib import Path

import pytest

from orchard_commander import connections, fsproviders, vfs
from orchard_commander.fsproviders import AuthenticationRequired, ConnectContext
from orchard_commander.remote import webdav
from servers import PASSWORD, USER, start_webdav_server


@pytest.fixture
def folder(tmp_path: Path) -> Path:
    root = tmp_path / "share"
    (root / "docs").mkdir(parents=True)
    (root / "docs" / "readme.txt").write_text("hello from webdav")
    (root / "big.bin").write_bytes(bytes(range(256)) * 4096)  # 1 MiB
    (root / "a b ü %.txt").write_text("odd name")
    with zipfile.ZipFile(root / "archive.zip", "w") as z:
        z.writestr("inside/note.txt", "zipped")
    return root


@pytest.fixture
def server(folder: Path):
    srv = start_webdav_server(folder)
    yield srv
    srv.stop()


def connect(server, tmp_path: Path, **kwargs) -> webdav.WebDAVFileSystem:
    options = {"username": USER, "password": PASSWORD, "secure": False, "root": server.root,
               "trusted_certificates": tmp_path / "trusted.json", "confirm_certificate": lambda q: True}
    return webdav.connect(server.host, port=server.port, **{**options, **kwargs})


def test_browse_and_read(server, tmp_path: Path):
    remote = connect(server, tmp_path)
    try:
        root = vfs.VPath(remote, "/")
        assert root.parent() is None
        names = [e.name for e in root.list_dir()]
        assert names == ["docs", "a b ü %.txt", "archive.zip", "big.bin"]
        odd = root.child("a b ü %.txt")
        assert odd.stat().size == len("odd name")
        with remote.open_read(odd.path) as f:
            assert f.read() == b"odd name"
        assert root.child("docs", "readme.txt").display.endswith("/dav/docs/readme.txt")
        assert remote.display("/").startswith(f"webdav://{USER}@127.0.0.1:")
    finally:
        remote.close()


def test_wrong_or_missing_password(server, tmp_path: Path):
    with pytest.raises(AuthenticationRequired):
        connect(server, tmp_path, password="nope")
    with pytest.raises(AuthenticationRequired):
        connect(server, tmp_path, password=None)


def test_digest_authentication(folder: Path, tmp_path: Path):
    srv = start_webdav_server(folder, digest=True)
    try:
        remote = connect(srv, tmp_path)
        assert [e.name for e in vfs.VPath(remote, "/docs").list_dir()] == ["readme.txt"]
        remote.close()
        with pytest.raises(AuthenticationRequired):
            connect(srv, tmp_path, password="nope")
    finally:
        srv.stop()


def test_seeking_reads_ranges(server, tmp_path: Path):
    remote = connect(server, tmp_path)
    data = bytes(range(256)) * 4096
    try:
        with remote.open_read("/big.bin") as f:
            assert f.seekable()
            f.seek(500_000)
            assert f.read(1000) == data[500_000:501_000]
            f.seek(-10, io.SEEK_END)
            assert f.read() == data[-10:]
            f.seek(3)
            assert f.read(5) == data[3:8]
        archive = vfs.open_archive(vfs.VPath(remote, "/archive.zip"))  # read in place
        with archive.fs.open_read(archive.child("inside", "note.txt").path) as f:
            assert f.read() == b"zipped"
    finally:
        remote.close()


def test_media_streams_from_webdav(server, tmp_path: Path, qapp):
    from orchard_commander.preview.api import LoadContext
    from orchard_commander.preview.builtin.media import MediaPreviewer
    from orchard_commander.preview.source import PreviewSource
    from orchard_commander.themes import get_scheme

    (tmp_path / "share" / "song.mp3").write_bytes((Path(__file__).parent / "data" / "tone-then-silence.mp3").read_bytes())
    remote = connect(server, tmp_path)
    try:
        ctx = LoadContext(colors=get_scheme("monokai"), show_hidden=False)
        loaded = MediaPreviewer().load(PreviewSource(vfs.VPath(remote, "/song.mp3")), ctx)
        assert loaded.data.stream is not None and loaded.data.path is None  # streamed, not downloaded
        assert loaded.data.waveform_stream is not None
        loaded.close()
    finally:
        remote.close()


def test_failed_copy_uploads_nothing(server, tmp_path: Path, folder: Path):
    remote = connect(server, tmp_path)
    try:
        with pytest.raises(RuntimeError):
            with remote.open_write("/partial.bin") as f:
                f.write(b"x" * 1000)
                raise RuntimeError("cancelled")
        assert not (folder / "partial.bin").exists()
        with remote.open_write("/ä.txt") as f:
            f.write(b"unicode name")
        assert (folder / "ä.txt").read_bytes() == b"unicode name"
        remote.mkdir("/new folder/sub")
        remote.rename("/ä.txt", "/new folder/sub/moved.txt")
        assert (folder / "new folder" / "sub" / "moved.txt").exists()
        with pytest.raises(FileExistsError):
            remote.mkdir("/new folder")
        remote.remove("/new folder")
        assert not (folder / "new folder").exists()
        with pytest.raises(FileNotFoundError):
            remote.stat("/nothing")
    finally:
        remote.close()


# -- certificates -------------------------------------------------------------


@pytest.fixture
def https_server(folder: Path, tmp_path: Path):
    srv = start_webdav_server(folder, tls=True, cert_dir=tmp_path / "cert")
    yield srv
    srv.stop()


def test_untrusted_certificate_is_confirmed_and_pinned(https_server, tmp_path: Path):
    asked: list[webdav.CertificateQuestion] = []
    remote = connect(https_server, tmp_path, secure=True, confirm_certificate=lambda q: asked.append(q) or True)
    assert [e.name for e in vfs.VPath(remote, "/docs").list_dir()] == ["readme.txt"]
    remote.close()
    assert asked[0].fingerprint == https_server.cert_fingerprint and asked[0].port == https_server.port
    assert asked[0].problem
    # Known now: no question.
    connect(https_server, tmp_path, secure=True, confirm_certificate=lambda q: pytest.fail("asked again")).close()


def test_rejected_and_changed_certificates(https_server, tmp_path: Path):
    with pytest.raises(webdav.CertificateRejected):
        connect(https_server, tmp_path, secure=True, confirm_certificate=lambda q: False)
    trusted = tmp_path / "trusted.json"
    trusted.write_text(json.dumps({f"{https_server.host}:{https_server.port}": "SHA256:00:11"}))
    with pytest.raises(webdav.CertificateChanged):
        connect(https_server, tmp_path, secure=True)


def test_plain_http_to_an_https_server_fails_cleanly(https_server, tmp_path: Path):
    with pytest.raises(ConnectionError):
        connect(https_server, tmp_path, secure=False)


# -- the provider ---------------------------------------------------------------


def test_provider_addresses():
    dav = fsproviders.get("webdavs")
    plain = fsproviders.get("webdav")
    assert dav is not None and plain is not None
    params = dav.parse_url("webdavs://me@cloud.example.com/remote.php/dav/files/me")
    assert params == {"host": "cloud.example.com", "username": "me", "root": "/remote.php/dav/files/me"}
    saved = {**params, "path": "Photos/2025"}
    assert dav.format_url(saved) == "webdavs://me@cloud.example.com/remote.php/dav/files/me/Photos/2025"
    assert dav.account(saved) == "webdavs://me@cloud.example.com/remote.php/dav/files/me"
    assert dav.parse_url(dav.account(saved))["root"] == "/remote.php/dav/files/me"  # bookmarks reconnect
    assert plain.format_url({"host": "nas", "port": 8080}) == "webdav://nas:8080"
    assert plain.format_url({"host": "nas", "port": 80}) == "webdav://nas"
    assert dav.validate({"host": "https://cloud.example.com"})
    assert dav.validate({"host": "cloud.example.com"}) is None


def test_provider_connects_with_start_folder(server, tmp_path: Path):
    provider = fsproviders.get("webdav")
    conn = connections.Connection("webdav", {"host": server.host, "port": server.port, "root": server.root,
                                             "username": USER, "path": "docs"})
    ctx = ConnectContext(data_dir=tmp_path, confirm=lambda *a: True)
    location = connections.Sessions().connect(conn, PASSWORD, ctx)
    try:
        assert location.path == "/docs"
        assert [e.name for e in location.list_dir()] == ["readme.txt"]
        assert provider.is_connected(location.fs)
    finally:
        location.fs.close()
