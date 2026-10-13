//! Selection rules, plugins, and online-only cloud files.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::fs::{LocalFs, MemFs};
use super::*;
use crate::builtin::ImagePreviewer;
use crate::{
    Access, FileInfo, LoadContext, Loaded, PreviewData, PreviewError, PreviewSource, Previewer, Registry, RegistryError,
    Score,
};

fn ids(reg: &Registry, src: &PreviewSource) -> Vec<String> {
    reg.candidates(src, true).unwrap().iter().map(|c| c.previewer.id().to_string()).collect()
}

fn local(path: &std::path::Path) -> PreviewSource {
    PreviewSource::new(LocalFs::vpath(path)).unwrap()
}

#[test]
fn ranking() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("a.md"), "# hi\n").unwrap();
    std::fs::write(d.join("a.py"), "print(1)\n").unwrap();
    std::fs::write(d.join("a.bin"), (0..=255u8).collect::<Vec<_>>()).unwrap();
    std::fs::write(d.join("e.txt"), "").unwrap();
    let reg = Registry::with_builtins();
    assert_eq!(ids(&reg, &local(&d.join("a.md"))), ["markdown", "text", "hex"]);
    assert_eq!(ids(&reg, &local(&d.join("a.py"))), ["text", "hex"]);
    assert_eq!(ids(&reg, &local(&d.join("a.bin"))), ["hex"]);
    assert_eq!(ids(&reg, &local(&d.join("e.txt"))), ["hex"]); // shows "(empty file)"
    assert_eq!(ids(&reg, &local(d)), ["listing"]);
}

#[test]
fn fallback_when_a_previewer_cannot_show_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.png");
    std::fs::write(&path, "not an image at all").unwrap();
    let outcome = Registry::with_builtins().preview(&LocalFs::vpath(&path), &LoadContext::default(), None);
    assert_eq!(outcome.candidate_ids(), ["image", "hex"]);
    assert_eq!(outcome.previewer_id(), Some("hex"));
    assert!(outcome.error.is_none());
}

#[test]
fn preferred_previewer_wins_if_it_fits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.md");
    std::fs::write(&path, "# hi\n").unwrap();
    let reg = Registry::with_builtins();
    let target = LocalFs::vpath(&path);
    let ctx = LoadContext::default();
    assert_eq!(reg.preview(&target, &ctx, Some("text")).previewer_id(), Some("text"));
    assert_eq!(reg.preview(&target, &ctx, Some("image")).previewer_id(), Some("markdown")); // doesn't fit: ignored
    assert_eq!(reg.preview(&target, &ctx, None).previewer_id(), Some("markdown"));
}

#[test]
fn media_is_chosen_by_name_without_reading() {
    let mem = Arc::new(MemFs::new());
    mem.add("movie.mp4", garbage(100_000, 3));
    let src = PreviewSource::new(mem.vpath("movie.mp4")).unwrap();
    assert_eq!(ids(&Registry::with_builtins(), &src)[0], "media");
    assert_eq!(mem.read(), 0); // the name decided; no sniffing round trip to the server
}

#[test]
fn head_is_read_once_and_resumed() {
    let mem = Arc::new(MemFs::new());
    mem.add("notes", "hello\n".repeat(5000));
    let src = PreviewSource::new(mem.vpath("notes")).unwrap();
    assert_eq!(ids(&Registry::with_builtins(), &src), ["text", "hex"]); // sniffed
    assert_eq!(mem.read(), 8192);
    assert_eq!(src.head(100).unwrap().len(), 100);
    assert_eq!(mem.read(), 8192); // cached
    assert_eq!(src.head(20_000).unwrap().len(), 20_000);
    assert_eq!(mem.read(), 20_000); // continued after the cached part
    assert_eq!(src.head(1_000_000).unwrap().len(), 30_000);
    assert_eq!(src.head(2_000_000).unwrap().len(), 30_000);
    assert_eq!(mem.read(), 30_000); // whole file known: no more reads
    assert_eq!(src.read_all(Some(10)).unwrap().len(), 30_000); // served from the cache
}

#[test]
fn unseekable_head_reads_from_the_start() {
    let mem = Arc::new(MemFs { unseekable: true, ..MemFs::default() });
    mem.add("notes", "abc".repeat(10_000));
    let src = PreviewSource::new(mem.vpath("notes")).unwrap();
    src.head(10).unwrap();
    assert_eq!(src.head(20).unwrap(), b"abcabcabcabcabcabcab");
    assert_eq!(mem.read(), 30);
}

#[test]
fn show_message_stops_the_search() {
    let reg = Registry::with_builtins();
    reg.register(Arc::new(ImagePreviewer { max_size: 10 }), true).unwrap();
    assert_eq!(reg.previewers()[1].id(), "image"); // replaced in place
    let mem = Arc::new(MemFs::new());
    mem.add("big.png", png_bytes(8, 4));
    let outcome = reg.preview(&mem.vpath("big.png"), &LoadContext::default(), None);
    let err = outcome.error.as_ref().unwrap();
    assert!(err.is_message() && err.to_string().contains("copy it with F5"), "{err}");
    assert!(err.to_string().starts_with("big.png is "), "{err}");
    assert!(err.to_string().contains("from test server"), "{err}");
    assert!(outcome.loaded.is_none());
    assert_eq!(mem.read(), 0); // no hex fallback, nothing fetched
}

struct StlPreviewer {
    id: &'static str,
    available: bool,
}

impl Previewer for StlPreviewer {
    fn id(&self) -> &str {
        self.id
    }
    fn title(&self) -> &str {
        "3D model"
    }
    fn access(&self) -> Access {
        Access::LocalFile
    }
    fn available(&self) -> bool {
        self.available
    }
    fn score(&self, f: &FileInfo) -> i32 {
        if f.ext == "stl" { Score::FORMAT } else { Score::NO }
    }
    fn load(&self, src: &PreviewSource, _ctx: &LoadContext) -> Result<Loaded, PreviewError> {
        let data = std::fs::read(src.local_file(None)?)?;
        Ok(Loaded::new(PreviewData::Message(String::from_utf8_lossy(&data[..5]).into_owned()), "mesh"))
    }
}

#[test]
fn register_a_previewer_at_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("part.stl");
    std::fs::write(&path, "solid part\n").unwrap();
    let reg = Registry::with_builtins();
    reg.register(Arc::new(StlPreviewer { id: "test-stl", available: true }), false).unwrap();
    let outcome = reg.preview(&LocalFs::vpath(&path), &LoadContext::default(), None);
    assert_eq!(outcome.previewer_id(), Some("test-stl"));
    let Some(Loaded { data: PreviewData::Message(m), info }) = &outcome.loaded else { panic!("{outcome:?}") };
    assert_eq!((m.as_str(), info.as_str()), ("solid", "mesh"));
    assert_eq!(
        reg.register(Arc::new(StlPreviewer { id: "test-stl", available: true }), false),
        Err(RegistryError::Duplicate("test-stl".into()))
    );
    assert_eq!(
        RegistryError::Duplicate("x".into()).to_string(),
        "A previewer with id 'x' is already registered"
    );
    reg.register(Arc::new(StlPreviewer { id: "test-unavailable", available: false }), false).unwrap();
    assert!(reg.get("test-unavailable").is_none()); // skipped: dependency missing
    reg.unregister("test-stl");
    assert!(reg.get("test-stl").is_none());
}

#[test]
fn stl_from_another_file_system_is_copied_to_the_cache() {
    let cache = tempfile::tempdir().unwrap();
    let mem = Arc::new(MemFs::new());
    mem.add("dir/part.stl", "solid remote\n");
    mem.add_dir("dir");
    let reg = Registry::new();
    reg.register(Arc::new(StlPreviewer { id: "test-stl", available: true }), false).unwrap();
    let ctx = LoadContext { cache_dir: Some(cache.path().to_path_buf()), ..LoadContext::default() };
    let outcome = reg.preview(&mem.vpath("dir/part.stl"), &ctx, None);
    assert_eq!(outcome.previewer_id(), Some("test-stl"), "{outcome:?}");
    let read = mem.read();
    let src = PreviewSource::new(mem.vpath("dir/part.stl")).unwrap().with_cache_dir(cache.path());
    let path = src.local_file(None).unwrap();
    assert!(path.starts_with(cache.path()) && path.ends_with("part.stl"));
    assert_eq!(std::fs::read(&path).unwrap(), b"solid remote\n");
    assert_eq!(mem.read(), read); // reused, not fetched again
    assert!(src.local_file(Some(3)).unwrap_err().is_message()); // too big to fetch
}

struct Panicky;

impl Previewer for Panicky {
    fn id(&self) -> &str {
        "panicky"
    }
    fn title(&self) -> &str {
        "Panicky"
    }
    fn score(&self, f: &FileInfo) -> i32 {
        if f.ext == "boom" {
            panic!("score exploded");
        }
        Score::SPECIFIC
    }
    fn load(&self, _src: &PreviewSource, _ctx: &LoadContext) -> Result<Loaded, PreviewError> {
        panic!("load exploded")
    }
}

#[test]
fn panicking_add_ons_do_not_break_previews() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.boom"), "a").unwrap();
    std::fs::write(dir.path().join("x.txt"), "a").unwrap();
    let reg = Registry::with_builtins();
    reg.register(Arc::new(Panicky), false).unwrap();
    let ctx = LoadContext::default();
    let o = reg.preview(&LocalFs::vpath(&dir.path().join("x.boom")), &ctx, None);
    assert_eq!(o.previewer_id(), Some("text")); // scoring panic: skipped
    let o = reg.preview(&LocalFs::vpath(&dir.path().join("x.txt")), &ctx, None);
    assert_eq!(o.previewer_id(), Some("panicky"));
    assert!(matches!(o.error, Some(PreviewError::Failed(_))));
}

#[test]
fn nothing_fits_and_missing_files() {
    let reg = Registry::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x"), "a").unwrap();
    let o = reg.preview(&LocalFs::vpath(&dir.path().join("x")), &LoadContext::default(), None);
    assert_eq!(o.error.unwrap().to_string(), "No previewer can show this file");
    let o = reg.preview(&LocalFs::vpath(&dir.path().join("gone")), &LoadContext::default(), None);
    assert!(matches!(o.error, Some(PreviewError::Vfs(ref e)) if e.is_not_found()));
}

#[test]
fn cancelled_preview_reports_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a").unwrap();
    let ctx = LoadContext { is_cancelled: Arc::new(|| true), ..LoadContext::default() };
    let o = Registry::with_builtins().preview(&LocalFs::vpath(&dir.path().join("a.txt")), &ctx, None);
    assert!(o.loaded.is_none() && o.error.is_none() && o.previewer.is_none());
}

#[test]
fn pdf_ranking_and_sniffing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("doc.pdf"), pdf_bytes()).unwrap();
    std::fs::write(dir.path().join("download"), pdf_bytes()).unwrap(); // no extension: recognised by "%PDF-"
    let reg = Registry::with_builtins();
    assert_eq!(ids(&reg, &local(&dir.path().join("doc.pdf"))), ["pdf", "hex"]);
    assert_eq!(ids(&reg, &local(&dir.path().join("download")))[0], "pdf");
}

#[test]
fn pdf_on_another_file_system_is_streamed() {
    let cache = tempfile::tempdir().unwrap();
    let mem = Arc::new(MemFs::new());
    mem.add("remote.pdf", pdf_bytes());
    let ctx = LoadContext { cache_dir: Some(cache.path().to_path_buf()), ..LoadContext::default() };
    let outcome = Registry::with_builtins().preview(&mem.vpath("remote.pdf"), &ctx, None);
    let Some(Loaded { data: PreviewData::Pdf(pdf), .. }) = &outcome.loaded else { panic!("{outcome:?}") };
    assert!(matches!(pdf.source, crate::DocumentSource::Stream(_)));
    assert_eq!(std::fs::read_dir(cache.path()).unwrap().count(), 0); // nothing downloaded
}

#[test]
fn garbage_pdf_falls_back_to_hex() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("broken.pdf"), "this is not a pdf").unwrap();
    let o = Registry::with_builtins().preview(&LocalFs::vpath(&dir.path().join("broken.pdf")), &LoadContext::default(), None);
    assert_eq!(o.previewer_id(), Some("hex"));
    assert!(o.error.is_none());
}

// -- online-only cloud files (Dropbox, iCloud, OneDrive placeholders) ------------------------

fn cloud() -> Arc<MemFs> {
    Arc::new(MemFs { online_only: true, ..MemFs::default() })
}

#[test]
fn big_online_only_file_is_not_downloaded() {
    let fs = cloud();
    fs.add_generated("big.pdf", 30 * 1024 * 1024, b'%');
    fs.add("notes.unknown", vec![b'x'; 30 * 1024 * 1024]); // would need sniffing to classify
    let reg = Registry::with_builtins();
    for name in ["big.pdf", "notes.unknown"] {
        let o = reg.preview(&fs.vpath(name), &LoadContext::default(), None);
        assert!(matches!(o.error, Some(PreviewError::NeedsDownload { size }) if size == 30 * 1024 * 1024), "{o:?}");
        assert!(o.loaded.is_none());
        assert_eq!(
            o.error.unwrap().to_string(),
            "This file is stored online only (30.0 M).\n\nPreviewing it means downloading it from the cloud first."
        );
    }
    assert_eq!(fs.read(), 0); // not even the first bytes for sniffing
    assert_eq!(fs.fetches.load(Ordering::SeqCst), 0);
    fs.add("big.pdf", [pdf_bytes(), vec![0; 30 * 1024 * 1024]].concat());
    let allowed = LoadContext { download_limit: None, ..LoadContext::default() };
    let o = reg.preview(&fs.vpath("big.pdf"), &allowed, None); // the user asked for it
    assert_eq!(o.previewer_id(), Some("pdf"));
    assert_eq!(fs.fetches.load(Ordering::SeqCst), 1); // fetched before reading
}

#[test]
fn small_online_only_file_previews_automatically() {
    let fs = cloud();
    fs.add("small.txt", "hello from the cloud\n");
    let o = Registry::with_builtins().preview(&fs.vpath("small.txt"), &LoadContext::default(), None);
    assert_eq!(o.previewer_id(), Some("text"));
    assert!(o.error.is_none());
}

#[test]
fn stuck_cloud_download_times_out_with_explanation() {
    let deliver = Arc::new(AtomicBool::new(false));
    let fs = Arc::new(MemFs { online_only: true, deliver: Some(deliver.clone()), ..MemFs::default() });
    fs.add("report.pdf", pdf_bytes());
    let reg = Registry::with_builtins();
    let ctx = LoadContext { fetch_timeout: |_| Duration::from_millis(300), ..LoadContext::default() };
    let t0 = Instant::now();
    let o = reg.preview(&fs.vpath("report.pdf"), &ctx, None);
    assert!(t0.elapsed() < Duration::from_secs(2));
    let err = o.error.as_ref().unwrap();
    assert!(matches!(err, PreviewError::CloudNotDelivered { .. }) && err.is_message());
    assert!(err.to_string().starts_with("report.pdf hasn't arrived from the cloud yet (waited 0 s)."), "{err}");
    assert_eq!(o.candidate_ids(), ["pdf", "hex"]); // by name, without reading
    assert_eq!(fs.read(), 0);

    let cancelled = LoadContext { is_cancelled: Arc::new(|| true), ..ctx.clone() };
    let o = reg.preview(&fs.vpath("report.pdf"), &cancelled, None);
    assert!(o.error.is_none() && o.candidates.is_empty()); // moved on: no message

    deliver.store(true, Ordering::SeqCst); // the cloud app catches up: "Try again" works
    let o = reg.preview(&fs.vpath("report.pdf"), &ctx, None);
    assert_eq!(o.previewer_id(), Some("pdf"));
}

#[test]
fn fetch_timeout_grows_with_size() {
    assert_eq!(crate::fetch_timeout(0), Duration::from_secs(20));
    assert_eq!(crate::fetch_timeout(10 * 1024 * 1024), Duration::from_secs(30));
    let msg = PreviewError::CloudNotDelivered { name: "a".into(), waited: Duration::from_secs(25) }.to_string();
    assert_eq!(
        msg,
        "a hasn't arrived from the cloud yet (waited 25 s).\n\nCheck that the cloud app (Dropbox, iCloud Drive, \
         OneDrive) is running, signed in and online. The download continues in the background."
    );
}
