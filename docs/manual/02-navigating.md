# Navigating

## Moving the cursor

| Key | Action |
| --- | --- |
| ↑ ↓ | Previous / next entry |
| Page Up / Page Down | One page up / down |
| Home / End | First entry (`..`) / last entry |
| Letters | Jump to the next entry whose name starts with the letters you type |

On Mac keyboards without Home and End keys, press **fn+←** and **fn+→**.

## Opening folders

- To open the folder under the cursor, press **Enter** or double-click it.
- To go to the parent folder, press **Backspace**, or open the `..` entry at
  the top of the list. The cursor is placed on the folder you came from.

Archives open like folders; see [Archives](06-archives.md).

Pressing **Enter** on a file opens it with its default application. See
[Opening files](03-files.md#opening-files).

## Going to a specific folder

1. Press **⌘G** / **Ctrl+G**.
2. Type the path of a folder, for example `~/Documents/Invoices` or
   `D:\Projects`, or a server address such as `sftp://anna@example.com/var/www`.
3. Press **Enter**.

The active panel opens the folder. A relative path such as `src/app` or
`../other` is resolved from the active panel's folder. Server addresses are
described in [Servers](07-servers.md).

## Choosing a drive or location

Press **⌥F1** / **Alt+F1** for the left panel or **⌥F2** / **Alt+F2** for the
right panel. The list contains:

- your drives (Windows) or the file system root, your home folder and
  mounted volumes (macOS and Linux),
- your saved server connections, marked ⇄,
- your bookmarks, marked ★.

Select an entry and press **Enter**.

## Showing the same folder in both panels

| Command | macOS | Windows / Linux |
| --- | --- | --- |
| Show the active panel's folder in the other panel too | ⌘E | Ctrl+E |
| Exchange the folders of the two panels | ⌘U | Ctrl+U |

## Transferring a folder to the other panel

**⌘←** / **Ctrl+←** and **⌘→** / **Ctrl+→** send a location to the left or
right panel:

- If you press the key while working in the *other* panel, the target panel
  opens the folder or archive under the cursor. If the cursor is on a file,
  the target panel opens the folder that contains it.
- If you press the key while working in the target panel itself, it opens
  the folder shown in the other panel.

For example, with the cursor on `Photos` in the left panel, **⌘→** /
**Ctrl+→** opens `Photos` in the right panel.

## Sorting

Click a column header (**Name**, **Size** or **Modified**) to sort by that
column. Click the same header again to reverse the order. Folders always come
before files.

Sorting reorders the entries already shown; the folder is not read again, so
on a server there is no wait. The sort order is not saved: each panel starts
sorted by name.

## Hidden files

Hidden files (names starting with a dot, and files with the hidden attribute
on Windows) are shown by default, in a dimmer color. To show or hide them,
press **⇧⌘.** / **Ctrl+Shift+.** (on Windows and Linux also **Ctrl+H**).
The setting applies to both panels and is remembered.

## Refreshing

The panels update automatically when the contents of a local folder change.
To re-read both folders manually, for example on a server, press **⌘R** /
**Ctrl+R**.

## Online-only files

Files in cloud folders such as Dropbox, iCloud Drive and OneDrive can be
stored online only. The panel marks these files with ☁ next to the size.
They are listed normally but downloaded only when you open, copy or preview
them. See [Cloud files](04-preview.md#cloud-files).

## Zooming

| Command | macOS | Windows / Linux |
| --- | --- | --- |
| Larger font | ⌘= | Ctrl+= |
| Smaller font | ⌘- | Ctrl+- |
| Default size | ⌘0 | Ctrl+0 |

The font size applies to the whole window and is saved.

## Where the app starts

When you start Orchard Commander, each panel opens the last local folder it
showed. Folders on servers and inside archives are not restored; the panel
opens the nearest local folder instead.
