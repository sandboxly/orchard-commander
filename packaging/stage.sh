#!/usr/bin/env bash
# Assemble the portable app folder that every Windows/Linux package is made
# from (installer, zip, tar.gz, deb, rpm, AppImage):
#
#   packaging/stage.sh win-x64   target/release/orchard-commander.exe dist/orchard-commander
#   packaging/stage.sh linux-x64 target/release/orchard-commander     dist/orchard-commander
#   packaging/stage.sh licenses  -                                    "Orchard Commander.app/Contents/Resources/licenses"
#
# Layout (Windows)                    Layout (Linux)
#   orchard-commander.exe               orchard-commander
#   pdfium.dll                          libpdfium.so
#   avcodec-62.dll, avformat-62.dll,    lib/libavcodec.so.62, libavformat.so.62,
#   avutil-60.dll, swresample-6.dll,        libavutil.so.60, libswresample.so.6,
#   swscale-9.dll                           libswscale.so.9  (rpath $ORIGIN/lib)
#   licenses/                           licenses/
#
# PDFium and FFmpeg come from scripts/fetch-pdfium.sh and
# scripts/fetch-ffmpeg.sh (run them first). The `licenses` form only copies
# the license folder (for the macOS bundle, packaging/macos/).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PDFIUM="${PDFIUM_VENDOR_DIR:-$ROOT/vendor/pdfium}"
FFMPEG="${FFMPEG_VENDOR_DIR:-$ROOT/vendor/ffmpeg}"
# The FFmpeg libraries oc-media links (ffmpeg-next features codec, format,
# software-resampling, software-scaling); not avfilter/avdevice.
FFMPEG_LIBS=(avcodec avformat avutil swresample swscale)

if [[ $# -ne 3 ]]; then
  sed -n '2,20p' "$0" >&2
  exit 2
fi
platform="$1" binary="$2" dest="$3"

# Our license, the third-party ones and a README saying what is what.
stage_licenses() {
  local out="$1"
  rm -rf "$out"
  mkdir -p "$out"
  cp "$ROOT/LICENSE" "$out/LICENSE.txt"
  cp "$ROOT/licenses/README.md" "$ROOT/licenses/UnRAR.txt" "$ROOT/licenses/THIRD-PARTY-RUST.txt" "$out/"
  cp -R "$ROOT/licenses/ffmpeg" "$ROOT/licenses/pdfium" "$out/"
}

need() {
  if [[ ! -e "$1" ]]; then
    echo "missing $1 ($2)" >&2
    exit 1
  fi
}

case "$platform" in
  licenses)
    stage_licenses "$dest"
    echo "licenses: $dest"
    exit 0
    ;;
  win-*)
    need "$binary" "cargo build --release"
    need "$PDFIUM/$platform/pdfium.dll" "scripts/fetch-pdfium.sh $platform"
    need "$FFMPEG/$platform/bin" "scripts/fetch-ffmpeg.sh $platform"
    rm -rf "$dest"
    mkdir -p "$dest"
    cp "$binary" "$dest/orchard-commander.exe"
    cp "$PDFIUM/$platform/pdfium.dll" "$dest/"
    for lib in "${FFMPEG_LIBS[@]}"; do
      cp "$FFMPEG/$platform/bin/$lib"-*.dll "$dest/"
    done
    ;;
  linux-*)
    need "$binary" "cargo build --release"
    need "$PDFIUM/$platform/libpdfium.so" "scripts/fetch-pdfium.sh $platform"
    need "$FFMPEG/$platform/lib" "scripts/fetch-ffmpeg.sh $platform"
    rm -rf "$dest"
    mkdir -p "$dest/lib"
    cp "$binary" "$dest/orchard-commander"
    chmod 755 "$dest/orchard-commander"
    cp "$PDFIUM/$platform/libpdfium.so" "$dest/"
    for lib in "${FFMPEG_LIBS[@]}"; do
      # The soname file (libavcodec.so.62, a symlink to .so.62.x.y) as a plain file.
      local_so=("$FFMPEG/$platform/lib/lib$lib".so.[0-9]*)
      for so in "${local_so[@]}"; do
        if [[ "$so" =~ \.so\.[0-9]+$ ]]; then
          cp -L "$so" "$dest/lib/"
        fi
      done
    done
    ;;
  *)
    echo "unsupported platform: $platform (win-x64, linux-x64, licenses)" >&2
    exit 2
    ;;
esac
stage_licenses "$dest/licenses"
echo "staged $platform: $dest"
find "$dest" -maxdepth 2 -type f ! -path "*/licenses/*" -exec ls -l {} + | awk '{print "  " $5 "\t" $NF}'
