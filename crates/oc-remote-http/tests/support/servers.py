"""Real servers for the Rust integration tests, from the Python version's test venv.

    python -I servers.py webdav FOLDER [--digest] [--tls CERT_DIR]
    python -I servers.py s3

Prints one JSON line ({"host", "port", "root", "fingerprint"}) once the server
listens, then serves until stdin closes (the test process ended or dropped
the server). Port of `tests/servers.py` (start_webdav_server, start_s3_server).
"""

from __future__ import annotations

import json
import logging
import sys
import threading
from pathlib import Path

USER, PASSWORD = "tester", "s3cret"


def webdav(folder: Path, digest: bool, cert_dir: Path | None):
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
    server.error_log = lambda *args, **kwargs: None  # e.g. the certificate probes' aborted handshakes
    fingerprint = None
    if cert_dir is not None:
        from cheroot.ssl.builtin import BuiltinSSLAdapter

        cert, key, fingerprint = self_signed_certificate(cert_dir)
        server.ssl_adapter = BuiltinSSLAdapter(str(cert), str(key))
    server.prepare()
    thread = threading.Thread(target=server.serve, daemon=True)
    thread.start()
    return {"host": "127.0.0.1", "port": server.bind_addr[1], "root": "/dav", "fingerprint": fingerprint}, server.stop


def self_signed_certificate(directory: Path) -> tuple[Path, Path, str]:
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


def s3():
    import http.client

    from moto.server import ThreadedMotoServer

    logging.getLogger("werkzeug").setLevel(logging.ERROR)
    server = ThreadedMotoServer(ip_address="127.0.0.1", port=0, verbose=False)
    server.start()
    host, port = server.get_host_and_port()
    conn = http.client.HTTPConnection(host, port, timeout=10)
    conn.request("POST", "/moto-api/reset")
    conn.getresponse().read()
    conn.close()
    return {"host": host, "port": port, "root": "", "fingerprint": None}, server.stop


def main() -> None:
    kind, args = sys.argv[1], sys.argv[2:]
    if kind == "webdav":
        cert_dir = Path(args[args.index("--tls") + 1]) if "--tls" in args else None
        info, stop = webdav(Path(args[0]), "--digest" in args, cert_dir)
    elif kind == "s3":
        info, stop = s3()
    else:
        raise SystemExit(f"unknown server {kind}")
    print(json.dumps(info), flush=True)
    sys.stdin.read()  # until the test closes our stdin (or exits)
    stop()


if __name__ == "__main__":
    main()
