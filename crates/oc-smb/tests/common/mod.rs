//! Real SMB servers for the integration tests (port of `tests/servers.py`):
//!
//! - impacket's pure-Python SMB2 server from the Python version's venv
//!   (read-only use; skipped when the venv or impacket isn't there)
//! - Samba in Docker (full read/write; skipped without Docker)

#![allow(dead_code)]

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use oc_smb::{ConnectOptions, SmbConnectError, SmbFileSystem};

pub const USER: &str = "tester";
pub const PASSWORD: &str = "s3cret";

pub struct Server {
    pub host: String,
    pub port: u16,
    pub share: String,
    guard: Guard,
}

enum Guard {
    Process(Child, tempfile::TempDir),
    Container(String),
}

impl Drop for Server {
    fn drop(&mut self) {
        match &mut self.guard {
            Guard::Process(child, _) => {
                let _ = child.kill();
                let _ = child.wait();
            }
            Guard::Container(name) => {
                let _ = Command::new("docker").args(["rm", "-f", name]).output();
            }
        }
    }
}

impl Server {
    pub fn connect(&self) -> Result<SmbFileSystem, SmbConnectError> {
        self.connect_with(PASSWORD)
    }

    pub fn connect_with(&self, password: &str) -> Result<SmbFileSystem, SmbConnectError> {
        let opts = ConnectOptions {
            port: self.port,
            username: Some(USER.into()),
            password: Some(password.into()),
            timeout: Duration::from_secs(10),
        };
        oc_smb::connect(&self.host, &self.share, &opts)
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map(|a| a.port()).expect("free port")
}

fn wait_for_port(port: u16, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

// ---------------------------------------------------------------------------
// impacket
// ---------------------------------------------------------------------------

const IMPACKET_SERVER: &str = r#"
import binascii, logging, sys
from impacket import ntlm, smbserver
root, port, user, password = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
logging.getLogger("impacket").setLevel(logging.ERROR)
server = smbserver.SimpleSMBServer(listenAddress="127.0.0.1", listenPort=port)
server.addShare("SHARE", root, "test share")
server.setSMB2Support(True)
server.setLogFile("")
nthash = binascii.hexlify(ntlm.compute_nthash(password)).decode()
server.addCredential(user, 0, "aad3b435b51404eeaad3b435b51404ee", nthash)
server.start()
"#;

/// OC_TEST_PYTHON, else the Python version's dev venv next to this
/// repository, else `python3` — whichever has impacket.
fn python() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::env::var_os("OC_TEST_PYTHON").map(PathBuf::from).into_iter().collect();
    for dir in Path::new(env!("CARGO_MANIFEST_DIR")).ancestors() {
        candidates.push(dir.join("fcmd/.venv/bin/python"));
    }
    candidates.push(PathBuf::from("python3"));
    candidates.into_iter().find(|python| {
        Command::new(python)
            .args(["-I", "-c", "import impacket.smbserver"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// impacket's SMB2 server sharing `root` as SHARE, or None (with a note) if
/// no Python with impacket is available (set OC_TEST_PYTHON to point at one).
pub fn start_impacket(root: &Path) -> Option<Server> {
    let Some(python) = python() else {
        eprintln!("skipped: no Python with impacket (set OC_TEST_PYTHON)");
        return None;
    };
    let scripts = tempfile::tempdir().expect("tempdir");
    let script = scripts.path().join("smb_server.py");
    std::fs::File::create(&script).and_then(|mut f| f.write_all(IMPACKET_SERVER.as_bytes())).expect("write script");
    let port = free_port();
    let child = Command::new(python)
        .arg("-I")
        .arg(&script)
        .arg(root)
        .arg(port.to_string())
        .args([USER, PASSWORD])
        .current_dir(scripts.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start impacket");
    let server =
        Server { host: "127.0.0.1".into(), port, share: "SHARE".into(), guard: Guard::Process(child, scripts) };
    assert!(wait_for_port(port, Duration::from_secs(15)), "impacket server did not start");
    Some(server)
}

// ---------------------------------------------------------------------------
// Samba in Docker
// ---------------------------------------------------------------------------

pub const SAMBA_IMAGE: &str = "ghcr.io/servercontainers/samba:latest";

pub fn docker_available() -> bool {
    // The Samba image needs Linux containers (Docker on Windows runners runs Windows ones).
    Command::new("docker")
        .args(["info", "--format", "{{.OSType}}"])
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "linux")
}

/// A real Samba server; the share lives in the container (tmpfs).
/// `max_credits` limits SMB2 credits like small NAS boxes do (Samba's default is 8192).
/// None (with a note) when Docker isn't available.
pub fn start_samba(max_credits: Option<u32>) -> Option<Server> {
    if !docker_available() {
        eprintln!("skipped: Docker not available for the Samba test server");
        return None;
    }
    let name = format!("orchard-commander-test-samba-rs-{}-{}", std::process::id(), free_port());
    let config = format!("[SHARE]; path=/shares/share; valid users = {USER}; read only = no; force user = {USER}");
    let mut args: Vec<String> =
        ["run", "-d", "--rm", "--name", &name, "-p", "127.0.0.1::445"].map(String::from).to_vec();
    for env in [
        format!("ACCOUNT_{USER}={PASSWORD}"),
        format!("UID_{USER}=1000"),
        format!("SAMBA_VOLUME_CONFIG_share={config}"),
    ] {
        args.extend(["-e".to_string(), env]);
    }
    if let Some(n) = max_credits {
        args.extend(["-e".to_string(), format!("SAMBA_GLOBAL_CONFIG_smb2_SPACE_max_SPACE_credits={n}")]);
    }
    args.extend(["--tmpfs".to_string(), "/shares/share:mode=1777".to_string(), SAMBA_IMAGE.to_string()]);
    let out = Command::new("docker").args(&args).output().expect("docker run");
    assert!(out.status.success(), "docker run failed: {}", String::from_utf8_lossy(&out.stderr));
    let guard = Guard::Container(name.clone());
    let port_out = Command::new("docker").args(["port", &name, "445"]).output().expect("docker port");
    let port: u16 = String::from_utf8_lossy(&port_out.stdout)
        .lines()
        .next()
        .and_then(|l| l.rsplit(':').next())
        .and_then(|p| p.trim().parse().ok())
        .expect("published port");
    let server = Server { host: "127.0.0.1".into(), port, share: "SHARE".into(), guard };
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let logs = Command::new("docker").args(["logs", &name]).output().map(|o| o.stdout).unwrap_or_default();
        if String::from_utf8_lossy(&logs).contains("smbd version") && wait_for_port(port, Duration::from_millis(500)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    // smbd may accept TCP a moment before it serves sessions.
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        match server.connect() {
            Ok(fs) => {
                drop(fs);
                return Some(server);
            }
            Err(_) => std::thread::sleep(Duration::from_millis(500)),
        }
    }
    panic!("Samba in Docker did not come up");
}
