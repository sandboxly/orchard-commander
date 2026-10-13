//! S3 against moto (the Python version's test server) and MinIO in Docker
//! (which checks signatures for real): buckets and folders, multipart uploads
//! and copies, Amazon's per-bucket addressing, the provider, the file system
//! contract. Port of `tests/test_s3.py` (the signing tests are unit tests in
//! `src/s3.rs`).

mod support;

use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;

use oc_provider::FileSystemProvider;
use oc_remote_http::s3::{self, S3Options, Tuning};
use oc_remote_http::{Error, PlainS3Provider, S3FileSystem};
use oc_vfs::{FileSystem, ListOptions, VPath, VfsError};
use support::{MINIO_SECRET, MINIO_USER, Minio, PyServer, TestContext};

const ACCESS_KEY: &str = "AKIDEXAMPLE";
const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
const MIB: usize = 1024 * 1024;

fn options(port: u16, tmp: &std::path::Path) -> S3Options {
    S3Options {
        secure: false,
        port: Some(port),
        access_key: Some(ACCESS_KEY.into()),
        secret_key: Some(SECRET_KEY.into()),
        trusted_certificates: tmp.join("trusted.json"),
        ..Default::default()
    }
}

fn connect(server: &PyServer, tmp: &std::path::Path) -> S3FileSystem {
    s3::connect(&server.host, &options(server.port, tmp), &|_| true).unwrap()
}

fn write(fs: &dyn FileSystem, path: &str, data: &[u8], chunk: usize) {
    let mut w = fs.open_write(path).unwrap();
    for part in data.chunks(chunk.max(1)) {
        w.write_all(part).unwrap();
    }
    if data.is_empty() {
        w.write_all(b"").unwrap();
    }
    w.finish().unwrap();
}

fn read_all(fs: &dyn FileSystem, path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    fs.open_read(path).unwrap().read_to_end(&mut out).unwrap();
    out
}

fn names(fs: &dyn FileSystem, path: &str, hidden: bool) -> Vec<String> {
    let opts = ListOptions { show_hidden: hidden, ..Default::default() };
    fs.list_dir(path, opts).unwrap().into_iter().map(|e| e.name).collect()
}

fn pattern(len: usize) -> Vec<u8> {
    (0..=255u8).cycle().take(len).collect()
}

fn is_kind(e: &VfsError, kind: std::io::ErrorKind) -> bool {
    matches!(e, VfsError::Io(io) if io.kind() == kind)
}

/// moto with a "photos" bucket.
struct Fixture {
    _tmp: tempfile::TempDir,
    server: PyServer,
    remote: S3FileSystem,
}

fn fixture() -> Option<Fixture> {
    let tmp = tempfile::tempdir().unwrap();
    let server = PyServer::s3()?;
    let remote = connect(&server, tmp.path());
    remote.mkdir("/photos", false).unwrap();
    Some(Fixture { _tmp: tmp, server, remote })
}

#[test]
fn buckets_and_folders() {
    let f = require!(fixture());
    let remote = Arc::new(f.remote.clone());
    remote.mkdir("/photos/2025", false).unwrap();
    write(&*remote, "/photos/2025/beach.jpg", b"jpeg", MIB);
    write(&*remote, "/photos/a b+ü %.txt", b"odd name", MIB);
    write(&*remote, "/photos/.hidden", b"", MIB);
    let root = VPath::new(remote.clone(), "/");
    assert!(root.parent().is_none());
    let top: Vec<(String, bool)> = remote.list_dir("/", ListOptions::default()).unwrap().into_iter().map(|e| (e.name, e.is_dir)).collect();
    assert_eq!(top, [("photos".to_string(), true)]);
    let photos = root.child(&["photos"]);
    assert_eq!(photos.parent(), Some(root.clone()));
    assert_eq!(names(&*remote, "/photos", true), [ "2025", ".hidden", "a b+ü %.txt"]);
    assert!(photos.child(&["2025"]).stat().unwrap().is_dir);
    assert_eq!(photos.child(&["2025", "beach.jpg"]).stat().unwrap().size, 4);
    assert!(photos.child(&["a b+ü %.txt"]).stat().unwrap().mtime > 0.0);
    assert_eq!(read_all(&*remote, "/photos/a b+ü %.txt"), b"odd name");
    assert_eq!(photos.child(&["2025"]).display(), format!("s3+http://{ACCESS_KEY}@127.0.0.1:{}/photos/2025", f.server.port));
    for missing in ["/photos/nothing", "/no-such-bucket", "/no-such-bucket/x"] {
        let err = remote.stat(missing).unwrap_err();
        assert!(err.is_not_found(), "{missing}: {err:?}");
    }
    assert!(remote.list_dir("/photos/nothing", ListOptions::default()).unwrap_err().is_not_found());
    assert!(is_kind(&remote.mkdir("/photos/2025", false).unwrap_err(), std::io::ErrorKind::AlreadyExists));
    assert!(is_kind(&remote.open_write("/top-level.txt").err().unwrap(), std::io::ErrorKind::PermissionDenied));
    assert!(is_kind(&remote.open_read("/photos").err().unwrap(), std::io::ErrorKind::IsADirectory));
    // A folder of a missing bucket creates the bucket.
    remote.mkdir("/newbucket/inside", false).unwrap();
    assert!(remote.is_dir("/newbucket/inside"));
}

#[test]
fn a_folder_stays_when_its_last_file_is_deleted() {
    let f = require!(fixture());
    let remote = &f.remote;
    write(remote, "/photos/trip/day1/a.jpg", b"a", MIB); // no folder markers: implied by the key
    assert!(remote.is_dir("/photos/trip/day1"));
    remote.remove("/photos/trip/day1/a.jpg").unwrap();
    assert!(remote.is_dir("/photos/trip/day1"));
    assert!(remote.list_dir("/photos/trip/day1", ListOptions::default()).unwrap().is_empty());
    remote.remove("/photos/trip/day1").unwrap();
    assert!(!remote.exists("/photos/trip/day1") && remote.is_dir("/photos/trip"));
}

#[test]
fn walk_lists_once() {
    let f = require!(fixture());
    let remote = &f.remote;
    for path in ["/photos/a/1.txt", "/photos/a/b/2.txt", "/photos/c.txt"] {
        write(remote, path, b"x", MIB);
    }
    remote.mkdir("/photos/a/empty", false).unwrap();
    let before = remote.request_count();
    let walked: std::collections::BTreeMap<String, bool> =
        remote.walk("/photos").unwrap().map(|r| r.map(|(rel, e)| (rel.join("/"), e.is_dir))).collect::<Result<_, _>>().unwrap();
    assert_eq!(remote.request_count() - before, 2, "a HEAD of the bucket and one listing");
    let expected: Vec<(&str, bool)> =
        vec![("", true), ("a", true), ("a/1.txt", false), ("a/b", true), ("a/b/2.txt", false), ("a/empty", true), ("c.txt", false)];
    assert_eq!(walked.iter().map(|(k, v)| (k.as_str(), *v)).collect::<Vec<_>>(), expected);
    // Every bucket from the top.
    let all: Vec<String> = remote.walk("/").unwrap().map(|r| r.unwrap().0.join("/")).collect();
    assert!(all.contains(&"photos/a/b/2.txt".to_string()) && all[0].is_empty());
}

#[test]
fn rename_files_and_folders() {
    let f = require!(fixture());
    let remote = &f.remote;
    write(remote, "/photos/docs/one.txt", b"1", MIB);
    write(remote, "/photos/docs/sub/two.txt", b"2", MIB);
    remote.rename("/photos/docs", "/photos/moved").unwrap();
    assert!(!remote.exists("/photos/docs"));
    assert_eq!(names(remote, "/photos/moved", false), ["sub", "one.txt"]);
    remote.rename("/photos/moved/one.txt", "/photos/one.txt").unwrap();
    assert_eq!(read_all(remote, "/photos/one.txt"), b"1");
    assert!(remote.is_dir("/photos/moved")); // still holds sub/
    remote.rename("/photos/moved/sub/two.txt", "/photos/two.txt").unwrap();
    assert!(remote.is_dir("/photos/moved/sub"), "the emptied folder stays (marker)");
    let err = remote.rename("/photos/one.txt", "/photos/two.txt").unwrap_err();
    assert!(is_kind(&err, std::io::ErrorKind::AlreadyExists), "{err:?}");
    let err = remote.rename("/photos", "/pictures").unwrap_err();
    assert!(is_kind(&err, std::io::ErrorKind::PermissionDenied), "{err:?}");
}

#[test]
fn large_files_go_up_in_parts() {
    let f = require!(fixture());
    let remote = &f.remote;
    remote.set_tuning(Tuning { part_size: 5 * MIB as u64, ..Tuning::default() }); // S3's minimum
    let data = pattern(11 * MIB);
    write(remote, "/photos/big.bin", &data, MIB);
    let (head, _) = remote.raw_request("HEAD", Some("photos"), "big.bin", &[]).unwrap();
    assert!(head.header("ETag").unwrap_or("").trim_matches('"').ends_with("-3"), "three parts: {:?}", head.header("ETag"));
    assert_eq!(remote.stat("/photos/big.bin").unwrap().size, data.len() as u64);
    let mut r = remote.open_read("/photos/big.bin").unwrap();
    assert!(r.seekable());
    r.seek(SeekFrom::Start(6 * MIB as u64)).unwrap();
    let mut buf = vec![0; 1000];
    r.read_exact(&mut buf).unwrap();
    assert_eq!(buf, data[6 * MIB..6 * MIB + 1000]);
    r.seek(SeekFrom::End(-10)).unwrap();
    let mut tail = Vec::new();
    r.read_to_end(&mut tail).unwrap();
    assert_eq!(tail, data[data.len() - 10..]);
    // One big write call is split as well.
    write(remote, "/photos/big2.bin", &data, data.len());
    assert_eq!(read_all(remote, "/photos/big2.bin"), data);
}

#[test]
fn a_cancelled_upload_leaves_nothing() {
    let f = require!(fixture());
    let remote = &f.remote;
    remote.set_tuning(Tuning { part_size: 5 * MIB as u64, ..Tuning::default() });
    let mut w = remote.open_write("/photos/partial.bin").unwrap();
    w.write_all(&vec![b'x'; 6 * MIB]).unwrap(); // one part is up already
    drop(w); // cancelled
    assert!(!remote.exists("/photos/partial.bin"));
    let (_, data) = remote.raw_request("GET", Some("photos"), "", &[("uploads", "")]).unwrap();
    let text = String::from_utf8_lossy(&data);
    assert!(!text.contains("<UploadId>"), "the multipart upload was aborted: {text}");
}

#[test]
fn large_objects_are_copied_in_parts() {
    let f = require!(fixture());
    let remote = &f.remote;
    remote.set_tuning(Tuning { part_size: 16 * MIB as u64, copy_limit: 6 * MIB as u64, copy_part_size: 5 * MIB as u64 });
    let data = pattern(11 * MIB);
    write(remote, "/photos/big.bin", &data, MIB);
    remote.rename("/photos/big.bin", "/photos/renamed.bin").unwrap();
    assert_eq!(read_all(remote, "/photos/renamed.bin"), data);
    assert!(!remote.exists("/photos/big.bin"));
}

#[test]
fn buckets_are_folders_at_the_top() {
    let f = require!(fixture());
    let remote = &f.remote;
    remote.mkdir("/videos", false).unwrap();
    remote.mkdir("/videos", true).unwrap();
    assert!(is_kind(&remote.mkdir("/videos", false).unwrap_err(), std::io::ErrorKind::AlreadyExists));
    write(remote, "/videos/clip.mp4", b"mp4", MIB);
    assert_eq!(names(remote, "/", false), ["photos", "videos"]);
    remote.remove("/videos").unwrap(); // emptied, then deleted
    assert_eq!(names(remote, "/", false), ["photos"]);
    assert!(is_kind(&remote.remove("/").unwrap_err(), std::io::ErrorKind::PermissionDenied));
}

#[test]
fn a_secret_is_needed_with_an_access_key() {
    let server = require!(PyServer::s3());
    let tmp = tempfile::tempdir().unwrap();
    let mut o = options(server.port, tmp.path());
    o.secret_key = None;
    let err = s3::connect(&server.host, &o, &|_| true).err().unwrap();
    assert!(matches!(&err, Error::AuthenticationRequired(m) if m == "The secret key for AKIDEXAMPLE is needed"), "{err}");
}

#[test]
fn amazon_buckets_by_name_and_region() {
    // Amazon: each bucket at its own host name in its region, found out once.
    let server = require!(PyServer::s3());
    let creator = S3FileSystem::new(s3::AWS_HOST, 443, true, Some("eu-central-1"), Some(ACCESS_KEY), Some(SECRET_KEY), None, oc_remote_http::http::TIMEOUT);
    creator.route_all_to(&server.host, server.port); // really: moto
    creator.mkdir("/euro-bucket", false).unwrap();
    creator.mkdir("/dotted.bucket", false).unwrap();
    creator.close();

    let remote = S3FileSystem::new(s3::AWS_HOST, 443, true, None, Some(ACCESS_KEY), Some(SECRET_KEY), None, oc_remote_http::http::TIMEOUT);
    remote.route_all_to(&server.host, server.port);
    assert!(remote.is_aws());
    assert_eq!(remote.type_name(), format!("s3://{ACCESS_KEY}@s3.amazonaws.com"));
    write(&remote, "/euro-bucket/hello.txt", b"hi", MIB);
    write(&remote, "/dotted.bucket/hello.txt", b"hi", MIB);
    assert_eq!(names(&remote, "/euro-bucket", false), ["hello.txt"]);
    let regions = remote.bucket_regions();
    assert_eq!(regions.get("euro-bucket").map(String::as_str), Some("eu-central-1"));
    assert_eq!(regions.get("dotted.bucket").map(String::as_str), Some("eu-central-1"));
    let hosts = remote.hosts_used();
    assert!(hosts.contains(&"euro-bucket.s3.eu-central-1.amazonaws.com".to_string()), "virtual-hosted: {hosts:?}");
    assert!(hosts.contains(&"s3.eu-central-1.amazonaws.com".to_string()), "dots don't fit the certificate: path-style: {hosts:?}");
    remote.close();
}

#[test]
fn contract() {
    let f = require!(fixture());
    let remote: Arc<dyn FileSystem> = Arc::new(f.remote.clone());
    remote.mkdir("/contract", false).unwrap();
    oc_vfs::testing::check_filesystem(&VPath::new(remote.clone(), "/contract"));
    // And in a folder below a bucket's root.
    remote.mkdir("/contract/deeper", false).unwrap();
    oc_vfs::testing::check_filesystem(&VPath::new(remote, "/contract/deeper"));
}

// -- the provider ---------------------------------------------------------------

#[test]
fn provider_connects_with_start_folder() {
    let server = require!(PyServer::s3());
    let tmp = tempfile::tempdir().unwrap();
    let remote = connect(&server, tmp.path());
    remote.mkdir("/media/music", false).unwrap();
    remote.close();
    let provider = PlainS3Provider;
    let params = serde_json::json!({"host": server.host, "port": server.port, "username": ACCESS_KEY, "path": "media"});
    let params = params.as_object().unwrap().clone();
    let ctx = TestContext::new(tmp.path(), true);
    let fs = provider.connect(&params, Some(SECRET_KEY), &ctx).unwrap();
    let start = provider.start_path(&params, &fs);
    assert_eq!(start, "/media");
    assert_eq!(names(&*fs, &start, false), ["music"]);
    assert_eq!(VPath::new(fs.clone(), start).parent(), Some(VPath::new(fs.clone(), "/")));
    assert!(provider.is_connected(&fs));
    fs.close();
    assert!(!provider.is_connected(&fs));
    let err = provider.connect(&params, None, &ctx).err().unwrap();
    assert!(matches!(err, oc_provider::ConnectError::AuthenticationRequired(_)), "{err}");
}

// -- copies across file systems ---------------------------------------------------

#[test]
fn copies_and_moves_between_file_systems() {
    use oc_vfs::testing::{MemFS, sample_tree, write_tree};
    use oc_vfs::{Conflict, TransferCallbacks, ops};

    let f = require!(fixture());
    let s3fs: Arc<dyn FileSystem> = Arc::new(f.remote.clone());
    let mem = Arc::new(MemFS::new());
    mem.add_dir("src");
    let src = VPath::new(mem.clone(), "src");
    write_tree(&src, &sample_tree()).unwrap();
    let mut overwrite = |_: &str, _: &str| Conflict::Overwrite;
    let not_cancelled = || false;
    // Up to S3, moved within it (a server-side rename), and back down.
    let cb = TransferCallbacks { on_conflict: &mut overwrite, on_progress: None, is_cancelled: &not_cancelled };
    ops::copy(std::slice::from_ref(&src), &VPath::new(s3fs.clone(), "/photos"), cb).unwrap();
    assert_eq!(read_all(&*s3fs, "/photos/src/docs/deep/data.bin"), sample_tree()[3].1);
    let cb = TransferCallbacks { on_conflict: &mut overwrite, on_progress: None, is_cancelled: &not_cancelled };
    ops::move_to(&[VPath::new(s3fs.clone(), "/photos/src")], &VPath::new(s3fs.clone(), "/photos/moved"), cb).unwrap();
    assert!(!s3fs.exists("/photos/src") && s3fs.exists("/photos/moved/.hidden"));
    mem.add_dir("back");
    let cb = TransferCallbacks { on_conflict: &mut overwrite, on_progress: None, is_cancelled: &not_cancelled };
    ops::copy(&[VPath::new(s3fs.clone(), "/photos/moved")], &VPath::new(mem.clone(), "back"), cb).unwrap();
    for (rel, data) in sample_tree() {
        assert_eq!(mem.read_file(&format!("back/moved/{rel}")).as_deref(), Some(data.as_slice()), "{rel}");
    }
}

// -- MinIO: real signature checks -----------------------------------------------

fn minio_fs(minio: &Minio, secret: &str) -> oc_remote_http::Result<S3FileSystem> {
    let tmp = std::env::temp_dir();
    let o = S3Options {
        secure: false,
        port: Some(minio.port),
        access_key: Some(MINIO_USER.into()),
        secret_key: Some(secret.into()),
        trusted_certificates: tmp.join("unused-trusted.json"),
        ..Default::default()
    };
    s3::connect("127.0.0.1", &o, &|_| true)
}

#[test]
fn minio_with_real_signatures() {
    let minio = require!(Minio::start());
    let err = minio_fs(&minio, "wrong-secret").err().unwrap();
    assert!(matches!(&err, Error::AuthenticationRequired(m) if m.contains("didn't accept the access key")), "{err}");

    let remote = minio_fs(&minio, MINIO_SECRET).unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(remote.clone());
    fs.mkdir("/contract", false).unwrap();
    oc_vfs::testing::check_filesystem(&VPath::new(fs.clone(), "/contract"));

    // Odd names in keys, listings (encoding-type=url) and copy sources.
    fs.mkdir("/files/a b+ü %", false).unwrap();
    write(&*fs, "/files/a b+ü %/x&y=1?.txt", b"odd", MIB);
    assert_eq!(names(&*fs, "/files/a b+ü %", false), ["x&y=1?.txt"]);
    fs.rename("/files/a b+ü %", "/files/renamed ü").unwrap();
    assert_eq!(read_all(&*fs, "/files/renamed ü/x&y=1?.txt"), b"odd");

    // Multipart uploads and copies in parts, signed and checked.
    remote.set_tuning(Tuning { part_size: 5 * MIB as u64, copy_limit: 6 * MIB as u64, copy_part_size: 5 * MIB as u64 });
    let data = pattern(11 * MIB);
    write(&*fs, "/files/big.bin", &data, MIB);
    fs.rename("/files/big.bin", "/files/big-renamed.bin").unwrap();
    let mut r = fs.open_read("/files/big-renamed.bin").unwrap();
    r.seek(SeekFrom::Start(10 * MIB as u64)).unwrap();
    let mut rest = Vec::new();
    r.read_to_end(&mut rest).unwrap();
    assert_eq!(rest, data[10 * MIB..]);
    drop(r);
    let mut w = fs.open_write("/files/cancelled.bin").unwrap();
    w.write_all(&vec![0; 6 * MIB]).unwrap();
    drop(w);
    assert!(!fs.exists("/files/cancelled.bin"));
    fs.remove("/files").unwrap();
    assert!(!fs.exists("/files"));
    fs.close();
}
