//! An open PDF: page count, title, and pages rendered on the PDFium thread.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs::File;
use std::io::{Read, Seek};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::engine;
use crate::error::PdfError;

/// Rendered pages kept per document (current page, neighbours, a resize back).
pub const CACHE_PAGES: usize = 8;

/// A rendered page: straight RGBA, 4 bytes per pixel, rows top to bottom, no
/// padding, opaque (pages are drawn on white).
#[derive(Clone, PartialEq, Eq)]
pub struct Page {
    /// 0-based page number.
    pub index: usize,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Page {
    /// The RGBA bytes of pixel (x, y), if inside the page.
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = (y as usize * self.width as usize + x as usize) * 4;
        self.rgba.get(i..i + 4).and_then(|p| p.try_into().ok())
    }
}

impl fmt::Debug for Page {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Page({}, {}×{})", self.index + 1, self.width, self.height)
    }
}

/// Which render requests go first when pages are turned quickly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    /// The page the user is looking at: requests queued before it (of this
    /// document) are skipped with [`PdfError::Cancelled`].
    Show,
    /// A page the user will probably look at next; skipped once a newer
    /// `Show` request arrives.
    Prefetch,
}

/// Called with the result of [`Document::render_async`].
pub type RenderCallback = Box<dyn FnOnce(Result<Arc<Page>, PdfError>) + Send>;

type CacheKey = (usize, u32, u32);

struct Inner {
    id: u64,
    page_count: usize,
    title: String,
    closed: AtomicBool,
    /// Bumped by every `Show` request; queued requests of older generations are skipped.
    generation: AtomicU64,
    cache: Mutex<VecDeque<(CacheKey, Arc<Page>)>>,
    sizes: Mutex<HashMap<usize, (f32, f32)>>,
}

impl Inner {
    fn close(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            let id = self.id;
            let _ = engine::post(move |e| e.close(id));
            self.cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
    }

    fn check_open(&self) -> Result<(), PdfError> {
        if self.closed.load(Ordering::SeqCst) { Err(PdfError::Closed) } else { Ok(()) }
    }

    fn cached(&self, key: CacheKey) -> Option<Arc<Page>> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let i = cache.iter().position(|(k, _)| *k == key)?;
        let hit = cache.remove(i)?;
        let page = hit.1.clone();
        cache.push_back(hit);
        Some(page)
    }

    fn remember(&self, key: CacheKey, page: Page, size: (f32, f32)) -> Arc<Page> {
        self.sizes.lock().unwrap_or_else(|e| e.into_inner()).insert(page.index, size);
        let page = Arc::new(page);
        if self.closed.load(Ordering::SeqCst) {
            return page;
        }
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.retain(|(k, _)| *k != key);
        cache.push_back((key, page.clone()));
        while cache.len() > CACHE_PAGES {
            cache.pop_front();
        }
        page
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.close();
    }
}

/// An open PDF. Cheap to clone and thread-safe; the document is closed when
/// [`close`](Self::close) is called or the last clone is dropped.
///
/// Blocking calls ([`render`](Self::render), [`page_size`](Self::page_size))
/// belong on worker threads; the UI thread uses
/// [`render_async`](Self::render_async) and [`cached`](Self::cached).
#[derive(Clone)]
pub struct Document {
    inner: Arc<Inner>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl Document {
    /// Open a PDF file on the local disk.
    pub fn open_path(path: &Path) -> Result<Document, PdfError> {
        Self::open_reader(File::open(path)?)
    }

    /// Open a PDF from a seekable reader (a stream from a server, say). PDFium
    /// reads only the parts it needs, when it needs them; the reader is
    /// dropped when the document is closed.
    pub fn open_reader(reader: impl Read + Seek + Send + 'static) -> Result<Document, PdfError> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let opened = engine::open(id, reader)?;
        Ok(Document {
            inner: Arc::new(Inner {
                id,
                page_count: opened.page_count,
                title: opened.title,
                closed: AtomicBool::new(false),
                generation: AtomicU64::new(0),
                cache: Mutex::new(VecDeque::new()),
                sizes: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn page_count(&self) -> usize {
        self.inner.page_count
    }

    /// The document's title from its metadata, trimmed; "" if none.
    pub fn title(&self) -> &str {
        &self.inner.title
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// Close the document now (for every clone). Pending requests fail with
    /// [`PdfError::Closed`].
    pub fn close(&self) {
        self.inner.close();
    }

    /// Width and height of page `index` (0-based) in points. Blocks unless the
    /// page was rendered or measured before.
    pub fn page_size(&self, index: usize) -> Result<(f32, f32), PdfError> {
        self.inner.check_open()?;
        if let Some(size) = self.inner.sizes.lock().unwrap_or_else(|e| e.into_inner()).get(&index) {
            return Ok(*size);
        }
        let id = self.inner.id;
        let size = engine::call(move |e| e.page_size(id, index))??;
        self.inner.sizes.lock().unwrap_or_else(|e| e.into_inner()).insert(index, size);
        Ok(size)
    }

    /// Page `index` (0-based), rendered as large as fits into `max_width` ×
    /// `max_height` pixels keeping its aspect ratio (each side at most
    /// [`MAX_RENDER_PIXELS`](crate::MAX_RENDER_PIXELS)). Blocks; cached.
    pub fn render(&self, index: usize, max_width: u32, max_height: u32) -> Result<Arc<Page>, PdfError> {
        self.inner.check_open()?;
        let key = (index, max_width, max_height);
        if let Some(page) = self.inner.cached(key) {
            return Ok(page);
        }
        let id = self.inner.id;
        let (page, size) = engine::call(move |e| e.render(id, index, max_width, max_height))??;
        Ok(self.inner.remember(key, page, size))
    }

    /// Page `index` rendered `width` pixels wide (Python's `render(index, width_px)`).
    pub fn render_width(&self, index: usize, width: u32) -> Result<Arc<Page>, PdfError> {
        self.render(index, width, u32::MAX)
    }

    /// The page as rendered before for this box, if still cached. Never blocks.
    pub fn cached(&self, index: usize, max_width: u32, max_height: u32) -> Option<Arc<Page>> {
        self.inner.cached((index, max_width, max_height))
    }

    /// Like [`render`](Self::render) without blocking: `done` gets the page
    /// (or why not) on the PDFium thread, or right away on this thread when
    /// the page is cached or the document closed. `done` should only hand the
    /// result on (to the UI thread); blocking calls on a document from inside
    /// it fail with [`PdfError::Reentrant`].
    pub fn render_async(
        &self,
        index: usize,
        max_width: u32,
        max_height: u32,
        priority: Priority,
        done: impl FnOnce(Result<Arc<Page>, PdfError>) + Send + 'static,
    ) {
        let generation = match priority {
            Priority::Show => self.inner.generation.fetch_add(1, Ordering::SeqCst) + 1,
            Priority::Prefetch => self.inner.generation.load(Ordering::SeqCst),
        };
        if let Err(e) = self.inner.check_open() {
            return done(Err(e));
        }
        let key = (index, max_width, max_height);
        if let Some(page) = self.inner.cached(key) {
            return done(Ok(page));
        }
        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        let id = self.inner.id;
        let done: RenderCallback = Box::new(done);
        let job = move |e: &mut engine::Engine| {
            let Some(inner) = weak.upgrade() else { return done(Err(PdfError::Closed)) };
            let result = if inner.closed.load(Ordering::SeqCst) {
                Err(PdfError::Closed)
            } else if inner.generation.load(Ordering::SeqCst) != generation {
                Err(PdfError::Cancelled)
            } else if let Some(page) = inner.cached(key) {
                Ok(page) // a prefetch got there first
            } else {
                e.render(id, index, max_width, max_height).map(|(page, size)| inner.remember(key, page, size))
            };
            done(result);
        };
        // A failed post drops the job, and with it `done`; nothing to report
        // to: the engine is gone, so the document was never opened.
        let _ = engine::post(job);
    }

    /// Render a page into the cache in the background (the next page, say).
    pub fn prefetch(&self, index: usize, max_width: u32, max_height: u32) {
        if index < self.page_count() && self.cached(index, max_width, max_height).is_none() {
            self.render_async(index, max_width, max_height, Priority::Prefetch, |_| {});
        }
    }
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Document")
            .field("page_count", &self.inner.page_count)
            .field("title", &self.inner.title)
            .field("closed", &self.is_closed())
            .finish()
    }
}
