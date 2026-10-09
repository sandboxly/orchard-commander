"""Generate the app icons from packaging/icons/source.png.

    uv run python packaging/make_icons.py

Writes:
    packaging/icons/app.icns                        macOS app bundle (needs macOS: iconutil)
    packaging/icons/app.ico                         Windows executable
    src/orchard_commander/resources/icon.png        window icon at run time (512 px)
    src/orchard_commander/resources/icon-macos.png  Dock icon at run time on macOS (512 px)

The artwork is cropped to its visible pixels and centred on the macOS icon
grid (about 10% margin), so it lines up with other apps in the Dock.

On macOS the folder sits on a dark rounded square: since macOS 26, Finder and
the Dock put app icons of any other shape on a gray plate. The running app
uses the same picture, so the Dock icon doesn't change when it starts.
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

ROOT = Path(__file__).resolve().parent.parent
ICONS = ROOT / "packaging" / "icons"
CANVAS = 1024
ARTWORK = 824  # Apple's icon grid: content area of a 1024 px icon
NIGHT = ((58, 52, 48), (24, 21, 19))  # macOS icon background, top to bottom
FOLDER_ON_PLATE = 0.80  # the folder's size relative to the rounded square


def centered(source: Path) -> Image.Image:
    img = Image.open(source).convert("RGBA")
    bbox = img.getchannel("A").point(lambda a: 255 if a > 16 else 0).getbbox()
    art = img.crop(bbox)
    scale = ARTWORK / max(art.size)
    art = art.resize((round(art.width * scale), round(art.height * scale)), Image.Resampling.LANCZOS)
    canvas = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
    canvas.alpha_composite(art, ((CANVAS - art.width) // 2, (CANVAS - art.height) // 2))
    return canvas


CORNER_RADIUS = 185.4  # Apple's macOS icon template, for the 824 px rounded square

# One corner of a "continuous" rounded rectangle (Apple's icon shape, not circular
# arcs): Bézier segments as multiples of the radius, measured from the top-right
# corner — (dx from the right edge, dy from the top edge).
_CORNER = [
    ((1.08849323, 0.0), (0.86840689, 0.02355647), (0.66993427, 0.06549600)),
    ((0.37282392, 0.15890747), (0.15890747, 0.37282392), (0.06549600, 0.66993427)),
    ((0.02355647, 0.86840689), (0.0, 1.08849323), (0.0, 1.52866471)),
]


def rounded_square(size: int, radius: float = CORNER_RADIUS) -> Image.Image:
    """An antialiased mask of the macOS icon shape, `size` px square."""
    k = 4  # supersampling
    s, r = size * k, radius * size / ARTWORK * k

    def bezier(p0, p1, p2, p3, steps=24):
        for i in range(1, steps + 1):
            t = i / steps
            u = 1 - t
            yield tuple(u**3 * a + 3 * u * u * t * b + 3 * u * t * t * c + t**3 * d for a, b, c, d in zip(p0, p1, p2, p3))

    # The top-right corner in (x, y), then the same rotated to the other three corners.
    corner = [(s - 1.52866471 * r, 0.0)]
    for c1, c2, end in _CORNER:
        p0 = corner[-1]
        corner += bezier(p0, (s - c1[0] * r, c1[1] * r), (s - c2[0] * r, c2[1] * r), (s - end[0] * r, end[1] * r))
    points = []
    for rotate in range(4):
        for x, y in corner:
            for _ in range(rotate):
                x, y = s - y, x  # 90° clockwise about the centre
            points.append((x, y))
    mask = Image.new("L", (s, s), 0)
    ImageDraw.Draw(mask).polygon(points, fill=255)
    return mask.resize((size, size), Image.Resampling.LANCZOS)


def macos_icon(master: Image.Image) -> Image.Image:
    """The artwork on the Night rounded square, on the 1024 px macOS icon grid."""
    plate = Image.new("RGBA", (ARTWORK, ARTWORK))
    draw = ImageDraw.Draw(plate)
    (r0, g0, b0), (r1, g1, b1) = NIGHT
    for y in range(ARTWORK):
        t = y / (ARTWORK - 1)
        draw.line([(0, y), (ARTWORK, y)], fill=(round(r0 + (r1 - r0) * t), round(g0 + (g1 - g0) * t),
                                                round(b0 + (b1 - b0) * t), 255))
    art = master.crop(master.getchannel("A").getbbox())
    scale = ARTWORK * FOLDER_ON_PLATE / max(art.size)
    art = art.resize((round(art.width * scale), round(art.height * scale)), Image.Resampling.LANCZOS)
    pos = ((ARTWORK - art.width) // 2, (ARTWORK - art.height) // 2 + ARTWORK // 50)
    shadow = Image.new("RGBA", plate.size, (0, 0, 0, 0))
    shadow.paste((0, 0, 0, 255), (pos[0], pos[1] + ARTWORK // 40), art.getchannel("A").point(lambda a: a * 110 // 255))
    plate.alpha_composite(shadow.filter(ImageFilter.GaussianBlur(ARTWORK // 50)))
    plate.alpha_composite(art, pos)
    plate.putalpha(rounded_square(ARTWORK))
    canvas = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
    margin = (CANVAS - ARTWORK) // 2
    canvas.alpha_composite(plate, (margin, margin))
    return canvas


def main() -> int:
    master = centered(ICONS / "source.png")
    mac = macos_icon(master)
    master.save(ICONS / "app-1024.png")
    resources = ROOT / "src" / "orchard_commander" / "resources"
    resources.mkdir(parents=True, exist_ok=True)
    master.resize((512, 512), Image.Resampling.LANCZOS).save(resources / "icon.png")
    mac.resize((512, 512), Image.Resampling.LANCZOS).save(resources / "icon-macos.png")
    master.save(ICONS / "app.ico", sizes=[(s, s) for s in (16, 24, 32, 48, 64, 128, 256)])

    if sys.platform == "darwin":
        with tempfile.TemporaryDirectory() as tmp:
            iconset = Path(tmp) / "app.iconset"
            iconset.mkdir()
            for size in (16, 32, 128, 256, 512):
                for scale in (1, 2):
                    px = size * scale
                    name = f"icon_{size}x{size}{'@2x' if scale == 2 else ''}.png"
                    mac.resize((px, px), Image.Resampling.LANCZOS).save(iconset / name)
            subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(ICONS / "app.icns")], check=True)
    elif not (ICONS / "app.icns").exists():
        print("note: app.icns can only be generated on macOS (iconutil)")
    for f in sorted(ICONS.iterdir()):
        print(f"{f.relative_to(ROOT)}  {f.stat().st_size // 1024} KB")
    print(f"{(resources / 'icon.png').relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
