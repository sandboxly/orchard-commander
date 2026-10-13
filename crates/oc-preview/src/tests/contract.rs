//! The contract every registered previewer must meet: sane identity, scores
//! for any input, and `load()` that returns data or refuses cleanly
//! (CannotPreview / a message) for empty, garbage, valid and huge files —
//! never a panic, never another error.

use std::sync::Arc;

use super::fs::{LocalFs, MemFs};
use super::*;
use crate::{Access, FileInfo, LoadContext, PreviewError, PreviewSource, Registry};

const EXTS: &[&str] = &[
    "txt", "md", "py", "png", "jpg", "gif", "webp", "svg", "mp4", "mp3", "zip", "tar.gz", "7z", "rar", "pdf", "bin", "",
];

fn corpus() -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    for (i, ext) in EXTS.iter().enumerate() {
        let suffix = if ext.is_empty() { String::new() } else { format!(".{ext}") };
        files.push((format!("empty{suffix}"), Vec::new()));
        files.push((format!("garbage{suffix}"), garbage(3000, i as u64 + 1)));
        files.push((format!("text{suffix}"), b"plain words\nand more\n".to_vec()));
    }
    files.push(("valid.png".into(), png_bytes(8, 4)));
    files.push(("valid.gif".into(), gif_bytes(3)));
    files.push(("valid.webp".into(), webp_bytes(5, 5)));
    files.push(("valid.svg".into(), SVG.to_vec()));
    files.push(("valid.md".into(), b"# Title\n\nSome *markdown*.\n".to_vec()));
    files.push(("valid.pdf".into(), pdf_bytes()));
    files.push(("pdf-without-extension".into(), pdf_bytes()));
    files.push(("valid.zip".into(), b"PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0".to_vec()));
    files.push(("bad-svg.svg".into(), b"<svg><rect".to_vec()));
    files.push(("binary.txt".into(), b"\0\0\0\x01".to_vec()));
    files
}

/// Huge files, sparse on disk / generated in memory: previewers must not read them whole.
const HUGE: &[&str] = &["huge.txt", "huge.png", "huge.pdf", "huge.bin", "huge.md", "huge.mp4"];
const HUGE_SIZE: u64 = 3 * 1024 * 1024 * 1024;

struct Sources {
    _dir: Option<tempfile::TempDir>,
    mem: Option<Arc<MemFs>>,
    list: Vec<PreviewSource>,
}

fn local_sources() -> Sources {
    let dir = tempfile::tempdir().unwrap();
    let mut names = Vec::new();
    for (name, data) in corpus() {
        std::fs::write(dir.path().join(&name), data).unwrap();
        names.push(name);
    }
    for name in HUGE {
        let f = std::fs::File::create(dir.path().join(name)).unwrap();
        f.set_len(HUGE_SIZE).unwrap();
        names.push(name.to_string());
    }
    std::fs::create_dir(dir.path().join("folder")).unwrap();
    names.push("folder".into());
    let cache = dir.path().join(".cache");
    let list = names
        .iter()
        .map(|n| PreviewSource::new(LocalFs::vpath(&dir.path().join(n))).unwrap().with_cache_dir(&cache))
        .collect();
    Sources { _dir: Some(dir), mem: None, list }
}

fn mem_sources(cache: &std::path::Path) -> Sources {
    let mem = Arc::new(MemFs::new());
    let mut names = Vec::new();
    for (name, data) in corpus() {
        mem.add(&name, data);
        names.push(name);
    }
    for name in HUGE {
        mem.add_generated(name, HUGE_SIZE, b'x');
        names.push(name.to_string());
    }
    mem.add_dir("folder");
    mem.add("folder/inner.txt", "x");
    names.push("folder".into());
    let list = names.iter().map(|n| PreviewSource::new(mem.vpath(n)).unwrap().with_cache_dir(cache)).collect();
    Sources { _dir: None, mem: Some(mem), list }
}

fn ctx() -> LoadContext {
    LoadContext {
        // A fake archive backend: every archive holds one file.
        open_archive: Some(Arc::new(|_target| {
            let fs = Arc::new(MemFs::new());
            fs.add("inside.txt", "hi");
            Ok(fs.vpath(""))
        })),
        ..LoadContext::default()
    }
}

#[test]
fn identity() {
    let reg = Registry::with_builtins();
    let mut ids = Vec::new();
    for p in reg.previewers() {
        assert!(!p.id().is_empty() && !p.title().is_empty());
        assert!(Arc::ptr_eq(&reg.get(p.id()).unwrap(), &p));
        let _: Access = p.access();
        ids.push(p.id().to_string());
    }
    assert_eq!(ids, ["listing", "image", "pdf", "media", "markdown", "text", "hex"]);
}

#[test]
fn score_handles_anything() {
    let weird = [
        FileInfo { has_direct_path: true, ..FileInfo::default() },
        FileInfo { name: ".hidden".into(), size: 10, head: Some(Vec::new()), ..FileInfo::default() },
        FileInfo { name: "dir.zip".into(), ext: "zip".into(), is_dir: true, has_direct_path: true, ..FileInfo::default() },
        FileInfo {
            name: format!("{}.PNG", "x".repeat(500)),
            ext: "png".into(),
            size: 10u64.pow(12),
            head: Some(garbage(64, 7)),
            ..FileInfo::default()
        },
        FileInfo { name: "a.md".into(), ext: "md".into(), size: 5, has_direct_path: true, head: Some(vec![0, 1]), ..FileInfo::default() },
        FileInfo { name: "ünïcødé.😀".into(), ext: "😀".into(), size: u64::MAX, mtime: f64::NAN, ..FileInfo::default() },
    ];
    for p in Registry::with_builtins().previewers() {
        for info in &weird {
            assert!(p.score(info) >= 0, "{} scored {:?} negative", p.id(), info.name);
        }
    }
}

fn check_contract(sources: &Sources, label: &str) {
    let ctx = ctx();
    for p in Registry::with_builtins().previewers() {
        let mut claimed = 0;
        for src in &sources.list {
            let info = src.info(true).unwrap();
            if p.score(&info) <= 0 {
                continue;
            }
            claimed += 1;
            let before = sources.mem.as_ref().map_or(0, |m| m.read());
            match p.load(src, &ctx) {
                Ok(loaded) => {
                    let _ = loaded.data.plain_text();
                    assert!(!loaded.info.contains('\n'), "{label}: {} on {}: {:?}", p.id(), src.name(), loaded.info);
                }
                Err(PreviewError::CannotPreview(_)) => {}
                Err(e) if e.is_message() => {}
                Err(e) => panic!("{label}: {} on {} failed with {e:?}", p.id(), src.name()),
            }
            if let Some(mem) = &sources.mem {
                let read = mem.read() - before;
                assert!(read <= 2 * 1024 * 1024, "{label}: {} read {read} bytes of {}", p.id(), src.name());
            }
        }
        assert!(claimed > 0, "{label}: {} claimed no file of the test corpus", p.id());
    }
}

#[test]
fn load_returns_or_refuses_cleanly_local() {
    check_contract(&local_sources(), "local");
}

#[test]
fn load_returns_or_refuses_cleanly_other_fs() {
    let cache = tempfile::tempdir().unwrap();
    check_contract(&mem_sources(cache.path()), "other-fs");
}

#[test]
fn load_on_unseekable_streams() {
    let cache = tempfile::tempdir().unwrap();
    let mem = Arc::new(MemFs { unseekable: true, ..MemFs::default() });
    for (name, data) in corpus() {
        mem.add(&name, data);
    }
    let ctx = ctx();
    for p in Registry::with_builtins().previewers() {
        for (name, _) in corpus() {
            let src = PreviewSource::new(mem.vpath(&name)).unwrap().with_cache_dir(cache.path());
            if p.score(&src.info(true).unwrap()) > 0 {
                match p.load(&src, &ctx) {
                    Ok(_) | Err(PreviewError::CannotPreview(_)) => {}
                    Err(e) if e.is_message() => {}
                    Err(e) => panic!("{} on {name}: {e:?}", p.id()),
                }
            }
        }
    }
}
