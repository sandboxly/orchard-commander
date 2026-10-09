"""Audio waveforms: level math, decoding, the widget, and the media view."""

from __future__ import annotations

import math
import struct
import wave
from array import array
from pathlib import Path

import pytest
from PySide6.QtCore import QPoint, Qt

from orchard_commander import vfs
from orchard_commander.preview.builtin.media import MediaData, MediaView
from orchard_commander.preview.builtin.waveform import (
    BLOCK_MS,
    WaveformAnalyzer,
    WaveformWidget,
    block_levels,
    format_time,
)
from orchard_commander.ui.style import ThemeManager

from test_ui import wait_until


def write_wav(path: Path, seconds_tone: float = 1.0, seconds_silence: float = 1.0, amplitude: float = 0.5) -> Path:
    """A 440 Hz tone at `amplitude`, then silence; 16-bit stereo."""
    rate = 22050
    frames = bytearray()
    for i in range(int(rate * (seconds_tone + seconds_silence))):
        v = amplitude * math.sin(2 * math.pi * 440 * i / rate) if i < rate * seconds_tone else 0.0
        frames += struct.pack("<hh", int(v * 32767), int(v * 32767))
    with wave.open(str(path), "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes(bytes(frames))
    return path


def test_block_levels():
    samples = array("f", [0.5, -1.0, 0.25, 0.0, 0.1])
    peaks, rms, rest = block_levels(samples, 2, 1.0)
    assert peaks == [1.0, 0.25]
    assert rms[0] == pytest.approx(math.sqrt((0.25 + 1.0) / 2))
    assert list(rest) == pytest.approx([0.1])
    peaks, _, _ = block_levels(array("h", [16384, -32768]), 2, 32768.0)
    assert peaks == [1.0]


def test_format_time():
    assert format_time(0) == "00:00"
    assert format_time(83_000) == "01:23"
    assert format_time(3_723_000) == "1:02:03"


@pytest.fixture
def themes(qapp) -> ThemeManager:
    manager = ThemeManager()
    manager.apply()
    return manager


def test_analyzer_finds_the_loud_and_the_quiet_part(qtbot, tmp_path: Path):
    analyzer = WaveformAnalyzer()
    analyzer.start(path=write_wav(tmp_path / "tone.wav"))
    wait_until(lambda: analyzer.complete, timeout=10)
    assert not analyzer.error
    assert analyzer.decoded_ms == pytest.approx(2000, abs=2 * BLOCK_MS)
    half = len(analyzer.peaks) // 2
    loud, quiet = analyzer.peaks[: half - 2], analyzer.peaks[half + 2 :]
    assert min(loud) > 0.3 and max(quiet) < 0.01
    assert all(r <= p + 1e-6 for p, r in zip(analyzer.peaks, analyzer.rms))
    analyzer.stop()
    assert analyzer.peaks == [] and not analyzer.complete


def test_analyzer_decodes_mp3(qtbot):
    # Asking Qt's decoder to resample MP3s made it hang: stuck at "Analyzing…".
    analyzer = WaveformAnalyzer()
    analyzer.start(path=Path(__file__).parent / "data" / "tone-then-silence.mp3")
    wait_until(lambda: analyzer.complete, timeout=10)
    assert not analyzer.error
    assert analyzer.decoded_ms == pytest.approx(2000, abs=5 * BLOCK_MS)  # MP3 adds encoder padding
    third = len(analyzer.peaks) // 3
    assert min(analyzer.peaks[5:third]) > 0.3 and max(analyzer.peaks[-third:]) < 0.01


def test_analyzer_gives_up_when_the_decoder_stalls(qtbot, tmp_path: Path, monkeypatch):
    from orchard_commander.preview.builtin import waveform

    monkeypatch.setattr(waveform, "STALL_MS", 200)
    analyzer = WaveformAnalyzer()
    analyzer.start(path=write_wav(tmp_path / "tone.wav"))
    analyzer.decoder.bufferReady.disconnect()  # as if it stopped delivering
    analyzer.decoder.finished.disconnect()
    wait_until(lambda: analyzer.complete, timeout=5)
    assert analyzer.error


def test_analyzer_reads_streams(qtbot, tmp_path: Path):
    analyzer = WaveformAnalyzer()
    path = write_wav(tmp_path / "tone.wav")
    analyzer.start(stream=path.open("rb"), size=path.stat().st_size)
    wait_until(lambda: analyzer.complete, timeout=10)
    assert analyzer.decoded_ms == pytest.approx(2000, abs=2 * BLOCK_MS)
    analyzer.stop()


def test_analyzer_fails_quietly_on_garbage(qtbot, tmp_path: Path):
    bad = tmp_path / "garbage.mp3"
    bad.write_bytes(b"\x00\x01not audio" * 300)
    analyzer = WaveformAnalyzer()
    analyzer.start(path=bad)
    wait_until(lambda: analyzer.complete, timeout=10)
    assert analyzer.error and not analyzer.peaks


def test_widget_paints_and_seeks(qtbot, themes: ThemeManager, tmp_path: Path):
    analyzer = WaveformAnalyzer()
    widget = WaveformWidget(themes, analyzer)
    qtbot.addWidget(widget)
    widget.resize(600, 160)
    widget.show()
    assert not widget.grab().isNull()  # nothing decoded yet: a baseline
    analyzer.start(path=write_wav(tmp_path / "tone.wav"))
    wait_until(lambda: analyzer.complete, timeout=10)
    widget.set_duration(2000)
    widget.set_position(500)
    assert not widget.grab().isNull()
    columns = widget._columns(100)
    assert all(c is not None for c in columns)
    assert columns[10][0] > 0.3 and columns[90][0] < 0.01

    seeks: list[int] = []
    widget.seek_requested.connect(seeks.append)
    qtbot.mouseClick(widget, Qt.MouseButton.LeftButton, pos=QPoint(450, 80))
    assert seeks and seeks[-1] == pytest.approx(1500, abs=2 * BLOCK_MS)


def test_media_view_shows_the_waveform_for_audio(qtbot, themes: ThemeManager, tmp_path: Path):
    view = MediaView(themes)
    qtbot.addWidget(view)
    path = write_wav(tmp_path / "tone.wav")
    view.show_data(MediaData(False, path.name, path.stat().st_size, path=path))
    assert view.screen_stack.currentIndex() == 1 and not view.slider.isVisibleTo(view)
    assert view.title_label.text() == "tone.wav"
    wait_until(lambda: view.analyzer.complete, timeout=10)
    assert view.analyzer.peaks
    wait_until(lambda: view.player.duration() > 0, timeout=10)
    view.waveform.seek_requested.emit(1000)
    wait_until(lambda: view.player.position() == pytest.approx(1000, abs=100), timeout=5)
    view.clear()
    assert not view.analyzer.peaks


def test_large_audio_on_a_server_gets_no_waveform(qtbot, themes: ThemeManager, monkeypatch, tmp_path: Path):
    from orchard_commander.preview.builtin import media
    from orchard_commander.preview.api import LoadContext
    from orchard_commander.preview.source import PreviewSource
    from orchard_commander.themes import get_scheme
    from test_vfs import MemoryFS

    path = write_wav(tmp_path / "tone.wav", 0.2, 0.2)
    mem = MemoryFS()  # a file system without local paths, like a server
    mem.files["song.wav"] = path.read_bytes()
    ctx = LoadContext(colors=get_scheme("monokai"), show_hidden=False)
    loaded = media.MediaPreviewer().load(PreviewSource(vfs.VPath(mem, "song.wav")), ctx)
    assert loaded.data.waveform_stream is not None  # small: analyzed from its own stream
    loaded.close()

    monkeypatch.setattr(media, "REMOTE_WAVEFORM_LIMIT", 10)
    loaded = media.MediaPreviewer().load(PreviewSource(vfs.VPath(mem, "song.wav")), ctx)
    assert loaded.data.waveform_stream is None
    view = MediaView(themes)
    qtbot.addWidget(view)
    view.show_data(loaded.data)
    assert "large files" in view.waveform.note
    view.clear()
    loaded.close()


def test_player_follows_the_default_output_device(qtbot, themes: ThemeManager, monkeypatch):
    # QAudioOutput keeps the device that was the default when it was created:
    # AirPods connected later still played on the built-in speakers.
    from PySide6.QtMultimedia import QMediaDevices

    outputs = QMediaDevices.audioOutputs()
    if len(outputs) < 2:
        pytest.skip("needs two audio output devices")
    view = MediaView(themes)
    qtbot.addWidget(view)
    current = view.audio.device()
    other = next(o for o in outputs if o.id() != current.id())
    monkeypatch.setattr(QMediaDevices, "defaultAudioOutput", staticmethod(lambda: other))
    view.follow_default_output()
    assert view.audio.device().id() == other.id()
