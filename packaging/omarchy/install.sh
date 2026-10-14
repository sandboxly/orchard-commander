#!/usr/bin/env bash
# Install Orchard Commander on Omarchy (or any Arch-based system), per user,
# no root needed except for the runtime libraries:
#
#   curl -fsSL https://raw.githubusercontent.com/sandboxly/orchard-commander/main/packaging/omarchy/install.sh | bash
#   ... | bash -s -- --bind --default      # also add a key and make it the folder opener
#   ... | bash -s -- --uninstall
#
# Options
#   --bind        Super+Ctrl+Shift+F opens (or focuses) Orchard Commander
#   --free-f9     unbind F9 from Omarchy's push-to-talk dictation, so F9 reaches the
#                 app (its menu strip); dictation keeps working if you rebind it
#   --default     open folders (inode/directory) with Orchard Commander
#   --version X   install release X (for example 2.1.0) instead of the latest
#   --uninstall   remove the app, the launcher entry and the key binding
#
# The app lands in ~/.local/opt/orchard-commander and is linked as
# ~/.local/bin/orchard-commander. It ships its own FFmpeg and PDFium, so
# nothing else is installed system-wide except the libraries listed in DEPS.
set -euo pipefail

REPO="sandboxly/orchard-commander"
ASSET="Orchard-Commander-Linux.tar.gz"
PREFIX="$HOME/.local/opt/orchard-commander"
BIN="$HOME/.local/bin/orchard-commander"
APPS="$HOME/.local/share/applications"
ICONS="$HOME/.local/share/icons/hicolor/256x256/apps"
BINDINGS="$HOME/.config/hypr/bindings.lua"
MARK="-- Orchard Commander (added by its installer)"
MARK_F9="-- Orchard Commander: F9 is its menu (added by its installer)"
# Window system, keyboard, audio, fonts and GPU loader. Most are on Omarchy already.
DEPS=(wayland libxkbcommon libx11 libxcursor libxi alsa-lib fontconfig vulkan-icd-loader mesa ca-certificates)

say() { printf '\033[1m%s\033[0m\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

uninstall() {
  rm -rf "$PREFIX"
  rm -f "$BIN" "$APPS/orchard-commander.desktop" "$ICONS/orchard-commander.png"
  if [[ -f "$BINDINGS" ]] && grep -qF -- "$MARK" "$BINDINGS"; then
    # The marker line and the bind line after it.
    sed -i "\|^$MARK\$|,+1d" "$BINDINGS"
  fi
  if [[ -f "$BINDINGS" ]] && grep -qF -- "$MARK_F9" "$BINDINGS"; then
    sed -i "\|^$MARK_F9\$|,+1d" "$BINDINGS"
  fi
  if [[ "$(xdg-mime query default inode/directory 2>/dev/null)" == orchard-commander.desktop ]]; then
    xdg-mime default org.gnome.Nautilus.desktop inode/directory
  fi
  update-desktop-database "$APPS" 2>/dev/null || true
  say "Orchard Commander removed. Settings in ~/.config/orchard-commander stay."
}

install_deps() {
  local missing=()
  for d in "${DEPS[@]}"; do pacman -Qq "$d" &>/dev/null || missing+=("$d"); done
  [[ ${#missing[@]} -eq 0 ]] && return
  say "Installing libraries: ${missing[*]}"
  sudo pacman -S --needed --noconfirm "${missing[@]}"
}

install_app() {
  local version="$1" url tmp
  if [[ -n "$version" ]]; then
    url="https://github.com/$REPO/releases/download/v${version#v}/$ASSET"
  else
    url="https://github.com/$REPO/releases/latest/download/$ASSET"
  fi
  tmp="$(mktemp -d)"
  trap "rm -rf '$tmp'" EXIT
  say "Downloading Orchard Commander"
  curl -fL --progress-bar -o "$tmp/$ASSET" "$url" || die "download failed: $url (check the version and your connection)"
  tar xzf "$tmp/$ASSET" -C "$tmp"
  [[ -x "$tmp/orchard-commander/orchard-commander" ]] || die "the download is not an Orchard Commander package"

  mkdir -p "$(dirname "$PREFIX")" "$(dirname "$BIN")" "$APPS" "$ICONS"
  rm -rf "$PREFIX.new"
  mv "$tmp/orchard-commander" "$PREFIX.new"
  rm -rf "$PREFIX"
  mv "$PREFIX.new" "$PREFIX"
  ln -sf "$PREFIX/orchard-commander" "$BIN"

  curl -fsSL -o "$ICONS/orchard-commander.png" \
    "https://raw.githubusercontent.com/$REPO/main/packaging/linux/orchard-commander.png" || true
  cat >"$APPS/orchard-commander.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Orchard Commander
GenericName=File Manager
Comment=Two-panel file manager with previews, archives, servers and a terminal
Exec=$PREFIX/orchard-commander %F
Icon=orchard-commander
Terminal=false
Categories=System;FileTools;FileManager;Utility;
Keywords=files;folders;two-panel;twin panel;orthodox file manager;sftp;archive;
MimeType=inode/directory;
StartupWMClass=orchard-commander
EOF
  update-desktop-database "$APPS" 2>/dev/null || true
}

add_binding() {
  [[ -f "$BINDINGS" ]] || die "no $BINDINGS: is this Omarchy 4 or later?"
  if grep -qF -- "$MARK" "$BINDINGS"; then return; fi
  printf '\n%s\no.bind("SUPER + CTRL + SHIFT + F", "Orchard Commander", { launch = "orchard-commander", focus = "^orchard-commander$" })\n' \
    "$MARK" >>"$BINDINGS"
  hyprctl reload >/dev/null 2>&1 || true
}

free_f9() {
  [[ -f "$BINDINGS" ]] || die "no $BINDINGS: is this Omarchy 4 or later?"
  if grep -qF -- "$MARK_F9" "$BINDINGS"; then return; fi
  printf '\n%s\nhl.unbind("F9")\n' "$MARK_F9" >>"$BINDINGS"
  hyprctl reload >/dev/null 2>&1 || true
}

main() {
  local free=0 bind=0 default=0 version=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --bind) bind=1 ;;
      --free-f9) free=1 ;;
      --default) default=1 ;;
      --version) version="${2:?--version needs a number, for example 2.1.0}"; shift ;;
      --uninstall) uninstall; exit 0 ;;
      -h|--help) sed -n '2,17p' "${BASH_SOURCE[0]:-$0}" 2>/dev/null || true; exit 0 ;;
      *) die "unknown option: $1" ;;
    esac
    shift
  done

  [[ "$(uname -m)" == x86_64 ]] || die "Orchard Commander for Linux needs an x86-64 machine"
  command -v pacman >/dev/null || die "this installer is for Arch-based systems such as Omarchy"
  install_deps
  install_app "$version"
  [[ $bind -eq 1 ]] && add_binding
  [[ $free -eq 1 ]] && free_f9
  [[ $default -eq 1 ]] && xdg-mime default orchard-commander.desktop inode/directory

  say "Installed. Start it from the launcher (Super+Space) or with: orchard-commander"
  [[ $bind -eq 1 ]] && echo "Key: Super+Ctrl+Shift+F"
  [[ $default -eq 1 ]] && echo "Folders now open in Orchard Commander."
  [[ $bind -eq 0 || $default -eq 0 ]] && echo "Add --bind and --default to set up a key or the folder opener."
  case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) echo "Add ~/.local/bin to your PATH to run it from a terminal." ;; esac
}

main "$@"
