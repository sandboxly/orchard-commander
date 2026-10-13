//! Throughput check against a real server:
//! `cargo run --release -p oc-sftp --example sftp_bench -- HOST PORT USER KEYFILE REMOTE_DIR [MiB]`
//! (trusts the host key; key-file login, no password).

use std::io::{Read, Write};
use std::time::Instant;

use oc_sftp::{Agent, ConnectOptions, connect};
use oc_vfs::FileSystem;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [host, port, user, key, dir, rest @ ..] = &args[..] else {
        eprintln!("usage: sftp_bench HOST PORT USER KEYFILE REMOTE_DIR [MiB]");
        std::process::exit(2);
    };
    let mib: usize = rest.first().and_then(|m| m.parse().ok()).unwrap_or(64);
    let known_hosts = std::env::temp_dir().join("oc-sftp-bench-known_hosts");
    let mut o = ConnectOptions::new(host.clone(), known_hosts);
    o.port = port.parse().expect("port");
    o.username = Some(user.clone());
    o.key_file = Some(key.into());
    o.agent = Agent::Off;
    let fs = connect(&o, &|_| true).unwrap_or_else(|e| panic!("connect: {e}"));
    let data: Vec<u8> =
        (0..mib * 1024 * 1024).map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[3]).collect();
    let path = format!("{dir}/oc-bench.bin");

    let t = Instant::now();
    let mut w = fs.open_write(&path).unwrap();
    for chunk in data.chunks(1024 * 1024) {
        w.write_all(chunk).unwrap();
    }
    w.finish().unwrap();
    let up = t.elapsed().as_secs_f64();

    let t = Instant::now();
    let mut back = Vec::with_capacity(data.len());
    fs.open_read(&path).unwrap().read_to_end(&mut back).unwrap();
    let down = t.elapsed().as_secs_f64();
    assert!(back == data, "read back differs");
    fs.remove(&path).unwrap();
    let mb = data.len() as f64 / 1e6;
    println!("oc-sftp {mib} MiB: upload {:.0} MB/s, download {:.0} MB/s", mb / up, mb / down);
}
