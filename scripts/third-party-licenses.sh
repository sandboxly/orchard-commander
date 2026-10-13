#!/usr/bin/env bash
# Regenerate licenses/THIRD-PARTY-RUST.txt: the license texts of every Rust
# crate in the release build (from Cargo.lock, for the Windows, Linux and
# macOS targets; no dev or build-only dependencies). Needs cargo-about
# (`cargo install cargo-about --locked` or `brew install cargo-about`).
#
#   scripts/third-party-licenses.sh            # rewrite the committed file
#   scripts/third-party-licenses.sh --check    # fail if it is out of date
#
# Run it after changing dependencies and commit the result; the packaging CI
# job regenerates it anyway, so packages always match their Cargo.lock.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/licenses/THIRD-PARTY-RUST.txt"

generate() {
  cargo about generate --locked --manifest-path "$ROOT/app/Cargo.toml" -c "$ROOT/about.toml" \
    "$ROOT/packaging/licenses/about.hbs" -o "$1"
}

if [[ "${1:-}" == "--check" ]]; then
  tmp="$(mktemp)"
  trap 'rm -f "$tmp"' EXIT
  generate "$tmp"
  if ! cmp -s "$tmp" "$OUT"; then
    echo "licenses/THIRD-PARTY-RUST.txt is out of date: run scripts/third-party-licenses.sh" >&2
    diff -u "$OUT" "$tmp" | head -40 >&2 || true
    exit 1
  fi
  echo "licenses/THIRD-PARTY-RUST.txt is up to date"
else
  generate "$OUT"
  echo "wrote $OUT"
fi
