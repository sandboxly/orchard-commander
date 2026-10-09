"""SFTP file system over SSH (paramiko).

Paths are absolute POSIX paths on the server. One SSH connection is shared;
each thread gets its own SFTP channel on it, so a long transfer never blocks
listing in another thread.
"""

from __future__ import annotations

import base64
import errno
import hashlib
import posixpath
import socket
import stat
import threading
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import BinaryIO

import paramiko

from orchard_commander import fs, vfs
from orchard_commander.remote import (
    AuthenticationRequired,
    HostKeyChanged,
    HostKeyQuestion,
    HostKeyRejected,
    describe_connect_error,
)

DEFAULT_PORT = 22


def fingerprint(key: paramiko.PKey) -> str:
    digest = hashlib.sha256(key.asbytes()).digest()
    return "SHA256:" + base64.b64encode(digest).decode().rstrip("=")


class _AskPolicy(paramiko.MissingHostKeyPolicy):
    """Unknown host key: ask the user; remember it in our known_hosts if trusted."""

    def __init__(self, confirm: Callable[[HostKeyQuestion], bool], known_hosts: Path, port: int) -> None:
        self.confirm = confirm
        self.known_hosts = known_hosts
        self.port = port

    def missing_host_key(self, client: paramiko.SSHClient, hostname: str, key: paramiko.PKey) -> None:
        question = HostKeyQuestion(hostname, self.port, key.get_name(), fingerprint(key))
        if not self.confirm(question):
            raise HostKeyRejected(f"Host key for {hostname} was not trusted")
        client.get_host_keys().add(hostname, key.get_name(), key)
        client.save_host_keys(str(self.known_hosts))


def connect(
    host: str,
    *,
    port: int = DEFAULT_PORT,
    username: str | None = None,
    password: str | None = None,
    key_filename: str | None = None,
    known_hosts: Path,
    confirm_host_key: Callable[[HostKeyQuestion], bool],
    timeout: float = 15.0,
) -> SFTPFileSystem:
    """Connect and authenticate: with the password if given (or key file +
    passphrase), otherwise with ssh-agent / default keys.

    Raises AuthenticationRequired, HostKeyChanged, HostKeyRejected or OSError.
    """
    use_keys = password is None or key_filename is not None
    client = paramiko.SSHClient()
    try:
        client.load_system_host_keys()
    except OSError:
        pass
    known_hosts.parent.mkdir(parents=True, exist_ok=True)
    known_hosts.touch(exist_ok=True)
    client.load_host_keys(str(known_hosts))
    client.set_missing_host_key_policy(_AskPolicy(confirm_host_key, known_hosts, port))
    try:
        client.connect(
            host,
            port=port,
            username=username,
            password=password,
            key_filename=key_filename,
            passphrase=password if key_filename else None,
            timeout=timeout,
            banner_timeout=timeout,
            auth_timeout=timeout,
            # With a password, use just that: offering every agent key first can
            # exhaust the server's MaxAuthTries before the password is tried.
            allow_agent=use_keys,
            look_for_keys=use_keys,
        )
    except paramiko.BadHostKeyException as exc:
        client.close()
        raise HostKeyChanged(
            f"The host key of {host} has changed (now {fingerprint(exc.key)}). "
            "This could mean someone is intercepting the connection; refusing to connect."
        ) from exc
    except paramiko.AuthenticationException as exc:
        client.close()
        raise AuthenticationRequired(f"Authentication failed for {username or ''}@{host}") from exc
    except HostKeyRejected:
        client.close()
        raise
    except (paramiko.SSHException, socket.error) as exc:
        client.close()
        raise ConnectionError(describe_connect_error(host, port, exc)) from exc
    label = f"{username}@{host}" if username else host
    if port != DEFAULT_PORT:
        label += f":{port}"
    return SFTPFileSystem(client, label)


class SFTPFileSystem(vfs.FileSystem):
    writable = True
    can_rename = True

    def __init__(self, client: paramiko.SSHClient, label: str) -> None:
        self._client = client
        transport = client.get_transport()
        assert transport is not None
        self._transport = transport
        self._local = threading.local()
        self.label = label
        self.type_name = f"sftp://{label}"
        self.home = self._sftp().normalize(".")

    @property
    def read_only_reason(self) -> str:
        return f"{self.type_name} refused the operation"

    @property
    def connected(self) -> bool:
        return self._transport.is_active()

    def _sftp(self) -> paramiko.SFTPClient:
        sftp = getattr(self._local, "sftp", None)
        if sftp is None or sftp.sock.closed:
            if not self._transport.is_active():
                raise ConnectionError(f"Connection to {self.label} was lost")
            sftp = paramiko.SFTPClient.from_transport(self._transport)
            assert sftp is not None
            self._local.sftp = sftp
        return sftp

    # -- reading -------------------------------------------------------------------

    def _entry(self, directory: str, attr: paramiko.SFTPAttributes) -> fs.Entry:
        name = attr.filename
        mode = attr.st_mode or 0
        is_link = stat.S_ISLNK(mode)
        if is_link:  # what does it point to?
            try:
                target = self._sftp().stat(posixpath.join(directory, name))
                mode, attr = target.st_mode or 0, target
            except OSError:
                pass  # dangling link
        is_dir = stat.S_ISDIR(mode)
        return fs.Entry(
            name=name,
            is_dir=is_dir,
            is_link=is_link,
            is_hidden=name.startswith("."),
            is_executable=not is_dir and bool(mode & 0o111),
            size=0 if is_dir else (attr.st_size or 0),
            mtime=float(attr.st_mtime or 0),
        )

    def list_dir(self, path: str, **kwargs) -> list[fs.Entry]:
        entries = [self._entry(path, a) for a in self._sftp().listdir_attr(path)]
        return fs.sort_entries(entries, **kwargs)

    def stat(self, path: str) -> fs.Entry:
        attr = self._sftp().lstat(path)
        attr.filename = posixpath.basename(path) or path
        return self._entry(posixpath.dirname(path), attr)

    def is_dir(self, path: str) -> bool:
        try:
            return stat.S_ISDIR(self._sftp().stat(path).st_mode or 0)
        except FileNotFoundError:
            return False

    def parent_path(self, path: str) -> str | None:
        return None if path == "/" else posixpath.dirname(path)

    def join(self, path: str, *names: str) -> str:
        return posixpath.join(path, *names)

    def basename(self, path: str) -> str:
        return posixpath.basename(path) or path

    def display(self, path: str) -> str:
        return f"{self.type_name}{path}"

    def open_read(self, path: str) -> BinaryIO:
        f = self._sftp().open(path, "rb")
        f.prefetch()  # pipeline reads: much faster for large files
        return f  # type: ignore[return-value]

    def walk(self, path: str) -> Iterator[tuple[vfs.RelPath, fs.Entry]]:
        root = self.stat(path)
        yield (), root
        if root.is_dir and not root.is_link:
            yield from self._walk(path, ())

    def _walk(self, path: str, rel: vfs.RelPath) -> Iterator[tuple[vfs.RelPath, fs.Entry]]:
        for entry in sorted(self.list_dir(path, show_hidden=True), key=lambda e: e.name):
            if entry.is_link and entry.is_dir:  # copy the link as a file-like item, don't follow
                entry = fs.Entry(entry.name, False, True, entry.is_hidden, False, 0, entry.mtime)
            yield (*rel, entry.name), entry
            if entry.is_dir:
                yield from self._walk(posixpath.join(path, entry.name), (*rel, entry.name))

    # -- writing -------------------------------------------------------------------

    def mkdir(self, path: str, *, exist_ok: bool = False) -> None:
        sftp = self._sftp()
        missing = []
        p = path
        while p not in ("", "/") and not self.exists(p):
            missing.append(p)
            p = posixpath.dirname(p)
        if not missing and not exist_ok:
            raise FileExistsError(errno.EEXIST, "File exists", path)
        for p in reversed(missing):
            sftp.mkdir(p)

    def remove(self, path: str) -> None:
        sftp = self._sftp()
        if stat.S_ISDIR(sftp.lstat(path).st_mode or 0):
            for attr in sftp.listdir_attr(path):
                self.remove(posixpath.join(path, attr.filename))
            sftp.rmdir(path)
        else:
            sftp.remove(path)

    def rename(self, src: str, dst: str) -> None:
        sftp = self._sftp()
        try:
            sftp.posix_rename(src, dst)
        except OSError:
            sftp.rename(src, dst)  # servers without the posix-rename extension

    def open_write(self, path: str) -> BinaryIO:
        f = self._sftp().open(path, "wb")
        f.set_pipelined(True)
        return f  # type: ignore[return-value]

    def set_mtime(self, path: str, mtime: float) -> None:
        self._sftp().utime(path, (mtime, mtime))

    def close(self) -> None:
        self._client.close()
