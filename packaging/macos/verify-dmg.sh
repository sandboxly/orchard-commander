#!/usr/bin/env bash
# Check a built disk image the way a user gets it:
#
#   packaging/macos/verify-dmg.sh [dist/Orchard-Commander-macOS.dmg]
#
# Mounts it read-only, copies the app out to a temporary folder, checks its
# signature (and Gatekeeper's verdict), that no Mach-O references Homebrew or
# /usr/local, runs `--self-test`, and opens the window once (OC_TIMING=exit:
# quits after the first frame). Runs use a scratch config dir
# (ORCHARD_COMMANDER_CONFIG_DIR), never the real one.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DMG="${1:-$ROOT/dist/Orchard-Commander-macOS.dmg}"
APP_NAME="Orchard Commander"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/oc-verify.XXXXXX")"
mnt="$tmp/mnt"
cleanup() {
  hdiutil detach -quiet "$mnt" 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT

echo "==> $DMG ($(du -h "$DMG" | cut -f1 | tr -d ' '))"
mkdir -p "$mnt"
hdiutil attach -quiet -readonly -nobrowse -mountpoint "$mnt" "$DMG"
ls -l "$mnt"
[[ -L "$mnt/Applications" ]] || { echo "no /Applications link" >&2; exit 1; }
ditto "$mnt/$APP_NAME.app" "$tmp/$APP_NAME.app"
hdiutil detach -quiet "$mnt"
app="$tmp/$APP_NAME.app"
exe="$app/Contents/MacOS/orchard-commander"

echo "==> signature"
codesign --verify --deep --strict "$app"
codesign -dv "$app" 2>&1 | grep -E '^(Identifier|Authority=Developer|TeamIdentifier|Runtime Version|Timestamp)|flags=' || true
spctl --assess --type execute -v "$app" 2>&1 || echo "(Gatekeeper: not accepted, e.g. ad-hoc or not notarized)"
/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' -c 'Print :LSMinimumSystemVersion' "$app/Contents/Info.plist" |
  paste -sd' ' - | sed 's/^/version, minimum macOS: /'

echo "==> linkage"
bad=0
while IFS= read -r f; do
  deps="$(otool -L "$f" | tail -n +2 | awk '{print $1}' | grep -v '^/System/Library/\|^/usr/lib/' | paste -sd' ' -)"
  printf '  %-24s %s\n' "$(basename "$f")" "${deps:-(system only)}"
  if otool -L "$f" | grep -Eq '/opt/homebrew|/usr/local'; then bad=1; fi
done < <(find "$app/Contents" -type f \( -name '*.dylib' -o -path '*/MacOS/*' \))
[[ $bad -eq 0 ]] || { echo "references to /opt/homebrew or /usr/local" >&2; exit 1; }

echo "==> --self-test"
ORCHARD_COMMANDER_CONFIG_DIR="$tmp/config" "$exe" --self-test

echo "==> launch (OC_TIMING=exit)"
ORCHARD_COMMANDER_CONFIG_DIR="$tmp/config" OC_TIMING=exit "$exe" "$tmp" "$tmp"
echo "opened and closed: ok"
