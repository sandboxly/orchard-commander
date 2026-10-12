# Orchard Commander

A modern, cross-platform (Windows, macOS, Linux) two-panel file manager in the
spirit of Norton Commander and Total Commander. It's a desktop application
built on Qt (PySide6) that looks like a terminal app — monospace grid, box
borders, function-key bar — so it can show real images and play video inline.

## Run

```sh
uv run orchard-commander [LEFT_DIR] [RIGHT_DIR]
```

## Panels and preview

Each side has two tabs: **Files** and **Preview**. A side in Preview mode shows
the file under the cursor in the *other* side's file list, updating as you
move. F3 toggles this "quick view" for the side you're not working in. Only
one side can preview at a time.

| File type | Preview |
| --- | --- |
| Source code / text | Syntax-highlighted (Pygments), first 1 MB |
| Markdown | Rendered |
| Images (png, jpg, gif, webp, svg, tiff, heic on macOS, …) | Full resolution, scaled to fit; animated GIF/WebP play. Right-click: Open (default app) or Copy (the full-size image) |
| Video (mp4, mov, mkv, webm, avi, …) | Space play/pause, ←/→ seek, M mute, A autoplay |
| Audio (mp3, flac, wav, ogg, …) | Title and artist, a live spectrum analyzer while playing, and the waveform of the whole file, which you can click or drag to seek — Space play/pause, ←/→ seek, M mute, A autoplay. On servers, files up to 100 MB get a waveform (it reads the whole file) |
| PDF | Rendered pages, fitted to the pane — ←/→, PgUp/PgDn, Space or the scroll wheel turn pages; Home/End |
| Directories and archives | Contents listing, like a folder |
| Other binary | File info and hex dump |

When an audio or video file ends, the next one in the panel is selected and
plays (Settings → Preview; switched off, videos loop instead). The player
remembers whether it was muted — it starts muted until you unmute it once —
and its **[x] Autoplay** toggle (key A, or Settings → Preview): on, files
start playing as soon as they're selected; off, Space starts them.

Video and audio decoding uses Qt Multimedia's bundled FFmpeg backend, so the
same formats work on every OS.

When several previewers fit a file, **View as** chips at the top of the preview
switch between them — e.g. Markdown rendered or as source, any file as hex.
Shift+F3 cycles them; the choice is remembered per file extension for the
session. Files on servers and in archives are read only as far as a
previewer needs: the beginning for text and hex, a stream for media and PDFs
(PDFium fetches only the parts of a PDF it needs), the whole file only for
images and archives (up to 512 MB).

Cloud folders (Dropbox, iCloud Drive, OneDrive) often hold **online-only**
placeholders; reading one makes the OS download the whole file first. The file
list marks them with ☁ next to the size. They preview automatically up to
25 MB ("downloading from the cloud…"); bigger ones show a **Download and
preview** button instead of starting a large download as the cursor passes.

## Archives

zip, tar (plain, .gz, .bz2, .xz), 7z and rar archives — and single
compressed files (`.gz`, `.bz2`, `.xz`) — behave like read-only folders:
Enter goes inside (also into archives nested in archives), Backspace or `..`
comes back out, files inside preview and open like any other file, and F5
extracts the marked entries to the other panel. Moving, deleting and creating
inside archives isn't supported yet.

RAR listing is built in; extracting RAR data uses `unrar`, 7-Zip or `bsdtar`,
whichever is installed. `bsdtar` ships with macOS and (as `tar.exe`) with
Windows 10+, and handles RAR5 and non-solid RAR4; solid RAR4 archives need
`unrar` or 7-Zip.

## Servers (SFTP, SMB, WebDAV and S3)

Press **Ctrl+K** (⌘K on macOS) to manage saved connections, or type an
address into **Go to** (Ctrl+G):

- `sftp://user@host[:port]/absolute/path` or `sftp://user@host/~/in/home`
- `smb://user@server[:port]/share/folder` (Windows shares, Samba, NAS boxes)
- `webdavs://user@host[:port]/server/path` (HTTPS) or `webdav://…` (plain HTTP):
  Nextcloud, ownCloud, NAS boxes, Apache/nginx WebDAV shares. For Nextcloud
  the server path is `/remote.php/dav/files/USER`.
- `s3://access-key@server[:port]/bucket/folder` (HTTPS) or `s3+http://…`
  (plain HTTP): Amazon S3 (server `s3.amazonaws.com`, or just
  `s3://bucket/folder` as the AWS CLI writes it) and S3-compatible storage —
  MinIO, Cloudflare R2, Backblaze B2, Wasabi, Hetzner, DigitalOcean Spaces,
  Ceph, Garage. Add `?region=…` where the server needs one it can't tell.

Saved connections also appear in the drive picker (Alt+F1 / Alt+F2). A
server panel works like any other: browse, preview, copy/move in both
directions (moves within one server are server-side renames), make
directories, open files, and browse archives stored on the server. Servers
have no trash, so F8 deletes permanently — after a confirmation that says so.
Ctrl+Shift+K disconnects the active panel's server.

- **SFTP** authenticates with the password if one is given (or a key file and
  its passphrase), otherwise with ssh-agent and your default SSH keys. Unknown
  host keys are shown with their SHA256 fingerprint before anything is sent;
  trusted keys are stored in Orchard Commander's own `known_hosts` (your
  `~/.ssh/known_hosts` is read but never written), and a changed key blocks the
  connection.
- **SMB** speaks SMB 2/3 with NTLM/Kerberos (via `smbprotocol`), so it needs no
  OS mounts. Connections name the share; share discovery isn't supported.
- **WebDAV** uses Basic or Digest authentication (for Nextcloud, an app
  password is a good idea). Certificates are checked against the operating
  system's trust store. A certificate the system doesn't trust (e.g. a NAS
  with a self-made one) is shown with its SHA-256 fingerprint and can be
  trusted once; a changed certificate then blocks the connection, as with SSH
  host keys. Files are read with range requests, so media and archives stream
  from the server; uploads are sent when complete.
- **S3** signs requests with the access key ID and secret key (AWS Signature
  Version 4); without a key, public buckets can be read. The top level lists
  the buckets (F7 creates one, F8 deletes one with everything in it), below
  them folders are key prefixes, as in the AWS console. A folder that loses its
  last file stays (as an empty `folder/` marker object, which is also what F7
  creates). On Amazon, each bucket's region is found out automatically. Files
  stream with range requests; uploads go up in parts while they're written,
  so large files need no temporary copy and a cancelled copy leaves nothing
  behind. Renames and moves within a server are server-side copies plus a
  delete (S3 can't rename), and keep the upload date: S3 has no settable
  modification times. Certificates are handled as for WebDAV.
- **Passwords** are stored only if you tick "Remember", and then in the OS
  credential store (macOS Keychain, Windows Credential Manager, Secret
  Service on Linux) — never in Orchard Commander's files.

All file-system work, including directory listing, runs off the UI thread,
so a slow server never freezes the window.

## Bookmarks

Like Total Commander's Directory Hotlist. **Ctrl+D** (⌘D), or a click on a
panel's path, opens the bookmark menu at the panel:

- Each entry has a hotkey: an `&` in its title marks the letter (“&Projects” →
  P), shown in the menu's first column. If several entries share a letter,
  pressing it moves between them.
- A bookmark can also remember the **other panel's folder** and open both at
  once (Total Commander's “target” path).
- Bookmarks can point anywhere a panel can go: local folders, folders inside
  archives, and folders on servers (reconnecting with the saved
  connection and its keychain password).
- **+** adds the current folder (choose a title, a folder of bookmarks, and
  whether to include the other panel), **−** removes it if it's bookmarked,
  **\*** opens the organizer: drag and drop to reorder or move into folders,
  plus folders, separators, editing and deleting.
- **Ctrl+Alt+D** (⌥⌘D) searches all bookmarks as you type; bookmarks also appear
  in the drive picker (Alt+F1 / Alt+F2).

Bookmarks are saved in `bookmarks.json` in the config directory.

## Find files

Like Total Commander's Find Files (Alt+F7) with "Feed to listbox" built in.
**Alt+F7** or **⌘F** / Ctrl+F opens the search:

- **Names**: masks separated by spaces or `;` (`*.pdf;*.docx`), a mask
  without wildcards matches anywhere in the name (`report`), and one `|`
  separates what to exclude (`*.zip|*old*`). Or a regular expression;
  case-sensitive if you want.
- **Where**: the active panel's folder (also on servers and inside archives),
  or several local folders separated by `;`; all subfolders or a maximum
  depth; files, folders or both; hidden files; optionally inside archives.
- **Contents**: text or a regular expression, case-sensitive, whole words, or
  files *not* containing it; the matching line is kept. Cloud-only files
  (Dropbox & co.) are skipped for content searches unless you allow
  downloading them.
- **Size and date**: at least / at most (`10M`, `1.5G`), modified within or
  before a period.

Results appear in the panel **as a folder while the search runs**, each hit
named by its path below the search folder. Everything acts on the real
files: preview, open, F5/F6 (mark all with `+` and move every hit to one
place), F8; hits that were moved or deleted drop out of the list. Enter on
a folder hit opens the real folder; `..` goes back to where you searched;
**⌘⇧F** shows the last results again; **Esc** stops a running search.

## Multi-rename

Like Total Commander's Multi-Rename Tool: mark files (also search hits) and
press **⌘M** / Ctrl+M. The new name is built from masks with TC's
placeholders — pick them from the **Insert…** menu:

| Placeholder | Meaning |
|---|---|
| `[N]` `[E]` | name without extension, extension |
| `[N2-5]` `[N2,5]` `[N3-]` `[N-5-]` `[N-8-5]` `[N2--5]` | characters 2–5, 5 from the 2nd, from the 3rd, the last 5, 8th-last to 5th-last, 2nd to 5th-last |
| `[1-5]` | characters of the whole name with extension |
| `[P]` `[G]` | parent / grandparent folder |
| `[C]` `[C10+5:3]` | counter (start, step, digits from the dialog or inline) |
| `[Y]` `[M]` `[D]` `[h]` `[m]` `[s]` `[d]` `[t]` | modification date and time |
| `[U]` `[L]` `[F]` `[n]` | upper / lower / Title Case / original case from here on |
| `[[]` `[]]` | literal brackets |
| `[=artist]` `[=artist:1-3]` `[=artist\|Unknown]` | file metadata, a range of it, a fallback when the file has none |

Metadata fields (Insert… lists them per kind of file):
**audio tags** `artist` `albumartist` `album` `title` `track` (03) `tracktotal`
`disc` `year` `genre` `composer` `duration` `bitrate`; **video and images**
`width` `height` (vertical resolution) `resolution` (1920x1080) `quality`
(1080p) `duration` `fps` `title`; **photos (EXIF)** `taken` (2026-03-07)
`taken_time` `taken_year` `taken_month` `camera` `make` `model` `lens` `iso`
`focal`. Metadata is read in the background; files without a value are
skipped (shown in the preview) unless the placeholder has a fallback. Values
are made safe for names ("AC/DC" → "AC-DC"). Add-ons can provide more fields
through the `orchard_commander.metadata` entry point.

Then **search & replace** (several pairs with `|`, RegEx with `$1` groups)
and an optional case conversion. The preview shows every new name and
**refuses to rename while anything is wrong**: duplicate names, names that
already exist, invalid names, or a broken mask (with the reason). Swaps
(`a→b`, `b→a`) and case-only changes are done safely through temporary names.
**⌘Z** / Ctrl+Z undoes the last batch. Renamed search hits stay in the results.

## Terminal

**⌃\`** (the Control key, on every platform; on macOS also **⌘J**, which
works with keyboard layouts where \` is a dead key) opens a terminal below both
panels, running your shell — your login shell, PowerShell on Windows, or
what you set in Settings → Terminal. Each side has its own shell: switching
the active side switches the terminal to that side's session, and the other
keeps running.

A side's shell and panel are **linked**: `cd` in the terminal takes the panel
along, and navigating the panel types a `cd` into the shell — only when the
shell is idle at its prompt (never into vim or a running build; it catches
up when the program ends). Servers, archives and search results leave the
shell where it is. After every command the panels refresh, so `rm`,
`touch` or `echo >> file` show up right away. Settings → Terminal can unlink
them; **⌃⇧\`** (Commands → Terminal: go to panel folder) still moves the shell
to the panel's folder then.

While the terminal has the focus it gets every key — Esc, Tab, F-keys,
Ctrl+C — except ⌃\` / ⌘J (hide it again) and, on macOS, ⌘ shortcuts: ⌘C / ⌘V
copy and paste (Ctrl+Shift+C / V elsewhere). Full-screen programs (vim,
less, htop, tmux, mc) work, including the mouse: programs that ask for it
get clicks, drags, motion and the wheel (xterm protocol, SGR and classic
encodings) and focus changes. Otherwise the wheel scrolls back through the
output, drag selects, double-click selects a word (or path), triple-click a
line; hold Shift to select while a program has the mouse. When the shell
exits, Enter starts a new one.

The terminal uses your theme's font (ligatures included, e.g. Fira Code).
Icons from prompt themes like powerlevel10k, starship or oh-my-posh come
from an installed Nerd Font (e.g. MesloLGS NF, FiraCode Nerd Font) when the
theme font doesn't have them; powerline separators, block elements and box
lines are drawn to fill their cells exactly, whatever the font.

## Keys

| Key | Action |
| --- | --- |
| ↑ ↓ PgUp PgDn Home End | Move cursor |
| Type letters | Jump to matching file |
| Enter / double-click | Enter directory or archive / open file with the default application |
| Shift+Enter | Open with the default application (archives, and folders in the OS file manager) |
| Backspace | Parent directory |
| Tab | Switch side |
| Insert / Space, Ctrl-click, right-click | Mark file |
| `+` / `-` / `*` | Mark all / unmark all / invert marks |
| Ctrl+A / Ctrl+Alt+A (⌘A / ⌥⌘A) | Select (mark) all / clear the selection — also in the Files menu |
| Home / End (fn+← / fn+→ on Mac keyboards) | First (`..`) / last entry |
| F3 | Toggle preview in the other side |
| Shift+F3 | Next "View as" for the previewed file |
| F5 | Copy (to the other panel; edit the target to copy under a new name); extracts from archives |
| F6 | Move / rename |
| F7 | Make directory |
| F8 / Delete / Ctrl+Backspace | Move to trash / recycle bin |
| F9 | Menu bar |
| Ctrl+, | Settings |
| F10 / Ctrl+Q | Quit |
| Ctrl+P | Command palette — every command, searchable |
| Ctrl+R | Reload |
| Ctrl+Shift+. (also Ctrl+H on Windows/Linux) | Toggle hidden files |
| Ctrl+U | Swap the left and right folders |
| Ctrl+E | Show the active folder in the other panel too (TC's "Target = Source") |
| Ctrl+← / Ctrl+→ | Left / right panel: open the folder (or archive) under the cursor there — or, pressed in that panel itself, match the other panel (TC's cm_TransferLeft / cm_TransferRight) |
| Ctrl+G | Go to path or server address (sftp://, smb://, webdavs://, …) |
| Ctrl+D / Ctrl+Alt+D (⌥⌘D) | Bookmark menu (also: click a panel's path) / search bookmarks |
| Control+\` / Control+Shift+\` | Terminal (show, focus; inside it: hide) / go to the panel's folder |
| Ctrl+M / Ctrl+Z | Multi-rename marked files / undo the last multi-rename |
| Alt+F7 or Ctrl+F / Ctrl+Shift+F / Esc | Find files / show the last search results / stop the search |
| Ctrl+K / Ctrl+Shift+K | Connect to a server / disconnect |
| Alt+F1 / Alt+F2 | Choose drive / volume for left / right panel |
| Ctrl+= / Ctrl+- / Ctrl+0 | Zoom in / out / reset |

On macOS, Ctrl means ⌘, and the F-keys may need `fn`. Click a column header to
sort by it; click again to reverse.

## Appearance

Settings (Options → Settings…, ⌘, on macOS, Ctrl+, elsewhere) apply as you
change them. Appearance is made of:

- **Color scheme**: `monokai` (default), `dracula`, `nord`, `gruvbox`,
  `tokyo-night`, `commander` (classic Norton blue), `solarized-light` and
  `paper`, or your own. Optionally one scheme for light and one for dark mode,
  following the system. "Edit…" changes a scheme's colors with a live preview;
  a built-in scheme is saved as a copy. Your schemes are TOML files in the config
  directory's `color-schemes/` folder; colors a file leaves out come from a
  built-in scheme.
- **Font**: typeface (monospace fonts by default), style, size (also Ctrl+= /
  Ctrl+- / Ctrl+0) and line spacing.
- **Frames**: rounded, single line, double line or heavy.

The menu bar (Left, Files, Commands, Options, Right, as in Norton Commander) is
the native one on macOS. In the window it's hidden unless "Always show the menu
bar" is on; F9 shows it. Settings, last directories, panel modes and window
geometry are saved in the platform config directory (`settings.json`).

## Building the app

```sh
uv run python packaging/build.py
```

PyInstaller produces `dist/Orchard Commander.app` on macOS (about 140 MB, mostly Qt and
FFmpeg) and `dist/orchard-commander/` with the `orchard-commander` executable on Linux
and Windows; build on each platform for that platform. Check a build with
`"dist/Orchard Commander.app/Contents/MacOS/orchard-commander" --self-test`
(or `dist/orchard-commander/orchard-commander --self-test`),
which loads everything the app imports on demand.

On macOS the app bundle matters beyond convenience: macOS only grants *Local
Network* access (needed for servers on your LAN) to programs with a
proper identity, and asks for it using the `NSLocalNetworkUsageDescription`
set in `packaging/orchard-commander.spec`. `uv run orchard-commander` runs on a bare Python binary,
which macOS may silently block from the local network ("No route to host").
macOS ties those permissions (and Keychain access to saved passwords) to the
app's code signature, so the build signs with a stable identity: the
`FCMD_CODESIGN_IDENTITY` environment variable or `--sign IDENTITY`, otherwise
the first "Developer ID Application" (then "Apple Development") certificate in
your keychain, with the hardened runtime and `packaging/entitlements.plist`.
Without a certificate it falls back to ad-hoc signing (or use `--adhoc`), and
macOS then treats every build as a new app and asks again. Distributing the
app additionally requires notarization (`xcrun notarytool`).

Regenerate the app icons (macOS `.icns`, Windows `.ico`, window icon) from
`packaging/icons/source.png` with `uv run python packaging/make_icons.py`.

## Development

```sh
uv run pytest
```

The SFTP tests run a real SSH server (asyncssh), the SMB tests impacket's
SMB2 server, the WebDAV tests WsgiDAV (Basic, Digest, and HTTPS with a
self-signed certificate) and the S3 tests moto (request signatures are checked
against botocore and AWS's documented examples), all on localhost. Full SMB read/write tests run against Samba in
Docker and are skipped when Docker isn't available.

| Module | Role |
| --- | --- |
| `vfs.py` | Virtual file system: `FileSystem` interface, `VPath`, `LocalFS`, `ArchiveFS`, copy/move/trash across file systems |
| `fs.py` | Native local-disk operations: listing, copy, move, trash, open (used by `LocalFS`) |
| `archives.py` | Archive format readers (zip/tar/7z/rar/gz/bz2/xz) and the extraction cache (used by `ArchiveFS`) |
| `fsproviders.py` | Connection types: `FileSystemProvider` interface, registry, entry-point discovery |
| `remote/sftp.py`, `remote/smb.py`, `remote/webdav.py`, `remote/s3.py` | SFTP (paramiko), SMB (smbprotocol), WebDAV and S3 (standard library HTTP) file systems |
| `remote/httpclient.py` | HTTP(S) shared by WebDAV and S3: per-thread keep-alive connections, system trust store, pinned certificates, ranged reads |
| `remote/providers.py` | The built-in SFTP, SMB, WebDAV and S3 connection types |
| `connections.py` | Provider-agnostic connections, saved connections, keychain passwords, live sessions |
| `bookmarks.py`, `locations.py` | Bookmark tree and storage; serializable location specs (local, server, archive) |
| `rename.py`, `ui/rename_ui.py` | Multi-rename: TC masks, search & replace, checks, safe two-phase renames, undo |
| `terminal.py`, `ui/terminal_view.py` | Shell sessions (pseudo-terminal + pyte screen) and the terminal panel |
| `search.py`, `ui/search_ui.py` | Find files on any file system; results as a virtual folder (`SearchResultsFS`); the dialog |
| `testing.py` | Contract checks for file system implementations (also for add-on authors) |
| `preview/api.py` | The previewer plugin interface: `Previewer`, `PreviewView`, `Access`, `Score`, … |
| `preview/registry.py` | Registration, entry-point discovery, scoring, fallback |
| `preview/source.py` | `PreviewSource`: head / stream / local file through the generic file system interface |
| `preview/builtin/` | Built-in previewers: listing, image, PDF, media, Markdown, text, hex |
| `preview/content.py` | Qt-free helpers: text detection and decoding, hex dumps, listings |
| `themes.py`, `config.py` | Color schemes (built-in and user files), appearance, persisted settings |
| `ui/settings_dialog.py` | Settings window, color scheme editor |
| `ui/main_window.py` | Window, sides, commands and shortcuts, function-key bar |
| `ui/panel.py` | File list model, row painting, key handling |
| `ui/preview_pane.py` | Hosts previewer views, "View as" chips, background loading; no type-specific code |
| `ui/boxframe.py` | The terminal-style box border |
| `ui/dialogs.py`, `ui/operations.py` | Dialogs; file operations on a worker thread |
| `ui/connect_dialogs.py` | Connection manager, editor, password and host-key prompts |
| `ui/bookmarks_ui.py` | Bookmark menu with hotkeys, "add bookmark" dialog, organizer |
| `ui/tasks.py` | Background jobs with callbacks on the UI thread; asking the UI from a worker |

Plans for third-party add-ons and an add-on marketplace: [docs/addon-marketplace.md](docs/addon-marketplace.md).

### Adding a file system

Two parts, both discoverable like previewers:

1. **The file system** subclasses `vfs.FileSystem`: listing, `stat`, path
   handling and `open_read`; writable ones add `mkdir`, `remove`, `open_write`
   (and `rename` with `can_rename`). The UI only uses `vfs.VPath` values and
   capability flags (`writable`, `can_trash`, `watchable`), so copy/move,
   previews and browsing archives stored on it work without UI changes.
2. **The connection type** subclasses `fsproviders.FileSystemProvider`: an
   address scheme (`webdav://`), the fields of its connection dialog (the form
   is generated from them), `parse_url` / `format_url`, and `connect(params,
   secret, ctx)`, which returns the file system. It may ask the user questions
   through `ctx.confirm(...)` (that's how SFTP asks to trust a host key) and
   keep files in `ctx.data_dir`. See `remote/providers.py` for SFTP and SMB.

Register providers in `fsproviders._builtin_providers()` or through an entry
point (add-ons may override built-ins with the same scheme):

```toml
[project.entry-points."orchard_commander.filesystems"]
webdav = "orchard_webdav:WebDavProvider"
```

Saved connections store the scheme plus the provider's params, so connection
types need no changes elsewhere. Check an implementation with the contract in
`orchard_commander.testing` (`check_filesystem(root)`), which every built-in
file system passes (`tests/test_fs_contract.py`); `tests/test_vfs.py` has a
small in-memory backend and `tests/test_fsproviders.py` a matching connection
type as examples.

### Adding a previewer

A previewer is one class (see the docstring of `preview/api.py`):

- `score(FileInfo)` says how well it fits a file — by extension, size, or the
  first bytes if it sets `needs_head`. Highest score wins; `Score.FORMAT` (100)
  for "this is my format", `Score.FALLBACK` (1) for "I can show anything".
- `access` declares what it reads; `load(src, ctx)` gets it from the
  `PreviewSource` (`head(n)`, `open_stream()`, `local_file(max_size)`) on a
  worker thread and returns `Loaded(data, info)`. It never learns which file
  system the file is on — the file system provides the bytes.
- `create_view(ctx)` returns a `PreviewView` widget (created once, reused):
  `show_data(data)`, `clear()`.
- Raise `CannotPreview` to let the next previewer try (e.g. a corrupt file),
  or `ShowMessage` to show a message instead (e.g. "too large to fetch").
- `available()` returns False when an optional dependency is missing.

Register built-ins in `preview/builtin/__init__.py`; separate packages register
through an entry point and may override built-ins with the same `id`:

```toml
[project.entry-points."orchard_commander.previewers"]
pdf = "orchard_pdf:PdfPreviewer"
```

`tests/test_previewers.py` runs a contract against every registered previewer —
empty, garbage and valid files, on the local disk and a non-local file system —
so a new previewer is checked automatically.

## License

MIT — see [LICENSE](LICENSE). The app bundles Qt (PySide6) and pyte under the LGPL.
