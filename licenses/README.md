# Licenses

Orchard Commander is free software under the MIT license (`LICENSE.txt` in
the packages, `LICENSE` in the source). It contains or comes with the
third-party components below, each under its own license. This folder ships
with every package: `licenses\` in the Windows install folder and zip,
`/usr/share/doc/orchard-commander/` and `/opt/orchard-commander/licenses/` on
Linux (also inside the tar.gz and AppImage), `Contents/Resources/licenses/`
in the macOS app. Options > About Orchard Commander opens it.

| Component | Used for | License | Files |
| --- | --- | --- | --- |
| FFmpeg (libavcodec, libavformat, libavutil, libswresample, libswscale), shared libraries | audio and video previews | LGPL 3.0 or later | `ffmpeg/` (version and source in `ffmpeg/README.md`) |
| PDFium, shared library | PDF previews | BSD-3-Clause and Apache-2.0, plus the libraries compiled into it | `pdfium/` (see `pdfium/README.md`) |
| UnRAR (RAR decoder) | opening RAR archives | RARLAB UnRAR license: freeware, **not** open source; the code must not be used to re-create the RAR compression algorithm | `UnRAR.txt` |
| Rust crates (about 600; MIT, Apache-2.0, BSD, ISC, Zlib, Unicode-3.0, MPL-2.0 and others) and the C code some of them compile in (Oniguruma: BSD-2-Clause, liblzma: 0BSD, zstd: BSD-3-Clause, bzip2) | everything else | per crate | `THIRD-PARTY-RUST.txt` |

Fonts are not bundled: Orchard Commander uses the fonts installed on the
system.

On macOS the FFmpeg libraries are not BtbN's but an LGPL-2.1-or-later build
without GPL or non-free parts made by `packaging/macos/build-ffmpeg.sh` from
the pinned release tarball; it writes `LICENSE.md`, `COPYING.LGPLv2.1` and
`SOURCE.txt` (source URL, SHA-256, configure line), which
`packaging/macos/package.sh` puts in `Contents/Resources/licenses/ffmpeg/`.

## For maintainers

- `UnRAR.txt` is copied verbatim from `unrar_sys-0.5.8/vendor/unrar/license.txt`.
  It is only needed with the `rar` cargo feature (default on); builds without
  it (`--no-default-features` on the app) don't contain UnRAR.
- `pdfium/` is copied from the pdfium-binaries archive that
  `scripts/fetch-pdfium.sh` pins; update it with the pin.
- `ffmpeg/` matches the build that `scripts/fetch-ffmpeg.sh` pins (Windows,
  Linux); update `ffmpeg/README.md` with the pin.
- `THIRD-PARTY-RUST.txt` is generated from `Cargo.lock` by
  `scripts/third-party-licenses.sh` (cargo-about, `about.toml`,
  `packaging/licenses/about.hbs`). Rerun it after dependency changes; the
  packaging CI job regenerates it for the packages anyway.
- `packaging/stage.sh` copies this folder into every package
  (`packaging/stage.sh licenses - <dir>` for the macOS bundle).
