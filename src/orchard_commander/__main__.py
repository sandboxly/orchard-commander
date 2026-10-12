"""Entry point: `orchard-commander [LEFT] [RIGHT]`."""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

from orchard_commander import APP_NAME, COMMAND


def drop_launcher_dir() -> None:
    """Remove the launcher script's directory from sys.path.

    Python puts it first when running a console script, so any `*.py` there
    (e.g. impacket's `smbclient.py` example in a dev venv's bin/) would shadow
    real packages. Orchard Commander never imports anything from there.
    """
    if getattr(sys, "frozen", False):
        return  # a PyInstaller app imports only from its own bundle
    launcher = os.path.dirname(os.path.abspath(sys.argv[0])) if sys.argv and sys.argv[0] else ""
    if launcher and not os.path.isfile(os.path.join(launcher, "orchard_commander", "__init__.py")):
        sys.path[:] = [p for p in sys.path if os.path.abspath(p or ".") != launcher]


def self_test() -> int:
    """Load everything the app imports lazily; for checking packaged builds."""
    import importlib

    failures = []
    for name in [
        "PySide6.QtWidgets", "PySide6.QtMultimedia", "PySide6.QtMultimediaWidgets", "PySide6.QtSvg",
        "orchard_commander.ui.main_window", "orchard_commander.remote.sftp", "orchard_commander.remote.smb",
        "orchard_commander.remote.webdav", "orchard_commander.remote.s3", "truststore", "py7zr", "rarfile", "keyring",
        "mutagen.easyid3", "mutagen.easymp4", "mutagen.flac", "exifread", "hachoir.parser.video",
        "hachoir.parser.image", "hachoir.metadata.video", "hachoir.metadata.image",
    ]:
        try:
            importlib.import_module(name)
        except Exception as exc:
            failures.append(f"import {name}: {exc}")
    try:
        from pygments.lexers import get_lexer_for_filename
        from pygments.styles import get_style_by_name

        get_lexer_for_filename("x.py")
        get_style_by_name("monokai")
    except Exception as exc:
        failures.append(f"pygments: {exc}")
    try:  # HTTPS (WebDAV) checks certificates with the system's trust store
        import ssl

        import truststore

        truststore.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    except Exception as exc:
        failures.append(f"truststore: {exc}")
    try:  # PDFium is a native library: check it loads (code signing, bundling)
        import pypdfium2 as pdfium

        blank = pdfium.PdfDocument.new()
        blank.new_page(100, 100).render(scale=1).close()
        blank.close()
    except Exception as exc:
        failures.append(f"pdfium: {exc}")
    try:
        from orchard_commander.preview import registry

        found = sorted(p.id for p in registry.previewers())
        print("previewers:", ", ".join(found))
        if not {"hex", "image", "listing", "markdown", "media", "pdf", "text"} <= set(found):
            failures.append(f"previewers missing: {found}")
    except Exception as exc:
        failures.append(f"previewers: {exc}")
    try:
        import keyring

        print("keyring backend:", type(keyring.get_keyring()).__module__)
    except Exception as exc:
        failures.append(f"keyring: {exc}")
    try:
        from PySide6.QtCore import QCoreApplication
        from PySide6.QtMultimedia import QMediaFormat

        app = QCoreApplication.instance() or QCoreApplication([])
        decoders = QMediaFormat().supportedFileFormats(QMediaFormat.ConversionMode.Decode)
        print("video formats:", len(decoders))
        if not decoders:
            failures.append("multimedia: no decodable formats (FFmpeg plugin missing?)")
        del app
    except Exception as exc:
        failures.append(f"multimedia: {exc}")
    try:  # the terminal panel: a shell in a pseudo-terminal (hardened runtime, bundled pyte)
        import threading
        from pathlib import Path

        from orchard_commander import terminal

        done = threading.Event()
        argv = ["cmd.exe", "/c", "echo terminal-ok"] if terminal.IS_WINDOWS else ["/bin/sh", "-c", "echo terminal-ok"]
        session = terminal.Session(argv, Path.home(), on_exit=done.set)
        session.start()
        done.wait(10)
        with session.lock:
            shown = "\n".join(session.screen.display)
        print("terminal shell:", " ".join(terminal.default_shell()))
        if "terminal-ok" not in shown:
            failures.append(f"terminal: no output from {argv[0]} (exited: {done.is_set()}, "
                            f"status {session.exit_status}, screen: {shown.strip()[:200]!r})")
    except Exception as exc:
        failures.append(f"terminal: {exc}")
    for line in failures:
        print("FAIL", line)
    print("self-test", "failed" if failures else "ok")
    return 1 if failures else 0


def set_app_icon(app) -> None:
    """Window/taskbar icon; on macOS the running app's Dock icon, which must look
    like the bundle's (the rounded square, not the bare folder)."""
    from importlib.resources import as_file, files

    from PySide6.QtGui import QIcon

    name = "icon-macos.png" if sys.platform == "darwin" else "icon.png"
    resource = files("orchard_commander").joinpath(f"resources/{name}")
    if resource.is_file():
        with as_file(resource) as path:
            app.setWindowIcon(QIcon(str(path)))


def main() -> None:
    drop_launcher_dir()
    parser = argparse.ArgumentParser(prog=COMMAND, description=f"{APP_NAME}: a two-panel file manager.")
    parser.add_argument("left", nargs="?", type=Path, help="directory for the left panel")
    parser.add_argument("right", nargs="?", type=Path, help="directory for the right panel")
    parser.add_argument("--self-test", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.self_test:
        sys.exit(self_test())

    from PySide6.QtWidgets import QApplication

    from orchard_commander.config import Settings, migrate_legacy_config
    from orchard_commander.ui.main_window import MainWindow
    from orchard_commander.ui.style import ThemeManager

    app = QApplication(sys.argv[:1])
    app.setApplicationName(APP_NAME)
    app.setDesktopFileName(COMMAND)  # Linux: match orchard-commander.desktop (icon, taskbar)
    app.setStyle("Fusion")  # identical widget rendering on every platform
    set_app_icon(app)

    migrate_legacy_config()
    settings = Settings.load()
    themes = ThemeManager(settings.appearance())
    themes.apply()
    window = MainWindow(settings, themes, args.left, args.right)
    window.show()
    sys.exit(app.exec())


if __name__ == "__main__":
    main()
