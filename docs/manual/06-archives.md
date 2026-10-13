# Archives

Orchard Commander opens archives like folders. You can browse them, preview
and open the files inside, and copy files out of them.

## Supported formats

| Format | File name endings |
| --- | --- |
| ZIP | `.zip` |
| Tar, also compressed | `.tar`, `.tar.gz`, `.tgz`, `.tar.bz2`, `.tbz`, `.tbz2`, `.tar.xz`, `.txz` |
| 7-Zip | `.7z` |
| RAR (RAR4 and RAR5) | `.rar` |
| Single compressed files | `.gz`, `.bz2`, `.xz`, `.lzma` |

A single compressed file, such as `report.txt.gz`, opens as a folder that
contains the uncompressed file (`report.txt`).

All formats, including RAR and 7-Zip, are handled by Orchard Commander
itself. No other program, such as `unrar` or 7-Zip, needs to be installed.
All formats are read-only (see [Limitations](#limitations)).

Password-protected (encrypted) entries can't be read. Encrypted 7-Zip and
RAR archives can't be opened.

## Browsing an archive

1. Place the cursor on the archive.
2. Press **Enter**.

The panel shows the archive's contents. Move through its folders as usual.
To leave the archive, press **Backspace** in its top folder or open `..`.

Archives inside archives open the same way.

Archives on servers can also be browsed. Orchard Commander downloads the
archive first.

To open an archive in the system's archive tool instead of browsing it,
press **Shift+Enter**.

## Extracting files

1. Open the archive in one panel and the destination folder in the other.
2. Mark the files and folders you want to extract.
3. Press **F5** and confirm the destination.

To extract the complete archive, mark everything with **+** before pressing
**F5**.

## Limitations

Archives are read-only. You can't move, rename or delete files inside an
archive, create folders in it, or copy files into it.

## Previewing archives

With the cursor on an archive and the Preview view open on the other side,
the preview lists the archive's contents. Files inside an archive preview
like any other file.
