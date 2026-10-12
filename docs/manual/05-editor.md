# Editing text files

Orchard Commander includes a text editor for source code, configuration files,
notes and other plain text. It opens in its own window, highlights the syntax of
most programming and markup languages, and works with files on your disk, on
servers and inside archives.

## Opening a file

1. Move the cursor to the file in the active panel.
2. Press **F4**.

The editor window opens with the file's contents. If the file is already open
in an editor window, that window comes to the front instead.

To create a new file and edit it:

1. Go to the folder in the active panel.
2. Press **⇧F4** / **Shift+F4**.
3. Type a name for the file and press **Enter**.

The new, empty file appears in the panel and opens in the editor.

Note: Files larger than 20 MB open only after you confirm. Very large files
open without syntax highlighting.

## The editor window

The window title shows the file name. A dot (●) in front of the name means
there are unsaved changes.

The line below the text shows:

- the cursor position (line and column)
- the file's encoding and line endings, for example **UTF-8 · LF**
- the language used for syntax highlighting
- **read-only**, if the file can't be changed

Line numbers appear at the left. The line with the cursor is highlighted. The
colors follow the color scheme you chose in Settings.

When you press **Enter**, the new line keeps the indentation of the previous one.

On macOS, the editor has its own **File** and **Edit** menus in the menu bar
while its window is in front. They list every editor command with its shortcut.

## Saving and closing

| Action | macOS | Windows / Linux |
| --- | --- | --- |
| Save | ⌘S | Ctrl+S |
| Close the window | ⌘W or Esc | Ctrl+W or Esc |
| Undo / redo | ⌘Z / ⇧⌘Z | Ctrl+Z / Ctrl+Y |

If you close a window with unsaved changes, the editor asks whether to save
them. Choose **Save**, **Don't save** or **Cancel**. Quitting Orchard Commander
asks the same question for every editor window with unsaved changes.

Files on your disk are saved safely: the editor writes the new contents to a
temporary file in the same folder and then replaces the original. The file's
permissions stay the same.

Files on servers are saved directly on the server.

## Encodings and line endings

The editor keeps the encoding and the line endings of the file:

- UTF-8, UTF-16 and UTF-32, with or without a byte order mark (BOM)
- Windows-1252 and Latin-1, for older files that aren't UTF-8
- LF (macOS, Linux), CRLF (Windows) and CR line endings

If a file mixes line endings, the editor uses the most common one when it saves.

If you type characters that the file's encoding can't store, such as an emoji
in a Windows-1252 file, the editor offers to save the file as UTF-8 instead.

## Find and replace

| Action | macOS | Windows / Linux |
| --- | --- | --- |
| Find | ⌘F | Ctrl+F |
| Find and replace | ⌥⌘F | Ctrl+Alt+F |
| Next match | ⌘G, F3 or Enter in the search field | F3 or Enter in the search field |
| Previous match | ⇧⌘G | Shift+F3 |
| Go to line | ⌘L or ⌥⌘L | Ctrl+L or Ctrl+Alt+L |

The search bar appears below the text. Searches ignore case unless you select
**Match case**. When the search reaches the end of the file, it continues at the
beginning.

To replace text:

1. Press **⌥⌘F** / **Ctrl+Alt+F**.
2. Type the text to find and the replacement.
3. Press **Enter** in the replacement field to replace the current match and go
   to the next one, or click **Replace all**.

**Replace all** is a single step: one undo restores every replaced match.

Press **Esc** to close the search bar.

## Files that change on disk

If another program changes a file while it is open in the editor, and you have
no unsaved changes, the editor reloads it. If you do have unsaved changes, the
editor asks whether to reload the file and discard your changes.

This applies to files on your disk. Changes made on a server by someone else
aren't detected.

## Files the editor can't change

These files open read-only:

- files inside archives
- files on connections that don't allow changes

The editor shows **read-only** in the line below the text, and you can't type
in the window. To change a file inside an archive, copy it out of the archive
first.

If you don't have permission to change a file, the editor opens it normally and
shows an error when you save.

Binary files, such as images or programs, don't open in the editor. To look at
their contents, use the hex view in the preview (**F3**, then choose **Hex**).
