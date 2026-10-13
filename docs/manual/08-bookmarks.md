# Bookmarks

Bookmarks take you to frequently used folders. A bookmark can point to a
local folder, a folder inside an archive, or a folder on a server. A bookmark
can also set the other panel's folder at the same time.

## Opening the bookmark menu

Press **⌘D** / **Ctrl+D**, or click the folder path at the top of a panel.
The menu opens at the active panel.

To open a bookmark:

- press the hotkey shown in the first column, or
- select the bookmark with the arrow keys and press **Enter**, or
- click it.

If several bookmarks share a hotkey, pressing the key moves between them;
press **Enter** to open the selected one.

Bookmarks to servers connect with the saved connection, using the password
from the system keychain if it was saved.

## Adding a bookmark

1. Open the folder you want to bookmark in the active panel.
2. Press **⌘D** / **Ctrl+D**, then **+** (**Add current folder…**).
3. Edit the **Title**. Type `&` before the letter you want to use as the
   hotkey, for example `&Projects` for P.
4. In **Put in**, choose a bookmark folder, or leave the top level selected.
5. Optionally select **Also open the other panel at…** to store the other
   panel's folder with the bookmark.
6. Click **Add**.

To remove the bookmark of the current folder, open the menu and press **-**.

## Searching bookmarks

Press **⌥⌘D** / **Ctrl+Alt+D**, type part of a bookmark's title or path, and
press **Enter**.

Bookmarks also appear in the drive list (**⌥F1** / **Alt+F1** and **⌥F2** /
**Alt+F2**), marked ★.

## Organizing bookmarks

Open the bookmark menu and press **\*** (**Organize bookmarks…**), or choose
**Commands → Organize bookmarks**.

| Action | How |
| --- | --- |
| Reorder | Drag an entry, or select it and click ▲ or ▼ |
| Move into a folder | Drag the entry onto the folder |
| Create a folder | Click **New folder** |
| Add a separator line | Click **Separator** |
| Change title, folder or other panel folder | Select the entry and click **Edit…**, or double-click it |
| Delete | Select the entry and click **Delete** |

Click **OK** to save your changes, or **Cancel** to discard them.

In the **Edit bookmark** dialog, **Folder** is the folder the bookmark opens
and **Other panel** is an optional folder for the other panel. Click
**Use active panel** or **Use other panel** to fill in the folder currently
shown in that panel.

Bookmarks are stored in the file `bookmarks.json` in the settings folder
(see [Settings](12-settings.md#where-settings-are-stored)).
