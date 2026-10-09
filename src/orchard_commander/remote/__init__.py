"""Network file systems (SFTP, SMB) behind the `vfs.FileSystem` interface."""

from __future__ import annotations

import errno
import ipaddress
import socket
import sys
from dataclasses import dataclass

from orchard_commander.fsproviders import AuthenticationRequired, ConnectionCancelled  # re-exported


__all__ = ["AuthenticationRequired", "HostKeyChanged", "HostKeyQuestion", "HostKeyRejected", "describe_connect_error"]


class HostKeyChanged(Exception):
    """The server's host key differs from the one on record: possible attack."""


class HostKeyRejected(ConnectionCancelled):
    """The user declined to trust an unknown host key."""


@dataclass(frozen=True, slots=True)
class HostKeyQuestion:
    host: str
    port: int
    key_type: str
    fingerprint: str  # "SHA256:..." as shown by OpenSSH


def describe_connect_error(host: str, port: int, exc: BaseException) -> str:
    """A connection error message, with a hint for macOS's Local Network privacy.

    macOS (15+) silently blocks programs without Local Network permission from
    reaching devices on the LAN; they just see "No route to host".
    """
    message = f"Cannot connect to {host}:{port}: {exc}"
    if sys.platform == "darwin" and _errno_of(exc) == errno.EHOSTUNREACH and _is_local(host):
        message += (
            ". macOS may be blocking local network access for this program: allow it in "
            "System Settings → Privacy & Security → Local Network (look for python), then retry."
        )
    return message


def _errno_of(exc: BaseException | None) -> int | None:
    while exc is not None:  # libraries often wrap the socket error
        if isinstance(exc, OSError) and exc.errno:
            return exc.errno
        if "No route to host" in str(exc):
            return errno.EHOSTUNREACH
        exc = exc.__cause__ or exc.__context__
    return None


def _is_local(host: str) -> bool:
    try:
        addresses = {info[4][0] for info in socket.getaddrinfo(host, None)}
    except OSError:
        return host.endswith(".local")
    for address in addresses:
        try:
            ip = ipaddress.ip_address(address.split("%")[0])
        except ValueError:
            continue
        if ip.is_private or ip.is_link_local:
            return True
    return False
