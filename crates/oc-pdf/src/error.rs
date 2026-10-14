use pdfium_render::prelude::{PdfiumError, PdfiumInternalError};

/// Why a PDF couldn't be opened or a page rendered.
#[derive(Debug, thiserror::Error)]
pub enum PdfError {
    /// The PDFium library couldn't be found or loaded.
    #[error("PDF support isn't available: {0}")]
    Unavailable(String),
    /// The document needs a password to open.
    #[error("This PDF is password-protected")]
    Password,
    /// Not a PDF, or a damaged one. The text is PDFium's reason in plain
    /// words: "the file is damaged or isn't a PDF".
    #[error("{0}")]
    Unreadable(String),
    #[error("page {index} doesn't exist (the document has {count})")]
    NoSuchPage { index: usize, count: usize },
    /// A page that PDFium can't load or draw.
    #[error("{0}")]
    Render(String),
    /// The document was closed (or dropped) before the request ran.
    #[error("document closed")]
    Closed,
    /// A newer `render_async` request replaced this one before it started.
    #[error("superseded by a newer request")]
    Cancelled,
    /// PDFium didn't answer in time (a stalled network stream, say).
    #[error("the PDF took longer than {0} s to draw")]
    Timeout(u64),
    /// A blocking call from inside a render callback (it would wait for itself).
    #[error("blocking PDF call from a render callback")]
    Reentrant,
    /// The PDFium thread panicked while working on this request.
    #[error("the PDF couldn't be drawn")]
    Crashed(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// PDFium's error codes in plain words (shown to the user).
fn describe(e: &PdfiumInternalError) -> &'static str {
    match e {
        PdfiumInternalError::FileError => "the file can't be read",
        PdfiumInternalError::FormatError => "the file is damaged or isn't a PDF",
        PdfiumInternalError::PasswordError => "the password is wrong",
        PdfiumInternalError::SecurityError => "its encryption isn't supported",
        PdfiumInternalError::PageError => "the page is missing or damaged",
        PdfiumInternalError::Unknown => "the reason is unknown",
    }
}

impl PdfError {
    /// A failed `FPDF_LoadDocument`-style call.
    pub(crate) fn from_load(e: PdfiumError) -> Self {
        match e {
            PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => PdfError::Password,
            PdfiumError::PdfiumLibraryInternalError(code) => {
                PdfError::Unreadable(describe(&code).to_owned())
            }
            PdfiumError::IoError(io) => PdfError::Io(io),
            other => {
                log::warn!("PDF load failed: {other:?}");
                PdfError::Unreadable(describe(&PdfiumInternalError::Unknown).to_owned())
            }
        }
    }

    /// A failed page load or render.
    pub(crate) fn from_page(index: usize, e: PdfiumError) -> Self {
        match e {
            PdfiumError::PdfiumLibraryInternalError(code) => {
                PdfError::Render(format!("page {} can't be drawn: {}", index + 1, describe(&code)))
            }
            other => {
                log::warn!("PDF page {} failed: {other:?}", index + 1);
                PdfError::Render(format!("page {} can't be drawn", index + 1))
            }
        }
    }
}
