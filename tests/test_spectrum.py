"""The live spectrum analyzer: FFT, band levels, buffer intake, animation, playback."""

from __future__ import annotations

import cmath
import math
import random
import struct
from pathlib import Path

import pytest
from PySide6.QtCore import QByteArray
from PySide6.QtMultimedia import QAudioBuffer, QAudioFormat

from orchard_commander.preview.builtin.media import MediaData, MediaView
from orchard_commander.preview.builtin.spectrum import FFT, SpectrumAnalyzer, SpectrumWidget, band_edges, band_levels
from orchard_commander.ui.style import ThemeManager

from test_ui import wait_until
from test_waveform import write_wav

RATE = 44100


def sine(freq: float, n: int, amplitude: float = 1.0, rate: int = RATE) -> list[float]:
    return [amplitude * math.sin(2 * math.pi * freq * i / rate) for i in range(n)]


def stereo_int16_buffer(samples: list[float], rate: int = RATE) -> QAudioBuffer:
    fmt = QAudioFormat()
    fmt.setSampleRate(rate)
    fmt.setChannelCount(2)
    fmt.setSampleFormat(QAudioFormat.SampleFormat.Int16)
    data = b"".join(struct.pack("<hh", int(s * 32767), int(s * 32767)) for s in samples)
    return QAudioBuffer(QByteArray(data), fmt)


@pytest.fixture
def themes(qapp) -> ThemeManager:
    manager = ThemeManager()
    manager.apply()
    return manager


def test_fft_matches_the_dft():
    n = 64
    x = [random.uniform(-1, 1) for _ in range(n)]
    result = FFT(n)(x)
    for k in (0, 1, 5, 31, 63):
        dft = sum(x[i] * cmath.exp(-2j * math.pi * k * i / n) for i in range(n))
        assert abs(result[k] - dft) < 1e-9


def test_a_full_scale_sine_reads_about_one():
    fft = FFT(2048)
    bin_ = 93  # a frequency exactly on a bin
    mags = fft.magnitudes(sine(bin_ * RATE / 2048, 2048))
    assert mags[bin_] == pytest.approx(1.0, abs=0.02)
    assert max(mags[: bin_ - 3] + mags[bin_ + 4 :]) < 0.01


def test_a_tone_lights_its_band():
    count = 40
    levels = band_levels(FFT(2048).magnitudes(sine(1000, 2048, 0.5)), RATE, count)
    edges = band_edges(count, RATE)
    band = next(i for i in range(count) if edges[i] <= 1000 < edges[i + 1])
    assert levels.index(max(levels)) == band and levels[band] > 0.8
    assert levels[0] == 0 and levels[-1] == 0  # far away: unlit
    assert all(0.0 <= v <= 1.0 for v in levels)


def test_analyzer_takes_played_buffers(qapp):
    analyzer = SpectrumAnalyzer()
    assert analyzer.levels(32) is None
    tone = sine(2000, 6000, 0.5)
    for i in range(0, len(tone), 576):  # buffers the size the player delivers
        analyzer.feed(stereo_int16_buffer(tone[i : i + 576]))
    assert analyzer.rate == RATE
    levels = analyzer.levels(32)
    edges = band_edges(32, RATE)
    assert edges[levels.index(max(levels))] <= 2000 < edges[levels.index(max(levels)) + 1]
    analyzer.reset()
    assert analyzer.levels(32) is None


def test_widget_rises_while_playing_and_settles_when_paused(qtbot, themes: ThemeManager):
    analyzer = SpectrumAnalyzer()
    widget = SpectrumWidget(themes, analyzer)
    qtbot.addWidget(widget)
    widget.resize(500, 200)
    widget.show()
    analyzer.feed(stereo_int16_buffer(sine(440, 4096, 0.8)))
    widget.set_active(True)
    wait_until(lambda: max(widget._bars, default=0) > 0.5)
    assert max(widget._peaks) >= max(widget._bars)
    assert not widget.grab().isNull()
    widget.set_active(False)
    wait_until(lambda: not widget._timer.isActive(), timeout=5)  # stops animating by itself
    assert max(widget._bars) == 0


def test_spectrum_follows_playback(qtbot, themes: ThemeManager, tmp_path: Path):
    view = MediaView(themes)
    qtbot.addWidget(view)
    view.show()
    path = write_wav(tmp_path / "tone.wav", seconds_tone=3, seconds_silence=0)
    view.show_data(MediaData(False, path.name, path.stat().st_size, path=path))
    view.player.play()  # muted, as previews start
    wait_until(lambda: max(view.spectrum._bars, default=0) > 0.3, timeout=10)
    view.player.pause()
    wait_until(lambda: not view.spectrum._timer.isActive(), timeout=5)
    view.clear()
    assert view.spectrum_analyzer.levels(8) is None
