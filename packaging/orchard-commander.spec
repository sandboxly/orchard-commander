# PyInstaller spec for Orchard Commander. Build with:  uv run python packaging/build.py
# Produces "dist/Orchard Commander.app" on macOS, dist/orchard-commander/ (with the
# orchard-commander executable) elsewhere.

import os
import sys
from pathlib import Path

from PyInstaller.utils.hooks import collect_data_files, collect_submodules

ROOT = Path(SPECPATH).parent  # noqa: F821 (SPECPATH is provided by PyInstaller)
sys.path.insert(0, str(ROOT / "src"))
from orchard_commander import APP_NAME, COMMAND, __version__  # noqa: E402

BUNDLE_ID = "io.github.orchard-commander"

# macOS code signing. A stable identity (not ad-hoc) keeps the Local Network
# permission and Keychain access across rebuilds; packaging/build.py picks one.
SIGN_IDENTITY = os.environ.get("FCMD_CODESIGN_IDENTITY") or None
ENTITLEMENTS = str(ROOT / "packaging" / "entitlements.plist") if SIGN_IDENTITY else None

# Modules the app imports lazily or that are discovered at run time.
hidden = [
    *collect_submodules("orchard_commander"),
    *collect_submodules("pygments.lexers"),
    *collect_submodules("pygments.styles"),
    *collect_submodules("pygments.formatters"),
    "py7zr",
    "rarfile",
    "paramiko",
    "smbclient",
    "smbclient.shutil",
    "smbprotocol",
    "spnego",
    "keyring.backends.macOS",
    "keyring.backends.Windows",
    "keyring.backends.SecretService",
    "keyring.backends.kwallet",
    "keyring.backends.chainer",
    "keyring.backends.fail",
    "PySide6.QtMultimedia",
    "PySide6.QtMultimediaWidgets",
    "PySide6.QtSvg",
]

# Test-only and unused heavy packages that hooks might otherwise pull in.
excluded = ["impacket", "asyncssh", "pytest", "pytestqt", "tkinter", "PIL",  # PIL: dev-only (icon script)
            "PySide6.QtWebEngineCore",
            "PySide6.Qt3DCore", "PySide6.QtQuick3D", "PySide6.QtCharts", "PySide6.QtDataVisualization"]

a = Analysis(  # noqa: F821
    [str(ROOT / "packaging" / "launcher.py")],
    pathex=[str(ROOT / "src")],
    hiddenimports=hidden,
    datas=collect_data_files("orchard_commander"),  # resources/icon.png (window icon)
    excludes=excluded,
    noarchive=False,
)
pyz = PYZ(a.pure)  # noqa: F821

exe = EXE(  # noqa: F821
    pyz,
    a.scripts,
    [],
    exclude_binaries=True,
    name=COMMAND,
    console=False,  # a GUI app: no terminal window on Windows
    argv_emulation=False,
    icon=str(ROOT / "packaging" / "icons" / "app.ico"),  # Windows executable icon
    codesign_identity=SIGN_IDENTITY,
    entitlements_file=ENTITLEMENTS,
)
coll = COLLECT(exe, a.binaries, a.datas, name=COMMAND)  # noqa: F821

if sys.platform == "darwin":
    app = BUNDLE(  # noqa: F821
        coll,
        name=f"{APP_NAME}.app",
        bundle_identifier=BUNDLE_ID,
        icon=str(ROOT / "packaging" / "icons" / "app.icns"),
        version=__version__,
        info_plist={
            "CFBundleName": APP_NAME,
            "CFBundleDisplayName": APP_NAME,
            "CFBundleShortVersionString": __version__,
            "LSMinimumSystemVersion": "12.0",
            "NSHighResolutionCapable": True,
            # Without this, macOS never asks and silently blocks LAN access (SMB/SFTP to a NAS).
            "NSLocalNetworkUsageDescription": (
                f"{APP_NAME} connects to file servers on your local network (SMB shares, SFTP servers) "
                "when you ask it to."
            ),
        },
    )
