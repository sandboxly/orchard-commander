"""WebDAV file system over HTTP(S): Nextcloud, ownCloud, NAS boxes, Apache/nginx shares.

The HTTP plumbing (keep-alive connections per thread, the system trust store,
user-trusted certificates pinned by fingerprint) is in `httpclient`.

Paths are absolute POSIX paths below the address's root folder: "/" is the
root, e.g. https://cloud.example.com/remote.php/dav/files/me/.
"""

from __future__ import annotations

import base64
import errno
import hashlib
import http.client
import io
import os
import posixpath
import re
import tempfile
import threading
import xml.etree.ElementTree as ET
from collections.abc import Callable, Iterator
from email.utils import formatdate, parsedate_to_datetime
from pathlib import Path
from typing import BinaryIO
from urllib.parse import quote, unquote, urlsplit

from orchard_commander import fs, vfs
from orchard_commander.remote import AuthenticationRequired
from orchard_commander.remote.httpclient import (  # re-exported: the certificate API predates httpclient
    READ_BUFFER,
    TIMEOUT,
    CertificateChanged,
    CertificateQuestion,
    CertificateRejected,
    HTTPClient,
    RangeReader,
    check_certificate,
)

__all__ = ["CertificateChanged", "CertificateQuestion", "CertificateRejected", "WebDAVFileSystem", "connect"]

SPOOL_IN_MEMORY = 16 * 1024**2  # uploads larger than this are spooled to a temp file

DAV = "{DAV:}"
_PROPFIND = (
    b'<?xml version="1.0" encoding="utf-8"?>'
    b'<d:propfind xmlns:d="DAV:"><d:prop>'
    b"<d:resourcetype/><d:getcontentlength/><d:getlastmodified/><d:quota-available-bytes/>"
    b"</d:prop></d:propfind>"
)
_SAFE = "/!$&'()*+,;=:@~"  # left unescaped in URL paths


# ---------------------------------------------------------------------------
# Authentication: Basic, or Digest when the server asks for it
# ---------------------------------------------------------------------------


class _Auth:
    def __init__(self, username: str | None, password: str | None) -> None:
        self.username = username or ""
        self.password = password or ""
        self._lock = threading.Lock()
        self._digest: dict[str, str] | None = None
        self._count = 0

    def authorize(self, method: str, uri: str, headers: dict[str, str], body: object) -> None:
        if not self.username:
            return
        with self._lock:
            if self._digest is None:
                token = base64.b64encode(f"{self.username}:{self.password}".encode()).decode()
                headers["Authorization"] = f"Basic {token}"
                return
            self._count += 1
            headers["Authorization"] = self._digest_header(method, uri, self._digest, self._count)

    def challenged(self, response: http.client.HTTPResponse) -> bool:
        """The server answered 401: whether a retry with other credentials makes sense."""
        if not self.username:
            return False
        for challenge in response.headers.get_all("WWW-Authenticate") or []:
            scheme, _, rest = challenge.strip().partition(" ")
            if scheme.lower() == "digest":
                params = {k.lower(): v1 if v1 else v2 for k, v1, v2 in re.findall(r'(\w+)=(?:"([^"]*)"|([^,\s]*))', rest)}
                with self._lock:
                    retry = self._digest is None or params.get("stale", "").lower() == "true"
                    self._digest, self._count = params, 0
                return retry
        return False

    def _digest_header(self, method: str, uri: str, c: dict[str, str], count: int) -> str:
        algorithm = c.get("algorithm", "MD5")
        hash_name = "sha256" if algorithm.upper().startswith("SHA-256") else "md5"

        def h(text: str) -> str:
            return hashlib.new(hash_name, text.encode()).hexdigest()

        realm, nonce = c.get("realm", ""), c.get("nonce", "")
        cnonce = os.urandom(8).hex()
        nc = f"{count:08x}"
        ha1 = h(f"{self.username}:{realm}:{self.password}")
        if algorithm.lower().endswith("-sess"):
            ha1 = h(f"{ha1}:{nonce}:{cnonce}")
        ha2 = h(f"{method}:{uri}")
        qop = "auth" if "auth" in [q.strip() for q in c.get("qop", "").split(",")] else ""
        response = h(f"{ha1}:{nonce}:{nc}:{cnonce}:{qop}:{ha2}") if qop else h(f"{ha1}:{nonce}:{ha2}")
        parts = [f'username="{self.username}"', f'realm="{realm}"', f'nonce="{nonce}"', f'uri="{uri}"',
                 f"algorithm={algorithm}", f'response="{response}"']
        if qop:
            parts += [f"qop={qop}", f"nc={nc}", f'cnonce="{cnonce}"']
        if "opaque" in c:
            parts.append(f'opaque="{c["opaque"]}"')
        return "Digest " + ", ".join(parts)


# ---------------------------------------------------------------------------
# Connecting
# ---------------------------------------------------------------------------


def connect(
    host: str,
    *,
    secure: bool = True,
    port: int | None = None,
    root: str = "/",
    username: str | None = None,
    password: str | None = None,
    trusted_certificates: Path,
    confirm_certificate: Callable[[CertificateQuestion], bool],
    timeout: float = TIMEOUT,
) -> WebDAVFileSystem:
    """Connect and check the root folder. Raises AuthenticationRequired,
    CertificateChanged, CertificateRejected or ConnectionError."""
    port = port or (443 if secure else 80)
    pin = None
    if secure:
        pin = check_certificate(host, port, trusted_certificates=trusted_certificates,
                                confirm_certificate=confirm_certificate, timeout=timeout)
    filesystem = WebDAVFileSystem(host, port, secure=secure, root=root, username=username, password=password,
                                  pin=pin, timeout=timeout)
    try:
        filesystem.check_root()
    except BaseException:
        filesystem.close()
        raise
    return filesystem


# ---------------------------------------------------------------------------
# The file system
# ---------------------------------------------------------------------------


class WebDAVFileSystem(vfs.FileSystem):
    writable = True
    can_rename = True

    def __init__(self, host: str, port: int, *, secure: bool, root: str = "/", username: str | None = None,
                 password: str | None = None, pin: str | None = None, timeout: float = TIMEOUT) -> None:
        self.host, self.port, self.secure = host, port, secure
        self.root = "/" + root.strip("/") if root.strip("/") else ""  # URL path prefix, no trailing slash
        self.http = HTTPClient(host, port, secure=secure, pin=pin, timeout=timeout,
                               authorizer=_Auth(username, password))
        self._quota: dict[str, int] = {}
        self._mtime_method: str | None = None  # how this server accepts modification times; "" = it doesn't
        self.label = (f"{username}@" if username else "") + self.http.netloc
        self.type_name = f"{'webdavs' if secure else 'webdav'}://{self.label}"

    def url_path(self, path: str) -> str:
        return quote(self.root + posixpath.normpath("/" + path.lstrip("/")), safe=_SAFE)

    def _send(self, method: str, url_path: str, *, headers: dict[str, str] | None = None,
              body: bytes | BinaryIO | None = None, conn: http.client.HTTPConnection | None = None,
              stream: bool = False) -> tuple[http.client.HTTPResponse, bytes]:
        response, data = self.http.send(method, url_path, headers=headers, body=body, conn=conn, stream=stream)
        if response.status == 401:
            raise AuthenticationRequired(f"{self.type_name} didn't accept the user name and password")
        return response, data

    def _request(self, method: str, path: str, *, ok: tuple[int, ...], collection: bool = False,
                 headers: dict[str, str] | None = None, body: bytes | BinaryIO | None = None) -> bytes:
        url = self.url_path(path)
        if collection and not url.endswith("/"):
            url += "/"
        response, data = self._send(method, url, headers=headers, body=body)
        if response.status in (301, 302, 307, 308) and not collection and method in ("PROPFIND", "DELETE", "MOVE"):
            location = urlsplit(response.headers.get("Location", "")).path
            if location.rstrip("/") == url.rstrip("/"):  # just the trailing slash of a folder
                return self._request(method, path, ok=ok, collection=True, headers=headers, body=body)
        if response.status not in ok:
            raise _error(response.status, response.reason, f"{method} {self.display(path)}")
        return data

    # -- reading -------------------------------------------------------------------

    def check_root(self) -> None:
        if not self.stat("/").is_dir:
            raise ConnectionError(f"{self.display('/')} is not a folder")

    def _propfind(self, path: str, depth: str) -> list[tuple[str, dict[str, ET.Element]]]:
        data = self._request("PROPFIND", path, ok=(207,), collection=depth == "1", body=_PROPFIND,
                             headers={"Depth": depth, "Content-Type": "application/xml; charset=utf-8"})
        try:
            tree = ET.fromstring(data)
        except ET.ParseError as exc:
            raise OSError(errno.EIO, f"{self.type_name} sent an invalid listing: {exc}") from exc
        result = []
        for response in tree.iter(f"{DAV}response"):
            href = unquote(urlsplit(response.findtext(f"{DAV}href") or "").path)
            props: dict[str, ET.Element] = {}
            for propstat in response.findall(f"{DAV}propstat"):
                if " 200" not in (propstat.findtext(f"{DAV}status") or ""):
                    continue
                for prop in propstat.findall(f"{DAV}prop"):
                    for child in prop:
                        props[child.tag] = child
            result.append((href, props))
        return result

    def _entry(self, name: str, props: dict[str, ET.Element]) -> fs.Entry:
        kind = props.get(f"{DAV}resourcetype")
        is_dir = kind is not None and kind.find(f"{DAV}collection") is not None
        try:
            size = 0 if is_dir else int((props[f"{DAV}getcontentlength"].text or "0").strip())
        except (KeyError, ValueError):
            size = 0
        try:
            mtime = parsedate_to_datetime((props[f"{DAV}getlastmodified"].text or "").strip()).timestamp()
        except (KeyError, ValueError, TypeError, IndexError):
            mtime = 0.0
        return fs.Entry(name=name, is_dir=is_dir, is_link=False, is_hidden=name.startswith("."),
                        is_executable=False, size=size, mtime=mtime)

    def list_dir(self, path: str, **kwargs) -> list[fs.Entry]:
        own = unquote(self.url_path(path)).rstrip("/")
        entries = []
        for href, props in self._propfind(path, "1"):
            if href.rstrip("/") == own:  # the folder itself
                quota = props.get(f"{DAV}quota-available-bytes")
                if quota is not None and (quota.text or "").strip().isdigit():
                    self._quota[path] = int(quota.text.strip())  # type: ignore[union-attr]
                continue
            name = posixpath.basename(href.rstrip("/"))
            if name:
                entries.append(self._entry(name, props))
        return fs.sort_entries(entries, **kwargs)

    def stat(self, path: str) -> fs.Entry:
        responses = self._propfind(path, "0")
        if not responses:
            raise FileNotFoundError(errno.ENOENT, "Not found", self.display(path))
        return self._entry(self.basename(path), responses[0][1])

    def parent_path(self, path: str) -> str | None:
        path = posixpath.normpath("/" + path.lstrip("/"))
        return None if path == "/" else posixpath.dirname(path)

    def join(self, path: str, *names: str) -> str:
        return posixpath.join(path, *names)

    def basename(self, path: str) -> str:
        return posixpath.basename(path.rstrip("/")) or self.host

    def display(self, path: str) -> str:
        return f"{self.type_name}{self.root}{posixpath.normpath('/' + path.lstrip('/'))}"

    def free_space(self, path: str) -> int | None:
        return self._quota.get(path)  # from the folder's last listing (RFC 4331), if the server says

    def open_read(self, path: str) -> BinaryIO:
        def get(conn: http.client.HTTPConnection, headers: dict[str, str]) -> http.client.HTTPResponse:
            response, _ = self._send("GET", self.url_path(path), headers=headers, conn=conn, stream=True)
            if response.status not in (200, 206, 416):
                raise _error(response.status, response.reason, f"GET {self.display(path)}")
            return response

        reader = RangeReader(self.http, get, lambda: self.stat(path).size)
        return io.BufferedReader(reader, buffer_size=READ_BUFFER)  # type: ignore[return-value]

    def walk(self, path: str) -> Iterator[tuple[vfs.RelPath, fs.Entry]]:
        root = self.stat(path)
        yield (), root
        if root.is_dir:
            yield from self._walk(path, ())

    def _walk(self, path: str, rel: vfs.RelPath) -> Iterator[tuple[vfs.RelPath, fs.Entry]]:
        for entry in sorted(self.list_dir(path, show_hidden=True), key=lambda e: e.name):
            yield (*rel, entry.name), entry
            if entry.is_dir:
                yield from self._walk(posixpath.join(path, entry.name), (*rel, entry.name))

    # -- writing -------------------------------------------------------------------

    def mkdir(self, path: str, *, exist_ok: bool = False) -> None:
        missing = []
        p = posixpath.normpath("/" + path.lstrip("/"))
        while p != "/" and not self.exists(p):
            missing.append(p)
            p = posixpath.dirname(p)
        if not missing:
            if exist_ok and self.is_dir(path):
                return
            raise FileExistsError(errno.EEXIST, "File exists", self.display(path))
        for p in reversed(missing):
            try:
                self._request("MKCOL", p, ok=(200, 201, 204), collection=True)
            except PermissionError:
                if not self.is_dir(p):  # 405: something is in the way (or a race)
                    raise
                if not exist_ok and p == missing[0]:
                    raise FileExistsError(errno.EEXIST, "File exists", self.display(p)) from None

    def remove(self, path: str) -> None:
        self._request("DELETE", path, ok=(200, 202, 204), collection=self.is_dir(path))

    def rename(self, src: str, dst: str) -> None:
        collection = self.is_dir(src)
        target = self.http.origin + self.url_path(dst) + ("/" if collection else "")
        self._request("MOVE", src, ok=(201, 204), collection=collection,
                      headers={"Destination": target, "Overwrite": "F"})

    def open_write(self, path: str) -> BinaryIO:
        return _Upload(self, path)  # type: ignore[return-value]

    def set_mtime(self, path: str, mtime: float) -> None:
        """Best effort: servers differ in which property, if any, they let clients set."""
        methods = [self._mtime_method] if self._mtime_method is not None else list(_MTIME_PROPERTIES)
        for method in methods:
            if not method:
                return  # this server accepts none of them
            body = _MTIME_PROPERTIES[method](mtime)
            try:
                data = self._request("PROPPATCH", path, ok=(207,), body=body,
                                     headers={"Content-Type": "application/xml; charset=utf-8"})
            except OSError:
                continue
            if b" 200" not in data or any(code in data for code in (b" 403", b" 409", b" 424")):
                continue
            if self._mtime_method is None:  # finding out: some servers just store the
                try:                        # property and leave the file's date alone
                    if abs(self.stat(path).mtime - mtime) >= 2:
                        continue
                except OSError:
                    continue
                self._mtime_method = method
            return
        if self._mtime_method is None:
            self._mtime_method = ""

    def close(self) -> None:
        self.http.close()

    @property
    def connected(self) -> bool:
        return not self.http.closed


def _proppatch(namespace: str, prefix: str, name: str, value: str) -> bytes:
    return (f'<?xml version="1.0" encoding="utf-8"?><d:propertyupdate xmlns:d="DAV:" xmlns:{prefix}="{namespace}">'
            f"<d:set><d:prop><{prefix}:{name}>{value}</{prefix}:{name}></d:prop></d:set></d:propertyupdate>").encode()


_MTIME_PROPERTIES: dict[str, Callable[[float], bytes]] = {
    # RFC 4918's own property (read-only on many servers, writable on some).
    "getlastmodified": lambda t: _proppatch("DAV:", "x", "getlastmodified", formatdate(t, usegmt=True)),
    # Nextcloud / ownCloud: seconds since the epoch.
    "lastmodified": lambda t: _proppatch("DAV:", "x", "lastmodified", str(int(t))),
    # IIS and Windows' WebDAV client.
    "win32": lambda t: _proppatch("urn:schemas-microsoft-com:", "z", "Win32LastModifiedTime",
                                  formatdate(t, usegmt=True)),
}


def _error(status: int, reason: str, what: str) -> OSError:
    message = f"{what}: {status} {reason}"
    if status == 404:
        return FileNotFoundError(errno.ENOENT, message)
    if status in (403, 405):
        return PermissionError(errno.EACCES, message)
    if status == 409:  # e.g. the parent folder doesn't exist
        return FileNotFoundError(errno.ENOENT, message)
    if status == 412:
        return FileExistsError(errno.EEXIST, message)
    if status == 507:
        return OSError(errno.ENOSPC, message)
    return OSError(errno.EIO, message)


class _Upload(io.RawIOBase):
    """Collects the written data and uploads it with one PUT when closed — with a
    known length, which every server accepts (some refuse chunked uploads).
    Leaving a `with` block through an exception discards the upload."""

    def __init__(self, filesystem: WebDAVFileSystem, path: str) -> None:
        super().__init__()
        self._fs, self._path = filesystem, path
        self._data = tempfile.SpooledTemporaryFile(max_size=SPOOL_IN_MEMORY)

    def writable(self) -> bool:
        return True

    def write(self, data) -> int:
        return self._data.write(data)

    def close(self) -> None:
        if self.closed:
            return
        try:
            size = self._data.tell()
            self._data.seek(0)
            self._fs._request("PUT", self._path, ok=(200, 201, 204), body=self._data,  # type: ignore[arg-type]
                              headers={"Content-Length": str(size), "Content-Type": "application/octet-stream"})
        finally:
            self._data.close()
            super().close()

    def discard(self) -> None:
        self._data.close()
        super().close()

    def __exit__(self, exc_type, exc, tb) -> None:
        if exc_type is not None:
            self.discard()
        else:
            self.close()
