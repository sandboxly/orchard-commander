"""Generate the app icons from packaging/icons/source.png.

    uv run python packaging/make_icons.py

Writes:
    packaging/icons/app.icns                  macOS app bundle (needs macOS: iconutil)
    packaging/icons/app.ico                   Windows executable
    src/orchard_commander/resources/icon.png  window icon at run time (512 px)

The artwork is cropped to its visible pixels and centred on the macOS icon
grid (about 10% margin), so it lines up with other apps in the Dock.
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
ICONS = ROOT / "packaging" / "icons"
CANVAS = 1024
ARTWORK = 824  # Apple's icon grid: content area of a 1024 px icon


def centered(source: Path) -> Image.Image:
    img = Image.open(source).convert("RGBA")
    bbox = img.getchannel("A").point(lambda a: 255 if a > 16 else 0).getbbox()
    art = img.crop(bbox)
    scale = ARTWORK / max(art.size)
    art = art.resize((round(art.width * scale), round(art.height * scale)), Image.Resampling.LANCZOS)
    canvas = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
    canvas.alpha_composite(art, ((CANVAS - art.width) // 2, (CANVAS - art.height) // 2))
    return canvas


def main() -> int:
    master = centered(ICONS / "source.png")
    master.save(ICONS / "app-1024.png")
    resources = ROOT / "src" / "orchard_commander" / "resources"
    resources.mkdir(parents=True, exist_ok=True)
    master.resize((512, 512), Image.Resampling.LANCZOS).save(resources / "icon.png")
    master.save(ICONS / "app.ico", sizes=[(s, s) for s in (16, 24, 32, 48, 64, 128, 256)])

    if sys.platform == "darwin":
        with tempfile.TemporaryDirectory() as tmp:
            iconset = Path(tmp) / "app.iconset"
            iconset.mkdir()
            for size in (16, 32, 128, 256, 512):
                for scale in (1, 2):
                    px = size * scale
                    name = f"icon_{size}x{size}{'@2x' if scale == 2 else ''}.png"
                    master.resize((px, px), Image.Resampling.LANCZOS).save(iconset / name)
            subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(ICONS / "app.icns")], check=True)
    elif not (ICONS / "app.icns").exists():
        print("note: app.icns can only be generated on macOS (iconutil)")
    for f in sorted(ICONS.iterdir()):
        print(f"{f.relative_to(ROOT)}  {f.stat().st_size // 1024} KB")
    print(f"{(resources / 'icon.png').relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
