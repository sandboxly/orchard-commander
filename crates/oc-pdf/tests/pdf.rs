//! oc-pdf against the real PDFium library (skipped, with a note, when it
//! isn't installed: run scripts/fetch-pdfium.sh).

use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use oc_pdf::testing::{TestPage, TestPdf, pdf_bytes};
use oc_pdf::{Document, FIRST_PAGE_WIDTH, PdfError, Priority};

fn pdfium() -> bool {
    match oc_pdf::library_path() {
        Ok(_) => true,
        Err(e) => {
            eprintln!("skipped: {e}");
            false
        }
    }
}

fn open(bytes: Vec<u8>) -> Result<Document, PdfError> {
    Document::open_reader(Cursor::new(bytes))
}

const WHITE: [u8; 4] = [255, 255, 255, 255];

/// A reader that counts what it reads and reports when it's dropped (closed).
struct Probe {
    inner: Cursor<Vec<u8>>,
    read: Arc<AtomicU64>,
    dropped: Arc<AtomicBool>,
}

impl Probe {
    fn new(bytes: Vec<u8>) -> (Self, Arc<AtomicU64>, Arc<AtomicBool>) {
        let read = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        (Probe { inner: Cursor::new(bytes), read: read.clone(), dropped: dropped.clone() }, read, dropped)
    }
}

impl Read for Probe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.fetch_add(n as u64, Ordering::SeqCst);
        Ok(n)
    }
}

impl Seek for Probe {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

fn eventually(what: &str, cond: impl Fn() -> bool) {
    let t0 = Instant::now();
    while !cond() {
        assert!(t0.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn self_test_draws_a_blank_page() {
    if pdfium() {
        oc_pdf::self_test().unwrap();
        assert!(oc_pdf::available());
    }
}

#[test]
fn page_count_and_title() {
    if !pdfium() {
        return;
    }
    let doc = open(TestPdf::with_pages(3).title("  Annual report  ").build()).unwrap();
    assert_eq!((doc.page_count(), doc.title()), (3, "Annual report")); // trimmed, like Python's strip()
    let doc = open(TestPdf::with_pages(1).title("Jahresbericht – Ü").build()).unwrap();
    assert_eq!(doc.title(), "Jahresbericht – Ü");
    assert_eq!(open(pdf_bytes(2)).unwrap().title(), "");
}

/// Port of test_pdf_load_renders_first_page.
#[test]
fn renders_pages_at_the_requested_size() {
    if !pdfium() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("doc.pdf");
    std::fs::write(&path, pdf_bytes(3)).unwrap();
    let doc = Document::open_path(&path).unwrap();
    let first = doc.render_width(0, FIRST_PAGE_WIDTH).unwrap();
    assert_eq!((first.width, first.height), (1200, 1698));
    assert_eq!(first.rgba.len(), 1200 * 1698 * 4);
    assert_eq!(first.pixel(10, 10), Some(WHITE)); // a blank page renders white
    let page = doc.render_width(1, 300).unwrap(); // the landscape page: wider than tall
    assert!(page.width == 300 && page.height < 300, "{page:?}");
    let fitted = doc.render(0, 1000, 500).unwrap(); // fitted into the pane
    assert_eq!((fitted.width, fitted.height), (353, 500));
    assert_eq!(doc.page_size(1).unwrap(), (595.0, 420.0));
    doc.close();
    assert!(matches!(doc.render(0, 100, 100), Err(PdfError::Closed)));
    assert!(matches!(doc.page_size(2), Err(PdfError::Closed)));
}

#[test]
fn colors_come_out_as_rgba() {
    if !pdfium() {
        return;
    }
    let pdf = TestPdf::default().page(TestPage::left_half(200.0, 100.0, [1.0, 0.0, 0.0])).build();
    let page = open(pdf).unwrap().render(0, 200, 100).unwrap();
    assert_eq!((page.width, page.height), (200, 100));
    assert_eq!(page.pixel(10, 50), Some([255, 0, 0, 255]));
    assert_eq!(page.pixel(190, 50), Some(WHITE));
    assert_eq!(page.pixel(200, 0), None);
}

#[test]
fn renders_are_cached() {
    if !pdfium() {
        return;
    }
    let doc = open(pdf_bytes(2)).unwrap();
    assert!(doc.cached(0, 100, 100).is_none());
    let a = doc.render(0, 100, 100).unwrap();
    let b = doc.cached(0, 100, 100).unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    for w in 0..oc_pdf::CACHE_PAGES as u32 {
        doc.render(1, 50 + w, 50).unwrap();
    }
    assert!(doc.cached(0, 100, 100).is_none()); // evicted, least recently used first
}

#[test]
fn missing_page() {
    if !pdfium() {
        return;
    }
    let doc = open(pdf_bytes(2)).unwrap();
    assert!(matches!(doc.render(2, 10, 10), Err(PdfError::NoSuchPage { index: 2, count: 2 })));
    assert!(matches!(doc.page_size(7), Err(PdfError::NoSuchPage { .. })));
}

/// Python: "Not a readable PDF: Failed to load document (PDFium: Data format error)."
#[test]
fn corrupt_files_say_why() {
    if !pdfium() {
        return;
    }
    for bytes in [b"%PDF-1.7\nthis is not really a pdf".to_vec(), b"plain text".to_vec(), Vec::new()] {
        match open(bytes) {
            Err(PdfError::Unreadable(msg)) => assert_eq!(msg, "Failed to load document (PDFium: Data format error)."),
            other => panic!("{other:?}"),
        }
    }
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(Document::open_path(&dir.path().join("gone.pdf")), Err(PdfError::Io(_))));
}

#[test]
fn password_protected() {
    if !pdfium() {
        return;
    }
    let err = open(TestPdf::with_pages(1).encrypted("secret", "owner").build()).unwrap_err();
    assert!(matches!(err, PdfError::Password));
    assert_eq!(err.to_string(), "This PDF is password-protected.");
    // Only an owner password (restrictions, no prompt): opens and decrypts.
    let pdf = TestPdf::with_pages(1)
        .title("Locked down")
        .page(TestPage::left_half(100.0, 100.0, [0.0, 0.0, 1.0]))
        .encrypted("", "owner")
        .build();
    let doc = open(pdf).unwrap();
    assert_eq!((doc.page_count(), doc.title()), (2, "Locked down"));
    assert_eq!(doc.render(1, 100, 100).unwrap().pixel(5, 50), Some([0, 0, 255, 255]));
}

#[test]
fn streams_are_read_only_where_needed_and_closed_with_the_document() {
    if !pdfium() {
        return;
    }
    let bytes = TestPdf::with_pages(2).junk(4 * 1024 * 1024).build();
    let total = bytes.len() as u64;
    let (probe, read, dropped) = Probe::new(bytes);
    let doc = Document::open_reader(probe).unwrap();
    doc.render_width(1, 200).unwrap();
    assert!(read.load(Ordering::SeqCst) < total / 4, "read {} of {total}", read.load(Ordering::SeqCst));
    let copy = doc.clone();
    drop(doc);
    assert!(!dropped.load(Ordering::SeqCst)); // a clone still uses it
    drop(copy);
    eventually("the stream to be closed", || dropped.load(Ordering::SeqCst));

    let (probe, _, dropped) = Probe::new(b"%PDF-1.7\nbroken".to_vec());
    assert!(Document::open_reader(probe).is_err());
    assert!(dropped.load(Ordering::SeqCst)); // closed on failure too
}

#[test]
fn render_async_delivers_and_newer_requests_win() {
    if !pdfium() {
        return;
    }
    let doc = open(pdf_bytes(4)).unwrap();
    let (tx, rx) = mpsc::channel();
    // Hold the PDFium thread inside the first callback while more requests queue up.
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let first = tx.clone();
    doc.render_async(0, 80, 80, Priority::Show, move |r| {
        started_tx.send(()).unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(5));
        first.send((0, r.map(|p| p.index))).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let send = |i: usize| {
        let tx = tx.clone();
        move |r: Result<Arc<oc_pdf::Page>, PdfError>| tx.send((i, r.map(|p| p.index))).unwrap()
    };
    doc.render_async(1, 80, 80, Priority::Show, send(1));
    doc.render_async(3, 80, 80, Priority::Prefetch, send(3));
    doc.render_async(2, 80, 80, Priority::Show, send(2));
    release_tx.send(()).unwrap();
    let mut results: Vec<_> = (0..4).map(|_| rx.recv_timeout(Duration::from_secs(10)).unwrap()).collect();
    results.sort_by_key(|(i, _)| *i);
    assert!(matches!(results[0], (0, Ok(0))));
    assert!(matches!(results[1], (1, Err(PdfError::Cancelled))));
    assert!(matches!(results[2], (2, Ok(2))));
    assert!(matches!(results[3], (3, Err(PdfError::Cancelled)))); // a stale prefetch
    // Cached pages come back at once, on this thread.
    let here = std::thread::current().id();
    let (tx2, rx2) = mpsc::channel();
    doc.render_async(2, 80, 80, Priority::Show, move |r| tx2.send((std::thread::current().id(), r.is_ok())).unwrap());
    assert_eq!(rx2.try_recv().unwrap(), (here, true));
}

#[test]
fn prefetch_fills_the_cache() {
    if !pdfium() {
        return;
    }
    let doc = open(pdf_bytes(3)).unwrap();
    doc.prefetch(1, 64, 64);
    doc.prefetch(9, 64, 64); // past the end: ignored
    eventually("the prefetched page", || doc.cached(1, 64, 64).is_some());
}

#[test]
fn closing_fails_pending_requests() {
    if !pdfium() {
        return;
    }
    let doc = open(pdf_bytes(2)).unwrap();
    let (tx, rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    doc.render_async(0, 40, 40, Priority::Show, move |_| {
        let _ = release_rx.recv_timeout(Duration::from_secs(5));
    });
    doc.render_async(1, 40, 40, Priority::Prefetch, move |r| tx.send(r.map(|_| ())).unwrap());
    doc.close();
    release_tx.send(()).unwrap();
    assert!(matches!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), Err(PdfError::Closed)));
    let (tx, rx) = mpsc::channel();
    doc.render_async(0, 40, 40, Priority::Show, move |r| tx.send(r.map(|_| ())).unwrap());
    assert!(matches!(rx.try_recv().unwrap(), Err(PdfError::Closed))); // at once
}

#[test]
fn callbacks_cant_block_on_the_pdfium_thread_and_panics_are_contained() {
    if !pdfium() {
        return;
    }
    let doc = open(pdf_bytes(2)).unwrap();
    let (tx, rx) = mpsc::channel();
    let inner = doc.clone();
    doc.render_async(0, 30, 30, Priority::Show, move |_| tx.send(inner.render(1, 30, 30).map(|_| ())).unwrap());
    assert!(matches!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), Err(PdfError::Reentrant)));
    doc.render_async(1, 31, 31, Priority::Show, |_| panic!("a broken callback"));
    assert_eq!(doc.render(1, 32, 32).unwrap().width, 32); // the thread lives on
}

#[test]
fn many_threads_at_once() {
    if !pdfium() {
        return;
    }
    let doc = open(TestPdf::with_pages(6).build()).unwrap();
    let handles: Vec<_> = (0..12)
        .map(|i| {
            let doc = doc.clone();
            let own = pdf_bytes(1);
            std::thread::spawn(move || {
                let page = doc.render(i % 6, 100 + i as u32, 100).unwrap();
                let other = open(own).unwrap().render(0, 50, 50).unwrap();
                (page.index, other.pixel(1, 1))
            })
        })
        .collect();
    for (i, h) in handles.into_iter().enumerate() {
        assert_eq!(h.join().unwrap(), (i % 6, Some(WHITE)));
    }
}

/// A network stream: `read` returns at most a few bytes per call (a packet
/// at a time), as SFTP/HTTP streams do. PDFium asks for whole blocks, so the
/// blocks must be filled completely.
struct Trickle<R> {
    inner: R,
    chunk: usize,
}

impl<R: Read> Read for Trickle<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf.len().min(self.chunk);
        self.inner.read(&mut buf[..n])
    }
}

impl<R: Seek> Seek for Trickle<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

#[test]
fn short_reads_are_completed() {
    if !pdfium() {
        return;
    }
    let pdf = TestPdf::with_pages(2).title("Trickled").page(TestPage::left_half(200.0, 100.0, [1.0, 0.0, 0.0])).build();
    let whole = open(pdf.clone()).unwrap().render(2, 200, 100).unwrap();
    for chunk in [1, 7, 1000] {
        let doc = Document::open_reader(Trickle { inner: Cursor::new(pdf.clone()), chunk }).unwrap();
        assert_eq!((doc.page_count(), doc.title()), (3, "Trickled"), "chunk {chunk}");
        assert_eq!(*doc.render(2, 200, 100).unwrap(), *whole, "chunk {chunk}");
    }
}

/// A stream that breaks once `broken` is set: with an error (a dropped
/// connection) or a panic (a buggy stream). Neither may hang, abort the
/// process or render garbage.
struct Breaks {
    inner: Cursor<Vec<u8>>,
    broken: Arc<AtomicBool>,
    panic: bool,
}

impl Read for Breaks {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.broken.load(Ordering::SeqCst) {
            if self.panic {
                panic!("a buggy stream");
            }
            return Err(io::Error::new(io::ErrorKind::ConnectionReset, "connection reset"));
        }
        self.inner.read(buf)
    }
}

impl Seek for Breaks {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

#[test]
fn broken_streams_fail_cleanly() {
    if !pdfium() {
        return;
    }
    let mut pdf = TestPdf::default();
    for _ in 0..50 {
        pdf = pdf.page(TestPage::left_half(100.0, 100.0, [1.0, 0.0, 0.0])).junk(64 * 1024);
    }
    let pdf = pdf.build();
    for panic in [false, true] {
        let broken = Arc::new(AtomicBool::new(true));
        let stream = Breaks { inner: Cursor::new(pdf.clone()), broken: broken.clone(), panic };
        assert!(matches!(Document::open_reader(stream), Err(PdfError::Unreadable(_))), "panic: {panic}");

        broken.store(false, Ordering::SeqCst);
        let stream = Breaks { inner: Cursor::new(pdf.clone()), broken: broken.clone(), panic };
        let doc = Document::open_reader(stream).unwrap();
        assert_eq!(doc.page_count(), 50);
        broken.store(true, Ordering::SeqCst); // the connection drops
        match doc.render(25, 100, 100) {
            Err(PdfError::Render(msg)) => assert!(msg.contains("page 26"), "{msg}"),
            Ok(page) => assert_eq!(page.pixel(10, 50), Some([255, 0, 0, 255])), // read before it broke
            Err(e) => panic!("{e:?}"),
        }
    }
    oc_pdf::self_test().unwrap(); // the PDFium thread lives on
}

/// A one-page PDF whose page is turned by 90°.
fn rotated_pdf() -> Vec<u8> {
    let mut pdf = pdf_bytes(1);
    // Same length as what it replaces, so the xref offsets stay right.
    let (from, to) = (b"/Resources << >>", b"/Rotate 90      ");
    let at = pdf.windows(from.len()).position(|w| w == from).unwrap();
    pdf[at..at + to.len()].copy_from_slice(to);
    pdf
}

/// Pages with /Rotate are measured and drawn as displayed (turned), like
/// pypdfium2's get_page_size()/get_size()/render().
#[test]
fn rotated_pages_are_measured_as_shown() {
    if !pdfium() {
        return;
    }
    let doc = open(rotated_pdf()).unwrap();
    assert_eq!(doc.page_size(0).unwrap(), (842.0, 595.0));
    let page = doc.render_width(0, 842).unwrap();
    assert_eq!((page.width, page.height), (842, 595));
    let fresh = open(rotated_pdf()).unwrap();
    let page = fresh.render(0, 421, 421).unwrap(); // measured while rendering, not by page_size
    assert_eq!((page.width, page.height), (421, 298));
    assert_eq!(fresh.page_size(0).unwrap(), (842.0, 595.0));
}
