//! The audio/video engine behind the preview player, independent of any UI
//! toolkit.
//!
//! - [`Player`] decodes a local file or an [`oc_vfs::ReadStream`] (streamed
//!   from a server or archive, never downloaded first) with FFmpeg, plays its
//!   audio through the system's default output device (cpal) and keeps video
//!   in sync with the audio clock. The UI polls [`Player::video_frame`] on
//!   redraw (RGBA, already scaled to the size it asked for) and reads
//!   [`Event`]s for state changes, position ticks and the end of a file.
//! - [`WaveformAnalyzer`] decodes a whole audio file in the background into
//!   per-20 ms peak/RMS levels, the data the waveform seek bar draws.
//! - [`SpectrumAnalyzer`] holds the samples being played (tapped from the
//!   audio output) and turns them into band levels; [`SpectrumAnimator`] adds
//!   the bars' rise/fall and falling peak caps.
//! - [`prefs`] are the remembered player settings (mute, autoplay) in
//!   `Settings.extra`, [`playlist`] the "play the next file" rules.
//!
//! Port of `preview/builtin/media.py`, `waveform.py`, `spectrum.py` and
//! `ui/media_stream.py` in the Python version (minus the widgets).
//!
//! Dropping a [`Player`] or [`WaveformAnalyzer`] never waits for its threads:
//! they are told to stop and wind down on their own (closing streams and the
//! audio device off the UI thread).
//!
//! # Wiring a player view (what 1.x's `MediaView` did)
//!
//! - On a new file: drop the old player/analyzer, then
//!   `Player::open(source, PlayerOptions { muted: prefs.muted, autoplay: prefs.autoplay,
//!   looping: should_loop(is_video, settings.play_next), video_size, waker, .. })`
//!   with [`MediaPrefs::from_extra`]. With oc-preview's `media` feature,
//!   `MediaData::into_sources()` gives the player's and the waveform's sources;
//!   audio without a waveform source shows [`waveform::REMOTE_NOTE`].
//! - Audio files: title [`MediaInfo::display_title`], below it
//!   [`MediaInfo::display_subtitle`], the spectrum (animate at
//!   [`spectrum::FPS`] while playing: `animator.tick(dt, player.spectrum().levels(n), n)`),
//!   and the waveform as the seek bar ([`WaveformLevels::columns`],
//!   [`waveform::x_to_ms`]). Videos: [`Player::video_frame`] plus a slider.
//! - Keys: Space/K [`Player::toggle_play`], ←/→ [`Player::seek_by`] ±[`SEEK_STEP_MS`],
//!   M [`Player::toggle_mute`] (store with [`MediaPrefs::set`]), A toggles
//!   autoplay (stored the same way). Time readout: [`format_time`].
//! - [`Event::Finished`]: the host plays the next file (see [`playlist`]);
//!   [`Event::Error`]: show [`error_message`].

pub mod info;
pub mod playlist;
pub mod prefs;
pub mod source;
pub mod spectrum;
pub mod time;
pub mod waveform;

#[cfg(feature = "ffmpeg")]
mod ffmpeg;
#[cfg(feature = "ffmpeg")]
mod player;
mod threads;

pub use info::{AudioInfo, MediaInfo, VideoInfo};
pub use playlist::{SEEK_STEP_MS, next_to_play, should_loop};
pub use prefs::MediaPrefs;
pub use source::MediaSource;
pub use spectrum::{Fft, SpectrumAnalyzer, SpectrumAnimator, band_edges, band_levels};
pub use threads::ShutdownWatch;
pub use time::format_time;
pub use waveform::{BLOCK_MS, WaveformAnalyzer, WaveformLevels, WaveformOptions, block_levels};

#[cfg(feature = "ffmpeg")]
pub use ffmpeg::{PROBE_BUDGET, probe, probe_stream};
#[cfg(feature = "ffmpeg")]
pub use player::{AudioOutput, Event, MediaStatus, PlaybackState, Player, PlayerOptions, VideoFrame};

use std::sync::Arc;

/// Called from the player's and analyzer's threads when something changed
/// that the UI should look at: an event was queued, a waveform grew, the
/// first frame arrived. Use it to wake the UI's event loop (e.g.
/// `ctx.request_repaint()`); keep it cheap. It is never called with one of
/// oc-media's locks held, nor from calls the UI makes ([`Player::play`] & co.
/// only queue their events), so it may take the UI's own locks.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// FFmpeg couldn't open or decode the file; the text is FFmpeg's.
    #[error("{0}")]
    Decode(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// Built without the `ffmpeg` feature.
    #[error("media support is not built in")]
    Unsupported,
}

/// The message the player shows when a file can't be played (as 1.x did).
pub fn error_message(detail: &str) -> String {
    format!("Cannot play this file:\n{detail}")
}
