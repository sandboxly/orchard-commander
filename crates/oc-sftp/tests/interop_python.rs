//! Interoperability with the Python 1.x version's world: its asyncssh test
//! server (a different SSH/SFTP implementation than ours) and paramiko's
//! known_hosts files, read and written both ways.
//!
//! Needs the Python checkout (`fcmd/` next to this repository, or
//! `OC_FCMD_DIR`) with its `.venv`; skips otherwise.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use oc_sftp::{Agent, ConnectOptions, connect};
use oc_vfs::{FileSystem, VPath};

fn fcmd_dir() -> Option<PathBuf> {
    let ok = |d: &Path| d.join("tests/servers.py").is_file() && python_in(d).is_file();
    if let Some(d) = std::env::var_os("OC_FCMD_DIR").map(PathBuf::from) {
        return ok(&d).then_some(d);
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().map(|a| a.join("fcmd")).find(|d| ok(d))
}

fn python_in(fcmd: &Path) -> PathBuf {
    if cfg!(windows) { fcmd.join(".venv/Scripts/python.exe") } else { fcmd.join(".venv/bin/python") }
}

const SERVER_SCRIPT: &str = r#"
import sys, pathlib
sys.path.insert(0, sys.argv[1])
from servers import start_sftp_server
import paramiko
root, known_hosts = pathlib.Path(sys.argv[2]), sys.argv[3]
srv = start_sftp_server(root)
t = paramiko.Transport((srv.host, srv.port))
t.start_client(timeout=10)
key = t.get_remote_server_key()
t.close()
keys = paramiko.HostKeys()
keys.add(f"[{srv.host}]:{srv.port}", key.get_name(), key)
keys.save(known_hosts)
print(srv.port, srv.host_key_fingerprint, flush=True)
sys.stdin.read()
srv.stop()
"#;

const CHECK_SCRIPT: &str = r#"
import sys, paramiko
keys = paramiko.HostKeys(sys.argv[1])
entry = keys.lookup(sys.argv[2])
assert entry is not None, "host not found"
key = entry["ssh-ed25519"]
import base64, hashlib
print("SHA256:" + base64.b64encode(hashlib.sha256(key.asbytes()).digest()).decode().rstrip("="))
"#;

struct PyServer {
    child: Child,
    port: u16,
    fingerprint: String,
}

impl Drop for PyServer {
    fn drop(&mut self) {
        drop(self.child.stdin.take()); // EOF: the script stops the server
        let _ = self.child.wait();
    }
}

fn start(fcmd: &Path, root: &Path, known_hosts: &Path) -> PyServer {
    let mut child = Command::new(python_in(fcmd))
        .arg("-c")
        .arg(SERVER_SCRIPT)
        .arg(fcmd.join("tests"))
        .arg(root)
        .arg(known_hosts)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start python");
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let mut parts = line.split_whitespace();
    let port = parts.next().and_then(|p| p.parse().ok()).unwrap_or_else(|| panic!("server said {line:?}"));
    let fingerprint = parts.next().unwrap_or_default().to_string();
    PyServer { child, port, fingerprint }
}

fn options(port: u16, known_hosts: &Path, home: &Path) -> ConnectOptions {
    let mut o = ConnectOptions::new("127.0.0.1", known_hosts);
    o.port = port;
    o.username = Some("tester".into());
    o.password = Some("s3cret".into());
    o.agent = Agent::Off;
    o.home = Some(home.to_path_buf());
    o.timeout = Duration::from_secs(10);
    o
}

#[test]
fn asyncssh_server_and_paramiko_known_hosts() {
    let Some(fcmd) = fcmd_dir() else {
        eprintln!("skipped: no Python checkout with .venv (set OC_FCMD_DIR)");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().canonicalize().unwrap();
    let root = tmp.join("remote");
    fs::create_dir_all(root.join("contract")).unwrap();
    let paramiko_hosts = tmp.join("paramiko_known_hosts");
    let server = start(&fcmd, &root, &paramiko_hosts);

    // A known_hosts written by paramiko (1.x) is trusted as is.
    let remote: Arc<dyn FileSystem> =
        connect(&options(server.port, &paramiko_hosts, &tmp.join("home")), &|_| panic!("asked about a known host"))
            .unwrap_or_else(|e| panic!("connect: {e}"));
    oc_vfs::testing::check_filesystem(&VPath::new(remote.clone(), "/contract"));
    remote.close();

    // One we write is read by paramiko.
    let ours = tmp.join("our_known_hosts");
    let fs2 = connect(&options(server.port, &ours, &tmp.join("home")), &|q| {
        assert_eq!(q.fingerprint, server.fingerprint);
        true
    })
    .unwrap();
    fs2.close();
    let out = Command::new(python_in(&fcmd))
        .arg("-c")
        .arg(CHECK_SCRIPT)
        .arg(&ours)
        .arg(format!("[127.0.0.1]:{}", server.port))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), server.fingerprint);
}
