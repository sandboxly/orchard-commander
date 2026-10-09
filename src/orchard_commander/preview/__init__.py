"""Pluggable file previews.

A previewer (see `api.Previewer`) declares which files it handles and what data
it needs, loads that data on a worker thread, and supplies a view widget. The
registry picks the best previewer for a file and falls back to the next one
when a previewer can't handle it. Built-in previewers live in `builtin/`;
add-ons register through the `orchard_commander.previewers` entry point.
"""

from orchard_commander.preview.content import (  # Qt-free helpers, re-exported
    AUDIO_EXTS,
    HEXDUMP_BYTES,
    IMAGE_EXTS,
    MARKDOWN_EXTS,
    MAX_TEXT_BYTES,
    TEXT_SNIFF_BYTES,
    VIDEO_EXTS,
    Kind,
    TextContent,
    decode_text,
    describe,
    describe_entry,
    detect,
    directory_summary,
    extension,
    format_listing,
    hexdump,
    kind_by_name,
    kind_from_head,
    looks_like_text,
    read_hexdump,
    read_text,
)

__all__ = [
    "AUDIO_EXTS",
    "HEXDUMP_BYTES",
    "IMAGE_EXTS",
    "MARKDOWN_EXTS",
    "MAX_TEXT_BYTES",
    "TEXT_SNIFF_BYTES",
    "VIDEO_EXTS",
    "Kind",
    "TextContent",
    "decode_text",
    "describe",
    "describe_entry",
    "detect",
    "directory_summary",
    "extension",
    "format_listing",
    "hexdump",
    "kind_by_name",
    "kind_from_head",
    "looks_like_text",
    "read_hexdump",
    "read_text",
]
