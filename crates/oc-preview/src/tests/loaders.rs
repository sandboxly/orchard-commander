//! Content helpers and what the built-in loaders produce.

use std::sync::Arc;
use std::time::Duration;

use super::fs::{LocalFs, MemFs};
use super::*;
use crate::content::{self, Kind, MAX_TEXT_BYTES};
use crate::{Color, LoadContext, Loaded, PreviewData, PreviewSource, Registry, SyntaxRole};

fn load(target: &oc_vfs::VPath) -> (String, Loaded) {
    let o = Registry::with_builtins().preview(target, &LoadContext::default(), None);
    assert!(o.error.is_none(), "{o:?}");
    (o.previewer_id().unwrap().to_string(), o.loaded.unwrap())
}

// -- content ---------------------------------------------------------------------------------

#[test]
fn detect_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("a.py"), "print('hi')\n").unwrap();
    std::fs::write(d.join("r.md"), "# Title\n").unwrap();
    std::fs::write(d.join("x.bin"), (0..=255u8).collect::<Vec<_>>()).unwrap();
    std::fs::write(d.join("e.txt"), "").unwrap();
    std::fs::write(d.join("p.png"), "not really").unwrap();
    std::fs::write(d.join("v.mkv"), "").unwrap();
    std::fs::write(d.join("s.flac"), "").unwrap();
    std::fs::write(d.join("noext"), "plain words").unwrap();
    std::fs::write(d.join("a.tar.gz"), "").unwrap();
    let k = |n: &str| content::detect(&LocalFs::vpath(&d.join(n)));
    assert_eq!(content::detect(&LocalFs::vpath(d)), Kind::Directory);
    assert_eq!(k("a.py"), Kind::Text);
    assert_eq!(k("r.md"), Kind::Markdown);
    assert_eq!(k("x.bin"), Kind::Binary);
    assert_eq!(k("e.txt"), Kind::Empty);
    assert_eq!(k("p.png"), Kind::Image); // by extension; decoding decides later
    assert_eq!(k("v.mkv"), Kind::Video);
    assert_eq!(k("s.flac"), Kind::Audio);
    assert_eq!(k("noext"), Kind::Text);
    assert_eq!(k("a.tar.gz"), Kind::Archive);
    assert_eq!(k("missing"), Kind::Unreadable);
    assert_eq!(Kind::Binary.as_str(), "binary");
}

#[test]
fn looks_like_text_handles_split_utf8_and_latin1() {
    let s = "héllo wörld".as_bytes();
    assert!(content::looks_like_text(&s[..s.len() - 1])); // cut inside the last char: still text
    assert!(content::looks_like_text(b"Gr\xfc\xdfe aus K\xf6ln"));
    assert!(!content::looks_like_text(b"\x00\x01\x02binary"));
    assert!(content::looks_like_text(b""));
    assert!(!content::looks_like_text(&[0x80u8; 100])); // C1 bytes: not printable
}

#[test]
fn read_text_truncates_and_decodes() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("big.txt");
    std::fs::write(&f, "x".repeat(100)).unwrap();
    let c = content::read_text(&f, 10).unwrap();
    assert_eq!((c.text.as_str(), c.truncated), ("xxxxxxxxxx", true));
    assert!(!content::read_text(&f, MAX_TEXT_BYTES).unwrap().truncated);
    assert_eq!(content::decode_text(b"a\r\nb", false).text, "a\nb");
    assert_eq!(content::decode_text(b"caf\xe9", false).text, "café"); // Latin-1
    assert_eq!(content::decode_text(b"ok \xe2\x82", true).text, "ok "); // cut char dropped
    assert_eq!(content::decode_text(b"a\xffb\xe2\x82", true).text, "ab");
}

#[test]
fn hexdump_format() {
    let dump = content::hexdump(b"ABCDEFGHIJKLMNOPQ", 0);
    let lines: Vec<&str> = dump.lines().collect();
    assert_eq!(lines[0], "00000000  41 42 43 44 45 46 47 48  49 4a 4b 4c 4d 4e 4f 50  ABCDEFGHIJKLMNOP");
    assert_eq!(lines[1], format!("00000010  51{}  Q", " ".repeat(46)));
    assert_eq!(content::hexdump(b"", 0), "");
    assert_eq!(content::hexdump(b"\x00\x7f", 0x100), format!("00000100  00 7f{}  ..", " ".repeat(43)));
}

#[test]
fn directory_summary() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("pkg")).unwrap();
    std::fs::write(src.join("main.py"), "print(1)\n").unwrap();
    std::fs::write(src.join("util.py"), "x = 1\n").unwrap();
    std::fs::write(src.join(".hidden"), "h").unwrap();
    let target = LocalFs::vpath(&src);
    let text = content::directory_summary(&target, false).unwrap();
    assert!(text.starts_with("1 directory, 2 files (15 B)\n\npkg/\nmain.py"), "{text}");
    assert!(!text.contains(".hidden"));
    assert!(content::directory_summary(&target, true).unwrap().contains(".hidden"));
}

#[test]
fn format_listing_limits_and_plurals() {
    let entries: Vec<oc_vfs::Entry> = (0..503).map(|i| oc_vfs::Entry::file(format!("f{i:03}"), 1, 0.0)).collect();
    let text = content::format_listing(&entries, 500);
    assert!(text.starts_with("0 directories, 503 files (503 B)"));
    assert!(text.ends_with("\n… and 3 more"));
    assert_eq!(text.lines().nth(2).unwrap(), format!("{:<40} {:>9}", "f000", "1 B"));
    let one = content::format_listing(&[oc_vfs::Entry::file("a", 1, 0.0)], 500);
    assert!(one.starts_with("0 directories, 1 file (1 B)"));
    let listing = crate::ListingData::new(entries, 500);
    assert_eq!(listing.text(), text);
}

#[test]
fn archives_and_facts() {
    assert_eq!(content::archive_type("x.TAR.GZ"), Some("tar"));
    assert_eq!(content::archive_type("x.gz"), Some("gz"));
    assert_eq!(content::archive_type(".zip"), None);
    assert!(!content::is_archive("zip"));
    assert_eq!(content::thousands(1234567), "1,234,567");
    assert_eq!(content::thousands(999), "999");
    let facts = content::describe_entry(&oc_vfs::Entry::file("a.bin", 2048, 0.0), "/tmp");
    assert_eq!(facts[2], ("Size".to_string(), "2.0 K (2,048 bytes)".to_string()));
    assert_eq!(facts.len(), 3); // no mtime: no "Modified"
}

#[cfg(unix)]
#[test]
fn describe_local_file_with_permissions_and_link() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("run.sh");
    std::fs::write(&f, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o4755)).unwrap();
    let facts = content::describe(&f).unwrap();
    let get = |k: &str| facts.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
    assert_eq!(get("Permissions").as_deref(), Some("-rwsr-xr-x"));
    std::os::unix::fs::symlink(&f, dir.path().join("link")).unwrap();
    let facts = content::describe(&dir.path().join("link")).unwrap();
    assert!(facts.iter().any(|(k, v)| k == "Link to" && v.ends_with("run.sh")));
    assert!(facts.iter().any(|(k, v)| k == "Permissions" && v.starts_with('l')));
}

// -- text, markdown, hex, listing ------------------------------------------------------------

#[test]
fn text_is_highlighted_by_name() {
    let mem = Arc::new(MemFs::new());
    mem.add("a.py", "def f():\n    return 1\n");
    mem.add("notes.txt", "def f():\n");
    let (id, loaded) = load(&mem.vpath("a.py"));
    assert_eq!((id.as_str(), loaded.info.as_str()), ("text", "22 B"));
    let PreviewData::StyledText(t) = loaded.data else { panic!() };
    assert_eq!(t.language.as_deref(), Some("Python"));
    assert_eq!(t.text(), "def f():\n    return 1\n");
    assert!(t.lines[0].runs.iter().any(|r| r.style.fg == Some(Color::Syntax(SyntaxRole::Keyword))));
    let (_, loaded) = load(&mem.vpath("notes.txt"));
    let PreviewData::StyledText(t) = loaded.data else { panic!() };
    assert!(t.language.is_none() && t.lines.iter().all(|l| l.runs.is_empty()));
}

#[test]
fn big_text_shows_the_first_megabyte_unhighlighted() {
    let mem = Arc::new(MemFs::new());
    mem.add_generated("big.py", 5 * 1024 * 1024, b'x');
    let (id, loaded) = load(&mem.vpath("big.py"));
    assert_eq!((id.as_str(), loaded.info.as_str()), ("text", "5.0 M · first 1 MB"));
    let PreviewData::StyledText(t) = loaded.data else { panic!() };
    assert_eq!(t.lines[0].text.len(), MAX_TEXT_BYTES);
    assert!(t.language.is_none()); // above the highlighting limit
    assert_eq!(mem.read(), MAX_TEXT_BYTES as u64 + 1); // sniff + rest, read once
}

#[test]
fn markdown_is_rendered() {
    let mem = Arc::new(MemFs::new());
    mem.add("README.md", "# Title\n\nSome *markdown*.\n");
    let (id, loaded) = load(&mem.vpath("README.md"));
    assert_eq!((id.as_str(), loaded.info.as_str()), ("markdown", "26 B · markdown"));
    let PreviewData::Markdown(md) = loaded.data else { panic!() };
    assert_eq!(md.blocks.len(), 2);
    assert_eq!(md.source, "# Title\n\nSome *markdown*.\n");
}

#[test]
fn hex_dump_and_empty_file() {
    let mem = Arc::new(MemFs::new());
    mem.add_dir("d");
    mem.add("d/x.bin", (0..=255u8).cycle().take(5000).collect::<Vec<_>>());
    mem.add("e.bin", "");
    let (id, loaded) = load(&mem.vpath("d/x.bin"));
    assert_eq!((id.as_str(), loaded.info.as_str()), ("hex", "4.9 K · binary"));
    let PreviewData::Hex(h) = &loaded.data else { panic!() };
    assert_eq!(h.bytes.len(), 4096);
    let text = h.text();
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("Name:        x.bin"));
    assert_eq!(lines.next(), Some("Location:    mem:/d"));
    assert_eq!(lines.next(), Some("Size:        4.9 K (5,000 bytes)"));
    assert!(text.contains("\n\n00000000  00 01 02 03"));
    assert_eq!(text.lines().count(), 4 + 1 + 256); // facts, blank line, 4096 / 16 dump lines
    let (_, loaded) = load(&mem.vpath("e.bin"));
    assert_eq!(loaded.info, "0 B");
    assert_eq!(loaded.data.plain_text().as_deref(), Some("(empty file)"));
}

#[test]
fn folder_listing() {
    let mem = Arc::new(MemFs::new());
    mem.add_dir("f");
    mem.add_dir("f/sub");
    mem.add("f/a.txt", "abc");
    mem.add("f/.dot", "abc");
    let (id, loaded) = load(&mem.vpath("f"));
    assert_eq!((id.as_str(), loaded.info.as_str()), ("listing", "folder · test server"));
    let PreviewData::Listing(l) = &loaded.data else { panic!() };
    assert_eq!((l.dirs, l.files, l.total_size), (1, 1, 3));
    assert_eq!(l.summary(), "1 directory, 1 file (3 B)");
}

#[test]
fn archive_listing_needs_a_backend() {
    let mem = Arc::new(MemFs::new());
    mem.add("a.zip", "PK");
    // Without an archive backend: the hex fallback.
    let o = Registry::with_builtins().preview(&mem.vpath("a.zip"), &LoadContext::default(), None);
    assert_eq!(o.previewer_id(), Some("hex"));
    let cache = tempfile::tempdir().unwrap();
    let ctx = LoadContext {
        cache_dir: Some(cache.path().to_path_buf()),
        open_archive: Some(Arc::new(|t: &oc_vfs::VPath| {
            if t.path.ends_with("bad.zip") {
                return Err(oc_vfs::VfsError::Operation("Not a valid ZIP archive".into()));
            }
            let fs = Arc::new(MemFs::new());
            fs.add("inside.txt", "hi");
            Ok(fs.vpath(""))
        })),
        ..LoadContext::default()
    };
    let o = Registry::with_builtins().preview(&mem.vpath("a.zip"), &ctx, None);
    assert_eq!(o.previewer_id(), Some("listing"));
    let loaded = o.loaded.unwrap();
    assert_eq!(loaded.info, "test server · 2 B");
    assert!(loaded.data.plain_text().unwrap().starts_with("0 directories, 1 file (2 B)\n\ninside.txt"));
    mem.add("bad.zip", "PK");
    let o = Registry::with_builtins().preview(&mem.vpath("bad.zip"), &ctx, None);
    assert_eq!(o.error.map(|e| e.to_string()).as_deref(), Some("Not a valid ZIP archive"));
}

// -- images ----------------------------------------------------------------------------------

#[test]
fn png_is_decoded() {
    let mem = Arc::new(MemFs::new());
    mem.add("p.png", png_bytes(8, 4));
    let (id, loaded) = load(&mem.vpath("p.png"));
    assert_eq!(id, "image");
    assert!(loaded.info.starts_with("8×4 · PNG · "), "{}", loaded.info);
    let PreviewData::Image(img) = loaded.data else { panic!() };
    assert_eq!((img.width, img.height, img.frames.len()), (8, 4, 1));
    assert_eq!(&img.frames[0].rgba[..4], &[0, 128, 0, 255]);
    assert!(!img.is_animated());
}

#[test]
fn animated_gif_has_frames_and_delays() {
    let img = crate::builtin::decode_image(&gif_bytes(3)).unwrap();
    assert_eq!((img.format, img.width, img.height), ("GIF", 6, 3));
    assert_eq!(img.frames.len(), 3);
    assert!(img.frames.iter().all(|f| f.delay == Duration::from_millis(70) && f.rgba.len() == 6 * 3 * 4));
    let still = crate::builtin::decode_image(&gif_bytes(1)).unwrap();
    assert!(!still.is_animated() && still.frames[0].delay.is_zero());
    let mem = Arc::new(MemFs::new());
    mem.add("a.gif", gif_bytes(2));
    assert!(load(&mem.vpath("a.gif")).1.info.ends_with(" · animated"));
}

#[test]
fn webp_and_content_sniffed_format() {
    let img = crate::builtin::decode_image(&webp_bytes(5, 7)).unwrap();
    assert_eq!((img.format, img.width, img.height), ("WEBP", 5, 7));
    // A PNG named .jpg decodes by its contents.
    let mem = Arc::new(MemFs::new());
    mem.add("photo.jpg", png_bytes(2, 2));
    assert!(load(&mem.vpath("photo.jpg")).1.info.starts_with("2×2 · PNG"));
}

#[test]
fn huge_images_are_scaled_down() {
    let img = crate::builtin::decode_image(&png_bytes(5000, 10)).unwrap();
    assert_eq!((img.width, img.height, img.source_width, img.source_height), (4096, 8, 5000, 10));
    assert_eq!(img.frames[0].rgba.len(), 4096 * 8 * 4);
}

#[test]
fn svg_is_rendered() {
    let img = crate::builtin::decode_svg(SVG).unwrap();
    assert_eq!((img.format, img.width, img.height), ("SVG", 20, 10));
    assert_eq!(&img.frames[0].rgba[..4], &[0, 0, 255, 255]);
    let big = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10000" height="5000"></svg>"#;
    let img = crate::builtin::decode_svg(big).unwrap();
    assert_eq!((img.width, img.height, img.source_width), (4096, 2048, 10000));
    assert!(crate::builtin::decode_svg(b"<svg").is_err());
    assert!(crate::builtin::decode_svg(br#"<svg xmlns="http://www.w3.org/2000/svg" width="0" height="0"/>"#).is_err());
    let text = br#"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><text y="15">Hi</text></svg>"#;
    assert_eq!(crate::builtin::decode_svg(text).unwrap().width, 40);
}

#[test]
fn qt_formats_cur_xbm_xpm_wbmp() {
    let mut ico = Vec::new();
    image::RgbaImage::from_pixel(4, 2, image::Rgba([0, 0, 255, 255]))
        .write_to(&mut std::io::Cursor::new(&mut ico), image::ImageFormat::Ico)
        .unwrap();
    ico[2] = 2; // a cursor
    let mem = Arc::new(MemFs::new());
    mem.add("arrow.cur", ico);
    mem.add("b.xbm", "#define b_width 2\n#define b_height 1\nstatic char b_bits[] = { 0x01 };\n");
    mem.add("p.xpm", "/* XPM */\nstatic char *p[] = {\n\"1 1 1 1\",\n\". c #00FF00\",\n\".\"};\n");
    mem.add("w.wbmp", vec![0u8, 0, 1, 1, 0x80]);
    for (name, info) in [("arrow.cur", "4×2 · CUR"), ("b.xbm", "2×1 · XBM"), ("p.xpm", "1×1 · XPM"), ("w.wbmp", "1×1 · WBMP")] {
        let (id, loaded) = load(&mem.vpath(name));
        assert_eq!(id, "image");
        assert!(loaded.info.starts_with(info), "{name}: {}", loaded.info);
    }
}

/// HEIC, ICNS and JPEG 2000 through ImageIO (1.x: Qt's macOS plugins).
#[cfg(target_os = "macos")]
#[test]
fn imageio_formats_on_macos() {
    let dir = tempfile::tempdir().unwrap();
    let png = dir.path().join("src.png");
    std::fs::write(&png, png_bytes(128, 128)).unwrap(); // ICNS has no 64×64 slot
    for (ext, format) in [("heic", "HEIC"), ("icns", "ICNS"), ("jp2", "JP2")] {
        let out = dir.path().join(format!("x.{ext}"));
        let ok = std::process::Command::new("sips")
            .args(["-s", "format", ext])
            .arg(&png)
            .arg("--out")
            .arg(&out)
            .output()
            .is_ok_and(|o| o.status.success());
        assert!(ok, "sips could not write {ext}");
        let (id, loaded) = load(&LocalFs::vpath(&out));
        assert_eq!(id, "image");
        assert!(loaded.info.starts_with(&format!("128×128 · {format} · ")), "{}", loaded.info);
        let PreviewData::Image(img) = loaded.data else { panic!() };
        assert_eq!((img.width, img.height), (128, 128));
        let px = &img.frames[0].rgba[..4];
        assert!(px[1] > 100 && px[0] < 40 && px[2] < 40 && px[3] == 255, "{ext}: {px:?}");
    }
}

#[test]
fn local_images_are_read_directly() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.png");
    std::fs::write(&p, png_bytes(3, 3)).unwrap();
    let src = PreviewSource::new(LocalFs::vpath(&p)).unwrap();
    assert_eq!(src.direct_path(), Some(p.as_path()));
    let (_, loaded) = load(&LocalFs::vpath(&p));
    assert!(loaded.info.starts_with("3×3 · PNG"));
}

// -- media -------------------------------------------------------------------------------------

#[test]
fn media_streams_from_other_file_systems() {
    let mem = Arc::new(MemFs::new());
    mem.add("song.mp3", garbage(1000, 9));
    mem.add("clip.mkv", garbage(1000, 10));
    let (id, loaded) = load(&mem.vpath("song.mp3"));
    assert_eq!((id.as_str(), loaded.info.as_str()), ("media", "1000 B · audio · streaming"));
    let PreviewData::Media(m) = loaded.data else { panic!() };
    assert!(!m.video && m.waveform_stream.is_some() && matches!(m.source, crate::DocumentSource::Stream(_)));
    let (_, loaded) = load(&mem.vpath("clip.mkv"));
    let PreviewData::Media(m) = loaded.data else { panic!() };
    assert!(m.video && m.waveform_stream.is_none());

    let cache = tempfile::tempdir().unwrap();
    let piped = Arc::new(MemFs { unseekable: true, ..MemFs::default() });
    piped.add("song.mp3", garbage(1000, 9));
    let ctx = LoadContext { cache_dir: Some(cache.path().to_path_buf()), ..LoadContext::default() };
    let o = Registry::with_builtins().preview(&piped.vpath("song.mp3"), &ctx, None);
    let loaded = o.loaded.unwrap();
    assert_eq!(loaded.info, "1000 B · audio"); // copied locally
    let PreviewData::Media(m) = loaded.data else { panic!() };
    assert!(matches!(m.source, crate::DocumentSource::Path(ref p) if p.starts_with(cache.path())));
}

#[cfg(feature = "media")]
#[test]
fn media_carries_info_and_sources() {
    let clip = include_bytes!("../../../oc-media/tests/data/clip-160x90.mp4");
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("clip.mp4");
    std::fs::write(&local, clip).unwrap();
    let (_, loaded) = load(&LocalFs::vpath(&local));
    let PreviewData::Media(m) = loaded.data else { panic!() };
    let info = m.info.clone().expect("probed");
    assert_eq!(info.title.as_deref(), Some("Test Clip"));
    assert_eq!(info.video.as_ref().map(|v| (v.width, v.height)), Some((160, 90)));
    let (player, waveform) = m.into_sources();
    assert!(matches!(player, oc_media::MediaSource::Path(ref p) if *p == local) && waveform.is_none());

    // Streamed from a server: probed, and the stream starts at the beginning again.
    let mem = Arc::new(MemFs::new());
    mem.add("clip.mp4", clip.to_vec());
    mem.add("song.mp3", garbage(1000, 9));
    let (_, loaded) = load(&mem.vpath("clip.mp4"));
    let PreviewData::Media(m) = loaded.data else { panic!() };
    assert!(m.info.as_ref().is_some_and(|i| i.has_video() && i.seekable));
    let (player, _) = m.into_sources();
    let oc_media::MediaSource::Stream { mut stream, size, name } = player else { panic!() };
    assert_eq!((size, name.as_str()), (Some(clip.len() as u64), "clip.mp4"));
    let mut head = [0u8; 8];
    std::io::Read::read_exact(&mut stream, &mut head).unwrap();
    assert_eq!(&head, &clip[..8]);
    // Not media after all: no info, still a player preview (which reports the error).
    let (_, loaded) = load(&mem.vpath("song.mp3"));
    let PreviewData::Media(m) = loaded.data else { panic!() };
    assert!(m.info.is_none());
    let (_, waveform) = m.into_sources();
    assert!(waveform.is_some()); // small audio on a server: its own stream
}
