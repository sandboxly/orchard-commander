"""S3: request signing, buckets and folders, multipart uploads and copies, Amazon's
per-bucket addressing, the provider."""

from __future__ import annotations

import datetime as dt
import io
from pathlib import Path
from unittest import mock

import pytest

from orchard_commander import connections, fsproviders, vfs
from orchard_commander.fsproviders import AuthenticationRequired, ConnectContext
from orchard_commander.remote import s3
from servers import start_s3_server

ACCESS_KEY, SECRET_KEY = "AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"
MiB = 1024**2


@pytest.fixture
def server():
    srv = start_s3_server()
    yield srv
    srv.stop()


def connect(server, tmp_path: Path, **kwargs) -> s3.S3FileSystem:
    options = {"secure": False, "port": server.port, "access_key": ACCESS_KEY, "secret_key": SECRET_KEY,
               "trusted_certificates": tmp_path / "trusted.json", "confirm_certificate": lambda q: True}
    return s3.connect(server.host, **{**options, **kwargs})


def write(remote: vfs.FileSystem, path: str, data: bytes, chunk: int = MiB) -> None:
    with remote.open_write(path) as f:
        for i in range(0, len(data), chunk):
            f.write(data[i : i + chunk])


@pytest.fixture
def remote(server, tmp_path: Path):
    filesystem = connect(server, tmp_path)
    filesystem.mkdir("/photos")
    yield filesystem
    filesystem.close()


# -- signing ------------------------------------------------------------------------


def aws_signer(host: str = "examplebucket.s3.amazonaws.com") -> s3.Signer:
    # The credentials and time of the examples in AWS's S3 Signature Version 4 documentation.
    return s3.Signer(host, "us-east-1", "AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                     now=lambda: dt.datetime(2013, 5, 24, tzinfo=dt.UTC))


def signature(headers: dict[str, str]) -> str:
    return headers["Authorization"].rpartition("Signature=")[2]


def test_signatures_match_the_aws_documentation():
    headers = {"Range": "bytes=0-9"}
    aws_signer().authorize("GET", "/test.txt", headers, None)
    assert signature(headers) == "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
    assert "SignedHeaders=host;range;x-amz-content-sha256;x-amz-date," in headers["Authorization"]
    headers = {}
    aws_signer().authorize("GET", "/?max-keys=2&prefix=J", headers, None)
    assert signature(headers) == "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"


@pytest.mark.parametrize("method, url, headers, body", [
    ("GET", "/bucket/" + s3.encode_key("a b/ü+%.txt"), {"Range": "bytes=5-"}, None),
    ("GET", "/bucket?list-type=2&encoding-type=url&delimiter=%2F&prefix=a%20b%2F&continuation-token=x%2By%3D",
     {}, None),
    ("PUT", "/bucket/key.txt", {"Content-Type": "text/plain"}, b"data"),
    ("PUT", "/bucket/copy", {"x-amz-copy-source": "bucket/a%20b/%C3%BC.txt"}, b""),
    ("POST", "/bucket/big.bin?uploads=", {"Content-Type": "application/octet-stream"}, b""),
    ("PUT", "/bucket/big.bin?partNumber=2&uploadId=abc%2Bdef", {}, b"x" * 1000),
])
def test_signatures_match_botocore(method: str, url: str, headers: dict[str, str], body: bytes | None):
    from botocore.auth import S3SigV4Auth
    from botocore.awsrequest import AWSRequest
    from botocore.credentials import Credentials

    host, when = "127.0.0.1:9000", dt.datetime(2026, 10, 12, 8, 30, 15)
    ours = dict(headers)
    s3.Signer(host, "eu-central-1", ACCESS_KEY, SECRET_KEY, now=lambda: when.replace(tzinfo=dt.UTC)).authorize(
        method, url, ours, body)
    request = AWSRequest(method=method, url=f"http://{host}{url}", headers=dict(headers), data=body)
    with mock.patch("botocore.auth.get_current_datetime", return_value=when):
        S3SigV4Auth(Credentials(ACCESS_KEY, SECRET_KEY), "s3", "eu-central-1").add_auth(request)
    assert ours["Authorization"] == request.headers["Authorization"]


def test_anonymous_requests_are_unsigned():
    headers: dict[str, str] = {}
    s3.Signer("s3.amazonaws.com", "us-east-1", None, None).authorize("GET", "/", headers, None)
    assert "Authorization" not in headers and headers["Host"] == "s3.amazonaws.com"


# -- the file system ----------------------------------------------------------------


def test_buckets_and_folders(remote: s3.S3FileSystem):
    remote.mkdir("/photos/2025")
    write(remote, "/photos/2025/beach.jpg", b"jpeg")
    write(remote, "/photos/a b+ü %.txt", b"odd name")
    write(remote, "/photos/.hidden", b"")
    root = vfs.VPath(remote, "/")
    assert root.parent() is None and [(e.name, e.is_dir) for e in root.list_dir()] == [("photos", True)]
    photos = root.child("photos")
    assert photos.parent() == root
    assert [e.name for e in photos.list_dir(show_hidden=True)] == ["2025", ".hidden", "a b+ü %.txt"]
    assert photos.child("2025").stat().is_dir and photos.child("2025", "beach.jpg").stat().size == 4
    assert photos.child("a b+ü %.txt").stat().mtime > 0
    with remote.open_read("/photos/a b+ü %.txt") as f:
        assert f.read() == b"odd name"
    assert photos.child("2025").display == f"s3+http://{ACCESS_KEY}@127.0.0.1:{remote.port}/photos/2025"
    for missing in ("/photos/nothing", "/no-such-bucket", "/no-such-bucket/x"):
        with pytest.raises(FileNotFoundError):
            remote.stat(missing)
    with pytest.raises(FileNotFoundError):
        remote.list_dir("/photos/nothing")
    with pytest.raises(FileExistsError):
        remote.mkdir("/photos/2025")
    with pytest.raises(PermissionError):
        remote.open_write("/top-level.txt")


def test_a_folder_stays_when_its_last_file_is_deleted(remote: s3.S3FileSystem):
    write(remote, "/photos/trip/day1/a.jpg", b"a")  # no folder markers: implied by the key
    assert remote.is_dir("/photos/trip/day1")
    remote.remove("/photos/trip/day1/a.jpg")
    assert remote.is_dir("/photos/trip/day1") and remote.list_dir("/photos/trip/day1") == []
    remote.remove("/photos/trip/day1")
    assert not remote.exists("/photos/trip/day1") and remote.is_dir("/photos/trip")


def test_walk_lists_once(remote: s3.S3FileSystem):
    for path in ("/photos/a/1.txt", "/photos/a/b/2.txt", "/photos/c.txt"):
        write(remote, path, b"x")
    remote.mkdir("/photos/a/empty")
    with mock.patch.object(remote, "_list", wraps=remote._list) as listing:
        walked = {"/".join(rel): e.is_dir for rel, e in remote.walk("/photos")}
    assert listing.call_count == 1
    assert walked == {"": True, "a": True, "a/1.txt": False, "a/b": True, "a/b/2.txt": False, "a/empty": True,
                      "c.txt": False}


def test_rename_files_and_folders(remote: s3.S3FileSystem):
    write(remote, "/photos/docs/one.txt", b"1")
    write(remote, "/photos/docs/sub/two.txt", b"2")
    remote.rename("/photos/docs", "/photos/moved")
    assert not remote.exists("/photos/docs")
    assert [e.name for e in remote.list_dir("/photos/moved")] == ["sub", "one.txt"]
    remote.rename("/photos/moved/one.txt", "/photos/one.txt")
    with remote.open_read("/photos/one.txt") as f:
        assert f.read() == b"1"
    assert remote.is_dir("/photos/moved")  # still holds sub/
    with pytest.raises(FileExistsError):
        remote.rename("/photos/one.txt", "/photos/moved/sub/two.txt")
    with pytest.raises(PermissionError):
        remote.rename("/photos", "/pictures")


def test_large_files_go_up_in_parts(remote: s3.S3FileSystem, monkeypatch):
    monkeypatch.setattr(s3, "PART_SIZE", 5 * MiB)  # S3's minimum
    data = bytes(range(256)) * (11 * MiB // 256)
    with mock.patch.object(s3._Multipart, "upload", autospec=True, side_effect=s3._Multipart.upload) as parts:
        write(remote, "/photos/big.bin", data)
    assert parts.call_count == 3
    assert remote.stat("/photos/big.bin").size == len(data)
    with remote.open_read("/photos/big.bin") as f:
        assert f.seekable()
        f.seek(6 * MiB)
        assert f.read(1000) == data[6 * MiB : 6 * MiB + 1000]
        f.seek(-10, io.SEEK_END)
        assert f.read() == data[-10:]


def test_a_cancelled_upload_leaves_nothing(remote: s3.S3FileSystem, monkeypatch):
    monkeypatch.setattr(s3, "PART_SIZE", 5 * MiB)
    with pytest.raises(RuntimeError):
        with remote.open_write("/photos/partial.bin") as f:
            f.write(b"x" * (6 * MiB))  # one part is up already
            raise RuntimeError("cancelled")
    assert not remote.exists("/photos/partial.bin")
    _, data = remote._request("GET", "photos", query={"uploads": ""})
    assert b"<UploadId>" not in data  # the multipart upload was aborted


def test_large_objects_are_copied_in_parts(remote: s3.S3FileSystem, monkeypatch):
    monkeypatch.setattr(s3, "COPY_LIMIT", 6 * MiB)
    monkeypatch.setattr(s3, "COPY_PART_SIZE", 5 * MiB)
    data = bytes(range(256)) * (11 * MiB // 256)
    write(remote, "/photos/big.bin", data)
    remote.rename("/photos/big.bin", "/photos/renamed.bin")
    with remote.open_read("/photos/renamed.bin") as f:
        assert f.read() == data


def test_buckets_are_folders_at_the_top(remote: s3.S3FileSystem):
    remote.mkdir("/videos")
    remote.mkdir("/videos", exist_ok=True)
    with pytest.raises(FileExistsError):
        remote.mkdir("/videos")
    write(remote, "/videos/clip.mp4", b"mp4")
    assert [e.name for e in remote.list_dir("/")] == ["photos", "videos"]
    remote.remove("/videos")  # emptied, then deleted
    assert [e.name for e in remote.list_dir("/")] == ["photos"]


def test_a_secret_is_needed_with_an_access_key(server, tmp_path: Path):
    with pytest.raises(AuthenticationRequired):
        connect(server, tmp_path, secret_key=None)


def test_rejected_keys_ask_for_the_secret_again(remote: s3.S3FileSystem, monkeypatch):
    from orchard_commander.remote.httpclient import HTTPClient

    original = HTTPClient.send

    def send(self, method, url, **kwargs):
        response, _ = original(self, method, url, **kwargs)
        response.status = 403
        return response, b"<Error><Code>SignatureDoesNotMatch</Code><Message>Check your key</Message></Error>"

    monkeypatch.setattr(HTTPClient, "send", send)
    with pytest.raises(AuthenticationRequired, match="Check your key"):
        remote.check_credentials()


def test_amazon_buckets_by_name_and_region(server, monkeypatch):
    """Amazon: each bucket at its own host name in its region, found out once."""
    hosts: list[str] = []
    original = s3.S3FileSystem._client

    def client(self, host, region):
        c = original(self, host, region)
        hosts.append(c.authorizer.host)
        c.host, c.port, c.secure, c.pin = server.host, server.port, False, None  # really: moto
        return c

    monkeypatch.setattr(s3.S3FileSystem, "_client", client)
    creator = s3.S3FileSystem(region="eu-central-1", access_key=ACCESS_KEY, secret_key=SECRET_KEY)
    creator.mkdir("/euro-bucket")
    creator.mkdir("/dotted.bucket")
    creator.close()
    hosts.clear()

    remote = s3.S3FileSystem(access_key=ACCESS_KEY, secret_key=SECRET_KEY)  # s3.amazonaws.com, no region
    assert remote.aws and remote.type_name == f"s3://{ACCESS_KEY}@s3.amazonaws.com"
    write(remote, "/euro-bucket/hello.txt", b"hi")
    write(remote, "/dotted.bucket/hello.txt", b"hi")
    assert [e.name for e in remote.list_dir("/euro-bucket")] == ["hello.txt"]
    assert remote._regions == {"euro-bucket": "eu-central-1", "dotted.bucket": "eu-central-1"}
    assert "euro-bucket.s3.eu-central-1.amazonaws.com" in hosts  # virtual-hosted
    assert "s3.eu-central-1.amazonaws.com" in hosts  # dots don't fit the certificate: path-style
    remote.close()


def test_region_from_the_server_name():
    assert s3.region_of_host("s3.eu-central-1.amazonaws.com") == "eu-central-1"
    assert s3.region_of_host("s3.us-west-004.backblazeb2.com") == "us-west-004"
    assert s3.region_of_host("s3.amazonaws.com") is None
    assert s3.region_of_host("minio.local") is None


# -- the provider ---------------------------------------------------------------


def test_provider_addresses():
    provider = fsproviders.get("s3")
    plain = fsproviders.get("s3+http")
    assert provider is not None and plain is not None
    assert provider.parse_url("s3://my-bucket/some/folder") == {"host": "s3.amazonaws.com",
                                                                "path": "my-bucket/some/folder"}  # AWS CLI style
    params = provider.parse_url(f"s3://{ACCESS_KEY}@fsn1.your-objectstorage.com/media/2025?region=fsn1")
    assert params == {"host": "fsn1.your-objectstorage.com", "username": ACCESS_KEY, "path": "media/2025",
                      "region": "fsn1"}
    assert provider.format_url(params) == f"s3://{ACCESS_KEY}@fsn1.your-objectstorage.com/media/2025?region=fsn1"
    assert provider.parse_url(provider.account(params)).get("region") == "fsn1"  # bookmarks reconnect
    assert provider.format_url({"username": ACCESS_KEY}) == f"s3://{ACCESS_KEY}@s3.amazonaws.com"
    assert plain.parse_url("s3+http://minio:9000/bucket") == {"host": "minio", "port": 9000, "path": "bucket"}
    assert plain.format_url({"host": "nas", "port": 80}) == "s3+http://nas"
    assert provider.validate({"host": "https://minio.local"})
    assert provider.validate({}) is None  # Amazon


def test_provider_connects_with_start_folder(server, tmp_path: Path):
    remote = connect(server, tmp_path)
    remote.mkdir("/media/music")
    remote.close()
    conn = connections.Connection("s3+http", {"host": server.host, "port": server.port, "username": ACCESS_KEY,
                                              "path": "media"})
    ctx = ConnectContext(data_dir=tmp_path, confirm=lambda *a: True)
    location = connections.Sessions().connect(conn, SECRET_KEY, ctx)
    try:
        assert location.path == "/media"
        assert [e.name for e in location.list_dir()] == ["music"]
        assert location.parent() == vfs.VPath(location.fs, "/")
        assert conn.provider.is_connected(location.fs)
    finally:
        location.fs.close()
