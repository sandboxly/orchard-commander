//! The PDFium thread.
//!
//! PDFium is not thread-safe, so the library is loaded, and every document
//! opened, rendered and closed, on one dedicated thread (Python serialised
//! calls with a global lock instead). Other threads send it jobs and, for
//! blocking calls, wait for the answer with a time limit, so a stalled network
//! stream can't hang a caller forever. A panicking job is caught; the thread
//! carries on with the next one.

use std::cell::Cell;
use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use pdfium_render::prelude::{
    PdfDocument as RawDocument, PdfDocumentMetadataTagType, PdfPagePaperSize, PdfPoints, Pdfium,
};

use crate::error::PdfError;
use crate::{Page, fit_size, library};

/// How long a blocking call waits for the PDFium thread. Generous: a big page
/// of a document on a slow server can take a while, and the stream itself has
/// its own network timeouts.
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const START_TIMEOUT: Duration = Duration::from_secs(30);

type Job = Box<dyn FnOnce(&mut Engine) + Send>;

/// State owned by the PDFium thread.
pub(crate) struct Engine {
    pdfium: &'static Pdfium,
    docs: HashMap<u64, RawDocument<'static>>,
}

/// What opening a document finds out.
pub(crate) struct Opened {
    pub page_count: usize,
    pub title: String,
}

struct Handle {
    jobs: mpsc::Sender<Job>,
    library: PathBuf,
}

static ENGINE: OnceLock<Result<Handle, String>> = OnceLock::new();

thread_local! {
    static ON_ENGINE: Cell<bool> = const { Cell::new(false) };
}

fn handle() -> Result<&'static Handle, PdfError> {
    ENGINE.get_or_init(start).as_ref().map_err(|e| PdfError::Unavailable(e.clone()))
}

/// Start the thread and load the library (once per process).
pub(crate) fn ensure_started() -> Result<PathBuf, PdfError> {
    handle().map(|h| h.library.clone())
}

fn start() -> Result<Handle, String> {
    let (jobs, rx) = mpsc::channel::<Job>();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("pdfium".into())
        .spawn(move || {
            ON_ENGINE.set(true);
            let (pdfium, library) = match bind() {
                Ok(found) => found,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(library));
            let mut engine = Engine { pdfium, docs: HashMap::new() };
            while let Ok(job) = rx.recv() {
                if let Err(panic) = catch_unwind(AssertUnwindSafe(|| job(&mut engine))) {
                    log::error!("PDFium job panicked: {}", panic_text(&panic));
                }
            }
        })
        .map_err(|e| format!("can't start the PDF thread: {e}"))?;
    match ready_rx.recv_timeout(START_TIMEOUT) {
        Ok(Ok(library)) => {
            log::info!("PDFium loaded from {}", library.display());
            Ok(Handle { jobs, library })
        }
        Ok(Err(e)) => {
            log::warn!("PDF previews disabled: {e}");
            Err(e)
        }
        Err(_) => Err("PDF support didn't start in time".into()),
    }
}

/// Load the first library that works (see [`library::candidates`]).
fn bind() -> Result<(&'static Pdfium, PathBuf), String> {
    let mut tried = Vec::new();
    for path in library::candidates() {
        if path.is_absolute() && !path.is_file() {
            tried.push(format!("{}: missing", path.display()));
            continue;
        }
        match Pdfium::bind_to_library(&path) {
            Ok(bindings) => return Ok((Box::leak(Box::new(Pdfium::new(bindings))), path)),
            Err(e) => tried.push(format!("{}: {e:?}", path.display())),
        }
    }
    Err(format!(
        "no usable {} found; set {} or run scripts/fetch-pdfium.sh ({})",
        library::library_name(),
        library::ENV_VAR,
        tried.join("; ")
    ))
}

fn panic_text(panic: &Box<dyn std::any::Any + Send>) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// Queue a job without waiting for it.
pub(crate) fn post(job: impl FnOnce(&mut Engine) + Send + 'static) -> Result<(), PdfError> {
    handle()?.jobs.send(Box::new(job)).map_err(|_| PdfError::Crashed("the PDF thread has stopped".into()))
}

/// Run `f` on the PDFium thread and wait for its result.
pub(crate) fn call<R: Send + 'static>(f: impl FnOnce(&mut Engine) -> R + Send + 'static) -> Result<R, PdfError> {
    if ON_ENGINE.get() {
        return Err(PdfError::Reentrant);
    }
    let (tx, rx) = mpsc::sync_channel(1);
    post(move |engine| {
        let _ = tx.send(f(engine));
    })?;
    wait(&rx)
}

pub(crate) fn wait<R>(rx: &Receiver<R>) -> Result<R, PdfError> {
    match rx.recv_timeout(CALL_TIMEOUT) {
        Ok(r) => Ok(r),
        Err(RecvTimeoutError::Timeout) => Err(PdfError::Timeout(CALL_TIMEOUT.as_secs())),
        Err(RecvTimeoutError::Disconnected) => Err(PdfError::Crashed("the request was dropped (panic?)".into())),
    }
}

/// Open a document from `reader` as `id`. If the caller has given up waiting
/// (timeout), the document is closed again instead of leaking.
pub(crate) fn open(id: u64, reader: impl Read + Seek + Send + 'static) -> Result<Opened, PdfError> {
    if ON_ENGINE.get() {
        return Err(PdfError::Reentrant);
    }
    let (tx, rx) = mpsc::sync_channel(1);
    post(move |engine| {
        let result = engine.open(id, reader);
        if tx.send(result).is_err() {
            engine.close(id);
        }
    })?;
    wait(&rx)?
}

/// What PDFium reads documents through. pdfium-render answers each of
/// PDFium's block requests with a single `read` call and takes a short read
/// for a full block (the rest of the block is garbage), and a panicking
/// reader would unwind through PDFium's C frames (an abort). Network streams
/// do return short reads, so: fill every block completely or fail it, and
/// turn panics into errors.
struct BlockReader<R>(R);

impl<R: Read> Read for BlockReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut filled = 0;
        while filled < buf.len() {
            match catch_unwind(AssertUnwindSafe(|| self.0.read(&mut buf[filled..]))) {
                // PDFium only asks for bytes below the length it was given:
                // the file got shorter. Fail the block.
                Ok(Ok(0)) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the PDF ended early")),
                Ok(Ok(n)) => filled += n.min(buf.len() - filled),
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                Ok(Err(e)) => return Err(e),
                Err(panic) => return Err(io::Error::other(format!("reading the PDF failed: {}", panic_text(&panic)))),
            }
        }
        Ok(filled)
    }
}

impl<R: Seek> Seek for BlockReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        catch_unwind(AssertUnwindSafe(|| self.0.seek(pos)))
            .unwrap_or_else(|panic| Err(io::Error::other(format!("seeking in the PDF failed: {}", panic_text(&panic)))))
    }
}

impl Engine {
    fn open(&mut self, id: u64, reader: impl Read + Seek + 'static) -> Result<Opened, PdfError> {
        let doc = self.pdfium.load_pdf_from_reader(BlockReader(reader), None).map_err(PdfError::from_load)?;
        let page_count = usize::try_from(doc.pages().len()).unwrap_or(0);
        let title = doc
            .metadata()
            .get(PdfDocumentMetadataTagType::Title)
            .map(|t| t.value().trim().to_string())
            .unwrap_or_default();
        self.docs.insert(id, doc);
        Ok(Opened { page_count, title })
    }

    pub(crate) fn close(&mut self, id: u64) {
        self.docs.remove(&id); // closes the document and drops its reader
    }

    fn doc(&self, id: u64) -> Result<&RawDocument<'static>, PdfError> {
        self.docs.get(&id).ok_or(PdfError::Closed)
    }

    fn page_index(&self, id: u64, index: usize) -> Result<i32, PdfError> {
        let count = usize::try_from(self.doc(id)?.pages().len()).unwrap_or(0);
        if index >= count {
            return Err(PdfError::NoSuchPage { index, count });
        }
        i32::try_from(index).map_err(|_| PdfError::NoSuchPage { index, count })
    }

    /// Width and height of a page in points, without loading the page.
    pub(crate) fn page_size(&self, id: u64, index: usize) -> Result<(f32, f32), PdfError> {
        let i = self.page_index(id, index)?;
        let rect = self.doc(id)?.pages().page_size(i).map_err(|e| PdfError::from_page(index, e))?;
        Ok((rect.width().value, rect.height().value))
    }

    /// Render a page fitted into `max_width` × `max_height` pixels. Also
    /// returns the page size in points.
    pub(crate) fn render(
        &self,
        id: u64,
        index: usize,
        max_width: u32,
        max_height: u32,
    ) -> Result<(Page, (f32, f32)), PdfError> {
        let i = self.page_index(id, index)?;
        let page = self.doc(id)?.pages().get(i).map_err(|e| PdfError::from_page(index, e))?;
        let size = (page.width().value, page.height().value);
        let (width, height) = fit_size(size.0, size.1, max_width, max_height)
            .ok_or_else(|| PdfError::Render(format!("Page {} has no size.", index + 1)))?;
        // Both sides fit in i32: fit_size caps them at MAX_RENDER_PIXELS.
        let bitmap = page.render(width as i32, height as i32, None).map_err(|e| PdfError::from_page(index, e))?;
        let rgba = bitmap.as_rgba_bytes();
        if rgba.len() != width as usize * height as usize * 4 {
            return Err(PdfError::Render(format!("Page {} rendered to an unexpected bitmap.", index + 1)));
        }
        Ok((Page { index, width, height, rgba }, size))
    }

    /// A blank page drawn to check the library works (bundling, code signing).
    pub(crate) fn self_test(&self) -> Result<(), String> {
        let mut doc = self.pdfium.create_new_pdf().map_err(|e| format!("{e:?}"))?;
        let page = doc
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::new_custom(PdfPoints::new(100.0), PdfPoints::new(100.0)))
            .map_err(|e| format!("{e:?}"))?;
        let bitmap = page.render(20, 20, None).map_err(|e| format!("{e:?}"))?;
        let rgba = bitmap.as_rgba_bytes();
        match rgba.get(..4) {
            Some([255, 255, 255, 255]) => Ok(()),
            other => Err(format!("a blank page rendered as {other:?}, not white")),
        }
    }
}
