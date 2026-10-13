//! Edge cases found in review: cache copies, archive fetches, image and SVG
//! robustness, PDF headers, highlighting limits.

use std::sync::Arc;

use super::fs::{LocalFs, MemFs};
use super::*;
use crate::highlight::{MAX_HIGHLIGHT_BYTES, highlight_lines, syntax_for_file, within_highlight_limit};
use crate::{LoadContext, Loaded, PreviewData, PreviewSource, Registry};

#[test]
fn concurrent_copies_of_one_file_are_never_torn() {
    let cache = tempfile::tempdir().unwrap();
    let mem = Arc::new(MemFs::new());
    let data = garbage(3 * 1024 * 1024, 42);
    mem.add("big.stl", data.clone());
    let paths: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (mem, cache) = (mem.clone(), cache.path());
                s.spawn(move || {
                    let src = PreviewSource::new(mem.vpath("big.stl")).unwrap().with_cache_dir(cache);
                    src.local_file(None).unwrap()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for p in &paths {
        assert_eq!(p, &paths[0]);
        assert!(std::fs::read(p).unwrap() == data, "torn copy at {}", p.display());
    }
    // No partial files left behind.
    let dir = paths[0].parent().unwrap();
    assert_eq!(std::fs::read_dir(dir).unwrap().count(), 1);
}

#[test]
fn archive_listing_does_not_download_a_second_copy() {
    let cache = tempfile::tempdir().unwrap();
    let mem = Arc::new(MemFs::new());
    mem.add("a.zip", garbage(10_000, 5));
    let ctx = LoadContext {
        cache_dir: Some(cache.path().to_path_buf()),
        open_archive: Some(Arc::new(|_t: &oc_vfs::VPath| {
            let fs = Arc::new(MemFs::new());
            fs.add("inside.txt", "hi");
            Ok(fs.vpath(""))
        })),
        ..LoadContext::default()
    };
    let o = Registry::with_builtins().preview(&mem.vpath("a.zip"), &ctx, None);
    assert_eq!(o.previewer_id(), Some("listing"));
    assert_eq!(mem.read(), 0); // the opener fetches it, not the previewer
    assert_eq!(std::fs::read_dir(cache.path()).unwrap().count(), 0);

    // Too big to fetch: the message, without reading anything.
    mem.add_generated("huge.zip", crate::builtin::MAX_FETCH_BYTES + 1, 0);
    let o = Registry::with_builtins().preview(&mem.vpath("huge.zip"), &ctx, None);
    let err = o.error.unwrap();
    assert!(err.is_message() && err.to_string().contains("copy it with F5"), "{err}");
    assert_eq!(mem.read(), 0);
}

#[test]
fn svg_does_not_load_files_it_names() {
    let dir = tempfile::tempdir().unwrap();
    let png = dir.path().join("secret.png");
    std::fs::write(&png, png_bytes(4, 4)).unwrap();
    let svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="4" height="4"><image width="4" height="4" xlink:href="{}"/></svg>"#,
        png.display()
    );
    let img = crate::builtin::decode_svg(svg.as_bytes()).unwrap();
    assert!(img.frames[0].rgba.iter().all(|&b| b == 0), "the referenced file was rendered");
    // Embedded images still work.
    let b64 = base64_encode(&png_bytes(4, 4));
    let svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><image width="4" height="4" href="data:image/png;base64,{b64}"/></svg>"#
    );
    let img = crate::builtin::decode_svg(svg.as_bytes()).unwrap();
    assert_eq!(&img.frames[0].rgba[..4], &[0, 128, 0, 255]);
}

fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A TGA header claiming 65535×65535 RGBA pixels (16 GiB), with no pixel data.
fn huge_tga() -> Vec<u8> {
    let mut h = vec![0u8, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    h.extend_from_slice(&65535u16.to_le_bytes());
    h.extend_from_slice(&65535u16.to_le_bytes());
    h.extend_from_slice(&[32, 0x28]);
    h
}

#[test]
fn images_claiming_huge_sizes_are_refused_before_allocating() {
    let err = crate::builtin::decode_image_as(&huge_tga(), Some(image::ImageFormat::Tga)).unwrap_err();
    assert!(err.contains("too large"), "{err}");
    // GIF header: 65535×65535 logical screen, no frames.
    let gif = [b"GIF89a".as_slice(), &[0xff, 0xff, 0xff, 0xff, 0, 0, 0], b";"].concat();
    assert!(crate::builtin::decode_image(&gif).is_err());
}

#[test]
fn signature_less_formats_decode_by_extension() {
    let img = image::RgbaImage::from_pixel(3, 2, image::Rgba([1, 2, 3, 255]));
    let mut tga = std::io::Cursor::new(Vec::new());
    img.write_to(&mut tga, image::ImageFormat::Tga).unwrap();
    let mem = Arc::new(MemFs::new());
    mem.add("pic.tga", tga.into_inner());
    let o = Registry::with_builtins().preview(&mem.vpath("pic.tga"), &LoadContext::default(), None);
    assert_eq!(o.previewer_id(), Some("image"), "{o:?}");
    assert!(o.loaded.unwrap().info.starts_with("3×2 · TGA"));
}

#[test]
fn misnamed_images_decode_by_contents() {
    let mem = Arc::new(MemFs::new());
    mem.add("drawing.png", SVG);
    mem.add("photo.svg", png_bytes(2, 2));
    let reg = Registry::with_builtins();
    let o = reg.preview(&mem.vpath("drawing.png"), &LoadContext::default(), None);
    assert!(o.loaded.as_ref().unwrap().info.starts_with("20×10 · SVG"), "{o:?}");
    let o = reg.preview(&mem.vpath("photo.svg"), &LoadContext::default(), None);
    assert!(o.loaded.as_ref().unwrap().info.starts_with("2×2 · PNG"), "{o:?}");
}

#[test]
fn pdf_header_after_leading_junk() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("scan.pdf");
    std::fs::write(&p, [b"\r\n\0junk from a mail gateway\n".as_slice(), &pdf_bytes()].concat()).unwrap();
    let o = Registry::with_builtins().preview(&LocalFs::vpath(&p), &LoadContext::default(), None);
    assert_eq!(o.previewer_id(), Some("pdf"));
    assert!(matches!(o.loaded, Some(Loaded { data: PreviewData::Pdf(_), .. })));
}

#[test]
fn highlight_limit_counts_characters_like_python() {
    assert!(within_highlight_limit(&"x".repeat(MAX_HIGHLIGHT_BYTES)));
    assert!(!within_highlight_limit(&"x".repeat(MAX_HIGHLIGHT_BYTES + 1)));
    let wide = "é".repeat(MAX_HIGHLIGHT_BYTES); // 512 KB, but 256 K characters
    assert!(within_highlight_limit(&wide));
    assert!(!within_highlight_limit(&format!("{wide}é")));
}

#[test]
fn highlighting_keeps_every_character() {
    let py = syntax_for_file("a.py").unwrap();
    for text in ["a = 'x\ry'\n", "x = 1\r\n", "\r", "a\n\nb", "s = \"é😀\"\r", ""] {
        let lines = highlight_lines(text, py, &|| false).unwrap();
        let joined = lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
        assert_eq!(joined, text.replace("\r\n", "\n"), "{text:?}");
        for l in &lines {
            assert!(l.runs.iter().all(|r| r.start < r.end && r.end <= l.text.len()));
        }
    }
}

#[test]
fn long_lines_still_poll_for_cancellation() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let polls = AtomicUsize::new(0);
    let text = format!("{}\n", "x = 1; ".repeat(10_000)).repeat(3); // 3 lines of 70 KB
    let cancel = || polls.fetch_add(1, Ordering::SeqCst) >= 1;
    assert!(highlight_lines(&text, syntax_for_file("a.py").unwrap(), &cancel).is_none());
}

#[test]
fn read_all_never_reads_past_the_limit() {
    let mem = Arc::new(MemFs::new());
    mem.add("a.png", png_bytes(2, 2));
    let src = PreviewSource::new(mem.vpath("a.png")).unwrap();
    assert!(src.read_all(Some(10)).unwrap_err().is_message());
    assert_eq!(mem.read(), 0);
    let n = png_bytes(2, 2).len() as u64;
    assert_eq!(src.read_all(Some(n)).unwrap().len() as u64, n);
}
