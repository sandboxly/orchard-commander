"""Real network servers for integration tests, bound to localhost."""

from __future__ import annotations

import asyncio
import threading
from dataclasses import dataclass
from pathlib import Path

USER, PASSWORD = "tester", "s3cret"


@dataclass
class SFTPServer:
    host: str
    port: int
    root: Path
    host_key_fingerprint: str
    stop: callable


def start_sftp_server(root: Path) -> SFTPServer:
    import asyncssh

    key = asyncssh.generate_private_key("ssh-ed25519")

    class Server(asyncssh.SSHServer):
        def begin_auth(self, username: str) -> bool:
            return True

        def password_auth_supported(self) -> bool:
            return True

        def validate_password(self, username: str, password: str) -> bool:
            return (username, password) == (USER, PASSWORD)

    loop = asyncio.new_event_loop()
    ready = threading.Event()
    box: dict = {}

    async def main() -> None:
        server = await asyncssh.listen(
            "127.0.0.1",
            0,
            server_host_keys=[key],
            server_factory=Server,
            sftp_factory=lambda chan: asyncssh.SFTPServer(chan, chroot=str(root).encode()),
        )
        box["server"] = server
        box["port"] = server.sockets[0].getsockname()[1]
        ready.set()

    thread = threading.Thread(target=lambda: (loop.run_until_complete(main()), loop.run_forever()), daemon=True)
    thread.start()
    ready.wait(10)

    async def shutdown() -> None:
        box["server"].close()
        await box["server"].wait_closed()
        tasks = [t for t in asyncio.all_tasks() if t is not asyncio.current_task()]
        for t in tasks:
            t.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)

    def stop() -> None:
        asyncio.run_coroutine_threadsafe(shutdown(), loop).result(5)
        loop.call_soon_threadsafe(loop.stop)
        thread.join(5)

    import base64
    import hashlib

    blob = key.export_public_key("openssh").split()[1]
    digest = hashlib.sha256(base64.b64decode(blob)).digest()
    fp = "SHA256:" + base64.b64encode(digest).decode().rstrip("=")
    return SFTPServer("127.0.0.1", box["port"], root, fp, stop)


@dataclass
class SMBServer:
    host: str
    port: int
    share: str
    root: Path


def start_smb_server(root: Path) -> SMBServer:
    """impacket's pure-Python SMB2 server (no Samba/Docker needed)."""
    import binascii
    import logging
    import socket

    from impacket import ntlm, smbserver

    logging.getLogger("impacket").setLevel(logging.ERROR)
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    server = smbserver.SimpleSMBServer(listenAddress="127.0.0.1", listenPort=port)
    server.addShare("SHARE", str(root), "test share")
    server.setSMB2Support(True)
    server.setLogFile("")  # no log file in the working directory
    nthash = binascii.hexlify(ntlm.compute_nthash(PASSWORD)).decode()
    server.addCredential(USER, 0, "aad3b435b51404eeaad3b435b51404ee", nthash)
    threading.Thread(target=server.start, daemon=True).start()
    deadline = __import__("time").monotonic() + 10
    while __import__("time").monotonic() < deadline:
        with socket.socket() as probe:
            if probe.connect_ex(("127.0.0.1", port)) == 0:
                break
    return SMBServer("127.0.0.1", port, "SHARE", root)


SAMBA_IMAGE = "ghcr.io/servercontainers/samba:latest"


def docker_available() -> bool:
    import shutil
    import subprocess

    if not shutil.which("docker"):
        return False
    return subprocess.run(["docker", "info"], capture_output=True).returncode == 0


def start_samba_container(max_credits: int | None = None) -> tuple[SMBServer, callable]:
    """A real Samba server in Docker; the share lives in the container (tmpfs).

    `max_credits` limits SMB2 credits like small NAS boxes do (Samba's default is 8192).
    """
    import socket
    import subprocess
    import time
    import uuid

    name = f"orchard-commander-test-samba-{uuid.uuid4().hex[:8]}"
    config = f"[SHARE]; path=/shares/share; valid users = {USER}; read only = no; force user = {USER}"
    subprocess.run(
        ["docker", "run", "-d", "--rm", "--name", name, "-p", "127.0.0.1::445",
         "-e", f"ACCOUNT_{USER}={PASSWORD}", "-e", f"UID_{USER}=1000",
         "-e", f"SAMBA_VOLUME_CONFIG_share={config}",
         *(["-e", f"SAMBA_GLOBAL_CONFIG_smb2_SPACE_max_SPACE_credits={max_credits}"] if max_credits else []),
         "--tmpfs", "/shares/share:mode=1777", SAMBA_IMAGE],
        check=True, capture_output=True,
    )
    port = int(subprocess.run(["docker", "port", name, "445"], capture_output=True, text=True, check=True)
               .stdout.split(":")[-1])
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:  # wait until smbd answers
        logs = subprocess.run(["docker", "logs", name], capture_output=True, text=True).stdout
        if "smbd version" in logs:
            with socket.socket() as probe:
                if probe.connect_ex(("127.0.0.1", port)) == 0:
                    break
        time.sleep(0.5)
    time.sleep(1)

    def stop() -> None:
        subprocess.run(["docker", "rm", "-f", name], capture_output=True)

    return SMBServer("127.0.0.1", port, "SHARE", Path("/shares/share")), stop


@dataclass
class WebDAVServer:
    host: str
    port: int
    root: str  # the URL path the share is mounted at
    cert_fingerprint: str | None  # "SHA256:…" of the self-signed certificate (HTTPS only)
    stop: callable


def start_webdav_server(folder: Path, *, digest: bool = False, tls: bool = False,
                        cert_dir: Path | None = None) -> WebDAVServer:
    """WsgiDAV on cheroot, serving `folder` at /dav (a server path, like Nextcloud's).

    With `tls`, HTTPS with a fresh self-signed certificate (written to `cert_dir`).
    Modification times can be set (PROPPATCH of DAV:getlastmodified).
    """
    import logging

    from cheroot import wsgi
    from wsgidav.wsgidav_app import WsgiDAVApp

    logging.getLogger("wsgidav").setLevel(logging.ERROR)
    config = {
        "provider_mapping": {"/dav": str(folder)},
        "http_authenticator": {"accept_basic": not digest, "accept_digest": digest, "default_to_digest": digest},
        "simple_dc": {"user_mapping": {"*": {USER: {"password": PASSWORD}}}},
        "mutable_live_props": ["{DAV:}getlastmodified"],
        "verbose": 0,
        "logging": {"enable": False, "enable_loggers": []},
        "property_manager": True,
        "lock_storage": True,
    }
    server = wsgi.Server(("127.0.0.1", 0), WsgiDAVApp(config), numthreads=8)
    fingerprint = None
    if tls:
        from cheroot.ssl.builtin import BuiltinSSLAdapter

        cert, key, fingerprint = _self_signed_certificate(cert_dir or folder.parent)
        server.ssl_adapter = BuiltinSSLAdapter(str(cert), str(key))
    server.prepare()
    port = server.bind_addr[1]
    thread = threading.Thread(target=server.serve, daemon=True)
    thread.start()

    def stop() -> None:
        server.stop()
        thread.join(5)

    return WebDAVServer("127.0.0.1", port, "/dav", fingerprint, stop)


def _self_signed_certificate(directory: Path) -> tuple[Path, Path, str]:
    import datetime as dt
    import hashlib
    import ipaddress

    from cryptography import x509
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import ec
    from cryptography.x509.oid import NameOID

    key = ec.generate_private_key(ec.SECP256R1())
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "localhost")])
    now = dt.datetime.now(dt.UTC)
    cert = (
        x509.CertificateBuilder().subject_name(name).issuer_name(name).public_key(key.public_key())
        .serial_number(x509.random_serial_number()).not_valid_before(now - dt.timedelta(minutes=5))
        .not_valid_after(now + dt.timedelta(days=1))
        .add_extension(x509.SubjectAlternativeName([x509.DNSName("localhost"),
                                                    x509.IPAddress(ipaddress.ip_address("127.0.0.1"))]), False)
        .sign(key, hashes.SHA256())
    )
    directory.mkdir(parents=True, exist_ok=True)
    cert_path, key_path = directory / "cert.pem", directory / "key.pem"
    cert_path.write_bytes(cert.public_bytes(serialization.Encoding.PEM))
    key_path.write_bytes(key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8,
                                           serialization.NoEncryption()))
    digest = hashlib.sha256(cert.public_bytes(serialization.Encoding.DER)).hexdigest().upper()
    return cert_path, key_path, "SHA256:" + ":".join(digest[i : i + 2] for i in range(0, len(digest), 2))
