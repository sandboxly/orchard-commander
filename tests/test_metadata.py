"""File metadata for [=field] rename placeholders: audio tags, video properties, photo EXIF."""

from __future__ import annotations

import shutil
import struct
from pathlib import Path

import pytest

from orchard_commander import metadata, rename, vfs
from orchard_commander.rename import Rule

DATA = Path(__file__).parent / "data"


@pytest.fixture
def song(tmp_path: Path) -> Path:
    from mutagen.easyid3 import EasyID3

    path = tmp_path / "track.mp3"
    shutil.copy(DATA / "tone-then-silence.mp3", path)
    tags = EasyID3()
    tags.update({"artist": "AC/DC", "album": "Live: Volume 1", "title": "Tone", "tracknumber": "3/12",
                 "date": "1992-10-27", "genre": "Rock"})
    tags.save(path)
    return path


def exif_jpeg(path: Path) -> Path:
    """A real JPEG (via Qt) with an EXIF block: make, model, date taken, ISO, focal length."""
    from PySide6.QtCore import QBuffer, QByteArray, QIODevice
    from PySide6.QtGui import QColor, QImage

    image = QImage(64, 48, QImage.Format.Format_RGB32)
    image.fill(QColor("orange"))
    data = QByteArray()
    buffer = QBuffer(data)
    buffer.open(QIODevice.OpenModeFlag.WriteOnly)
    image.save(buffer, "JPEG")
    jpeg = bytes(data)

    def ifd(entries: list[tuple[int, int, int, bytes]], offset: int) -> tuple[bytes, bytes]:
        """entries: (tag, type, count, value bytes); returns (ifd, extra data placed after it)."""
        head = struct.pack("<H", len(entries))
        extra = b""
        extra_at = offset + 2 + 12 * len(entries) + 4
        for tag, kind, count, value in entries:
            if len(value) <= 4:
                head += struct.pack("<HHI", tag, kind, count) + value.ljust(4, b"\0")
            else:
                head += struct.pack("<HHII", tag, kind, count, extra_at + len(extra))
                extra += value
        return head + struct.pack("<I", 0), extra

    ascii_ = lambda s: s.encode() + b"\0"  # noqa: E731
    make, model = ascii_("Canon"), ascii_("Canon EOS R6")
    ifd0_len = 2 + 12 * 3 + 4 + len(make) + len(model)
    exif_at = 8 + ifd0_len
    ifd0, extra0 = ifd([(0x010F, 2, len(make), make), (0x0110, 2, len(model), model),
                        (0x8769, 4, 1, struct.pack("<I", exif_at))], 8)
    taken = ascii_("2026:03:07 09:05:04")
    exif, extra1 = ifd([(0x8827, 3, 1, struct.pack("<H", 400)), (0x9003, 2, len(taken), taken),
                        (0x920A, 5, 1, struct.pack("<II", 50, 1))], exif_at)
    tiff = b"II*\0" + struct.pack("<I", 8) + ifd0 + extra0 + exif + extra1
    app1 = b"Exif\0\0" + tiff
    path.write_bytes(jpeg[:2] + b"\xff\xe1" + struct.pack(">H", len(app1) + 2) + app1 + jpeg[2:])
    return path


def read(path: Path) -> dict[str, str]:
    return metadata.read(vfs.local(path))


def test_audio_tags(song: Path):
    meta = read(song)
    assert meta["artist"] == "AC-DC"  # "/" can't be in a file name
    assert meta["album"] == "Live - Volume 1"
    assert meta["track"] == "03" and meta["tracktotal"] == "12" and meta["year"] == "1992"
    assert meta["duration"] == "2s" and meta["bitrate"] == "32"


def test_video_properties():
    meta = read(DATA / "clip-160x90.mp4")
    assert meta["width"] == "160" and meta["height"] == "90" and meta["quality"] == "90p"
    assert meta["resolution"] == "160x90" and meta["duration"] == "2s"
    assert meta["title"] == "Test Clip" and meta["year"] == "2024"  # MP4 tags


def test_photo_exif(tmp_path: Path):
    meta = read(exif_jpeg(tmp_path / "IMG_0001.jpg"))
    assert meta["taken"] == "2026-03-07" and meta["taken_time"] == "09.05.04" and meta["taken_year"] == "2026"
    assert meta["camera"] == "Canon EOS R6" and meta["make"] == "Canon" and meta["iso"] == "400"
    assert meta["focal"] == "50"
    assert meta["width"] == "64" and meta["height"] == "48"  # from the image itself


def test_unknown_and_unreadable(tmp_path: Path):
    (tmp_path / "notes.txt").write_text("hi")
    (tmp_path / "broken.mp3").write_bytes(b"not really an mp3")
    assert read(tmp_path / "notes.txt") == {}
    assert read(tmp_path / "broken.mp3") == {}
    with pytest.raises(metadata.MetadataUnavailable):
        metadata.read(vfs.local(tmp_path / "broken.mp3"), online_only=True)


# -- placeholders ---------------------------------------------------------------------------------


def with_meta(path: Path) -> rename.Item:
    location = vfs.local(path)
    item = rename.item_for(location, location.stat())
    return rename.Item(**{**item.__dict__, "meta": metadata.read(location)})


def test_rename_with_tags(song: Path, tmp_path: Path):
    item = with_meta(song)
    rule = Rule(name_mask="[=track] - [=artist] - [=title] ([=year])")
    assert rename.new_name(item, 0, rule) == "03 - AC-DC - Tone (1992).mp3"
    assert rename.new_name(item, 0, Rule(name_mask="[U][=artist:1-2]")) == "AC.mp3"
    clip = with_meta(DATA / "clip-160x90.mp4")
    assert rename.new_name(clip, 0, Rule(name_mask="[=title] [[][=quality][]]")) == "Test Clip [90p].mp4"


def test_missing_values_skip_or_fall_back(song: Path, tmp_path: Path):
    (tmp_path / "untagged.mp3").write_bytes((DATA / "tone-then-silence.mp3").read_bytes())
    items = [with_meta(song), with_meta(tmp_path / "untagged.mp3")]
    plan = rename.plan(items, Rule(name_mask="[=artist] - [N]"))
    assert [c.new for c in plan.changes] == ["AC-DC - track.mp3", "untagged.mp3"]
    assert plan.changes[1].note == "skipped: no artist" and plan.can_run  # skipped, not a blocker
    fallback = rename.plan(items, Rule(name_mask="[=artist|Unknown] - [N]"))
    assert fallback.changes[1].new == "Unknown - untagged.mp3"


def test_metadata_pending_and_bad_fields(song: Path):
    location = vfs.local(song)
    unread = rename.item_for(location, location.stat())  # meta not read yet
    plan = rename.plan([unread], Rule(name_mask="[=artist]"))
    assert plan.pending == 1 and not plan.can_run
    assert "Unknown field" in rename.plan([unread], Rule(name_mask="[=nonsense]")).mask_error
    assert rename.plan([unread], Rule(name_mask="[=artist:0-2]")).mask_error  # bad range, found early


def test_add_on_providers(tmp_path: Path, monkeypatch):
    class Pages(metadata.MetadataProvider):
        id = "pages"
        label = "Documents"
        fields = {"pages": "Number of pages"}

        def handles(self, name):
            return name.endswith(".doc")

        def read(self, path):
            return {"pages": str(len(path.read_text().split("\f")))}

    monkeypatch.setattr(metadata, "_providers", None)
    metadata.register(Pages())
    (tmp_path / "x.doc").write_text("a\fb\fc")
    assert with_meta(tmp_path / "x.doc").meta == {"pages": "3"}
    assert rename.new_name(with_meta(tmp_path / "x.doc"), 0, Rule(name_mask="[N] ([=pages]p)")) == "x (3p).doc"
