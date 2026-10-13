#!/usr/bin/env bash
# Build "Orchard Commander.app" and dist/Orchard-Commander-macOS.dmg.
#
#   packaging/macos/package.sh               # build, bundle, sign, dmg
#   packaging/macos/package.sh --notarize    # ... and notarize + staple the dmg
#
# Options:
#   --identity NAME   code-signing identity (default: $OC_CODESIGN_IDENTITY, else
#                     the first "Developer ID Application" in the keychain, else ad-hoc)
#   --adhoc           ad-hoc signature (no Developer ID; can't be notarized)
#   --notarize        submit the dmg to Apple's notary service, wait, staple
#   --profile NAME    notarytool keychain profile (default: $OC_NOTARY_PROFILE or
#                     "orchard-commander"; create it once with
#                     `xcrun notarytool store-credentials orchard-commander ...`)
#   --no-build        reuse the last cargo build
#   --dist DIR        output folder (default: dist/ in the repository)
#
# Steps: FFmpeg (packaging/macos/build-ffmpeg.sh, LGPL shared libraries) and
# PDFium (scripts/fetch-pdfium.sh) into vendor/; `cargo build --release`
# linked against that FFmpeg with an @executable_path/../Frameworks rpath;
# the bundle (Info.plist from packaging/macos/Info.plist, dylibs in
# Contents/Frameworks, licences in Contents/Resources/licenses); checks that
# no Mach-O references Homebrew//usr/local or needs a newer macOS than
# MACOSX_DEPLOYMENT_TARGET; inside-out hardened-runtime signing with a secure
# timestamp; --self-test of the bundle; the dmg (app + /Applications link).
#
# Output: dist/Orchard Commander.app, dist/Orchard-Commander-macOS.dmg.
# CARGO_TARGET_DIR is honoured.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HERE="$ROOT/packaging/macos"
# shellcheck source=packaging/macos/env.sh
source "$HERE/env.sh"

APP_NAME="Orchard Commander"
EXE="orchard-commander"
ARCH=arm64
TARGET=aarch64-apple-darwin
DIST="$ROOT/dist"
FFMPEG="$ROOT/vendor/ffmpeg/mac-$ARCH"
PDFIUM="$ROOT/vendor/pdfium/mac-$ARCH"

identity="${OC_CODESIGN_IDENTITY:-}"
adhoc=0
notarize=0
build=1
profile="${OC_NOTARY_PROFILE:-orchard-commander}"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --identity) identity="$2"; shift 2 ;;
    --adhoc) adhoc=1; shift ;;
    --notarize) notarize=1; shift ;;
    --profile) profile="$2"; shift 2 ;;
    --no-build) build=0; shift ;;
    --dist) DIST="$2"; shift 2 ;;
    -h | --help) sed -n '2,/^set -/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option $1 (see --help)" >&2; exit 2 ;;
  esac
done

mkdir -p "$DIST"
DIST="$(cd "$DIST" && pwd)"
APP="$DIST/$APP_NAME.app"
DMG="$DIST/Orchard-Commander-macOS.dmg"

step() { printf '\n==> %s\n' "$*"; }

if [[ $adhoc -eq 1 ]]; then
  identity="-"
elif [[ -z "$identity" ]]; then
  identity="$(security find-identity -v -p codesigning | sed -n 's/.*"\(Developer ID Application:[^"]*\)".*/\1/p' | head -1)"
  if [[ -z "$identity" ]]; then
    echo "No \"Developer ID Application\" identity in the keychain: signing ad-hoc." >&2
    identity="-"
  fi
fi
if [[ $notarize -eq 1 && "$identity" != "Developer ID Application:"* ]]; then
  echo "--notarize needs a \"Developer ID Application\" identity (have: $identity)" >&2
  exit 1
fi

VERSION="$(cargo metadata --no-deps --format-version 1 --manifest-path "$ROOT/Cargo.toml" |
  python3 -c 'import json, sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "orchard-commander"))')"
echo "Orchard Commander $VERSION, macOS $MACOSX_DEPLOYMENT_TARGET+ $ARCH, signing: $identity"

# --- dependencies + build ------------------------------------------------------

step "FFmpeg"
"$HERE/build-ffmpeg.sh"
step "PDFium"
"$ROOT/scripts/fetch-pdfium.sh" "mac-$ARCH"

TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="$TARGET_DIR/$TARGET/release/$EXE"
if [[ $build -eq 1 ]]; then
  step "cargo build --release"
  (
    unset CFLAGS CPPFLAGS CXXFLAGS LDFLAGS CPATH C_INCLUDE_PATH LIBRARY_PATH PKG_CONFIG_PATH
    export FFMPEG_DIR="$FFMPEG"
    export PKG_CONFIG_LIBDIR="$FFMPEG/lib/pkgconfig"
    export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C link-arg=-Wl,-rpath,@executable_path/../Frameworks"
    cargo build --release --locked --target "$TARGET" -p orchard-commander --manifest-path "$ROOT/Cargo.toml"
  )
fi
[[ -x "$BIN" ]] || { echo "missing $BIN (build first)" >&2; exit 1; }

# --- bundle ----------------------------------------------------------------------

step "Bundle $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Frameworks" "$APP/Contents/Resources/licenses"
cp "$BIN" "$APP/Contents/MacOS/$EXE"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@MIN_MACOS@/$MACOSX_DEPLOYMENT_TARGET/g" "$HERE/Info.plist" >"$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist" >/dev/null
printf 'APPL????' >"$APP/Contents/PkgInfo"
cp "$ROOT/packaging/icons/app.icns" "$APP/Contents/Resources/app.icns"

# FFmpeg: the versioned files the binary references (@rpath/libavcodec.62.dylib ...).
for lib in $(otool -L "$BIN" | awk '$1 ~ /^@rpath\/lib(av|sw)/ { sub("@rpath/", "", $1); print $1 }'); do
  cp "$FFMPEG/lib/$lib" "$APP/Contents/Frameworks/$lib"
done
# ... and what they reference among themselves.
for f in "$APP"/Contents/Frameworks/*.dylib; do
  for lib in $(otool -L "$f" | awk 'NR > 1 && $1 ~ /^@rpath\// { sub("@rpath/", "", $1); print $1 }'); do
    [[ -f "$APP/Contents/Frameworks/$lib" ]] || cp "$FFMPEG/lib/$lib" "$APP/Contents/Frameworks/$lib"
  done
done
# PDFium is dlopen'ed from Contents/Frameworks (crates/oc-pdf/src/library.rs).
cp "$PDFIUM/libpdfium.dylib" "$APP/Contents/Frameworks/libpdfium.dylib"
chmod u+w "$APP"/Contents/Frameworks/*.dylib
install_name_tool -id @rpath/libpdfium.dylib "$APP/Contents/Frameworks/libpdfium.dylib" 2>/dev/null

# Licences: ours and the repository's third-party texts (UnRAR, PDFium, the
# Rust crates' THIRD-PARTY-RUST.txt from scripts/third-party-licenses.sh), with
# licenses/ffmpeg/ (BtbN's Windows/Linux build) replaced by this build's own
# texts and exact source.
lic="$APP/Contents/Resources/licenses"
cp "$ROOT/LICENSE" "$lic/LICENSE.txt"
cp -R "$ROOT/licenses/." "$lic/"
cp "$PDFIUM/LICENSE" "$lic/pdfium/LICENSE"
rm -rf "$lic/ffmpeg"
mkdir -p "$lic/ffmpeg"
cp "$FFMPEG/licenses/"* "$lic/ffmpeg/"

# --- checks ------------------------------------------------------------------------

step "Check linkage and deployment target"
min_major="${MACOSX_DEPLOYMENT_TARGET%%.*}"
bad=0
while IFS= read -r f; do
  if otool -L "$f" | tail -n +2 | grep -Eq '/opt/homebrew|/usr/local|/Users/'; then
    echo "$f references a non-system path:" >&2
    otool -L "$f" | tail -n +2 | grep -E '/opt/homebrew|/usr/local|/Users/' >&2
    bad=1
  fi
  minos="$(otool -l "$f" | awk '/LC_BUILD_VERSION/ { b = 1 } b && $1 == "minos" { print $2; exit }')"
  if [[ -z "$minos" || "${minos%%.*}" -gt "$min_major" ]]; then
    echo "$f: needs macOS ${minos:-?} (> $MACOSX_DEPLOYMENT_TARGET)" >&2
    bad=1
  fi
  printf '  %-28s macOS %-5s %s\n' "$(basename "$f")" "$minos" "$(lipo -archs "$f")"
done < <(find "$APP/Contents" -type f \( -name '*.dylib' -o -path '*/MacOS/*' \))
[[ $bad -eq 0 ]] || exit 1

# --- sign ----------------------------------------------------------------------------

step "Sign"
sign() { codesign --force --sign "$identity" --options runtime ${timestamp} "$@"; }
timestamp="--timestamp"
[[ "$identity" == "-" ]] && timestamp="--timestamp=none"
for f in "$APP"/Contents/Frameworks/*.dylib; do
  out="$(sign "$f" 2>&1)" || { echo "$out" >&2; exit 1; } # quiet: "replacing existing signature"
done
sign --entitlements "$HERE/entitlements.plist" "$APP"
codesign --verify --deep --strict "$APP"

step "Self-test of the bundle"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/oc-selftest.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT
ditto "$APP" "$scratch/$APP_NAME.app"
ORCHARD_COMMANDER_CONFIG_DIR="$scratch/config" "$scratch/$APP_NAME.app/Contents/MacOS/$EXE" --self-test

# --- dmg -------------------------------------------------------------------------------

step "Disk image $DMG"
staging="$scratch/dmg"
mkdir -p "$staging"
ditto "$APP" "$staging/$APP_NAME.app" # keeps signatures, symlinks, attributes
ln -s /Applications "$staging/Applications"
rm -f "$DMG"
hdiutil create -volname "$APP_NAME" -srcfolder "$staging" -ov -format UDZO -fs HFS+ "$DMG" >/dev/null
if [[ "$identity" != "-" ]]; then
  codesign --sign "$identity" --timestamp "$DMG"
fi

if [[ $notarize -eq 1 ]]; then
  step "Notarize"
  out="$(xcrun notarytool submit "$DMG" --keychain-profile "$profile" --wait --output-format json)" || true
  status="$(python3 -c 'import json, sys; print(json.loads(sys.argv[1]).get("status", ""))' "$out" 2>/dev/null || true)"
  id="$(python3 -c 'import json, sys; print(json.loads(sys.argv[1]).get("id", ""))' "$out" 2>/dev/null || true)"
  echo "Notarization: ${status:-failed} ${id}"
  if [[ "$status" != "Accepted" ]]; then
    echo "$out" >&2
    [[ -n "$id" ]] && xcrun notarytool log "$id" --keychain-profile "$profile" >&2
    exit 1
  fi
  xcrun stapler staple "$DMG"
  xcrun stapler validate "$DMG"
  spctl --assess --type open --context context:primary-signature -v "$DMG"
fi

echo
echo "App:        $APP"
echo "Disk image: $DMG ($(du -h "$DMG" | cut -f1 | tr -d ' '))"
