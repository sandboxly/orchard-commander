"""A terminal-style box: rounded border with a title set into the top edge and a
subtitle set into the bottom edge, sized in character cells."""

from __future__ import annotations

from PySide6.QtCore import QPoint, QRectF, Qt, Signal
from PySide6.QtGui import QColor, QFontMetricsF, QMouseEvent, QPainter, QPaintEvent, QPen
from PySide6.QtWidgets import QFrame, QVBoxLayout, QWidget

from orchard_commander.ui.style import ThemeManager


class BoxFrame(QFrame):
    title_clicked = Signal(QPoint)  # global position below the title, e.g. for a menu

    def __init__(self, themes: ThemeManager, parent: QWidget | None = None, *, heavy: bool = False) -> None:
        super().__init__(parent)
        self.setObjectName("box")
        self.themes = themes
        self.heavy = heavy
        self._title = ""
        self._subtitle = ""
        self._active = False
        self._title_rect = QRectF()
        self.body = QVBoxLayout(self)
        self.body.setSpacing(0)
        themes.changed.connect(self._restyle)
        self._restyle()

    # -- properties --------------------------------------------------------

    def set_title(self, text: str) -> None:
        if text != self._title:
            self._title = text
            self.update()

    def set_subtitle(self, text: str) -> None:
        if text != self._subtitle:
            self._subtitle = text
            self.update()

    def set_active(self, active: bool) -> None:
        if active != self._active:
            self._active = active
            self.update()

    @property
    def active(self) -> bool:
        return self._active

    # -- geometry ----------------------------------------------------------

    def _cell(self) -> tuple[float, float]:
        fm = QFontMetricsF(self.themes.font)
        return fm.horizontalAdvance("M"), fm.height()

    def _restyle(self) -> None:
        cw, ch = self._cell()
        self.body.setContentsMargins(round(cw), round(ch), round(cw), round(ch))
        self.update()

    def _elide_left(self, text: str, width: float) -> str:
        fm = QFontMetricsF(self.themes.font)
        if fm.horizontalAdvance(text) <= width:
            return text
        while text and fm.horizontalAdvance("…" + text) > width:
            text = text[1:]
        return "…" + text

    def mousePressEvent(self, event: QMouseEvent) -> None:
        if event.button() == Qt.MouseButton.LeftButton and self._title_rect.contains(event.position()):
            below = self._title_rect.bottomLeft().toPoint()
            self.title_clicked.emit(self.mapToGlobal(below))
            event.accept()
            return
        super().mousePressEvent(event)

    def title_anchor(self) -> QPoint:
        """Global position just below the title (where its menu opens)."""
        return self.mapToGlobal(self._title_rect.bottomLeft().toPoint())

    # -- painting ----------------------------------------------------------

    def paintEvent(self, event: QPaintEvent) -> None:
        t = self.themes.theme
        cw, ch = self._cell()
        p = QPainter(self)
        p.setRenderHint(QPainter.RenderHint.Antialiasing)
        p.setFont(self.themes.font)
        fm = QFontMetricsF(self.themes.font)

        p.fillRect(self.rect(), QColor(t.background))
        box = QRectF(cw / 2, ch / 2, self.width() - cw, self.height() - ch)
        color = QColor(t.border_active if self._active else t.border)
        pen = QPen(color, 2.0 if (self.heavy or self._active) else 1.0)
        p.setPen(pen)
        p.setBrush(QColor(t.surface))
        p.drawRoundedRect(box, cw * 0.7, cw * 0.7)

        room = self.width() - 6 * cw
        if self._title and room > cw:
            text = f" {self._elide_left(self._title, room - 2 * cw)} "
            rect = QRectF(2 * cw, 0, fm.horizontalAdvance(text), ch)
            self._title_rect = rect
            if self._active:
                p.fillRect(rect, color)
                p.setPen(QColor(t.surface))
                f = p.font()
                f.setBold(True)
                p.setFont(f)
            else:
                p.fillRect(rect, QColor(t.surface))
                p.setPen(QColor(t.muted))
            p.drawText(rect, Qt.AlignmentFlag.AlignCenter, text)
            p.setFont(self.themes.font)

        if self._subtitle and room > cw:
            text = f" {self._elide_left(self._subtitle, room)} "
            w = fm.horizontalAdvance(text)
            rect = QRectF(self.width() - 2 * cw - w, self.height() - ch, w, ch)
            p.fillRect(rect, QColor(t.surface))
            p.setPen(QColor(t.muted))
            p.drawText(rect, Qt.AlignmentFlag.AlignCenter, text)
        p.end()
