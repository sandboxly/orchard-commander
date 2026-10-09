"""Folders and archives: a listing of what's inside."""

from __future__ import annotations

from orchard_commander import archives, fs, vfs
from orchard_commander.preview import content
from orchard_commander.preview.api import (
    Access,
    FileInfo,
    LoadContext,
    Loaded,
    Previewer,
    PreviewView,
    Score,
    ShowMessage,
    ViewContext,
)
from orchard_commander.preview.builtin.textview import TextData, TextView
from orchard_commander.preview.source import PreviewSource

MAX_FETCH_BYTES = 512 * 1024**2  # archives without a direct path are fetched whole to list them


class ListingPreviewer(Previewer):
    id = "listing"
    title = "Contents"
    access = Access.METADATA

    def score(self, f: FileInfo) -> int:
        if f.is_dir:
            return Score.FORMAT
        return Score.FORMAT if archives.is_archive(f.name) else Score.NO

    def load(self, src: PreviewSource, ctx: LoadContext) -> Loaded:
        target = src.target
        if src.entry.is_dir:
            listing = content.format_listing(target.list_dir(show_hidden=ctx.show_hidden))
            return Loaded(TextData(listing), f"folder · {target.fs.type_name}")
        if src.direct_path is None:  # the whole archive has to be fetched to list it
            src.local_file(MAX_FETCH_BYTES)  # raises ShowMessage if too large
        try:
            root = vfs.open_archive(target)
        except (archives.ArchiveError, fs.OperationError) as exc:
            raise ShowMessage(str(exc)) from exc
        listing = content.format_listing(root.list_dir(show_hidden=ctx.show_hidden))
        return Loaded(TextData(listing), f"{root.fs.type_name} · {fs.human_size(src.size)}")

    def create_view(self, ctx: ViewContext) -> PreviewView:
        return TextView(ctx)
