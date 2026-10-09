"""Preview content helpers, independent of the UI toolkit.

Decides what kind of preview a path gets and produces the text-based previews
(source text, directory summaries, hex dumps). Images and media are loaded by
the UI, which knows what its toolkit can decode.
"""

from __future__ import annotations

import datetime as dt
import enum
import os
import stat
from dataclasses import dataclass
from pathlib import Path

from orchard_commander import archives, fs

TEXT_SNIFF_BYTES = 8192
MAX_TEXT_BYTES = 1024 * 1024
HEXDUMP_BYTES = 4096


class Kind(enum.Enum):
    DIRECTORY = "directory"
    ARCHIVE = "archive"
    TEXT = "text"
    MARKDOWN = "markdown"
    IMAGE = "image"
    VIDEO = "video"
    AUDIO = "audio"
    BINARY = "binary"
    EMPTY = "empty"
    UNREADABLE = "unreadable"


# Overridden at startup by the UI with what its image decoder actually supports.
IMAGE_EXTS = {"png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff", "ico", "svg"}
VIDEO_EXTS = {
    "mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "flv", "mpg", "mpeg", "3gp", "ts", "mts", "m2ts", "ogv",
}
AUDIO_EXTS = {"mp3", "m4a", "aac", "wav", "flac", "ogg", "oga", "opus", "wma", "aiff", "aif"}
MARKDOWN_EXTS = {"md", "markdown", "mdown"}


def extension(path: Path) -> str:
    return path.suffix.lower().lstrip(".")


def looks_like_text(data: bytes) -> bool:
    """Heuristic: no NUL bytes and decodes as UTF-8 (allowing a cut-off final character)."""
    if not data:
        return True
    if b"\0" in data:
        return False
    try:
        data.decode("utf-8")
        return True
    except UnicodeDecodeError as exc:
        if exc.start >= len(data) - 3:  # multi-byte char split by the sniff limit
            return True
    # Fall back to "mostly printable" for legacy 8-bit encodings.
    printable = sum(32 <= b < 127 or b in (9, 10, 13) or b >= 160 for b in data)
    return printable / len(data) > 0.95


def kind_by_name(name: str) -> Kind | None:
    """The kind implied by the file name alone (archives and media), if any."""
    if archives.is_archive(name):
        return Kind.ARCHIVE
    ext = name.rpartition(".")[2].lower() if "." in name else ""
    if ext in IMAGE_EXTS:
        return Kind.IMAGE
    if ext in VIDEO_EXTS:
        return Kind.VIDEO
    if ext in AUDIO_EXTS:
        return Kind.AUDIO
    return None


def kind_from_head(name: str, head: bytes, size: int) -> Kind:
    """Classify a file from its name and first bytes (no local copy needed)."""
    by_name = kind_by_name(name)
    if by_name is not None:
        return by_name
    if size == 0:
        return Kind.EMPTY
    if looks_like_text(head):
        return Kind.MARKDOWN if name.rpartition(".")[2].lower() in MARKDOWN_EXTS else Kind.TEXT
    return Kind.BINARY


def detect(path: Path) -> Kind:
    try:
        st = path.stat()
    except OSError:
        return Kind.UNREADABLE
    if stat.S_ISDIR(st.st_mode):
        return Kind.DIRECTORY
    if (by_name := kind_by_name(path.name)) is not None:
        return by_name
    try:
        with open(path, "rb") as f:
            head = f.read(TEXT_SNIFF_BYTES)
    except OSError:
        return Kind.UNREADABLE
    return kind_from_head(path.name, head, st.st_size)


@dataclass(slots=True)
class TextContent:
    text: str
    truncated: bool


def decode_text(data: bytes, truncated: bool) -> TextContent:
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        text = data.decode("utf-8", errors="ignore") if truncated else data.decode("latin-1")
    return TextContent(text.replace("\r\n", "\n"), truncated)


def read_text(path: Path, limit: int = MAX_TEXT_BYTES) -> TextContent:
    with open(path, "rb") as f:
        data = f.read(limit + 1)
    return decode_text(data[:limit], len(data) > limit)


def hexdump(data: bytes, offset: int = 0) -> str:
    lines = []
    for i in range(0, len(data), 16):
        chunk = data[i : i + 16]
        hexes = " ".join(f"{b:02x}" for b in chunk)
        hexes = hexes[:23] + " " + hexes[23:] if len(chunk) > 8 else hexes
        ascii_ = "".join(chr(b) if 32 <= b < 127 else "." for b in chunk)
        lines.append(f"{offset + i:08x}  {hexes:<48}  {ascii_}")
    return "\n".join(lines)


def read_hexdump(path: Path, limit: int = HEXDUMP_BYTES) -> str:
    with open(path, "rb") as f:
        data = f.read(limit)
    return hexdump(data)


def describe_entry(entry: fs.Entry, location: str) -> list[tuple[str, str]]:
    """Facts about a file on any file system, from its listing entry."""
    facts = [("Name", entry.name), ("Location", location)]
    if not entry.is_dir:
        facts.append(("Size", f"{fs.human_size(entry.size)} ({entry.size:,} bytes)"))
    if entry.mtime:
        facts.append(("Modified", dt.datetime.fromtimestamp(entry.mtime).strftime("%Y-%m-%d %H:%M:%S")))
    return facts


def describe(path: Path) -> list[tuple[str, str]]:
    """Basic facts about a file for the preview header / info view."""
    st = path.lstat()
    facts = [("Name", path.name), ("Location", str(path.parent))]
    if not stat.S_ISDIR(st.st_mode):
        facts.append(("Size", f"{fs.human_size(st.st_size)} ({st.st_size:,} bytes)"))
    facts.append(("Modified", dt.datetime.fromtimestamp(st.st_mtime).strftime("%Y-%m-%d %H:%M:%S")))
    facts.append(("Permissions", stat.filemode(st.st_mode)))
    if stat.S_ISLNK(st.st_mode):
        facts.append(("Link to", os.readlink(path)))
    return facts


def format_listing(entries: list[fs.Entry], max_entries: int = 500) -> str:
    """A folder-style summary: counts and total size, then one line per entry."""
    dirs = [e for e in entries if e.is_dir]
    files = [e for e in entries if not e.is_dir]
    total = sum(e.size for e in files)
    lines = [
        f"{len(dirs)} director{'y' if len(dirs) == 1 else 'ies'}, "
        f"{len(files)} file{'' if len(files) == 1 else 's'} ({fs.human_size(total)})",
        "",
    ]
    for e in entries[:max_entries]:
        lines.append(f"{e.name}{os.sep}" if e.is_dir else f"{e.name:<40} {fs.human_size(e.size):>9}")
    if len(entries) > max_entries:
        lines.append(f"… and {len(entries) - max_entries} more")
    return "\n".join(lines)


def directory_summary(path: Path, show_hidden: bool) -> str:
    return format_listing(fs.list_dir(path, show_hidden=show_hidden))
