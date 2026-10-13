//! File previews, independent of any UI toolkit.
//!
//! A [`Previewer`] declares which files it handles ([`Previewer::score`]) and
//! loads them on a worker thread into plain [`PreviewData`] the UI renders.
//! The [`Registry`] picks the best previewer for a file — by name first,
//! reading the file's first bytes only when that could change the outcome —
//! and falls back to the next one when a previewer can't handle it.
//! [`PreviewSource`] gives previewers bytes, streams or local paths only
//! through the generic [`oc_vfs::FileSystem`] interface, so every previewer
//! works on every file system.
//!
//! Port of `preview/` and `textfile.py` in the Python version.

pub mod api;
pub mod builtin;
pub mod content;
pub mod data;
pub mod highlight;
pub mod markdown;
pub mod registry;
pub mod source;
pub mod textfile;

pub use api::{
    AUTO_DOWNLOAD_LIMIT, Access, ArchiveOpener, Candidate, FileInfo, LoadContext, Loaded, Outcome, PreviewError,
    Previewer, Score, ext_for_name, fetch_timeout,
};
pub use content::{Kind, TextContent};
pub use data::{
    Align, Color, DocumentSource, Frame, HexData, ImageData, ListingData, MarkdownData, MdBlock, MdBlockKind,
    MediaData, PDF_FIRST_PAGE_WIDTH, PageRenderer, PdfData, PdfPages, PreviewData, RenderDone, RenderedPage, Rgb,
    StyleRun, StyledLine, StyledText, SyntaxRole, TextStyle, ThemeRole,
};
pub use registry::{Registry, RegistryError};
pub use source::{PreviewSource, SNIFF_BYTES, default_cache_dir, remove_default_cache_dir};

#[cfg(test)]
mod tests;
