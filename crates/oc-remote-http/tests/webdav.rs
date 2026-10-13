//! WebDAV against WsgiDAV (the Python version's test server): authentication
//! (Basic, Digest), certificates, ranged reads, uploads, names, the provider,
//! and the file system contract. Port of `tests/test_webdav.py`.

mod support;

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use oc_provider::FileSystemProvider;
use oc_remote_http::webdav::{self, WebDavOptions};
use oc_remote_http::{Error, WebDavFileSystem, WebDavProvider, PlainWebDavProvider};
use oc_vfs::{FileSystem, ListOptions, VPath, VfsError};
use support::{PASSWORD, PyServer, TestContext, USER};

fn big() -> Vec<u8> {
    (0..=255u8).collect::<Vec<_>>().repeat(4096) // 1 MiB
}

fn share(tmp: &Path) -> PathBuf {
    let root = tmp.join("share");
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("docs/readme.txt"), "hello from webdav").unwrap();
    std::fs::write(root.join("big.bin"), big()).unwrap();
    std::fs::write(root.join("a b ü %.txt"), "odd name").unwrap();
    let mut zip = zip::ZipWriter::new(std::fs::File::create(root.join("archive.zip")).unwrap());
    zip.start_file("inside/note.txt", zip::write::SimpleFileOptions::default()).unwrap();
    zip.write_all(b"zipped").unwrap();
    zip.finish().unwrap();
    root
}

struct Setup {
    tmp: tempfile::TempDir,
    folder: PathBuf,
}

fn setup() -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let folder = share(tmp.path());
    Setup { tmp, folder }
}

fn options(server: &PyServer, tmp: &Path) -> WebDavOptions {
    WebDavOptions {
        secure: false,
        port: Some(server.port),
        root: server.root.clone(),
        username: Some(USER.into()),
        password: Some(PASSWORD.into()),
        trusted_certificates: tmp.join("trusted.json"),
        ..Default::default()
    }
}

fn connect(server: &PyServer, tmp: &Path, tweak: impl FnOnce(&mut WebDavOptions)) -> oc_remote_http::Result<WebDavFileSystem> {
    let mut o = options(server, tmp);
    tweak(&mut o);
    webdav::connect(&server.host, &o, &|_| true)
}

fn names(fs: &dyn FileSystem, path: &str) -> Vec<String> {
    fs.list_dir(path, ListOptions::default()).unwrap().into_iter().map(|e| e.name).collect()
}

fn read_all(fs: &dyn FileSystem, path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    fs.open_read(path).unwrap().read_to_end(&mut out).unwrap();
    out
}

fn is_kind(e: &VfsError, kind: std::io::ErrorKind) -> bool {
    matches!(e, VfsError::Io(io) if io.kind() == kind)
}

#[test]
fn browse_and_read() {
    let s = setup();
    let server = require!(PyServer::webdav(&s.folder, false, None));
    let remote = Arc::new(connect(&server, s.tmp.path(), |_| {}).unwrap());
    let root = VPath::new(remote.clone(), "/");
    assert!(root.parent().is_none());
    assert_eq!(names(&*remote, "/"), ["docs", "a b ü %.txt", "archive.zip", "big.bin"]);
    let odd = root.child(&["a b ü %.txt"]);
    assert_eq!(odd.stat().unwrap().size, "odd name".len() as u64);
    assert!(odd.stat().unwrap().mtime > 0.0);
    assert_eq!(read_all(&*remote, &odd.path), b"odd name");
    assert!(root.child(&["docs", "readme.txt"]).display().ends_with("/dav/docs/readme.txt"));
    assert!(remote.display("/").starts_with(&format!("webdav://{USER}@127.0.0.1:")));
    assert_eq!(remote.type_name(), format!("webdav://{USER}@127.0.0.1:{}", server.port));
    assert_eq!(VPath::new(remote.clone(), "/docs").name(), "docs");
    assert_eq!(root.child(&["docs"]).parent().map(|p| p.path), Some("/".to_string()));
    remote.close();
}

#[test]
fn wrong_or_missing_password() {
    let s = setup();
    let server = require!(PyServer::webdav(&s.folder, false, None));
    let err = connect(&server, s.tmp.path(), |o| o.password = Some("nope".into())).err().unwrap();
    assert!(matches!(&err, Error::AuthenticationRequired(m) if m.contains("didn't accept the user name and password")), "{err}");
    let err = connect(&server, s.tmp.path(), |o| o.password = None).err().unwrap();
    assert!(matches!(err, Error::AuthenticationRequired(_)), "{err}");
}

#[test]
fn digest_authentication() {
    let s = setup();
    let server = require!(PyServer::webdav(&s.folder, true, None));
    let remote = connect(&server, s.tmp.path(), |_| {}).unwrap();
    assert_eq!(names(&remote, "/docs"), ["readme.txt"]);
    // Digest is kept for further requests (with increasing nonce counts), uploads too.
    let mut w = remote.open_write("/docs/digest.txt").unwrap();
    w.write_all(b"via digest").unwrap();
    w.finish().unwrap();
    assert_eq!(read_all(&remote, "/docs/digest.txt"), b"via digest");
    remote.close();
    let err = connect(&server, s.tmp.path(), |o| o.password = Some("nope".into())).err().unwrap();
    assert!(matches!(err, Error::AuthenticationRequired(_)), "{err}");
}

#[test]
fn seeking_reads_ranges() {
    let s = setup();
    let server = require!(PyServer::webdav(&s.folder, false, None));
    let remote = Arc::new(connect(&server, s.tmp.path(), |_| {}).unwrap());
    let data = big();
    let mut f = remote.open_read("/big.bin").unwrap();
    assert!(f.seekable());
    f.seek(SeekFrom::Start(500_000)).unwrap();
    let mut buf = vec![0; 1000];
    f.read_exact(&mut buf).unwrap();
    assert_eq!(buf, data[500_000..501_000]);
    f.seek(SeekFrom::End(-10)).unwrap();
    let mut tail = Vec::new();
    f.read_to_end(&mut tail).unwrap();
    assert_eq!(tail, data[data.len() - 10..]);
    f.seek(SeekFrom::Start(3)).unwrap();
    let mut five = [0; 5];
    f.read_exact(&mut five).unwrap();
    assert_eq!(five, data[3..8]);
    assert_eq!(f.stream_position().unwrap(), 8);
    f.seek(SeekFrom::Current(1000)).unwrap(); // a short hop reads on
    f.read_exact(&mut five).unwrap();
    assert_eq!(five, data[1008..1013]);
    assert!(f.seek(SeekFrom::Current(-2000)).is_err());
    f.seek(SeekFrom::Start(data.len() as u64 + 5)).unwrap();
    assert_eq!(f.read(&mut five).unwrap(), 0);
    drop(f);

    // A zip read in place: only the parts it needs, by seeking.
    let reader = remote.open_read("/archive.zip").unwrap();
    let mut zip = zip::ZipArchive::new(reader).unwrap();
    let mut note = String::new();
    zip.by_name("inside/note.txt").unwrap().read_to_string(&mut note).unwrap();
    assert_eq!(note, "zipped");
    // And as a folder, the way panels open it.
    let archive = oc_archive::open_archive(&VPath::new(remote.clone(), "/archive.zip")).unwrap();
    let inside = archive.child(&["inside", "note.txt"]);
    assert_eq!(read_all(&*inside.fs, &inside.path), b"zipped");
    remote.close();
}

#[test]
fn failed_copy_uploads_nothing() {
    let s = setup();
    let server = require!(PyServer::webdav(&s.folder, false, None));
    let remote = connect(&server, s.tmp.path(), |_| {}).unwrap();
    let folder = &s.folder;
    let mut w = remote.open_write("/partial.bin").unwrap();
    w.write_all(&[b'x'; 1000]).unwrap();
    drop(w); // cancelled
    assert!(!folder.join("partial.bin").exists());
    let mut w = remote.open_write("/ä.txt").unwrap();
    w.write_all(b"unicode name").unwrap();
    w.finish().unwrap();
    assert_eq!(std::fs::read(folder.join("ä.txt")).unwrap(), b"unicode name");
    remote.mkdir("/new folder/sub", false).unwrap();
    remote.rename("/ä.txt", "/new folder/sub/moved.txt").unwrap();
    assert!(folder.join("new folder/sub/moved.txt").exists());
    let err = remote.mkdir("/new folder", false).unwrap_err();
    assert!(is_kind(&err, std::io::ErrorKind::AlreadyExists), "{err:?}");
    remote.mkdir("/new folder", true).unwrap();
    remote.rename("/new folder/sub", "/renamed sub").unwrap();
    assert!(folder.join("renamed sub/moved.txt").exists());
    let err = remote.rename("/renamed sub", "/docs").unwrap_err();
    assert!(is_kind(&err, std::io::ErrorKind::AlreadyExists), "{err:?}");
    remote.remove("/new folder").unwrap();
    assert!(!folder.join("new folder").exists());
    let err = remote.stat("/nothing").unwrap_err();
    assert!(err.is_not_found(), "{err:?}");
    let err = remote.list_dir("/nothing", ListOptions::default()).unwrap_err();
    assert!(err.is_not_found(), "{err:?}");
    let mut w = remote.open_write("/no/such/folder.txt").unwrap();
    w.write_all(b"x").unwrap();
    let err = w.finish().unwrap_err();
    assert!(err.is_not_found(), "{err:?}");
    // Bigger than the in-memory spool: goes through a temp file.
    let large: Vec<u8> = (0..20 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let mut w = remote.open_write("/large.bin").unwrap();
    for chunk in large.chunks(1024 * 1024) {
        w.write_all(chunk).unwrap();
    }
    w.finish().unwrap();
    assert_eq!(std::fs::read(folder.join("large.bin")).unwrap(), large);
    remote.close();
}

#[test]
fn modification_times_can_be_set() {
    let s = setup();
    let server = require!(PyServer::webdav(&s.folder, false, None));
    let remote = connect(&server, s.tmp.path(), |_| {}).unwrap();
    remote.set_mtime("/docs/readme.txt", 1_600_000_000.0).unwrap();
    assert!((remote.stat("/docs/readme.txt").unwrap().mtime - 1_600_000_000.0).abs() < 2.0);
    remote.set_mtime("/big.bin", 1_500_000_000.0).unwrap(); // the method found out above
    assert!((remote.stat("/big.bin").unwrap().mtime - 1_500_000_000.0).abs() < 2.0);
    remote.close();
}

#[test]
fn walks_and_parallel_requests() {
    let s = setup();
    let server = require!(PyServer::webdav(&s.folder, false, None));
    let remote = Arc::new(connect(&server, s.tmp.path(), |_| {}).unwrap());
    let walked: Vec<(String, bool)> =
        remote.walk("/").unwrap().map(|r| r.map(|(rel, e)| (rel.join("/"), e.is_dir))).collect::<Result<_, _>>().unwrap();
    assert_eq!(walked[0], (String::new(), true));
    assert!(walked.contains(&("docs/readme.txt".into(), false)));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let remote = remote.clone();
            std::thread::spawn(move || {
                for _ in 0..10 {
                    assert_eq!(remote.stat("/docs/readme.txt").unwrap().size, 17);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    remote.close();
    let err = remote.stat("/docs").unwrap_err();
    assert!(err.to_string().contains("was closed"), "{err}");
}

#[test]
fn contract() {
    let s = setup();
    let server = require!(PyServer::webdav(&s.folder, false, None));
    let remote: Arc<dyn FileSystem> = Arc::new(connect(&server, s.tmp.path(), |_| {}).unwrap());
    remote.mkdir("/contract", false).unwrap();
    oc_vfs::testing::check_filesystem(&VPath::new(remote.clone(), "/contract"));
    remote.close();
}

// -- certificates -------------------------------------------------------------

#[test]
fn untrusted_certificate_is_confirmed_and_pinned() {
    let s = setup();
    let cert_dir = s.tmp.path().join("cert");
    let server = require!(PyServer::webdav(&s.folder, false, Some(&cert_dir)));
    let asked = std::sync::Mutex::new(Vec::new());
    let mut o = options(&server, s.tmp.path());
    o.secure = true;
    let remote = webdav::connect(&server.host, &o, &|q| {
        asked.lock().unwrap().push(q.clone());
        true
    })
    .unwrap();
    assert_eq!(names(&remote, "/docs"), ["readme.txt"]);
    assert!(remote.display("/").starts_with("webdavs://"));
    remote.close();
    let asked = asked.into_inner().unwrap();
    assert_eq!(asked.len(), 1);
    assert_eq!(Some(&asked[0].fingerprint), server.fingerprint.as_ref());
    assert_eq!(asked[0].port, server.port);
    assert_eq!(asked[0].problem, "self-signed certificate");
    // Known now: no question, and the pinned certificate carries uploads and the contract.
    let remote: Arc<dyn FileSystem> = Arc::new(webdav::connect(&server.host, &o, &|_| panic!("asked again")).unwrap());
    remote.mkdir("/contract", false).unwrap();
    oc_vfs::testing::check_filesystem(&VPath::new(remote.clone(), "/contract"));
    remote.close();
}

#[test]
fn rejected_and_changed_certificates() {
    let s = setup();
    let cert_dir = s.tmp.path().join("cert");
    let server = require!(PyServer::webdav(&s.folder, false, Some(&cert_dir)));
    let mut o = options(&server, s.tmp.path());
    o.secure = true;
    let err = webdav::connect(&server.host, &o, &|_| false).err().unwrap();
    assert!(matches!(err, Error::CertificateRejected(_)), "{err}");
    assert!(matches!(oc_provider::ConnectError::from(err), oc_provider::ConnectError::Cancelled));
    let trusted = s.tmp.path().join("trusted.json");
    std::fs::write(&trusted, format!("{{\"{}:{}\": \"SHA256:00:11\"}}", server.host, server.port)).unwrap();
    let err = webdav::connect(&server.host, &o, &|_| true).err().unwrap();
    assert!(matches!(&err, Error::CertificateChanged(m) if m.contains("has changed")), "{err}");
    // A pinned server showing another certificate later: refused on every connection.
    let pinned = WebDavFileSystem::new(
        &server.host,
        server.port,
        true,
        &server.root,
        Some(USER),
        Some(PASSWORD),
        Some("SHA256:00:11".into()),
        oc_remote_http::http::TIMEOUT,
    );
    let err = pinned.stat("/").unwrap_err();
    let VfsError::Io(io) = &err else { panic!("{err:?}") };
    let inner = io.get_ref().and_then(|e| e.downcast_ref::<oc_remote_http::RemoteError>());
    assert!(matches!(inner, Some(oc_remote_http::RemoteError::CertificateChanged(m)) if m.contains("intercepting")), "{err:?}");
}

#[test]
fn plain_http_to_an_https_server_fails_cleanly() {
    let s = setup();
    let cert_dir = s.tmp.path().join("cert");
    let server = require!(PyServer::webdav(&s.folder, false, Some(&cert_dir)));
    let err = connect(&server, s.tmp.path(), |_| {}).err().unwrap();
    assert!(matches!(err, Error::Connection(_)), "{err:?}");
}

// -- the provider ---------------------------------------------------------------

#[test]
fn provider_connects_with_start_folder() {
    let s = setup();
    let server = require!(PyServer::webdav(&s.folder, false, None));
    let provider = PlainWebDavProvider;
    let params = serde_json::json!({"host": server.host, "port": server.port, "root": server.root, "username": USER, "path": "docs"});
    let params = params.as_object().unwrap().clone();
    let ctx = TestContext::new(s.tmp.path(), true);
    let fs = provider.connect(&params, Some(PASSWORD), &ctx).unwrap();
    let start = provider.start_path(&params, &fs);
    assert_eq!(start, "/docs");
    assert_eq!(names(&*fs, &start), ["readme.txt"]);
    assert!(provider.is_connected(&fs));
    fs.close();
    assert!(!provider.is_connected(&fs));
    let err = provider.connect(&params, Some("wrong"), &ctx).err().unwrap();
    assert!(matches!(err, oc_provider::ConnectError::AuthenticationRequired(_)), "{err}");
}

#[test]
fn provider_asks_about_certificates() {
    let s = setup();
    let cert_dir = s.tmp.path().join("cert");
    let server = require!(PyServer::webdav(&s.folder, false, Some(&cert_dir)));
    let params = serde_json::json!({"host": server.host, "port": server.port, "root": server.root, "username": USER});
    let params = params.as_object().unwrap().clone();
    let ctx = TestContext::new(s.tmp.path(), true);
    let fs = WebDavProvider.connect(&params, Some(PASSWORD), &ctx).unwrap();
    fs.close();
    let asked = ctx.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    let (title, message, ok) = &asked[0];
    assert_eq!((title.as_str(), ok.as_str()), ("Untrusted certificate", "Trust and connect"));
    let expected = format!(
        "The certificate of 127.0.0.1:{} isn't trusted by this computer (self-signed certificate). Servers on a home \
         network often use such self-made certificates.\n\nCertificate fingerprint:\n  {}\n\nIf you know the server's \
         fingerprint, check that it matches. Trusting it remembers this certificate; Orchard Commander will refuse to \
         connect if it ever changes.",
        server.port,
        server.fingerprint.as_deref().unwrap()
    );
    assert_eq!(message, &expected);
    assert!(s.tmp.path().join("trusted_certificates.json").exists());
    // Declined: the connection is cancelled.
    let declined = TestContext::new(&s.tmp.path().join("other"), false);
    let err = WebDavProvider.connect(&params, Some(PASSWORD), &declined).err().unwrap();
    assert!(matches!(err, oc_provider::ConnectError::Cancelled), "{err}");
}
