"""Characters the terminal draws itself so they fill their cell exactly.

Powerline separators (prompt themes like powerlevel10k, agnoster, starship),
block elements and box-drawing lines are meant to touch their neighbours; a
font's glyphs leave gaps or misalign as soon as its proportions differ from
the cell (other font, line spacing). Like iTerm, kitty and WezTerm, draw them
geometrically instead.
"""

from __future__ import annotations

from PySide6.QtCore import QPointF, QRectF, Qt
from PySide6.QtGui import QBrush, QColor, QPainter, QPainterPath, QPen

# Box drawing: char -> (left, right, up, down) with 1 = light, 2 = heavy, 3 = double.
_BOX: dict[str, tuple[int, int, int, int]] = {
    "─": (1, 1, 0, 0), "━": (2, 2, 0, 0), "│": (0, 0, 1, 1), "┃": (0, 0, 2, 2),
    "┌": (0, 1, 0, 1), "┐": (1, 0, 0, 1), "└": (0, 1, 1, 0), "┘": (1, 0, 1, 0),
    "├": (0, 1, 1, 1), "┤": (1, 0, 1, 1), "┬": (1, 1, 0, 1), "┴": (1, 1, 1, 0), "┼": (1, 1, 1, 1),
    "┏": (0, 2, 0, 2), "┓": (2, 0, 0, 2), "┗": (0, 2, 2, 0), "┛": (2, 0, 2, 0),
    "┣": (0, 2, 2, 2), "┫": (2, 0, 2, 2), "┳": (2, 2, 0, 2), "┻": (2, 2, 2, 0), "╋": (2, 2, 2, 2),
    "╴": (1, 0, 0, 0), "╵": (0, 0, 1, 0), "╶": (0, 1, 0, 0), "╷": (0, 0, 0, 1),
    "═": (3, 3, 0, 0), "║": (0, 0, 3, 3), "╔": (0, 3, 0, 3), "╗": (3, 0, 0, 3), "╚": (0, 3, 3, 0),
    "╝": (3, 0, 3, 0), "╠": (0, 3, 3, 3), "╣": (3, 0, 3, 3), "╦": (3, 3, 0, 3), "╩": (3, 3, 3, 0),
    "╬": (3, 3, 3, 3),
}
_ROUND = {"╭": ("right", "down"), "╮": ("left", "down"), "╯": ("left", "up"), "╰": ("right", "up")}


def handles(char: str) -> bool:
    if len(char) != 1:
        return False
    cp = ord(char)
    return char in _BOX or char in _ROUND or 0x2580 <= cp <= 0x2595 or 0xE0B0 <= cp <= 0xE0BF


def draw(p: QPainter, char: str, rect: QRectF, color: QColor) -> None:
    """Draw `char` (one for which `handles()` is true) filling `rect`."""
    cp = ord(char)
    p.save()
    p.setRenderHint(QPainter.RenderHint.Antialiasing, cp >= 0xE0B0 or char in _ROUND)
    if 0xE0B0 <= cp <= 0xE0BF:
        _powerline(p, cp, rect, color)
    elif 0x2580 <= cp <= 0x2595:
        _block(p, cp, rect, color)
    elif char in _ROUND:
        _rounded(p, _ROUND[char], rect, color)
    else:
        _box(p, _BOX[char], rect, color)
    p.restore()


def _thin(rect: QRectF) -> float:
    return max(1.0, round(rect.width() / 8))


def _box(p: QPainter, arms: tuple[int, int, int, int], rect: QRectF, color: QColor) -> None:
    cx, cy = rect.center().x(), rect.center().y()
    thin = _thin(rect)
    left, right, up, down = arms
    ends = {
        "left": (QPointF(rect.left(), cy), left), "right": (QPointF(rect.right() + 1, cy), right),
        "up": (QPointF(cx, rect.top()), up), "down": (QPointF(cx, rect.bottom() + 1), down),
    }
    for name, (end, weight) in ends.items():
        if not weight:
            continue
        horizontal = name in ("left", "right")
        if weight == 3:  # double: two thin lines, one cell-width apart
            gap = thin * 1.5
            for offset in (-gap, gap):
                pen = QPen(color, thin)
                pen.setCapStyle(Qt.PenCapStyle.FlatCap)
                p.setPen(pen)
                shift = QPointF(0, offset) if horizontal else QPointF(offset, 0)
                start = QPointF(cx, cy) + (QPointF(-gap if name == "right" else gap, 0) if horizontal and
                                            _crosses(arms, horizontal) else QPointF(0, 0))
                p.drawLine(start + shift, end + shift)
            continue
        pen = QPen(color, thin * (2 if weight == 2 else 1))
        pen.setCapStyle(Qt.PenCapStyle.SquareCap)
        p.setPen(pen)
        p.drawLine(QPointF(cx, cy), end)


def _crosses(arms: tuple[int, int, int, int], horizontal: bool) -> bool:
    left, right, up, down = arms
    return bool(up or down) if horizontal else bool(left or right)


def _rounded(p: QPainter, arms: tuple[str, str], rect: QRectF, color: QColor) -> None:
    cx, cy = rect.center().x(), rect.center().y()
    horizontal, vertical = arms
    hx = rect.right() + 1 if horizontal == "right" else rect.left()
    vy = rect.bottom() + 1 if vertical == "down" else rect.top()
    path = QPainterPath(QPointF(hx, cy))
    path.quadTo(QPointF(cx, cy), QPointF(cx, vy))
    pen = QPen(color, _thin(rect))
    p.setPen(pen)
    p.setBrush(Qt.BrushStyle.NoBrush)
    p.drawPath(path)


def _block(p: QPainter, cp: int, rect: QRectF, color: QColor) -> None:
    x, y, w, h = rect.x(), rect.y(), rect.width(), rect.height()
    if cp == 0x2580:  # upper half
        p.fillRect(QRectF(x, y, w, h / 2), color)
    elif 0x2581 <= cp <= 0x2588:  # lower 1/8 … full block
        part = h * (cp - 0x2580) / 8
        p.fillRect(QRectF(x, y + h - part, w, part), color)
    elif 0x2589 <= cp <= 0x258F:  # left 7/8 … 1/8
        part = w * (0x2590 - cp) / 8
        p.fillRect(QRectF(x, y, part, h), color)
    elif cp == 0x2590:  # right half
        p.fillRect(QRectF(x + w / 2, y, w / 2, h), color)
    elif 0x2591 <= cp <= 0x2593:  # light, medium, dark shade
        shade = QColor(color)
        shade.setAlphaF((cp - 0x2590) / 4)
        p.fillRect(rect, shade)
    elif cp == 0x2594:  # upper 1/8
        p.fillRect(QRectF(x, y, w, h / 8), color)
    elif cp == 0x2595:  # right 1/8
        p.fillRect(QRectF(x + w * 7 / 8, y, w / 8, h), color)


def _powerline(p: QPainter, cp: int, rect: QRectF, color: QColor) -> None:
    # Slightly past the cell edges, so no hairline of background shows next to the segment.
    x0, x1 = rect.left() - 0.5, rect.right() + 1.5
    y0, y1 = rect.top() - 0.5, rect.bottom() + 1.5
    ym = (y0 + y1) / 2
    solid = cp % 2 == 0
    path = QPainterPath()
    if cp in (0xE0B0, 0xE0B1):  #  right-pointing triangle / chevron
        pts = [QPointF(x0, y0), QPointF(x1 - 1.5, ym), QPointF(x0, y1)]
    elif cp in (0xE0B2, 0xE0B3):  #  left-pointing
        pts = [QPointF(x1, y0), QPointF(x0 + 1.5, ym), QPointF(x1, y1)]
    elif cp in (0xE0B4, 0xE0B5):  #  right half circle
        path.moveTo(x0, y0)
        path.cubicTo(QPointF(x1 + rect.width() * 0.3, y0), QPointF(x1 + rect.width() * 0.3, y1), QPointF(x0, y1))
        pts = []
    elif cp in (0xE0B6, 0xE0B7):  #  left half circle
        path.moveTo(x1, y0)
        path.cubicTo(QPointF(x0 - rect.width() * 0.3, y0), QPointF(x0 - rect.width() * 0.3, y1), QPointF(x1, y1))
        pts = []
    elif cp in (0xE0B8, 0xE0B9):  #  lower-left triangle
        pts = [QPointF(x0, y0), QPointF(x1, y1), QPointF(x0, y1)]
    elif cp in (0xE0BA, 0xE0BB):  #  lower-right triangle
        pts = [QPointF(x1, y0), QPointF(x1, y1), QPointF(x0, y1)]
    elif cp in (0xE0BC, 0xE0BD):  #  upper-left triangle
        pts = [QPointF(x0, y0), QPointF(x1, y0), QPointF(x0, y1)]
    else:  #  upper-right triangle
        pts = [QPointF(x0, y0), QPointF(x1, y0), QPointF(x1, y1)]
    if pts:
        path.moveTo(pts[0])
        for pt in pts[1:]:
            path.lineTo(pt)
    if solid:
        path.closeSubpath()
        p.setPen(Qt.PenStyle.NoPen)
        p.setBrush(QBrush(color))
    else:
        if cp in (0xE0B9, 0xE0BB, 0xE0BD, 0xE0BF):  # the thin slants: just the diagonal
            path = QPainterPath(pts[0] if cp in (0xE0B9, 0xE0BD) else pts[1])
            path.lineTo(pts[1] if cp in (0xE0B9, 0xE0BD) else pts[2])
        p.setPen(QPen(color, _thin(rect)))
        p.setBrush(Qt.BrushStyle.NoBrush)
    p.drawPath(path)
