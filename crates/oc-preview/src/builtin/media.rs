//! Audio/video: the files are located here (a local path, or a
//! seekable stream so nothing is downloaded first); rendering and playback
//! are the UI's job.

use oc_vfs::{ReadStream, human_size};

use crate::api::{Access, FileInfo, LoadContext, Loaded, PreviewError, Previewer, Score, ext_for_name};
use crate::content::{AUDIO_EXTS, VIDEO_EXTS};
use crate::data::{DocumentSource, MediaData, PreviewData};
use crate::source::PreviewSource;

/// A waveform reads the whole file; from servers, only up to this size.
pub const REMOTE_WAVEFORM_LIMIT: u64 = 100 * 1024 * 1024;

/// A seekable stream, or None after closing an unseekable one.
fn seekable_stream(src: &PreviewSource) -> Result<Option<Box<dyn ReadStream>>, PreviewError> {
    let stream = src.open_stream()?;
    Ok(stream.seekable().then_some(stream))
}

pub struct MediaPreviewer;

impl Previewer for MediaPreviewer {
    fn id(&self) -> &str {
        "media"
    }

    fn title(&self) -> &str {
        "Player"
    }

    fn access(&self) -> Access {
        Access::Stream
    }

    fn reload_on_theme_change(&self) -> bool {
        false
    }

    fn score(&self, f: &FileInfo) -> i32 {
        if f.is_dir {
            return Score::NO;
        }
        let ext = f.ext.as_str();
        if VIDEO_EXTS.contains(&ext) || AUDIO_EXTS.contains(&ext) { Score::FORMAT } else { Score::NO }
    }

    fn load(&self, src: &PreviewSource, _ctx: &LoadContext) -> Result<Loaded, PreviewError> {
        let video = VIDEO_EXTS.contains(&ext_for_name(src.name()).as_str());
        let kind = if video { "video" } else { "audio" };
        let size = human_size(src.size());
        let media = |source, waveform_stream| MediaData {
            video,
            name: src.name().to_string(),
            size: src.size(),
            source,
            waveform_stream,
        };
        if let Some(p) = src.direct_path() {
            let data = media(DocumentSource::Path(p.to_path_buf()), None);
            return Ok(Loaded::new(PreviewData::Media(data), format!("{size} · {kind}")));
        }
        // No direct path: stream rather than fetch it all.
        if let Some(stream) = seekable_stream(src)? {
            let waveform = if !video && src.size() <= REMOTE_WAVEFORM_LIMIT { Some(src.open_stream()?) } else { None };
            let data = media(DocumentSource::Stream(stream), waveform);
            return Ok(Loaded::new(PreviewData::Media(data), format!("{size} · {kind} · streaming")));
        }
        // E.g. inside a compressed archive: needs a local copy.
        let data = media(DocumentSource::Path(src.local_file(None)?), None);
        Ok(Loaded::new(PreviewData::Media(data), format!("{size} · {kind}")))
    }
}
