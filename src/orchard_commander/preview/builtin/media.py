"""Video and audio, played with Qt Multimedia (FFmpeg). Files on other file
systems are streamed, not downloaded."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO

from PySide6.QtCore import Qt, QTimer, QUrl
from PySide6.QtGui import QKeyEvent
from PySide6.QtMultimedia import QAudioBufferOutput, QAudioOutput, QMediaDevices, QMediaMetaData, QMediaPlayer
from PySide6.QtMultimediaWidgets import QVideoWidget
from PySide6.QtWidgets import (
    QApplication,
    QHBoxLayout,
    QLabel,
    QSlider,
    QStackedWidget,
    QToolButton,
    QVBoxLayout,
    QWidget,
)

from orchard_commander import fs
from orchard_commander.preview import content
from orchard_commander.preview.api import (
    Access,
    FileInfo,
    LoadContext,
    Loaded,
    PreviewStore,
    PreviewView,
    Previewer,
    Score,
    ViewContext,
)
from orchard_commander.preview.builtin.spectrum import SpectrumAnalyzer, SpectrumWidget
from orchard_commander.preview.builtin.waveform import WaveformAnalyzer, WaveformWidget, format_time
from orchard_commander.preview.source import PreviewSource
from orchard_commander.ui.media_stream import StreamDevice
from orchard_commander.ui.style import ThemeManager

# A waveform reads the whole file; from servers, only up to this size.
REMOTE_WAVEFORM_LIMIT = 100 * 1024**2


@dataclass(slots=True)
class MediaData:
    video: bool
    name: str
    size: int
    path: Path | None = None  # a local file, or
    stream: BinaryIO | None = None  # an open seekable stream from another file system
    waveform_stream: BinaryIO | None = None  # audio from a stream: a second one for the waveform


class MediaPreviewer(Previewer):
    reload_on_theme_change = False  # the view follows theme changes itself
    id = "media"
    title = "Player"
    access = Access.STREAM

    def score(self, f: FileInfo) -> int:
        if f.is_dir:
            return Score.NO
        return Score.FORMAT if f.ext in content.VIDEO_EXTS or f.ext in content.AUDIO_EXTS else Score.NO

    def load(self, src: PreviewSource, ctx: LoadContext) -> Loaded:
        video = src.name.rpartition(".")[2].lower() in content.VIDEO_EXTS
        kind = "video" if video else "audio"
        if (path := src.direct_path) is not None:
            return Loaded(MediaData(video, src.name, src.size, path=path), f"{fs.human_size(src.size)} · {kind}")
        stream = src.open_stream()  # no direct path: stream rather than fetch it all
        if _seekable(stream):
            data = MediaData(video, src.name, src.size, stream=stream)
            if not video and src.size <= REMOTE_WAVEFORM_LIMIT:
                data.waveform_stream = src.open_stream()

            def close() -> None:
                for s in (stream, data.waveform_stream):
                    if s is not None:
                        s.close()

            return Loaded(data, f"{fs.human_size(src.size)} · {kind} · streaming", close=close)
        stream.close()  # e.g. inside a compressed archive: needs a local copy
        path = src.local_file()
        return Loaded(MediaData(video, src.name, src.size, path=path), f"{fs.human_size(src.size)} · {kind}")

    def create_view(self, ctx: ViewContext) -> PreviewView:
        return MediaView(ctx.themes, ctx.store)


def _seekable(stream: BinaryIO) -> bool:
    try:
        return bool(stream.seekable())
    except (AttributeError, OSError):
        return False


class MediaView(PreviewView):
    """Video/audio player: Space play/pause, ←/→ seek 5 s, M mute, A autoplay on/off."""

    def __init__(self, themes: ThemeManager, store: PreviewStore | None = None) -> None:
        super().__init__()
        self.setObjectName("previewPage")
        self.setFocusPolicy(Qt.FocusPolicy.StrongFocus)
        self._store = store or PreviewStore()
        self._continuous = False  # the host plays the next file at the end: don't loop videos
        self.player = QMediaPlayer(self)
        self._device: StreamDevice | None = None
        self.audio = QAudioOutput(self)
        self.player.setAudioOutput(self.audio)
        # QAudioOutput keeps the device that was the default when it was created;
        # follow the system's choice instead (AirPods connecting, Control Center).
        self._media_devices = QMediaDevices(self)
        self._media_devices.audioOutputsChanged.connect(self.follow_default_output)
        self._output_check = QTimer(self, interval=1500)  # while playing: switches in Control Center
        self._output_check.timeout.connect(self.follow_default_output)
        self.video = QVideoWidget()
        self.video.setFocusPolicy(Qt.FocusPolicy.NoFocus)
        self.player.setVideoOutput(self.video)
        # The video surface is a native child window that grabs focus when it
        # appears; give focus straight back so the file list keeps the cursor.
        QApplication.instance().focusChanged.connect(self._reject_video_focus)  # type: ignore[union-attr]
        self._name = ""
        self._is_video = False
        # Audio: title and artist, a live spectrum of what's playing, and below it
        # the whole file's waveform, which is also the seek bar.
        self.spectrum_analyzer = SpectrumAnalyzer(self)
        self.spectrum = SpectrumWidget(themes, self.spectrum_analyzer)
        self.buffer_output = QAudioBufferOutput(self)
        self.player.setAudioBufferOutput(self.buffer_output)
        self.buffer_output.audioBufferReceived.connect(self._audio_played)
        self.analyzer = WaveformAnalyzer(self)
        self.waveform = WaveformWidget(themes, self.analyzer)
        self.waveform.seek_requested.connect(self.player.setPosition)
        self.title_label = QLabel()
        self.title_label.setObjectName("dialogTitle")
        self.artist_label = QLabel()
        self.artist_label.setObjectName("muted")
        audio_screen = QWidget()
        audio_screen.setObjectName("previewPage")
        audio_layout = QVBoxLayout(audio_screen)
        for label in (self.title_label, self.artist_label):
            label.setAlignment(Qt.AlignmentFlag.AlignCenter)
            label.setTextFormat(Qt.TextFormat.PlainText)
        line = self.fontMetrics().height()
        audio_layout.addWidget(self.title_label)
        audio_layout.addWidget(self.artist_label)
        audio_layout.addSpacing(line // 2)
        audio_layout.addWidget(self.spectrum, 1)
        audio_layout.addSpacing(line // 2)
        self.waveform.setFixedHeight(round(line * 4.5))
        audio_layout.addWidget(self.waveform)
        self.screen_stack = QStackedWidget()
        self.screen_stack.addWidget(self.video)
        self.screen_stack.addWidget(audio_screen)

        self.play_button = self._button("▶", self.toggle_play)
        self.mute_button = self._button("🔇", self.toggle_mute)
        self.autoplay_button = self._button("Autoplay", self.toggle_autoplay)
        self.autoplay_button.setCheckable(True)
        self._update_autoplay()
        self.autoplay_button.setToolTip("Start playing as soon as a file is selected (A)")
        self.slider = QSlider(Qt.Orientation.Horizontal)
        self.slider.setFocusPolicy(Qt.FocusPolicy.NoFocus)
        self.slider.sliderMoved.connect(self.player.setPosition)
        self.time_label = QLabel("00:00 / 00:00")

        controls = QWidget()
        controls.setObjectName("mediaControls")
        row = QHBoxLayout(controls)
        row.setContentsMargins(0, 4, 0, 0)
        row.addWidget(self.play_button)
        row.addWidget(self.slider, 1)
        row.addWidget(self.time_label)
        row.addWidget(self.autoplay_button)
        row.addWidget(self.mute_button)

        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.addWidget(self.screen_stack, 1)
        layout.addWidget(controls)

        self.player.positionChanged.connect(self._position_changed)
        self.player.durationChanged.connect(lambda d: self.slider.setRange(0, d))
        self.player.durationChanged.connect(self.waveform.set_duration)
        self.player.metaDataChanged.connect(self._metadata_changed)
        self.player.playbackStateChanged.connect(self._state_changed)
        self.player.mediaStatusChanged.connect(self._status_changed)
        self.player.hasVideoChanged.connect(lambda v: self.screen_stack.setCurrentIndex(0 if v else 1))
        self.player.errorOccurred.connect(lambda _e, msg: msg and self.message.emit(f"Cannot play this file:\n{msg}"))

    def show_data(self, data: MediaData) -> None:
        self.stop()
        self.audio.setMuted(bool(self._store.get("muted", True)))  # remembered; muted until first unmuted
        self._update_mute()
        self._name = data.name
        self._is_video = data.video
        self._apply_loops()
        self.spectrum.reset()
        self.spectrum_analyzer.reset()
        self.slider.setVisible(data.video)  # audio seeks on the waveform
        self.screen_stack.setCurrentIndex(0 if data.video else 1)
        self.waveform.set_duration(0)
        self.waveform.set_position(0)
        self._metadata_changed()
        if not data.video:
            if data.path is not None:
                self.analyzer.start(path=data.path)
                self.waveform.set_note("")
            elif data.waveform_stream is not None:
                self.analyzer.start(stream=data.waveform_stream, size=data.size)
                self.waveform.set_note("")
            else:
                self.waveform.set_note("No waveform for large files on servers")
        if data.stream is not None:
            self._device = StreamDevice(data.stream, data.size)
            # The URL only tells the player the format; data comes from the device.
            self.player.setSourceDevice(self._device, QUrl(f"stream:///{data.name}"))
        else:
            assert data.path is not None
            self.player.setSource(QUrl.fromLocalFile(str(data.path)))
        if self.autoplay:  # otherwise Space starts it (or the host, when it moves on to this file)
            self.play()

    def play(self) -> None:
        self.follow_default_output()
        self.player.play()

    def set_continuous(self, on: bool) -> None:
        self._continuous = on
        self._apply_loops()

    def _apply_loops(self) -> None:
        loop = self._is_video and not self._continuous
        self.player.setLoops(QMediaPlayer.Loops.Infinite if loop else QMediaPlayer.Loops.Once)

    def _status_changed(self, status: QMediaPlayer.MediaStatus) -> None:
        if status == QMediaPlayer.MediaStatus.EndOfMedia:
            self.finished.emit()

    def clear(self) -> None:
        self.stop()

    def _reject_video_focus(self, old: QWidget | None, new: QWidget | None) -> None:
        if new is not None and self.video.isAncestorOf(new):
            if old is not None and not self.video.isAncestorOf(old):
                old.setFocus()
            else:
                self.setFocus()

    def _button(self, text: str, slot) -> QToolButton:
        b = QToolButton()
        b.setText(text)
        b.setObjectName("fkey")
        b.setFocusPolicy(Qt.FocusPolicy.NoFocus)
        b.clicked.connect(slot)
        return b

    def stop(self) -> None:
        self.player.stop()
        self.player.setSource(QUrl())
        self.analyzer.stop()
        self.spectrum.reset()
        self.spectrum_analyzer.reset()
        if self._device is not None:
            self._device.close()
            self._device = None

    def toggle_play(self) -> None:
        if self.player.playbackState() == QMediaPlayer.PlaybackState.PlayingState:
            self.player.pause()
        else:
            self.follow_default_output()
            self.player.play()

    def follow_default_output(self) -> None:
        """Play on the system's current default output device."""
        default = QMediaDevices.defaultAudioOutput()
        if not default.isNull() and default.id() != self.audio.device().id():
            self.audio.setDevice(default)

    @property
    def autoplay(self) -> bool:
        return bool(self._store.get("autoplay", True))

    def toggle_autoplay(self) -> None:
        self._store.set("autoplay", not self.autoplay)  # for the next file, and the next run
        self._update_autoplay()

    def store_changed(self) -> None:
        self._update_autoplay()
        self.audio.setMuted(bool(self._store.get("muted", True)))
        self._update_mute()

    def _update_autoplay(self) -> None:
        self.autoplay_button.setChecked(self.autoplay)
        self.autoplay_button.setText(f"[{'x' if self.autoplay else ' '}] Autoplay")

    def toggle_mute(self) -> None:
        self.audio.setMuted(not self.audio.isMuted())
        self._store.set("muted", self.audio.isMuted())  # for the next file, and the next run
        self._update_mute()

    def seek(self, delta_ms: int) -> None:
        self.player.setPosition(max(0, min(self.player.position() + delta_ms, self.player.duration())))

    def _update_mute(self) -> None:
        self.mute_button.setText("🔇" if self.audio.isMuted() else "🔊")

    def _position_changed(self, pos: int) -> None:
        if not self.slider.isSliderDown():
            self.slider.setValue(pos)
        self.waveform.set_position(pos)
        self.time_label.setText(f"{format_time(pos)} / {format_time(self.player.duration())}")

    def _metadata_changed(self) -> None:
        meta = self.player.metaData()
        title = meta.stringValue(QMediaMetaData.Key.Title) or self._name
        artist = meta.stringValue(QMediaMetaData.Key.ContributingArtist) or meta.stringValue(
            QMediaMetaData.Key.AlbumArtist
        )
        album = meta.stringValue(QMediaMetaData.Key.AlbumTitle)
        self.title_label.setText(title)
        self.artist_label.setText(" · ".join(x for x in (artist, album) if x))

    def _state_changed(self, state: QMediaPlayer.PlaybackState) -> None:
        playing = state == QMediaPlayer.PlaybackState.PlayingState
        self.play_button.setText("❚❚" if playing else "▶")
        if playing:
            self.follow_default_output()
            self._output_check.start()
        else:
            self._output_check.stop()
        if not self._is_video:
            self.spectrum.set_active(playing)

    def _audio_played(self, buffer) -> None:
        if not self._is_video:
            self.spectrum_analyzer.feed(buffer)

    def keyPressEvent(self, event: QKeyEvent) -> None:
        match event.key():
            case Qt.Key.Key_Space | Qt.Key.Key_K:
                self.toggle_play()
            case Qt.Key.Key_Left:
                self.seek(-5000)
            case Qt.Key.Key_Right:
                self.seek(5000)
            case Qt.Key.Key_M:
                self.toggle_mute()
            case Qt.Key.Key_A:
                self.toggle_autoplay()
            case _:
                super().keyPressEvent(event)
