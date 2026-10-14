//! Port of tests/test_sftp.py (+ the SFTP parts of test_fs_contract.py), run
//! against the in-process russh server from `oc_sftp::testing`.

use std::cell::RefCell;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_provider::ConnectError;
use oc_sftp::testing::{
    PASSWORD, ServerOptions, SftpServer, USER, random_key, start_sftp_server, start_sftp_server_with,
};
use oc_sftp::{Agent, ConnectOptions, HostKeyQuestion, SftpFileSystem, connect};
use oc_vfs::{Conflict, FileSystem, ListOptions, Progress, TransferCallbacks, VPath, VfsError};

struct Fixture {
    _dir: tempfile::TempDir,
    tmp: PathBuf,
    server: SftpServer,
    known_hosts: PathBuf,
}

fn tree(root: &Path) {
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(root.join("docs").join("readme.txt"), "hello from sftp").unwrap();
    let big: Vec<u8> = (0..=255u8).collect::<Vec<_>>().repeat(4096); // 1 MiB
    fs::write(root.join("big.bin"), big).unwrap();
    fs::write(root.join(".profile"), "x").unwrap();
}

fn fixture_with(opts: ServerOptions) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().canonicalize().unwrap();
    let root = tmp.join("remote");
    tree(&root);
    let server = start_sftp_server_with(&root, opts);
    Fixture { known_hosts: tmp.join("known_hosts"), tmp, server, _dir: dir }
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().canonicalize().unwrap();
    let root = tmp.join("remote");
    tree(&root);
    let server = start_sftp_server(&root);
    Fixture { known_hosts: tmp.join("known_hosts"), tmp, server, _dir: dir }
}

impl Fixture {
    fn options(&self, password: Option<&str>) -> ConnectOptions {
        let mut o = ConnectOptions::new(self.server.host.clone(), self.known_hosts.clone());
        o.port = self.server.port;
        o.username = Some(USER.into());
        o.password = password.map(str::to_string);
        o.agent = Agent::Off;
        o.home = Some(self.tmp.join("home")); // no ~/.ssh keys, no system known_hosts
        o.timeout = Duration::from_secs(5);
        o
    }

    fn connect(&self) -> Arc<SftpFileSystem> {
        connect(&self.options(Some(PASSWORD)), &|_| true).unwrap_or_else(|e| panic!("connect: {e}"))
    }
}

fn never(src: &str, dst: &str) -> Conflict {
    panic!("unexpected conflict {src} -> {dst}")
}

fn names(entries: &[oc_vfs::Entry]) -> Vec<String> {
    entries.iter().map(|e| e.name.clone()).collect()
}

#[test]
fn unknown_host_key_is_confirmed_and_remembered() {
    let f = fixture();
    let asked = RefCell::new(Vec::<HostKeyQuestion>::new());
    let remote = connect(&f.options(Some(PASSWORD)), &|q| {
        asked.borrow_mut().push(q.clone());
        true
    })
    .unwrap();
    remote.close();
    let asked = asked.into_inner();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].fingerprint, f.server.host_key_fingerprint);
    assert_eq!(asked[0].port, f.server.port);
    assert_eq!(asked[0].host, "127.0.0.1");
    assert_eq!(asked[0].key_type, "ssh-ed25519");
    // Stored the way paramiko writes it.
    let line = fs::read_to_string(&f.known_hosts).unwrap();
    assert!(line.starts_with(&format!("[127.0.0.1]:{} ssh-ed25519 AAAA", f.server.port)), "{line}");
    assert!(line.ends_with('\n') && line.lines().count() == 1);
    // Second time: known, no question.
    connect(&f.options(Some(PASSWORD)), &|_| panic!("asked again")).unwrap().close();
}

#[test]
fn rejected_and_changed_host_keys() {
    let f = fixture();
    match connect(&f.options(Some(PASSWORD)), &|_| false) {
        Err(ConnectError::Cancelled) => {}
        other => panic!("expected Cancelled, got {:?}", other.err()),
    }
    assert!(fs::read_to_string(&f.known_hosts).unwrap_or_default().is_empty(), "rejected key remembered");
    // Pretend we know a different key for this host.
    let other = random_key();
    oc_sftp::known_hosts::append(&f.known_hosts, &format!("[{}]:{}", f.server.host, f.server.port), other.public_key())
        .unwrap();
    match connect(&f.options(Some(PASSWORD)), &|_| panic!("asked")) {
        Err(ConnectError::Connection(msg)) => {
            assert!(msg.starts_with("The host key of 127.0.0.1 has changed (now SHA256:"), "{msg}");
            assert!(msg.contains(&f.server.host_key_fingerprint), "{msg}");
            assert!(msg.contains("Someone may be intercepting the connection"), "{msg}");
        }
        other => panic!("expected a changed host key, got {:?}", other.err()),
    }
}

#[test]
fn system_known_hosts_is_used_and_hashed_entries_match() {
    use hmac_line::hashed_line;
    let f = fixture();
    let ssh = f.tmp.join("home").join(".ssh");
    fs::create_dir_all(&ssh).unwrap();
    let name = format!("[127.0.0.1]:{}", f.server.port);
    fs::write(ssh.join("known_hosts"), hashed_line(&name, &f.server.host_key)).unwrap();
    connect(&f.options(Some(PASSWORD)), &|_| panic!("asked although ~/.ssh/known_hosts knows the host"))
        .unwrap()
        .close();
}

mod hmac_line {
    use data_encoding::BASE64;
    use hmac::{Hmac, KeyInit, Mac};
    use russh::keys::PublicKey;
    use sha1::Sha1;

    /// A hashed known_hosts line (`ssh-keygen -H` style).
    pub fn hashed_line(name: &str, key: &PublicKey) -> String {
        let salt = [42u8; 20];
        let mac = Hmac::<Sha1>::new_from_slice(&salt).unwrap().chain_update(name.as_bytes()).finalize().into_bytes();
        let blob = key.to_openssh().unwrap();
        let blob = blob.split_whitespace().nth(1).unwrap().to_string();
        format!("|1|{}|{} ssh-ed25519 {blob}\n", BASE64.encode(&salt), BASE64.encode(&mac))
    }
}

#[test]
fn wrong_password() {
    let f = fixture();
    match connect(&f.options(Some("nope")), &|_| true) {
        Err(ConnectError::AuthenticationRequired(msg)) => {
            assert_eq!(msg, format!("The server didn't accept the login for {USER}@127.0.0.1"))
        }
        other => panic!("expected AuthenticationRequired, got {:?}", other.err()),
    }
}

#[test]
fn password_asked_without_agent_or_keys() {
    // No ssh-agent and no ~/.ssh keys (e.g. a fresh Windows account): ask for the password.
    let f = fixture();
    match connect(&f.options(None), &|_| true) {
        Err(ConnectError::AuthenticationRequired(msg)) => assert_eq!(msg, format!("{USER}@127.0.0.1 needs a password")),
        other => panic!("expected AuthenticationRequired, got {:?}", other.err()),
    }
}

#[test]
fn connection_refused_is_described() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let dir = tempfile::tempdir().unwrap();
    let mut o = ConnectOptions::new("127.0.0.1", dir.path().join("known_hosts"));
    o.port = port;
    o.agent = Agent::Off;
    match connect(&o, &|_| true) {
        Err(ConnectError::Connection(msg)) => {
            assert!(msg.starts_with(&format!("Can't connect to 127.0.0.1:{port}: ")), "{msg}");
            assert!(msg.contains("refused"), "{msg}");
        }
        other => panic!("expected a connection error, got {:?}", other.err()),
    }
}

#[test]
fn browse_read_write() {
    let f = fixture();
    let remote: Arc<dyn FileSystem> = f.connect();
    let root = VPath::new(remote.clone(), "/");
    assert!(root.parent().is_none());
    assert_eq!(names(&root.list_dir(ListOptions::default()).unwrap()), ["docs", "big.bin"]);
    let all = root.list_dir(ListOptions { show_hidden: true, ..Default::default() }).unwrap();
    assert!(names(&all).contains(&".profile".to_string()));
    let docs = root.child(&["docs"]);
    assert!(docs.is_dir());
    assert_eq!(docs.display(), format!("sftp://{USER}@127.0.0.1:{}/docs", f.server.port));
    let readme = docs.child(&["readme.txt"]);
    assert_eq!(readme.stat().unwrap().size, 15);
    let mut text = String::new();
    readme.open_read().unwrap().read_to_string(&mut text).unwrap();
    assert_eq!(text, "hello from sftp");

    // Download a tree.
    let out = f.tmp.join("down");
    fs::create_dir(&out).unwrap();
    let mut progress: Vec<Progress> = Vec::new();
    {
        let mut on_conflict = never;
        let mut on_progress = |p: &Progress| progress.push(p.clone());
        let cb = TransferCallbacks {
            on_conflict: &mut on_conflict,
            on_progress: Some(&mut on_progress),
            is_cancelled: &|| false,
        };
        oc_local::copy(&[docs.clone(), root.child(&["big.bin"])], &oc_local::local(&out), false, cb).unwrap();
    }
    assert_eq!(fs::read_to_string(out.join("docs").join("readme.txt")).unwrap(), "hello from sftp");
    assert!(fs::read(out.join("big.bin")).unwrap() == fs::read(f.server.root.join("big.bin")).unwrap());
    let last = progress.last().unwrap();
    assert_eq!(last.bytes_done, last.bytes_total);

    // Upload, mkdir, server-side move, delete.
    fs::write(f.tmp.join("up.txt"), "uploaded").unwrap();
    oc_vfs::ops::mkdir(&root, "new/nested").unwrap();
    fn cb<'a>(c: &'a mut dyn FnMut(&str, &str) -> Conflict) -> TransferCallbacks<'a> {
        TransferCallbacks { on_conflict: c, on_progress: None, is_cancelled: &|| false }
    }
    let mut on_conflict = never;
    oc_local::copy(
        &[oc_local::local(f.tmp.join("up.txt"))],
        &root.child(&["new", "nested"]),
        false,
        cb(&mut on_conflict),
    )
    .unwrap();
    assert_eq!(fs::read_to_string(f.server.root.join("new/nested/up.txt")).unwrap(), "uploaded");
    oc_local::copy(&[root.child(&["new", "nested", "up.txt"])], &root, true, cb(&mut on_conflict)).unwrap();
    assert!(f.server.root.join("up.txt").exists() && !f.server.root.join("new/nested/up.txt").exists());
    oc_vfs::ops::delete(&[root.child(&["new"]), root.child(&["up.txt"])]).unwrap();
    assert!(!f.server.root.join("new").exists() && !f.server.root.join("up.txt").exists());
    assert!(!remote.capabilities().can_trash);
    remote.close();
}

#[test]
fn archive_on_server() {
    let f = fixture();
    {
        let file = fs::File::create(f.server.root.join("pack.zip")).unwrap();
        let mut z = zip::ZipWriter::new(file);
        z.start_file("inside.txt", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(b"zipped").unwrap();
        z.finish().unwrap();
    }
    let remote: Arc<dyn FileSystem> = f.connect();
    let pack = oc_archive::open_archive(&VPath::new(remote.clone(), "/pack.zip")).unwrap();
    assert_eq!(names(&pack.list_dir(ListOptions::default()).unwrap()), ["inside.txt"]);
    let mut text = String::new();
    pack.child(&["inside.txt"]).open_read().unwrap().read_to_string(&mut text).unwrap();
    assert_eq!(text, "zipped");
    assert_eq!(pack.parent(), Some(VPath::new(remote.clone(), "/")));
    assert!(pack.local_path().is_none());
    remote.close();
}

#[test]
fn filesystem_contract() {
    let f = fixture();
    fs::create_dir(f.server.root.join("contract")).unwrap();
    let remote: Arc<dyn FileSystem> = f.connect();
    oc_vfs::testing::check_filesystem(&VPath::new(remote.clone(), "/contract"));
    remote.close();
}

#[test]
fn filesystem_contract_with_short_reads_and_limits() {
    // A server answering reads with at most 1000 bytes, and advertising limits.
    let f = fixture_with(ServerOptions {
        max_read: Some(1000),
        limits: Some((64 * 1024, 16 * 1024)),
        ..Default::default()
    });
    fs::create_dir(f.server.root.join("contract")).unwrap();
    let remote: Arc<dyn FileSystem> = f.connect();
    oc_vfs::testing::check_filesystem(&VPath::new(remote.clone(), "/contract"));
    let mut data = Vec::new();
    remote.open_read("/big.bin").unwrap().read_to_end(&mut data).unwrap();
    assert!(data == fs::read(f.server.root.join("big.bin")).unwrap());
    remote.close();
}

#[test]
fn rename_without_posix_rename_extension() {
    let f = fixture_with(ServerOptions { posix_rename: false, ..Default::default() });
    let remote = f.connect();
    remote.rename("/big.bin", "/moved.bin").unwrap();
    assert!(f.server.root.join("moved.bin").exists() && !f.server.root.join("big.bin").exists());
    remote.close();
}

#[test]
fn seeking_reads() {
    let f = fixture();
    let expected = fs::read(f.server.root.join("big.bin")).unwrap();
    let remote = f.connect();
    let mut r = remote.open_read("/big.bin").unwrap();
    assert!(r.seekable());
    let mut buf = [0u8; 300];
    // A deterministic pseudo-random walk: forward and backward jumps, tail reads.
    let mut x: u64 = 12345;
    for _ in 0..200 {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let pos = (x >> 33) % (expected.len() as u64 + 100);
        let got = r.seek(SeekFrom::Start(pos)).unwrap();
        assert_eq!(got, pos);
        let n = r.read(&mut buf).unwrap();
        let start = (pos as usize).min(expected.len());
        let end = (start + n).min(expected.len());
        assert_eq!(&buf[..n], &expected[start..end], "at {pos}");
        if pos as usize >= expected.len() {
            assert_eq!(n, 0);
        }
        // Small forward skip inside the read-ahead.
        let at = r.seek(SeekFrom::Current(1000)).unwrap() as usize;
        assert_eq!(at, pos as usize + n + 1000);
        let n = r.read(&mut buf).unwrap();
        let start = at.min(expected.len());
        assert_eq!(&buf[..n], &expected[start..(start + n).min(expected.len())], "at {at}");
    }
    // Sequential read after seeking to the end minus 10.
    assert_eq!(r.seek(SeekFrom::End(-10)).unwrap(), expected.len() as u64 - 10);
    let mut tail = Vec::new();
    r.read_to_end(&mut tail).unwrap();
    assert_eq!(tail, &expected[expected.len() - 10..]);
    assert!(r.seek(SeekFrom::Current(-(expected.len() as i64) - 1)).is_err());
    // Stream position doesn't disturb read-ahead.
    r.seek(SeekFrom::Start(5)).unwrap();
    let mut head = vec![0u8; 10];
    r.read_exact(&mut head).unwrap();
    assert_eq!(r.stream_position().unwrap(), 15);
    assert_eq!(head, &expected[5..15]);
    drop(r);
    remote.close();
}

#[test]
fn forward_skips_within_read_ahead() {
    let f = fixture();
    let expected = fs::read(f.server.root.join("big.bin")).unwrap();
    let remote = f.connect();
    let mut r = remote.open_read("/big.bin").unwrap();
    let mut buf = vec![0u8; 4000];
    let mut pos = 0usize;
    // Read and skip alternately (like parsing a container format).
    while pos < expected.len() {
        let n = r.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        assert_eq!(&buf[..n], &expected[pos..pos + n], "at {pos}");
        pos += n;
        let skip = 70_000.min(expected.len() - pos);
        r.seek(SeekFrom::Current(skip as i64)).unwrap();
        pos += skip;
    }
    assert_eq!(pos, expected.len());
    remote.close();
}

#[cfg(unix)]
#[test]
fn symlinks_like_python() {
    use std::os::unix::fs::symlink;
    let f = fixture();
    symlink("docs", f.server.root.join("docs-link")).unwrap();
    symlink("big.bin", f.server.root.join("big-link")).unwrap();
    symlink("missing", f.server.root.join("dangling")).unwrap();
    let remote = f.connect();
    let entries = remote.list_dir("/", ListOptions { show_hidden: true, ..Default::default() }).unwrap();
    let get = |n: &str| entries.iter().find(|e| e.name == n).unwrap().clone();
    let d = get("docs-link");
    assert!(d.is_dir && d.is_link && d.size == 0);
    let b = get("big-link");
    assert!(!b.is_dir && b.is_link && b.size == 1024 * 1024);
    let x = get("dangling");
    assert!(!x.is_dir && x.is_link);
    let st = remote.stat("/docs-link").unwrap();
    assert!(st.is_dir && st.is_link && st.name == "docs-link");
    // A walk copies directory links as link items and doesn't follow them.
    let walked: Vec<_> = remote.walk("/").unwrap().map(|r| r.unwrap()).collect();
    let link = walked.iter().find(|(rel, _)| rel == &["docs-link"]).unwrap();
    assert!(!link.1.is_dir && link.1.is_link);
    assert!(!walked.iter().any(|(rel, _)| rel.first().is_some_and(|r| r == "docs-link") && rel.len() > 1));
    // Removing a directory link removes the link, not the target.
    remote.remove("/docs-link").unwrap();
    assert!(f.server.root.join("docs/readme.txt").exists());
    remote.close();
}

#[test]
fn errors_name_the_path() {
    let f = fixture();
    let remote = f.connect();
    let err = remote.stat("/nope").unwrap_err();
    assert!(err.is_not_found(), "{err:?}");
    let err = remote.open_read("/nope").err().unwrap();
    assert!(err.is_not_found(), "{err:?}");
    match remote.mkdir("/docs", false) {
        Err(VfsError::AlreadyExists(p)) => assert_eq!(p, format!("sftp://{USER}@127.0.0.1:{}/docs", f.server.port)),
        other => panic!("{other:?}"),
    }
    remote.mkdir("/docs", true).unwrap();
    let err = remote.list_dir("/missing", ListOptions::default()).unwrap_err();
    assert!(err.is_not_found());
    assert_eq!(remote.read_only_reason(), format!("sftp://{USER}@127.0.0.1:{} refused the operation", f.server.port));
    remote.close();
}

#[test]
fn concurrent_listings_and_reads() {
    let f = fixture();
    for i in 0..20 {
        fs::write(f.server.root.join("docs").join(format!("f{i}.txt")), format!("file {i}")).unwrap();
    }
    let expected = fs::read(f.server.root.join("big.bin")).unwrap();
    let remote = f.connect();
    std::thread::scope(|s| {
        for t in 0..8 {
            let remote = remote.clone();
            let expected = &expected;
            s.spawn(move || {
                for _ in 0..5 {
                    let list = remote.list_dir("/docs", ListOptions::default()).unwrap();
                    assert_eq!(list.len(), 21);
                    if t % 2 == 0 {
                        let mut data = Vec::new();
                        remote.open_read("/big.bin").unwrap().read_to_end(&mut data).unwrap();
                        assert!(data == *expected);
                    } else {
                        let mut w = remote.open_write(&format!("/w{t}.bin")).unwrap();
                        w.write_all(&expected[..300_000]).unwrap();
                        w.finish().unwrap();
                    }
                }
            });
        }
    });
    remote.close();
}

#[test]
fn write_errors_surface() {
    let f = fixture();
    let remote = f.connect();
    // Writing into a missing folder fails at open.
    let err = remote.open_write("/no/such/dir/x").err().unwrap();
    assert!(err.is_not_found(), "{err:?}");
    // A dropped, unfinished writer doesn't hang or break the connection.
    let mut w = remote.open_write("/partial.bin").unwrap();
    w.write_all(&[1u8; 200_000]).unwrap();
    drop(w);
    assert!(remote.list_dir("/", ListOptions::default()).is_ok());
    remote.close();
}

#[test]
fn closing_and_lost_connections_are_noticed() {
    let f = fixture();
    let remote = f.connect();
    assert!(remote.is_connected());
    remote.close();
    assert!(!remote.is_connected());
    let err = remote.list_dir("/", ListOptions::default()).unwrap_err();
    assert!(err.to_string().contains("was lost"), "{err}");

    let remote = f.connect();
    let port = f.server.port;
    let Fixture { server, _dir, .. } = f;
    server.stop();
    let deadline = Instant::now() + Duration::from_secs(10);
    while remote.is_connected() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!remote.is_connected(), "a dead server must be noticed");
    let started = Instant::now();
    let err = remote.list_dir("/", ListOptions::default()).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(err.to_string(), format!("Connection to {USER}@127.0.0.1:{port} was lost"));
}

#[test]
fn home_and_start_path() {
    let f = fixture();
    let remote = f.connect();
    assert_eq!(remote.home(), "/");
    assert_eq!(remote.label(), format!("{USER}@127.0.0.1:{}", f.server.port));
    remote.close();
}

#[test]
fn streams_share_channels_when_the_server_allows_no_more() {
    // OpenSSH refuses more than MaxSessions (10) channels per connection;
    // here 3: the metadata channel and two for streams.
    let f = fixture_with(ServerOptions { max_sessions: Some(3), ..Default::default() });
    let expected = fs::read(f.server.root.join("big.bin")).unwrap();
    let remote = f.connect();
    let mut readers: Vec<_> = (0..6).map(|i| remote.open_read("/big.bin").unwrap_or_else(|e| panic!("{i}: {e}"))).collect();
    let mut writers: Vec<_> =
        (0..3).map(|i| remote.open_write(&format!("/w{i}.bin")).unwrap_or_else(|e| panic!("w{i}: {e}"))).collect();
    let mut got = vec![Vec::new(); readers.len()];
    let mut buf = vec![0u8; 50_000];
    let mut rounds = 0u64;
    // Interleaved, so requests of several streams are in flight on a shared channel.
    loop {
        let mut progressed = false;
        for (r, out) in readers.iter_mut().zip(got.iter_mut()) {
            let n = r.read(&mut buf).unwrap();
            out.extend_from_slice(&buf[..n]);
            progressed |= n > 0;
        }
        for w in &mut writers {
            w.write_all(&expected[..20_000]).unwrap();
        }
        rounds += 1;
        if !progressed {
            break;
        }
    }
    assert!(got.iter().all(|g| *g == expected));
    for w in writers {
        w.finish().unwrap();
    }
    drop(readers);
    assert_eq!(fs::metadata(f.server.root.join("w0.bin")).unwrap().len(), 20_000 * rounds);
    assert!(remote.list_dir("/", ListOptions::default()).is_ok());
    let mut again = Vec::new();
    remote.open_read("/docs/readme.txt").unwrap().read_to_end(&mut again).unwrap();
    assert_eq!(again, b"hello from sftp");
    remote.close();

    // A server allowing a single channel: streams share the metadata channel.
    let f = fixture_with(ServerOptions { max_sessions: Some(1), ..Default::default() });
    let remote = f.connect();
    let mut a = remote.open_read("/big.bin").unwrap();
    let mut b = remote.open_read("/big.bin").unwrap();
    let mut w = remote.open_write("/one.bin").unwrap();
    w.write_all(&expected[..100_000]).unwrap();
    let (mut da, mut db) = (Vec::new(), Vec::new());
    a.read_to_end(&mut da).unwrap();
    assert_eq!(remote.list_dir("/docs", ListOptions::default()).unwrap().len(), 1);
    b.read_to_end(&mut db).unwrap();
    w.finish().unwrap();
    assert!(da == expected && db == expected);
    assert_eq!(fs::read(f.server.root.join("one.bin")).unwrap(), &expected[..100_000]);
    drop((a, b));
    assert!(remote.list_dir("/", ListOptions::default()).is_ok());
    remote.close();
}

#[test]
fn bogus_names_from_the_server_are_skipped() {
    // A broken or hostile server must not make a download write outside its
    // target ("../x"), nor walks and recursive removes loop ("", ".").
    let f = fixture_with(ServerOptions {
        bogus_names: ["", ".", "..", "../escape", "a/b", "/abs"].map(String::from).to_vec(),
        ..Default::default()
    });
    let remote = f.connect();
    let all = remote.list_dir("/", ListOptions { show_hidden: true, ..Default::default() }).unwrap();
    assert_eq!(names(&all), ["docs", ".profile", "big.bin"].map(String::from));
    let walked: Vec<_> = remote.walk("/docs").unwrap().map(|r| r.unwrap().0.join("/")).collect();
    assert_eq!(walked, ["", "readme.txt"]);
    remote.remove("/docs").unwrap();
    assert!(!f.server.root.join("docs").exists());
    remote.close();
}

#[test]
fn a_dropped_writer_has_closed_its_file() {
    let f = fixture();
    let remote = f.connect();
    let mut w = remote.open_write("/partial.bin").unwrap();
    w.write_all(&vec![1u8; 3_000_000]).unwrap();
    drop(w);
    // Everything sent has landed and the handle is closed by now (the
    // in-process server uses 32 KiB chunks; the unsent rest is dropped).
    let len = fs::metadata(f.server.root.join("partial.bin")).unwrap().len();
    assert_eq!(len, 3_000_000 / 32_768 * 32_768);
    remote.remove("/partial.bin").unwrap();
    // The channel went back to the pool in working order.
    let mut w = remote.open_write("/partial.bin").unwrap();
    w.write_all(b"fresh").unwrap();
    w.finish().unwrap();
    assert_eq!(fs::read(f.server.root.join("partial.bin")).unwrap(), b"fresh");
    remote.close();
}

#[test]
fn local_network_hint_on_macos() {
    use oc_sftp::describe_connect_error;
    let hint = |host: &str, unreachable: bool| {
        describe_connect_error(host, 22, &"No route to host", unreachable).contains("Local Network")
    };
    assert_eq!(hint("192.168.1.20", true), cfg!(target_os = "macos"));
    assert!(!hint("8.8.8.8", true));
    assert!(!hint("192.168.1.2", false));
    assert_eq!(
        describe_connect_error("h", 2222, &"the connection was refused", false),
        "Can't connect to h:2222: the connection was refused"
    );
}
