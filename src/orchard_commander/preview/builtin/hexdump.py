"""Any file: facts and a hex dump of the beginning. The last-resort previewer."""

from __future__ import annotations

from orchard_commander import fs
from orchard_commander.preview import content
from orchard_commander.preview.api import (
    Access,
    FileInfo,
    LoadContext,
    Loaded,
    Previewer,
    PreviewView,
    Score,
    ViewContext,
)
from orchard_commander.preview.builtin.textview import TextData, TextView
from orchard_commander.preview.source import PreviewSource


class HexPreviewer(Previewer):
    id = "hex"
    title = "Hex"
    access = Access.HEAD

    def score(self, f: FileInfo) -> int:
        return Score.NO if f.is_dir else Score.FALLBACK

    def load(self, src: PreviewSource, ctx: LoadContext) -> Loaded:
        if src.size == 0:
            return Loaded(TextData("(empty file)"), "0 B")
        if (path := src.direct_path) is not None:
            facts = content.describe(path)  # includes permissions, link target
        else:
            facts = content.describe_entry(src.entry, src.location)
        lines = "\n".join(f"{k + ':':<13}{v}" for k, v in facts)
        dump = content.hexdump(src.head(content.HEXDUMP_BYTES))
        return Loaded(TextData(f"{lines}\n\n{dump}"), f"{fs.human_size(src.size)} · binary")

    def create_view(self, ctx: ViewContext) -> PreviewView:
        return TextView(ctx)
