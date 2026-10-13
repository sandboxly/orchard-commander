//! PDF documents, one page at a time fitted to the pane (port of
//! `preview/builtin/pdf.py`'s `PdfView`).
//!
//! Page 1 arrives with the data (rendered 1200 px wide) and shows at once;
//! then the page is rendered at the pane's size in device pixels through
//! oc-pdf's queue (`render_async`: pages flipped past are dropped), and the
//! next page is prefetched. Keys: → ↓ PgDn Space next, ← ↑ PgUp Backspace
//! previous, Home/End; the wheel turns one page per step; ◀ ▶ below.

use std::any::Any;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_grid::{Flags, Frame, ImagePlacement, Style};
use oc_preview::{PdfData, PdfPages, PreviewError, RenderedPage};

use super::image::{area_px, fit};
use super::views::{ContentView, Handle};
use crate::app::{App, Outcome};
use crate::geom::Rect;
use crate::images::{self, ImageKey};
use crate::input::{Event, Key, MouseButton, MouseKind};
use crate::widgets::{DrawCtx, fill, put, width};

/// After a page turn by the wheel, the rest of that flick is ignored this long.
const WHEEL_PAUSE: Duration = Duration::from_millis(350);
/// Re-render this long after the last resize.
const RESIZE_DELAY: Duration = Duration::from_millis(150);

pub struct PdfView {
    pub pages: PdfPages,
    pub count: usize,
    /// The page the user is on (0-based).
    pub index: usize,
    /// What is on screen: the page image (maybe of another page or size
    /// until the exact render lands).
    pub shown: Option<Arc<RenderedPage>>,
    key: Option<ImageKey>,
    /// The render on its way: (page, width, height).
    wanted: Option<(u64, (usize, u32, u32))>,
    requests: u64,
    wheel_paused_until: Option<Instant>,
    resize_timer: Option<u64>,
}

impl PdfView {
    pub fn new(data: PdfData) -> Self {
        PdfView {
            count: data.page_count,
            pages: data.pages,
            index: 0,
            shown: Some(data.first_page),
            key: None,
            wanted: None,
            requests: 0,
            wheel_paused_until: None,
            resize_timer: None,
        }
    }

    /// Where the page goes (the row below holds the controls).
    fn page_area(area: Rect) -> Rect {
        area.split_bottom(1.min(area.h.saturating_sub(1))).0
    }

    /// The pixel box a page is rendered into for `area`.
    fn target(area: Rect, cell_px: (f32, f32)) -> (u32, u32) {
        let px = area_px(Self::page_area(area), cell_px);
        ((px.w as u32).max(16), (px.h as u32).max(16))
    }

    /// The controls row: (◀ button, counter, ▶ button).
    fn controls(&self, area: Rect) -> (Rect, Rect, Rect) {
        let row = area.row(area.h.saturating_sub(1));
        let counter = self.counter_text();
        let cw = width(&counter) + 2;
        let total = 3 + cw + 3;
        let x = row.x + row.w.saturating_sub(total) / 2;
        (Rect::new(x, row.y, 3, 1), Rect::new(x + 3, row.y, cw, 1), Rect::new(x + 3 + cw, row.y, 3, 1))
    }

    pub fn counter_text(&self) -> String {
        if self.count > 0 { format!("{} / {}", self.index + 1, self.count) } else { String::new() }
    }

    fn show(&mut self, app: &mut App, page: Arc<RenderedPage>) {
        let store = images::store(app);
        match self.key {
            Some(k) => store.set(k, page.width, page.height, page.clone()),
            None => self.key = Some(store.insert(page.width, page.height, page.clone())),
        }
        self.shown = Some(page);
        app.redraw();
    }

    /// Go to page `index` (clamped).
    pub fn go(&mut self, app: &mut App, h: Handle, area: Rect, index: usize) {
        let index = index.min(self.count.saturating_sub(1));
        if index == self.index {
            return;
        }
        self.index = index;
        self.request(app, h, area);
        app.redraw();
    }

    /// Show the current page at the pane's size: from the cache, or rendered.
    fn request(&mut self, app: &mut App, h: Handle, area: Rect) {
        let (w, hgt) = Self::target(area, app.cell_px);
        let index = self.index;
        if let Some(page) = self.pages.cached(index, w, hgt) {
            self.wanted = None;
            self.show(app, page);
            self.pages.prefetch(index + 1, w, hgt);
            return;
        }
        if self.wanted.is_some_and(|(_, k)| k == (index, w, hgt)) {
            return;
        }
        self.requests += 1;
        let serial = self.requests;
        self.wanted = Some((serial, (index, w, hgt)));
        let pages = self.pages.clone();
        // A worker waits for the render so the job counts as pending work
        // (tests and screenshots wait for it); the render queue itself drops
        // requests for pages flipped past.
        app.tasks.spawn(
            move || {
                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                pages.render_async(index, w, hgt, Box::new(move |r| drop(tx.send(r))));
                rx.recv().unwrap_or_else(|_| Err(PreviewError::cannot("render dropped")))
            },
            move |app, result| {
                h.with::<PdfView, _>(app, |v, app, _| v.rendered(app, serial, (index, w, hgt), result));
            },
        );
    }

    fn rendered(&mut self, app: &mut App, serial: u64, key: (usize, u32, u32), result: Result<Arc<RenderedPage>, PreviewError>) {
        if self.wanted.map(|(s, _)| s) != Some(serial) {
            return;
        }
        self.wanted = None;
        match result {
            Ok(page) => {
                self.show(app, page);
                self.pages.prefetch(key.0 + 1, key.1, key.2);
            }
            Err(e) => log::debug!("PDF page {} not rendered: {e}", key.0 + 1),
        }
    }

    fn wheel(&mut self, app: &mut App, h: Handle, area: Rect, lines: f32) {
        let now = Instant::now();
        if app.wheel_momentum || self.wheel_paused_until.is_some_and(|t| now < t) || lines == 0.0 {
            return;
        }
        self.wheel_paused_until = Some(now + WHEEL_PAUSE);
        if lines < 0.0 {
            self.go(app, h, area, self.index + 1);
        } else {
            self.go(app, h, area, self.index.saturating_sub(1));
        }
    }
}

impl ContentView for PdfView {
    fn kind(&self) -> &'static str {
        "pdf"
    }

    fn draw(&self, _app: &App, f: &mut Frame, area: Rect, ctx: &DrawCtx, _focused: bool) {
        let c = &ctx.theme.colors;
        let page_area = Self::page_area(area);
        if let (Some(page), Some(key)) = (&self.shown, self.key) {
            let dest = fit(page.width, page.height, area_px(page_area, ctx.cell_px), true);
            f.add_image(ImagePlacement::new(key.id(), dest));
        }
        if area.h < 2 {
            return;
        }
        let (prev, counter, next) = self.controls(area);
        let button = |enabled: bool| {
            if enabled { Style::new(c.foreground, c.border).with(Flags::BOLD) } else { Style::new(c.muted, c.border) }
        };
        for (r, label, enabled) in [(prev, "◀", self.index > 0), (next, "▶", self.index + 1 < self.count)] {
            let s = button(enabled);
            fill(f, r, s.bg);
            put(f, r.x + 1, r.y, label, s, 1);
        }
        put(f, counter.x + 1, counter.y, &self.counter_text(), Style::new(c.muted, c.surface), counter.w);
    }

    fn handle_event(&mut self, app: &mut App, h: Handle, ev: &Event, area: Rect) -> Outcome {
        match ev {
            Event::Key(k) if k.mods.is_empty() => {
                match k.key {
                    Key::Right | Key::PageDown | Key::Space | Key::Down => self.go(app, h, area, self.index + 1),
                    Key::Left | Key::PageUp | Key::Backspace | Key::Up => self.go(app, h, area, self.index.saturating_sub(1)),
                    Key::Home => self.go(app, h, area, 0),
                    Key::End => self.go(app, h, area, self.count.saturating_sub(1)),
                    _ => return Outcome::Ignored,
                }
                Outcome::Consumed
            }
            Event::Mouse(m) => {
                match m.kind {
                    MouseKind::Scroll(lines) => self.wheel(app, h, area, lines),
                    MouseKind::Down(MouseButton::Left) => {
                        let (prev, _, next) = self.controls(area);
                        if prev.contains(m.col, m.row) {
                            self.go(app, h, area, self.index.saturating_sub(1));
                        } else if next.contains(m.col, m.row) {
                            self.go(app, h, area, self.index + 1);
                        }
                    }
                    _ => return Outcome::Ignored,
                }
                Outcome::Consumed
            }
            _ => Outcome::Ignored,
        }
    }

    fn shown(&mut self, app: &mut App, h: Handle, area: Rect) {
        if let Some(page) = self.shown.clone() {
            self.show(app, page);
        }
        self.request(app, h, area);
    }

    fn resized(&mut self, app: &mut App, h: Handle, _area: Rect) {
        if let Some(t) = self.resize_timer.take() {
            app.cancel_timer(t);
        }
        self.resize_timer = Some(app.schedule(RESIZE_DELAY, move |app| {
            h.with::<PdfView, _>(app, |v, app, area| {
                v.resize_timer = None;
                v.request(app, h, area);
            });
        }));
    }

    fn clear(&mut self, app: &mut App) {
        if let Some(t) = self.resize_timer.take() {
            app.cancel_timer(t);
        }
        if let Some(k) = self.key.take() {
            images::store(app).remove(k);
        }
        self.wanted = None;
        self.pages.close(); // queued renders fail; PDFium closes it on its own thread
    }

    fn text(&self) -> Option<String> {
        Some(self.counter_text())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
