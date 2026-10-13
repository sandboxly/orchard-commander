//! What a media file contains: tags, duration, streams.

use std::time::Duration;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct VideoInfo {
    /// Coded size in pixels.
    pub width: u32,
    pub height: u32,
    /// Size on screen (non-square pixels applied), what frames are scaled to.
    pub display_width: u32,
    pub display_height: u32,
    /// Frames per second, when the file says.
    pub fps: Option<f64>,
    /// FFmpeg's codec name: "h264", "hevc", "vp9", …
    pub codec: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioInfo {
    /// "mp3", "aac", "flac", …
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Tags and stream facts of a media file. Tags are trimmed; empty ones are None.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaInfo {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<Duration>,
    /// FFmpeg's container name ("mov,mp4,m4a,3gp,3g2,mj2", "mp3", "matroska,webm").
    pub container: String,
    /// The video stream; cover art in audio files doesn't count.
    pub video: Option<VideoInfo>,
    pub audio: Option<AudioInfo>,
    /// Whether the player can seek in it.
    pub seekable: bool,
}

impl MediaInfo {
    pub fn has_video(&self) -> bool {
        self.video.is_some()
    }

    /// The headline of the audio screen: the title tag, else the file name.
    pub fn display_title(&self, file_name: &str) -> String {
        self.title.clone().unwrap_or_else(|| file_name.to_string())
    }

    /// The line below: "Artist · Album" (the album artist when there is no
    /// artist), or "" without either.
    pub fn display_subtitle(&self) -> String {
        let artist = self.artist.as_deref().or(self.album_artist.as_deref());
        [artist, self.album.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" · ")
    }

    pub fn duration_ms(&self) -> Option<i64> {
        self.duration.map(|d| d.as_millis() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_and_subtitle_like_the_player() {
        let mut info = MediaInfo::default();
        assert_eq!(info.display_title("song.mp3"), "song.mp3");
        assert_eq!(info.display_subtitle(), "");
        info.album_artist = Some("Band".into());
        info.album = Some("Album".into());
        assert_eq!(info.display_subtitle(), "Band · Album");
        info.artist = Some("Singer".into());
        info.title = Some("Song".into());
        assert_eq!((info.display_title("x"), info.display_subtitle()), ("Song".into(), "Singer · Album".into()));
    }
}
