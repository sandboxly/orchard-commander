//! The Preview side view driven without a window (ports of 1.x's
//! `test_ui.py` preview tests): following the cursor, debouncing, "View as"
//! chips and their memory, images, PDFs, cloud placeholders.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oc_ui::features::preview::image::ImageView;
use oc_ui::features::preview::pdf::PdfView;
use oc_ui::features::preview::{PreviewOptions, PreviewView};
use oc_ui::images::Images;
use oc_ui::main_window::{self, SideMode, view_as};
use oc_ui::panel::{self, LoadOptions};
use oc_ui::testing::{headless_app, press, press_with, settle, wait_until};
use oc_ui::{App, Event, Key, MAIN_WINDOW, Mods, MouseButton, MouseEvent, MouseKind};
use oc_vfs::{Entry, FileSystem, ListOptions, ReadStream, RelPath, VPath, VfsError, sort_entries};

struct Env {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    src: PathBuf,
    dst: PathBuf,
}

/// src/{a.txt, b.bin, sub/c.txt}, an empty dst.
fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = oc_local::absolute(&fs::canonicalize(tmp.path()).unwrap());
    let (config, src, dst) = (base.join("config"), base.join("src"), base.join("dst"));
    for d in [&config, &src, &dst, &src.join("sub")] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(src.join("a.txt"), "alpha").unwrap();
    fs::write(src.join("b.bin"), [0u8, 0, 1, 2, 255]).unwrap();
    fs::write(src.join("sub/c.txt"), "gamma").unwrap();
    Env { _tmp: tmp, config, src, dst }
}

fn app(e: &Env) -> App {
    let mut app = headless_app(&e.config, &e.src, &e.dst);
    app.resize(MAIN_WINDOW, 120, 40);
    settle(&mut app);
    app
}

fn pane(app: &App) -> &PreviewView {
    view_as::<PreviewView>(app, 1, "preview").expect("the preview view")
}

fn shown_text(app: &App) -> String {
    pane(app).content().and_then(|c| c.text()).unwrap_or_default()
}

fn kind(app: &App) -> &'static str {
    pane(app).content().map_or("", |c| c.kind())
}

/// The cursor of the left panel on `name` (as a user would move it).
fn select(app: &mut App, name: &str) {
    let p = &mut app.main.sides[0].panel;
    let row = p.row_of(name).unwrap_or_else(|| panic!("{name} not listed"));
    p.set_cursor(row);
    main_window::cursor_moved(app, 0);
}

fn wait_for(app: &mut App, what: &str, done: impl FnMut(&App) -> bool) {
    wait_until(app, what, Duration::from_secs(20), done);
}

fn open_preview(app: &mut App) {
    press(app, Key::F(3));
    assert_eq!(app.main.sides[1].mode_id(), "preview");
}


fn click(app: &mut App, col: u16, row: u16, button: MouseButton) {
    for kind in [MouseKind::Down(button), MouseKind::Up(button)] {
        app.handle_event(MAIN_WINDOW, Event::Mouse(MouseEvent { kind, col, row, mods: Mods::NONE }));
    }
}

fn store_len(app: &App) -> usize {
    app.try_state::<Images>().map_or(0, Images::len)
}

#[test]
fn f3_toggles_and_only_one_side_previews() {
    let e = env();
    let mut app = app(&e);
    open_preview(&mut app);
    assert_eq!(app.main.active, 0, "the focus stays with the file list");
    press(&mut app, Key::F(3));
    assert_eq!(app.main.sides[1].mode, SideMode::Files);
    main_window::toggle_view(&mut app, 1, "preview");
    main_window::toggle_view(&mut app, 0, "preview");
    assert_eq!(app.main.sides[0].mode_id(), "preview");
    assert_eq!(app.main.sides[1].mode, SideMode::Files);
    assert_eq!(app.main.active, 1);
}

#[test]
fn preview_follows_the_cursor_in_the_other_panel() {
    let e = env();
    let mut app = app(&e);
    open_preview(&mut app);
    select(&mut app, "a.txt");
    wait_for(&mut app, "a.txt as text", |app| pane(app).current_id() == Some("text") && shown_text(app) == "alpha");
    assert_eq!(pane(&app).title, "a.txt");
    assert_eq!(pane(&app).info, "5 B");
    select(&mut app, "sub");
    wait_for(&mut app, "the folder listing", |app| shown_text(app).contains("c.txt"));
    assert_eq!(pane(&app).current_id(), Some("listing"));
    let frame = main_window::layout(&app).frames[1];
    assert!(pane(&app).chip_rect(frame, 0).is_none(), "a single candidate shows no chips");
    select(&mut app, "b.bin");
    wait_for(&mut app, "a hex dump", |app| shown_text(app).contains("00000000  00 00"));
    assert_eq!(pane(&app).current_id(), Some("hex"));
    assert!(pane(&app).info.ends_with("binary"));
}

#[test]
fn moving_on_quickly_keeps_the_old_content_until_the_new_one_is_ready() {
    let e = env();
    let mut app = app(&e);
    open_preview(&mut app);
    select(&mut app, "a.txt");
    wait_for(&mut app, "a.txt", |app| shown_text(app) == "alpha");
    // Run over b.bin to sub: debounced, b.bin is never loaded.
    select(&mut app, "b.bin");
    select(&mut app, "sub");
    assert_eq!(shown_text(&app), "alpha", "the previous content stays while the next one loads");
    assert!(pane(&app).loading());
    wait_for(&mut app, "sub", |app| shown_text(app).contains("c.txt"));
    assert_eq!(pane(&app).title, "sub");
    // Closing the preview forgets the file.
    press(&mut app, Key::F(3));
    assert!(pane(&app).content().is_none() && pane(&app).path.is_none());
}

#[test]
fn view_as_chips_cycle_and_are_remembered_per_extension() {
    let e = env();
    fs::write(e.src.join("notes.md"), "# Heading\n\n*emphasis*\n").unwrap();
    fs::write(e.src.join("more.md"), "# Other\n").unwrap();
    let mut app = app(&e);
    open_preview(&mut app);
    select(&mut app, "notes.md");
    wait_for(&mut app, "markdown", |app| pane(app).current_id() == Some("markdown"));
    assert_eq!(pane(&app).chips.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(), ["Markdown", "Text", "Hex"]);
    assert!(shown_text(&app).contains("Heading") && !shown_text(&app).contains('#'), "rendered");

    // Click the "Text" chip.
    let frame = main_window::layout(&app).frames[1];
    let chip = pane(&app).chip_rect(frame, 1).expect("chip shown");
    click(&mut app, chip.x + 1, chip.y, MouseButton::Left);
    wait_for(&mut app, "the source", |app| pane(app).current_id() == Some("text"));
    assert!(shown_text(&app).starts_with("# Heading"));

    select(&mut app, "more.md"); // remembered for .md files
    wait_for(&mut app, "more.md as text", |app| pane(app).current_id() == Some("text") && shown_text(app).starts_with("# Other"));

    press(&mut app, Key::F(3)); // close the preview…
    press(&mut app, Key::Tab); // (that side has the focus now)
    press(&mut app, Key::F(3)); // …and open it again: still remembered
    wait_for(&mut app, "text again", |app| pane(app).current_id() == Some("text"));
    press_with(&mut app, Key::F(3), Mods::SHIFT); // Shift+F3: text -> hex
    wait_for(&mut app, "hex", |app| pane(app).current_id() == Some("hex"));
    press_with(&mut app, Key::F(3), Mods::SHIFT); // hex -> markdown (wraps around)
    wait_for(&mut app, "markdown", |app| pane(app).current_id() == Some("markdown"));
}

#[test]
fn tab_moves_into_the_preview_and_keys_scroll_it() {
    let e = env();
    let long: String = (0..500).map(|i| format!("line {i}\n")).collect();
    fs::write(e.src.join("long.txt"), long).unwrap();
    let mut app = app(&e);
    open_preview(&mut app);
    select(&mut app, "long.txt");
    wait_for(&mut app, "long.txt", |app| shown_text(app).starts_with("line 0"));
    press(&mut app, Key::Tab);
    assert_eq!(app.main.active, 1);
    press(&mut app, Key::PageDown);
    let top = |app: &App| pane(app).content_as::<oc_ui::features::preview::text::TextView>().unwrap().top;
    assert!(top(&app) > 10);
    press(&mut app, Key::End);
    press(&mut app, Key::Home);
    assert_eq!(top(&app), 0);
    assert_eq!(app.main.sides[0].panel.cursor_name(), Some("long.txt"), "the file panel didn't move");
    press(&mut app, Key::Tab);
    assert_eq!(app.main.active, 0);
}

fn write_png(path: &Path, w: u32, h: u32) {
    image::RgbaImage::from_pixel(w, h, image::Rgba([0, 0, 255, 255])).save(path).unwrap();
}

#[test]
fn image_preview_fits_and_copies_the_full_size() {
    let e = env();
    write_png(&e.src.join("pic.png"), 64, 32);
    write_png(&e.src.join("wide.png"), 5000, 300); // wider than previews decode
    let mut app = app(&e);
    open_preview(&mut app);
    select(&mut app, "pic.png");
    wait_for(&mut app, "the image", |app| pane(app).current_id() == Some("image"));
    assert!(pane(&app).info.contains("64×32"), "{}", pane(&app).info);
    assert_eq!(store_len(&app), 1, "pixels in the image store");

    select(&mut app, "wide.png");
    wait_for(&mut app, "the wide image", |app| pane(app).title == "wide.png" && kind(app) == "image");
    let view = pane(&app).content_as::<ImageView>().unwrap();
    assert!(view.prep.width < 5000, "the preview is scaled down");
    wait_for(&mut app, "a smooth copy at the pane's size", |app| {
        pane(app).content_as::<ImageView>().is_some_and(|v| v.scaled.is_some())
    });
    assert_eq!(store_len(&app), 2, "the old image's pixels left the store");

    // Right-click: Open / Copy; Copy puts the full-size image on the clipboard.
    let frame = main_window::layout(&app).frames[1];
    click(&mut app, frame.x + frame.w / 2, frame.y + frame.h / 2, MouseButton::Right);
    assert_eq!(app.modal_titles(), ["popup menu"]);
    assert_eq!(app.top_modal_text().unwrap(), "Open\nCopy");
    press(&mut app, Key::Char('c'));
    wait_for(&mut app, "the copy", |app| app.toast.as_ref().is_some_and(|t| t.text.starts_with("Copied")));
    assert_eq!(app.toast.as_ref().unwrap().text, "Copied wide.png (5000×300)");

    press(&mut app, Key::F(3));
    assert_eq!(store_len(&app), 0, "closing the preview frees the pixels");
}

#[test]
fn animated_gifs_play() {
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame, RgbaImage};
    let e = env();
    let file = fs::File::create(e.src.join("anim.gif")).unwrap();
    let mut enc = GifEncoder::new(file);
    for c in [0u8, 255] {
        let img = RgbaImage::from_pixel(8, 8, image::Rgba([c, 0, 0, 255]));
        enc.encode_frame(Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(50, 1))).unwrap();
    }
    drop(enc);
    let mut app = app(&e);
    open_preview(&mut app);
    select(&mut app, "anim.gif");
    wait_for(&mut app, "the animation", |app| pane(app).current_id() == Some("image"));
    assert!(pane(&app).info.ends_with("animated"));
    wait_for(&mut app, "the next frame", |app| pane(app).content_as::<ImageView>().is_some_and(|v| v.frame == 1));
    wait_for(&mut app, "the first frame again", |app| pane(app).content_as::<ImageView>().is_some_and(|v| v.frame == 0));
}

/// Takes a second over `.slowpreview` files.
struct SlowPreviewer;

impl oc_preview::Previewer for SlowPreviewer {
    fn id(&self) -> &str {
        "slow-test"
    }

    fn title(&self) -> &str {
        "Slow"
    }

    fn score(&self, f: &oc_preview::FileInfo) -> i32 {
        if f.ext == "slowpreview" { oc_preview::Score::SPECIFIC } else { oc_preview::Score::NO }
    }

    fn load(&self, _: &oc_preview::PreviewSource, _: &oc_preview::LoadContext) -> Result<oc_preview::Loaded, oc_preview::PreviewError> {
        std::thread::sleep(Duration::from_secs(1));
        Ok(oc_preview::Loaded::new(oc_preview::PreviewData::StyledText(oc_preview::StyledText::plain("slow")), "slow"))
    }
}

/// 1.x named the new file as soon as it started loading it; 2.0 keeps the
/// previous content for a moment (no flicker), then does the same.
#[test]
fn a_slow_local_load_names_the_new_file() {
    let _ = oc_preview::Registry::global().register(Arc::new(SlowPreviewer), true);
    let e = env();
    fs::write(e.src.join("big.slowpreview"), "x").unwrap();
    let mut app = app(&e);
    open_preview(&mut app);
    select(&mut app, "a.txt");
    wait_for(&mut app, "a.txt", |app| shown_text(app) == "alpha");
    select(&mut app, "big.slowpreview");
    wait_for(&mut app, "the new name", |app| pane(app).title == "big.slowpreview");
    assert!(pane(&app).loading());
    assert_eq!((shown_text(&app).as_str(), pane(&app).info.as_str()), ("", ""), "not the old file's content");
    wait_for(&mut app, "the slow preview", |app| shown_text(app) == "slow");
}

fn mouse(app: &mut App, kind: MouseKind, col: u16, row: u16) {
    app.handle_event(MAIN_WINDOW, Event::Mouse(MouseEvent { kind, col, row, mods: Mods::NONE }));
}

/// 1.x's QTextBrowser: drag to select, double-click a word, ⌘C, and the
/// right-click menu Copy / Copy Link Location / Select All.
#[test]
fn preview_text_can_be_selected_and_copied() {
    let e = env();
    fs::write(e.src.join("lines.txt"), "first line\nsecond line\n").unwrap();
    fs::write(e.src.join("doc.md"), "See [the site](https://example.com) here.\n").unwrap();
    let mut app = app(&e);
    open_preview(&mut app);
    select(&mut app, "lines.txt");
    wait_for(&mut app, "the text", |app| shown_text(app).contains("second line"));
    let area = pane(&app).content_rect(main_window::layout(&app).frames[1]);
    // Drag from "line" to "sec"; the button comes up over the other panel.
    mouse(&mut app, MouseKind::Down(MouseButton::Left), area.x + 6, area.y);
    mouse(&mut app, MouseKind::Move, area.x + 3, area.y + 1);
    mouse(&mut app, MouseKind::Up(MouseButton::Left), 2, 2);
    mouse(&mut app, MouseKind::Move, area.x + 8, area.y + 1); // no button: no change
    assert_eq!(app.main.active, 1, "clicking the preview focuses it");
    press_with(&mut app, Key::Char('c'), Mods::primary());
    assert_eq!(app.clipboard.get_text().as_deref(), Some("line\nsec"));

    mouse(&mut app, MouseKind::DoubleClick(MouseButton::Left), area.x + 9, area.y + 1);
    press_with(&mut app, Key::Char('c'), Mods::primary());
    assert_eq!(app.clipboard.get_text().as_deref(), Some("line"));

    // Right-click → Select All, then Copy from the menu.
    click(&mut app, area.x + 2, area.y + 3, MouseButton::Right);
    assert_eq!(app.top_modal_text().unwrap(), "Copy\nCopy Link Location\nSelect All");
    press(&mut app, Key::Up);
    press(&mut app, Key::Enter);
    settle(&mut app);
    click(&mut app, area.x + 2, area.y + 3, MouseButton::Right); // a double-click to the app
    press(&mut app, Key::Enter);
    settle(&mut app);
    assert_eq!(app.clipboard.get_text().as_deref(), Some("first line\nsecond line\n"));

    // Markdown: Copy Link Location on a link; a plain click still opens nothing here.
    app.clipboard.set_text("");
    select(&mut app, "doc.md");
    wait_for(&mut app, "markdown", |app| kind(app) == "markdown");
    let area = pane(&app).content_rect(main_window::layout(&app).frames[1]);
    click(&mut app, area.x + 6, area.y, MouseButton::Right);
    press(&mut app, Key::Char('l'));
    settle(&mut app);
    assert_eq!(app.clipboard.get_text().as_deref(), Some("https://example.com"));
    mouse(&mut app, MouseKind::DoubleClick(MouseButton::Left), area.x + 1, area.y);
    press_with(&mut app, Key::Char('c'), Mods::primary());
    assert_eq!(app.clipboard.get_text().as_deref(), Some("See"));
}

#[test]
fn markdown_renders() {
    let e = env();
    fs::write(e.src.join("doc.md"), "# Title\n\n- one\n- two\n\n| a | b |\n|---|---|\n| 1 | 2 |\n").unwrap();
    let mut app = app(&e);
    open_preview(&mut app);
    select(&mut app, "doc.md");
    wait_for(&mut app, "markdown", |app| pane(app).current_id() == Some("markdown"));
    let text = shown_text(&app);
    assert!(text.contains("Title") && text.contains(" • one") && text.contains("│ 1 │ 2 │"), "{text}");
    assert!(pane(&app).info.ends_with("· markdown"));
}

fn pdf_view(app: &App) -> &PdfView {
    pane(app).content_as::<PdfView>().expect("a PDF shown")
}

#[test]
fn pdf_pages_turn_with_keys_and_render_at_the_pane_size() {
    if !oc_pdf::available() {
        eprintln!("PDFium not found; skipped (scripts/fetch-pdfium.sh)");
        return;
    }
    let e = env();
    fs::write(e.src.join("manual.pdf"), oc_pdf::testing::pdf_bytes(3)).unwrap();
    let mut app = app(&e);
    open_preview(&mut app);
    select(&mut app, "manual.pdf");
    wait_for(&mut app, "the PDF", |app| pane(app).current_id() == Some("pdf"));
    assert!(pane(&app).info.starts_with("3 pages"));
    assert_eq!((pdf_view(&app).count, pdf_view(&app).index, pdf_view(&app).counter_text()), (3, 0, "1 / 3".to_owned()));
    // Rendered for the pane (the first look was 1200 px wide).
    let frame = main_window::layout(&app).frames[1];
    let (cw, ch) = app.cell_px;
    let pane_px = ((frame.w - 2) as f32 * cw, (frame.h - 5) as f32 * ch);
    wait_for(&mut app, "the page at the pane's size", |app| {
        pdf_view(app).shown.as_ref().is_some_and(|p| p.width != 1200 && p.width as f32 <= pane_px.0 + 1.0)
    });

    press(&mut app, Key::Tab);
    press(&mut app, Key::PageDown);
    assert_eq!((pdf_view(&app).index, pdf_view(&app).counter_text()), (1, "2 / 3".to_owned()));
    wait_for(&mut app, "page 2", |app| pdf_view(app).shown.as_ref().is_some_and(|p| p.index == 1));
    press(&mut app, Key::End);
    assert_eq!(pdf_view(&app).index, 2);
    press(&mut app, Key::Space);
    assert_eq!(pdf_view(&app).index, 2, "stays on the last page");
    press(&mut app, Key::Backspace);
    assert_eq!(pdf_view(&app).index, 1);
    press(&mut app, Key::Home);
    assert_eq!(pdf_view(&app).index, 0);
    press(&mut app, Key::Up);
    assert_eq!(pdf_view(&app).index, 0);
    press(&mut app, Key::Tab);

    // The window grows: the page is rendered again, larger.
    let before = pdf_view(&app).shown.as_ref().unwrap().height;
    app.resize(MAIN_WINDOW, 160, 60);
    wait_for(&mut app, "a larger render", |app| pdf_view(app).shown.as_ref().is_some_and(|p| p.height > before));

    let pages = pdf_view(&app).pages.clone();
    select(&mut app, "a.txt"); // switching files closes the document
    wait_for(&mut app, "a.txt", |app| pane(app).current_id() == Some("text"));
    assert!(pages.render(0, 100, 100).is_err(), "closed");
}

#[test]
fn pdf_wheel_turns_one_page_per_step() {
    if !oc_pdf::available() {
        return;
    }
    let e = env();
    fs::write(e.src.join("long.pdf"), oc_pdf::testing::pdf_bytes(10)).unwrap();
    let mut app = app(&e);
    open_preview(&mut app);
    select(&mut app, "long.pdf");
    wait_for(&mut app, "the PDF", |app| pane(app).current_id() == Some("pdf"));
    let frame = main_window::layout(&app).frames[1];
    let wheel = |app: &mut App, lines: f32| {
        let m = MouseEvent { kind: MouseKind::Scroll(lines), col: frame.x + 5, row: frame.y + 5, mods: Mods::NONE };
        app.handle_event(MAIN_WINDOW, Event::Mouse(m));
    };
    for _ in 0..10 {
        wheel(&mut app, -1.0); // one flick
    }
    assert_eq!(pdf_view(&app).index, 1, "one flick, one page");
    std::thread::sleep(Duration::from_millis(400));
    wheel(&mut app, -1.0);
    assert_eq!(pdf_view(&app).index, 2);
    std::thread::sleep(Duration::from_millis(400));
    wheel(&mut app, 1.0);
    assert_eq!(pdf_view(&app).index, 1);

    // Trackpad momentum after the fingers lift turns no page (1.x ignored
    // ScrollMomentum), however long it goes on.
    std::thread::sleep(Duration::from_millis(400));
    app.wheel_momentum = true;
    wheel(&mut app, -1.0);
    app.wheel_momentum = false;
    assert_eq!(pdf_view(&app).index, 1, "momentum");
    wheel(&mut app, -1.0);
    assert_eq!(pdf_view(&app).index, 2, "a new flick after the momentum");
}

// ---------------------------------------------------------------------------
// Online-only (cloud) files and slow servers
// ---------------------------------------------------------------------------

/// A non-local file system whose files can be cloud placeholders that
/// arrive when `deliver` is set, or slow to stat.
#[derive(Default)]
struct CloudFs {
    files: Mutex<BTreeMap<String, Arc<Vec<u8>>>>,
    online_only: bool,
    deliver: Arc<AtomicBool>,
    bytes_read: Arc<AtomicU64>,
    slow: Option<Duration>,
}

struct Reader {
    data: Arc<Vec<u8>>,
    pos: u64,
    counter: Arc<AtomicU64>,
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let start = (self.pos as usize).min(self.data.len());
        let n = buf.len().min(self.data.len() - start);
        buf[..n].copy_from_slice(&self.data[start..start + n]);
        self.pos += n as u64;
        self.counter.fetch_add(n as u64, Ordering::SeqCst);
        Ok(n)
    }
}

impl Seek for Reader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::End(d) => (self.data.len() as i64 + d) as u64,
            SeekFrom::Current(d) => (self.pos as i64 + d) as u64,
        };
        Ok(self.pos)
    }
}

impl ReadStream for Reader {
    fn seekable(&self) -> bool {
        true
    }
}

impl CloudFs {
    fn entry(&self, name: &str, size: usize) -> Entry {
        let mut e = Entry::file(name, size as u64, 1_700_000_000.0);
        e.is_online_only = self.online_only;
        e
    }
}

impl FileSystem for CloudFs {
    fn type_name(&self) -> String {
        "test cloud".into()
    }

    fn list_dir(&self, _path: &str, opts: ListOptions) -> oc_vfs::Result<Vec<Entry>> {
        let files = self.files.lock().unwrap();
        Ok(sort_entries(files.iter().map(|(n, d)| self.entry(n, d.len())).collect(), opts))
    }

    fn stat(&self, path: &str) -> oc_vfs::Result<Entry> {
        if let Some(d) = self.slow {
            std::thread::sleep(d);
        }
        if path.is_empty() {
            return Ok(Entry::dir("", 0.0));
        }
        let files = self.files.lock().unwrap();
        let d = files.get(path).ok_or_else(|| VfsError::NotFound(path.into()))?;
        Ok(self.entry(path, d.len()))
    }

    fn parent_path(&self, path: &str) -> Option<String> {
        (!path.is_empty()).then(String::new)
    }

    fn join(&self, path: &str, names: &[&str]) -> String {
        let mut parts: Vec<&str> = if path.is_empty() { Vec::new() } else { vec![path] };
        parts.extend(names);
        parts.join("/")
    }

    fn basename(&self, path: &str) -> String {
        path.rsplit('/').next().unwrap_or("").to_string()
    }

    fn display(&self, path: &str) -> String {
        format!("cloud:/{path}")
    }

    fn open_read(&self, path: &str) -> oc_vfs::Result<Box<dyn ReadStream>> {
        let data = self.files.lock().unwrap().get(path).cloned().ok_or_else(|| VfsError::NotFound(path.into()))?;
        Ok(Box::new(Reader { data, pos: 0, counter: self.bytes_read.clone() }))
    }

    fn walk<'a>(&'a self, _path: &str) -> oc_vfs::Result<Box<dyn Iterator<Item = oc_vfs::Result<(RelPath, Entry)>> + 'a>> {
        Ok(Box::new(std::iter::empty()))
    }

    fn fetch_online_only(&self, path: &str, timeout: Duration, is_cancelled: &dyn Fn() -> bool) -> oc_vfs::Result<()> {
        let deadline = Instant::now() + timeout;
        while !self.deliver.load(Ordering::SeqCst) {
            if is_cancelled() {
                return Err(VfsError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(VfsError::FetchTimeout(path.into()));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }
}

fn show_fs(app: &mut App, fs: Arc<CloudFs>) {
    panel::load(app, 0, VPath::new(fs, ""), LoadOptions::default());
    settle(app);
}

fn message_button_click(app: &mut App) {
    // The button row is the message's last line; Tab in and press Enter.
    press(app, Key::Tab);
    press(app, Key::Enter);
    press(app, Key::Tab);
}

#[test]
fn big_online_only_files_need_an_explicit_download() {
    let e = env();
    let mut app = app(&e);
    let cloud = Arc::new(CloudFs { online_only: true, ..CloudFs::default() });
    cloud.deliver.store(true, Ordering::SeqCst);
    let mut big = b"hello cloud\n".to_vec();
    big.resize(30 * 1024 * 1024, b'x');
    cloud.files.lock().unwrap().insert("big.txt".into(), Arc::new(big));
    show_fs(&mut app, cloud.clone());
    open_preview(&mut app);
    select(&mut app, "big.txt");
    wait_for(&mut app, "the download offer", |app| kind(app) == "message" && shown_text(app).contains("stored online only"));
    assert_eq!(cloud.bytes_read.load(Ordering::SeqCst), 0, "nothing downloaded yet");
    let text = shown_text(&app);
    assert!(text.starts_with("This file is stored online only (30.0 M)."), "{text}");
    message_button_click(&mut app);
    wait_for(&mut app, "the text", |app| pane(app).current_id() == Some("text"));
    assert!(shown_text(&app).starts_with("hello cloud"));
}

#[test]
fn small_online_only_files_download_with_a_message_and_stuck_ones_offer_try_again() {
    let e = env();
    let mut app = app(&e);
    app.state::<PreviewOptions>().fetch_timeout = |_| Duration::from_millis(300);
    let cloud = Arc::new(CloudFs { online_only: true, ..CloudFs::default() });
    cloud.files.lock().unwrap().insert("a.txt".into(), Arc::new(b"from the cloud".to_vec()));
    show_fs(&mut app, cloud.clone());
    open_preview(&mut app);
    select(&mut app, "a.txt");
    wait_for(&mut app, "the download message", |app| shown_text(app).starts_with("Downloading a.txt from the cloud…\n14 B"));
    wait_for(&mut app, "the timeout", |app| shown_text(app).contains("hasn't arrived from the cloud"));
    assert!(shown_text(&app).starts_with("a.txt hasn't arrived from the cloud yet (waited 0 s)."));
    cloud.deliver.store(true, Ordering::SeqCst);
    message_button_click(&mut app); // "Try again"
    wait_for(&mut app, "the text", |app| shown_text(app) == "from the cloud");
}

#[test]
fn slow_servers_show_loading_after_a_moment() {
    let e = env();
    let mut app = app(&e);
    let server = Arc::new(CloudFs::default());
    server.files.lock().unwrap().insert("far.txt".into(), Arc::new(b"far away".to_vec()));
    show_fs(&mut app, server.clone());
    open_preview(&mut app);
    // From here on a stat takes a while.
    let slow = Arc::new(CloudFs { slow: Some(Duration::from_millis(700)), ..CloudFs::default() });
    slow.files.lock().unwrap().insert("far.txt".into(), Arc::new(b"far away".to_vec()));
    slow.files.lock().unwrap().insert("near.txt".into(), Arc::new(b"near".to_vec()));
    panel::load(&mut app, 0, VPath::new(slow, ""), LoadOptions::default());
    wait_for(&mut app, "the listing", |app| app.main.sides[0].panel.row_of("far.txt").is_some());
    select(&mut app, "far.txt");
    wait_for(&mut app, "the loading message", |app| shown_text(app) == "Loading far.txt…");
    wait_for(&mut app, "the text", |app| shown_text(app) == "far away");
}
