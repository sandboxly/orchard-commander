"""File metadata for multi-rename placeholders like [=artist] or [=height].

Total Commander gets these from content plugins ([=plugin.field]); here they
come from `MetadataProvider`s — built-ins for audio tags (mutagen), photos
(EXIF via exifread) and video/image properties (hachoir), plus add-ons from
the `orchard_commander.metadata` entry point:

    class MyProvider(MetadataProvider):
        id = "mine"
        label = "My format"
        fields = {"page_count": "Number of pages"}

        def handles(self, name): return name.lower().endswith(".foo")
        def read(self, path): return {"page_count": "12"}

Field names are shared: several providers may offer "title" or "width"; for
a file, the first provider that handles it and has a value wins. Values are
strings, already cleaned up for use in file names.
"""

from __future__ import annotations

import logging
import re
import sys
import threading
from abc import ABC, abstractmethod
from importlib.metadata import entry_points
from pathlib import Path

from orchard_commander import fs, vfs

log = logging.getLogger(__name__)
ENTRY_POINT_GROUP = "orchard_commander.metadata"

AUDIO = {".mp3", ".flac", ".m4a", ".m4b", ".aac", ".ogg", ".oga", ".opus", ".wma", ".wav", ".aiff", ".aif",
         ".ape", ".wv", ".mpc", ".dsf"}
VIDEO = {".mp4", ".m4v", ".mov", ".mkv", ".webm", ".avi", ".wmv", ".asf", ".flv", ".mpg", ".mpeg", ".3gp", ".ts"}
IMAGES = {".jpg", ".jpeg", ".png", ".gif", ".bmp", ".tif", ".tiff", ".webp", ".heic", ".heif", ".dng", ".cr2",
          ".nef", ".arw", ".orf", ".rw2"}


class MetadataUnavailable(Exception):
    """The file's metadata can't be read (e.g. a cloud-only file); the message says why."""


class MetadataProvider(ABC):
    id: str = ""
    label: str = ""  # for the Insert menu
    fields: dict[str, str] = {}  # field name -> description

    @abstractmethod
    def handles(self, name: str) -> bool:
        """Whether this provider reads files with this name (by extension, cheap)."""

    @abstractmethod
    def read(self, path: Path) -> dict[str, str]:
        """All fields it finds in the local file `path` (blocking; on a worker thread)."""


def suffix(name: str) -> str:
    return Path(name).suffix.lower()


# ---------------------------------------------------------------------------
# Value formatting
# ---------------------------------------------------------------------------

_WINDOWS_BAD = re.compile(r'[<>:"\\|?*]')


def clean(value: object) -> str:
    """A metadata value made safe for a file name: one line, no path separators."""
    text = " ".join(str(value).split())
    text = text.replace(": ", " - ").replace("/", "-").replace("\\", "-").replace(":", "-").replace("\0", "")
    if sys.platform == "win32":
        text = _WINDOWS_BAD.sub("_", text)
    return text.strip(" .") if sys.platform == "win32" else text.strip()


def duration(seconds: float) -> str:
    """'3m25s', '1h02m03s', '45s'."""
    total = int(round(seconds))
    h, rest = divmod(total, 3600)
    m, s = divmod(rest, 60)
    if h:
        return f"{h}h{m:02d}m{s:02d}s"
    return f"{m}m{s:02d}s" if m else f"{s}s"


def _dimensions(width: object, height: object) -> dict[str, str]:
    try:
        w, h = int(str(width)), int(str(height))
    except ValueError:
        return {}
    return {"width": str(w), "height": str(h), "resolution": f"{w}x{h}"}


def _year(text: str) -> str:
    match = re.search(r"\b(\d{4})\b", text)
    return match.group(1) if match else ""


def _number(text: str) -> tuple[str, str]:
    """'3/12' -> ('3', '12'); '03' -> ('3', '')."""
    number, _, total = text.partition("/")
    number, total = number.strip(), total.strip()
    return (str(int(number)) if number.isdigit() else number), (str(int(total)) if total.isdigit() else "")


# ---------------------------------------------------------------------------
# Built-ins
# ---------------------------------------------------------------------------


class AudioTags(MetadataProvider):
    id = "audio"
    label = "Audio tags"
    fields = {
        "artist": "Artist",
        "albumartist": "Album artist",
        "album": "Album",
        "title": "Title",
        "track": "Track number, 2 digits (03)",
        "tracktotal": "Number of tracks",
        "disc": "Disc number",
        "year": "Year",
        "genre": "Genre",
        "composer": "Composer",
        "duration": "Length (3m25s)",
        "bitrate": "Bit rate in kbit/s",
    }

    def handles(self, name: str) -> bool:
        return suffix(name) in AUDIO or suffix(name) in {".mp4", ".m4v", ".mov"}  # MP4 tags: also on videos

    def read(self, path: Path) -> dict[str, str]:
        import mutagen

        f = mutagen.File(path, easy=True)
        if f is None:
            return {}
        out: dict[str, str] = {}
        tags = f.tags or {}

        def tag(*names: str, raw: bool = False) -> str:
            for n in names:
                try:
                    values = tags.get(n)
                except (KeyError, ValueError):
                    values = None
                if values:
                    value = str(values[0] if isinstance(values, list) else values)
                    return value.strip() if raw else clean(value)
            return ""

        for name in ("artist", "albumartist", "album", "title", "genre", "composer"):
            out[name] = tag(name)
        track, total = _number(tag("tracknumber", raw=True))
        out["tracktotal"] = tag("tracktotal", "totaltracks") or total
        if track.isdigit():
            width = 3 if out["tracktotal"].isdigit() and int(out["tracktotal"]) >= 100 else 2
            out["track"] = track.zfill(width)
        out["disc"] = _number(tag("discnumber", raw=True))[0]
        out["year"] = _year(tag("date", "originaldate", "year"))
        info = getattr(f, "info", None)
        if info is not None and getattr(info, "length", 0):
            out["duration"] = duration(info.length)
        if info is not None and getattr(info, "bitrate", 0):
            out["bitrate"] = str(round(info.bitrate / 1000))
        if suffix(path.name) in VIDEO:  # MP4 video: only the tags, not the audio properties
            out.pop("bitrate", None)
        return {k: v for k, v in out.items() if v}


class MediaProperties(MetadataProvider):
    """Video and image properties from the container/file headers (hachoir)."""

    id = "media"
    label = "Video and image properties"
    fields = {
        "width": "Width in pixels",
        "height": "Height in pixels (vertical resolution)",
        "resolution": "Width x height (1920x1080)",
        "quality": "Height as 1080p / 720p (videos)",
        "duration": "Length (1h02m03s)",
        "fps": "Frames per second (videos)",
        "title": "Title (videos)",
    }

    def handles(self, name: str) -> bool:
        return suffix(name) in VIDEO or suffix(name) in IMAGES

    def read(self, path: Path) -> dict[str, str]:
        from hachoir.core import config as hachoir_config
        from hachoir.metadata import extractMetadata
        from hachoir.parser import createParser

        hachoir_config.quiet = True
        parser = createParser(str(path))
        if parser is None:
            return {}
        with parser:
            try:
                meta = extractMetadata(parser)
            except Exception as exc:  # hachoir raises all kinds of things on odd files
                log.debug("hachoir: %s: %s", path.name, exc)
                return {}
        if meta is None:
            return {}

        def get(key: str):
            try:
                return meta.get(key) if meta.has(key) else None
            except (ValueError, KeyError):
                return None

        out: dict[str, str] = {}
        width, height = get("width"), get("height")
        if width is None:  # some containers keep the size per stream ("video[1]")
            for group in getattr(meta, "iterGroups", lambda: [])():
                if group.has("width") and group.has("height"):
                    width, height = group.get("width"), group.get("height")
                    break
        if width is not None and height is not None:
            out.update(_dimensions(width, height))
            if suffix(path.name) in VIDEO:
                out["quality"] = f"{out['height']}p"
        length = get("duration")
        if length is not None:
            out["duration"] = duration(length.total_seconds())
        rate = get("frame_rate")
        if rate:
            out["fps"] = f"{float(rate):g}"
        title = get("title")
        if title and suffix(path.name) in VIDEO:
            out["title"] = clean(title)
        return {k: v for k, v in out.items() if v}


class PhotoExif(MetadataProvider):
    id = "exif"
    label = "Photo (EXIF)"
    fields = {
        "taken": "Date taken (2026-03-07)",
        "taken_time": "Time taken (09.05.04)",
        "taken_year": "Year taken",
        "taken_month": "Month taken (03)",
        "camera": "Camera make and model",
        "make": "Camera make",
        "model": "Camera model",
        "lens": "Lens",
        "iso": "ISO",
        "focal": "Focal length in mm",
        "width": "Width in pixels",
        "height": "Height in pixels",
        "resolution": "Width x height",
    }

    def handles(self, name: str) -> bool:
        return suffix(name) in IMAGES

    def read(self, path: Path) -> dict[str, str]:
        import exifread

        logging.getLogger("exifread").setLevel(logging.ERROR)
        with open(path, "rb") as f:
            tags = exifread.process_file(f, details=False)
        if not tags:
            return {}

        def tag(*names: str) -> str:
            for n in names:
                if n in tags:
                    return str(tags[n]).strip()
            return ""

        out: dict[str, str] = {}
        taken = tag("EXIF DateTimeOriginal", "EXIF DateTimeDigitized", "Image DateTime")
        match = re.match(r"(\d{4}):(\d{2}):(\d{2})[ T](\d{2}):(\d{2}):(\d{2})", taken)
        if match:
            y, mo, d, h, mi, s = match.groups()
            out.update(taken=f"{y}-{mo}-{d}", taken_time=f"{h}.{mi}.{s}", taken_year=y, taken_month=mo)
        make, model = clean(tag("Image Make")), clean(tag("Image Model"))
        out["make"], out["model"] = make, model
        # Many cameras repeat the make in the model ("Canon" + "Canon EOS R6").
        out["camera"] = model if make and model.lower().startswith(make.lower().split()[0]) else f"{make} {model}".strip()
        out["lens"] = clean(tag("EXIF LensModel"))
        out["iso"] = tag("EXIF ISOSpeedRatings", "EXIF PhotographicSensitivity")
        focal = tag("EXIF FocalLength")
        if focal:
            try:
                num, _, den = focal.partition("/")
                out["focal"] = f"{float(num) / float(den or 1):g}"
            except ValueError:
                pass
        out.update(_dimensions(tag("EXIF ExifImageWidth"), tag("EXIF ExifImageLength")))
        return {k: v for k, v in out.items() if v}


BUILTIN: list[type[MetadataProvider]] = [AudioTags, MediaProperties, PhotoExif]  # first with a value wins


# ---------------------------------------------------------------------------
# Registry
# ---------------------------------------------------------------------------

_lock = threading.Lock()
_providers: list[MetadataProvider] | None = None


def providers() -> list[MetadataProvider]:
    global _providers
    with _lock:
        if _providers is not None:
            return list(_providers)
        found: list[MetadataProvider] = [cls() for cls in BUILTIN]
        for ep in entry_points(group=ENTRY_POINT_GROUP):
            try:
                provider = ep.load()
                found.insert(0, provider() if isinstance(provider, type) else provider)  # add-ons first
            except Exception:  # a broken add-on must not break renaming
                log.exception("could not load metadata provider %s from %s", ep.name, ep.value)
        _providers = found
        return list(found)


def register(provider: MetadataProvider) -> None:
    """Add a provider (ahead of the built-ins). For tests and embedding."""
    global _providers
    current = providers()
    with _lock:
        _providers = [provider, *(p for p in current if p.id != provider.id)]


def all_fields() -> dict[str, str]:
    """Every field name any provider offers -> its description (first provider's)."""
    out: dict[str, str] = {}
    for provider in providers():
        for name, description in provider.fields.items():
            out.setdefault(name, description)
    return out


def handled(name: str) -> bool:
    return any(p.handles(name) for p in providers())


def read(location: vfs.VPath, *, is_dir: bool = False, online_only: bool = False) -> dict[str, str]:
    """All metadata fields for a file (blocking). Raises MetadataUnavailable.

    Files on servers are downloaded first (as for opening them); cloud-only
    placeholders are not read — that would download them.
    """
    candidates = [p for p in providers() if p.handles(location.name)]
    if not candidates:
        return {}
    if is_dir:
        return {}
    if online_only:
        raise MetadataUnavailable("cloud-only file, not downloaded")
    try:
        path = location.local_path() or location.materialize()
    except (OSError, fs.OperationError) as exc:
        raise MetadataUnavailable(str(exc)) from exc
    out: dict[str, str] = {}
    for provider in candidates:
        try:
            values = provider.read(Path(path))
        except Exception as exc:  # corrupt files: no metadata from this provider
            log.debug("%s: %s: %s", provider.id, location.name, exc)
            continue
        for name, value in values.items():
            if value and name not in out:
                out[name] = value
    return out
