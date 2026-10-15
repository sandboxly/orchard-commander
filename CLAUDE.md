# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Orchard Commander 2.x: a cross-platform (macOS 12+ Apple silicon, Windows 10/11 x64, Linux x86_64 glibc 2.35+) classic two-panel file manager, rewritten in pure Rust. It is a **GPU-drawn desktop app (winit + wgpu) styled to look like a terminal program** (character grid, box borders, F-key bar), not a TUI. The Python/Qt 1.x code (tag `python-1.1.1`, branch `python-1.x`) is the behavioural reference: keys, menus, settings files, keychain entries and saved data must stay compatible with 1.x. The package and binary are `orchard-commander` (`app/`); everything else is in `crates/oc-*`.

## Commands

```sh
cargo run -p orchard-commander -- [LEFT] [RIGHT]       # run from source
cargo test --workspace                                 # all tests
cargo test -p oc-ui --test app bookmark                # a subset
cargo clippy --workspace --all-targets -- -D warnings  # lint (CI uses -D warnings)
cargo run -p orchard-commander -- --self-test          # renderer, fonts, archives, PDFium, FFmpeg, config, providers
cargo run -p orchard-commander -- --screenshot /tmp/x.png --keys "Down F4"   # look at the UI headless
scripts/fetch-pdfium.sh                                # PDFium into vendor/pdfium (debug builds find it)
scripts/fetch-ffmpeg.sh                                # LGPL FFmpeg for Windows/Linux into vendor/ffmpeg
packaging/macos/build-ffmpeg.sh                        # LGPL FFmpeg for macOS into vendor/ffmpeg/mac-arm64
packaging/macos/package.sh --notarize                  # signed, notarized dist/Orchard-Commander-macOS.dmg
```

- Build against FFmpeg with `FFMPEG_DIR=vendor/ffmpeg/<platform>`; `cargo run`/`cargo test` need the libraries on `PATH` (Windows) or `LD_LIBRARY_PATH` (Linux). bindgen needs libclang.
- `ORCHARD_COMMANDER_CONFIG_DIR=<empty dir>` runs the app (or `--screenshot`) with default settings and without touching the real config.
- `OC_TIMING=1` prints startup phases; `OC_TIMING=exit` quits after the first frame.
- Server tests: SFTP runs an in-process russh server; WebDAV/S3/SMB use WsgiDAV, moto and impacket from a Python environment (`OC_TEST_PYTHON`, default `.venv` at the repository root: `uv venv .venv && uv pip install --python .venv/bin/python -r tests/servers-requirements.txt`) and Samba/OpenSSH/MinIO in Docker. Each skips with a note when its server is unavailable. The SFTP interop test against 1.x runs only with `OC_PYTHON1_CHECKOUT=<dir>` (e.g. `git worktree add <dir> python-1.x`).
- The orchard-* crates come from `https://github.com/sandboxly/orchard-commons` at a tag. To work on them together with this repo, add an override to `.cargo/config.toml` (one line per crate) and hide it from git with `git update-index --skip-worktree .cargo/config.toml` (the file is tracked for the Windows linker flags, so `.git/info/exclude` doesn't help; undo with `--no-skip-worktree` before changing the tracked part), then bump the tag in `Cargo.toml` once the commons change is tagged:
  ```toml
  [patch."https://github.com/sandboxly/orchard-commons"]
  orchard-grid = { path = "../orchard-commons/crates/orchard-grid" }
  ```
  Cargo still fetches the git source to resolve the tag, so the tag must exist upstream, and `Cargo.lock` must be refreshed without the override (`cargo update -p orchard-grid -p orchard-theme -p orchard-metadata`) before committing it, or the `--locked` builds in CI and packaging fail; `cargo test` in orchard-commons covers the moved code (renderer, schemes, metadata providers).
- Windows/Linux packages come from CI (`.github/workflows/ci.yml`, `packaging/stage.sh`, Inno Setup, nfpm, appimagetool). Release steps and the native-library table: `docs/RELEASING.md`; fixed asset names and identities: `docs/RELEASE-2.0-REQUIREMENTS.md`.

## Architecture

Read `docs/ARCHITECTURE.md` before touching `crates/oc-ui` or `app/`. Summary:

- **Lower crates never depend on `oc-ui`.** `oc-vfs` (the `FileSystem` trait, `VPath`, capability flags, cross-file-system ops, the `testing::check_filesystem` contract), `oc-local`, `oc-archive` (zip/tar/gz/bz2/xz/7z/RAR, all read-only, in-process; RAR via vendored UnRAR behind the default `rar` feature), `oc-provider` (connection-type registry) with `oc-sftp`, `oc-smb`, `oc-remote-http` (WebDAV, S3), `oc-config` (1.x-compatible settings, bookmarks, connections, keychain, color schemes), `oc-preview` (+ `oc-pdf`, `oc-media`), `oc-search`, `oc-rename`, `oc-term`. Shared with the other Orchard apps and kept in the separate repository orchard-commons (git dependencies with a tag in `[workspace.dependencies]`): `orchard-grid` (wgpu character-grid renderer), `orchard-theme` (color schemes, `Colors`, the config directory's location; `oc-config` re-exports `orchard_theme::schemes` as `themes` and `ConfigPaths::platform()` delegates to `orchard_theme::dirs`, so every app finds the same `color-schemes/` folder) and `orchard-metadata` (multi-rename metadata providers; `oc-rename::metadata` re-exports it and adds reading a `VPath`).
- **`oc_ui::App` owns all UI state**; the binary owns only winit windows, wgpu surfaces and the macOS muda menu. Drawing is a pure function into an `orchard_grid::Frame`; layout is recomputed, never stored; redraw only on change.
- **Never block the UI thread**: disk, network and processes go through `app.tasks` (`run_in_background`, `spawn`, `post`, `ask_ui`); each job gets its own thread.
- **Commands** (`commands.rs`) are the single source for key dispatch, the native menu, the F9 strip, the F-key bar and the command palette. `Primary` = ⌘ on macOS, Ctrl elsewhere. A shortcut belongs to exactly one command (a test enforces it).
- **Features** live in `crates/oc-ui/src/features/<name>.rs` with `register(reg)`, added to `ALL` in `features/mod.rs` (the only shared edit). Feature state goes in `app.state::<T>()`, not in new `App` fields. Settings are written with `app.update_settings` (only changed keys; 1.x keys keep their meaning; feature data in `extra` under a prefix).
- Extension points: new file systems implement `FileSystem` + `FileSystemProvider` (registered in `register_connection_types`, `app/src/main.rs`); new previewers implement `oc_preview::Previewer` (built-ins in `crates/oc-preview/src/builtin/mod.rs`). The file system contract and the previewer contract (`crates/oc-preview/src/tests/contract.rs`) apply to new implementations automatically.

## Gotchas

- **Tests must never touch the real config dir or keychain.** App tests use `oc_ui::testing::headless_app(config_dir, …)` with a `tempfile` dir; it swaps in `oc_config::MemoryCredentials`. Wait for background work with `settle` / `wait_until`, never with sleeps.
- **Disk fills fast.** `target/` alone reaches tens of GB; every extra `CARGO_TARGET_DIR` (parallel agents, worktrees, `package.sh`) adds a full build. Reuse one target dir where possible and remove stale ones.
- **FFmpeg must be LGPL-only** (no `--enable-gpl` / `--enable-nonfree`): BtbN `lgpl-shared` builds on Windows/Linux, `packaging/macos/build-ffmpeg.sh` on macOS (it fails on any other license). Homebrew's ffmpeg is GPL: fine for local experiments, never for anything shipped.
- **macOS packaging**: `packaging/macos/package.sh --notarize` with the notarytool keychain profile `orchard-commander` (`--profile` / `$OC_NOTARY_PROFILE` to override). It signs with `$OC_CODESIGN_IDENTITY` or the first "Developer ID Application" certificate. Ad-hoc builds make macOS forget Local Network and Keychain permissions on every rebuild, and a bare `cargo run` binary may be blocked from LAN hosts ("No route to host"): test LAN servers with the packaged app. `MACOSX_DEPLOYMENT_TARGET` is 12.0 (`packaging/macos/env.sh`); `package.sh` rejects any Mach-O needing newer.
- **Licenses**: after dependency or version changes rerun `scripts/third-party-licenses.sh` (regenerates `licenses/THIRD-PARTY-RUST.txt`, which is committed).
- **UI copy** (menus, commands, dialogs, buttons, toasts, errors, empty states, bottom lines) follows `docs/ai-microcopy-guide.md`: sentence case, front-loaded and short, buttons as action + object, errors that say what went wrong, why and how to fix it, deletions that say what goes and whether it can be undone, no periods on fragments. Read it before writing or changing user-visible text.
- **No third-party product names** in user-visible text, docs or code comments (describe the behaviour, e.g. "classic two-panel"), except where a license requires naming a component.
- Without a GPU (CI, VMs) `orchard_grid` falls back to a software adapter (WARP on Windows, llvmpipe on Linux; CI installs `mesa-vulkan-drivers`).
