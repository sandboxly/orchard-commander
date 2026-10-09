"""The previewers that ship with Orchard Commander, in tie-break order."""

from orchard_commander.preview.builtin.hexdump import HexPreviewer
from orchard_commander.preview.builtin.image import ImagePreviewer
from orchard_commander.preview.builtin.listing import ListingPreviewer
from orchard_commander.preview.builtin.media import MediaPreviewer
from orchard_commander.preview.builtin.pdf import PdfPreviewer
from orchard_commander.preview.builtin.text import MarkdownPreviewer, TextPreviewer

PREVIEWERS = [ListingPreviewer, ImagePreviewer, PdfPreviewer, MediaPreviewer, MarkdownPreviewer, TextPreviewer, HexPreviewer]
