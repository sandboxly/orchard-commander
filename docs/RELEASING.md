# Building and releasing Orchard Commander 2.x

The release assets and identities (names, bundle id, Inno AppId, install
paths) are fixed by [RELEASE-2.0-REQUIREMENTS.md](RELEASE-2.0-REQUIREMENTS.md).
Windows and Linux packages come from CI; the macOS app is built, signed and
notarized locally.

## Version

One version for everything: `[workspace.package] version` in `Cargo.toml`.
`orchard-commander --version`, `--self-test`, About, the installer
(`/DAppVersion`, read with `cargo pkgid` in CI) and the Linux packages
(`VERSION` for nfpm) all take it from there. Bump it, run `cargo check` so
`Cargo.lock` follows, and rerun `scripts/third-party-licenses.sh` (the
workspace crates are listed there with their version).

## Native libraries

| | Windows | Linux | macOS |
| --- | --- | --- | --- |
| PDFium (`scripts/fetch-pdfium.sh`, pinned + SHA-256) | `pdfium.dll` next to the exe | `libpdfium.so` next to the binary | `Contents/Frameworks` |
| FFmpeg, LGPL shared libraries | `scripts/fetch-ffmpeg.sh win-x64` (BtbN lgpl-shared 8.1, pinned + SHA-256); DLLs next to the exe | `scripts/fetch-ffmpeg.sh linux-x64`; `lib/*.so.N` next to the binary, found via the binary's rpath `$ORIGIN/lib` (`app/build.rs`) | `packaging/macos/` (must be an LGPL build too: Homebrew's ffmpeg is GPL) |

Build against the fetched FFmpeg with `FFMPEG_DIR=vendor/ffmpeg/<platform>`
(bindgen needs libclang: `libclang-dev` on Linux, `LIBCLANG_PATH` to LLVM's
`bin` on Windows). For `cargo test` / `cargo run` put the libraries on the
library path (`PATH` on Windows, `LD_LIBRARY_PATH` on Linux). Windows builds
link the C runtime statically (`.cargo/config.toml`), so no Visual C++
Redistributable is needed.

The FFmpeg build bundles the codec libraries it was configured with (dav1d,
libvpx, libaom, Opus, LAME, OpenSSL, …) but no GPL or non-free parts; its
license is LGPL 3.0 or later. To move to a newer FFmpeg: pick a month-end
`autobuild-*` release of BtbN/FFmpeg-Builds (the daily ones are deleted after
a few weeks), update `RELEASE`/`BUILD`/`SERIES` and the sums in
`scripts/fetch-ffmpeg.sh`, the version and commits in
`licenses/ffmpeg/README.md`, and the library names in `packaging/stage.sh` /
the CI checks if the major versions (`avcodec-62` …) change. ffmpeg-next must
support that FFmpeg version.

## Licenses

`licenses/` is what ships (see its README): our MIT license, UnRAR, PDFium's
and FFmpeg's texts, and `THIRD-PARTY-RUST.txt`, generated from `Cargo.lock`
with cargo-about by `scripts/third-party-licenses.sh`. The generated file is
committed (so local builds such as the macOS one have it); the CI packaging
job checks it, warns when it is stale and regenerates it for the packages.

`packaging/stage.sh` puts the folder into every package:
`licenses\` in the Windows install folder and zip, `/opt/orchard-commander/licenses`
and `/usr/share/doc/orchard-commander/` on Linux (also in the tar.gz and the
AppImage), and — through `packaging/stage.sh licenses - <dir>` from the macOS
script — `Contents/Resources/licenses` in the app. Options > About Orchard
Commander shows the version and opens that folder.

## Windows and Linux: CI

`.github/workflows/ci.yml` runs on every push to `main` (and
by hand, workflow_dispatch):

- **Tests** (ubuntu-22.04, windows-latest, macos-latest): clippy with
  `-D warnings`, `cargo test --workspace --all-features` (config dir in a
  temp folder; Docker-based SFTP/SMB tests skip without Docker), a release
  build and its `--self-test`.
- **Package** (windows-latest, ubuntu-22.04): release build, staged app
  folder (`packaging/stage.sh`), `--self-test` of that folder without any
  library paths set, then
  - Windows: `Orchard-Commander-Setup.exe` (Inno Setup,
    `packaging/windows/orchard-commander.iss`, the AppId of 1.x so it upgrades
    in place) and `Orchard-Commander-Windows.zip`; silent per-user install
    (`/CURRENTUSER`), installed files compared with the folder, self-test,
    silent uninstall that must remove the install folder.
  - Linux: `Orchard-Commander-Linux.tar.gz`,
    `Orchard-Commander-Linux-x86_64.{deb,rpm}` (nfpm,
    `packaging/linux/nfpm.yaml`) and `Orchard-Commander-Linux-x86_64.AppImage`
    (appimagetool, `packaging/linux/AppRun`); the .deb is installed with apt and
    self-tested and removed, the rpm the same in a Fedora container, the
    AppImage and the tarball are self-tested.
  - The files are uploaded as the workflow artifacts `packages-Windows` and
    `packages-Linux`.

No step uploads to a GitHub release or publishes anywhere, and the workflow
has no tag trigger: release wiring (tag trigger, `gh release upload`, the
package-manager manifests of 1.x's `packages.yml`) is added when 2.0 replaces
1.x on `main`.

## macOS: local

The macOS agent's script in `packaging/macos/` builds
`Orchard-Commander-macOS.dmg`: release build, app bundle with PDFium and
FFmpeg in `Contents/Frameworks`, licenses in `Contents/Resources/licenses`,
Developer ID signing, notarization and stapling. See that folder's README.

## Making a release (once 2.0 is on main)

1. Bump the version, regenerate the licenses file, commit, push; wait for a
   green CI run.
2. Download `packages-Windows` and `packages-Linux` from that run and build the
   macOS dmg locally from the same commit.
3. Create the release and attach all assets (names in
   RELEASE-2.0-REQUIREMENTS.md); then update brew/winget/Chocolatey/AUR.
