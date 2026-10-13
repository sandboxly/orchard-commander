#!/usr/bin/env bash
# Download the prebuilt FFmpeg that oc-media links against (Windows, Linux).
#
#   scripts/fetch-ffmpeg.sh                  # this machine's platform
#   scripts/fetch-ffmpeg.sh linux-x64 win-x64
#   scripts/fetch-ffmpeg.sh all              # every platform listed below
#
# Builds come from https://github.com/BtbN/FFmpeg-Builds ("lgpl-shared":
# shared libraries, no GPL or non-free parts, LGPL version 3 or later because
# of --enable-version3), pinned to a month-end autobuild release (those are
# kept for years; the daily ones are deleted after a few weeks) and checked
# against the SHA-256 sums below. Each platform lands in
# vendor/ffmpeg/<platform>/ with include/, lib/ (import libraries, .so files),
# bin/ (Windows DLLs) and LICENSE.txt. Build with
#
#   FFMPEG_DIR=vendor/ffmpeg/<platform> cargo build ...
#
# and put the libraries next to the executable (Windows) or into lib/ next to
# it (Linux; the binary has an rpath of $ORIGIN/lib, see app/build.rs).
# `cargo test` / `cargo run` need them on PATH (Windows) or LD_LIBRARY_PATH.
# packaging/stage.sh copies only the libraries the app links.
#
# macOS has no LGPL build here: use Homebrew's ffmpeg for development
# (`brew install ffmpeg@8`, FFMPEG_DIR=$(brew --prefix ffmpeg@8): ffmpeg-next
# 8.1 does not build against FFmpeg 9); the release bundle is made by packaging/macos/.
#
# When updating: pick a month-end autobuild-YYYY-MM-DD-* release, take the
# sums from the GitHub API (assets[].digest) or `sha256sum`, and update
# licenses/ffmpeg/README.md (version, source and build-script commits).
set -euo pipefail

RELEASE="autobuild-2026-09-30-13-08"
BUILD="ffmpeg-n8.1.3-9-g29e619e767"
SERIES="8.1"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${FFMPEG_VENDOR_DIR:-$ROOT/vendor/ffmpeg}"
ALL=(linux-x64 linux-arm64 win-x64 win-arm64)

# BtbN's names for our platforms.
asset_of() {
  case "$1" in
    linux-x64) echo "$BUILD-linux64-lgpl-shared-$SERIES.tar.xz" ;;
    linux-arm64) echo "$BUILD-linuxarm64-lgpl-shared-$SERIES.tar.xz" ;;
    win-x64) echo "$BUILD-win64-lgpl-shared-$SERIES.zip" ;;
    win-arm64) echo "$BUILD-winarm64-lgpl-shared-$SERIES.zip" ;;
    mac-*) echo "no LGPL FFmpeg build for macOS here: brew install ffmpeg@8 (see the header of $0)" >&2; return 1 ;;
    *) echo "unsupported platform: $1 (one of: ${ALL[*]})" >&2; return 1 ;;
  esac
}

sha256_of() {
  case "$1" in
    linux-x64) echo d4d7b6936f492c0b866b1cb4a29a1bbae09d6a064b4ec4eb90c95bed7969535a ;;
    linux-arm64) echo d44838f375c66cf31a8b1473b6f8bad76e5c7dac55870c4d96a75c907a52ba2d ;;
    win-x64) echo 3e47bda1607740550141e37c0e49d1e5182b34699f15adfd137ee266d346811a ;;
    win-arm64) echo 7674d11915c95c8f65eeec36b329917c3023f6bae5786549cc47aed52cc1cea7 ;;
  esac
}

host_platform() {
  local os arch
  case "$(uname -s)" in
    Darwin) os=mac ;;
    Linux) os=linux ;;
    MINGW* | MSYS* | CYGWIN*) os=win ;;
    *) echo "unknown OS $(uname -s)" >&2; return 1 ;;
  esac
  case "$(uname -m)" in
    arm64 | aarch64) arch=arm64 ;;
    x86_64 | amd64) arch=x64 ;;
    *) echo "unknown CPU $(uname -m)" >&2; return 1 ;;
  esac
  echo "$os-$arch"
}

sha256() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

unpack() {
  case "$1" in
    *.tar.xz) tar -xJf "$1" -C "$2" ;;
    *.zip)
      if command -v unzip >/dev/null; then
        unzip -q "$1" -d "$2"
      else
        # Git Bash without unzip: Windows' own bsdtar reads zip files.
        "$SYSTEMROOT/System32/tar.exe" -xf "$1" -C "$2"
      fi
      ;;
  esac
}

fetch() {
  local platform="$1" asset want tmp got top
  asset="$(asset_of "$platform")"
  want="$(sha256_of "$platform")"
  if [[ -d "$DEST/$platform/include" && -f "$DEST/$platform/.sha256" && "$(cat "$DEST/$platform/.sha256")" == "$want" ]]; then
    echo "ffmpeg $BUILD $platform: up to date"
    return
  fi
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  echo "ffmpeg $BUILD $platform: downloading"
  curl -fsSL --retry 3 --connect-timeout 20 --max-time 900 -o "$tmp/$asset" \
    "https://github.com/BtbN/FFmpeg-Builds/releases/download/$RELEASE/$asset"
  got="$(sha256 "$tmp/$asset")"
  if [[ "$got" != "$want" ]]; then
    echo "$asset: SHA-256 mismatch (got $got, want $want)" >&2
    return 1
  fi
  mkdir -p "$tmp/x"
  unpack "$tmp/$asset" "$tmp/x"
  top="$tmp/x/${asset%.tar.xz}"
  top="${top%.zip}"
  rm -rf "${DEST:?}/$platform"
  mkdir -p "$DEST/$platform"
  # Headers, libraries and the license; not the programs, docs or man pages.
  cp -R "$top/include" "$top/lib" "$top/LICENSE.txt" "$DEST/$platform/"
  if [[ "$platform" == win-* ]]; then
    mkdir -p "$DEST/$platform/bin"
    cp "$top"/bin/*.dll "$DEST/$platform/bin/"
  fi
  echo "$want" >"$DEST/$platform/.sha256"
  echo "ffmpeg $BUILD $platform: FFMPEG_DIR=$DEST/$platform"
}

platforms=("$@")
if [[ ${#platforms[@]} -eq 0 ]]; then
  platforms=("$(host_platform)")
elif [[ "${platforms[0]}" == "all" ]]; then
  platforms=("${ALL[@]}")
fi
for p in "${platforms[@]}"; do
  fetch "$p"
done
