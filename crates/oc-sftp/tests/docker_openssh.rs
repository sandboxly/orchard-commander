//! Against a real OpenSSH server (linuxserver/openssh-server in Docker):
//! the contract, large pipelined transfers, OpenSSH's limits and
//! posix-rename extensions, symlinks, MaxAuthTries with an agent full of keys,
//! and noticing a server that went away. Skips cleanly without Docker.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_provider::ConnectError;
use oc_sftp::{Agent, ConnectOptions, SftpFileSystem, connect};
use oc_vfs::{FileSystem, ListOptions, VPath};

const IMAGE: &str = "lscr.io/linuxserver/openssh-server:latest";

struct Container {
    id: String,
    port: u16,
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.id]).output();
    }
}

impl Container {
    fn exec(&self, script: &str) {
        let out = Command::new("docker").args(["exec", &self.id, "sh", "-c", script]).output().unwrap();
        assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
    }
}

fn docker_available() -> bool {
    Command::new("docker").args(["info", "--format", "{{.ServerVersion}}"]).output().is_ok_and(|o| o.status.success())
}

fn start_container() -> Option<Container> {
    if !docker_available() {
        eprintln!("skipped: Docker is not available");
        return None;
    }
    let out = Command::new("docker")
        .args(["run", "-d", "--rm", "-p", "127.0.0.1::2222"])
        .args(["-e", "PUID=1000", "-e", "PGID=1000", "-e", "PASSWORD_ACCESS=true"])
        .args(["-e", "USER_NAME=tester", "-e", "USER_PASSWORD=s3cret", IMAGE])
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!("skipped: cannot start {IMAGE}: {}", String::from_utf8_lossy(&out.stderr));
        return None;
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let mapping = Command::new("docker").args(["port", &id, "2222"]).output().unwrap();
    let mapping = String::from_utf8_lossy(&mapping.stdout).to_string();
    let port = mapping.lines().next().and_then(|l| l.rsplit(':').next()).and_then(|p| p.trim().parse().ok());
    let container = Container { id, port: port.unwrap_or(0) };
    assert!(container.port != 0, "no port mapping: {mapping}");
    // Wait for sshd (it generates host keys on first start).
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let banner = TcpStream::connect(("127.0.0.1", container.port)).ok().and_then(|mut s| {
            s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
            let mut buf = [0u8; 8];
            s.read_exact(&mut buf).ok()?;
            Some(buf)
        });
        if banner.is_some_and(|b| b.starts_with(b"SSH-")) {
            break;
        }
        assert!(Instant::now() < deadline, "sshd did not come up");
        std::thread::sleep(Duration::from_millis(300));
    }
    Some(container)
}

fn options(c: &Container, home: &std::path::Path, known_hosts: &std::path::Path) -> ConnectOptions {
    let mut o = ConnectOptions::new("127.0.0.1", known_hosts);
    o.port = c.port;
    o.username = Some("tester".into());
    o.password = Some("s3cret".into());
    o.agent = Agent::Off;
    o.home = Some(home.to_path_buf());
    o.timeout = Duration::from_secs(10);
    o
}

#[test]
fn real_openssh_server() {
    let Some(c) = start_container() else { return };
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().canonicalize().unwrap();
    let kh = tmp.join("known_hosts");
    let home = tmp.join("home");

    let sftp = connect(&options(&c, &home, &kh), &|q| q.fingerprint.starts_with("SHA256:"))
        .unwrap_or_else(|e| panic!("connect: {e}"));
    assert_eq!(sftp.home(), "/config");
    let remote: Arc<dyn FileSystem> = sftp.clone();

    // The contract.
    remote.mkdir("/config/contract", false).unwrap();
    oc_vfs::testing::check_filesystem(&VPath::new(remote.clone(), "/config/contract"));

    // Large pipelined transfers.
    let data: Vec<u8> = (0..32 * 1024 * 1024u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    let started = Instant::now();
    let mut w = remote.open_write("/config/big.bin").unwrap();
    for chunk in data.chunks(1024 * 1024) {
        w.write_all(chunk).unwrap();
    }
    w.finish().unwrap();
    let up = started.elapsed();
    let started = Instant::now();
    let mut back = Vec::with_capacity(data.len());
    remote.open_read("/config/big.bin").unwrap().read_to_end(&mut back).unwrap();
    let down = started.elapsed();
    assert!(back == data, "download differs");
    let mb = data.len() as f64 / 1e6;
    eprintln!("32 MiB: upload {:.0} MB/s, download {:.0} MB/s", mb / up.as_secs_f64(), mb / down.as_secs_f64());
    assert_eq!(remote.stat("/config/big.bin").unwrap().size, data.len() as u64);

    // posix-rename replaces an existing target.
    let mut w = remote.open_write("/config/a.txt").unwrap();
    w.write_all(b"a").unwrap();
    w.finish().unwrap();
    remote.rename("/config/a.txt", "/config/big.bin").unwrap();
    assert_eq!(remote.stat("/config/big.bin").unwrap().size, 1);

    // Symlinks and a recursive remove.
    c.exec("mkdir -p /config/links/sub && echo hi > /config/links/sub/f && ln -s sub /config/links/dirlink && ln -s nowhere /config/links/dangling && chown -R tester /config/links");
    let entries = remote.list_dir("/config/links", ListOptions::default()).unwrap();
    let get = |n: &str| entries.iter().find(|e| e.name == n).unwrap();
    assert!(get("dirlink").is_dir && get("dirlink").is_link);
    assert!(!get("dangling").is_dir && get("dangling").is_link);
    remote.remove("/config/links").unwrap();
    assert!(!remote.exists("/config/links"));

    // Start folders through the provider.
    let provider = oc_sftp::SftpProvider;
    let params = oc_provider::Params::from_iter([("path".to_string(), serde_json::json!("contract/../x"))]);
    assert_eq!(oc_provider::FileSystemProvider::start_path(&provider, &params, &remote), "/config/x");
    remote.close();

    // MaxAuthTries (6 here): an agent with 8 keys must not lock out a password login…
    #[cfg(unix)]
    {
        let keys: Vec<_> = (0..8).map(|_| oc_sftp::testing::random_key()).collect();
        let agent = oc_sftp::testing::start_agent(&keys);
        let mut o = options(&c, &home, &kh);
        o.agent = Agent::Socket(agent.socket.clone());
        connect(&o, &|_| panic!("asked")).unwrap_or_else(|e| panic!("password login with an agent: {e}")).close();
        // …while without a password, the agent's keys are tried (and rejected).
        o.password = None;
        match connect(&o, &|_| panic!("asked")) {
            Err(ConnectError::AuthenticationRequired(msg)) => {
                assert_eq!(msg, "Authentication failed for tester@127.0.0.1")
            }
            other => panic!("{:?}", other.err()),
        }
    }

    // Wrong password.
    let mut o = options(&c, &home, &kh);
    o.password = Some("nope".into());
    assert!(matches!(connect(&o, &|_| true), Err(ConnectError::AuthenticationRequired(_))));

    // A server that goes away is noticed.
    let live: Arc<SftpFileSystem> = connect(&options(&c, &home, &kh), &|_| true).unwrap();
    let Container { id, .. } = &c;
    let _ = Command::new("docker").args(["kill", id]).output();
    let deadline = Instant::now() + Duration::from_secs(60);
    while live.is_connected() && Instant::now() < deadline {
        let _ = live.list_dir("/config", ListOptions::default());
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(!live.is_connected());
}
