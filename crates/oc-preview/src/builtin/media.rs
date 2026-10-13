//! PDFs and audio/video: the files are located here (a local path, or a
//! seekable stream so nothing is downloaded first); rendering and playback
//! are the UI's job.

use oc_vfs::{ReadStream, human_size};

use crate::api::{Access, FileInfo, LoadContext, Loaded, PreviewError, Previewer, Score, ext_for_name};
use crate::content::{AUDIO_EXTS, VIDEO_EXTS};
use crate::data::{DocumentSource, MediaData, PdfData, PreviewData};
use crate::source::PreviewSource;

use super::MAX_FETCH_BYTES;

/// A waveform reads the whole file; from servers, only up to this size.
pub const REMOTE_WAVEFORM_LIMIT: u64 = 100 * 1024 * 1024;

/// Where PDF readers look for the "%PDF-" header.
const PDF_HEADER_WINDOW: usize = 1024;

/// A seekable stream, or None after closing an unseekable one.
fn seekable_stream(src: &PreviewSource) -> Result<Option<Box<dyn ReadStream>>, PreviewError> {
    let stream = src.open_stream()?;
    Ok(stream.seekable().then_some(stream))
}

pub struct PdfPreviewer;

impl Previewer for PdfPreviewer {
    fn id(&self) -> &str {
        "pdf"
    }

    fn title(&self) -> &str {
        "PDF"
    }

    fn access(&self) -> Access {
        Access::Stream
    }

    fn max_size(&self) -> Option<u64> {
        Some(MAX_FETCH_BYTES)
    }

    /// Recognise PDFs without a .pdf extension.
    fn needs_head(&self) -> bool {
        true
    }

    fn reload_on_theme_change(&self) -> bool {
        false
    }

    fn score(&self, f: &FileInfo) -> i32 {
        if f.is_dir || f.is_empty() {
            return Score::NO;
        }
        if f.ext == "pdf" || f.head.as_deref().is_some_and(|h| h.starts_with(b"%PDF-")) {
            Score::FORMAT
        } else {
            Score::NO
        }
    }

    fn load(&self, src: &PreviewSource, _ctx: &LoadContext) -> Result<Loaded, PreviewError> {
        // Cheap sanity check until rendering lives here: garbage falls back to
        // hex. Like PDFium, accept the header anywhere in the first 1 KB
        // (some generators put junk before it).
        if !src.with_head(PDF_HEADER_WINDOW, |h| h.windows(5).any(|w| w == b"%PDF-"))? {
            return Err(PreviewError::cannot("Not a readable PDF: no PDF header"));
        }
        let source = match src.direct_path() {
            Some(p) => DocumentSource::Path(p.to_path_buf()),
            None => match seekable_stream(src)? {
                Some(s) => DocumentSource::Stream(s), // the renderer reads only the parts it needs
                None => DocumentSource::Path(src.local_file(self.max_size())?),
            },
        };
        Ok(Loaded::new(PreviewData::Pdf(PdfData { source }), human_size(src.size())))
    }
}

/// Tags and stream facts for the player, read on the loader's thread
/// (streams are rewound for the player).
#[cfg(feature = "media")]
fn probe(source: &mut DocumentSource, src: &PreviewSource) -> Option<Box<oc_media::MediaInfo>> {
    let result = match source {
        DocumentSource::Path(p) => oc_media::probe(p),
        DocumentSource::Stream(s) => oc_media::probe_stream(&mut **s, Some(src.size()), src.name()),
    };
    result.map(Box::new).map_err(|e| log::debug!("media info of {}: {e}", src.name())).ok()
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
        #[cfg_attr(not(feature = "media"), allow(unused_mut))]
        let media = |mut source, waveform_stream| MediaData {
            video,
            name: src.name().to_string(),
            size: src.size(),
            #[cfg(feature = "media")]
            info: probe(&mut source, src),
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
