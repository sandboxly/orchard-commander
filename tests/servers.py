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
