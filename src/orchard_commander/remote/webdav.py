"""WebDAV file system over HTTP(S): Nextcloud, ownCloud, NAS boxes, Apache/nginx shares.

Plain standard-library HTTP: one keep-alive connection per thread (like the
SFTP channels), plus a dedicated one per open file, so a long download never
blocks listing. Certificates are checked against the operating system's trust
store (truststore), which also makes HTTPS work in the packaged app. A server
whose certificate isn't trusted, e.g. a NAS with a self-signed one, can be
trusted by its fingerprint once, like an SSH host key; the pinned certificate
is then required on every connection.

Paths are absolute POSIX paths below the address's root folder: "/" is the
root, e.g. https://cloud.example.com/remote.php/dav/files/me/.
"""

from __future__ import annotations

import base64
import errno
import hashlib
import http.client
import io
import json
import os
import posixpath
import re
import socket
import ssl
import tempfile
import threading
import xml.etree.ElementTree as ET
from collections.abc import Callable, Iterator
from dataclasses import dataclass
from email.utils import formatdate, parsedate_to_datetime
from pathlib import Path
from typing import BinaryIO
from urllib.parse import quote, unquote, urlsplit

from orchard_commander import APP_NAME, fs, vfs
from orchard_commander.remote import AuthenticationRequired, ConnectionCancelled, describe_connect_error

TIMEOUT = 30.0
SPOOL_IN_MEMORY = 16 * 1024**2  # uploads larger than this are spooled to a temp file
READ_BUFFER = 256 * 1024
USER_AGENT = f"{APP_NAME.replace(' ', '')}/1.0"

DAV = "{DAV:}"
_PROPFIND = (
    b'<?xml version="1.0" encoding="utf-8"?>'
    b'<d:propfind xmlns:d="DAV:"><d:prop>'
    b"<d:resourcetype/><d:getcontentlength/><d:getlastmodified/><d:quota-available-bytes/>"
    b"</d:prop></d:propfind>"
)
_SAFE = "/!$&'()*+,;=:@~"  # left unescaped in URL paths


# ---------------------------------------------------------------------------
# Certificates
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class CertificateQuestion:
    host: str
    port: int
    fingerprint: str  # "SHA256:aa:bb:…"
    problem: str  # why the system doesn't trust it, e.g. "self-signed certificate"


class CertificateChanged(Exception):
    """The server's certificate differs from the one the user trusted: possible attack."""


class CertificateRejected(ConnectionCancelled):
    """The user declined to trust the server's certificate."""


def _fingerprint(der: bytes) -> str:
    digest = hashlib.sha256(der).hexdigest().upper()
    return "SHA256:" + ":".join(digest[i : i + 2] for i in range(0, len(digest), 2))


def _system_trust_context() -> ssl.SSLContext:
    try:
        import truststore

        return truststore.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    except Exception:  # truststore unavailable: Python's own CA handling
        return ssl.create_default_context()


def _unverified_context() -> ssl.SSLContext:
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    return context


def _server_certificate(host: str, port: int, timeout: float) -> bytes:
    with socket.create_connection((host, port), timeout=timeout) as sock:
        with _unverified_context().wrap_socket(sock, server_hostname=host) as tls:
            der = tls.getpeercert(binary_form=True)
    if not der:
        raise ConnectionError(f"{host}:{port} sent no certificate")
    return der


class TrustedCertificates:
    """Certificates the user chose to trust, by host and port (a small JSON file)."""

    def __init__(self, path: Path) -> None:
        self.path = path

    def _load(self) -> dict[str, str]:
        try:
            data = json.loads(self.path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return {}
        return data if isinstance(data, dict) else {}

    def get(self, host: str, port: int) -> str | None:
        return self._load().get(f"{host}:{port}")

    def add(self, host: str, port: int, fingerprint: str) -> None:
        data = self._load()
        data[f"{host}:{port}"] = fingerprint
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.path.write_text(json.dumps(data, indent=2), encoding="utf-8")


class _PinnedHTTPSConnection(http.client.HTTPSConnection):
    """HTTPS to a server with a user-trusted certificate: exactly that one is accepted."""

    def __init__(self, *args, pin: str, **kwargs) -> None:
        super().__init__(*args, context=_unverified_context(), **kwargs)
        self._pin = pin

    def connect(self) -> None:
        super().connect()
        der = self.sock.getpeercert(binary_form=True)  # type: ignore[union-attr]
        if not der or _fingerprint(der) != self._pin:
            self.close()
            raise CertificateChanged(
                f"The certificate of {self.host} has changed. This could mean someone is "
                "intercepting the connection; refusing to connect."
            )


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

    def header(self, method: str, uri: str) -> str | None:
        if not self.username:
            return None
        with self._lock:
            if self._digest is None:
                token = base64.b64encode(f"{self.username}:{self.password}".encode()).decode()
                return f"Basic {token}"
            self._count += 1
            return self._digest_header(method, uri, self._digest, self._count)

    def challenged(self, challenges: list[str]) -> bool:
        """The server answered 401: whether a retry with other credentials makes sense."""
        if not self.username:
            return False
        for challenge in challenges:
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
        try:  # does the system trust the server's certificate?
            with socket.create_connection((host, port), timeout=timeout) as sock:
                _system_trust_context().wrap_socket(sock, server_hostname=host).close()
        except ssl.SSLCertVerificationError as exc:
            pin = _trust_certificate(host, port, exc, trusted_certificates, confirm_certificate, timeout)
        except (OSError, ssl.SSLError) as exc:
            raise ConnectionError(describe_connect_error(host, port, exc)) from exc
    filesystem = WebDAVFileSystem(host, port, secure=secure, root=root, username=username, password=password,
                                  pin=pin, timeout=timeout)
    try:
        filesystem.check_root()
    except BaseException:
        filesystem.close()
        raise
    return filesystem


def _trust_certificate(host: str, port: int, error: ssl.SSLCertVerificationError, store_path: Path,
                       confirm: Callable[[CertificateQuestion], bool], timeout: float) -> str:
    try:
        fingerprint = _fingerprint(_server_certificate(host, port, timeout))
    except (OSError, ssl.SSLError) as exc:
        raise ConnectionError(describe_connect_error(host, port, exc)) from exc
    store = TrustedCertificates(store_path)
    known = store.get(host, port)
    if known == fingerprint:
        return fingerprint
    if known is not None:
        raise CertificateChanged(
            f"The certificate of {host} has changed (now {fingerprint}). This could mean someone is "
            "intercepting the connection; refusing to connect."
        )
    problem = error.verify_message or str(error)
    if not confirm(CertificateQuestion(host, port, fingerprint, problem)):
        raise CertificateRejected(f"The certificate of {host} was not trusted")
    store.add(host, port, fingerprint)
    return fingerprint


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
        self.pin = pin
        self.timeout = timeout
        self._auth = _Auth(username, password)
        self._local = threading.local()
        self._connections: list[http.client.HTTPConnection] = []
        self._lock = threading.Lock()
        self._closed = False
        self._quota: dict[str, int] = {}
        self._mtime_method: str | None = None  # how this server accepts modification times; "" = it doesn't
        scheme = "webdavs" if secure else "webdav"
        default = 443 if secure else 80
        self.label = (f"{username}@" if username else "") + host + (f":{port}" if port != default else "")
        self.type_name = f"{scheme}://{self.label}"
        self._origin = f"{'https' if secure else 'http'}://{host}" + (f":{port}" if port != default else "")

    # -- HTTP --------------------------------------------------------------------

    def _new_connection(self) -> http.client.HTTPConnection:
        if self.pin is not None:
            conn: http.client.HTTPConnection = _PinnedHTTPSConnection(
                self.host, self.port, timeout=self.timeout, pin=self.pin, blocksize=READ_BUFFER)
        elif self.secure:
            conn = http.client.HTTPSConnection(self.host, self.port, timeout=self.timeout,
                                               context=_system_trust_context(), blocksize=READ_BUFFER)
        else:
            conn = http.client.HTTPConnection(self.host, self.port, timeout=self.timeout, blocksize=READ_BUFFER)
        with self._lock:
            self._connections.append(conn)
        return conn

    def _connection(self) -> http.client.HTTPConnection:
        if self._closed:
            raise ConnectionError(f"Connection to {self.label} was closed")
        conn = getattr(self._local, "conn", None)
        if conn is None:
            conn = self._local.conn = self._new_connection()
        return conn

    def _forget(self, conn: http.client.HTTPConnection) -> None:
        conn.close()
        if getattr(self._local, "conn", None) is conn:
            self._local.conn = None
        with self._lock:
            if conn in self._connections:
                self._connections.remove(conn)

    def url_path(self, path: str) -> str:
        return quote(self.root + posixpath.normpath("/" + path.lstrip("/")), safe=_SAFE)

    def _send(self, method: str, url_path: str, *, headers: dict[str, str] | None = None,
              body: bytes | BinaryIO | None = None, conn: http.client.HTTPConnection | None = None,
              stream: bool = False) -> tuple[http.client.HTTPResponse, bytes]:
        """One request, retried after a dropped keep-alive connection or an
        authentication challenge. Returns the response and its body (b"" if `stream`)."""
        own = conn is None
        for attempt in range(4):
            c = self._connection() if own else conn
            assert c is not None
            h = {"User-Agent": USER_AGENT, **(headers or {})}
            if (auth := self._auth.header(method, url_path)) is not None:
                h["Authorization"] = auth
            if hasattr(body, "seek"):
                body.seek(0)  # type: ignore[union-attr]
            try:
                c.request(method, url_path, body=body, headers=h)
                response = c.getresponse()
                data = b"" if stream and response.status < 300 else response.read()
            except CertificateChanged:
                raise
            except (http.client.HTTPException, ConnectionError, BrokenPipeError, socket.timeout, ssl.SSLError,
                    OSError) as exc:
                c.close()
                if own:
                    self._forget(c)
                if attempt == 0 and not isinstance(exc, socket.timeout):
                    continue  # the server closed an idle keep-alive connection
                raise ConnectionError(describe_connect_error(self.host, self.port, exc)) from exc
            if response.status == 401 and self._auth.challenged(response.headers.get_all("WWW-Authenticate") or []):
                continue
            if response.status == 401:
                raise AuthenticationRequired(f"{self.type_name} didn't accept the user name and password")
            return response, data
        raise ConnectionError(f"{self.type_name}: too many retries")

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
        return io.BufferedReader(_RangeReader(self, path), buffer_size=READ_BUFFER)  # type: ignore[return-value]

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
        target = self._origin + self.url_path(dst) + ("/" if collection else "")
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
        self._closed = True
        with self._lock:
            connections, self._connections = self._connections, []
        for conn in connections:
            conn.close()

    @property
    def connected(self) -> bool:
        return not self._closed


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


class _RangeReader(io.RawIOBase):
    """A seekable download: reading streams one GET, seeking starts a new one at
    that offset (Range), so media and archives on the server can be read in place."""

    def __init__(self, filesystem: WebDAVFileSystem, path: str) -> None:
        super().__init__()
        self._fs, self._path = filesystem, path
        self._pos = 0
        self._size: int | None = None
        self._conn: http.client.HTTPConnection | None = None
        self._response: http.client.HTTPResponse | None = None

    def readable(self) -> bool:
        return True

    def seekable(self) -> bool:
        return True

    def tell(self) -> int:
        return self._pos

    def _size_now(self) -> int:
        if self._size is None:
            self._size = self._fs.stat(self._path).size
        return self._size

    def seek(self, offset: int, whence: int = io.SEEK_SET) -> int:
        if whence == io.SEEK_CUR:
            offset += self._pos
        elif whence == io.SEEK_END:
            offset += self._size_now()
        if offset < 0:
            raise ValueError("negative seek position")
        if offset != self._pos:
            self._drop_response()
            self._pos = offset
        return self._pos

    def _drop_response(self) -> None:
        if self._response is not None:
            self._response.close()
            self._response = None
            if self._conn is not None:  # an unfinished response can't be reused
                self._fs._forget(self._conn)
                self._conn = None

    def _open(self) -> None:
        if self._conn is None:
            self._conn = self._fs._new_connection()
        headers = {"Range": f"bytes={self._pos}-"} if self._pos else {}
        response, data = self._fs._send("GET", self._fs.url_path(self._path), headers=headers,
                                        conn=self._conn, stream=True)
        if response.status == 416:  # at or past the end
            self._response = None
            raise EOFError
        if response.status not in (200, 206):
            raise _error(response.status, response.reason, f"GET {self._fs.display(self._path)}")
        self._response = response
        if response.status == 206:
            total = response.headers.get("Content-Range", "").rpartition("/")[2]
            if total.isdigit():
                self._size = int(total)
        else:
            length = response.headers.get("Content-Length", "")
            if length.isdigit():
                self._size = int(length)
            skip = self._pos  # the server ignored Range: read up to the position
            while skip > 0:
                chunk = response.read(min(skip, READ_BUFFER))
                if not chunk:
                    break
                skip -= len(chunk)

    def readinto(self, buffer) -> int:
        if self._size is not None and self._pos >= self._size:
            return 0
        if self._response is None:
            try:
                self._open()
            except EOFError:
                return 0
        assert self._response is not None
        n = self._response.readinto(buffer)
        self._pos += n
        return n

    def close(self) -> None:
        if not self.closed:
            self._drop_response()
            if self._conn is not None:
                self._fs._forget(self._conn)
                self._conn = None
        super().close()


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
