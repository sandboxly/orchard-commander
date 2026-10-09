"""Video and audio, played with Qt Multimedia (FFmpeg). Files on other file
systems are streamed, not downloaded."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO

from PySide6.QtCore import Qt, QUrl
from PySide6.QtGui import QKeyEvent
from PySide6.QtMultimedia import QAudioOutput, QMediaPlayer
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
    Previewer,
    PreviewView,
    Score,
    ViewContext,
)
from orchard_commander.preview.source import PreviewSource
from orchard_commander.ui.media_stream import StreamDevice


@dataclass(slots=True)
class MediaData:
    video: bool
    name: str
    size: int
    path: Path | None = None  # a local file, or
    stream: BinaryIO | None = None  # an open seekable stream from another file system


class MediaPreviewer(Previewer):
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
            return Loaded(data, f"{fs.human_size(src.size)} · {kind} · streaming", close=stream.close)
        stream.close()  # e.g. inside a compressed archive: needs a local copy
        path = src.local_file()
        return Loaded(MediaData(video, src.name, src.size, path=path), f"{fs.human_size(src.size)} · {kind}")

    def create_view(self, ctx: ViewContext) -> PreviewView:
        return MediaView()


def _seekable(stream: BinaryIO) -> bool:
    try:
        return bool(stream.seekable())
    except (AttributeError, OSError):
        return False


def _fmt_time(ms: int) -> str:
    s = max(ms, 0) // 1000
    h, rem = divmod(s, 3600)
    m, s = divmod(rem, 60)
    return f"{h}:{m:02d}:{s:02d}" if h else f"{m:02d}:{s:02d}"


class MediaView(PreviewView):
    """Video/audio player: Space play/pause, ←/→ seek 5 s, M mute."""

    def __init__(self) -> None:
        super().__init__()
        self.setObjectName("previewPage")
        self.setFocusPolicy(Qt.FocusPolicy.StrongFocus)
        self.player = QMediaPlayer(self)
        self._device: StreamDevice | None = None
        self.audio = QAudioOutput(self)
        self.player.setAudioOutput(self.audio)
        self.video = QVideoWidget()
        self.video.setFocusPolicy(Qt.FocusPolicy.NoFocus)
        self.player.setVideoOutput(self.video)
        # The video surface is a native child window that grabs focus when it
        # appears; give focus straight back so the file list keeps the cursor.
        QApplication.instance().focusChanged.connect(self._reject_video_focus)  # type: ignore[union-attr]
        self.audio_label = QLabel("♪")
        self.audio_label.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self.audio_label.setObjectName("muted")
        f = self.audio_label.font()
        f.setPointSize(f.pointSize() * 5)
        self.audio_label.setFont(f)
        self.screen_stack = QStackedWidget()
        self.screen_stack.addWidget(self.video)
        self.screen_stack.addWidget(self.audio_label)

        self.play_button = self._button("▶", self.toggle_play)
        self.mute_button = self._button("🔇", self.toggle_mute)
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
        row.addWidget(self.mute_button)

        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.addWidget(self.screen_stack, 1)
        layout.addWidget(controls)

        self.player.positionChanged.connect(self._position_changed)
        self.player.durationChanged.connect(lambda d: self.slider.setRange(0, d))
        self.player.playbackStateChanged.connect(self._state_changed)
        self.player.hasVideoChanged.connect(lambda v: self.screen_stack.setCurrentIndex(0 if v else 1))
        self.player.errorOccurred.connect(lambda _e, msg: msg and self.message.emit(f"Cannot play this file:\n{msg}"))

    def show_data(self, data: MediaData) -> None:
        self.stop()
        self.audio.setMuted(True)
        self._update_mute()
        self.player.setLoops(QMediaPlayer.Loops.Infinite if data.video else QMediaPlayer.Loops.Once)
        if data.stream is not None:
            self._device = StreamDevice(data.stream, data.size)
            # The URL only tells the player the format; data comes from the device.
            self.player.setSourceDevice(self._device, QUrl(f"stream:///{data.name}"))
        else:
            assert data.path is not None
            self.player.setSource(QUrl.fromLocalFile(str(data.path)))
        if data.video:  # videos autoplay muted and loop; audio waits for Space
            self.player.play()

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
        if self._device is not None:
            self._device.close()
            self._device = None

    def toggle_play(self) -> None:
        if self.player.playbackState() == QMediaPlayer.PlaybackState.PlayingState:
            self.player.pause()
        else:
            self.player.play()

    def toggle_mute(self) -> None:
        self.audio.setMuted(not self.audio.isMuted())
        self._update_mute()

    def seek(self, delta_ms: int) -> None:
        self.player.setPosition(max(0, min(self.player.position() + delta_ms, self.player.duration())))

    def _update_mute(self) -> None:
        self.mute_button.setText("🔇" if self.audio.isMuted() else "🔊")

    def _position_changed(self, pos: int) -> None:
        if not self.slider.isSliderDown():
            self.slider.setValue(pos)
        self.time_label.setText(f"{_fmt_time(pos)} / {_fmt_time(self.player.duration())}")

    def _state_changed(self, state: QMediaPlayer.PlaybackState) -> None:
        self.play_button.setText("❚❚" if state == QMediaPlayer.PlaybackState.PlayingState else "▶")

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
            case _:
                super().keyPressEvent(event)
