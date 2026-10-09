"""The built-in connection types: SFTP, SMB and WebDAV."""

from __future__ import annotations

import os
import posixpath
from urllib.parse import quote

from orchard_commander import APP_NAME, vfs
from orchard_commander.fsproviders import (
    ConnectContext,
    Field,
    FileSystemProvider,
    Params,
    join_server_url,
    split_server_url,
)


def _port(params: Params, default: int) -> int:
    value = params.get("port")
    return int(value) if value not in (None, "") else default


class SFTPProvider(FileSystemProvider):
    scheme = "sftp"
    title = "SFTP (SSH)"
    default_port = 22
    fields = (
        Field("host", "Server", required=True, placeholder="server name or address"),
        Field("port", "Port", kind="int", placeholder="default 22"),
        Field("username", "User"),
        Field("key_file", "Key file", kind="path", placeholder="optional, e.g. ~/.ssh/id_ed25519"),
        Field("path", "Start folder", placeholder="/absolute or relative to home"),
    )
    secret_hint = "empty: use SSH agent/keys, or ask"

    def parse_url(self, url: str) -> dict[str, str | int]:
        """sftp://[user@]host[:port][/abs/path | /~/path/in/home]"""
        params, segments = split_server_url(url, self.scheme)
        if segments and segments[0] == "~":
            params["path"] = "/".join(segments[1:])  # relative to the home directory
        elif segments:
            params["path"] = "/" + "/".join(segments)
        return params

    def format_url(self, params: Params) -> str:
        path = str(params.get("path") or "")
        if path.startswith("/"):
            suffix = quote(path)
        else:
            suffix = f"/~/{quote(path.strip('/'))}" if path.strip("/") else ""
        return join_server_url(self.scheme, params, self.default_port, suffix)

    def connect(self, params: Params, secret: str | None, ctx: ConnectContext) -> vfs.FileSystem:
        from orchard_commander.remote import sftp

        host = str(params["host"])
        key_file = str(params.get("key_file") or "")

        def confirm_host_key(q) -> bool:
            shown = q.host if q.port == 22 else f"{q.host}:{q.port}"
            return ctx.confirm(
                "Unknown server",
                f"{APP_NAME} hasn't connected to {shown} before. Its identity can't be confirmed yet.\n\n"
                f"{q.key_type} key fingerprint:\n  {q.fingerprint}\n\n"
                f"If you know the server's fingerprint, check that it matches. Trusting it remembers this key; "
                f"{APP_NAME} will refuse to connect if it ever changes.",
                "Trust and connect",
            )

        return sftp.connect(
            host,
            port=_port(params, self.default_port),
            username=str(params.get("username") or "") or None,
            password=secret,
            key_filename=os.path.expanduser(key_file) if key_file else None,
            known_hosts=ctx.data_dir / "known_hosts",
            confirm_host_key=confirm_host_key,
        )

    def start_path(self, params: Params, filesystem: vfs.FileSystem) -> str:
        home = getattr(filesystem, "home", "/")
        path = str(params.get("path") or "")
        if not path:
            return home
        return posixpath.normpath(path if path.startswith("/") else posixpath.join(home, path))


class SMBProvider(FileSystemProvider):
    scheme = "smb"
    title = "SMB (Windows / Samba share)"
    default_port = 445
    fields = (
        Field("host", "Server", required=True, placeholder="server name or address"),
        Field("port", "Port", kind="int", placeholder="default 445"),
        Field("share", "Share", required=True, placeholder="e.g. media"),
        Field("username", "User"),
        Field("path", "Start folder", placeholder="folder inside the share"),
    )

    def parse_url(self, url: str) -> dict[str, str | int]:
        """smb://[user@]host[:port]/share[/path]"""
        params, segments = split_server_url(url, self.scheme)
        if not segments:
            raise ValueError("SMB addresses need a share: smb://server/share")
        params["share"] = segments[0]
        if segments[1:]:
            params["path"] = "/".join(segments[1:])
        return params

    def format_url(self, params: Params) -> str:
        share = str(params.get("share") or "").strip("/\\")
        path = str(params.get("path") or "").strip("/")
        suffix = f"/{quote(share)}" + (f"/{quote(path)}" if path else "")
        return join_server_url(self.scheme, params, self.default_port, suffix)

    def connect(self, params: Params, secret: str | None, ctx: ConnectContext) -> vfs.FileSystem:
        from orchard_commander.remote import smb

        return smb.connect(
            str(params["host"]),
            str(params["share"]).strip("/\\"),
            port=_port(params, self.default_port),
            username=str(params.get("username") or "") or None,
            password=secret,
        )

    def start_path(self, params: Params, filesystem: vfs.FileSystem) -> str:
        return "/" + str(params.get("path") or "").strip("/")


class WebDAVProvider(FileSystemProvider):
    """WebDAV over HTTPS: webdavs://[user@]host[:port]/server/path/start/folder"""

    scheme = "webdavs"
    title = "WebDAV (HTTPS)"
    secure = True
    default_port = 443
    fields = (
        Field("host", "Server", required=True, placeholder="e.g. cloud.example.com"),
        Field("port", "Port", kind="int", placeholder="default 443"),
        Field("root", "Server path", placeholder="e.g. /remote.php/dav/files/USER for Nextcloud"),
        Field("username", "User"),
        Field("path", "Start folder", placeholder="below the server path"),
    )
    secret_hint = "empty: ask if the server wants one"

    def parse_url(self, url: str) -> dict[str, str | int]:
        params, segments = split_server_url(url, self.scheme)
        if segments:  # a typed address: its whole path is the root
            params["root"] = "/" + "/".join(segments)
        return params

    def format_url(self, params: Params) -> str:
        root = str(params.get("root") or "").strip("/")
        path = str(params.get("path") or "").strip("/")
        full = "/".join(p for p in (root, path) if p)
        return join_server_url(self.scheme, params, self.default_port, f"/{quote(full)}" if full else "")

    def validate(self, params: Params) -> str | None:
        if "://" in str(params.get("host", "")) or "/" in str(params.get("host", "")):
            return "Enter just the server name; the rest of the address goes into Server path."
        return super().validate(params)

    def connect(self, params: Params, secret: str | None, ctx: ConnectContext) -> vfs.FileSystem:
        from orchard_commander.remote import webdav

        def confirm_certificate(q: webdav.CertificateQuestion) -> bool:
            shown = q.host if q.port == 443 else f"{q.host}:{q.port}"
            return ctx.confirm(
                "Untrusted certificate",
                f"The certificate of {shown} isn't trusted by this computer ({q.problem}). "
                f"Servers on a home network often use such self-made certificates.\n\n"
                f"Certificate fingerprint:\n  {q.fingerprint}\n\n"
                f"If you know the server's fingerprint, check that it matches. Trusting it remembers this "
                f"certificate; {APP_NAME} will refuse to connect if it ever changes.",
                "Trust and connect",
            )

        return webdav.connect(
            str(params["host"]),
            secure=self.secure,
            port=_port(params, self.default_port),
            root=str(params.get("root") or "/"),
            username=str(params.get("username") or "") or None,
            password=secret,
            trusted_certificates=ctx.data_dir / "trusted_certificates.json",
            confirm_certificate=confirm_certificate,
        )

    def start_path(self, params: Params, filesystem: vfs.FileSystem) -> str:
        return "/" + str(params.get("path") or "").strip("/")


class PlainWebDAVProvider(WebDAVProvider):
    """WebDAV over unencrypted HTTP (only sensible on a trusted local network)."""

    scheme = "webdav"
    title = "WebDAV (HTTP, unencrypted)"
    secure = False
    default_port = 80
    fields = tuple(
        Field(f.key, f.label, f.kind, f.required, "default 80", f.default) if f.key == "port" else f
        for f in WebDAVProvider.fields
    )
