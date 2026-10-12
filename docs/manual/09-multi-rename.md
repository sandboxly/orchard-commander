# Multi-rename

Multi-rename renames many files in one step. The new names are built from a
pattern, which can include parts of the old name, a counter, the
modification date and metadata such as music tags or the date a photo was
taken. You see every new name before anything is renamed.

## Renaming a group of files

1. Mark the files and folders you want to rename.
2. Press **⌘M** / **Ctrl+M**.
3. Edit the **Name** and **Extension** patterns, and optionally the search
   and replace, case and counter settings.
4. Check the new names in the list.
5. Click **Rename** or press **Enter**.

You can also rename the results of a search. Each file is renamed in its own
folder.

The dialog keeps your last settings until you quit the app.

## The dialog

| Field | Description |
| --- | --- |
| Name | The pattern for the new name, without the extension. Default: `[N]` (the old name). |
| Extension | The pattern for the new extension. Default: `[E]` (the old extension). Leave empty to remove extensions. |
| Insert… | Inserts a placeholder at the cursor position in the field next to it |
| Search / Replace | Text to replace in the new name (see [Search and replace](#search-and-replace)) |
| RegEx | Treat **Search** as a regular expression |
| Case sensitive | Distinguish upper and lower case when searching |
| Case | Changes the case of the whole new name: **Unchanged**, **lowercase**, **UPPERCASE**, **First letter uppercase**, **Every Word Uppercase** |
| Counter [C]: start, step, digits | Settings for the `[C]` placeholder |

The new name is built in this order: patterns, then search and replace, then
the case conversion.

Folders have no extension; for folders, the **Extension** pattern is ignored.
A file name that starts with a dot and contains no other dot, such as
`.bashrc`, also has no extension.

## Placeholders

Placeholders are written in square brackets. They are case-sensitive: `[N]`
is the name, `[n]` restores the original case.

### Name and extension

| Placeholder | Inserts |
| --- | --- |
| `[N]` | The old name without the extension |
| `[E]` | The old extension |
| `[N3]` | The 3rd character of the name |
| `[N2-5]` | Characters 2 to 5 |
| `[N2,5]` | 5 characters, starting at the 2nd |
| `[N3-]` | From the 3rd character to the end |
| `[N-5-]` | The last 5 characters |
| `[N-5,2]` | 2 characters, starting at the 5th from the end |
| `[N-8-5]` | From the 8th-last to the 5th-last character |
| `[N2--5]` | From the 2nd to the 5th-last character |
| `[E1-2]` | Characters 1 to 2 of the extension (all ranges work with `E`) |
| `[1-5]` | Characters 1 to 5 of the complete name, including the extension |

### Folders

| Placeholder | Inserts |
| --- | --- |
| `[P]` | The name of the folder that contains the file |
| `[G]` | The name of the folder above that |

Ranges work here too, for example `[P1-4]`.

### Counter

| Placeholder | Inserts |
| --- | --- |
| `[C]` | A counter, using **start**, **step** and **digits** from the dialog |
| `[C10]` | A counter starting at 10 |
| `[C+5]` | A counter that increases by 5 |
| `[C:3]` | A counter with 3 digits (001, 002, …) |
| `[C10+5:3]` | Start at 10, step 5, 3 digits: 010, 015, 020, … |

The counter follows the order of the files in the list.

### Date and time

These placeholders insert the file's modification date and time.

| Placeholder | Inserts | Example |
| --- | --- | --- |
| `[Y]` | Year | 2026 |
| `[M]` | Month | 03 |
| `[D]` | Day | 07 |
| `[h]` | Hours (24-hour clock) | 09 |
| `[m]` | Minutes | 05 |
| `[s]` | Seconds | 04 |
| `[d]` | Date | 2026-03-07 |
| `[t]` | Time | 09.05.04 |

For the date a photo was taken, use the `[=taken]` metadata field instead.

### Case

These placeholders change the case of everything that follows them in the
pattern.

| Placeholder | Effect |
| --- | --- |
| `[U]` | UPPERCASE |
| `[L]` | lowercase |
| `[F]` | First Letter Of Each Word Uppercase |
| `[n]` | Original case |

Example: `[U][N1-3][n][N4-]` turns the first three characters to upper case
and keeps the rest.

### Brackets

To insert a square bracket, write `[[]` for `[` and `[]]` for `]`.
Example: `[N] [[]draft[]]` gives `report [draft]`.

## Metadata fields

Metadata placeholders insert information stored in the file itself. Write
them as `[=field]`. The **Insert…** menu lists them by file type.

| Syntax | Inserts |
| --- | --- |
| `[=artist]` | The artist tag |
| `[=artist:1-3]` | Characters 1 to 3 of it (same ranges as `[N]`) |
| `[=artist\|Unknown]` | The artist, or `Unknown` if the file has none |

### Audio files

MP3, FLAC, M4A, AAC, Ogg, Opus, WMA, WAV, AIFF and others.

| Field | Inserts |
| --- | --- |
| `artist` | Artist |
| `albumartist` | Album artist |
| `album` | Album |
| `title` | Title |
| `track` | Track number with two digits, for example `03` (three digits for albums with 100 or more tracks) |
| `tracktotal` | Number of tracks |
| `disc` | Disc number |
| `year` | Year |
| `genre` | Genre |
| `composer` | Composer |
| `duration` | Length, for example `3m25s` |
| `bitrate` | Bit rate in kbit/s |

The tags `title` and `year` are also read from MP4, M4V and MOV videos.

### Videos and images

| Field | Inserts |
| --- | --- |
| `width` | Width in pixels |
| `height` | Height in pixels |
| `resolution` | Width and height, for example `1920x1080` |
| `quality` | The height as `1080p`, `720p` and so on (videos) |
| `duration` | Length, for example `1h02m03s` (videos) |
| `fps` | Frames per second (videos) |
| `title` | Title (videos) |

### Photos (EXIF)

| Field | Inserts |
| --- | --- |
| `taken` | Date the photo was taken, for example `2026-03-07` |
| `taken_time` | Time the photo was taken, for example `09.05.04` |
| `taken_year` | Year taken |
| `taken_month` | Month taken, for example `03` |
| `camera` | Camera make and model |
| `make` | Camera make |
| `model` | Camera model |
| `lens` | Lens |
| `iso` | ISO setting |
| `focal` | Focal length in mm |

### How metadata is read

- Metadata is read in the background. Until it has been read for all files,
  the list shows "reading metadata…" for the remaining files and **Rename**
  is unavailable.
- Characters that aren't allowed in file names are replaced. For example,
  the artist "AC/DC" becomes `AC-DC`, and a colon followed by a space becomes
  ` - `.
- Files on servers are downloaded to read their metadata. Online-only cloud
  files are not downloaded; they are skipped.

### Files without a value

If a file doesn't have a value for a field in the pattern, it is not renamed.
Its row shows, for example, "skipped: no artist". The other files are renamed
normally. To rename such files anyway, add a fallback: `[=artist|Unknown]`.

## Search and replace

Search and replace is applied to the new name, including the extension.

| Search | Replace | Effect |
| --- | --- | --- |
| `copy` | *(empty)* | Removes `copy` |
| `_` | ` ` | Replaces underscores with spaces |
| `-\|_` | ` ` | Replaces hyphens and underscores with spaces |
| `-\|_` | `+\|=` | Replaces hyphens with `+` and underscores with `=` |

- Separate several search terms with `|`. With one replacement, all of them
  are replaced by it; with several, they are paired in order.
- Without **RegEx**, characters such as `.` and `*` are matched literally.
- With **RegEx**, use `$1`, `$2` (or `\1`, `\2`) in the replacement for the
  groups of the expression. For example, search `(\d+)-(\d+)-(\d+)` and
  replace with `$3.$2.$1` to turn `2026-03-07` into `07.03.2026`.

## Checking the new names

The list shows each file's old and new name. The status column explains
anything unusual:

| Status | Meaning |
| --- | --- |
| *(empty)* | The file will be renamed |
| unchanged | The new name is the same as the old one |
| skipped: … | A metadata value is missing; the file keeps its name |
| reading metadata… | The file's metadata hasn't been read yet |
| same new name as … | Two files would get the same name |
| a file with this name already exists | A file with the new name is already in the folder |
| empty name, contains "/", name too long | The new name isn't valid |
| inside …, which is renamed too | A file and the folder that contains it are both in the list. Rename them in two steps. |

Problems are shown in red. **Rename** is available only when there are no
problems. The line below the list summarizes how many files will be renamed,
skipped or have problems. If the pattern itself contains an error, such as a
missing `]`, the line describes it.

Files can be swapped or renamed in a circle, for example `1.txt` → `2.txt`
and `2.txt` → `3.txt`, and names can differ in case only, such as
`photo.JPG` → `photo.jpg`.

## Undoing a rename

Press **⌘Z** / **Ctrl+Z** and confirm with **Undo** to give the files of the
last multi-rename their old names back. Only the last multi-rename can be
undone.

## Examples

**Number photos in order, keeping the folder name**

| Name | Extension | Counter | Result |
| --- | --- | --- | --- |
| `[P]_[C]` | `[L][E]` | start 1, step 1, digits 3 | `Holiday_001.jpg`, `Holiday_002.jpg`, … |

**Name photos by the date they were taken**

| Name | Extension | Result |
| --- | --- | --- |
| `[=taken] [=taken_time]` | `[L][E]` | `2026-03-07 09.05.04.jpg` |

If two photos were taken in the same second, add a counter: `[=taken] [=taken_time] [C]`.

**Name music files from their tags**

| Name | Result |
| --- | --- |
| `[=track] [=artist] - [=title]` | `03 AC-DC - Thunderstruck.mp3` |
| `[=track] [=title\|Untitled]` | `07 Untitled.mp3` for a file without a title |

**Add the resolution to video names**

| Name | Result |
| --- | --- |
| `[N] ([=quality])` | `Interview (1080p).mp4` |

**Remove a prefix**

| Name | Result |
| --- | --- |
| `[N5-]` | `IMG_4501` → `4501` |
