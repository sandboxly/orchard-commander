"""HTTP(S) plumbing shared by the web-based file systems (WebDAV, S3).

Plain standard-library HTTP: one keep-alive connection per thread (like the
SFTP channels), plus a dedicated one per open file, so a long download never
blocks listing. Certificates are checked against the operating system's trust
store (truststore), which also makes HTTPS work in the packaged app. A server
whose certificate isn't trusted, e.g. a NAS with a self-signed one, can be
trusted by its fingerprint once, like an SSH host key; the pinned certificate
is then required on every connection.
"""

from __future__ import annotations

import hashlib
import http.client
import io
import json
import socket
import ssl
import threading
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO, Protocol

from orchard_commander import APP_NAME
from orchard_commander.remote import ConnectionCancelled, describe_connect_error

TIMEOUT = 30.0
READ_BUFFER = 256 * 1024
USER_AGENT = f"{APP_NAME.replace(' ', '')}/1.0"


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


def system_trust_context() -> ssl.SSLContext:
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


def check_certificate(host: str, port: int, *, trusted_certificates: Path,
                      confirm_certificate: Callable[[CertificateQuestion], bool], timeout: float = TIMEOUT) -> str | None:
    """Before the first HTTPS request: None if the system trusts the server, else
    the fingerprint of the certificate the user trusts (asked once, then pinned).
    Raises CertificateChanged, CertificateRejected or ConnectionError."""
    try:
        with socket.create_connection((host, port), timeout=timeout) as sock:
            system_trust_context().wrap_socket(sock, server_hostname=host).close()
        return None
    except ssl.SSLCertVerificationError as exc:
        error = exc
    except (OSError, ssl.SSLError) as exc:
        raise ConnectionError(describe_connect_error(host, port, exc)) from exc
    try:
        fingerprint = _fingerprint(_server_certificate(host, port, timeout))
    except (OSError, ssl.SSLError) as exc:
        raise ConnectionError(describe_connect_error(host, port, exc)) from exc
    store = TrustedCertificates(trusted_certificates)
    known = store.get(host, port)
    if known == fingerprint:
        return fingerprint
    if known is not None:
        raise CertificateChanged(
            f"The certificate of {host} has changed (now {fingerprint}). This could mean someone is "
            "intercepting the connection; refusing to connect."
        )
    problem = error.verify_message or str(error)
    if not confirm_certificate(CertificateQuestion(host, port, fingerprint, problem)):
        raise CertificateRejected(f"The certificate of {host} was not trusted")
    store.add(host, port, fingerprint)
    return fingerprint


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
# Connections and requests
# ---------------------------------------------------------------------------


class Authorizer(Protocol):
    def authorize(self, method: str, url: str, headers: dict[str, str], body: bytes | BinaryIO | None) -> None:
        """Add the credentials to `headers` (called for every attempt)."""

    def challenged(self, response: http.client.HTTPResponse) -> bool:
        """The server answered 401: whether a retry with other credentials makes sense."""


type Body = bytes | BinaryIO | None


class HTTPClient:
    """Keep-alive connections to one server, one per thread."""

    def __init__(self, host: str, port: int, *, secure: bool, pin: str | None = None, timeout: float = TIMEOUT,
                 authorizer: Authorizer | None = None) -> None:
        self.host, self.port, self.secure = host, port, secure
        self.pin = pin
        self.timeout = timeout
        self.authorizer = authorizer
        self._local = threading.local()
        self._connections: list[http.client.HTTPConnection] = []
        self._lock = threading.Lock()
        self.closed = False

    @property
    def netloc(self) -> str:
        """host[:port], the port only if it isn't the scheme's default."""
        return self.host + (f":{self.port}" if self.port != (443 if self.secure else 80) else "")

    @property
    def origin(self) -> str:
        return f"{'https' if self.secure else 'http'}://{self.netloc}"

    def new_connection(self) -> http.client.HTTPConnection:
        if self.pin is not None:
            conn: http.client.HTTPConnection = _PinnedHTTPSConnection(
                self.host, self.port, timeout=self.timeout, pin=self.pin, blocksize=READ_BUFFER)
        elif self.secure:
            conn = http.client.HTTPSConnection(self.host, self.port, timeout=self.timeout,
                                               context=system_trust_context(), blocksize=READ_BUFFER)
        else:
            conn = http.client.HTTPConnection(self.host, self.port, timeout=self.timeout, blocksize=READ_BUFFER)
        with self._lock:
            self._connections.append(conn)
        return conn

    def connection(self) -> http.client.HTTPConnection:
        if self.closed:
            raise ConnectionError(f"Connection to {self.host} was closed")
        conn = getattr(self._local, "conn", None)
        if conn is None:
            conn = self._local.conn = self.new_connection()
        return conn

    def forget(self, conn: http.client.HTTPConnection) -> None:
        conn.close()
        if getattr(self._local, "conn", None) is conn:
            self._local.conn = None
        with self._lock:
            if conn in self._connections:
                self._connections.remove(conn)

    def send(self, method: str, url: str, *, headers: dict[str, str] | None = None, body: Body = None,
             conn: http.client.HTTPConnection | None = None, stream: bool = False) -> tuple[http.client.HTTPResponse, bytes]:
        """One request, retried after a dropped keep-alive connection or an
        authentication challenge. Returns the response and its body (b"" if
        `stream` and successful: then read the response yourself)."""
        own = conn is None
        for attempt in range(4):
            c = self.connection() if own else conn
            assert c is not None
            h = {"User-Agent": USER_AGENT, **(headers or {})}
            if self.authorizer is not None:
                self.authorizer.authorize(method, url, h, body)
            if hasattr(body, "seek"):
                body.seek(0)  # type: ignore[union-attr]
            try:
                c.request(method, url, body=body, headers=h)
                response = c.getresponse()
                data = b"" if stream and response.status < 300 else response.read()
            except CertificateChanged:
                raise
            except (http.client.HTTPException, ConnectionError, BrokenPipeError, socket.timeout, ssl.SSLError,
                    OSError) as exc:
                c.close()
                if own:
                    self.forget(c)
                if attempt == 0 and not isinstance(exc, socket.timeout):
                    continue  # the server closed an idle keep-alive connection
                raise ConnectionError(describe_connect_error(self.host, self.port, exc)) from exc
            if response.status == 401 and self.authorizer is not None and self.authorizer.challenged(response):
                continue
            return response, data
        raise ConnectionError(f"{self.host}: too many retries")

    def close(self) -> None:
        self.closed = True
        with self._lock:
            connections, self._connections = self._connections, []
        for conn in connections:
            conn.close()


class RangeReader(io.RawIOBase):
    """A seekable download: reading streams one GET, seeking starts a new one at
    that offset (Range), so media and archives on a server can be read in place.

    `get(conn, headers)` sends the GET on `conn` (streaming) and returns the
    response if its status is 200, 206 or 416, raising for anything else.
    """

    def __init__(self, client: HTTPClient,
                 get: Callable[[http.client.HTTPConnection, dict[str, str]], http.client.HTTPResponse],
                 size: Callable[[], int]) -> None:
        super().__init__()
        self._client, self._get, self._stat_size = client, get, size
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
            self._size = self._stat_size()
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
                self._client.forget(self._conn)
                self._conn = None

    def _open(self) -> None:
        if self._conn is None:
            self._conn = self._client.new_connection()
        response = self._get(self._conn, {"Range": f"bytes={self._pos}-"} if self._pos else {})
        if response.status == 416:  # at or past the end
            self._response = None
            raise EOFError
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
                self._client.forget(self._conn)
                self._conn = None
        super().close()
