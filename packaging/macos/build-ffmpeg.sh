#!/usr/bin/env bash
# Build the FFmpeg shared libraries that the macOS app bundles.
#
#   packaging/macos/build-ffmpeg.sh            # -> vendor/ffmpeg/mac-arm64/
#
# LGPL-only: no --enable-gpl / --enable-nonfree, and the script fails if the
# configured licence is anything but "LGPL version 2.1 or later". Only the
# libraries oc-media uses are built (avcodec, avformat, avutil, swscale,
# swresample: see ffmpeg-next's features in crates/oc-media/Cargo.toml), as
# shared libraries with an @rpath install name, so the app finds them in
# Contents/Frameworks and users can replace them (LGPL section 6).
#
# What is left out: programs, docs, avdevice, avfilter, encoders, muxers,
# devices, filters, the network and every protocol but "file" (oc-media opens
# local paths itself and feeds everything else through custom AVIO), and all
# auto-detected external libraries (so nothing from Homebrew or /usr/local
# leaks in) except the system's zlib and bzip2. Hardware decoding is not
# built: oc-media decodes in software and never sets up a VideoToolbox device.
#
# The source is the pinned release tarball below, checked against its SHA-256
# (taken from the tarball whose signature verified against FFmpeg's release
# key FCF986EA15E6E293A5644F10B4322F04D67658D8). The output folder also gets
# the licence texts and SOURCE.txt (exact source URL, hash, configure line),
# which package.sh ships in Contents/Resources/licenses/ffmpeg.
#
# Rebuilds only when the version, flags or deployment target change.
set -euo pipefail

VERSION="8.0.3"
SHA256="6136812ea6d4e68bdba27e33c2a94382711cdf4f8602ffef056ff792bd6f9818"
URL="https://ffmpeg.org/releases/ffmpeg-$VERSION.tar.xz"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck source=packaging/macos/env.sh
source "$ROOT/packaging/macos/env.sh"
ARCH="${OC_MAC_ARCH:-arm64}"
case "$ARCH" in
  arm64) FF_ARCH=aarch64 ;;
  x86_64) FF_ARCH=x86_64 ;;
  *) echo "unsupported arch $ARCH" >&2; exit 1 ;;
esac
OUT="$ROOT/vendor/ffmpeg/mac-$ARCH"
CACHE="$ROOT/vendor/ffmpeg/src"

FLAGS=(
  --enable-shared --disable-static --enable-pic
  --disable-programs --disable-doc --disable-debug
  --disable-avdevice --disable-avfilter
  --disable-encoders --disable-muxers --disable-devices --disable-filters
  --disable-network --disable-protocols --enable-protocol=file
  --disable-autodetect --enable-zlib --enable-bzlib
)
STAMP="$VERSION $SHA256 $ARCH $MACOSX_DEPLOYMENT_TARGET ${FLAGS[*]}"

if [[ -f "$OUT/.stamp" && "$(cat "$OUT/.stamp")" == "$STAMP" ]]; then
  echo "ffmpeg $VERSION mac-$ARCH: up to date ($OUT)"
  exit 0
fi

sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }

mkdir -p "$CACHE"
tarball="$CACHE/ffmpeg-$VERSION.tar.xz"
if [[ ! -f "$tarball" || "$(sha256 "$tarball")" != "$SHA256" ]]; then
  echo "ffmpeg $VERSION: downloading $URL"
  curl -fsSL --retry 3 --connect-timeout 20 --max-time 900 -o "$tarball.part" "$URL"
  got="$(sha256 "$tarball.part")"
  if [[ "$got" != "$SHA256" ]]; then
    rm -f "$tarball.part"
    echo "ffmpeg-$VERSION.tar.xz: SHA-256 mismatch (got $got, want $SHA256)" >&2
    exit 1
  fi
  mv "$tarball.part" "$tarball"
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/oc-ffmpeg.XXXXXX")"
trap 'rm -rf "$work"' EXIT
tar -xJf "$tarball" -C "$work"
src="$work/ffmpeg-$VERSION"

# Keep the developer's environment (Homebrew flags, pkg-config paths) out.
unset CFLAGS CPPFLAGS CXXFLAGS LDFLAGS CPATH C_INCLUDE_PATH LIBRARY_PATH PKG_CONFIG_PATH
export PKG_CONFIG_LIBDIR="$work/no-pkgconfig"

sdk="$(xcrun --sdk macosx --show-sdk-path)"
minflag="-mmacosx-version-min=$MACOSX_DEPLOYMENT_TARGET"
configure=(
  ./configure
  --prefix="$work/install"
  --install-name-dir=@rpath
  --arch="$FF_ARCH" --target-os=darwin --cc=clang
  --sysroot="$sdk"
  --extra-cflags="-arch $ARCH $minflag"
  --extra-ldflags="-arch $ARCH $minflag -Wl,-headerpad_max_install_names"
  "${FLAGS[@]}"
)
if [[ "$ARCH" != "$(uname -m)" ]]; then
  configure+=(--enable-cross-compile)
fi
if [[ "$ARCH" == x86_64 ]] && ! command -v nasm >/dev/null; then
  configure+=(--disable-x86asm)
  echo "warning: nasm not found, building x86_64 without assembly (slow decoding)" >&2
fi

echo "ffmpeg $VERSION mac-$ARCH: configuring (macOS $MACOSX_DEPLOYMENT_TARGET)"
(cd "$src" && "${configure[@]}" >"$work/configure.log") || { tail -30 "$src/ffbuild/config.log"; exit 1; }

license="$(sed -n 's/^#define FFMPEG_LICENSE "\(.*\)"$/\1/p' "$src/config.h")"
if [[ "$license" != "LGPL version 2.1 or later" ]] || grep -Eq '^#define CONFIG_(GPL|NONFREE|GPLV3|VERSION3) 1' "$src/config.h"; then
  echo "ffmpeg: configured licence is \"$license\", expected LGPL 2.1+ only" >&2
  exit 1
fi

echo "ffmpeg $VERSION mac-$ARCH: building"
make -C "$src" -j"$(sysctl -n hw.ncpu)" >"$work/make.log" 2>&1 || { tail -40 "$work/make.log"; exit 1; }
make -C "$src" install >/dev/null

rm -rf "$OUT"
mkdir -p "$OUT/lib"
cp -R "$work/install/include" "$OUT/include"
cp -R "$work/install/lib/pkgconfig" "$OUT/lib/pkgconfig"
# The real files only (libavcodec.62.dylib), plus the unversioned link for the linker.
for lib in avcodec avformat avutil swscale swresample; do
  real="$(cd "$work/install/lib" && ls "lib$lib".*.dylib | awk -F. 'NF == 3')"
  cp "$work/install/lib/$real" "$OUT/lib/$real"
  ln -s "$real" "$OUT/lib/lib$lib.dylib"
done
# pkg-config files point at the final folder.
sed -i '' "s|$work/install|$OUT|g" "$OUT"/lib/pkgconfig/*.pc

mkdir -p "$OUT/licenses"
cp "$src/LICENSE.md" "$src/COPYING.LGPLv2.1" "$OUT/licenses/"
cat >"$OUT/licenses/SOURCE.txt" <<EOF
FFmpeg $VERSION ($license), shared libraries built from the unmodified
release source:

  $URL
  SHA-256 $SHA256

Configured with:

  ${configure[*]}

(--prefix and --sysroot were build-machine paths.) The build script is
packaging/macos/build-ffmpeg.sh in https://github.com/sandboxly/orchard-commander.
The libraries are in Contents/Frameworks and may be replaced with your own
builds of the same major versions.
EOF
echo "$STAMP" >"$OUT/.stamp"

for f in "$OUT"/lib/*.*.dylib; do
  if otool -L "$f" | tail -n +2 | grep -Eq '/opt/homebrew|/usr/local'; then
    echo "$f links outside the system:" >&2
    otool -L "$f" >&2
    exit 1
  fi
done
echo "ffmpeg $VERSION mac-$ARCH: $OUT ($license)"
