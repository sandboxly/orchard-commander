"""The waveform of an audio file: decoded with Qt Multimedia (FFmpeg) and drawn as
mirrored bars that double as the seek bar.

The decoder downmixes to mono floats at the file's own sample rate (asking it to
resample too makes Qt's FFmpeg decoder hang on MP3s). It runs on Qt's decoding
thread and hands buffers to the GUI thread, where each 20 ms block is reduced to
a peak and an RMS level. The widget fills in left to right while decoding.
"""

from __future__ import annotations

import math
from array import array
from pathlib import Path
from typing import BinaryIO

from PySide6.QtCore import QObject, QPointF, QRectF, QSize, Qt, QTimer, QUrl, Signal
from PySide6.QtGui import QColor, QFont, QFontMetricsF, QLinearGradient, QMouseEvent, QPainter, QPaintEvent
from PySide6.QtMultimedia import QAudioDecoder, QAudioFormat
from PySide6.QtWidgets import QSizePolicy, QWidget

from orchard_commander.ui.media_stream import StreamDevice
from orchard_commander.ui.style import ThemeManager

BLOCK_MS = 20  # one peak/RMS pair per 20 ms of audio
STALL_MS = 8000  # no data for this long while decoding: give up

# Sample formats the decoder may hand back: array typecode and full-scale value.
_FORMATS = {
    QAudioFormat.SampleFormat.Float: ("f", 1.0),
    QAudioFormat.SampleFormat.Int16: ("h", 32768.0),
    QAudioFormat.SampleFormat.Int32: ("i", 2147483648.0),
}


def format_time(ms: int) -> str:
    s = max(ms, 0) // 1000
    h, rem = divmod(s, 3600)
    m, s = divmod(rem, 60)
    return f"{h}:{m:02d}:{s:02d}" if h else f"{m:02d}:{s:02d}"


def block_levels(samples: array, per_block: int, full_scale: float) -> tuple[list[float], list[float], array]:
    """Peak and RMS (0..1, may exceed 1 slightly after downmixing) of each whole
    block of `per_block` samples, and the samples left over for the next call."""
    peaks: list[float] = []
    rms: list[float] = []
    whole = len(samples) - len(samples) % per_block
    for i in range(0, whole, per_block):
        block = samples[i : i + per_block]
        peaks.append(max(max(block), -min(block)) / full_scale)
        rms.append(math.sqrt(math.sumprod(block, block) / per_block) / full_scale)
    return peaks, rms, samples[whole:]


class WaveformAnalyzer(QObject):
    """Decodes a file and collects its levels; `changed` fires as they grow."""

    changed = Signal()

    def __init__(self, parent: QObject | None = None) -> None:
        super().__init__(parent)
        self.decoder = QAudioDecoder(self)
        wanted = QAudioFormat()  # no sample rate: resampling MP3s hangs the decoder
        wanted.setChannelCount(1)
        wanted.setSampleFormat(QAudioFormat.SampleFormat.Float)
        self.decoder.setAudioFormat(wanted)
        self.decoder.bufferReady.connect(self._buffer_ready)
        self.decoder.finished.connect(self._finished)
        self.decoder.error.connect(self._error)
        self._device: StreamDevice | None = None
        # Should the decoder ever stop delivering without an error, say so
        # rather than showing "Analyzing…" forever.
        self._watchdog = QTimer(self, singleShot=True, interval=STALL_MS)
        self._watchdog.timeout.connect(self._stalled)
        self._reset()

    def _reset(self) -> None:
        self.peaks: list[float] = []
        self.rms: list[float] = []
        self.complete = False
        self.error = ""
        self._pending: array | None = None
        self._full_scale = 1.0
        self._per_block = 0

    def start(self, path: Path | None = None, stream: BinaryIO | None = None, size: int = 0) -> None:
        """Analyze a local file, or a seekable stream (which this then owns)."""
        self.stop()
        if stream is not None:
            self._device = StreamDevice(stream, size)
            self.decoder.setSourceDevice(self._device)
        else:
            assert path is not None
            self.decoder.setSource(QUrl.fromLocalFile(str(path)))
        self.decoder.start()
        self._watchdog.start()
        self.changed.emit()

    def stop(self) -> None:
        self._watchdog.stop()
        self.decoder.stop()
        self.decoder.setSource(QUrl())
        if self._device is not None:
            self._device.close()
            self._device = None
        self._reset()

    @property
    def decoded_ms(self) -> int:
        return len(self.peaks) * BLOCK_MS

    def _buffer_ready(self) -> None:
        self._watchdog.start()
        buffer = self.decoder.read()
        fmt = buffer.format()
        kind = _FORMATS.get(fmt.sampleFormat())
        if not buffer.isValid() or kind is None or fmt.sampleRate() <= 0:
            return
        typecode, full_scale = kind
        if self._pending is None or self._pending.typecode != typecode:
            self._pending = array(typecode)
            self._full_scale = full_scale
            # Interleaved channels (if the decoder didn't downmix) just count as samples.
            self._per_block = max(1, fmt.sampleRate() * fmt.channelCount() * BLOCK_MS // 1000)
        data = buffer.constData()
        self._pending.frombytes(bytes(data)[: buffer.byteCount()])
        peaks, rms, self._pending = block_levels(self._pending, self._per_block, self._full_scale)
        if peaks:
            self.peaks += peaks
            self.rms += rms
            self.changed.emit()

    def _finished(self) -> None:
        self._watchdog.stop()
        if self._pending:  # the last, partial block
            n = len(self._pending)
            peaks, rms, _ = block_levels(self._pending, n, self._full_scale)
            self.peaks += peaks
            self.rms += rms
        self._pending = None
        self.complete = True
        self.changed.emit()

    def _error(self, _error: QAudioDecoder.Error) -> None:
        self._watchdog.stop()
        self.error = self.decoder.errorString() or "the audio can't be decoded"
        self.complete = True
        self.changed.emit()

    def _stalled(self) -> None:
        self.decoder.stop()
        if self.peaks:  # show what was decoded
            self.complete = True
        else:
            self.error = "the decoder stopped responding"
            self.complete = True
        self.changed.emit()


class WaveformWidget(QWidget):
    """Mirrored peak/RMS bars; the played part in the accent colors, a playhead,
    and a time readout under the mouse. Click or drag to seek."""

    seek_requested = Signal(int)  # ms

    def __init__(self, themes: ThemeManager, analyzer: WaveformAnalyzer) -> None:
        super().__init__()
        self.themes = themes
        self.analyzer = analyzer
        self.duration_ms = 0
        self.position_ms = 0
        self.note = ""  # shown instead of a waveform (e.g. why there is none)
        self._hover_x: float | None = None
        self.setMouseTracking(True)
        self.setCursor(Qt.CursorShape.PointingHandCursor)
        self.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Expanding)
        analyzer.changed.connect(self.update)
        themes.changed.connect(self.update)

    def sizeHint(self) -> QSize:
        line = QFontMetricsF(self.themes.font).height()
        return QSize(round(40 * line), round(8 * line))

    def minimumSizeHint(self) -> QSize:
        return QSize(0, round(4 * QFontMetricsF(self.themes.font).height()))

    def set_duration(self, ms: int) -> None:
        self.duration_ms = max(ms, 0)
        self.update()

    def set_position(self, ms: int) -> None:
        self.position_ms = ms
        self.update()

    def set_note(self, text: str) -> None:
        self.note = text
        self.update()

    # -- geometry ----------------------------------------------------------

    @property
    def total_ms(self) -> int:
        """The time span the width stands for."""
        a = self.analyzer
        if a.complete and a.peaks:
            return a.decoded_ms
        return max(self.duration_ms, a.decoded_ms)

    def _bar_metrics(self) -> tuple[float, float]:
        cw = QFontMetricsF(self.themes.font).horizontalAdvance("M")
        bar = max(3.0, round(cw * 0.45))
        return bar, max(1.0, round(bar * 0.35))

    def _columns(self, count: int) -> list[tuple[float, float] | None]:
        """(peak, rms) per column; None where nothing is decoded yet."""
        a = self.analyzer
        total_blocks = max(1, self.total_ms // BLOCK_MS)
        peaks, rms = a.peaks, a.rms
        result: list[tuple[float, float] | None] = []
        for i in range(count):
            lo = i * total_blocks // count
            hi = max(lo + 1, (i + 1) * total_blocks // count)
            if lo >= len(peaks):
                result.append(None)
                continue
            hi = min(hi, len(peaks))
            result.append((max(peaks[lo:hi]), sum(rms[lo:hi]) / (hi - lo)))
        return result

    def _x_to_ms(self, x: float) -> int:
        return round(max(0.0, min(x / max(1, self.width()), 1.0)) * self.total_ms)

    # -- painting ----------------------------------------------------------

    def paintEvent(self, event: QPaintEvent) -> None:
        t = self.themes.colors
        p = QPainter(self)
        p.setRenderHint(QPainter.RenderHint.Antialiasing)
        w, h = self.width(), self.height()
        line = QFontMetricsF(self.themes.font).height()
        top, bottom = line * 1.2, h - line * 0.4  # room for the time readout above
        mid = top + (bottom - top) * 0.68  # bars above, a shorter reflection below
        up, down = mid - top, (bottom - mid)

        a = self.analyzer
        if not a.peaks:
            p.setPen(QColor(t.border))
            p.drawLine(QPointF(0, mid), QPointF(w, mid))
            text = self.note or (a.error and f"No waveform: {a.error}") or ("" if a.complete else "Analyzing…")
            if text:
                p.setPen(QColor(t.muted))
                p.drawText(QRectF(0, top, w, up - line * 0.3), Qt.AlignmentFlag.AlignCenter, text)
            self._paint_playhead(p, top, bottom, line)
            p.end()
            return

        bar, gap = self._bar_metrics()
        count = max(1, int((w + gap) // (bar + gap)))
        columns = self._columns(count)
        loudest = max(0.05, max(c[0] for c in columns if c is not None))
        played_x = self._played_x()

        tint = QColor(t.border_active)  # the played part, faintly lit
        tint.setAlphaF(0.07)
        p.fillRect(QRectF(0, top, played_x, bottom - top), tint)

        played_top = QLinearGradient(0, top, 0, mid)
        played_top.setColorAt(0.0, QColor(t.accent))
        played_top.setColorAt(1.0, QColor(t.border_active))
        rest = QColor(t.muted)
        radius = bar / 2

        for i, column in enumerate(columns):
            x = i * (bar + gap)
            if column is None:  # still decoding: a faint baseline
                p.fillRect(QRectF(x, mid - 0.5, bar, 1), QColor(t.border))
                continue
            peak, rms = column[0] / loudest, min(column[1] / loudest, column[0] / loudest)
            played = x + bar / 2 <= played_x
            for level, alpha in ((peak, 0.45), (rms, 1.0)):
                height = max(1.0, level * up)
                if played:
                    p.setBrush(played_top)
                    p.setOpacity(alpha)
                else:
                    brush = QColor(rest)
                    brush.setAlphaF(0.35 if alpha < 1 else 0.85)
                    p.setBrush(brush)
                p.setPen(Qt.PenStyle.NoPen)
                p.drawRoundedRect(QRectF(x, mid - height, bar, height), radius, radius)
                # The reflection: shorter and fainter.
                p.setOpacity((alpha if played else 1.0) * 0.35)
                p.drawRoundedRect(QRectF(x, mid + 1, bar, max(1.0, level * down * 0.8)), radius, radius)
                p.setOpacity(1.0)
        self._paint_playhead(p, top, bottom, line)
        p.end()

    def _played_x(self) -> float:
        total = self.total_ms
        return self.width() * min(self.position_ms / total, 1.0) if total else 0.0

    def _paint_playhead(self, p: QPainter, top: float, bottom: float, line: float) -> None:
        t = self.themes.colors
        if self.total_ms and (self.position_ms > 0 or self._hover_x is None):
            x = self._played_x()
            p.fillRect(QRectF(x - 1, top - line * 0.2, 2, bottom - top + line * 0.2), QColor(t.foreground))
        if self._hover_x is not None and self.total_ms:
            x = self._hover_x
            ghost = QColor(t.foreground)
            ghost.setAlphaF(0.35)
            p.fillRect(QRectF(x, top, 1, bottom - top), ghost)
            label = f" {format_time(self._x_to_ms(x))} "
            font = QFont(self.themes.font)
            p.setFont(font)
            width = QFontMetricsF(font).horizontalAdvance(label)
            left = max(0.0, min(x - width / 2, self.width() - width))
            chip = QRectF(left, 0, width, line)
            p.fillRect(chip, QColor(t.border_active))
            p.setPen(QColor(t.surface))
            p.drawText(chip, Qt.AlignmentFlag.AlignCenter, label)

    # -- mouse -------------------------------------------------------------

    def mousePressEvent(self, event: QMouseEvent) -> None:
        if event.button() == Qt.MouseButton.LeftButton and self.total_ms:
            self.seek_requested.emit(self._x_to_ms(event.position().x()))

    def mouseMoveEvent(self, event: QMouseEvent) -> None:
        self._hover_x = event.position().x()
        if event.buttons() & Qt.MouseButton.LeftButton and self.total_ms:
            self.seek_requested.emit(self._x_to_ms(self._hover_x))
        self.update()

    def leaveEvent(self, event) -> None:
        self._hover_x = None
        self.update()
