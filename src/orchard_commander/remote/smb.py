"""SMB (Windows / Samba share) file system, via smbprotocol (SMB 2/3, pure Python).

One file system is one share. Paths are POSIX-style absolute paths inside the
share ("/" is the share root) and are translated to UNC paths for smbclient.
Each instance has its own connection cache, so two connections to the same
server with different credentials don't interfere.

Requests are serialized per connection: smbprotocol sizes each request by
the SMB credits free at that moment and fails instead of waiting when there
are none, so concurrent requests (listing while a preview downloads) exhaust
the credits of servers that grant few, such as many NAS boxes.
"""

from __future__ import annotations

import errno
import io
import posixpath
import stat
import threading
from collections.abc import Iterator
from typing import Any, BinaryIO

import smbclient
import smbclient.shutil
from smbprotocol.exceptions import LogonFailure, SMBAuthenticationError, SMBException, SMBOSError

from orchard_commander import fs, vfs
from orchard_commander.remote import AuthenticationRequired, describe_connect_error

DEFAULT_PORT = 445
FILE_ATTRIBUTE_HIDDEN = 0x2
FILE_ATTRIBUTE_DIRECTORY = 0x10


def connect(
    server: str,
    share: str,
    *,
    port: int = DEFAULT_PORT,
    username: str | None = None,
    password: str | None = None,
    timeout: float = 15.0,
) -> SMBFileSystem:
    """Connect to `\\\\server\\share`. Raises AuthenticationRequired or ConnectionError."""
    remote = SMBFileSystem(server, share, port=port, username=username, password=password)
    try:
        smbclient.register_session(
            server,
            username=username,
            password=password,
            port=port,
            connection_timeout=timeout,
            connection_cache=remote._cache,
        )
        remote.list_dir("/")  # proves the share exists and is readable
    except (LogonFailure, SMBAuthenticationError) as exc:
        remote.close()
        raise AuthenticationRequired(f"Authentication failed for {username or 'guest'} on {server}") from exc
    except Exception as exc:
        remote.close()
        if _is_auth_error(exc):
            raise AuthenticationRequired(f"Authentication failed for {username or 'guest'} on {server}") from exc
        raise ConnectionError(describe_connect_error(server, port, exc)) from exc
    return remote


def _is_auth_error(exc: BaseException) -> bool:
    text = f"{type(exc).__name__} {exc}".lower()
    return any(word in text for word in ("logon", "authentication", "credentials", "access_denied"))


# NT status codes smbprotocol doesn't map to an errno itself.
_NTSTATUS_ERRNO = {
    0xC000000F: errno.ENOENT,  # STATUS_NO_SUCH_FILE
    0xC0000034: errno.ENOENT,  # STATUS_OBJECT_NAME_NOT_FOUND
    0xC000003A: errno.ENOENT,  # STATUS_OBJECT_PATH_NOT_FOUND
    0xC0000035: errno.EEXIST,  # STATUS_OBJECT_NAME_COLLISION
    0xC0000022: errno.EACCES,  # STATUS_ACCESS_DENIED
    0xC0000101: errno.ENOTEMPTY,  # STATUS_DIRECTORY_NOT_EMPTY
}


def _translate(exc: SMBOSError) -> OSError:
    """smbclient raises SMBOSError for everything; give callers the usual OSError subclasses."""
    code = exc.errno or _NTSTATUS_ERRNO.get(getattr(exc, "ntstatus", 0) or 0, 0)
    if code:
        return OSError(code, exc.strerror or str(exc), exc.filename)  # becomes FileNotFoundError etc.
    return exc


class SMBFileSystem(vfs.FileSystem):
    writable = True
    can_rename = True

    def __init__(
        self, server: str, share: str, *, port: int = DEFAULT_PORT, username: str | None, password: str | None
    ) -> None:
        self.server = server
        self.share = share.strip("/\\")
        self.port = port
        self._cache: dict[str, Any] = {}
        self._lock = threading.RLock()  # one request in flight per connection
        self._kw: dict[str, Any] = {
            "username": username,
            "password": password,
            "port": port,
            "connection_cache": self._cache,
        }
        host = server if port == DEFAULT_PORT else f"{server}:{port}"
        self.type_name = f"smb://{host}/{self.share}"

    @property
    def read_only_reason(self) -> str:
        return f"{self.type_name} refused the operation"

    def _unc(self, path: str) -> str:
        inner = path.strip("/").replace("/", "\\")
        return f"\\\\{self.server}\\{self.share}" + (f"\\{inner}" if inner else "")

    def _call(self, fn: Any, *args: Any, **kwargs: Any) -> Any:
        try:
            with self._lock:
                result = fn(*args, **kwargs, **self._kw)
                if fn is smbclient.scandir:
                    result = list(result)  # the iterator makes requests too
                return result
        except SMBOSError as exc:
            raise _translate(exc) from exc
        except SMBException as exc:
            raise ConnectionError(f"{self.type_name}: {exc}") from exc

    # -- reading -------------------------------------------------------------------

    @staticmethod
    def _entry(name: str, st: Any, is_link: bool = False) -> fs.Entry:
        is_dir = stat.S_ISDIR(st.st_mode)
        attrs = getattr(st, "st_file_attributes", 0)
        return fs.Entry(
            name=name,
            is_dir=is_dir,
            is_link=is_link,
            is_hidden=name.startswith(".") or bool(attrs & FILE_ATTRIBUTE_HIDDEN),
            is_executable=False,
            size=0 if is_dir else st.st_size,
            mtime=float(st.st_mtime),
        )

    def list_dir(self, path: str, **kwargs) -> list[fs.Entry]:
        entries = []
        for de in self._call(smbclient.scandir, self._unc(path)):
            if de.name in (".", ".."):
                continue
            # The listing already carries size/times/attributes; de.stat() would
            # cost a round trip per entry.
            info = de.smb_info
            attrs = info.file_attributes
            is_dir = bool(attrs & FILE_ATTRIBUTE_DIRECTORY)
            entries.append(
                fs.Entry(
                    name=de.name,
                    is_dir=is_dir,
                    is_link=de.is_symlink(),
                    is_hidden=de.name.startswith(".") or bool(attrs & FILE_ATTRIBUTE_HIDDEN),
                    is_executable=False,
                    size=0 if is_dir else info.end_of_file,
                    mtime=info.last_write_time.timestamp(),
                )
            )
        return fs.sort_entries(entries, **kwargs)

    def stat(self, path: str) -> fs.Entry:
        st = self._call(smbclient.stat, self._unc(path))
        return self._entry(posixpath.basename(path) or self.share, st)

    def parent_path(self, path: str) -> str | None:
        return None if path in ("", "/") else posixpath.dirname(path)

    def join(self, path: str, *names: str) -> str:
        return posixpath.join(path or "/", *names)

    def basename(self, path: str) -> str:
        return posixpath.basename(path) or self.share

    def display(self, path: str) -> str:
        return self.type_name + (path if path != "/" else "")

    def open_read(self, path: str) -> BinaryIO:
        # Shared, so a preview and a copy (or video playback) can read the same file.
        f = self._call(smbclient.open_file, self._unc(path), mode="rb", share_access="rwd")
        return _Serialized(f, self._lock)  # type: ignore[return-value]

    def walk(self, path: str) -> Iterator[tuple[vfs.RelPath, fs.Entry]]:
        root = self.stat(path)
        yield (), root
        if root.is_dir:
            yield from self._walk(path, ())

    def _walk(self, path: str, rel: vfs.RelPath) -> Iterator[tuple[vfs.RelPath, fs.Entry]]:
        for entry in sorted(self.list_dir(path, show_hidden=True), key=lambda e: e.name):
            yield (*rel, entry.name), entry
            if entry.is_dir and not entry.is_link:
                yield from self._walk(posixpath.join(path, entry.name), (*rel, entry.name))

    # -- writing -------------------------------------------------------------------

    def mkdir(self, path: str, *, exist_ok: bool = False) -> None:
        self._call(smbclient.makedirs, self._unc(path), exist_ok=exist_ok)

    def remove(self, path: str) -> None:
        if self.stat(path).is_dir:
            self._call(smbclient.shutil.rmtree, self._unc(path))
        else:
            self._call(smbclient.remove, self._unc(path))

    def rename(self, src: str, dst: str) -> None:
        self._call(smbclient.rename, self._unc(src), self._unc(dst))

    def open_write(self, path: str) -> BinaryIO:
        f = self._call(smbclient.open_file, self._unc(path), mode="wb", share_access="r")
        return _Serialized(f, self._lock)  # type: ignore[return-value]

    def set_mtime(self, path: str, mtime: float) -> None:
        self._call(smbclient.utime, self._unc(path), times=(mtime, mtime))

    def close(self) -> None:
        try:
            smbclient.reset_connection_cache(connection_cache=self._cache)
        except Exception:
            pass  # already disconnected


class _Serialized(io.RawIOBase):
    """An open SMB file whose reads/writes take the connection's request lock."""

    def __init__(self, f: Any, lock: threading.RLock) -> None:
        super().__init__()
        self._f = f
        self._lock = lock

    def readable(self) -> bool:
        return self._f.readable()

    def writable(self) -> bool:
        return self._f.writable()

    def seekable(self) -> bool:
        return self._f.seekable()

    def read(self, size: int = -1) -> bytes:
        with self._lock:
            return self._f.read(size)

    def readinto(self, buffer: Any) -> int:
        with self._lock:
            return self._f.readinto(buffer)

    def write(self, data: Any) -> int:
        with self._lock:
            return self._f.write(data)

    def seek(self, offset: int, whence: int = io.SEEK_SET) -> int:
        with self._lock:
            return self._f.seek(offset, whence)

    def tell(self) -> int:
        return self._f.tell()

    def close(self) -> None:
        if not self.closed:
            with self._lock:
                self._f.close()
        super().close()
