#!/usr/bin/env bash
# Download the prebuilt PDFium library that oc-pdf loads at run time.
#
#   scripts/fetch-pdfium.sh                  # this machine's platform
#   scripts/fetch-pdfium.sh mac-arm64 win-x64
#   scripts/fetch-pdfium.sh all              # every platform we ship
#
# Binaries come from https://github.com/bblanchon/pdfium-binaries, pinned to
# the release that matches pdfium-render's `pdfium_7881` API feature (see
# crates/oc-pdf/Cargo.toml) and checked against the SHA-256 sums below.
# Each platform lands in vendor/pdfium/<platform>/ (library, VERSION, LICENSE,
# licenses/), which debug builds find on their own. For a release, copy the
# library next to the executable (or into Contents/Frameworks of the macOS
# app bundle), or point OC_PDFIUM_PATH at it (then nothing else is tried).
set -euo pipefail

RELEASE="chromium/7881"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${PDFIUM_VENDOR_DIR:-$ROOT/vendor/pdfium}"
ALL=(mac-arm64 mac-x64 win-x64 linux-x64)

sha256_of() {
  case "$1" in
    mac-arm64) echo 52e94ca5aa8847934330daf3f8150c190682c5ca93831468794f8b90d4392e40 ;;
    mac-x64) echo 6dedf83990e0e3d6b7c93c9e7589c5a126b0ae14b7464d76120cff7a26afb18b ;;
    win-x64) echo 73cc0de638ac2095e7445bf56a38200a5b7c7ca0e9f4ba144598f2457377ac08 ;;
    linux-x64) echo 1470e21b8b4a3b4ad7f85684e2da11d94f3b69a86d81dee11b9b6709d927ac1d ;;
    *) echo "unsupported platform: $1 (one of: ${ALL[*]})" >&2; return 1 ;;
  esac
}

library_of() {
  case "$1" in
    mac-*) echo lib/libpdfium.dylib ;;
    win-*) echo bin/pdfium.dll ;;
    *) echo lib/libpdfium.so ;;
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

fetch() {
  local platform="$1" want lib tmp got
  want="$(sha256_of "$platform")"
  lib="$(library_of "$platform")"
  if [[ -f "$DEST/$platform/$(basename "$lib")" && -f "$DEST/$platform/.sha256" && "$(cat "$DEST/$platform/.sha256")" == "$want" ]]; then
    echo "pdfium $RELEASE $platform: up to date"
    return
  fi
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  echo "pdfium $RELEASE $platform: downloading"
  curl -fsSL --retry 3 --connect-timeout 20 --max-time 600 -o "$tmp/pdfium.tgz" \
    "https://github.com/bblanchon/pdfium-binaries/releases/download/${RELEASE/\//%2F}/pdfium-$platform.tgz"
  got="$(sha256 "$tmp/pdfium.tgz")"
  if [[ "$got" != "$want" ]]; then
    echo "pdfium-$platform.tgz: SHA-256 mismatch (got $got, want $want)" >&2
    return 1
  fi
  mkdir -p "$tmp/x"
  tar -xzf "$tmp/pdfium.tgz" -C "$tmp/x"
  rm -rf "${DEST:?}/$platform"
  mkdir -p "$DEST/$platform"
  cp "$tmp/x/$lib" "$tmp/x/VERSION" "$tmp/x/LICENSE" "$DEST/$platform/"
  cp -R "$tmp/x/licenses" "$DEST/$platform/licenses"
  echo "$want" >"$DEST/$platform/.sha256"
  echo "pdfium $RELEASE $platform: $DEST/$platform/$(basename "$lib")"
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
