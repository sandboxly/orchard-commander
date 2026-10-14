//! Servers for the integration tests: the Python version's real test servers
//! (WsgiDAV, moto) from its venv, MinIO in Docker, and a tiny in-process HTTP
//! server for edge cases. Tests skip (with a note) when a server can't run.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oc_provider::ConnectContext;

pub const USER: &str = "tester";
pub const PASSWORD: &str = "s3cret";

/// Skip the test (returning early) when a server isn't available.
#[macro_export]
macro_rules! require {
    ($e:expr) => {
        match $e {
            Some(v) => v,
            None => {
                eprintln!("skipped: {} unavailable", stringify!($e));
                return;
            }
        }
    };
}

// ---------------------------------------------------------------------------
// The Python version's test servers
// ---------------------------------------------------------------------------

/// The Python test venv: $OC_TEST_PYTHON, or the repository's `.venv` (the
/// nearest ancestor of this crate that has one: worktrees find the main
/// checkout's). See `tests/servers-requirements.txt`.
pub fn python() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("OC_TEST_PYTHON") {
        return Some(PathBuf::from(p)).filter(|p| p.exists());
    }
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    here.ancestors().map(|a| a.join(".venv/bin/python")).find(|p| p.exists())
}

/// A server run by `tests/support/servers.py`; stopped when dropped.
pub struct PyServer {
    child: Child,
    pub host: String,
    pub port: u16,
    pub root: String,
    pub fingerprint: Option<String>,
}

impl PyServer {
    pub fn start(args: &[&str]) -> Option<PyServer> {
        let python = python()?;
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/servers.py");
        let mut child = Command::new(python)
            .arg("-I")
            .arg(script)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .ok()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take()?).read_line(&mut line).ok()?;
        let info: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        };
        Some(PyServer {
            child,
            host: info["host"].as_str()?.to_string(),
            port: info["port"].as_u64()? as u16,
            root: info["root"].as_str().unwrap_or("").to_string(),
            fingerprint: info["fingerprint"].as_str().map(str::to_string),
        })
    }

    pub fn webdav(folder: &Path, digest: bool, tls_dir: Option<&Path>) -> Option<PyServer> {
        let folder = folder.to_str()?;
        let mut args = vec!["webdav", folder];
        if digest {
            args.push("--digest");
        }
        if let Some(dir) = tls_dir {
            args.extend(["--tls", dir.to_str()?]);
        }
        Self::start(&args)
    }

    pub fn s3() -> Option<PyServer> {
        Self::start(&["s3"])
    }
}

impl Drop for PyServer {
    fn drop(&mut self) {
        drop(self.child.stdin.take()); // the server exits when stdin closes
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// MinIO in Docker (checks signatures for real, unlike moto)
// ---------------------------------------------------------------------------

pub const MINIO_USER: &str = "orchard-test";
pub const MINIO_SECRET: &str = "orchard-test-secret";

pub struct Minio {
    name: String,
    pub port: u16,
}

impl Minio {
    pub fn start() -> Option<Minio> {
        let info = Command::new("docker").args(["info", "--format", "{{.OSType}}"]).output().ok()?;
        if !info.status.success() || String::from_utf8_lossy(&info.stdout).trim() != "linux" {
            return None;
        }
        let name = format!("oc-remote-http-test-minio-{}-{}", std::process::id(), unique());
        let run = Command::new("docker")
            .args(["run", "-d", "--rm", "--name", &name, "-p", "127.0.0.1::9000"])
            .args(["-e", &format!("MINIO_ROOT_USER={MINIO_USER}"), "-e", &format!("MINIO_ROOT_PASSWORD={MINIO_SECRET}")])
            .args(["minio/minio:latest", "server", "/data"])
            .output()
            .ok()?;
        let mut minio = Minio { name, port: 0 };
        if !run.status.success() {
            eprintln!("docker run minio failed: {}", String::from_utf8_lossy(&run.stderr));
            return None;
        }
        let out = Command::new("docker").args(["port", &minio.name, "9000"]).output().ok()?;
        let port: u16 = String::from_utf8_lossy(&out.stdout).lines().next()?.rsplit(':').next()?.trim().parse().ok()?;
        minio.port = port;
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if http_status("127.0.0.1", port, "/minio/health/live") == Some(200) {
                return Some(minio);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        None
    }
}

impl Drop for Minio {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.name]).output();
    }
}

fn http_status(host: &str, port: u16, path: &str) -> Option<u16> {
    let mut s = TcpStream::connect((host, port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").ok()?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).ok()?;
    line.split_whitespace().nth(1)?.parse().ok()
}

pub fn unique() -> usize {
    static N: AtomicUsize = AtomicUsize::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// A tiny in-process HTTP server for edge cases
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct FakeRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Which connection (0, 1, …) and which request on it.
    pub connection: usize,
    pub index: usize,
}

impl FakeRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

pub enum Reply {
    /// Raw bytes to send, then keep the connection.
    Send(Vec<u8>),
    /// Raw bytes to send, then close.
    SendAndClose(Vec<u8>),
    /// Close without answering.
    Close,
    /// Answer nothing for this long, then close.
    Stall(Duration),
}

/// A complete response with Content-Length.
pub fn response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    let mut out = out.into_bytes();
    out.extend_from_slice(body);
    out
}

type Handler = dyn Fn(&FakeRequest) -> Reply + Send + Sync;

pub struct FakeServer {
    pub port: u16,
    stop: Arc<AtomicBool>,
    pub requests: Arc<Mutex<Vec<FakeRequest>>>,
}

impl FakeServer {
    pub fn start(handler: impl Fn(&FakeRequest) -> Reply + Send + Sync + 'static) -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<Handler> = Arc::new(handler);
        let (stop2, requests2) = (stop.clone(), requests.clone());
        std::thread::spawn(move || {
            for (n, stream) in listener.incoming().enumerate() {
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let (handler, requests) = (handler.clone(), requests2.clone());
                std::thread::spawn(move || serve(stream, n, &*handler, &requests));
            }
        });
        FakeServer { port, stop, requests }
    }

    pub fn requests(&self) -> Vec<FakeRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port)); // wake the accept loop
    }
}

fn serve(stream: TcpStream, connection: usize, handler: &Handler, requests: &Mutex<Vec<FakeRequest>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut writer = stream;
    for index in 0.. {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
        let mut headers = Vec::new();
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        let len: usize = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0; len];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let request = FakeRequest { method, path, headers, body, connection, index };
        requests.lock().unwrap().push(request.clone());
        match handler(&request) {
            Reply::Send(raw) => {
                if writer.write_all(&raw).is_err() {
                    return;
                }
            }
            Reply::SendAndClose(raw) => {
                let _ = writer.write_all(&raw);
                return;
            }
            Reply::Close => return,
            Reply::Stall(d) => {
                std::thread::sleep(d);
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// A ConnectContext for provider tests
// ---------------------------------------------------------------------------

pub struct TestContext {
    pub dir: PathBuf,
    pub answer: bool,
    pub asked: Mutex<Vec<(String, String, String)>>,
}

impl TestContext {
    pub fn new(dir: &Path, answer: bool) -> Self {
        TestContext { dir: dir.to_path_buf(), answer, asked: Mutex::new(Vec::new()) }
    }
}

impl ConnectContext for TestContext {
    fn data_dir(&self) -> PathBuf {
        self.dir.clone()
    }

    fn confirm(&self, title: &str, message: &str, ok_label: &str) -> bool {
        self.asked.lock().unwrap().push((title.into(), message.into(), ok_label.into()));
        self.answer
    }
}
