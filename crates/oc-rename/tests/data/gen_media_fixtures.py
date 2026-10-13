"""Small media fixtures for the container readers, and what the Python version reads from them.

Run with the Python version's virtualenv (mutagen, hachoir, orchard_commander) and ffmpeg on PATH:

    .venv/bin/python gen_media_fixtures.py <this directory>

Writes the files below plus python_media_reference.json: provider id -> fields, per file.
"""
import json
import struct
import subprocess
import sys
from pathlib import Path

from mutagen.dsf import DSF
from mutagen.id3 import TALB, TCON, TDRC, TIT2, TPE1, TPE2, TRCK

from orchard_commander import metadata

out = Path(sys.argv[1])
SMALL = "testsrc=size=96x64:rate=10:duration=3"
TONE = "sine=frequency=440:duration=3:sample_rate=8000"


def ff(*args):
    subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", *args], check=True)


ff("-f", "lavfi", "-i", SMALL, "-f", "lavfi", "-i", TONE, "-c:v", "wmv2", "-b:v", "20k", "-c:a", "wmav2",
   "-b:a", "24k", "-metadata", "title=My: Film", "-metadata", "author=Some/One", str(out / "clip.wmv"))
ff("-f", "lavfi", "-i", SMALL, "-c:v", "flv1", "-b:v", "20k", str(out / "clip.flv"))
ff("-f", "lavfi", "-i", SMALL, "-c:v", "mpeg2video", "-b:v", "50k", str(out / "clip.mpg"))
ff("-f", "lavfi", "-i", "testsrc=size=96x64:rate=30000/1001:duration=3", "-c:v", "mpeg2video", "-b:v", "50k",
   "-f", "mpegts", str(out / "clip.ts"))
ff("-f", "lavfi", "-i", "testsrc=size=100x58:rate=25:duration=3", "-c:v", "libx264", "-preset", "ultrafast",
   "-f", "mpegts", "-mpegts_m2ts_mode", "1", str(out / "clip.m2ts"))
ff("-f", "lavfi", "-i", TONE, "-c:a", "wmav2", "-b:a", "24k", "-metadata", "title=Tone: One",
   "-metadata", "artist=AC/DC", "-metadata", "album=Live", "-metadata", "date=1992", "-metadata", "track=3/12",
   "-metadata", "genre=Rock", str(out / "tone.wma"))

# DSF (ffmpeg can't write it): stereo DSD64 claiming 3 s, one block of samples, then an ID3v2 tag via mutagen.
dsf = out / "tone.dsf"
data = b"\x69" * 4096 * 2
fmt = struct.pack("<4sQIIIIIIQII", b"fmt ", 52, 1, 0, 2, 2, 2822400, 1, 2822400 * 3, 4096, 0)
chunk = struct.pack("<4sQ", b"data", 12 + len(data)) + data
dsf.write_bytes(struct.pack("<4sQQQ", b"DSD ", 28, 28 + 52 + len(chunk), 0) + fmt + chunk)
f = DSF(dsf)
f.add_tags()
f.tags.add(TIT2(encoding=3, text="Tone: Two"))
f.tags.add(TPE1(encoding=3, text="AC/DC"))
f.tags.add(TPE2(encoding=3, text="Various"))
f.tags.add(TALB(encoding=3, text="Live"))
f.tags.add(TRCK(encoding=3, text="7/9"))
f.tags.add(TDRC(encoding=3, text="1992-10-27"))
f.tags.add(TCON(encoding=3, text="Rock"))
f.save()

reference = {}
for name in ["clip.wmv", "clip.flv", "clip.mpg", "clip.ts", "clip.m2ts", "tone.wma", "tone.dsf"]:
    path = out / name
    reference[name] = {p.id: p.read(path) for p in metadata.providers() if p.handles(name)}
(out / "python_media_reference.json").write_text(json.dumps(reference, indent=1, sort_keys=True) + "\n")
print(json.dumps(reference, indent=1, sort_keys=True))
