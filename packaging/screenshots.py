"""Screenshots for the website, made from an invented home folder.

    uv run python packaging/screenshots.py [OUT_DIR]     (default: site/screenshots)

Everything shown is generated here — photos painted with Qt, synthesized
music with made-up tags, a PDF report, a small web project, archives — in a
temporary folder that HOME points to, so panels show "~/Photos/…" and no
real file of the machine appears. Needs ffmpeg for the music. Renders at 2x.
"""

from __future__ import annotations

import math
import os
import random
import shutil
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# Resolved (macOS: /var is /private/var), so the shell's prompt can shorten it to ~ too.
DEMO = Path(tempfile.mkdtemp(prefix="orchard-demo-")).resolve() / "alex"
os.environ["HOME"] = str(DEMO)  # panels show ~/…; the terminal's shell starts here
os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")
os.environ["QT_SCALE_FACTOR"] = "2"
sys.path.insert(0, str(ROOT / "src"))

from PySide6.QtCore import QEventLoop, QMarginsF, QPointF, QRectF, Qt, QTimer  # noqa: E402
from PySide6.QtGui import (  # noqa: E402
    QColor, QFont, QImage, QLinearGradient, QPageLayout, QPageSize, QPainter, QPainterPath, QPdfWriter,
    QRadialGradient, QTextDocument,
)
from PySide6.QtWidgets import QApplication  # noqa: E402

WIDTH, HEIGHT = 1440, 860
FONT = "Fira Code"


def wait(ms: int) -> None:
    loop = QEventLoop()
    QTimer.singleShot(ms, loop.quit)
    loop.exec()


def wait_until(predicate, timeout_ms: int = 8000) -> None:
    for _ in range(timeout_ms // 50):
        if predicate():
            return
        wait(50)


# ---------------------------------------------------------------------------
# The invented home folder
# ---------------------------------------------------------------------------

def landscape(path: Path, seed: int) -> None:
    """A painted landscape: sky, sun, layered hills, water."""
    rnd = random.Random(seed)
    w, h = 1600, 1067
    img = QImage(w, h, QImage.Format.Format_RGB32)
    p = QPainter(img)
    p.setRenderHint(QPainter.RenderHint.Antialiasing)
    palettes = [
        ("#1d2b53", "#ff7a59", "#ffd29d"), ("#0b3d5c", "#58b4d1", "#f4e4ba"), ("#2d1b4e", "#e0607e", "#ffc07a"),
        ("#103b2f", "#7cc6a4", "#f6f1d1"), ("#3a1c32", "#f28f3b", "#fde2a8"), ("#14213d", "#fca311", "#e5e5e5"),
    ]
    top, mid, glow = palettes[seed % len(palettes)]
    sky = QLinearGradient(0, 0, 0, h * 0.65)
    sky.setColorAt(0, QColor(top))
    sky.setColorAt(0.7, QColor(mid))
    sky.setColorAt(1, QColor(glow))
    p.fillRect(0, 0, w, h, sky)
    sun = QPointF(w * rnd.uniform(0.25, 0.75), h * rnd.uniform(0.32, 0.5))
    halo = QRadialGradient(sun, 260)
    halo.setColorAt(0, QColor(255, 244, 214, 230))
    halo.setColorAt(0.25, QColor(255, 220, 160, 120))
    halo.setColorAt(1, QColor(255, 200, 150, 0))
    p.fillRect(0, 0, w, h, halo)
    p.setBrush(QColor("#fff4d6"))
    p.setPen(Qt.PenStyle.NoPen)
    p.drawEllipse(sun, 70, 70)
    base = QColor(top)
    for layer in range(5):
        y0 = h * (0.48 + layer * 0.08)
        hill = QPainterPath(QPointF(0, h))
        phase, amp = rnd.uniform(0, 6), rnd.uniform(25, 70) * (1.2 - layer * 0.15)
        for x in range(0, w + 20, 20):
            y = y0 - amp * math.sin(x / rnd.uniform(140, 260) + phase) - amp * 0.4 * math.sin(x / 57 + phase * 2)
            hill.lineTo(x, y)
        hill.lineTo(w, h)
        shade = QColor(base).lighter(100 + (4 - layer) * 25)
        shade = QColor.fromHsv(shade.hue(), min(255, shade.saturation() + 20), max(20, 150 - layer * 28))
        p.setBrush(shade)
        p.drawPath(hill)
    water = QLinearGradient(0, h * 0.86, 0, h)
    water.setColorAt(0, QColor(mid).darker(160))
    water.setColorAt(1, QColor(top).darker(130))
    p.fillRect(QRectF(0, h * 0.86, w, h * 0.14), water)
    p.setPen(QColor(255, 240, 210, 90))
    for i in range(40):
        y = h * 0.87 + rnd.uniform(0, h * 0.12)
        x = sun.x() + rnd.uniform(-120, 120)
        p.drawLine(QPointF(x - rnd.uniform(10, 40), y), QPointF(x + rnd.uniform(10, 40), y))
    p.end()
    if not img.save(str(path), "JPEG", 90):
        raise OSError(f"could not write {path} (null image: {img.isNull()})")


def song(path: Path, seed: int, tags: dict[str, str]) -> None:
    """A synthesized track: a chord progression with a pulse, a few seconds of dynamics."""
    rnd = random.Random(seed)
    root = rnd.choice([110, 123.47, 130.81, 146.83, 164.81])
    chords = [(1, 1.25, 1.5), (0.89, 1.12, 1.33), (0.75, 0.94, 1.12), (0.84, 1.0, 1.26)]
    parts = []
    for i, chord in enumerate(chords):
        tones = "+".join(f"sin(2*PI*{root * r:.2f}*t)" for r in chord)
        parts.append(f"between(mod(t,16),{i * 4},{i * 4 + 4})*({tones})")
    pulse = f"(0.55+0.45*abs(sin(PI*{rnd.choice([1, 2])}*t)))"
    swell = "(0.35+0.65*sin(PI*t/40)^2)"
    expr = f"0.16*({'+'.join(parts)})*{pulse}*{swell}+0.05*sin(2*PI*{root / 2:.2f}*t)*{pulse}"
    subprocess.run(["ffmpeg", "-loglevel", "error", "-y", "-f", "lavfi", "-i", f"aevalsrc={expr.replace(',', chr(92) + ',')}:s=44100:d=40",
                    "-ac", "2", "-b:a", "128k", str(path)], check=True)
    from mutagen.easyid3 import EasyID3

    id3 = EasyID3()
    id3.update(tags)
    id3.save(path)


def report_pdf(path: Path) -> None:
    doc = QTextDocument()
    doc.setDefaultFont(QFont("Helvetica", 10))
    rows = "".join(
        f"<tr><td>{q}</td><td align=right>{a}</td><td align=right>{b}</td><td align=right>{c}</td></tr>"
        for q, a, b, c in [("Q1", "412", "1.84 M", "+12 %"), ("Q2", "468", "2.07 M", "+13 %"),
                           ("Q3", "501", "2.31 M", "+7 %"), ("Q4", "577", "2.66 M", "+15 %")]
    )
    doc.setHtml(f"""
      <h1 style="color:#1f4e79">Harbor Lights Studio</h1>
      <h2 style="color:#555">Annual Report 2025</h2>
      <p>This report summarizes the studio's projects, finances and outlook for the coming year.
      Revenue grew in every quarter, driven by three long-term clients and a new line of
      small-business websites.</p>
      <h3>Results by quarter</h3>
      <table border=1 cellspacing=0 cellpadding=6 width=100%>
        <tr style="background:#dde8f3"><th align=left>Quarter</th><th>Projects</th><th>Revenue</th>
        <th>Growth</th></tr>{rows}</table>
      <h3>Highlights</h3>
      <ul><li>Launched the redesigned booking platform for a regional ferry operator.</li>
      <li>Opened a second office in Porto with four new team members.</li>
      <li>Reduced average delivery time from eleven to eight weeks.</li></ul>
      <h3>Outlook</h3>
      <p>For 2026 the studio plans to expand its maintenance contracts and to invest in
      accessibility audits as a separate service. Hiring will focus on design and QA.</p>
      <p style="color:#888">Harbor Lights Studio · Rua das Flores 12 · Lisbon</p>""")
    writer = QPdfWriter(str(path))
    writer.setPageLayout(QPageLayout(QPageSize(QPageSize.PageSizeId.A4), QPageLayout.Orientation.Portrait,
                                     QMarginsF(18, 18, 18, 18)))
    writer.setResolution(150)
    doc.print_(writer)


APP_PY = '''"""Booking API for the ferry timetable site."""

from __future__ import annotations

from dataclasses import dataclass
from datetime import date, time

from flask import Flask, abort, jsonify, request

app = Flask(__name__)


@dataclass(frozen=True)
class Crossing:
    route: str
    day: date
    departs: time
    seats_left: int

    @property
    def sold_out(self) -> bool:
        return self.seats_left <= 0


def crossings_for(route: str, day: date) -> list[Crossing]:
    # TODO: read from the operator's feed instead of the fixture
    return [c for c in FIXTURE if c.route == route and c.day == day]


@app.get("/api/routes/<route>/crossings")
def list_crossings(route: str):
    day = date.fromisoformat(request.args.get("day", date.today().isoformat()))
    found = crossings_for(route, day)
    if not found:
        abort(404, description=f"No crossings on {day:%d %B}")
    return jsonify([
        {"departs": c.departs.isoformat("minutes"), "seats": c.seats_left, "sold_out": c.sold_out}
        for c in found
    ])


@app.post("/api/bookings")
def book():
    payload = request.get_json(force=True)
    # TODO: validate passenger counts against the vessel's capacity
    return jsonify({"status": "reserved", "reference": make_reference(payload)}), 201
'''


def build_home() -> None:
    d = DEMO
    projects = d / "Projects" / "harbor-ferries"
    for sub in ("src", "static/css", "static/img", "tests", "docs"):
        (projects / sub).mkdir(parents=True, exist_ok=True)
    (projects / "src" / "app.py").write_text(APP_PY)
    (projects / "src" / "pricing.py").write_text(
        '"""Fares by route and season."""\n\nSEASONS = {"low": 0.8, "mid": 1.0, "high": 1.35}\n\n\n'
        'def fare(base: float, season: str, children: int = 0) -> float:\n'
        '    # TODO: weekend surcharge\n    return round(base * SEASONS[season] + children * base * 0.5, 2)\n')
    (projects / "tests" / "test_app.py").write_text(
        "from src.app import crossings_for\n\n\ndef test_no_crossings_on_unknown_route():\n"
        "    assert crossings_for('nowhere', None) == []\n")
    (projects / "README.md").write_text(
        "# Harbor Ferries\n\nTimetable and booking site for the river ferries.\n\n## Run it\n\n"
        "```sh\nflask --app src.app run\n```\n\n## Open items\n\n- TODO: accessibility review of the booking form\n"
        "- Seat map for the large vessels\n")
    (projects / "docs" / "deployment.md").write_text("# Deployment\n\nTODO: describe the staging server.\n")
    (projects / "static" / "css" / "site.css").write_text(
        ":root { --sea: #0b3d5c; --sand: #f4e4ba; }\nbody { font-family: system-ui; color: var(--sea); }\n")
    (projects / "pyproject.toml").write_text('[project]\nname = "harbor-ferries"\nversion = "2.3.0"\n')
    (projects / ".gitignore").write_text(".venv/\n__pycache__/\n")
    landscape(projects / "static" / "img" / "hero.jpg", 3)
    (d / "Projects" / "website-relaunch").mkdir(parents=True)
    (d / "Projects" / "website-relaunch" / "notes.md").write_text("# Relaunch\n\n- TODO: new photos\n")
    (d / "Projects" / "budget-2026.csv").write_text("month,income,expenses\nJan,8400,6120\nFeb,7900,5980\n")

    photos = d / "Photos" / "2026-03 Lisbon"
    photos.mkdir(parents=True)
    for i in range(12):
        landscape(photos / f"IMG_{4501 + i}.JPG", i)
    (d / "Photos" / "2025-08 Porto").mkdir(parents=True)
    for i in range(4):
        landscape(d / "Photos" / "2025-08 Porto" / f"DSC0{1201 + i}.jpg", 20 + i)

    album = d / "Music" / "The Lantern Pilots - Night Drive"
    album.mkdir(parents=True)
    titles = ["Harbor Lights", "Slow Tide", "Ferry at Dawn", "Coastline", "Lantern Song", "Home Port"]
    for i, title in enumerate(titles, 1):
        song(album / f"track{i:02d}.mp3", i, {
            "artist": "The Lantern Pilots", "album": "Night Drive", "title": title,
            "tracknumber": f"{i}/{len(titles)}", "date": "2025", "genre": "Ambient"})

    docs = d / "Documents"
    docs.mkdir()
    report_pdf(docs / "Annual Report 2025.pdf")
    (docs / "Invoices").mkdir()
    for n in (231, 232, 233):
        (docs / "Invoices" / f"Invoice-{n:04d}.txt").write_text(f"Invoice {n}\nTotal: {n * 12.5:.2f} EUR\n")
    (docs / "Lease agreement.txt").write_text("Lease agreement\n\nBetween the landlord and the tenant…\n")
    with zipfile.ZipFile(docs / "Lisbon trip.zip", "w", zipfile.ZIP_DEFLATED) as z:
        for f in sorted(photos.iterdir())[:5]:
            z.write(f, f"Lisbon trip/Day 1/{f.name}")
        z.writestr("Lisbon trip/itinerary.md", "# Lisbon\n\n1. Alfama\n2. Belém\n3. Sintra\n")
    downloads = d / "Downloads"
    downloads.mkdir()
    shutil.copy(docs / "Lisbon trip.zip", downloads / "photos-from-maria.zip")
    (downloads / "ferry-timetable-2026.csv").write_text("route,departs\nA,07:15\nA,08:45\n")
    for hidden in (".zshrc", ".config"):
        (d / hidden).mkdir() if hidden == ".config" else (d / hidden).write_text("")


# ---------------------------------------------------------------------------
# Screenshots
# ---------------------------------------------------------------------------

def main() -> int:
    out = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "site" / "screenshots"
    out.mkdir(parents=True, exist_ok=True)
    app = QApplication([])
    app.setStyle("Fusion")
    build_home()

    from orchard_commander import config, rename, search, vfs

    vfs.LocalFS.free_space = lambda self, path: 412 * 1024**3  # an invented disk, not this machine's

    config.CONFIG_FILE = DEMO.parent / "settings.json"
    from orchard_commander.config import Settings
    from orchard_commander.ui import rename_ui
    from orchard_commander.ui.main_window import PREVIEW, MainWindow
    from orchard_commander.ui.style import ThemeManager

    def window(left: Path, right: Path, scheme: str = "gruvbox", size: tuple[int, int] = (WIDTH, HEIGHT)
               ) -> MainWindow:
        settings = Settings(color_scheme=scheme, font_family=FONT, show_hidden=False,
                            terminal_shell="/bin/zsh -f")
        themes = ThemeManager(settings.appearance())
        themes.apply()
        w = MainWindow(settings, themes, left, right)
        w.resize(*size)
        w.show()
        wait(600)
        return w

    def select(w: MainWindow, side, name: str) -> None:
        panel = side.panel
        wait_until(lambda: panel.model.row_of(name) is not None)
        if panel.model.row_of(name) is None:
            raise SystemExit(f"{name} not in {panel.location}: {[e.name for e in panel.entries][:10]} "
                             f"loading={panel.loading} jobs={getattr(panel, '_jobs', '?')} req={panel._request} "
                             f"done={panel._completed} dir_exists={Path(panel.location.path).is_dir()} "
                             f"listing={sorted(os.listdir(panel.location.path))[:3]}")
        panel.setFocus()
        panel.table.set_current_row(panel.model.row_of(name))

    def shot(widget, name: str) -> None:
        save(widget.grab().toImage(), out / f"{name}.webp")

    home = DEMO
    # 1. Photos: files and an image preview
    w = window(home / "Photos" / "2026-03 Lisbon", home / "Photos")
    w.set_mode(w.right, PREVIEW)
    select(w, w.left, "IMG_4503.JPG")
    w.left.panel.model.marked |= {"IMG_4505.JPG", "IMG_4506.JPG", "IMG_4507.JPG"}
    w.left.panel.model.refresh_rows()
    w.left.panel._update_subtitle()
    wait(1500)
    shot(w, "photos")
    w.close()

    # 2. Code with syntax highlighting
    w = window(home / "Projects" / "harbor-ferries" / "src", home / "Projects")
    w.set_mode(w.right, PREVIEW)
    select(w, w.left, "app.py")
    wait(1500)
    shot(w, "code")
    w.close()

    # 3. Find files: results as a folder, a hit previewed
    w = window(home, home / "Documents")
    query = search.Query(roots=(vfs.local(home / "Projects"),), names="*.py;*.md", text="TODO")
    results = w.start_search(query, w.left)
    wait_until(lambda: not results.running)
    wait(800)
    w.set_mode(w.right, PREVIEW)
    entries = w.left.panel.entries
    select(w, w.left, next(e.name for e in entries if e.name.endswith("app.py")))
    wait(1500)
    shot(w, "find-files")
    w.close()

    # 4. Multi-rename with music tags
    w = window(home / "Music" / "The Lantern Pilots - Night Drive", home / "Music", size=(1920, 1000))
    panel = w.left.panel
    wait_until(lambda: len(panel.entries) == 6)
    panel.set_all_marks(True)
    paths = panel.selected_paths()
    items = [rename.item_for(p, e) for p, e in zip(paths, panel.selected_entries())]
    dialog = rename_ui.MultiRenameDialog(w, w.themes, items, rename.Rule(
        name_mask="[=track] [=artist] - [=title]"), rename.existing_names(items))
    dialog.show()
    wait_until(lambda: dialog.plan is not None and dialog.plan.pending == 0)
    wait(500)
    composite(w.grab().toImage(), dialog.grab().toImage(), w, out / "multi-rename.webp")
    dialog.close()
    w.close()

    # 5. Audio preview
    w = window(home / "Music" / "The Lantern Pilots - Night Drive", home / "Music")
    w.set_mode(w.right, PREVIEW)
    select(w, w.left, "track03.mp3")
    wait(4000)
    shot(w, "audio")
    w.close()

    # 6. PDF preview
    w = window(home / "Documents", home / "Downloads")
    w.set_mode(w.right, PREVIEW)
    select(w, w.left, "Annual Report 2025.pdf")
    wait(2500)
    shot(w, "pdf")
    w.close()

    # 7. Inside an archive
    w = window(home / "Downloads", home / "Documents")
    select(w, w.left, "photos-from-maria.zip")
    w.left.panel.activate("photos-from-maria.zip")
    wait(800)
    w.left.panel.activate("Lisbon trip")
    wait(600)
    w.left.panel.activate("Day 1")
    wait(800)
    w.set_mode(w.right, PREVIEW)
    select(w, w.left, "IMG_4502.JPG")
    wait(1500)
    shot(w, "archive")
    w.close()

    # 8. The terminal, linked to the panel
    w = window(home / "Projects" / "harbor-ferries", home / "Downloads")
    w.toggle_terminal()
    wait_until(lambda: "left" in w.terminal.sessions)
    session = w.terminal.sessions["left"]
    wait(800)
    for command in (
        "PROMPT='%F{blue}%~%f %F{magenta}❯%f '; clear",
        "ls -G",
        "grep -rn --color=always TODO src docs",
        "cd src",
    ):
        session.write(command + "\r")
        wait(700)
    wait(1500)
    w.terminal.view.setFocus()
    wait(300)
    shot(w, "terminal")
    w.terminal.close_all()
    w.close()
    shutil.rmtree(DEMO.parent, ignore_errors=True)
    return 0


def composite(window_img: QImage, dialog_img: QImage, w, path: Path) -> None:
    """The window with a dialog on top, centered, with a soft shadow."""
    img = QImage(window_img)
    dialog_img = QImage(dialog_img)
    for i in (img, dialog_img):  # compose in pixels: with a 2x ratio, offsets would be doubled
        i.setDevicePixelRatio(1)
    p = QPainter(img)
    p.setRenderHint(QPainter.RenderHint.Antialiasing)
    p.fillRect(img.rect(), QColor(0, 0, 0, 90))
    x = (img.width() - dialog_img.width()) // 2
    y = (img.height() - dialog_img.height()) // 2
    for i in range(18, 0, -2):
        p.fillRect(QRectF(x - i, y - i + 8, dialog_img.width() + 2 * i, dialog_img.height() + 2 * i),
                   QColor(0, 0, 0, 6))
    p.drawImage(x, y, dialog_img)
    p.end()
    save(img, path)


def save(img: QImage, path: Path) -> None:
    if not img.save(str(path), None, 88):  # WebP at 88: a tenth of the PNG's size, no visible loss
        raise OSError(f"could not write {path}")
    print("wrote", path, f"({path.stat().st_size // 1024} KB)")


if __name__ == "__main__":
    sys.exit(main())
