//! PDF documents, rendered with PDFium (crate oc-pdf; port of
//! `preview/builtin/pdf.py`).
//!
//! Documents are read from a seekable stream when the file system offers one,
//! so a PDF on a server is not downloaded first; PDFium fetches only the parts
//! it needs. Page 1 is rendered during load; the view renders pages at the
//! pane's exact size through [`PdfPages`].

use std::sync::Arc;

use oc_pdf::{Document, PdfError, Priority};
use oc_vfs::human_size;

use crate::api::{Access, FileInfo, LoadContext, Loaded, PreviewError, Previewer, Score};
use crate::data::{PDF_FIRST_PAGE_WIDTH, PageRenderer, PdfData, PdfPages, PreviewData, RenderDone, RenderedPage};
use crate::source::PreviewSource;

use super::MAX_FETCH_BYTES;

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

    /// PDFs without a direct path or a seekable stream are fetched whole.
    fn max_size(&self) -> Option<u64> {
        Some(MAX_FETCH_BYTES)
    }

    /// Recognise PDFs without a .pdf extension.
    fn needs_head(&self) -> bool {
        true
    }

    /// The view follows theme changes itself.
    fn reload_on_theme_change(&self) -> bool {
        false
    }

    /// Without the PDFium library the previewer is skipped (PDFs get hex).
    fn available(&self) -> bool {
        oc_pdf::available()
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
        let doc = match src.direct_path() {
            Some(path) => Document::open_path(path),
            None => {
                let stream = src.open_stream()?;
                if stream.seekable() {
                    Document::open_reader(stream) // PDFium reads only the parts it needs
                } else {
                    drop(stream);
                    Document::open_path(&src.local_file(self.max_size())?)
                }
            }
        }
        .map_err(preview_error)?;
        // Dropping `doc` on an error below closes it.
        let page_count = doc.page_count();
        if page_count == 0 {
            return Err(PreviewError::cannot("PDF without pages"));
        }
        let first_page = doc.render_width(0, PDF_FIRST_PAGE_WIDTH).map_err(preview_error)?;
        let title = doc.title().to_string();
        let pages = format!("{page_count} page{}", if page_count == 1 { "" } else { "s" });
        let info = [pages, human_size(src.size()), one_line(&title)]
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        let data = PdfData { page_count, title, first_page, pages: PdfPages::new(Pages(doc)) };
        Ok(Loaded::new(PreviewData::Pdf(data), info))
    }
}

/// A title on the status line: line breaks and other control characters
/// become spaces.
fn one_line(s: &str) -> String {
    s.split(char::is_control).filter(|p| !p.trim().is_empty()).map(str::trim).collect::<Vec<_>>().join(" ")
}

/// What the registry and the UI make of oc-pdf's errors (Python's
/// `ShowMessage` / `CannotPreview`).
fn preview_error(e: PdfError) -> PreviewError {
    match e {
        PdfError::Password => PreviewError::message(e.to_string()),
        PdfError::Unreadable(msg) | PdfError::Render(msg) => PreviewError::cannot(format!("Not a readable PDF: {msg}")),
        PdfError::Io(io) => io.into(),
        PdfError::Unavailable(_) | PdfError::Closed | PdfError::Cancelled | PdfError::NoSuchPage { .. } => {
            PreviewError::cannot(e.to_string())
        }
        PdfError::Timeout(_) | PdfError::Reentrant | PdfError::Crashed(_) => PreviewError::Failed(e.to_string()),
    }
}

struct Pages(Document);

impl PageRenderer for Pages {
    fn page_count(&self) -> usize {
        self.0.page_count()
    }

    fn page_size(&self, index: usize) -> Result<(f32, f32), PreviewError> {
        self.0.page_size(index).map_err(preview_error)
    }

    fn render(&self, index: usize, width: u32, height: u32) -> Result<Arc<RenderedPage>, PreviewError> {
        self.0.render(index, width, height).map_err(preview_error)
    }

    fn cached(&self, index: usize, width: u32, height: u32) -> Option<Arc<RenderedPage>> {
        self.0.cached(index, width, height)
    }

    fn render_async(&self, index: usize, width: u32, height: u32, done: RenderDone) {
        self.0.render_async(index, width, height, Priority::Show, move |r| done(r.map_err(preview_error)));
    }

    fn prefetch(&self, index: usize, width: u32, height: u32) {
        self.0.prefetch(index, width, height);
    }

    fn close(&self) {
        self.0.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_fit_on_one_line() {
        assert_eq!(one_line("Annual\r\nreport\t2026 "), "Annual report 2026");
        assert_eq!(one_line("\n"), "");
    }

    #[test]
    fn errors_map_like_python() {
        let e = preview_error(PdfError::Password);
        assert!(matches!(&e, PreviewError::ShowMessage(m) if m == "This PDF is password-protected."));
        let e = preview_error(PdfError::Unreadable("Failed to load document (PDFium: Data format error).".into()));
        assert_eq!(e.to_string(), "Not a readable PDF: Failed to load document (PDFium: Data format error).");
        assert!(matches!(e, PreviewError::CannotPreview(_)));
        assert!(matches!(preview_error(PdfError::Closed), PreviewError::CannotPreview(m) if m == "document closed"));
        assert!(matches!(preview_error(PdfError::Timeout(120)), PreviewError::Failed(_)));
    }
}
