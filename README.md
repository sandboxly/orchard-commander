# Orchard Commander

A two-panel file manager for macOS, Windows and Linux in the classic
keyboard-driven style. It shows two folders side by side and is driven
by the keyboard: F5 copies, F6 moves, F7 creates a folder, F8 moves files to the
trash. It previews code, images, PDFs, audio and video, edits text files (F4),
opens archives and SFTP, SMB, WebDAV and S3 servers in a panel, and has a file
search, a multi-rename tool and a terminal.

The app is written in Python with Qt (PySide6) and drawn to look like a
terminal program.

Website: https://sandboxly.github.io/orchard-commander/

## Install

| System | Download | Package manager |
| --- | --- | --- |
| macOS 14 or later, Apple silicon | [Orchard-Commander-macOS.dmg](https://github.com/sandboxly/orchard-commander/releases/latest/download/Orchard-Commander-macOS.dmg) | `brew install --cask sandboxly/tap/orchard-commander` |
| Windows 10 and 11, 64-bit | [Installer](https://github.com/sandboxly/orchard-commander/releases/latest/download/Orchard-Commander-Setup.exe), [portable zip](https://github.com/sandboxly/orchard-commander/releases/latest/download/Orchard-Commander-Windows.zip) | |
| Linux, x86-64 | [AppImage](https://github.com/sandboxly/orchard-commander/releases/latest/download/Orchard-Commander-Linux-x86_64.AppImage), [.deb](https://github.com/sandboxly/orchard-commander/releases/latest/download/Orchard-Commander-Linux-x86_64.deb), [.rpm](https://github.com/sandboxly/orchard-commander/releases/latest/download/Orchard-Commander-Linux-x86_64.rpm), [.tar.gz](https://github.com/sandboxly/orchard-commander/releases/latest/download/Orchard-Commander-Linux.tar.gz) | |

All versions are on the [releases page](https://github.com/sandboxly/orchard-commander/releases).

If you enjoy Orchard Commander, you can [buy me a coffee](https://ko-fi.com/sandboxly).

## Documentation

The [user manual](https://sandboxly.github.io/orchard-commander/manual/) covers
everything from the first start to the keyboard reference. Its source is in
[docs/manual](docs/manual).

## Run from source

```sh
uv run orchard-commander [LEFT_DIR] [RIGHT_DIR]
```

Needs [uv](https://docs.astral.sh/uv/) and Python 3.12 or later.

## Building the app

```sh
uv run python packaging/build.py
```

PyInstaller produces `dist/Orchard Commander.app` on macOS (about 140 MB, mostly Qt and
FFmpeg) and `dist/orchard-commander/` with the `orchard-commander` executable on Linux
and Windows; build on each platform for that platform. Check a build with
`"dist/Orchard Commander.app/Contents/MacOS/orchard-commander" --self-test`
(or `dist/orchard-commander/orchard-commander --self-test`),
which loads everything the app imports on demand.

On macOS the app bundle matters beyond convenience: macOS only grants *Local
Network* access (needed for servers on your LAN) to programs with a
proper identity, and asks for it using the `NSLocalNetworkUsageDescription`
set in `packaging/orchard-commander.spec`. `uv run orchard-commander` runs on a bare Python binary,
which macOS may silently block from the local network ("No route to host").
macOS ties those permissions (and Keychain access to saved passwords) to the
app's code signature, so the build signs with a stable identity: the
`FCMD_CODESIGN_IDENTITY` environment variable or `--sign IDENTITY`, otherwise
the first "Developer ID Application" (then "Apple Development") certificate in
your keychain, with the hardened runtime and `packaging/entitlements.plist`.
Without a certificate it falls back to ad-hoc signing (or use `--adhoc`), and
macOS then treats every build as a new app and asks again. Distributing the
app additionally requires notarization (`xcrun notarytool`).

Regenerate the app icons (macOS `.icns`, Windows `.ico`, window icon) from
`packaging/icons/source.png` with `uv run python packaging/make_icons.py`.

## Development

```sh
uv run pytest
```

The SFTP tests run a real SSH server (asyncssh), the SMB tests impacket's
SMB2 server, the WebDAV tests WsgiDAV (Basic, Digest, and HTTPS with a
self-signed certificate) and the S3 tests moto (request signatures are checked
against botocore and AWS's documented examples), all on localhost. Full SMB read/write tests run against Samba in
Docker and are skipped when Docker isn't available.

| Module | Role |
| --- | --- |
| `vfs.py` | Virtual file system: `FileSystem` interface, `VPath`, `LocalFS`, `ArchiveFS`, copy/move/trash across file systems |
| `fs.py` | Native local-disk operations: listing, copy, move, trash, open (used by `LocalFS`) |
| `archives.py` | Archive format readers (zip/tar/7z/rar/gz/bz2/xz) and the extraction cache (used by `ArchiveFS`) |
| `fsproviders.py` | Connection types: `FileSystemProvider` interface, registry, entry-point discovery |
| `remote/sftp.py`, `remote/smb.py`, `remote/webdav.py`, `remote/s3.py` | SFTP (paramiko), SMB (smbprotocol), WebDAV and S3 (standard library HTTP) file systems |
| `remote/httpclient.py` | HTTP(S) shared by WebDAV and S3: per-thread keep-alive connections, system trust store, pinned certificates, ranged reads |
| `remote/providers.py` | The built-in SFTP, SMB, WebDAV and S3 connection types |
| `connections.py` | Provider-agnostic connections, saved connections, keychain passwords, live sessions |
| `bookmarks.py`, `locations.py` | Bookmark tree and storage; serializable location specs (local, server, archive) |
| `rename.py`, `ui/rename_ui.py` | Multi-rename: TC masks, search & replace, checks, safe two-phase renames, undo |
| `terminal.py`, `ui/terminal_view.py` | Shell sessions (pseudo-terminal + pyte screen) and the terminal panel |
| `search.py`, `ui/search_ui.py` | Find files on any file system; results as a virtual folder (`SearchResultsFS`); the dialog |
| `testing.py` | Contract checks for file system implementations (also for add-on authors) |
| `preview/api.py` | The previewer plugin interface: `Previewer`, `PreviewView`, `Access`, `Score`, … |
| `preview/registry.py` | Registration, entry-point discovery, scoring, fallback |
| `preview/source.py` | `PreviewSource`: head / stream / local file through the generic file system interface |
| `preview/builtin/` | Built-in previewers: listing, image, PDF, media, Markdown, text, hex |
| `preview/content.py` | Qt-free helpers: text detection and decoding, hex dumps, listings |
| `themes.py`, `config.py` | Color schemes (built-in and user files), appearance, persisted settings |
| `ui/settings_dialog.py` | Settings window, color scheme editor |
| `ui/main_window.py` | Window, sides, commands and shortcuts, function-key bar |
| `ui/panel.py` | File list model, row painting, key handling |
| `ui/preview_pane.py` | Hosts previewer views, "View as" chips, background loading; no type-specific code |
| `ui/boxframe.py` | The terminal-style box border |
| `ui/dialogs.py`, `ui/operations.py` | Dialogs; file operations on a worker thread |
| `ui/connect_dialogs.py` | Connection manager, editor, password and host-key prompts |
| `ui/bookmarks_ui.py` | Bookmark menu with hotkeys, "add bookmark" dialog, organizer |
| `ui/tasks.py` | Background jobs with callbacks on the UI thread; asking the UI from a worker |

Plans for third-party add-ons and an add-on marketplace: [docs/addon-marketplace.md](docs/addon-marketplace.md).

### Adding a file system

Two parts, both discoverable like previewers:

1. **The file system** subclasses `vfs.FileSystem`: listing, `stat`, path
   handling and `open_read`; writable ones add `mkdir`, `remove`, `open_write`
   (and `rename` with `can_rename`). The UI only uses `vfs.VPath` values and
   capability flags (`writable`, `can_trash`, `watchable`), so copy/move,
   previews and browsing archives stored on it work without UI changes.
2. **The connection type** subclasses `fsproviders.FileSystemProvider`: an
   address scheme (`webdav://`), the fields of its connection dialog (the form
   is generated from them), `parse_url` / `format_url`, and `connect(params,
   secret, ctx)`, which returns the file system. It may ask the user questions
   through `ctx.confirm(...)` (that's how SFTP asks to trust a host key) and
   keep files in `ctx.data_dir`. See `remote/providers.py` for SFTP and SMB.

Register providers in `fsproviders._builtin_providers()` or through an entry
point (add-ons may override built-ins with the same scheme):

```toml
[project.entry-points."orchard_commander.filesystems"]
webdav = "orchard_webdav:WebDavProvider"
```

Saved connections store the scheme plus the provider's params, so connection
types need no changes elsewhere. Check an implementation with the contract in
`orchard_commander.testing` (`check_filesystem(root)`), which every built-in
file system passes (`tests/test_fs_contract.py`); `tests/test_vfs.py` has a
small in-memory backend and `tests/test_fsproviders.py` a matching connection
type as examples.

### Adding a previewer

A previewer is one class (see the docstring of `preview/api.py`):

- `score(FileInfo)` says how well it fits a file: by extension, size, or the
  first bytes if it sets `needs_head`. Highest score wins; `Score.FORMAT` (100)
  for "this is my format", `Score.FALLBACK` (1) for "I can show anything".
- `access` declares what it reads; `load(src, ctx)` gets it from the
  `PreviewSource` (`head(n)`, `open_stream()`, `local_file(max_size)`) on a
  worker thread and returns `Loaded(data, info)`. It never learns which file
  system the file is on; the file system provides the bytes.
- `create_view(ctx)` returns a `PreviewView` widget (created once, reused):
  `show_data(data)`, `clear()`.
- Raise `CannotPreview` to let the next previewer try (e.g. a corrupt file),
  or `ShowMessage` to show a message instead (e.g. "too large to fetch").
- `available()` returns False when an optional dependency is missing.

Register built-ins in `preview/builtin/__init__.py`; separate packages register
through an entry point and may override built-ins with the same `id`:

```toml
[project.entry-points."orchard_commander.previewers"]
pdf = "orchard_pdf:PdfPreviewer"
```

`tests/test_previewers.py` runs a contract against every registered previewer:
empty, garbage and valid files, on the local disk and a non-local file system,
so a new previewer is checked automatically.

## License

MIT, see [LICENSE](LICENSE). The app bundles Qt (PySide6) and pyte under the LGPL.
