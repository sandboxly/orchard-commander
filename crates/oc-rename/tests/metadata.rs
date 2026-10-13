//! File metadata for `[=field]` rename placeholders: audio tags, video
//! properties, photo EXIF. Port of `tests/test_metadata.py`.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lofty::config::WriteOptions;
use lofty::prelude::*;
use lofty::tag::{Tag, TagType};
use oc_rename::metadata::{
    self, Field, Metadata, MetadataProvider, MetadataUnavailable, ProviderError, Registry, Source,
};
use oc_rename::{Item, Renamer, Rule, item_for, plan, plan_with};
use oc_vfs::VPath;
use oc_vfs::testing::MemFS;

use common::{data, local};

/// The test MP3 with ID3 tags, in a fresh temp dir.
fn song(dir: &Path) -> PathBuf {
    let path = dir.join("track.mp3");
    std::fs::copy(data("tone-then-silence.mp3"), &path).unwrap();
    let mut tag = Tag::new(TagType::Id3v2);
    tag.set_artist("AC/DC".into());
    tag.set_album("Live: Volume 1".into());
    tag.set_title("Tone".into());
    tag.set_track(3);
    tag.set_track_total(12);
    tag.insert_text(ItemKey::RecordingDate, "1992-10-27".into());
    tag.set_genre("Rock".into());
    tag.save_to_path(&path, WriteOptions::default()).unwrap();
    path
}

/// A real JPEG with an EXIF block: make, model, date taken, ISO, focal length.
fn exif_jpeg(path: &Path) -> PathBuf {
    let mut jpeg = Vec::new();
    let pixels: Vec<u8> = std::iter::repeat_n([255u8, 165, 0], 64 * 48).flatten().collect();
    jpeg_encoder::Encoder::new(&mut jpeg, 90).encode(&pixels, 64, 48, jpeg_encoder::ColorType::Rgb).unwrap();

    /// entries: (tag, type, count, value bytes); returns (ifd, extra data placed after it).
    fn ifd(entries: &[(u16, u16, u32, Vec<u8>)], offset: u32) -> (Vec<u8>, Vec<u8>) {
        let mut head = (entries.len() as u16).to_le_bytes().to_vec();
        let mut extra = Vec::new();
        let extra_at = offset + 2 + 12 * entries.len() as u32 + 4;
        for (tag, kind, count, value) in entries {
            head.extend(tag.to_le_bytes());
            head.extend(kind.to_le_bytes());
            head.extend(count.to_le_bytes());
            if value.len() <= 4 {
                let mut v = value.clone();
                v.resize(4, 0);
                head.extend(v);
            } else {
                head.extend((extra_at + extra.len() as u32).to_le_bytes());
                extra.extend(value);
            }
        }
        head.extend(0u32.to_le_bytes());
        (head, extra)
    }
    let ascii = |s: &str| [s.as_bytes(), b"\0"].concat();
    let (make, model) = (ascii("Canon"), ascii("Canon EOS R6"));
    let ifd0_len = 2 + 12 * 3 + 4 + make.len() + model.len();
    let exif_at = (8 + ifd0_len) as u32;
    let (ifd0, extra0) = ifd(
        &[
            (0x010F, 2, make.len() as u32, make.clone()),
            (0x0110, 2, model.len() as u32, model.clone()),
            (0x8769, 4, 1, exif_at.to_le_bytes().to_vec()),
        ],
        8,
    );
    let taken = ascii("2026:03:07 09:05:04");
    let (exif, extra1) = ifd(
        &[
            (0x8827, 3, 1, 400u16.to_le_bytes().to_vec()),
            (0x9003, 2, taken.len() as u32, taken.clone()),
            (0x920A, 5, 1, [50u32.to_le_bytes(), 1u32.to_le_bytes()].concat()),
        ],
        exif_at,
    );
    let tiff = [b"II*\0".to_vec(), 8u32.to_le_bytes().to_vec(), ifd0, extra0, exif, extra1].concat();
    let app1 = [b"Exif\0\0".to_vec(), tiff].concat();
    let out = [&jpeg[..2], b"\xff\xe1", &((app1.len() + 2) as u16).to_be_bytes(), &app1, &jpeg[2..]].concat();
    std::fs::write(path, out).unwrap();
    path.to_path_buf()
}

fn read(path: &Path) -> Metadata {
    metadata::read(&local(path), false, false).unwrap()
}

#[test]
fn audio_tags() {
    let tmp = tempfile::tempdir().unwrap();
    let meta = read(&song(tmp.path()));
    assert_eq!(meta["artist"], "AC-DC"); // "/" can't be in a file name
    assert_eq!(meta["album"], "Live - Volume 1");
    assert_eq!(meta["title"], "Tone");
    assert_eq!(meta["genre"], "Rock");
    assert_eq!(meta["track"], "03");
    assert_eq!(meta["tracktotal"], "12");
    assert_eq!(meta["year"], "1992");
    assert_eq!(meta["duration"], "2s");
    assert_eq!(meta["bitrate"], "32");
    assert!(!meta.contains_key("composer")); // empty values are left out
}

#[test]
fn untagged_audio_has_properties_only() {
    let meta = read(&data("tone-then-silence.mp3"));
    assert_eq!(meta, Metadata::from([("duration".into(), "2s".into()), ("bitrate".into(), "32".into())]));
}

#[test]
fn video_properties() {
    let meta = read(&data("clip-160x90.mp4"));
    assert_eq!(meta["width"], "160");
    assert_eq!(meta["height"], "90");
    assert_eq!(meta["quality"], "90p");
    assert_eq!(meta["resolution"], "160x90");
    assert_eq!(meta["duration"], "2s");
    assert_eq!(meta["title"], "Test Clip"); // MP4 tags
    assert_eq!(meta["year"], "2024");
    assert!(!meta.contains_key("bitrate")); // audio properties don't apply to videos
}

#[test]
fn photo_exif() {
    let tmp = tempfile::tempdir().unwrap();
    let meta = read(&exif_jpeg(&tmp.path().join("IMG_0001.jpg")));
    assert_eq!(meta["taken"], "2026-03-07");
    assert_eq!(meta["taken_time"], "09.05.04");
    assert_eq!(meta["taken_year"], "2026");
    assert_eq!(meta["taken_month"], "03");
    assert_eq!(meta["camera"], "Canon EOS R6");
    assert_eq!(meta["make"], "Canon");
    assert_eq!(meta["model"], "Canon EOS R6");
    assert_eq!(meta["iso"], "400");
    assert_eq!(meta["focal"], "50");
    assert_eq!(meta["width"], "64"); // from the image itself
    assert_eq!(meta["height"], "48");
}

#[test]
fn unknown_and_unreadable() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("notes.txt"), "hi").unwrap();
    std::fs::write(tmp.path().join("broken.mp3"), "not really an mp3").unwrap();
    std::fs::write(tmp.path().join("broken.mkv"), "not really a video").unwrap();
    std::fs::write(tmp.path().join("broken.jpg"), "not really a photo").unwrap();
    for name in ["notes.txt", "broken.mp3", "broken.mkv", "broken.jpg"] {
        assert_eq!(read(&tmp.path().join(name)), Metadata::new(), "{name}");
    }
    let err = metadata::read(&local(&tmp.path().join("broken.mp3")), false, true).unwrap_err();
    assert_eq!(err, MetadataUnavailable("cloud-only file, not downloaded".into()));
    // Not handled at all, or a folder: nothing to read, even when cloud-only.
    assert_eq!(metadata::read(&local(&tmp.path().join("notes.txt")), false, true).unwrap(), Metadata::new());
    assert_eq!(metadata::read(&local(&tmp.path().join("x.mp3")), true, false).unwrap(), Metadata::new());
}

#[test]
fn files_elsewhere_are_streamed() {
    let mem = MemFS::new();
    mem.add_file("videos/clip.mp4", std::fs::read(data("clip-160x90.mp4")).unwrap());
    let meta = metadata::read(&mem.root().child(&["videos", "clip.mp4"]), false, false).unwrap();
    assert_eq!(meta["resolution"], "160x90");
    assert_eq!(meta["title"], "Test Clip");
    let missing = metadata::read(&MemFS::new().root().child(&["gone.mp4"]), false, false).unwrap_err();
    assert!(missing.0.contains("not found"), "{missing}");
}

// -- placeholders ---------------------------------------------------------------------------------

fn with_meta(path: &Path) -> Item {
    let location = local(path);
    let item = item_for(&location, &location.stat().unwrap());
    let meta = metadata::read(&location, false, false).unwrap();
    item.with_meta(meta)
}

fn named(mask: &str) -> Rule {
    Rule { name_mask: mask.into(), ..Rule::default() }
}

#[test]
fn rename_with_tags() {
    let tmp = tempfile::tempdir().unwrap();
    let item = with_meta(&song(tmp.path()));
    let rule = named("[=track] - [=artist] - [=title] ([=year])");
    assert_eq!(oc_rename::new_name(&item, 0, &rule).unwrap(), "03 - AC-DC - Tone (1992).mp3");
    assert_eq!(oc_rename::new_name(&item, 0, &named("[U][=artist:1-2]")).unwrap(), "AC.mp3");
    let clip = with_meta(&data("clip-160x90.mp4"));
    assert_eq!(oc_rename::new_name(&clip, 0, &named("[=title] [[][=quality][]]")).unwrap(), "Test Clip [90p].mp4");
}

#[test]
fn missing_values_skip_or_fall_back() {
    let tmp = tempfile::tempdir().unwrap();
    let tagged = song(tmp.path());
    std::fs::copy(data("tone-then-silence.mp3"), tmp.path().join("untagged.mp3")).unwrap();
    let items = [with_meta(&tagged), with_meta(&tmp.path().join("untagged.mp3"))];
    let p = plan(&items, &named("[=artist] - [N]"));
    assert_eq!(p.changes.iter().map(|c| c.new.as_str()).collect::<Vec<_>>(), ["AC-DC - track.mp3", "untagged.mp3"]);
    assert_eq!(p.changes[1].note, "skipped: no artist");
    assert!(p.can_run()); // skipped, not a blocker
    let fallback = plan(&items, &named("[=artist|Unknown] - [N]"));
    assert_eq!(fallback.changes[1].new, "Unknown - untagged.mp3");
}

#[test]
fn metadata_pending_and_bad_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let location = local(&song(tmp.path()));
    let unread = item_for(&location, &location.stat().unwrap()); // meta not read yet
    let p = plan(std::slice::from_ref(&unread), &named("[=artist]"));
    assert_eq!(p.pending(), 1);
    assert!(!p.can_run());
    let unknown = plan(std::slice::from_ref(&unread), &named("[=nonsense]"));
    assert!(unknown.mask_error.contains("Unknown field"), "{}", unknown.mask_error);
    let bad_range = plan(std::slice::from_ref(&unread), &named("[=artist:0-2]")); // bad range, found early
    assert_eq!(bad_range.mask_error, "Ranges start at 1 (at position 1)");
    assert!(oc_rename::needs_metadata(&named("x[=artist]")));
    assert!(oc_rename::needs_metadata(&Rule { ext_mask: "[=year]".into(), ..Rule::default() }));
    assert!(!oc_rename::needs_metadata(&Rule::default()));
}

/// An add-on: pages of a ".doc" made of form feeds.
struct Pages;

impl MetadataProvider for Pages {
    fn id(&self) -> &str {
        "pages"
    }
    fn label(&self) -> &str {
        "Documents"
    }
    fn fields(&self) -> &[Field] {
        const FIELDS: &[Field] = &[Field::new("pages", "Number of pages")];
        FIELDS
    }
    fn handles(&self, name: &str) -> bool {
        name.ends_with(".doc")
    }
    fn read(&self, src: &mut Source<'_>) -> Result<Metadata, ProviderError> {
        let mut text = String::new();
        src.file.read_to_string(&mut text)?;
        Ok(Metadata::from([("pages".into(), text.split('\x0c').count().to_string())]))
    }
}

#[test]
fn add_on_providers() {
    let registry = Arc::new(Registry::builtin().with(Arc::new(Pages)));
    assert_eq!(registry.providers()[0].id(), "pages"); // add-ons first
    assert!(registry.all_fields().iter().any(|f| f.name == "pages"));
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("x.doc"), "a\x0cb\x0cc").unwrap();
    let location = local(&tmp.path().join("x.doc"));
    let meta = registry.read(&location, false, false).unwrap();
    assert_eq!(meta, Metadata::from([("pages".into(), "3".into())]));
    let item = item_for(&location, &location.stat().unwrap()).with_meta(meta);
    let renamer = Renamer::with_registry(&named("[N] ([=pages]p)"), registry.clone());
    assert_eq!(renamer.new_name(&item, 0).unwrap(), "x (3p).doc");
    // Without the add-on the field is unknown.
    let p = plan_with(&Renamer::with_registry(&named("[=pages]"), Arc::new(Registry::builtin())), &[item], &|l: &VPath| {
        l.exists()
    });
    assert!(p.mask_error.starts_with("Unknown field “pages”"));
    // Registering replaces a provider with the same id.
    let again = Registry::builtin().with(Arc::new(Pages)).with(Arc::new(Pages));
    assert_eq!(again.providers().len(), 4);
}

#[test]
fn global_registration() {
    struct Shout;
    impl MetadataProvider for Shout {
        fn id(&self) -> &str {
            "shout"
        }
        fn label(&self) -> &str {
            "Shouting"
        }
        fn fields(&self) -> &[Field] {
            const FIELDS: &[Field] = &[Field::new("shout_only_in_this_test", "Loud")];
            FIELDS
        }
        fn handles(&self, name: &str) -> bool {
            name.ends_with(".shout")
        }
        fn read(&self, _src: &mut Source<'_>) -> Result<Metadata, ProviderError> {
            panic!("a broken add-on must not break renaming")
        }
    }
    metadata::register(Arc::new(Shout));
    assert!(metadata::handled("a.shout"));
    assert!(metadata::all_fields().iter().any(|f| f.name == "shout_only_in_this_test"));
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.shout"), "x").unwrap();
    assert_eq!(read(&tmp.path().join("a.shout")), Metadata::new()); // the panic is contained
}
