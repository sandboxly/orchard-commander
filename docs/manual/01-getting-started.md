# Getting started

## System requirements

| System | Requirements |
| --- | --- |
| macOS | macOS 14 Sonoma or later, Apple silicon (M1 or newer) |
| Windows | Windows 10 or 11, 64-bit |
| Linux | x86-64, glibc 2.35 or later (for example Ubuntu 22.04, Fedora 36, Debian 12), X11 or Wayland |

Downloads are available from the Orchard Commander website and from the
releases page of the GitHub repository.

## Installing

### macOS

With Homebrew:

```sh
brew install --cask sandboxly/tap/orchard-commander
```

To update later, run `brew upgrade --cask orchard-commander`.

With the disk image:

1. Open `Orchard-Commander-macOS.dmg`.
2. Drag **Orchard Commander** onto the **Applications** folder in the window
   that opens.
3. Eject the disk image.
4. Start Orchard Commander from the Applications folder or Launchpad.

The app is signed and notarized, so macOS opens it without a warning.

### Windows

With winget, which is part of Windows 10 and 11:

```sh
winget install Sandboxly.OrchardCommander
```

With Chocolatey (in an administrator terminal):

```sh
choco install orchard-commander
```

To update later, run `winget upgrade Sandboxly.OrchardCommander` or
`choco upgrade orchard-commander`.

With the installer:

1. Run `Orchard-Commander-Setup.exe`.
2. Choose whether to install for yourself only or for all users of the
   computer. Installing for all users requires administrator rights.
3. Follow the steps of the installer. You can add a desktop shortcut on the
   **Select Additional Tasks** page.

The installer adds Orchard Commander to the Start menu and to **Settings >
Apps**, where you can uninstall it.

Windows SmartScreen may show "Windows protected your PC" when you run the
installer. Click **More info**, then **Run anyway**.

To use Orchard Commander without installing it, extract
`Orchard-Commander-Windows.zip` to a folder of your choice and run
`orchard-commander.exe` in the extracted `orchard-commander` folder.

### Linux

| Distribution | File | Install with |
| --- | --- | --- |
| Debian, Ubuntu, Linux Mint | `Orchard-Commander-Linux-x86_64.deb` | `sudo apt install ./Orchard-Commander-Linux-x86_64.deb` |
| Fedora, RHEL, openSUSE | `Orchard-Commander-Linux-x86_64.rpm` | `sudo dnf install ./Orchard-Commander-Linux-x86_64.rpm` (openSUSE: `sudo zypper install`) |
| Any distribution | `Orchard-Commander-Linux-x86_64.AppImage` | Make the file executable (`chmod +x`) and run it |
| Any distribution | `Orchard-Commander-Linux.tar.gz` | Extract it and run `./orchard-commander` in the `orchard-commander` folder |

The `.deb` and `.rpm` packages install the app to `/opt/orchard-commander`,
add the `orchard-commander` command and a menu entry, and install the
libraries the app needs. Uninstall them with your package manager.

The AppImage needs FUSE 2 (`libfuse2` on Debian and Ubuntu). Without it, run
the AppImage with `--appimage-extract-and-run`.

## The main window

The window is divided into two **sides**, left and right. Each side has two
tabs at the top:

- **Files** shows a folder as a list (the **panel**).
- **Preview** shows the file under the cursor in the *other* side's panel.

Each panel lists folders first, then files, with their size and modification
date. The panel's title shows the current folder; the line at the bottom
shows the number of folders and files, the free space, and, when you have
marked files, how many are marked and their total size.

Below the two sides is the **function-key bar**. Click a key in the bar or
press the key itself:

| Key | Command |
| --- | --- |
| F3 | Preview |
| F5 | Copy |
| F6 | Move |
| F7 | Make directory |
| F8 | Move to trash |
| F9 | Menu |
| F10 | Quit |

The right end of the bar shows the shortcuts for connecting to a server and
for the command palette.

### The menu bar

The menus are **Left**, **Files**, **Commands**, **Options** and **Right**.
**Left** and **Right** contain commands for that side, such as switching
between Files and Preview and choosing a drive.

On macOS, the menus are in the macOS menu bar at the top of the screen. On
all systems you can also show them inside the window: press **F9**, which
opens the menu of the side you are working in. To keep the menu bar visible
in the window, turn on **Always show the menu bar** in Settings (see
[Settings](12-settings.md)).

## The active panel

The panel with the highlighted frame is the **active panel**. File commands
such as Copy and Move act on the active panel and use the **other panel** as
the destination.

- Press **Tab** to make the other side active.
- Click a panel to make it active.

When the active side shows the Preview view, file commands act on the panel
on the other side, which is the one you are previewing from.

## The command palette

The command palette lists every command with its shortcut. Use it to find a
command without knowing where it is in the menus.

1. Press **⌘P** / **Ctrl+P**.
2. Type part of the command name.
3. Select the command with the arrow keys and press **Enter**.

## Notifications

Short messages, such as "Copied 3 files" or an error, appear briefly at the
bottom of the window. They don't require any action.

## Where to go next

- [Navigating](02-navigating.md) explains how to move around.
- [Working with files](03-files.md) covers copying, moving and deleting.
- [Keyboard reference](13-keyboard-reference.md) lists all shortcuts.
