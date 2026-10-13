# FFmpeg

Audio and video previews (crate `oc-media`) use FFmpeg's libraries libavcodec,
libavformat, libavutil, libswresample and libswscale, dynamically linked and
shipped as separate shared libraries next to the program (Windows:
`avcodec-62.dll` …; Linux: `lib/libavcodec.so.62` …). You may replace them with
your own compatible build of the same FFmpeg version.

FFmpeg is licensed under the GNU Lesser General Public License, version 3 or
later (this build is configured with `--enable-version3` and without
`--enable-gpl` or `--enable-nonfree`):

- `LGPL-3.0.txt`: the GNU Lesser General Public License, version 3
  (copied from the build archive's `LICENSE.txt`)
- `GPL-3.0.txt`: the GNU General Public License, version 3, whose terms
  the LGPL version 3 incorporates

## Windows and Linux packages

- FFmpeg version: n8.1.3-9-g29e619e767 (release/8.1 branch)
- Source code: <https://github.com/FFmpeg/FFmpeg/tree/29e619e767cde9045a75c29bc9a8278ae7b3a98b>
  (also <https://ffmpeg.org/download.html>, release 8.1.x)
- Binaries: <https://github.com/BtbN/FFmpeg-Builds/releases/tag/autobuild-2026-09-30-13-08>,
  variant `lgpl-shared` (`linux64`, `win64`), fetched and checked by
  `scripts/fetch-ffmpeg.sh`
- Build scripts and the sources of the libraries compiled into it
  (dav1d, libvpx, libaom, Opus, LAME, OpenSSL, …; each under its own
  LGPL-compatible license): <https://github.com/BtbN/FFmpeg-Builds/tree/6c9aec5fc9a72ec3abedd1fa84db141fa18cf52b>
- Configuration: printed by the libraries themselves
  (`avcodec_configuration()`), starting with
  `--enable-version3 --enable-shared --disable-static …`

Orchard Commander makes no changes to FFmpeg. On request we provide the exact
corresponding source code for three years from the release: open an issue at
<https://github.com/sandboxly/orchard-commander/issues>.

## macOS

The macOS app bundles its own FFmpeg build; see `packaging/macos/` for the
version and source. The same license texts apply.
