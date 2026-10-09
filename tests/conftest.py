import os
import sys
from pathlib import Path

# impacket (test-only SMB server) installs example scripts such as smbclient.py
# into the venv's bin directory, which is sys.path[0] when running the pytest
# script; they would shadow the real smbclient package.
_script_dir = os.path.dirname(os.path.abspath(sys.argv[0]))
if os.path.exists(os.path.join(_script_dir, "smbclient.py")) and _script_dir in sys.path:
    sys.path.remove(_script_dir)

import pytest

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

from orchard_commander import config  # noqa: E402


@pytest.fixture(autouse=True)
def isolated_config(tmp_path_factory, monkeypatch):
    monkeypatch.setattr(config, "CONFIG_FILE", tmp_path_factory.mktemp("cfg") / "settings.json")


@pytest.fixture
def tree(tmp_path: Path) -> Path:
    """A small directory tree:  src/{a.txt, b.bin, sub/c.txt, .hidden}, dst/"""
    src = tmp_path / "src"
    (src / "sub").mkdir(parents=True)
    (src / "a.txt").write_text("alpha")
    (src / "b.bin").write_bytes(b"\0" * 3000)
    (src / "sub" / "c.txt").write_text("gamma")
    (src / ".hidden").write_text("h")
    (tmp_path / "dst").mkdir()
    return tmp_path


class MemoryKeyring:
    """Stands in for the OS keychain so tests never touch the real one."""

    def __init__(self) -> None:
        self.data: dict[tuple[str, str], str] = {}

    def get_password(self, service, username):
        return self.data.get((service, username))

    def set_password(self, service, username, password):
        self.data[(service, username)] = password

    def delete_password(self, service, username):
        self.data.pop((service, username), None)


@pytest.fixture(autouse=True)
def memory_keyring(monkeypatch):
    import keyring

    ring = MemoryKeyring()
    for name in ("get_password", "set_password", "delete_password"):
        monkeypatch.setattr(keyring, name, getattr(ring, name))
    return ring
