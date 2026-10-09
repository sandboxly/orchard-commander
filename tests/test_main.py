import subprocess
import sys
from pathlib import Path


def test_launcher_directory_cannot_shadow_packages(tmp_path: Path):
    """A console script's directory comes first on sys.path; modules there must not win.

    In a dev venv, impacket puts an `smbclient.py` into bin/ next to the app launcher.
    """
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "smbclient.py").write_text("raise SystemExit('shadowed!')\n")
    launcher = bin_dir / "app-launcher.py"
    launcher.write_text(
        "from orchard_commander.__main__ import drop_launcher_dir\n"
        "drop_launcher_dir()\n"
        "import orchard_commander.remote.smb, smbclient\n"
        "print(smbclient.__file__)\n"
    )
    result = subprocess.run([sys.executable, str(launcher)], capture_output=True, text=True)
    assert result.returncode == 0, result.stderr or result.stdout
    assert "site-packages" in result.stdout
