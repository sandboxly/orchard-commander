# Requirements for the Rust version to ship as Orchard Commander 2.0.0

Source: the ocmd-python session (maintainer of the Python 1.x version and its packaging), 2026-10-13.
The Rust version replaces the Python one in github.com/sandboxly/orchard-commander (Python history kept
under a tag + `python-1.x` branch). Tell ocmd-python when close to ready; it does the repo switch, CI/packaging and release.

## Release / identity (must match 1.x so upgrades happen in place)
- Assets: Orchard-Commander-macOS.dmg (Developer ID signed + notarized, macOS 14+ arm64),
  Orchard-Commander-Setup.exe (Inno Setup), Orchard-Commander-Windows.zip,
  Orchard-Commander-Linux.tar.gz, Orchard-Commander-Linux-x86_64.{deb,rpm,AppImage}
- macOS bundle id `io.github.orchard-commander`; app name "Orchard Commander"; command `orchard-commander`
- Windows Inno AppId {46BEE22F-5C1E-5898-AFC7-E42B017F6AAB}, `{autopf}\Orchard Commander`, PrivilegesRequired=lowest with /CURRENTUSER /ALLUSERS override
- Linux package `orchard-commander`: /opt/orchard-commander, /usr/bin/orchard-commander, /usr/share/applications/orchard-commander.desktop, 256x256 hicolor icon
- Package IDs: brew cask sandboxly/tap/orchard-commander, winget Sandboxly.OrchardCommander, choco orchard-commander, AUR orchard-commander-bin (planned)
- `--self-test` flag: exit 0 if the bundle works (CI runs it after installing)
- Reuse from ../fcmd: packaging/windows/orchard-commander.iss, packaging/linux/nfpm.yaml + .desktop,
  packaging/manifests.py, packaging/icons (+ make_icons.py for the macOS 26 icon shape), .github/workflows/ci.yml + packages.yml, site/, docs/manual

## User data: upgrading must lose nothing
- Config dir = platformdirs user_config_dir(appauthor=False): "Orchard Commander" on macOS/Windows, "orchard-commander" on Linux
  (~/Library/Application Support/Orchard Commander/, %APPDATA%\Orchard Commander\, ~/.config/orchard-commander/)
- Files: settings.json, bookmarks.json, connections.json, known_hosts (SFTP), color-schemes/*.toml
- Settings: unset = ""/None; update() writes only changed keys (several instances). Settings.extra holds "preview.<id>.<key>" (media mute, autoplay)
- Keychain: service "Orchard Commander", username = connection account = provider format_url with path "" — exact strings
- Legacy "fcmd" migration may be dropped in 2.0

## Feature parity with 1.1.1
Two panels with the classic keys, marking, hidden files, reload keeping cursor, F5/F6/F7/F8, File-exists dialog O/A/S/K;
previews (F3 / Shift+F3 View as): listing, image, PDF, audio/video + waveform, markdown, highlighted text, hex;
media: remembered mute, autoplay (player + global), play next on end (configurable);
archives as folders, nested, also on servers (RAR: 1.x uses 7z; Rust uses the vendored unrar crate);
SFTP/SMB/WebDAV/S3 (connections.json + keychain); Find Files virtual folder (acts on real files, ".." to search root);
Multi-Rename with masks + [=field] metadata (MP3 tags, video resolution, EXIF) + undo; bookmarks (⌘D, ⌥⌘D);
terminal panel (session per side, PowerShell on Windows, mouse reporting, two-way folder linking via OSC 7/process cwd,
cd only at prompt, Nerd Font/powerline glyphs);
F4 editor (Shift+F4 new file, keeps encoding/BOM/line endings, temp+atomic replace keeping permissions, servers too,
archives read-only, reload on outside change, find/replace, go to line ⌘L/⌥⌘L, undo/redo, own File/Edit menu on macOS);
themes (built-in schemes incl. gruvbox + user TOML, font, line spacing, frame style, live settings);
Menus Left/Files/Commands/Options/Right (native on macOS + in-window strip via F9); command palette ⌘P; zoom.
Full key list: ../fcmd/docs/manual/13-keyboard-reference.md

## Gotchas to carry over
- Cloud placeholders: reads may block forever → dedicated thread + bounded wait, never the shared pool; previews > 25 MB need a click
- SMB: serialize requests per connection (credits), open files with shared access
- SFTP: with a password, don't offer agent keys (MaxAuthTries)
- S3: folders are prefixes, keep "folder/" marker when removing/renaming the last item; us-east-1 200 on re-create; <Error> inside 200 for copy/multipart complete
- macOS: Local Network privacy blocks unsigned/ad-hoc binaries from LAN; ad-hoc signing forgets Local Network + Keychain grants each rebuild
- PDFium is not thread-safe; audio decode needs a stall watchdog; start the shell once at its final size (zsh reprints on early resize)

## Packaging notes (ocmd-python, follow-up)
- UnRAR: vendored source is under RARLAB's UnRAR license (freeware, not OSS; forbids recreating RAR compression).
  Keep the crate's license file, ship the UnRAR license text with the app (about/licenses page; /usr/share/doc/orchard-commander/ on Linux)
  next to our MIT LICENSE. Keep RAR support behind a cargo feature (default on) so builds without it are possible.
- macOS minimum: 1.x needs macOS 14 only because of PySide6. Determine the Rust build's real minimum (MACOSX_DEPLOYMENT_TARGET / wgpu Metal
  requirements) and report it to ocmd-python at switch time so the cask's `depends_on macos: :sonoma`, site and manual can be relaxed.
