# Third-party licenses to ship

Orchard Commander itself is MIT licensed (`LICENSE`). Release packages must
also carry the licenses below: in the macOS bundle (`Contents/Resources/licenses/`),
next to the Windows executable, and in `/usr/share/doc/orchard-commander/` on Linux.
The About / licenses page should show them too.

## Needs its own text in every package

| Component | Where it comes from | License | File |
| --- | --- | --- | --- |
| UnRAR (RAR decoder) | `unrar_sys` crate, vendored C++ source in `vendor/unrar`; only with the `oc-archive/rar` cargo feature (default on) | RARLAB UnRAR license: freeware, **not** open source; forbids using the code to re-create the RAR compression algorithm | [`UnRAR.txt`](UnRAR.txt) (copied verbatim from `unrar_sys-0.5.8/vendor/unrar/license.txt`) |

FFmpeg (audio/video previews, crate `oc-media`) ships as LGPL-2.1-or-later
shared libraries built without GPL or non-free parts. On macOS
`packaging/macos/build-ffmpeg.sh` builds them from the pinned release tarball
and writes `LICENSE.md`, `COPYING.LGPLv2.1` and `SOURCE.txt` (source URL,
SHA-256, configure line) next to them; `packaging/macos/package.sh` puts these
in `Contents/Resources/licenses/ffmpeg/`. The Rust crates' notice
(`THIRD-PARTY-LICENSES.txt`) is generated with cargo-about from
`packaging/about.toml` + `packaging/about.hbs` when packaging.

Builds without RAR support (`cargo build --no-default-features --features …` on
`oc-archive`) don't contain UnRAR and don't need `UnRAR.txt`.

## Bundled C libraries (notice required; texts generated, see below)

| Library | Pulled in by | License |
| --- | --- | --- |
| Oniguruma (regex engine for syntax highlighting) | `onig_sys` via `syntect` (`regex-onig`) in `oc-preview` | BSD-2-Clause |
| liblzma (xz) | `liblzma-sys` in `oc-archive` | 0BSD / public domain |
| zstd | `zstd-sys` via `zip` in `oc-archive` | BSD-3-Clause |

## Rust crates

Nearly all crate dependencies are MIT and/or Apache-2.0 (some BSD, Zlib,
Unicode-3.0, MPL-2.0 for a few). Their notices must accompany binary
releases. Generate the combined file at release time instead of keeping it
here, so it always matches `Cargo.lock`:

```sh
cargo install cargo-about
cargo about generate about.hbs > THIRD-PARTY-LICENSES.html   # or a .txt template
```

Fonts are not bundled (the renderer uses installed system fonts), so no font
licenses apply.
