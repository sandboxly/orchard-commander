//! Port of tests/test_archives.py plus edge cases.

mod common;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use common::*;
use oc_archive::{
    Archive, ArchiveFS, extract_cached, is_archive, open_archive, open_archive_file,
};
use oc_vfs::{ListOptions, VfsError};

fn names(entries: &[oc_vfs::Entry]) -> Vec<&str> {
    entries.iter().map(|e| e.name.as_str()).collect()
}

const SHOW_HIDDEN: ListOptions = ListOptions {
    show_hidden: true,
    sort_key: oc_vfs::SortKey::Name,
    reverse: false,
};

fn browse_and_extract(kind: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let content = content(tmp.path());
    let path = make(kind, &content, tmp.path());
    let archive = open_archive_file(&path).unwrap();
    let root = archive.list_dir("", ListOptions::default()).unwrap();
    assert_eq!(
        names(&root),
        ["docs", "top.txt"],
        "{kind}: dirs first, hidden filtered"
    );
    assert!(names(&archive.list_dir("", SHOW_HIDDEN).unwrap()).contains(&".hidden"));
    let docs = archive.list_dir("docs", ListOptions::default()).unwrap();
    let docs: Vec<_> = docs.iter().map(|e| (e.name.as_str(), e.is_dir)).collect();
    assert_eq!(docs, [("deep", true), ("readme.txt", false)]);
    assert_eq!(
        archive
            .list_dir("docs/deep", ListOptions::default())
            .unwrap()[0]
            .size,
        5000
    );

    let out = tmp.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let root = open_archive(&local(&path)).unwrap();
    let mut files = 0;
    for name in ["docs", "top.txt"] {
        let src = root.child(&[name]);
        let dst = out.join(name);
        files += extract_tree(&src, &dst).0;
    }
    assert_eq!(
        std::fs::read_to_string(out.join("docs/deep/x.txt")).unwrap(),
        "x".repeat(5000)
    );
    assert_eq!(
        std::fs::read_to_string(out.join("docs/readme.txt")).unwrap(),
        "read me"
    );
    assert_eq!(std::fs::read_to_string(out.join("top.txt")).unwrap(), "top");
    assert_eq!(files, 3);
}

#[test]
fn browse_and_extract_zip() {
    browse_and_extract("zip");
}

#[test]
fn browse_and_extract_tar() {
    browse_and_extract("tar");
}

#[test]
fn browse_and_extract_tar_gz() {
    browse_and_extract("tar.gz");
}

#[test]
fn browse_and_extract_tar_bz2() {
    browse_and_extract("tar.bz2");
}

#[test]
fn browse_and_extract_tar_xz() {
    browse_and_extract("tar.xz");
}

#[test]
fn browse_and_extract_7z() {
    browse_and_extract("7z");
}

#[test]
fn solid_7z_reads_every_member() {
    let tmp = tempfile::tempdir().unwrap();
    let content = content(tmp.path());
    let archive = open_archive_file(&make("7z-solid", &content, tmp.path())).unwrap();
    assert_eq!(
        read_all(&mut *archive.open_member("top.txt").unwrap()),
        b"top"
    );
    assert_eq!(
        read_all(&mut *archive.open_member("docs/deep/x.txt").unwrap()),
        "x".repeat(5000).as_bytes()
    );
    assert_eq!(
        read_all(&mut *archive.open_member(".hidden").unwrap()),
        b"h"
    );
}

#[test]
fn extract_single_dir_under_new_name() {
    let tmp = tempfile::tempdir().unwrap();
    let content = content(tmp.path());
    let root = open_archive(&local(&make("zip", &content, tmp.path()))).unwrap();
    extract_tree(&root.child(&["docs"]), &tmp.path().join("renamed"));
    assert!(tmp.path().join("renamed/deep/x.txt").exists());
}

#[test]
fn extract_file_replaces_existing_target() {
    let tmp = tempfile::tempdir().unwrap();
    let content = content(tmp.path());
    let archive = open_archive_file(&make("zip", &content, tmp.path())).unwrap();
    let out = tmp.path().join("top.txt");
    std::fs::write(&out, "mine").unwrap();
    archive.extract_file("top.txt", &out, None).unwrap();
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "top");
    // The member's mtime is carried over (zip: 2-second resolution).
    let mtime = std::fs::metadata(&out).unwrap().modified().unwrap();
    let member = archive.member("top.txt").unwrap().mtime;
    let got = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    assert!((got - member).abs() < 1.0, "{got} vs {member}");
}

#[test]
fn extract_cancel_leaves_no_partial_file() {
    for kind in ["zip", "tar.gz", "7z"] {
        let tmp = tempfile::tempdir().unwrap();
        let content = content(tmp.path());
        let archive = open_archive_file(&make(kind, &content, tmp.path())).unwrap();
        let out = tmp.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let mut cancel = |_n: u64| Err(VfsError::Cancelled);
        let err = archive
            .extract_file("docs/deep/x.txt", &out.join("x.txt"), Some(&mut cancel))
            .unwrap_err();
        assert!(matches!(err, VfsError::Cancelled), "{kind}: {err:?}");
        assert_eq!(
            std::fs::read_dir(&out).unwrap().count(),
            0,
            "{kind}: leftovers"
        );
    }
}

#[test]
fn extract_reports_progress_chunks() {
    let tmp = tempfile::tempdir().unwrap();
    let content = content(tmp.path());
    for kind in ["zip", "tar", "7z"] {
        let archive = open_archive_file(&make(kind, &content, tmp.path())).unwrap();
        let mut total = 0;
        let mut count = |n: u64| {
            total += n;
            Ok(())
        };
        archive
            .extract_file(
                "docs/deep/x.txt",
                &tmp.path().join(format!("x-{kind}")),
                Some(&mut count),
            )
            .unwrap();
        assert_eq!(total, 5000, "{kind}");
    }
}

fn single_compressed(suffix: &str, write: impl FnOnce(File, &[u8])) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join(format!("notes.txt{suffix}"));
    write(File::create(&path).unwrap(), b"hello world");
    let archive = open_archive_file(&path).unwrap();
    let entries = archive.list_dir("", ListOptions::default()).unwrap();
    assert_eq!(names(&entries), ["notes.txt"]);
    let target = extract_cached(&archive, "notes.txt").unwrap();
    assert_eq!(target.file_name().unwrap(), "notes.txt");
    assert_eq!(std::fs::read(&target).unwrap(), b"hello world");
    // Cached: the second call returns the same file without extracting again.
    assert_eq!(extract_cached(&archive, "notes.txt").unwrap(), target);
}

#[test]
fn single_compressed_gz() {
    single_compressed(".gz", |f, data| {
        let mut w = flate2::write::GzEncoder::new(f, flate2::Compression::default());
        w.write_all(data).unwrap();
        w.finish().unwrap();
    });
    // gzip knows its size; the listing shows it.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("n.gz");
    let mut w =
        flate2::write::GzEncoder::new(File::create(&path).unwrap(), flate2::Compression::default());
    w.write_all(&[7u8; 1234]).unwrap();
    w.finish().unwrap();
    assert_eq!(
        Archive::open(&path).unwrap().member("n").unwrap().size,
        1234
    );
}

#[test]
fn single_compressed_bz2() {
    single_compressed(".bz2", |f, data| {
        let mut w = bzip2::write::BzEncoder::new(f, bzip2::Compression::default());
        w.write_all(data).unwrap();
        w.finish().unwrap();
    });
}

#[test]
fn single_compressed_xz() {
    single_compressed(".xz", |f, data| {
        let mut w = liblzma::write::XzEncoder::new(f, 6);
        w.write_all(data).unwrap();
        w.finish().unwrap();
    });
}

#[test]
fn concatenated_gzip_members_read_as_one() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("multi.log.gz");
    let mut f = File::create(&path).unwrap();
    for part in [b"one\n".as_slice(), b"two\n"] {
        let mut w = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        w.write_all(part).unwrap();
        f.write_all(&w.finish().unwrap()).unwrap();
    }
    drop(f);
    let archive = Archive::open(&path).unwrap();
    assert_eq!(
        read_all(&mut *archive.open_member("multi.log").unwrap()),
        b"one\ntwo\n"
    );
}

#[test]
fn zip_slip_names_are_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("evil.zip");
    let mut z = zip::ZipWriter::new(File::create(&path).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    z.start_file("../escape.txt", opts).unwrap();
    z.write_all(b"x").unwrap();
    z.start_file("ok/./fine.txt", opts).unwrap();
    z.write_all(b"y").unwrap();
    z.finish().unwrap();
    let archive = Archive::open(&path).unwrap();
    let mut members: Vec<_> = archive.members().map(|m| m.name.as_str()).collect();
    members.sort();
    assert_eq!(members, ["ok", "ok/fine.txt"]);
}

#[test]
fn corrupt_archives_raise_archive_error() {
    let tmp = tempfile::tempdir().unwrap();
    for name in [
        "bad.zip",
        "bad.tar",
        "bad.tar.gz",
        "bad.7z",
        #[cfg(feature = "rar")]
        "bad.rar",
        "bad.gz",
        "bad.bz2",
        "bad.xz",
    ] {
        let bad = tmp.path().join(name);
        std::fs::write(
            &bad,
            b"not an archive, honestly, not at all -- just some text that is long enough",
        )
        .unwrap();
        let err = Archive::open(&bad).expect_err(name);
        assert!(
            err.0.starts_with(&format!("Cannot read {name}: ")),
            "{name}: {}",
            err.0
        );
    }
    let err = open_archive_file(&tmp.path().join("x.txt")).unwrap_err();
    assert_eq!(err.0, "Not a supported archive: x.txt");
    assert!(open_archive_file(&tmp.path().join("missing.zip")).is_err());
}

#[cfg(feature = "rar")]
#[test]
fn rar_listing() {
    let archive = Archive::open(&data("rar3-solid.rar")).unwrap();
    assert_eq!(
        names(&archive.list_dir("", ListOptions::default()).unwrap()),
        ["stest1.txt", "stest2.txt"]
    );
    assert_eq!(archive.kind(), oc_archive::ArchiveKind::Rar);
    assert_eq!(archive.member("stest1.txt").unwrap().size, 2048);
}

#[cfg(feature = "rar")]
#[test]
fn rar_extract() {
    for name in ["rar5-solid.rar", "seektest.rar", "rar3-solid.rar"] {
        let tmp = tempfile::tempdir().unwrap();
        let root = open_archive(&local(&data(name))).unwrap();
        for member in ["stest1.txt", "stest2.txt"] {
            extract_tree(&root.child(&[member]), &tmp.path().join(member));
        }
        let data = std::fs::read(tmp.path().join("stest1.txt")).unwrap();
        assert_eq!(data.len(), 2048, "{name}");
        assert!(data.starts_with(b"000\n001\n"), "{name}");
        assert_eq!(
            std::fs::read(tmp.path().join("stest2.txt")).unwrap().len(),
            2048,
            "{name}"
        );
    }
}

#[cfg(feature = "rar")]
#[test]
fn rar_streams_are_seekable() {
    let archive = Archive::open(&data("rar5-solid.rar")).unwrap();
    let mut r = archive.open_member("stest2.txt").unwrap();
    assert!(r.seekable());
    r.seek(SeekFrom::Start(4)).unwrap();
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"001\n");
}

#[cfg(not(feature = "rar"))]
#[test]
fn without_the_rar_feature_rar_files_are_plain_files() {
    assert!(!oc_archive::is_archive_name("x.rar"));
    assert!(open_archive_file(&data("rar5-solid.rar")).is_err());
}

#[test]
fn register_plugs_archives_into_oc_vfs() {
    let tmp = tempfile::tempdir().unwrap();
    let path = make("zip", &content(tmp.path()), tmp.path());
    oc_archive::register();
    assert!(oc_vfs::is_archive_name("x.zip"));
    let root = oc_vfs::open_archive(&local(&path)).unwrap().unwrap();
    assert!(root.child(&["top.txt"]).exists());
    assert!(oc_vfs::open_archive(&local(&tmp.path().join("plain.txt"))).is_none());
    // Members materialize through the archive's own extraction cache.
    let cache = tempfile::tempdir().unwrap();
    let real = oc_vfs::ops::materialize(&root.child(&["top.txt"]), cache.path()).unwrap();
    assert_eq!(std::fs::read(&real).unwrap(), b"top");
    assert!(!real.starts_with(cache.path()));
}

// -- ArchiveFS ---------------------------------------------------------------

fn zip_fs() -> (tempfile::TempDir, oc_vfs::VPath) {
    let tmp = tempfile::tempdir().unwrap();
    let content = content(tmp.path());
    let root = open_archive(&local(&make("zip", &content, tmp.path()))).unwrap();
    (tmp, root)
}

#[test]
fn archive_fs_basics() {
    let (tmp, root) = zip_fs();
    let fs = &root.fs;
    assert_eq!(fs.type_name(), "zip archive");
    assert_eq!(fs.read_only_reason(), "a.zip is an archive (read-only)");
    assert_eq!(fs.capabilities(), oc_vfs::Capabilities::default());
    assert!(
        matches!(fs.mkdir("x", true), Err(VfsError::ReadOnly(m)) if m == "a.zip is an archive (read-only)")
    );
    assert!(fs.open_write("x").is_err());

    // The root is named after the archive; ".." leads to the archive's folder.
    assert_eq!(root.name(), "a.zip");
    let stat = root.stat().unwrap();
    assert!(stat.is_dir && stat.name == "a.zip" && !stat.is_hidden);
    assert_eq!(root.parent().unwrap(), local(tmp.path()));
    assert_eq!(fs.container().unwrap(), local(&tmp.path().join("a.zip")));

    let x = root.child(&["docs", "deep", "x.txt"]);
    assert_eq!(x.path, "docs/deep/x.txt");
    assert_eq!(x.name(), "x.txt");
    assert_eq!(x.parent().unwrap().path, "docs/deep");
    let sep = std::path::MAIN_SEPARATOR;
    assert_eq!(
        x.display(),
        format!(
            "{}{sep}docs{sep}deep{sep}x.txt",
            tmp.path().join("a.zip").display()
        )
    );
    assert_eq!(
        root.display(),
        tmp.path().join("a.zip").display().to_string()
    );
    assert_eq!(fs.join("", &["a", "", "b"]), "a/b");
    assert_eq!(fs.parent_path("top.txt").as_deref(), Some(""));
    assert_eq!(fs.parent_path(""), None);

    assert!(x.exists() && !x.is_dir() && root.child(&["docs"]).is_dir() && root.is_dir());
    assert!(!root.child(&["nope"]).exists());
    assert!(root.child(&["nope"]).stat().unwrap_err().is_not_found());
    assert_eq!(x.stat().unwrap().size, 5000);
    assert!(fs.local_path("top.txt").is_none());
    assert_eq!(
        read_all(&mut *x.open_read().unwrap()),
        "x".repeat(5000).as_bytes()
    );
    assert!(fs.open_read("nope").err().unwrap().is_not_found());
    assert!(fs.open_read("docs").is_err());

    let err = fs.list_dir("top.txt", ListOptions::default()).unwrap_err();
    assert!(
        matches!(&err, VfsError::Io(e) if e.kind() == std::io::ErrorKind::NotADirectory),
        "{err:?}"
    );
    assert_eq!(err.to_string(), "Not a directory in a.zip: top.txt");
}

#[test]
fn archive_fs_walk() {
    let (_tmp, root) = zip_fs();
    let items: Vec<_> = root.fs.walk("docs").unwrap().map(Result::unwrap).collect();
    assert_eq!(items[0].0, Vec::<String>::new());
    assert_eq!(items[0].1.name, "docs");
    let mut rels: Vec<String> = items[1..].iter().map(|(r, _)| r.join("/")).collect();
    rels.sort();
    assert_eq!(rels, ["deep", "deep/x.txt", "readme.txt"]);
    // Directories come before their contents.
    let pos = |n: &str| items.iter().position(|(r, _)| r.join("/") == n).unwrap();
    assert!(pos("deep") < pos("deep/x.txt"));

    let all: Vec<_> = root.fs.walk("").unwrap().map(Result::unwrap).collect();
    assert_eq!(all[0].1.name, "a.zip");
    assert_eq!(all.len(), 1 + 6); // root + docs, deep, x.txt, readme.txt, top.txt, .hidden

    let file: Vec<_> = root
        .fs
        .walk("top.txt")
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(file.len(), 1);
    assert!(root.fs.walk("missing").is_err());
}

#[test]
fn two_opens_of_one_archive_are_the_same_fs() {
    let (tmp, root) = zip_fs();
    let again = open_archive(&local(&tmp.path().join("a.zip"))).unwrap();
    assert!(!Arc::ptr_eq(&root.fs, &again.fs));
    assert_eq!(root, again);
    assert_eq!(root.child(&["top.txt"]), again.child(&["top.txt"]));
    let hash = |v: &oc_vfs::VPath| {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        v.hash(&mut h);
        h.finish()
    };
    assert_eq!(hash(&root), hash(&again));

    // A different archive is a different file system.
    let other = tmp.path().join("b.zip");
    std::fs::copy(tmp.path().join("a.zip"), &other).unwrap();
    assert_ne!(root, open_archive(&local(&other)).unwrap());
}

#[test]
fn archives_nest_and_can_live_on_remote_file_systems() {
    let tmp = tempfile::tempdir().unwrap();
    let content = content(tmp.path());
    let inner = make("zip", &content, tmp.path());
    let outer = tmp.path().join("outer.zip");
    let mut z = zip::ZipWriter::new(File::create(&outer).unwrap());
    z.start_file("nested/inner.zip", zip::write::SimpleFileOptions::default())
        .unwrap();
    z.write_all(&std::fs::read(&inner).unwrap()).unwrap();
    z.finish().unwrap();

    // The outer archive sits on a file system without local paths.
    let outer_root = open_archive(&remote(&outer)).unwrap();
    let inner_file = outer_root.child(&["nested", "inner.zip"]);
    assert!(is_archive(&inner_file));
    assert!(!is_archive(&outer_root.child(&["nested"])));
    let inner_root = open_archive(&inner_file).unwrap();
    assert_eq!(inner_root.name(), "inner.zip");
    assert_eq!(
        read_all(&mut *inner_root.child(&["top.txt"]).open_read().unwrap()),
        b"top"
    );
    // ".." from the inner archive's root leads into the outer archive.
    assert_eq!(inner_root.parent().unwrap(), outer_root.child(&["nested"]));
    let sep = std::path::MAIN_SEPARATOR;
    assert!(
        inner_root
            .child(&["top.txt"])
            .display()
            .ends_with(&format!("outer.zip{sep}nested{sep}inner.zip{sep}top.txt"))
    );
}

#[test]
fn archive_fs_materialize_uses_the_extract_cache() {
    let (_tmp, root) = zip_fs();
    let path = {
        let fs = open_archive(&root.fs.container().unwrap()).unwrap();
        // Rebuild an ArchiveFS by hand to reach the inherent method.
        let archive = open_archive_file(&fs.fs.container().unwrap().local_path().unwrap()).unwrap();
        ArchiveFS::new(archive, root.fs.container().unwrap())
            .materialize("docs/readme.txt")
            .unwrap()
    };
    assert_eq!(path.file_name().unwrap(), "readme.txt");
    assert_eq!(std::fs::read_to_string(path).unwrap(), "read me");
}

#[test]
fn open_cache_reuses_until_the_file_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let content = content(tmp.path());
    let path = make("zip", &content, tmp.path());
    let a = open_archive_file(&path).unwrap();
    let b = open_archive_file(&path).unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    // Rewrite with different contents (and size): a fresh archive is opened.
    let mut z = zip::ZipWriter::new(File::create(&path).unwrap());
    z.start_file("only.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    z.write_all(b"new").unwrap();
    z.finish().unwrap();
    let c = open_archive_file(&path).unwrap();
    assert!(!Arc::ptr_eq(&a, &c));
    assert_eq!(
        names(&c.list_dir("", ListOptions::default()).unwrap()),
        ["only.txt"]
    );
}

#[test]
fn closed_archives_reopen_and_open_streams_keep_working() {
    let tmp = tempfile::tempdir().unwrap();
    let content = content(tmp.path());
    for kind in ["zip", "tar", "tar.gz", "7z"] {
        let archive = Archive::open(&make(kind, &content, tmp.path())).unwrap();
        let mut r = archive.open_member("docs/deep/x.txt").unwrap();
        archive.close();
        assert_eq!(read_all(&mut *r).len(), 5000, "{kind}");
        assert_eq!(
            read_all(&mut *archive.open_member("top.txt").unwrap()),
            b"top",
            "{kind}"
        );
    }
}

#[test]
fn seeking_inside_members() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("s.zip");
    let data: Vec<u8> = (0..100_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let mut z = zip::ZipWriter::new(File::create(&path).unwrap());
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let deflated = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let bzip =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Bzip2);
    z.start_file("stored.bin", stored).unwrap();
    z.write_all(&data).unwrap();
    z.start_file("deflated.bin", deflated).unwrap();
    z.write_all(&data).unwrap();
    z.start_file("bzip2.bin", bzip).unwrap();
    z.write_all(&data).unwrap();
    z.finish().unwrap();
    let archive = Archive::open(&path).unwrap();
    for (name, seekable) in [
        ("stored.bin", true),
        ("deflated.bin", false),
        ("bzip2.bin", true),
    ] {
        let mut r = archive.open_member(name).unwrap();
        assert_eq!(r.seekable(), seekable, "{name}");
        let mut buf = [0u8; 16];
        r.seek(SeekFrom::Start(50_000)).unwrap();
        r.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &data[50_000..50_016], "{name}");
        r.seek(SeekFrom::Start(3)).unwrap();
        r.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &data[3..19], "{name}");
        assert_eq!(r.seek(SeekFrom::End(-16)).unwrap(), 100_000 - 16, "{name}");
        r.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &data[100_000 - 16..], "{name}");
        r.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(read_all(&mut *r), data, "{name}");
    }

    // Plain tar members are byte ranges: seekable. Compressed: emulated.
    let content = content(tmp.path());
    for (kind, seekable) in [("tar", true), ("tar.gz", false)] {
        let archive = Archive::open(&make(kind, &content, tmp.path())).unwrap();
        let mut r = archive.open_member("docs/readme.txt").unwrap();
        assert_eq!(r.seekable(), seekable, "{kind}");
        r.seek(SeekFrom::Start(5)).unwrap();
        assert_eq!(read_all(&mut *r), b"me", "{kind}");
    }
}

#[test]
fn corrupt_deflate_data_fails_the_crc_check() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("c.zip");
    let mut z = zip::ZipWriter::new(File::create(&path).unwrap());
    let stored = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    z.start_file("f.txt", stored).unwrap();
    z.write_all(&b"hello hello hello hello".repeat(100))
        .unwrap();
    z.finish().unwrap();
    // Flip a bit in the stored CRC of the central directory entry.
    let mut bytes = std::fs::read(&path).unwrap();
    let cd = bytes
        .windows(4)
        .rposition(|w| w == [0x50, 0x4b, 0x01, 0x02])
        .unwrap();
    bytes[cd + 16] ^= 0xFF;
    std::fs::write(&path, bytes).unwrap();
    let archive = Archive::open(&path).unwrap();
    let mut out = Vec::new();
    let err = archive
        .open_member("f.txt")
        .unwrap()
        .read_to_end(&mut out)
        .unwrap_err();
    assert_eq!(err.to_string(), "Bad CRC-32 for file 'f.txt'");
}

#[test]
fn explicit_directory_entries_and_implicit_parents() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("d.zip");
    let mut z = zip::ZipWriter::new(File::create(&path).unwrap());
    let opts = zip::write::SimpleFileOptions::default()
        .last_modified_time(zip::DateTime::from_date_and_time(2001, 2, 3, 4, 5, 6).unwrap());
    z.start_file("a/b/c.txt", opts).unwrap();
    z.write_all(b"c").unwrap();
    z.add_directory("a/", opts).unwrap();
    z.start_file("a/b/c.txt", opts).ok(); // duplicate names are refused by the writer
    z.start_file("empty.txt", opts).unwrap();
    z.finish().unwrap();
    let archive = Archive::open(&path).unwrap();
    let a = archive.member("a").unwrap();
    assert!(a.is_dir);
    // The explicit entry replaced the implicit one and brought its mtime.
    assert!((a.mtime - archive.member("a/b/c.txt").unwrap().mtime).abs() < 1e-9);
    let b = archive.member("a/b").unwrap();
    assert!(b.is_dir && b.mtime == archive.mtime());
    assert_eq!(
        names(&archive.list_dir("a", ListOptions::default()).unwrap()),
        ["b"]
    );
    assert_eq!(
        read_all(&mut *archive.open_member("empty.txt").unwrap()),
        b""
    );
}

#[test]
fn tar_links_read_their_targets() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("links.tar");
    let mut t = tar::Builder::new(File::create(&path).unwrap());
    let mut h = tar::Header::new_gnu();
    h.set_size(5);
    h.set_mode(0o644);
    h.set_entry_type(tar::EntryType::Regular);
    t.append_data(&mut h, "dir/real.txt", b"hello".as_slice())
        .unwrap();
    for (name, target, kind) in [
        ("dir/sym.txt", "real.txt", tar::EntryType::Symlink),
        (
            "top-sym.txt",
            "dir/../dir/real.txt",
            tar::EntryType::Symlink,
        ),
        ("hard.txt", "dir/real.txt", tar::EntryType::Link),
        ("dangling", "nowhere", tar::EntryType::Symlink),
    ] {
        let mut h = tar::Header::new_gnu();
        h.set_size(0);
        h.set_entry_type(kind);
        t.append_link(&mut h, name, target).unwrap();
    }
    t.into_inner().unwrap();
    let archive = Archive::open(&path).unwrap();
    let sym = archive.member("dir/sym.txt").unwrap();
    assert!(sym.is_link && sym.size == 0);
    assert!(!archive.member("hard.txt").unwrap().is_link);
    for name in ["dir/sym.txt", "top-sym.txt", "hard.txt"] {
        assert_eq!(
            read_all(&mut *archive.open_member(name).unwrap()),
            b"hello",
            "{name}"
        );
    }
    assert!(
        archive
            .open_member("dangling")
            .err()
            .unwrap()
            .is_not_found()
    );
    let fs = open_archive(&local(&path)).unwrap();
    assert!(fs.child(&["dir", "sym.txt"]).stat().unwrap().is_link);
}

#[test]
fn concurrent_reads() {
    let tmp = tempfile::tempdir().unwrap();
    let content = content(tmp.path());
    for kind in ["zip", "tar.gz", "7z"] {
        let archive = Archive::open(&make(kind, &content, tmp.path())).unwrap();
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..10 {
                        assert_eq!(
                            read_all(&mut *archive.open_member("docs/deep/x.txt").unwrap()).len(),
                            5000
                        );
                        assert_eq!(
                            read_all(&mut *archive.open_member("top.txt").unwrap()),
                            b"top"
                        );
                    }
                });
            }
        });
    }
}

#[test]
fn seven_zip_directories_and_empty_files() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("d.7z");
    let mut z = sevenz_rust2::ArchiveWriter::create(&path).unwrap();
    z.push_archive_entry::<&[u8]>(sevenz_rust2::ArchiveEntry::new_directory("emptydir"), None)
        .unwrap();
    z.push_archive_entry(
        sevenz_rust2::ArchiveEntry::new_file("sub/empty.txt"),
        Some(b"".as_slice()),
    )
    .unwrap();
    z.push_archive_entry(
        sevenz_rust2::ArchiveEntry::new_file("sub/full.txt"),
        Some(b"full".as_slice()),
    )
    .unwrap();
    z.finish().unwrap();
    let archive = Archive::open(&path).unwrap();
    assert_eq!(
        names(&archive.list_dir("", ListOptions::default()).unwrap()),
        ["emptydir", "sub"]
    );
    assert!(
        archive
            .list_dir("emptydir", ListOptions::default())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        read_all(&mut *archive.open_member("sub/empty.txt").unwrap()),
        b""
    );
    assert_eq!(
        read_all(&mut *archive.open_member("sub/full.txt").unwrap()),
        b"full"
    );
    let target = extract_cached(&archive, "sub/full.txt").unwrap();
    assert_eq!(std::fs::read(target).unwrap(), b"full");
}

#[test]
fn is_archive_checks_name_and_type() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("folder.zip")).unwrap();
    std::fs::write(tmp.path().join("file.zip"), b"").unwrap();
    std::fs::write(tmp.path().join("file.txt"), b"").unwrap();
    assert!(!is_archive(&local(&tmp.path().join("folder.zip"))));
    assert!(is_archive(&local(&tmp.path().join("file.zip"))));
    assert!(!is_archive(&local(&tmp.path().join("file.txt"))));
}

#[test]
fn hidden_root_name_is_not_hidden() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join(".dot.zip");
    let mut z = zip::ZipWriter::new(File::create(&path).unwrap());
    z.start_file("a", zip::write::SimpleFileOptions::default())
        .unwrap();
    z.finish().unwrap();
    let root = open_archive(&local(Path::new(&path))).unwrap();
    assert!(!root.stat().unwrap().is_hidden);
}

#[test]
fn zip_with_prepended_data_still_reads() {
    // Self-extracting zips start with an executable stub.
    let tmp = tempfile::tempdir().unwrap();
    let content = content(tmp.path());
    let plain = std::fs::read(make("zip", &content, tmp.path())).unwrap();
    let path = tmp.path().join("sfx.zip");
    let mut bytes = vec![0x90u8; 4096];
    bytes.extend_from_slice(&plain);
    std::fs::write(&path, bytes).unwrap();
    let archive = Archive::open(&path).unwrap();
    assert_eq!(
        read_all(&mut *archive.open_member("docs/readme.txt").unwrap()),
        b"read me"
    );
}

// -- review additions --------------------------------------------------------

#[test]
fn compressed_tar_members_read_right_in_any_order() {
    // Decoders are parked between members and reused: forwards, backwards,
    // partial reads and several threads must all see the right bytes.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("many.tar.gz");
    let body = |i: usize| format!("{i:03}|").repeat(200 + i).into_bytes();
    {
        let gz = flate2::write::GzEncoder::new(
            File::create(&path).unwrap(),
            flate2::Compression::default(),
        );
        let mut t = tar::Builder::new(gz);
        for i in 0..40 {
            let data = body(i);
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            t.append_data(&mut h, format!("d/f{i:02}.txt"), data.as_slice())
                .unwrap();
        }
        t.into_inner().unwrap().finish().unwrap();
    }
    let archive = Archive::open(&path).unwrap();
    let read = |i: usize| read_all(&mut *archive.open_member(&format!("d/f{i:02}.txt")).unwrap());
    for i in 0..40 {
        assert_eq!(read(i), body(i), "forward {i}");
    }
    for i in (0..40).rev() {
        assert_eq!(read(i), body(i), "backward {i}");
    }
    for i in (0..40).step_by(3) {
        let mut r = archive.open_member(&format!("d/f{i:02}.txt")).unwrap();
        let mut head = [0u8; 4];
        r.read_exact(&mut head).unwrap();
        assert_eq!(head, body(i)[..4], "partial {i}");
    }
    std::thread::scope(|s| {
        for shift in 0..4 {
            s.spawn(move || {
                for k in 0..40 {
                    let i = (k * 7 + shift * 11) % 40;
                    assert_eq!(read(i), body(i), "thread {shift}: {i}");
                }
            });
        }
    });
    // Walking the whole archive (what a copy does) still gets everything.
    let root = open_archive(&local(&path)).unwrap();
    let (files, bytes) = extract_tree(&root, &tmp.path().join("out"));
    assert_eq!(files, 40);
    assert_eq!(bytes, (0..40).map(|i| body(i).len() as u64).sum::<u64>());
}

#[test]
fn tar_header_quirks() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("q.tar");
    let mut t = tar::Builder::new(File::create(&path).unwrap());
    // Old V7 tars: a directory is a regular file (type NUL) ending in "/".
    let mut h = tar::Header::new_old();
    h.as_mut_bytes()[..7].copy_from_slice(b"olddir/");
    h.set_size(0);
    h.set_mode(0o755);
    h.as_mut_bytes()[156] = 0;
    h.set_cksum();
    t.append(&h, std::io::empty()).unwrap();
    // A Solaris extended header ('X') is metadata, not a member.
    let mut h = tar::Header::new_ustar();
    h.set_path("solaris-hdr").unwrap();
    h.set_size(0);
    h.as_mut_bytes()[156] = b'X';
    h.set_cksum();
    t.append(&h, std::io::empty()).unwrap();
    // An mtime no platform can represent must not break extraction.
    let mut h = tar::Header::new_gnu();
    h.set_size(3);
    h.set_mode(0o644);
    h.set_mtime(u64::MAX);
    t.append_data(&mut h, "future.txt", b"abc".as_slice())
        .unwrap();
    // mtime 0 means "unknown": the extracted file keeps its own time.
    let mut h = tar::Header::new_gnu();
    h.set_size(1);
    h.set_mode(0o644);
    h.set_mtime(0);
    t.append_data(&mut h, "epoch.txt", b"e".as_slice()).unwrap();
    t.into_inner().unwrap();

    let archive = Archive::open(&path).unwrap();
    assert!(archive.member("olddir").unwrap().is_dir);
    assert!(archive.member("solaris-hdr").is_none());
    assert_eq!(
        names(&archive.list_dir("", ListOptions::default()).unwrap()),
        ["olddir", "epoch.txt", "future.txt"]
    );
    let out = tmp.path().join("future.txt");
    archive.extract_file("future.txt", &out, None).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), b"abc");
    let out = tmp.path().join("epoch.txt");
    archive.extract_file("epoch.txt", &out, None).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), b"e");
}

fn stored_zip(dir: &Path, data: &[u8]) -> std::path::PathBuf {
    let path = dir.join("stored.zip");
    let mut z = zip::ZipWriter::new(File::create(&path).unwrap());
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    z.start_file("f.bin", opts).unwrap();
    z.write_all(data).unwrap();
    z.finish().unwrap();
    path
}

#[test]
fn corrupt_stored_data_fails_the_crc_check() {
    let tmp = tempfile::tempdir().unwrap();
    let data = b"0123456789abcdef".repeat(64);
    let path = stored_zip(tmp.path(), &data);
    let mut bytes = std::fs::read(&path).unwrap();
    let at = bytes
        .windows(16)
        .position(|w| w == b"0123456789abcdef")
        .unwrap();
    bytes[at + 100] ^= 0x01;
    std::fs::write(&path, bytes).unwrap();
    let archive = Archive::open(&path).unwrap();

    let mut r = archive.open_member("f.bin").unwrap();
    let err = r.read_to_end(&mut Vec::new()).unwrap_err();
    assert_eq!(err.to_string(), "Bad CRC-32 for file 'f.bin'");
    let err = archive
        .extract_file("f.bin", &tmp.path().join("out.bin"), None)
        .unwrap_err();
    assert!(err.to_string().contains("Bad CRC-32"), "{err}");
    assert!(!tmp.path().join("out.bin").exists());

    // Like zipfile, seeking elsewhere turns the check off; back to 0 restarts it.
    let mut r = archive.open_member("f.bin").unwrap();
    r.seek(SeekFrom::Start(512)).unwrap();
    assert_eq!(read_all(&mut *r).len(), 512);
    r.seek(SeekFrom::Start(0)).unwrap();
    assert!(r.read_to_end(&mut Vec::new()).is_err());
}

#[test]
fn intact_stored_members_pass_the_crc_check_after_seeking_home() {
    let tmp = tempfile::tempdir().unwrap();
    let data = b"stored data ".repeat(50);
    let archive = Archive::open(&stored_zip(tmp.path(), &data)).unwrap();
    let mut r = archive.open_member("f.bin").unwrap();
    let mut head = [0u8; 6];
    r.read_exact(&mut head).unwrap();
    r.seek(SeekFrom::Start(0)).unwrap();
    assert_eq!(read_all(&mut *r), data);
}

#[test]
fn encrypted_zip_entries_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let path = stored_zip(tmp.path(), b"secret");
    // Set the "encrypted" flag bit in the central directory entry.
    let mut bytes = std::fs::read(&path).unwrap();
    let cd = bytes
        .windows(4)
        .rposition(|w| w == [0x50, 0x4b, 0x01, 0x02])
        .unwrap();
    bytes[cd + 8] |= 0x01;
    std::fs::write(&path, bytes).unwrap();
    let archive = Archive::open(&path).unwrap();
    let err = archive.open_member("f.bin").err().unwrap();
    assert_eq!(
        err.to_string(),
        "Encrypted zip entries are not supported yet"
    );
    let err = archive
        .extract_file("f.bin", &tmp.path().join("o"), None)
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Encrypted zip entries are not supported yet"
    );
}
