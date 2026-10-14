# Troubleshooting

## Installing and starting

### macOS says the app can't be opened

Download the app again from the official download page. Releases are signed
and notarized; if macOS still refuses to open the app, the file may be
damaged or modified. Don't bypass the warning for a file from another source.

### Windows shows "Windows protected your PC"

The Windows version isn't code-signed, so SmartScreen warns about it the
first time. Click **More info**, then **Run anyway**.

### Linux: the app doesn't start

Start `orchard-commander` (or the AppImage) from a terminal to see error
messages. The app needs a graphical desktop session and draws its window
with Vulkan or OpenGL (EGL). On computers without a GPU, such as virtual
machines, the Mesa drivers draw in software.

If the message is "can't draw into the window: no GPU adapter", install
the Mesa drivers, for example `mesa-vulkan-drivers` and `libegl-mesa0` on
Debian and Ubuntu, or `mesa-vulkan-drivers` and `mesa-libEGL` on Fedora.
To try the other graphics interface, start the app with
`WGPU_BACKEND=gl orchard-commander` or `WGPU_BACKEND=vulkan orchard-commander`.

### Linux: F9 does nothing (Omarchy)

Omarchy uses **F9** for push-to-talk dictation, so the key never reaches the
app. Free it by running the install script again with `--free-f9`:

```sh
curl -fsSL https://raw.githubusercontent.com/sandboxly/orchard-commander/main/packaging/omarchy/install.sh | bash -s -- --free-f9
```

Or add `hl.unbind("F9")` to `~/.config/hypr/bindings.lua`. Dictation then
no longer starts with F9; bind it to another key there. Click **F9** in the
function-key bar to open the menu in the meantime.

## Servers

### A server on my network can't be reached (macOS)

Symptom: connecting to a server on your local network fails with "the
server can't be reached", while servers on the internet work.

macOS requires permission for apps to connect to devices on the local
network.

1. Open **System Settings → Privacy & Security → Local Network**.
2. Turn on **Orchard Commander**.
3. Connect again.

### The connection is refused because the host key or certificate changed

Orchard Commander refuses to connect when an SFTP server's key or a server's
trusted certificate is different from the one you trusted before. This
happens when a server is reinstalled or gets a new certificate, but it can
also indicate that someone is intercepting the connection. Check with the
server's administrator before you continue.

For SFTP, the trusted keys are stored in the file `known_hosts` in the
settings folder (see [Settings](12-settings.md#where-settings-are-stored)).
To accept a new key, remove the server's line from that file and connect
again.

### I'm asked for my password every time

Select **Remember in the system keychain** when you enter the password.
On Linux, this requires a running Secret Service, such as GNOME Keyring or
KWallet.

### A SMB share isn't listed

Orchard Commander doesn't list the shares of a server. Enter the share name
in the connection or address, for example `smb://anna@nas/media`.

## Files and previews

### A file in a cloud storage folder doesn't preview

Files marked ☁ are online-only and must be downloaded first.

- For files larger than 25 MB, click **Download and preview**.
- If the preview reports that the file hasn't arrived, make sure the cloud
  application is running, signed in and online, then click **Try again**.

### A RAR or 7-Zip archive doesn't open

RAR and 7-Zip archives are read by Orchard Commander itself; no other
program needs to be installed. Password-protected (encrypted) archives
can't be opened. See [Archives](06-archives.md#supported-formats).

### A file I deleted in another program still appears

Press **⌘R** / **Ctrl+R** to reload the panels. Folders on servers and some
network drives aren't updated automatically.

## Terminal

### The terminal shortcut doesn't work

On keyboard layouts where the \` key is a dead key, such as German or
French, the **⌃\`** shortcut may not reach Orchard Commander. On macOS, use
**⌘J** instead. You can always use **Commands → Terminal**.

### The prompt shows boxes instead of icons

Your prompt theme uses Nerd Font icons. Install a Nerd Font, for example
MesloLGS NF or FiraCode Nerd Font, and restart Orchard Commander. You can
keep your current font in Settings; the icons are taken from the Nerd Font.

### Commands I type are missing my PATH settings

The terminal starts your shell as an interactive login shell, so it reads
the same startup files as a new window in your system's terminal app, for
example `~/.zprofile` and `~/.zshrc` for zsh, or `~/.bash_profile` for bash.
Make sure the setting is in one of these files. If you set a custom shell
command in **Settings → Terminal**, include the option for a login shell,
usually `-l`.

### The panel doesn't follow `cd` in the terminal

Check that **Link each side's terminal and panel folders** is selected in
**Settings → Terminal**. The panel follows only folders on local disks.

## Resetting

To reset all settings, quit Orchard Commander and delete `settings.json` in
the settings folder. Bookmarks, saved connections and color schemes are kept
in separate files in the same folder.
