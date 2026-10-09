# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Orchard Commander: a cross-platform (macOS, Windows, Linux) two-panel file manager in the spirit of Norton/Total Commander. It is a **Qt (PySide6) desktop app styled to look like a terminal app** (monospace grid, box borders, F-key bar) — not a TUI; it was rebuilt from a Textual TUI so it could show images/video/PDFs natively. Total Commander behaviour and key bindings are the reference for features. The repository folder is still called `fcmd` (the old name); the package is `orchard_commander`, the command `orchard-commander`.

## Commands

```sh
uv run orchard-commander [LEFT] [RIGHT]          # run from source
uv run pytest                                    # all tests (QT_QPA_PLATFORM=offscreen is set in conftest)
uv run pytest tests/test_ui.py -k bookmark       # a subset
uv run --with pyflakes python -m pyflakes src tests   # lint (no linter configured in pyproject)
uv run python packaging/build.py                 # build "dist/Orchard Commander.app" (or dist/orchard-commander/)
"dist/Orchard Commander.app/Contents/MacOS/orchard-commander" --self-test   # verify a packaged build
uv run python packaging/make_icons.py            # regenerate icons from packaging/icons/source.png
```

- SMB write tests start Samba in Docker (`tests/servers.py`) and skip without Docker; SFTP tests run an asyncssh server and SMB read tests an impacket server, both in-process on localhost.
- The build signs with the first "Developer ID Application" certificate in the keychain (`--sign`, `--adhoc`, or `FCMD_CODESIGN_IDENTITY` override). Ad-hoc builds make macOS forget Local Network and Keychain permissions on every rebuild.
- On macOS, `uv run orchard-commander` (a bare, ad-hoc-signed Python) is silently blocked from LAN hosts by Local Network privacy ("No route to host"); test LAN servers (e.g. a NAS) with the built, signed app.
- After rebuilding, quit the running app (`osascript -e 'quit app "Orchard Commander"'`) before `open`ing the new one, otherwise `open` just activates the old process.

## Architecture

Layers, bottom to top — the UI only ever talks to the generic interfaces:

- **`vfs.py`** — the file system abstraction. `FileSystem` (listing, `stat`, `open_read`, `walk`, `materialize`, optional writes) + capability flags (`writable`, `can_trash`, `can_rename`, `watchable`); `VPath` = (file system, path) is the only path type above this layer. `LocalFS` wraps `fs.py`; `ArchiveFS` wraps an `archives.Archive` and knows its `container` VPath, so archives nest and can live on servers. `vfs.copy()` streams between any two file systems and delegates to `fs.py`'s native code when both sides are local.
- **`fsproviders.py`** — connection types (`FileSystemProvider`: scheme, dialog fields, URL parse/format, `connect()`), discovered from built-ins (`remote/providers.py`: SFTP via paramiko, SMB via smbprotocol) and the `orchard_commander.filesystems` entry point. `connections.py` stores provider-agnostic connections (kind + params; secrets only in the OS keychain via `keyring`) and keeps live `Sessions`. Nothing outside `remote/` mentions SFTP/SMB.
- **`preview/`** — pluggable previewers. `api.py` defines `Previewer` (score → load on a worker thread → reusable `PreviewView`), `registry.py` scores by name first and reads the file head only if needed, with fallback (`CannotPreview` → next; `ShowMessage`/`NeedsDownload` → message). `source.py` (`PreviewSource`) gets bytes/streams/local copies **only via the generic FileSystem interface** — previewers must never depend on concrete file systems (an explicit project decision). Built-ins in `preview/builtin/` (listing, image, pdf via pypdfium2, media via Qt Multimedia/FFmpeg with `ui/media_stream.py` for streaming, markdown, text, hex). Entry point: `orchard_commander.previewers`.
- **`ui/`** — `main_window.py` (sides, commands/shortcuts, connections, bookmarks navigation), `panel.py` (file list; listing runs in the background), `preview_pane.py` (generic host, "View as" chips), `tasks.py` (all blocking work off the GUI thread: `run_in_background`, `ask_gui` for prompts from workers).
- **`bookmarks.py` / `locations.py`** — TC "Directory Hotlist". Bookmarks store serializable location *specs* (local / server by connection account / archive nested), resolved back to VPaths asynchronously in `MainWindow.resolve`.
- `testing.py` holds the file system contract (`check_filesystem`); `tests/test_previewers.py` holds the previewer contract run against every registered previewer. Both apply automatically to new backends/previewers — extend their corpora when adding formats.
- `docs/addon-marketplace.md` is the (proposal) plan for third-party add-ons.

## Gotchas (learned the hard way)

- **Never use `QTest.qWait` / `qtbot.wait` in tests that need worker threads** — it holds the GIL and starves them. Use the `QEventLoop`-based `wait()` / `wait_until()` / `settle()` helpers in `tests/test_ui.py`; `settle()` is needed after any navigation because listings are asynchronous.
- **PySide `QRunnable` subclasses get garbage-collected** before running; pass plain callables to `QThreadPool` (as `tasks.py` does).
- `impacket` (test-only) installs `smbclient.py` into the venv's `bin/`, shadowing the real `smbclient` package for console scripts; `__main__.drop_launcher_dir()` and `tests/conftest.py` guard against that.
- SMB: smbprotocol fails instead of waiting when SMB credits run out, so `remote/smb.py` serializes all requests per connection; files are opened with shared access. PDFium is not thread-safe (global lock in `preview/builtin/pdf.py`).
- SFTP: with a password given, agent/key auth is disabled (offering many agent keys exhausts `MaxAuthTries`).
- Cloud placeholders (Dropbox/iCloud/OneDrive, `SF_DATALESS` on macOS) are detected from stat data (`fs.Entry.is_online_only`); reading one downloads the whole file, so previews above 25 MB require an explicit click.
- Settings fields default to "not chosen" (`""`/`None`) so default changes reach users; `Settings.update()` writes only changed keys (several app instances may run). Config/keychain data from the old `fcmd` name is migrated (`config.migrate_legacy_config`, legacy keyring service).
- Tests isolate config dir and keyring via autouse fixtures in `tests/conftest.py`; never let tests touch the real keychain.
- PyInstaller: `packaging/orchard-commander.spec` lists lazily imported modules and excludes test-only/heavy packages (Pillow sneaks in via Pygments otherwise); `--self-test` checks lazy imports, previewers, PDFium and FFmpeg in the bundle.
