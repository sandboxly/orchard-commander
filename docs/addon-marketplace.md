# Add-ons and an add-on marketplace

Status: **proposal**. Nothing here is built yet except the extension points it
builds on (pluggable file systems and previewers). Open decisions are marked
**Decision**.

## Goal

Let other people write and publish add-ons — new file systems (S3, WebDAV,
FTP, cloud drives), previewers (PDF, 3D models, fonts), commands and themes —
and let users find, install and update them from inside Orchard Commander.

## What exists today

| Extension point | Interface | Discovery |
| --- | --- | --- |
| Previewers | `preview/api.py`: `Previewer`, `PreviewView` | built-ins + `orchard_commander.previewers` entry point |
| File systems | `vfs.FileSystem`; connection types via `FileSystemProvider` | built-ins + `orchard_commander.filesystems` entry point |
| Archive formats | `archives.Archive` base class | not pluggable yet |
| Color schemes | `themes.ColorScheme` (data only; user TOML files in `color-schemes/`) | not pluggable yet |
| Commands | `MainWindow` command list | not pluggable yet |

Previewers and file systems are already decoupled: previewers get bytes,
streams or paths only through the generic `FileSystem` interface, never from
a concrete file system.

## Problems a marketplace adds

1. **Getting add-ons into the packaged app.** Python entry points need
   `pip install`, which doesn't exist inside the frozen `Orchard Commander.app`.
2. **Native dependencies.** An add-on with compiled libraries must match the
   app's Python version and platform.
3. **Trust.** A Python add-on runs with the user's full permissions: files,
   network, and the server passwords the app may read from the keychain.
4. **Compatibility.** Add-ons importing internals break with every refactor.

## Design

### Add-on package and manifest

An add-on is a folder (or a zip of one) with a manifest and its Python code,
including any pure-Python dependencies:

```toml
# addon.toml
id = "io.github.someone.pdf"
name = "PDF preview"
version = "1.2.0"
api = ">=1.0, <2"                       # add-on API versions it supports
platforms = ["any"]                     # or e.g. ["macos-arm64", "windows-x64"]
permissions = ["read-previewed-file"]   # shown at install time

[provides]
previewers = ["orchard_pdf:PdfPreviewer"]
filesystems = []                        # "module:Provider" entries
commands = []
themes = []                             # data files, e.g. "themes/solarized.toml"
```

Installed add-ons live in the config directory
(`~/Library/Application Support/Orchard Commander/addons/<id>/` on macOS). At
startup the loader reads the manifests, skips incompatible or disabled
add-ons, adds each add-on's folder to the import path and registers what it
provides. A broken add-on is disabled with a notice and never blocks startup.
Entry points keep working for development from source.

Add-ons with native code are built per platform (like Python wheels) and
list their platforms; pure-Python add-ons use `"any"`.

### A stable, versioned add-on API

Add-ons import only from a public facade, `orchard_commander.addon`, which
re-exports what they may use (previewer API, `FileSystem`, `FileSystemProvider`,
`Entry`, contexts, …). It has its own semantic version: 1.x stays compatible.
The loader refuses add-ons whose `api` range doesn't include the running
version, with a clear message. Everything else stays private.

### Trust

| Approach | Protects against | Cost |
| --- | --- | --- |
| Curated and signed: review before listing, signed packages, only the official index unless "allow unverified" is on | Tampering, casual malware (relies on review) | Low |
| Out-of-process: each add-on in a helper process with only its declared permissions (OS sandbox), talking to the app over a pipe; the app renders everything | A buggy or malicious add-on reaches only what it was granted; crashes don't take down the app | High: message-based APIs, previewers return data instead of widgets |
| WebAssembly (e.g. Extism/wasmtime) | Strongest isolation; add-ons in many languages | Highest: new runtime, heavy libraries harder to use |

**Proposal:** start curated and signed, with permissions declared in the
manifest and shown at install time. Keep APIs shaped so they can move out of
process later — previewers already separate `load()` (pure data, worker
thread) from the view, which is the boundary an out-of-process design needs.

**Decision:** curated + signed first, or sandboxing from the start?

macOS note: the app is signed with the hardened runtime, which only loads
native code signed by the same team. Add-ons with compiled libraries would
need the "disable library validation" entitlement or re-signing during review.
Pure-Python add-ons are unaffected, so they come first.

### Marketplace

- **Index:** a Git repository with a JSON catalogue (id, versions, platforms,
  download URL, SHA-256, signature), published as a static site. No server.
- **Publishing:** a template repository with CI that builds packages; authors
  open a pull request to the index; CI runs the contract test suites
  (previewers, file systems) against the add-on; a maintainer reviews and merges.
- **Signing:** packages signed by the index (e.g. minisign or Sigstore); the
  app verifies signature and checksum before installing.
- **In the app:** an "Add-ons…" screen (command palette) to browse, install,
  update, enable/disable and uninstall; only compatible versions for the
  running app and platform are offered; background update checks.
- **Developer tools:** `orchard-commander addon new | test | pack`, the
  contract tests as a pytest plugin, and `--addon-dev <folder>` to load an
  add-on from source.

### Further extension points

- **Themes** — data only, no trust issue: the ideal first marketplace category.
- **Commands** — command palette entries with optional shortcuts.
- **Archive formats** — `archives.Archive` is a base class, but not pluggable yet.
- Later: panel columns (git status, image size), "Open with…" actions.

## Plan

1. **Foundation** — discoverable file systems (`FileSystemProvider` registry,
   SFTP and SMB moved onto it) and a file system contract test suite *(done)*;
   then the public `orchard_commander.addon` facade, the manifest format, and
   the add-on folder loader (also in the packaged app).
2. **Themes as add-ons** — prove packaging, install, update and uninstall
   with zero security risk.
3. **Signed index and in-app browser** — with the CI template and review
   process; opened to previewers, file systems and commands.
4. **Optional isolated tier** for unreviewed add-ons, if there is demand.
