//! On a slow link, a running download or upload must not hold back a
//! listing for long (or make it time out): what a stream keeps in flight
//! is bounded by the time it takes to transfer, not just by bytes.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use oc_sftp::testing::{PASSWORD, USER, start_sftp_server};
use oc_sftp::{Agent, ConnectOptions, connect};
use oc_vfs::{FileSystem, ListOptions};

/// A TCP proxy to `target` passing at most `rate` bytes/s each way.
fn throttled_proxy(target: u16, rate: usize) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(client) = client else { return };
            let Ok(server) = TcpStream::connect(("127.0.0.1", target)) else { return };
            for (mut from, mut to) in
                [(client.try_clone().unwrap(), server.try_clone().unwrap()), (server, client)]
            {
                std::thread::spawn(move || {
                    let mut buf = vec![0u8; rate / 20];
                    loop {
                        let started = Instant::now();
                        let n = match from.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        if to.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        let due = Duration::from_secs_f64(n as f64 / rate as f64);
                        if let Some(rest) = due.checked_sub(started.elapsed()) {
                            std::thread::sleep(rest);
                        }
                    }
                    let _ = to.shutdown(Shutdown::Write);
                });
            }
        }
    });
    port
}

#[test]
fn listing_during_transfers_on_a_slow_link() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().canonicalize().unwrap();
    let root = tmp.join("remote");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("big.bin"), vec![5u8; 64 * 1024 * 1024]).unwrap();
    let server = start_sftp_server(&root);
    let rate = 256 * 1024;
    let mut o = ConnectOptions::new("127.0.0.1", tmp.join("known_hosts"));
    o.port = throttled_proxy(server.port, rate);
    o.username = Some(USER.into());
    o.password = Some(PASSWORD.into());
    o.agent = Agent::Off;
    o.home = Some(tmp.join("home"));
    let remote = connect(&o, &|_| true).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let download = {
        let (remote, stop) = (remote.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut r = remote.open_read("/big.bin").unwrap();
            let mut buf = vec![0u8; 64 * 1024];
            while !stop.load(Ordering::Relaxed) && r.read(&mut buf).unwrap() > 0 {}
        })
    };
    let upload = {
        let (remote, stop) = (remote.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut w = remote.open_write("/up.bin").unwrap();
            let buf = vec![7u8; 64 * 1024];
            while !stop.load(Ordering::Relaxed) {
                w.write_all(&buf).unwrap();
            }
        })
    };
    std::thread::sleep(Duration::from_secs(4)); // the transfers get going
    for _ in 0..2 {
        let started = Instant::now();
        let listed = remote.list_dir("/", ListOptions::default());
        let took = started.elapsed();
        assert!(listed.is_ok(), "{listed:?}");
        eprintln!("listing took {took:?}");
        // Unbounded (8 MiB per stream at 256 KiB/s) it is over a minute; a
        // second's worth each way is a few seconds, with headroom here for
        // slow shared CI runners (whose sleeps make the proxy slower still).
        assert!(took < Duration::from_secs(15), "a listing took {took:?} behind the transfers");
    }
    stop.store(true, Ordering::Relaxed);
    download.join().unwrap();
    upload.join().unwrap();
    remote.close();
}
