# Getting started

## System requirements

| System | Requirements | Download |
| --- | --- | --- |
| macOS | macOS 14 Sonoma or later, Apple silicon (M1 or newer) | `Orchard-Commander-macOS.dmg` |
| Windows | Windows 10 or 11, 64-bit | `Orchard-Commander-Windows.zip` |
| Linux | x86-64, glibc 2.35 or later (for example Ubuntu 22.04, Fedora 36, Debian 12), X11 or Wayland | `Orchard-Commander-Linux.tar.gz` |

Downloads are available from the Orchard Commander website and from the
releases page of the GitHub repository.

## Installing

### macOS

1. Open `Orchard-Commander-macOS.dmg`.
2. Drag **Orchard Commander** onto the **Applications** folder in the window
   that opens.
3. Eject the disk image.
4. Start Orchard Commander from the Applications folder or Launchpad.

The app is signed and notarized, so macOS opens it without a warning.

### Windows

1. Extract `Orchard-Commander-Windows.zip` to a folder of your choice, for example
   `C:\Program Files\Orchard Commander` or a folder in your user directory.
2. Run `orchard-commander.exe` in the extracted `orchard-commander` folder.

The first time you run it, Windows SmartScreen may show "Windows protected
your PC". Click **More info**, then **Run anyway**.

Tip: To start the app from the Start menu, right-click
`orchard-commander.exe` and choose **Pin to Start**.

### Linux

1. Extract `Orchard-Commander-Linux.tar.gz`, for example with
   `tar xzf Orchard-Commander-Linux.tar.gz`, or with your file manager.
2. Run `./orchard-commander` in the extracted `orchard-commander` folder.

The app needs a desktop session (X11 or Wayland).

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
[Settings](11-settings.md)).

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
- [Keyboard reference](12-keyboard-reference.md) lists all shortcuts.
