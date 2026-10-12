# Working with files

## Marking files

File commands act on the **marked** files in the active panel. If no files
are marked, they act on the file or folder under the cursor.

Marked entries are shown in a different color, and the bottom line of the
panel shows how many are marked and their total size.

| Action | Keys and mouse |
| --- | --- |
| Mark or unmark the entry under the cursor and move down | Space or Insert |
| Mark or unmark an entry with the mouse | ⌘-click or right-click / Ctrl-click or right-click |
| Mark all entries | + or ⌘A / Ctrl+A |
| Unmark all entries | - or ⌥⌘A / Ctrl+Alt+A |
| Invert the marks | * |

**Files → Select all** and **Files → Clear selection** do the same as ⌘A and
⌥⌘A.

Marks are cleared when you open another folder and after a file operation
completes.

## Copying

1. Mark the files and folders you want to copy.
2. Press **F5**.
3. Check the destination. It is the folder of the other panel. To copy
   somewhere else, edit the path.
4. Click **Copy** or press **Enter**.

Folders are copied with everything they contain.

To make a copy in the same folder under a new name, open the same folder in
both panels (**⌘E** / **Ctrl+E**), place the cursor on the file and press
**F5**. The dialog suggests the file name; change it and press **Enter**.

You can type a destination path that is:

- absolute, for example `/Volumes/Backup/2026` or `E:\Archive`;
- relative, which is resolved from the active panel's folder, for example
  `backup` or `../old`.

## Moving and renaming

**F6** moves the marked files to the other panel's folder. It works like
copying, and the original files are removed after they have been moved.

To rename a single file or folder:

1. Place the cursor on it, with nothing marked.
2. Press **F6**.
3. Replace the destination with the new name and press **Enter**.

A name without a path keeps the file in the same folder.

To rename many files at once with a pattern, use
[Multi-rename](09-multi-rename.md).

## When a file already exists

If a file with the same name exists at the destination, the **File exists**
dialog shows both files with their size and date. Choose:

| Button | Key | Result |
| --- | --- | --- |
| Overwrite | O | Replace this file |
| All | A | Replace this file and all further existing files |
| Skip | S | Keep the existing file and continue with the next one |
| Skip all | K | Skip this and all further existing files |
| Cancel | | Stop the operation |

## Progress and cancelling

While copying or moving, a progress window shows the current file, the
number of files done and the amount of data copied. Click **Cancel** to stop.
Files that were already copied before you cancelled stay at the destination;
the file that was being copied is removed.

Only one copy or move operation can run at a time.

## Creating a folder

1. Press **F7**.
2. Type the name of the new folder and press **Enter**.

To create several levels at once, type a path such as `2026/March`. The
cursor is placed on the new folder.

## Deleting

1. Mark the files and folders you want to delete.
2. Press **F8**, **Delete**, or **⌘⌫** / **Ctrl+Backspace**.
3. Confirm with **Trash**.

The files are moved to the macOS Trash, the Windows Recycle Bin or the Linux
trash, from where you can restore them.

Servers have no trash. On a server, the confirmation is titled
**Delete permanently** and the files are deleted immediately. This cannot be
undone.

## Opening files

| Action | Result |
| --- | --- |
| **Enter** or double-click on a file | Opens the file with its default application |
| **Shift+Enter** or **⌘Enter** / **Ctrl+Enter** on a file or archive | Opens it with the default application (an archive opens in the system's archive tool instead of being browsed) |
| **Shift+Enter** on a folder | Opens the folder in Finder, File Explorer or your desktop's file manager |

Files on servers and inside archives are downloaded to a temporary folder
first and then opened from there. Changes you make to such a temporary copy
are not written back.

## Read-only locations

Some locations can't be changed: archives, the results of a search (as a
folder; the files themselves can be changed), and servers or folders where
you lack permission. Commands that would change them show a message such as
"Can't create a directory here: read-only".
