# Orchard Commander

A two-panel file manager for macOS, Windows and Linux in the classic
keyboard-driven style. It shows two folders side by side and is driven
by the keyboard: F5 copies, F6 moves, F7 creates a folder, F8 moves files to the
trash. It previews code, images, PDFs, audio and video, edits text files (F4),
opens archives and SFTP, SMB, WebDAV and S3 servers in a panel, and has a file
search, a multi-rename tool and a terminal.

The app is written in Rust, drawn on the GPU (wgpu) and made to look like a
terminal program.

Website: https://sandboxly.github.io/orchard-commander/

## Install

| System | Download | Package manager |
| --- | --- | --- |
| macOS 12 or later, Apple silicon | [Orchard-Commander-macOS.dmg](https://github.com/sandboxly/orchard-commander/releases/latest/download/Orchard-Commander-macOS.dmg) | `brew install --cask sandboxly/tap/orchard-commander` |
| Windows 10 and 11, 64-bit | [Installer](https://github.com/sandboxly/orchard-commander/releases/latest/download/Orchard-Commander-Setup.exe), [portable zip](https://github.com/sandboxly/orchard-commander/releases/latest/download/Orchard-Commander-Windows.zip) | |
| Linux, x86-64 | [AppImage](https://github.com/sandboxly/orchard-commander/releases/latest/download/Orchard-Commander-Linux-x86_64.AppImage), [.deb](https://github.com/sandboxly/orchard-commander/releases/latest/download/Orchard-Commander-Linux-x86_64.deb), [.rpm](https://github.com/sandboxly/orchard-commander/releases/latest/download/Orchard-Commander-Linux-x86_64.rpm), [.tar.gz](https://github.com/sandboxly/orchard-commander/releases/latest/download/Orchard-Commander-Linux.tar.gz) | |

**Omarchy and other Arch-based systems** — one command, no root except for runtime libraries:

```sh
curl -fsSL https://raw.githubusercontent.com/sandboxly/orchard-commander/main/packaging/omarchy/install.sh | bash
```

Add `-s -- --bind --default --free-f9` after `bash` to bind Super+Ctrl+Shift+F, open folders in
Orchard Commander and free F9 from Omarchy's dictation key; `-s -- --uninstall` removes it. Rerun the command to update.
A `PKGBUILD` for the AUR package `orchard-commander-bin` is in `packaging/omarchy/`.

All versions are on the [releases page](https://github.com/sandboxly/orchard-commander/releases).

If you enjoy Orchard Commander, you can [buy me a coffee](https://ko-fi.com/sandboxly).

## Documentation

The [user manual](https://sandboxly.github.io/orchard-commander/manual/) covers
everything from the first start to the keyboard reference. Its source is in
[docs/manual](docs/manual).

## Run from source

```sh
cargo run -p orchard-commander -- [LEFT_DIR] [RIGHT_DIR]
```

Needs Rust 1.90 or later. PDFium (PDF previews) and FFmpeg (audio and video)
are native libraries outside Cargo; fetch them into `vendor/` once:

```sh
scripts/fetch-pdfium.sh                # debug builds find vendor/pdfium on their own
scripts/fetch-ffmpeg.sh                # Windows and Linux: LGPL shared libraries
packaging/macos/build-ffmpeg.sh        # macOS: LGPL build from source
```

Build against them with `FFMPEG_DIR=vendor/ffmpeg/<platform>` and put the
libraries on the library path for `cargo run` and `cargo test` (`PATH` on
Windows, `LD_LIBRARY_PATH` on Linux). bindgen needs libclang
(`libclang-dev` on Linux, `LIBCLANG_PATH` on Windows).

## Building the app

Windows and Linux packages (installer, zip, AppImage, .deb, .rpm, tar.gz) are
built by CI; the macOS app is built, signed and notarized locally:

```sh
packaging/macos/package.sh --notarize
```

It produces `dist/Orchard Commander.app` and `dist/Orchard-Commander-macOS.dmg`.
Check any build with `orchard-commander --self-test`, which exercises the
renderer, fonts, archives, PDFium, FFmpeg, the config folder and the server
connection types.

On macOS the signature matters beyond distribution: macOS grants *Local
Network* access (needed for servers on the LAN) and Keychain access to saved
passwords per code signature. An ad-hoc signed build counts as a new app every
time, and an unbundled `cargo run` binary may be silently blocked from the
local network ("No route to host"). `package.sh` signs with
`$OC_CODESIGN_IDENTITY` or `--identity NAME`, otherwise the first
"Developer ID Application" certificate in the keychain.

Versioning, native libraries, licenses, CI and the release steps:
[docs/RELEASING.md](docs/RELEASING.md).

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The SFTP tests run an in-process SSH server; the WebDAV, S3 and SMB tests use
WsgiDAV, moto and impacket from a Python environment (`OC_TEST_PYTHON`) and
Samba, OpenSSH and MinIO in Docker. Tests whose server isn't available skip.

| Crate | Role |
| --- | --- |
| `oc-vfs` | File system interface (`FileSystem`, `VPath`, capability flags), copy/move/trash across file systems, the file system contract (`testing`) |
| `oc-local` | The local disk: listing, native copy, trash, open |
| `oc-archive` | zip, tar (plain/gz/bz2/xz), gz/bz2/xz, 7z and RAR (vendored UnRAR), read-only |
| `oc-provider` | Connection types: `FileSystemProvider`, registry, addresses |
| `oc-sftp`, `oc-smb`, `oc-remote-http` | SFTP, SMB, WebDAV and S3 file systems |
| `oc-config` | Settings, bookmarks, saved connections, keychain, color schemes (same files as 1.x) |
| `oc-preview`, `oc-pdf`, `oc-media` | Previewer interface, registry and built-in previewers; PDFium; FFmpeg playback |
| `oc-search`, `oc-rename`, `oc-term` | Find files, multi-rename, terminal sessions |
| `oc-grid` | GPU character-grid renderer (wgpu) |
| `oc-ui` | All UI state and logic: commands, menus, panels, dialogs, features |
| `app` | The `orchard-commander` binary: event loop, windows, native menu |

Where a UI feature goes and how it fits in:
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

### Adding a file system

1. **The file system** implements `oc_vfs::FileSystem`: listing, `stat`, path
   handling and `open_read`; writable ones add `mkdir`, `remove`,
   `open_write` (and `rename`), declared through `capabilities()`
   (`writable`, `can_trash`, `can_rename`, `watchable`). The UI only uses
   `VPath` values and these flags, so copy/move, previews and archives stored
   on it work without UI changes.
2. **The connection type** implements `oc_provider::FileSystemProvider`: an
   address scheme, the fields of its connection dialog (the form is generated
   from them), `parse_url` / `format_url`, and `connect`, which returns the file
   system and may ask the user through its `ConnectContext` (that is how SFTP
   asks to trust a host key).

Register it with `oc_provider::register` at start (see
`register_connection_types` in `app/src/main.rs`). Check the implementation
with `oc_vfs::testing::check_filesystem(root)`, which every built-in file
system passes; `oc_vfs::testing::MemFS` is a small in-memory example.

### Adding a previewer

A previewer implements `oc_preview::Previewer` (see the docs of
`crates/oc-preview/src/api.rs`):

- `score(&FileInfo)` says how well it fits a file: by extension, size, or the
  first bytes if `needs_head` is true. Highest score wins; `Score::FORMAT`
  (100) for "this is my format", `Score::FALLBACK` (1) for "I can show
  anything".
- `access()` declares what it reads; `load(src, ctx)` gets it from the
  `PreviewSource` on a worker thread and returns `Loaded` (`PreviewData`
  plus an info line). It never learns which file system the file is on.
- Return `PreviewError::CannotPreview` to let the next previewer try, or
  `ShowMessage` to show a message instead.
- `available()` returns false when an optional dependency is missing.

Register built-ins in `crates/oc-preview/src/builtin/mod.rs`. The contract
test in `crates/oc-preview/src/tests/contract.rs` runs every built-in
previewer against empty, garbage and valid files, so a new one is checked
automatically.

## License

MIT, see [LICENSE](LICENSE). The packages bundle FFmpeg (LGPL), PDFium and
UnRAR; their license texts are in [licenses](licenses).
