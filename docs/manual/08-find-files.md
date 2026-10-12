# Finding files

Find files searches a folder and its subfolders by name, content, size and
date. The results appear in the active panel as a folder, and you can work
with them like any other files.

## Starting a search

1. Open the folder you want to search in the active panel.
2. Press **⌘F** / **Ctrl+F** or **⌥F7** / **Alt+F7**.
3. Fill in the search criteria (see below).
4. Click **Search** or press **Enter**.

The dialog keeps your last entries until you quit the app. **Search in**
always starts with the active panel's folder.

## Search criteria

### Names

| Field | Description |
| --- | --- |
| Search for | One or more file name masks (see below). Leave `*` to match all names. |
| RegEx | Treat **Search for** as a regular expression |
| Case sensitive | Distinguish upper and lower case in names |
| Search in | The folder to search. To search several local folders, separate them with `;`. |
| Depth | **All subfolders**, **This folder only**, or a number of levels |
| Find | **Files and folders**, **Files only** or **Folders only** |
| Include | **Hidden files**; **Inside archives** also searches the contents of archives |

**Search in** accepts any location a panel can show, including servers and
archives, if you start the search from there.

### Name masks

| Mask | Finds |
| --- | --- |
| `*.pdf` | Names ending in `.pdf` |
| `*.jpg;*.png` or `*.jpg *.png` | Names ending in `.jpg` or `.png` |
| `report` | Names that contain `report` anywhere (a mask without `*`, `?` or `[` matches anywhere in the name) |
| `report?.pdf` | `report1.pdf`, `reportA.pdf` and so on (`?` is one character) |
| `*.zip\|*old*` | ZIP files whose names don't contain `old` (masks after `\|` exclude) |
| `\|*.tmp` | Everything except names ending in `.tmp` |

Masks are not case-sensitive unless you select **Case sensitive**.

### Contents

| Field | Description |
| --- | --- |
| Containing | Text the files must contain. Leave empty to search by name only. |
| RegEx | Treat the text as a regular expression |
| Case sensitive | Distinguish upper and lower case |
| Whole words | Match only whole words |
| NOT containing | Find files that do *not* contain the text |
| Also cloud-only files | Also search online-only cloud files. These files are downloaded to be searched. |

Content searches apply to files only. Without **Also cloud-only files**,
online-only files are skipped, and the status line reports how many.

### Size and date

| Field | Description |
| --- | --- |
| Size: at least / at most | Sizes such as `500k`, `10M` or `1.5G`. Leave empty for no limit. |
| Modified | **Any time**, **Today**, **Last 7 days**, **Last 30 days**, **Last year**, **Older than 30 days** or **Older than a year** |

## The results

The active panel switches to the results while the search is running. Its
title shows the search, and the bottom line shows the progress, for example
"searching… 1,204 folders, 18,331 files · 27 hits". New hits appear as they
are found.

Each hit is listed with its path below the searched folder, for example
`2026/March/invoice.pdf`.

To stop a running search, press **Esc**.

When the search is complete, a message shows the number of hits. The bottom
line also reports any folders that couldn't be read and cloud-only files
that were skipped.

### Working with the results

The results act on the real files:

- Preview a hit with **F3**, or open it with **Enter**.
- Press **Enter** on a folder hit to open that folder.
- Copy (**F5**), move (**F6**), delete (**F8**) or rename
  ([Multi-rename](09-multi-rename.md)) marked hits.

Hits that you move or delete disappear from the results. Hits you rename stay
in the list under their new names.

To collect all hits in one folder:

1. Open the destination folder in the other panel.
2. Press **+** to mark all hits.
3. Press **F6** to move them, or **F5** to copy them.

You can't create folders or copy files into the results themselves.

### Leaving and returning to the results

- Press **Backspace** or open `..` to return to the folder you searched.
- Press **⇧⌘F** / **Ctrl+Shift+F** to show the last results again, in the
  active panel.

Starting a new search from the results searches the original folder again.
