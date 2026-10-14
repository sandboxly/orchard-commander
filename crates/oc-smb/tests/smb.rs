//! SMB file system against real servers (port of `tests/test_smb.py` and the
//! SMB case of `tests/test_fs_contract.py`):
//!
//! - impacket's pure-Python SMB2 server (read-only use)
//! - Samba in Docker (full read/write; skipped without Docker)

mod common;

use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use common::{PASSWORD, USER, start_impacket, start_samba};
use oc_smb::{ConnectOptions, SmbConnectError};
use oc_vfs::{Conflict, FileSystem, ListOptions, TransferCallbacks, VPath};

fn never(src: &str, dst: &str) -> Conflict {
    panic!("unexpected conflict {src} -> {dst}")
}

fn callbacks<'a>(on_conflict: &'a mut dyn FnMut(&str, &str) -> Conflict) -> TransferCallbacks<'a> {
    TransferCallbacks { on_conflict, on_progress: None, is_cancelled: &|| false }
}

fn all() -> ListOptions {
    ListOptions { show_hidden: true, ..ListOptions::default() }
}

fn mtime_of(path: &std::path::Path) -> f64 {
    std::fs::metadata(path).unwrap().modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
}

// -- impacket: listing and reading ------------------------------------------------------

fn impacket_tree() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("docs")).unwrap();
    std::fs::write(root.path().join("docs/readme.txt"), "hello from smb").unwrap();
    std::fs::write(root.path().join("big.bin"), (0..=255u8).collect::<Vec<_>>().repeat(4096)).unwrap();
    root
}

#[test]
fn wrong_password() {
    let root = impacket_tree();
    let Some(server) = start_impacket(root.path()) else {
        return;
    };
    match server.connect_with("nope") {
        Err(SmbConnectError::AuthenticationRequired(msg)) => {
            assert_eq!(msg, "The server 127.0.0.1 didn't accept the user name or password for tester")
        }
        Err(other) => panic!("expected AuthenticationRequired, got {other:?}"),
        Ok(_) => panic!("connected with a wrong password"),
    }
}

#[test]
fn browse_and_download() {
    let root_dir = impacket_tree();
    let Some(server) = start_impacket(root_dir.path()) else {
        return;
    };
    let remote: Arc<dyn FileSystem> = Arc::new(server.connect().unwrap());
    let root = VPath::new(remote.clone(), "/");
    assert!(root.parent().is_none());
    let listing = root.list_dir(ListOptions::default()).unwrap();
    assert_eq!(listing.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["docs", "big.bin"]);
    let big = &listing[1];
    assert_eq!(big.size, 1024 * 1024);
    assert!((big.mtime - mtime_of(&root_dir.path().join("big.bin"))).abs() < 2.0, "no timezone shift");
    let docs = root.child(&["docs"]);
    assert!(docs.is_dir());
    assert_eq!(docs.display(), format!("smb://127.0.0.1:{}/SHARE/docs", server.port));
    assert!(!root.child(&["missing.txt"]).exists());

    let cache = tempfile::tempdir().unwrap();
    let readme = oc_vfs::ops::materialize(&docs.child(&["readme.txt"]), cache.path()).unwrap();
    assert_eq!(std::fs::read_to_string(readme).unwrap(), "hello from smb");

    let out = tempfile::tempdir().unwrap();
    let mut on_conflict = never;
    oc_vfs::ops::copy(&[docs, root.child(&["big.bin"])], &oc_local::local(out.path()), callbacks(&mut on_conflict))
        .unwrap();
    assert_eq!(std::fs::read_to_string(out.path().join("docs/readme.txt")).unwrap(), "hello from smb");
    assert_eq!(
        std::fs::read(out.path().join("big.bin")).unwrap(),
        std::fs::read(root_dir.path().join("big.bin")).unwrap()
    );

    // Ranged, seekable reads.
    let mut f = remote.open_read("/big.bin").unwrap();
    f.seek(SeekFrom::Start(1000)).unwrap();
    let mut buf = [0u8; 4];
    f.read_exact(&mut buf).unwrap();
    assert_eq!(buf, [232, 233, 234, 235]);
    f.seek(SeekFrom::End(-2)).unwrap();
    let mut tail = Vec::new();
    f.read_to_end(&mut tail).unwrap();
    assert_eq!(tail, [254, 255]);
    remote.close();
}

#[test]
fn unreachable_server() {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let opts = ConnectOptions {
        port,
        username: Some(USER.into()),
        password: Some(PASSWORD.into()),
        timeout: Duration::from_secs(5),
    };
    match oc_smb::connect("127.0.0.1", "SHARE", &opts) {
        Err(SmbConnectError::Connection(msg)) => {
            assert_eq!(
                msg,
                format!(
                    "Can't connect to 127.0.0.1:{port}: the connection was refused — check the port and that the server is running"
                )
            )
        }
        other => panic!("expected a connection error, got {:?}", other.err()),
    }
}

/// A fake server on a local port: `reply` gets each accepted socket.
fn fake_server(reply: fn(std::net::TcpStream)) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || reply(stream));
        }
    });
    port
}

fn connect_to(port: u16) -> (Result<oc_smb::SmbFileSystem, SmbConnectError>, Duration) {
    let opts = ConnectOptions {
        port,
        username: Some(USER.into()),
        password: Some(PASSWORD.into()),
        timeout: Duration::from_secs(2),
    };
    let started = std::time::Instant::now();
    (oc_smb::connect("127.0.0.1", "SHARE", &opts), started.elapsed())
}

/// A server that accepts the connection and then never answers must not
/// hang the connect.
#[test]
fn silent_server_times_out() {
    let port = fake_server(|stream| {
        std::thread::sleep(Duration::from_secs(120));
        drop(stream);
    });
    let (result, took) = connect_to(port);
    match result {
        Err(SmbConnectError::Connection(msg)) => {
            assert!(msg.starts_with(&format!("Can't connect to 127.0.0.1:{port}: ")), "{msg}")
        }
        other => panic!("expected a connection error, got {:?}", other.err()),
    }
    // Bounded by the connect timeout (2 s here), as Python's was.
    assert!(took < Duration::from_secs(8), "took {took:?}");
}

/// Garbage instead of SMB: an error, no panic, no hang.
#[test]
fn garbage_server_fails_cleanly() {
    let port = fake_server(|mut stream| {
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        // A NetBIOS length prefix promising a frame, then nonsense; then a
        // well-formed length with a truncated SMB2 header.
        let _ = stream.write_all(&[0, 0, 0, 80, 0xFE, b'S', b'M', b'B', 64, 0, 1, 0]);
        let _ = stream.write_all(&[0xde, 0xad, 0xbe, 0xef].repeat(17));
        std::thread::sleep(Duration::from_secs(5));
    });
    let (result, took) = connect_to(port);
    assert!(matches!(result, Err(SmbConnectError::Connection(_))), "{:?}", result.err());
    assert!(took < Duration::from_secs(8), "took {took:?}");
}

// -- Samba: full read/write ---------------------------------------------------------------

#[test]
fn samba_contract() {
    let Some(server) = start_samba(None) else {
        return;
    };
    let remote: Arc<dyn FileSystem> = Arc::new(server.connect().unwrap());
    oc_vfs::testing::check_filesystem(&VPath::new(remote.clone(), "/"));
    remote.close();
}

#[test]
fn samba_write_operations() {
    let Some(server) = start_samba(None) else {
        return;
    };
    let remote: Arc<dyn FileSystem> = Arc::new(server.connect().unwrap());
    let root = VPath::new(remote.clone(), "/");
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("up.txt"), "uploaded").unwrap();
    let mut on_conflict = never;

    oc_vfs::ops::mkdir(&root, "new/nested").unwrap();
    oc_vfs::ops::copy(
        &[oc_local::local(tmp.path().join("up.txt"))],
        &root.child(&["new", "nested"]),
        callbacks(&mut on_conflict),
    )
    .unwrap();
    let uploaded = root.child(&["new", "nested", "up.txt"]);
    assert_eq!(uploaded.stat().unwrap().size, 8);
    assert!((uploaded.stat().unwrap().mtime - mtime_of(&tmp.path().join("up.txt"))).abs() < 2.0, "mtime preserved");

    oc_vfs::ops::move_to(std::slice::from_ref(&uploaded), &root, callbacks(&mut on_conflict)).unwrap(); // server-side rename
    assert!(root.child(&["up.txt"]).exists() && !uploaded.exists());
    let cache = tempfile::tempdir().unwrap();
    let local = oc_vfs::ops::materialize(&root.child(&["up.txt"]), cache.path()).unwrap();
    assert_eq!(std::fs::read_to_string(local).unwrap(), "uploaded");

    // Names with spaces and non-ASCII characters survive the round trip.
    let odd = root.child(&["Grüße & spaß.txt"]);
    let mut w = remote.open_write(&odd.path).unwrap();
    w.write_all("ü".as_bytes()).unwrap();
    w.finish().unwrap();
    assert!(root.list_dir(all()).unwrap().iter().any(|e| e.name == "Grüße & spaß.txt"));

    // Errors read like local ones.
    let err = remote.stat("/missing.txt").unwrap_err();
    assert!(err.is_not_found());
    assert_eq!(err.to_string(), r"\\127.0.0.1\SHARE\missing.txt not found");
    let err = remote.mkdir("/new", false).unwrap_err();
    assert!(err.to_string().ends_with(r"\new already exists"), "{err}");
    let err = remote.rename("/up.txt", "/new").unwrap_err();
    assert!(err.to_string().ends_with(" already exists"), "{err}");

    // A file being previewed (open for reading) can still be replaced.
    let mut reader = remote.open_read("/up.txt").unwrap();
    let mut w = remote.open_write("/up.txt").unwrap();
    w.write_all(b"replaced!").unwrap();
    w.finish().unwrap();
    let mut first = [0u8; 1];
    reader.read_exact(&mut first).unwrap();
    drop(reader);
    assert_eq!(remote.stat("/up.txt").unwrap().size, 9);

    // A failed copy leaves nothing behind (the writer releases its handle).
    let w = remote.open_write("/partial.bin").unwrap();
    drop(w);
    remote.remove("/partial.bin").unwrap();

    oc_vfs::ops::delete(&[root.child(&["new"]), root.child(&["up.txt"]), odd]).unwrap();
    assert_eq!(root.list_dir(all()).unwrap(), []);
    remote.close();
    assert!(remote.list_dir("/", all()).is_err(), "closed file system must refuse requests");
}

#[test]
fn samba_wrong_password_and_missing_share() {
    let Some(server) = start_samba(None) else {
        return;
    };
    assert!(matches!(server.connect_with("nope"), Err(SmbConnectError::AuthenticationRequired(_))));
    let opts = ConnectOptions {
        port: server.port,
        username: Some(USER.into()),
        password: Some(PASSWORD.into()),
        timeout: Duration::from_secs(10),
    };
    match oc_smb::connect(&server.host, "NOPE", &opts) {
        Err(SmbConnectError::Connection(msg)) => assert_eq!(
            msg,
            format!("Can't connect to 127.0.0.1:{}: the share doesn't exist — check the share name", server.port)
        ),
        other => panic!("expected a connection error, got {:?}", other.err()),
    }
    let shares = oc_smb::list_shares(&server.host, &opts).unwrap();
    assert!(shares.iter().any(|s| s.eq_ignore_ascii_case("share")), "{shares:?}");
}

/// Listing while files are read (preview + copy of the same file) must not
/// run out of credits on a server granting few, like many NAS boxes.
#[test]
fn concurrent_use_with_few_credits() {
    let Some(server) = start_samba(Some(16)) else {
        return;
    };
    let remote: Arc<dyn FileSystem> = Arc::new(server.connect().unwrap());
    let data: Vec<u8> = (0..8 * 1024 * 1024u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    let mut w = remote.open_write("/movie.mp4").unwrap();
    w.write_all(&data).unwrap();
    w.finish().unwrap();

    let read_all = {
        let remote = remote.clone();
        move || -> Vec<u8> {
            let mut f = remote.open_read("/movie.mp4").unwrap(); // two readers of one file at once
            let mut out = Vec::new();
            f.read_to_end(&mut out).unwrap();
            out
        }
    };
    let list_repeatedly = {
        let remote = remote.clone();
        move || {
            for _ in 0..50 {
                remote.list_dir("/", all()).unwrap();
                remote.stat("/movie.mp4").unwrap();
            }
        }
    };
    let readers = [std::thread::spawn(read_all.clone()), std::thread::spawn(read_all)];
    let listers = [std::thread::spawn(list_repeatedly.clone()), std::thread::spawn(list_repeatedly)];
    for t in listers {
        t.join().expect("listing failed");
    }
    for t in readers {
        assert!(t.join().expect("reading failed") == data);
    }

    // And an upload running alongside them.
    let upload = {
        let remote = remote.clone();
        let data = data.clone();
        std::thread::spawn(move || {
            let mut w = remote.open_write("/copy.mp4").unwrap();
            for chunk in data.chunks(1024 * 1024) {
                w.write_all(chunk).unwrap();
            }
            w.finish().unwrap();
        })
    };
    for _ in 0..20 {
        remote.list_dir("/", all()).unwrap();
    }
    upload.join().expect("upload failed");
    let mut copy = Vec::new();
    remote.open_read("/copy.mp4").unwrap().read_to_end(&mut copy).unwrap();
    assert!(copy == data);
    remote.close();
}

/// Throughput of streamed uploads and downloads (run with --ignored --nocapture).
#[test]
#[ignore = "benchmark"]
fn samba_throughput() {
    let Some(server) = start_samba(None) else {
        return;
    };
    let remote: Arc<dyn FileSystem> = Arc::new(server.connect().unwrap());
    let data: Vec<u8> = (0..128 * 1024 * 1024u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    let mb = data.len() as f64 / 1e6;
    for _ in 0..3 {
        let started = std::time::Instant::now();
        let mut w = remote.open_write("/big.bin").unwrap();
        for chunk in data.chunks(oc_vfs::CHUNK) {
            w.write_all(chunk).unwrap();
        }
        w.finish().unwrap();
        let up = started.elapsed();
        let started = std::time::Instant::now();
        let mut r = remote.open_read("/big.bin").unwrap();
        let mut buf = vec![0u8; oc_vfs::CHUNK];
        let mut total = 0;
        loop {
            let n = r.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        let down = started.elapsed();
        assert_eq!(total, data.len());
        eprintln!("upload {:.0} MB/s, download {:.0} MB/s", mb / up.as_secs_f64(), mb / down.as_secs_f64());
    }
}

/// A NAS that drops the connection (reboot, Wi-Fi roam): the file system
/// comes back by itself, also when the next request is not a listing (the
/// library replays only idempotent requests, so writes used to keep failing
/// with "Disconnected" until something listed).
#[test]
fn samba_dropped_connection_recovers() {
    let Some(server) = start_samba(None) else {
        return;
    };
    let remote: Arc<dyn FileSystem> = Arc::new(server.connect().unwrap());
    let mut w = remote.open_write("/a.txt").unwrap();
    w.write_all(b"before").unwrap();
    w.finish().unwrap();
    let reader = remote.open_read("/a.txt").unwrap(); // its handle dies with the connection

    server.drop_client_connections();
    std::thread::sleep(Duration::from_millis(300)); // the client sees the reset
    remote.mkdir("/d", false).unwrap();
    server.drop_client_connections();
    std::thread::sleep(Duration::from_millis(300));
    let mut w = remote.open_write("/d/b.txt").unwrap();
    w.write_all(b"after").unwrap();
    w.finish().unwrap();
    server.drop_client_connections();
    std::thread::sleep(Duration::from_millis(300));
    remote.rename("/a.txt", "/c.txt").unwrap();
    server.drop_client_connections();
    std::thread::sleep(Duration::from_millis(300));
    remote.remove("/c.txt").unwrap();
    drop(reader); // closing a handle of the dead session is harmless

    let mut text = String::new();
    remote.open_read("/d/b.txt").unwrap().read_to_string(&mut text).unwrap();
    assert_eq!(text, "after");
    assert_eq!(remote.list_dir("/", all()).unwrap().iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["d"]);
    remote.close();
}
