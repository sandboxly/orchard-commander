"""Amazon S3 and S3-compatible object storage (MinIO, Cloudflare R2, Backblaze B2,
Wasabi, Hetzner, DigitalOcean Spaces, Ceph, Garage, …).

Plain HTTP via `httpclient` with AWS Signature Version 4, no SDK. The file
system's root lists the buckets; below that, "folders" are key prefixes up to
a "/", as in the AWS console:

    /                       buckets
    /photos                 bucket "photos"
    /photos/2025/beach.jpg  object "2025/beach.jpg" in it

S3 has no real folders. A folder exists while something is in it, or while it
has a marker object ("2025/", empty) — which is what mkdir creates, and what
deleting the last item of a folder leaves behind so the folder stays, as on a
disk. Renaming copies on the server and deletes the original. Uploads go up
in parts while they're written (multipart upload), so large files need no
local spool and a cancelled copy leaves nothing behind.

Amazon itself is addressed per bucket (bucket.s3.REGION.amazonaws.com), the
region found out once per bucket. Other servers get path-style requests
(server/bucket/key) with the configured region; a server that names the
right region in its error is asked again with that one.
"""

from __future__ import annotations

import base64
import datetime as dt
import errno
import hashlib
import hmac
import http.client
import io
import mimetypes
import posixpath
import re
import threading
import xml.etree.ElementTree as ET
from collections.abc import Callable, Iterator
from dataclasses import dataclass
from email.utils import parsedate_to_datetime
from pathlib import Path
from typing import BinaryIO
from urllib.parse import quote, unquote, unquote_plus
from xml.sax.saxutils import escape

from orchard_commander import fs, vfs
from orchard_commander.remote import AuthenticationRequired
from orchard_commander.remote.httpclient import (
    READ_BUFFER,
    TIMEOUT,
    CertificateQuestion,
    HTTPClient,
    RangeReader,
    check_certificate,
)

AWS_HOST = "s3.amazonaws.com"
DEFAULT_REGION = "us-east-1"
PART_SIZE = 16 * 1024**2  # upload part size; doubles every 1000 parts (S3 allows 10,000 parts)
COPY_LIMIT = 5 * 1024**3  # CopyObject's limit; bigger objects are copied in parts
COPY_PART_SIZE = 512 * 1024**2
DELETE_BATCH = 1000  # keys per DeleteObjects request

NS = "{http://s3.amazonaws.com/doc/2006-03-01/}"
EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()
UNSIGNED_PAYLOAD = "UNSIGNED-PAYLOAD"
_UNRESERVED = "-_.~"
_AUTH_ERRORS = {"InvalidAccessKeyId", "SignatureDoesNotMatch", "InvalidToken", "ExpiredToken", "InvalidSecurity",
                "TokenRefreshRequired"}
_REGION_ERRORS = {"PermanentRedirect", "AuthorizationHeaderMalformed", "IllegalLocationConstraintException",
                  "TemporaryRedirect", "InvalidRegion"}


def is_aws(host: str) -> bool:
    return host.endswith((".amazonaws.com", ".amazonaws.com.cn"))


def region_of_host(host: str) -> str | None:
    """The region an endpoint names: s3.eu-central-1.amazonaws.com, s3.us-west-004.backblazeb2.com,
    s3.eu-central-1.wasabisys.com; None for global endpoints like s3.amazonaws.com."""
    m = re.match(r"^s3[.-](?:dualstack\.)?([a-z]{2}(?:-[a-z]+)+-\d+)\.", host)
    return m.group(1) if m else None


def _dns_compatible(bucket: str) -> bool:
    """Whether the bucket can be a host name label under TLS (no dots: wildcard certificates)."""
    return bool(re.fullmatch(r"[a-z0-9][a-z0-9-]{1,61}[a-z0-9]", bucket))


def encode_key(key: str) -> str:
    """URI-encode an object key for the request path (S3 rules: "/" stays)."""
    return quote(key, safe="/" + _UNRESERVED)


# ---------------------------------------------------------------------------
# AWS Signature Version 4
# ---------------------------------------------------------------------------


class Signer:
    """Signs requests for one host and region (`httpclient.Authorizer`)."""

    def __init__(self, host: str, region: str, access_key: str | None, secret_key: str | None,
                 *, now: Callable[[], dt.datetime] = lambda: dt.datetime.now(dt.UTC)) -> None:
        self.host, self.region = host, region
        self.access_key, self.secret_key = access_key, secret_key
        self.now = now
        self._keys: dict[str, bytes] = {}  # signing key per date

    def authorize(self, method: str, url: str, headers: dict[str, str], body: bytes | BinaryIO | None) -> None:
        headers["Host"] = self.host  # sent as signed (http.client then adds none of its own)
        if isinstance(body, (bytes, bytearray, memoryview)):
            payload = hashlib.sha256(body).hexdigest()
        else:
            payload = EMPTY_SHA256 if body is None else UNSIGNED_PAYLOAD
        headers["x-amz-content-sha256"] = payload
        if not self.access_key:
            return  # anonymous: public buckets
        stamp = self.now().strftime("%Y%m%dT%H%M%SZ")
        headers["x-amz-date"] = stamp
        headers["Authorization"] = self.authorization(method, url, headers, payload, stamp)

    def authorization(self, method: str, url: str, headers: dict[str, str], payload: str, stamp: str) -> str:
        path, _, query = url.partition("?")
        signed = {k.lower(): " ".join(str(v).split()) for k, v in headers.items()
                  if k.lower() in ("host", "range", "content-type", "content-md5") or k.lower().startswith("x-amz-")}
        names = ";".join(sorted(signed))
        canonical = "\n".join([
            method,
            path,
            _canonical_query(query),
            "".join(f"{k}:{signed[k]}\n" for k in sorted(signed)),
            names,
            payload,
        ])
        scope = f"{stamp[:8]}/{self.region}/s3/aws4_request"
        to_sign = f"AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{hashlib.sha256(canonical.encode()).hexdigest()}"
        signature = hmac.new(self._signing_key(stamp[:8]), to_sign.encode(), hashlib.sha256).hexdigest()
        return f"AWS4-HMAC-SHA256 Credential={self.access_key}/{scope}, SignedHeaders={names}, Signature={signature}"

    def _signing_key(self, date: str) -> bytes:
        key = self._keys.get(date)
        if key is None:
            key = f"AWS4{self.secret_key or ''}".encode()
            for part in (date, self.region, "s3", "aws4_request"):
                key = hmac.new(key, part.encode(), hashlib.sha256).digest()
            self._keys = {date: key}
        return key

    def challenged(self, response: http.client.HTTPResponse) -> bool:
        return False  # S3 answers 403, never asks again


def _canonical_query(query: str) -> str:
    pairs = []
    for item in query.split("&") if query else []:
        k, _, v = item.partition("=")
        pairs.append((quote(unquote(k), safe=_UNRESERVED), quote(unquote(v), safe=_UNRESERVED)))
    return "&".join(f"{k}={v}" for k, v in sorted(pairs))


def _query(params: dict[str, str | int] | None) -> str:
    if not params:
        return ""
    return "?" + "&".join(f"{quote(k, safe=_UNRESERVED)}={quote(str(v), safe=_UNRESERVED)}"
                          for k, v in params.items())


# ---------------------------------------------------------------------------
# Errors
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class S3Error:
    code: str = ""
    message: str = ""
    region: str = ""

    @classmethod
    def parse(cls, data: bytes) -> S3Error:
        try:
            root = ET.fromstring(data)
        except ET.ParseError:
            return cls()
        if root.tag.removeprefix(NS) != "Error":  # (an Element with no children is falsy: no `or`)
            nested = root.find(f"{NS}Error")
            nested = nested if nested is not None else root.find("Error")
            root = nested if nested is not None else root
        return cls(_text(root, "Code"), _text(root, "Message"), _text(root, "Region"))


def _text(element: ET.Element, name: str) -> str:
    """A child's text, with or without the S3 namespace (servers differ)."""
    found = element.find(f"{NS}{name}")
    if found is None:
        found = element.find(name)
    return (found.text or "") if found is not None else ""


def _error(status: int, reason: str, err: S3Error, what: str) -> OSError:
    detail = err.message or err.code or reason
    message = f"{what}: {status} {detail}"
    if err.code in _AUTH_ERRORS:
        raise AuthenticationRequired(f"The server didn't accept the access key: {detail}")
    if status == 404 or err.code in ("NoSuchKey", "NoSuchBucket", "NoSuchUpload"):
        return FileNotFoundError(errno.ENOENT, message)
    if status == 403:
        return PermissionError(errno.EACCES, message)
    if err.code in ("BucketAlreadyExists", "BucketAlreadyOwnedByYou"):
        return FileExistsError(errno.EEXIST, message)
    if err.code == "BucketNotEmpty":
        return OSError(errno.ENOTEMPTY, message)
    if err.code in ("InvalidBucketName", "KeyTooLongError"):
        return OSError(errno.EINVAL, message)
    if err.code in ("QuotaExceeded", "EntityTooLarge") or status == 507:
        return OSError(errno.ENOSPC, message)
    return OSError(errno.EIO, message)


def _timestamp(text: str) -> float:
    try:
        return dt.datetime.fromisoformat(text.strip().replace("Z", "+00:00")).timestamp()
    except ValueError:
        return 0.0


# ---------------------------------------------------------------------------
# Connecting
# ---------------------------------------------------------------------------


def connect(
    host: str = AWS_HOST,
    *,
    secure: bool = True,
    port: int | None = None,
    region: str | None = None,
    access_key: str | None = None,
    secret_key: str | None = None,
    trusted_certificates: Path,
    confirm_certificate: Callable[[CertificateQuestion], bool],
    timeout: float = TIMEOUT,
) -> S3FileSystem:
    """Connect and check the credentials. Raises AuthenticationRequired,
    CertificateChanged, CertificateRejected or ConnectionError."""
    if access_key and not secret_key:
        raise AuthenticationRequired(f"The secret key for {access_key} is needed")
    port = port or (443 if secure else 80)
    pin = None
    if secure:
        pin = check_certificate(host, port, trusted_certificates=trusted_certificates,
                                confirm_certificate=confirm_certificate, timeout=timeout)
    filesystem = S3FileSystem(host, port, secure=secure, region=region, access_key=access_key,
                              secret_key=secret_key, pin=pin, timeout=timeout)
    try:
        filesystem.check_credentials()
    except BaseException:
        filesystem.close()
        raise
    return filesystem


# ---------------------------------------------------------------------------
# The file system
# ---------------------------------------------------------------------------


class S3FileSystem(vfs.FileSystem):
    writable = True
    can_rename = True  # a copy on the server, then a delete: still no download

    def __init__(self, host: str = AWS_HOST, port: int = 443, *, secure: bool = True, region: str | None = None,
                 access_key: str | None = None, secret_key: str | None = None, pin: str | None = None,
                 timeout: float = TIMEOUT) -> None:
        self.host, self.port, self.secure = host, port, secure
        self.aws = is_aws(host)
        self.region = region or region_of_host(host) or ""  # "" = find out (Amazon) / DEFAULT_REGION
        self.access_key, self.secret_key = access_key, secret_key
        self.pin, self.timeout = pin, timeout
        self._clients: dict[tuple[str, str], HTTPClient] = {}
        self._regions: dict[str, str] = {}  # bucket -> region
        self._lock = threading.Lock()
        self._closed = False
        default = 443 if secure else 80
        self.label = (f"{access_key}@" if access_key else "") + host + (f":{port}" if port != default else "")
        self.type_name = f"{'s3' if secure else 's3+http'}://{self.label}"

    # -- requests ------------------------------------------------------------------

    def _client(self, host: str, region: str) -> HTTPClient:
        if self._closed:
            raise ConnectionError(f"Connection to {self.label} was closed")
        with self._lock:
            client = self._clients.get((host, region))
            if client is None:
                own = host == self.host
                netloc = host + (f":{self.port}" if own and self.port != (443 if self.secure else 80) else "")
                client = HTTPClient(host, self.port if own else 443, secure=self.secure if own else True,
                                    pin=self.pin if own else None, timeout=self.timeout,
                                    authorizer=Signer(netloc, region, self.access_key, self.secret_key))
                self._clients[(host, region)] = client
            return client

    def _bucket_region(self, bucket: str) -> str:
        region = self._regions.get(bucket)
        if region is None:
            region = self.region or DEFAULT_REGION
            if self.aws and not self.region:  # Amazon tells, even without permission to the bucket
                client = self._client(AWS_HOST if not self.host.endswith(".cn") else self.host, DEFAULT_REGION)
                response, _ = client.send("HEAD", "/" + encode_key(bucket))
                region = response.headers.get("x-amz-bucket-region") or region
            self._regions[bucket] = region
        return region

    def _target(self, bucket: str | None, key: str = "") -> tuple[HTTPClient, str]:
        """The client and URL path for a bucket (None: the service) and key."""
        if bucket is None:
            return self._client(self.host, self.region or DEFAULT_REGION), "/"
        region = self._bucket_region(bucket)
        if self.aws:
            suffix = "amazonaws.com.cn" if self.host.endswith(".cn") else "amazonaws.com"
            regional = f"s3.{region}.{suffix}"
            if _dns_compatible(bucket):
                return self._client(f"{bucket}.{regional}", region), "/" + encode_key(key)
            return self._client(regional, region), f"/{encode_key(bucket)}/{encode_key(key)}"
        return self._client(self.host, region), f"/{encode_key(bucket)}/{encode_key(key)}" if key else f"/{encode_key(bucket)}"

    def _request(self, method: str, bucket: str | None, key: str = "", *, query: dict[str, str | int] | None = None,
                 headers: dict[str, str] | None = None, body: bytes | None = None, ok: tuple[int, ...] = (200,),
                 what: str = "", conn: http.client.HTTPConnection | None = None,
                 stream: bool = False) -> tuple[http.client.HTTPResponse, bytes]:
        for attempt in range(2):
            client, url = self._target(bucket, key)
            response, data = client.send(method, url + _query(query), headers=headers, body=body, conn=conn,
                                         stream=stream)
            if response.status in ok:
                return response, data
            err = S3Error.parse(data)
            region = response.headers.get("x-amz-bucket-region") or err.region
            wrong_region = response.status in (301, 307, 400) or err.code in _REGION_ERRORS
            if attempt == 0 and bucket and region and wrong_region and region != self._regions.get(bucket):
                self._regions[bucket] = region  # ask again where the server says the bucket is
                continue
            if response.status in (301, 307) and not err.code:
                err = S3Error("PermanentRedirect", f"the bucket is in another region than {self._regions.get(bucket)}")
            raise _error(response.status, response.reason, err,
                         what or f"{method} {self.display(self.join('/', bucket or '', key))}")
        raise AssertionError("unreachable")

    def check_credentials(self) -> None:
        """List the buckets once: wrong keys fail here, not at the first folder. Keys
        limited to some buckets may not list them, which is fine."""
        try:
            self._request("GET", None, what=f"Connecting to {self.label}")
        except PermissionError:
            pass

    # -- paths -----------------------------------------------------------------------

    @staticmethod
    def _split(path: str) -> tuple[str | None, str]:
        """"/bucket/a/b" -> ("bucket", "a/b"); "/" -> (None, "")."""
        parts = [p for p in path.split("/") if p]
        return (parts[0], "/".join(parts[1:])) if parts else (None, "")

    def parent_path(self, path: str) -> str | None:
        path = posixpath.normpath("/" + path.lstrip("/"))
        return None if path == "/" else posixpath.dirname(path)

    def join(self, path: str, *names: str) -> str:
        return posixpath.join(path, *names)

    def basename(self, path: str) -> str:
        return posixpath.basename(path.rstrip("/")) or self.host

    def display(self, path: str) -> str:
        return f"{self.type_name}{posixpath.normpath('/' + path.lstrip('/'))}"

    # -- reading -------------------------------------------------------------------

    def _list(self, bucket: str, prefix: str, *, delimiter: bool = True,
              max_keys: int | None = None) -> Iterator[tuple[str, str, ET.Element]]:
        """("prefix", name, element) for common prefixes and ("object", key, element)
        for objects, page by page, names decoded."""
        token = None
        while True:
            query: dict[str, str | int] = {"list-type": 2, "encoding-type": "url"}
            if delimiter:
                query["delimiter"] = "/"
            if prefix:
                query["prefix"] = prefix
            if max_keys:
                query["max-keys"] = max_keys
            if token:
                query["continuation-token"] = token
            _, data = self._request("GET", bucket, query=query)
            try:
                root = ET.fromstring(data)
            except ET.ParseError as exc:
                raise OSError(errno.EIO, f"{self.type_name} sent an invalid listing: {exc}") from exc
            encoded = _text(root, "EncodingType") == "url"

            def decode(text: str) -> str:
                return unquote_plus(text) if encoded else text

            for element in root:
                tag = element.tag.removeprefix(NS)
                if tag == "CommonPrefixes":
                    yield "prefix", decode(_text(element, "Prefix")), element
                elif tag == "Contents":
                    yield "object", decode(_text(element, "Key")), element
            token = _text(root, "NextContinuationToken")
            if max_keys or _text(root, "IsTruncated") != "true" or not token:
                return

    def _object_entry(self, name: str, element: ET.Element) -> fs.Entry:
        size = _text(element, "Size")
        return fs.Entry(name=name, is_dir=False, is_link=False, is_hidden=name.startswith("."),
                        is_executable=False, size=int(size) if size.isdigit() else 0,
                        mtime=_timestamp(_text(element, "LastModified")))

    @staticmethod
    def _dir_entry(name: str, mtime: float = 0.0) -> fs.Entry:
        return fs.Entry(name=name, is_dir=True, is_link=False, is_hidden=name.startswith("."),
                        is_executable=False, size=0, mtime=mtime)

    def list_buckets(self) -> list[fs.Entry]:
        _, data = self._request("GET", None, what=f"Listing the buckets of {self.label}")
        root = ET.fromstring(data)
        buckets = root.find(f"{NS}Buckets")
        if buckets is None:
            buckets = root.find("Buckets")
        return [self._dir_entry(_text(b, "Name"), _timestamp(_text(b, "CreationDate")))
                for b in (buckets if buckets is not None else []) if _text(b, "Name")]

    def list_dir(self, path: str, **kwargs) -> list[fs.Entry]:
        bucket, key = self._split(path)
        if bucket is None:
            return fs.sort_entries(self.list_buckets(), **kwargs)
        prefix = key + "/" if key else ""
        entries, found = [], not prefix
        for kind, name, element in self._list(bucket, prefix):
            found = True
            rest = name[len(prefix):].rstrip("/") if name.startswith(prefix) else ""
            if not rest or "/" in rest:  # the folder's own marker, or a name S3 allows but paths can't show
                continue
            entries.append(self._dir_entry(rest) if kind == "prefix" else self._object_entry(rest, element))
        if not found:
            raise FileNotFoundError(errno.ENOENT, "No such folder", self.display(path))
        return fs.sort_entries(entries, **kwargs)

    def stat(self, path: str) -> fs.Entry:
        bucket, key = self._split(path)
        if bucket is None:
            return self._dir_entry(self.host)
        if not key:
            try:
                self._request("HEAD", bucket, what=f"Bucket {bucket}")
            except PermissionError:
                pass  # it exists, the key just may not look at it as a whole
            return self._dir_entry(bucket)
        try:
            response, _ = self._request("HEAD", bucket, key)
        except FileNotFoundError:
            for _ in self._list(bucket, key + "/", delimiter=False, max_keys=1):
                return self._dir_entry(self.basename(path))
            raise FileNotFoundError(errno.ENOENT, "Not found", self.display(path)) from None
        if key.endswith("/"):
            return self._dir_entry(self.basename(path))
        length = response.headers.get("Content-Length", "")
        try:
            mtime = parsedate_to_datetime(response.headers.get("Last-Modified", "")).timestamp()
        except (ValueError, TypeError, IndexError):
            mtime = 0.0
        name = self.basename(path)
        return fs.Entry(name=name, is_dir=False, is_link=False, is_hidden=name.startswith("."), is_executable=False,
                        size=int(length) if length.isdigit() else 0, mtime=mtime)

    def open_read(self, path: str) -> BinaryIO:
        bucket, key = self._split(path)
        if bucket is None or not key:
            raise IsADirectoryError(errno.EISDIR, "Is a folder", self.display(path))
        client, _ = self._target(bucket, key)

        def get(conn: http.client.HTTPConnection, headers: dict[str, str]) -> http.client.HTTPResponse:
            response, _ = self._request("GET", bucket, key, headers=headers, conn=conn, stream=True,
                                        ok=(200, 206, 416))
            return response

        reader = RangeReader(client, get, lambda: self.stat(path).size)
        return io.BufferedReader(reader, buffer_size=READ_BUFFER)  # type: ignore[return-value]

    def walk(self, path: str) -> Iterator[tuple[vfs.RelPath, fs.Entry]]:
        """One flat listing of every key below `path` (not one request per folder)."""
        root = self.stat(path)
        yield (), root
        if not root.is_dir:
            return
        bucket, key = self._split(path)
        if bucket is None:
            for entry in self.list_buckets():
                yield (entry.name,), entry
                for rel, child in self.walk("/" + entry.name):
                    if rel:
                        yield (entry.name, *rel), child
            return
        prefix = key + "/" if key else ""
        seen: set[vfs.RelPath] = {()}
        for _, name, element in self._list(bucket, prefix, delimiter=False):
            parts = tuple(name[len(prefix):].split("/"))
            if any(not p for p in parts[:-1]):
                continue  # "a//b": not reachable as a path
            for i in range(1, len(parts)):  # folders, before what's in them
                if parts[:i] not in seen:
                    seen.add(parts[:i])
                    yield parts[:i], self._dir_entry(parts[i - 1])
            if parts[-1]:  # not just a folder marker
                yield parts, self._object_entry(parts[-1], element)

    # -- writing -------------------------------------------------------------------

    def mkdir(self, path: str, *, exist_ok: bool = False) -> None:
        bucket, key = self._split(path)
        if bucket is None:
            raise FileExistsError(errno.EEXIST, "File exists", self.display(path))
        if not key:
            self._create_bucket(bucket, exist_ok)
            return
        try:
            entry = self.stat(path)
        except FileNotFoundError:
            pass
        else:
            if exist_ok and entry.is_dir:
                return
            raise FileExistsError(errno.EEXIST, "File exists", self.display(path))
        try:
            self._request("PUT", bucket, key + "/", body=b"", headers={"Content-Length": "0"})
        except FileNotFoundError:  # no such bucket: missing parents are created
            self._create_bucket(bucket, exist_ok=True)
            self._request("PUT", bucket, key + "/", body=b"", headers={"Content-Length": "0"})

    def _create_bucket(self, bucket: str, exist_ok: bool) -> None:
        if not exist_ok and self.exists("/" + bucket):  # us-east-1 answers "200 OK" to a second create
            raise FileExistsError(errno.EEXIST, "File exists", self.display("/" + bucket))
        region = self.region or DEFAULT_REGION
        self._regions[bucket] = region
        body = b""
        if region != DEFAULT_REGION and self.aws:  # elsewhere the server decides
            body = (f'<CreateBucketConfiguration xmlns="{NS[1:-1]}"><LocationConstraint>{region}'
                    "</LocationConstraint></CreateBucketConfiguration>").encode()
        try:
            self._request("PUT", bucket, body=body, headers={"Content-Length": str(len(body))},
                          what=f"Creating bucket {bucket}")
        except FileExistsError:
            if not exist_ok:
                raise

    def remove(self, path: str) -> None:
        bucket, key = self._split(path)
        if bucket is None:
            raise PermissionError(errno.EACCES, "Can't delete the list of buckets", self.display(path))
        entry = self.stat(path)
        if not key:  # a whole bucket: empty it, then delete it
            self._delete_keys(bucket, [name for _, name, _ in self._list(bucket, "", delimiter=False)])
            self._request("DELETE", bucket, ok=(200, 204), what=f"Deleting bucket {bucket}")
            self._regions.pop(bucket, None)
            return
        if entry.is_dir:
            keys = [name for _, name, _ in self._list(bucket, key + "/", delimiter=False)]
            self._delete_keys(bucket, keys)
        else:
            self._request("DELETE", bucket, key, ok=(200, 204))
        self._keep_folder(bucket, posixpath.dirname(key))

    def _delete_keys(self, bucket: str, keys: list[str]) -> None:
        for start in range(0, len(keys), DELETE_BATCH):
            batch = keys[start : start + DELETE_BATCH]
            body = ("<Delete><Quiet>true</Quiet>"
                    + "".join(f"<Object><Key>{escape(k)}</Key></Object>" for k in batch)
                    + "</Delete>").encode()
            md5 = base64.b64encode(hashlib.md5(body).digest()).decode()
            try:
                _, data = self._request("POST", bucket, query={"delete": ""}, body=body,
                                        headers={"Content-MD5": md5, "Content-Type": "application/xml"})
            except OSError as exc:
                if getattr(exc, "errno", None) != errno.EIO:
                    raise
                for k in batch:  # a server without DeleteObjects: one at a time
                    self._request("DELETE", bucket, k, ok=(200, 204))
                continue
            root = ET.fromstring(data) if data.strip() else None
            failed = [e for e in (root if root is not None else []) if e.tag.removeprefix(NS) == "Error"]
            if failed:
                first = failed[0]
                raise _error(403 if _text(first, "Code") == "AccessDenied" else 500, "", S3Error(
                    _text(first, "Code"), _text(first, "Message")), f"Deleting {_text(first, 'Key')}")

    def _keep_folder(self, bucket: str, folder: str) -> None:
        """After deleting its last item, a folder keeps existing (as a marker)."""
        if not folder:
            return
        for _ in self._list(bucket, folder + "/", delimiter=False, max_keys=1):
            return
        self._request("PUT", bucket, folder + "/", body=b"", headers={"Content-Length": "0"})

    def rename(self, src: str, dst: str) -> None:
        (sb, sk), (db, dk) = self._split(src), self._split(dst)
        if sb is None or db is None or not sk or not dk:
            raise PermissionError(errno.EACCES, "Buckets can't be renamed", self.display(src))
        if self.exists(dst):
            raise FileExistsError(errno.EEXIST, "File exists", self.display(dst))
        entry = self.stat(src)
        if entry.is_dir:
            sources = [(name, int(_text(e, "Size") or 0)) for _, name, e in self._list(sb, sk + "/", delimiter=False)]
            for name, size in sources:
                self._copy(sb, name, db, dk + "/" + name[len(sk) + 1:], size)
            self._delete_keys(sb, [name for name, _ in sources])
        else:
            self._copy(sb, sk, db, dk, entry.size)
            self._request("DELETE", sb, sk, ok=(200, 204))
        self._keep_folder(sb, posixpath.dirname(sk))

    def _copy(self, src_bucket: str, src_key: str, dst_bucket: str, dst_key: str, size: int) -> None:
        """A copy on the server: in one request up to 5 GB, in parts above."""
        source = quote(f"{src_bucket}/{src_key}", safe="/" + _UNRESERVED)
        if size <= COPY_LIMIT:
            _, data = self._request("PUT", dst_bucket, dst_key, headers={"x-amz-copy-source": source},
                                    body=b"")
            self._raise_embedded_error(data, f"Copying to {self.display('/' + dst_bucket + '/' + dst_key)}")
            return
        upload = _Multipart(self, dst_bucket, dst_key)
        try:
            for number, start in enumerate(range(0, size, COPY_PART_SIZE), 1):
                end = min(start + COPY_PART_SIZE, size) - 1
                response, data = self._request(
                    "PUT", dst_bucket, dst_key, query={"partNumber": number, "uploadId": upload.id}, body=b"",
                    headers={"x-amz-copy-source": source, "x-amz-copy-source-range": f"bytes={start}-{end}"})
                self._raise_embedded_error(data, "Copying a part")
                upload.etags.append(_text(ET.fromstring(data), "ETag"))
            upload.complete()
        except BaseException:
            upload.abort()
            raise

    def _raise_embedded_error(self, data: bytes, what: str) -> None:
        """Copies and completed uploads can fail after "200 OK", with an error in the body."""
        if b"<Error>" in data[:200]:
            raise _error(500, "", S3Error.parse(data), what)

    def open_write(self, path: str) -> BinaryIO:
        bucket, key = self._split(path)
        if bucket is None or not key:
            raise PermissionError(errno.EACCES, "Files go into a bucket, not next to the buckets",
                                  self.display(path))
        return _Upload(self, bucket, key)  # type: ignore[return-value]

    def close(self) -> None:
        self._closed = True
        with self._lock:
            clients, self._clients = list(self._clients.values()), {}
        for client in clients:
            client.close()

    @property
    def connected(self) -> bool:
        return not self._closed


class _Multipart:
    """A multipart upload: started, parts added, then completed or aborted."""

    def __init__(self, filesystem: S3FileSystem, bucket: str, key: str, headers: dict[str, str] | None = None) -> None:
        self.fs, self.bucket, self.key = filesystem, bucket, key
        _, data = filesystem._request("POST", bucket, key, query={"uploads": ""}, headers=headers, body=b"")
        self.id = _text(ET.fromstring(data), "UploadId")
        if not self.id:
            raise OSError(errno.EIO, f"{filesystem.type_name} started no upload for {key}")
        self.etags: list[str] = []

    def upload(self, data: bytes) -> None:
        response, _ = self.fs._request("PUT", self.bucket, self.key,
                                       query={"partNumber": len(self.etags) + 1, "uploadId": self.id}, body=data,
                                       headers={"Content-Length": str(len(data))})
        self.etags.append(response.headers.get("ETag", ""))

    def complete(self) -> None:
        body = ("<CompleteMultipartUpload>"
                + "".join(f"<Part><PartNumber>{n}</PartNumber><ETag>{escape(etag)}</ETag></Part>"
                          for n, etag in enumerate(self.etags, 1))
                + "</CompleteMultipartUpload>").encode()
        _, data = self.fs._request("POST", self.bucket, self.key, query={"uploadId": self.id}, body=body,
                                   headers={"Content-Type": "application/xml"})
        self.fs._raise_embedded_error(data, f"Uploading {self.fs.display('/' + self.bucket + '/' + self.key)}")

    def abort(self) -> None:
        try:
            self.fs._request("DELETE", self.bucket, self.key, query={"uploadId": self.id}, ok=(200, 204))
        except (OSError, ConnectionError):
            pass  # the server drops abandoned uploads eventually (or a lifecycle rule does)


class _Upload(io.RawIOBase):
    """Writes go up in parts as they come (no local spool); small files in one PUT
    when closed. Leaving a `with` block through an exception uploads nothing."""

    def __init__(self, filesystem: S3FileSystem, bucket: str, key: str) -> None:
        super().__init__()
        self._fs, self._bucket, self._key = filesystem, bucket, key
        self._buffer = bytearray()
        self._multipart: _Multipart | None = None
        content_type = mimetypes.guess_type(key)[0] or "application/octet-stream"
        self._headers = {"Content-Type": content_type}

    def writable(self) -> bool:
        return True

    def _part_size(self) -> int:
        parts = len(self._multipart.etags) if self._multipart else 0
        return PART_SIZE * 2 ** (parts // 1000)

    def write(self, data) -> int:
        self._buffer += data
        while len(self._buffer) >= self._part_size():
            size = self._part_size()
            if self._multipart is None:
                self._multipart = _Multipart(self._fs, self._bucket, self._key, self._headers)
            self._multipart.upload(bytes(self._buffer[:size]))
            del self._buffer[:size]
        return len(data)

    def close(self) -> None:
        if self.closed:
            return
        try:
            if self._multipart is None:
                self._fs._request("PUT", self._bucket, self._key, body=bytes(self._buffer),
                                  headers={**self._headers, "Content-Length": str(len(self._buffer))})
            else:
                if self._buffer:
                    self._multipart.upload(bytes(self._buffer))
                self._multipart.complete()
        except BaseException:
            if self._multipart is not None:
                self._multipart.abort()
            raise
        finally:
            self._buffer = bytearray()
            super().close()

    def discard(self) -> None:
        if self._multipart is not None:
            self._multipart.abort()
        self._buffer = bytearray()
        super().close()

    def __exit__(self, exc_type, exc, tb) -> None:
        if exc_type is not None:
            self.discard()
        else:
            self.close()
