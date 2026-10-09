"""A live spectrum analyzer for the audio being played.

The player hands over each buffer as it plays (QAudioBufferOutput, also while
muted). The analyzer keeps the latest samples as mono floats; at every frame of
the animation the widget asks for the levels of logarithmically spaced bands,
computed with a Hann-windowed FFT (pure Python: about 1 ms for 2048 points).
The widget draws them as segmented "LED" bars with falling peak caps.
"""

from __future__ import annotations

import cmath
import math
import time
from array import array

from PySide6.QtCore import QObject, QRectF, QSize, QTimer
from PySide6.QtGui import QColor, QFontMetricsF, QPainter, QPaintEvent
from PySide6.QtMultimedia import QAudioBuffer, QAudioFormat
from PySide6.QtWidgets import QSizePolicy, QWidget

from orchard_commander.ui.style import ThemeManager

MIN_HZ, MAX_HZ = 30.0, 16000.0
DB_FLOOR = -60.0  # quieter than this is an unlit bar
DB_CEILING = -8.0  # a full bar (single frequencies in music rarely get louder)
TILT_DB_PER_OCTAVE = 3.0  # music has less energy up high; lift it around 1 kHz
WINDOW_SECONDS = 0.046  # FFT length: ~46 ms, a power of two in samples
FPS = 60

_FORMATS = {
    QAudioFormat.SampleFormat.Float: ("f", 1.0),
    QAudioFormat.SampleFormat.Int16: ("h", 32768.0),
    QAudioFormat.SampleFormat.Int32: ("i", 2147483648.0),
}


class FFT:
    """Iterative radix-2 FFT of a fixed size, with its tables precomputed."""

    def __init__(self, n: int) -> None:
        assert n & (n - 1) == 0, "power of two"
        self.n = n
        bits = n.bit_length() - 1
        self._order = [int(f"{i:0{bits}b}"[::-1], 2) for i in range(n)] if n > 1 else [0]
        self._twiddles = [cmath.exp(-2j * math.pi * k / n) for k in range(n // 2)]
        self.window = [0.5 - 0.5 * math.cos(2 * math.pi * i / (n - 1)) for i in range(n)]  # Hann

    def __call__(self, x: list[float]) -> list[complex]:
        n, tw = self.n, self._twiddles
        a = [complex(x[i]) for i in self._order]
        size = 2
        while size <= n:
            half, step = size // 2, n // size
            for start in range(0, n, size):
                k = 0
                for j in range(start, start + half):
                    t = a[j + half] * tw[k]
                    a[j + half] = a[j] - t
                    a[j] += t
                    k += step
            size *= 2
        return a

    def magnitudes(self, samples: list[float]) -> list[float]:
        """Amplitude per bin 0..n/2 (a full-scale sine reads about 1.0)."""
        spectrum = self([s * w for s, w in zip(samples, self.window)])
        scale = 4.0 / self.n  # 2 for one side of the spectrum, 2 for the Hann window's gain
        return [abs(c) * scale for c in spectrum[: self.n // 2 + 1]]


def band_edges(count: int, rate: int) -> list[float]:
    top = min(MAX_HZ, rate / 2)
    return [MIN_HZ * (top / MIN_HZ) ** (i / count) for i in range(count + 1)]


def band_levels(magnitudes: list[float], rate: int, count: int) -> list[float]:
    """0..1 per logarithmic band: the loudest bin in the band, between DB_FLOOR and DB_CEILING."""
    n = (len(magnitudes) - 1) * 2
    edges = band_edges(count, rate)
    levels = []
    for lo_hz, hi_hz in zip(edges, edges[1:]):
        lo, hi = lo_hz * n / rate, hi_hz * n / rate
        if int(hi) > int(lo):  # whole bins in the band
            mag = max(magnitudes[int(lo) + 1 : int(hi) + 1])
        else:  # a band narrower than a bin (bass): interpolate at its centre
            c = (lo + hi) / 2
            i = min(int(c), len(magnitudes) - 2)
            mag = magnitudes[i] + (magnitudes[i + 1] - magnitudes[i]) * (c - i)
        centre = math.sqrt(lo_hz * hi_hz)
        db = 20 * math.log10(mag + 1e-12) + TILT_DB_PER_OCTAVE * math.log2(centre / 1000)
        levels.append(max(0.0, min(1.0, (db - DB_FLOOR) / (DB_CEILING - DB_FLOOR))))
    return levels


class SpectrumAnalyzer(QObject):
    """The latest played samples (mono), and their band levels on request."""

    def __init__(self, parent: QObject | None = None) -> None:
        super().__init__(parent)
        self.reset()

    def reset(self) -> None:
        self._samples = array("f")
        self.rate = 0
        self._fft: FFT | None = None

    def feed(self, buffer: QAudioBuffer) -> None:
        fmt = buffer.format()
        kind = _FORMATS.get(fmt.sampleFormat())
        channels = fmt.channelCount()
        if not buffer.isValid() or kind is None or fmt.sampleRate() <= 0 or channels <= 0:
            return
        typecode, full_scale = kind
        raw = array(typecode)
        raw.frombytes(bytes(buffer.constData())[: buffer.byteCount()])
        if channels == 1 and typecode == "f":
            mono = raw
        else:
            mono = array("f", (sum(raw[i : i + channels]) / (channels * full_scale)
                               for i in range(0, len(raw) - channels + 1, channels)))
        if fmt.sampleRate() != self.rate:
            self.rate = fmt.sampleRate()
            n = 1 << max(8, round(math.log2(self.rate * WINDOW_SECONDS)))
            self._fft = FFT(n)
            self._samples = array("f")
        self._samples.extend(mono)
        assert self._fft is not None
        excess = len(self._samples) - self._fft.n
        if excess > 0:
            del self._samples[:excess]

    def levels(self, count: int) -> list[float] | None:
        """Band levels of the most recent window, or None before any audio."""
        fft = self._fft
        if fft is None or not self._samples:
            return None
        samples = list(self._samples)
        samples = [0.0] * (fft.n - len(samples)) + samples
        return band_levels(fft.magnitudes(samples), self.rate, count)


class SpectrumWidget(QWidget):
    """Segmented bars with peak caps, animated while `active` (playing)."""

    PEAK_HOLD = 0.35  # seconds a peak cap stays before falling
    FALL = 1.6  # bar fall speed, in heights per second
    GRAVITY = 3.0  # peak caps fall faster and faster (heights per second²)

    def __init__(self, themes: ThemeManager, analyzer: SpectrumAnalyzer) -> None:
        super().__init__()
        self.themes = themes
        self.analyzer = analyzer
        self.active = False
        self._bars: list[float] = []
        self._peaks: list[float] = []
        self._peak_age: list[float] = []
        self._peak_speed: list[float] = []
        self._last = time.monotonic()
        self._timer = QTimer(self, interval=1000 // FPS)
        self._timer.timeout.connect(self._tick)
        self.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Expanding)
        themes.changed.connect(self.update)

    def sizeHint(self) -> QSize:
        line = QFontMetricsF(self.themes.font).height()
        return QSize(round(40 * line), round(10 * line))

    def minimumSizeHint(self) -> QSize:
        return QSize(0, round(4 * QFontMetricsF(self.themes.font).height()))

    def set_active(self, active: bool) -> None:
        """Playing: follow the music. Otherwise the bars fall and the animation stops."""
        self.active = active
        if not self._timer.isActive():
            self._last = time.monotonic()
            self._timer.start()

    def reset(self) -> None:
        self.active = False
        self._timer.stop()
        self._bars, self._peaks, self._peak_age, self._peak_speed = [], [], [], []
        self.update()

    # -- geometry ----------------------------------------------------------

    def _metrics(self) -> tuple[float, float, int]:
        """Bar width, gap, and how many bars fit."""
        cw = QFontMetricsF(self.themes.font).horizontalAdvance("M")
        bar = max(4.0, round(cw * 0.9))
        gap = max(2.0, round(cw * 0.25))
        return bar, gap, max(1, min(96, int((self.width() + gap) // (bar + gap))))

    # -- animation ---------------------------------------------------------

    def _tick(self) -> None:
        now = time.monotonic()
        dt, self._last = min(now - self._last, 0.1), now
        count = self._metrics()[2]
        target = (self.analyzer.levels(count) if self.active else None) or [0.0] * count
        if len(self._bars) != count:
            self._bars, self._peaks = [0.0] * count, [0.0] * count
            self._peak_age, self._peak_speed = [0.0] * count, [0.0] * count
        for i, level in enumerate(target):
            bar = self._bars[i]
            # Rise quickly (a little smoothing against flicker), fall at a steady pace.
            bar = bar + (level - bar) * 0.7 if level > bar else max(level, bar - self.FALL * dt)
            self._bars[i] = bar
            if bar >= self._peaks[i]:
                self._peaks[i], self._peak_age[i], self._peak_speed[i] = bar, 0.0, 0.0
            else:
                self._peak_age[i] += dt
                if self._peak_age[i] > self.PEAK_HOLD:
                    self._peak_speed[i] += self.GRAVITY * dt
                    self._peaks[i] = max(bar, self._peaks[i] - self._peak_speed[i] * dt)
        if not self.active and max(self._peaks, default=0.0) <= 0.001:
            self._timer.stop()  # everything has settled
        self.update()

    # -- painting ----------------------------------------------------------

    def paintEvent(self, event: QPaintEvent) -> None:
        t = self.themes.colors
        p = QPainter(self)
        bar, gap, count = self._metrics()
        line = QFontMetricsF(self.themes.font).height()
        segment = max(3.0, round(line / 4))  # LED segment height, plus a 1-2 px gap
        seg_gap = max(1.0, round(segment / 3))
        rows = max(1, int((self.height() + seg_gap) // (segment + seg_gap)))
        total = count * (bar + gap) - gap
        left = (self.width() - total) / 2

        low, mid, high = QColor(t.border_active), QColor(t.accent), QColor(t.error)
        unlit = QColor(t.border)
        unlit.setAlphaF(0.35)
        knee = rows * 0.7  # low → accent over the lower 70%, accent → error above
        colors = [_mix(low, mid, r / knee) if r < knee else _mix(mid, high, (r - knee) / (rows - knee))
                  for r in range(rows)]
        bars = self._bars if len(self._bars) == count else [0.0] * count
        peaks = self._peaks if len(self._peaks) == count else [0.0] * count
        for i in range(count):
            x = left + i * (bar + gap)
            lit = round(bars[i] * rows)
            peak_row = min(rows - 1, round(peaks[i] * rows))
            for r in range(rows):
                y = self.height() - (r + 1) * (segment + seg_gap) + seg_gap
                if r < lit:
                    color = colors[r]
                elif r == peak_row and peak_row > 0:
                    color = QColor(t.foreground)
                else:
                    color = unlit
                p.fillRect(QRectF(x, y, bar, segment), color)
        p.end()


def _mix(a: QColor, b: QColor, f: float) -> QColor:
    f = max(0.0, min(1.0, f))
    return QColor.fromRgbF(a.redF() + (b.redF() - a.redF()) * f, a.greenF() + (b.greenF() - a.greenF()) * f,
                           a.blueF() + (b.blueF() - a.blueF()) * f)
