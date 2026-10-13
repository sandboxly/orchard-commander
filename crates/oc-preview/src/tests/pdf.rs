//! The PDF previewer with the real PDFium (port of the PDF tests in
//! test_previewers.py). Skipped when PDFium isn't installed.

use std::sync::{Arc, mpsc};
use std::time::Duration;

use oc_pdf::testing::{TestPage, TestPdf, pdf_bytes};

use super::fs::{LocalFs, MemFs};
use super::pdf_available;
use super::registry::assert_pdf_fallback;
use crate::builtin::{MAX_FETCH_BYTES, PdfPreviewer};
use crate::{LoadContext, Loaded, Outcome, PdfData, PreviewData, PreviewError, PreviewSource, Previewer, Registry};

fn preview_local(name: &str, bytes: &[u8]) -> (tempfile::TempDir, Outcome) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    let o = Registry::with_builtins().preview(&LocalFs::vpath(&path), &LoadContext::default(), None);
    (dir, o)
}

fn pdf_data(o: &Outcome) -> &PdfData {
    match &o.loaded {
        Some(Loaded { data: PreviewData::Pdf(pdf), .. }) => pdf,
        _ => panic!("not a PDF preview: {o:?}"),
    }
}

/// test_pdf_load_renders_first_page
#[test]
fn load_renders_first_page() {
    if !pdf_available() {
        return;
    }
    let (_dir, o) = preview_local("doc.pdf", &pdf_bytes(3));
    assert_eq!(o.previewer_id(), Some("pdf"));
    let info = &o.loaded.as_ref().unwrap().info;
    assert!(info.starts_with("3 pages · "), "{info}");
    let data = pdf_data(&o);
    assert_eq!((data.page_count, data.title.as_str()), (3, ""));
    assert_eq!(data.first_page.width, crate::PDF_FIRST_PAGE_WIDTH);
    assert_eq!(data.first_page.pixel(10, 10), Some([255, 255, 255, 255])); // a blank page renders white
    let page = data.pages.render(1, 300, u32::MAX).unwrap(); // the landscape page: wider than tall
    assert!(page.width == 300 && page.height < 300, "{page:?}");
    let fitted = data.pages.render(0, 400, 400).unwrap(); // fitted into a pane
    assert_eq!((fitted.width, fitted.height), (283, 400));
    assert_eq!(data.pages.page_size(1).unwrap(), (595.0, 420.0));
    data.pages.close();
    assert!(matches!(data.pages.render(0, 100, 100), Err(PreviewError::CannotPreview(m)) if m == "document closed"));
}

#[test]
fn status_line_has_pages_size_and_title() {
    if !pdf_available() {
        return;
    }
    let bytes = TestPdf::with_pages(1).title("Quarterly\nnumbers").build();
    let (_dir, o) = preview_local("q.pdf", &bytes);
    let size = oc_vfs::human_size(bytes.len() as u64);
    assert_eq!(o.loaded.as_ref().unwrap().info, format!("1 page · {size} · Quarterly numbers"));
    assert_eq!(pdf_data(&o).title, "Quarterly\nnumbers");
}

/// test_corrupt_pdf_falls_back_to_hex
#[test]
fn corrupt_pdf_falls_back_to_hex() {
    for bytes in [b"%PDF-1.7\nthis is not really a pdf".as_slice(), b"this is not a pdf"] {
        let (_dir, o) = preview_local("broken.pdf", bytes);
        assert_pdf_fallback(&o);
    }
    if pdf_available() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.pdf");
        std::fs::write(&path, "%PDF-1.7\nthis is not really a pdf").unwrap();
        let err = PdfPreviewer.load(&PreviewSource::new(LocalFs::vpath(&path)).unwrap(), &LoadContext::default());
        assert_eq!(
            err.unwrap_err().to_string(),
            "Not a readable PDF: Failed to load document (PDFium: Data format error)."
        );
    }
}

#[test]
fn pdf_without_pages_falls_back_to_hex() {
    let (_dir, o) = preview_local("empty.pdf", &pdf_bytes(0));
    assert_pdf_fallback(&o);
}

#[test]
fn password_protected_pdf_shows_a_message() {
    if !pdf_available() {
        return;
    }
    let (_dir, o) = preview_local("locked.pdf", &TestPdf::with_pages(2).encrypted("secret", "boss").build());
    assert!(o.loaded.is_none());
    let err = o.error.unwrap();
    assert!(err.is_message(), "{err:?}");
    assert_eq!(err.to_string(), "This PDF is password-protected.");
    // An owner password alone (printing/copying restrictions) doesn't lock it.
    let (_dir, o) = preview_local("restricted.pdf", &TestPdf::with_pages(2).encrypted("", "boss").build());
    assert_eq!(pdf_data(&o).page_count, 2);
}

#[test]
fn unseekable_streams_are_fetched_first() {
    if !pdf_available() {
        return;
    }
    let cache = tempfile::tempdir().unwrap();
    let mem = Arc::new(MemFs { unseekable: true, ..MemFs::default() });
    mem.add("inside.pdf", pdf_bytes(2));
    let ctx = LoadContext { cache_dir: Some(cache.path().to_path_buf()), ..LoadContext::default() };
    let o = Registry::with_builtins().preview(&mem.vpath("inside.pdf"), &ctx, None);
    assert_eq!(pdf_data(&o).page_count, 2);
    assert_eq!(std::fs::read_dir(cache.path()).unwrap().count(), 1); // a local copy
    // max_size: an unseekable PDF above it isn't fetched.
    mem.add_generated("huge.pdf", MAX_FETCH_BYTES + 1, b'x');
    let o = Registry::with_builtins().preview(&mem.vpath("huge.pdf"), &ctx, None);
    let err = o.error.expect("a message");
    assert!(err.is_message() && err.to_string().contains("copy it with F5 instead"), "{err}");
}

#[test]
fn pages_render_in_the_background_and_close_with_the_preview() {
    if !pdf_available() {
        return;
    }
    let pdf = TestPdf::with_pages(1).page(TestPage::left_half(200.0, 100.0, [0.0, 0.5, 0.0])).build();
    let mem = Arc::new(MemFs::new());
    mem.add("remote.pdf", pdf);
    let o = Registry::with_builtins().preview(&mem.vpath("remote.pdf"), &LoadContext::default(), None);
    let pages = pdf_data(&o).pages.clone();
    let (tx, rx) = mpsc::channel();
    pages.render_async(1, 200, 200, Box::new(move |r| tx.send(r).unwrap()));
    let page = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
    assert_eq!((page.index, page.width, page.height), (1, 200, 100));
    assert_eq!(page.pixel(5, 50), Some([0, 128, 0, 255]));
    assert!(pages.cached(1, 200, 200).is_some());
    pages.prefetch(0, 50, 50);
    drop(o); // the pane moved on; the handle keeps the document open
    assert_eq!(pages.render(0, 60, 60).unwrap().height, 60);
    pages.close();
    let (tx, rx) = mpsc::channel();
    pages.render_async(0, 70, 70, Box::new(move |r| tx.send(r.is_err()).unwrap()));
    assert!(rx.recv_timeout(Duration::from_secs(10)).unwrap());
}

/// A server stream that hands over a few bytes per `read` (as network
/// streams do) still previews correctly, without downloading the file.
#[test]
fn pdf_streamed_in_small_reads() {
    if !pdf_available() {
        return;
    }
    let pdf = TestPdf::with_pages(1).title("Remote").page(TestPage::left_half(200.0, 100.0, [0.0, 0.0, 1.0])).build();
    let cache = tempfile::tempdir().unwrap();
    let mem = Arc::new(MemFs { max_read: 5, ..MemFs::default() });
    mem.add("remote.pdf", pdf);
    let ctx = LoadContext { cache_dir: Some(cache.path().to_path_buf()), ..LoadContext::default() };
    let o = Registry::with_builtins().preview(&mem.vpath("remote.pdf"), &ctx, None);
    let data = pdf_data(&o);
    assert_eq!((data.page_count, data.title.as_str()), (2, "Remote"));
    assert_eq!(data.pages.render(1, 200, 200).unwrap().pixel(5, 50), Some([0, 0, 255, 255]));
    assert_eq!(std::fs::read_dir(cache.path()).unwrap().count(), 0); // streamed, not copied
}
