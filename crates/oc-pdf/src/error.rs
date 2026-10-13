use pdfium_render::prelude::{PdfiumError, PdfiumInternalError};

/// Why a PDF couldn't be opened or a page rendered.
#[derive(Debug, thiserror::Error)]
pub enum PdfError {
    /// The PDFium library couldn't be found or loaded.
    #[error("PDFium is not available: {0}")]
    Unavailable(String),
    /// The document needs a password to open.
    #[error("This PDF is password-protected.")]
    Password,
    /// Not a PDF, or a damaged one. The text is PDFium's reason, worded like
    /// pypdfium2: "Failed to load document (PDFium: Data format error)."
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
    #[error("PDFium didn't answer within {0} s")]
    Timeout(u64),
    /// A blocking call from inside a render callback (it would wait for itself).
    #[error("blocking PDF call from a render callback")]
    Reentrant,
    /// The PDFium thread panicked while working on this request.
    #[error("the PDF renderer failed: {0}")]
    Crashed(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// pypdfium2's names for PDFium's error codes (`ErrorToStr`).
fn describe(e: &PdfiumInternalError) -> &'static str {
    match e {
        PdfiumInternalError::FileError => "File access error",
        PdfiumInternalError::FormatError => "Data format error",
        PdfiumInternalError::PasswordError => "Incorrect password error",
        PdfiumInternalError::SecurityError => "Unsupported security scheme error",
        PdfiumInternalError::PageError => "Page not found or content error",
        PdfiumInternalError::Unknown => "Unknown error",
    }
}

impl PdfError {
    /// A failed `FPDF_LoadDocument`-style call.
    pub(crate) fn from_load(e: PdfiumError) -> Self {
        match e {
            PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => PdfError::Password,
            PdfiumError::PdfiumLibraryInternalError(code) => {
                PdfError::Unreadable(format!("Failed to load document (PDFium: {}).", describe(&code)))
            }
            PdfiumError::IoError(io) => PdfError::Io(io),
            other => PdfError::Unreadable(format!("Failed to load document ({other:?}).")),
        }
    }

    /// A failed page load or render.
    pub(crate) fn from_page(index: usize, e: PdfiumError) -> Self {
        match e {
            PdfiumError::PdfiumLibraryInternalError(code) => {
                PdfError::Render(format!("Failed to load page {} (PDFium: {}).", index + 1, describe(&code)))
            }
            other => PdfError::Render(format!("Failed to render page {} ({other:?}).", index + 1)),
        }
    }
}
