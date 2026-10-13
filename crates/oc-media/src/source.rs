//! Where media comes from.

use std::fmt;
use std::path::PathBuf;

use oc_vfs::ReadStream;

/// A local file, or an open stream from another file system (SFTP, SMB,
/// WebDAV, S3, an archive member). Seekable streams are read on demand with
/// read-ahead; unseekable ones play from the start without seeking.
pub enum MediaSource {
    Path(PathBuf),
    Stream {
        stream: Box<dyn ReadStream>,
        /// The file's size, if known (lets FFmpeg seek relative to the end).
        size: Option<u64>,
        /// The file name: a hint for the format (".mp3").
        name: String,
    },
}

impl MediaSource {
    pub fn stream(stream: Box<dyn ReadStream>, size: Option<u64>, name: impl Into<String>) -> Self {
        MediaSource::Stream { stream, size, name: name.into() }
    }

    /// Whether playback can seek (always for local files).
    pub fn seekable(&self) -> bool {
        match self {
            MediaSource::Path(_) => true,
            MediaSource::Stream { stream, .. } => stream.seekable(),
        }
    }
}

impl fmt::Debug for MediaSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MediaSource::Path(p) => write!(f, "Path({})", p.display()),
            MediaSource::Stream { size, name, .. } => write!(f, "Stream({name:?}, {size:?} bytes)"),
        }
    }
}

impl From<PathBuf> for MediaSource {
    fn from(p: PathBuf) -> Self {
        MediaSource::Path(p)
    }
}
