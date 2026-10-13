//! HTTP(S)/1.1 plumbing shared by WebDAV and S3. Port of `httpclient.py`.
//!
//! A small blocking client: the file system interface is synchronous and
//! every call already runs on a worker thread. Keep-alive connections are
//! pooled per server, so each thread takes its own while it needs one (as
//! Python's one-connection-per-thread) and a long download never blocks a
//! listing: a streamed response owns its connection until it's read to the
//! end (then the connection goes back to the pool) or dropped (then it's
//! closed). Every socket operation has a timeout, so nothing blocks forever.
//!
//! Requests are retried once after a dropped keep-alive connection, and
//! again when an [`Authorizer`] answers an authentication challenge (Digest).

use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::{ClientConfig, ClientConnection, StreamOwned};

use crate::tls::{handshake, is_pin_mismatch, pinned_config, server_name, system_config, tcp_connect};
use crate::{Error, Result, describe_connect_error};

/// Timeout of every connect, read and write.
pub const TIMEOUT: Duration = Duration::from_secs(30);
/// Socket read buffer (as Python's `blocksize`).
pub const READ_BUFFER: usize = 256 * 1024;
pub const USER_AGENT: &str = "OrchardCommander/2.0";

/// Idle keep-alive connections kept per server.
const MAX_IDLE: usize = 8;
/// Longest response head accepted.
const MAX_HEAD: usize = 64 * 1024;
/// A forward seek up to this far in an open download reads on instead of
/// starting a new request.
const SKIP_LIMIT: u64 = 64 * 1024;
/// Most of an error answer's body kept (they're small; a server streaming an
/// endless one mustn't fill the memory). The rest goes with the connection.
const MAX_ERROR_BODY: u64 = 1024 * 1024;
/// Interim (1xx) answers accepted before the real one.
const MAX_INTERIM: usize = 16;

// ---------------------------------------------------------------------------
// Headers, bodies, responses
// ---------------------------------------------------------------------------

/// Header list in sending order; lookups ignore case.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    pub fn new() -> Self {
        Headers(Vec::new())
    }

    /// Builder-style [`Headers::set`].
    pub fn with(mut self, name: &str, value: impl Into<String>) -> Self {
        self.set(name, value);
        self
    }

    /// Replace the first header of this name (keeping its position), or add it.
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        let value = value.into();
        match self.0.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
            Some(slot) => slot.1 = value,
            None => self.0.push((name.to_string(), value)),
        }
    }

    /// Add a header even if one of this name exists (responses).
    pub fn push(&mut self, name: &str, value: impl Into<String>) {
        self.0.push((name.to_string(), value.into()));
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.0.iter().filter(move |(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Add every header of `other` (replacing same-named ones).
    pub fn extend(&mut self, other: &Headers) {
        for (k, v) in other.iter() {
            self.set(k, v);
        }
    }
}

pub trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

/// A request body. Streams are rewound before every attempt.
pub enum Body<'a> {
    None,
    Bytes(&'a [u8]),
    /// A seekable source and its length (sent with Content-Length, not chunked).
    Stream(&'a mut dyn ReadSeek, u64),
}

/// What an [`Authorizer`] may know about the body (S3 signs its hash).
#[derive(Clone, Copy, Debug)]
pub enum BodyKind<'a> {
    None,
    Bytes(&'a [u8]),
    Stream,
}

impl Body<'_> {
    pub fn kind(&self) -> BodyKind<'_> {
        match self {
            Body::None => BodyKind::None,
            Body::Bytes(b) => BodyKind::Bytes(b),
            Body::Stream(..) => BodyKind::Stream,
        }
    }

    fn len(&self) -> Option<u64> {
        match self {
            Body::None => None,
            Body::Bytes(b) => Some(b.len() as u64),
            Body::Stream(_, n) => Some(*n),
        }
    }
}

/// Adds credentials to requests.
pub trait Authorizer: Send + Sync {
    /// Add the credentials to `headers` (called for every attempt).
    fn authorize(&self, method: &str, url: &str, headers: &mut Headers, body: BodyKind<'_>);

    /// The server answered 401: whether a retry with other credentials makes sense.
    fn challenged(&self, _response: &Response) -> bool {
        false
    }
}

/// Status line and headers of a response.
#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Headers,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)
    }
}

/// A response body: read completely, or a stream to read yourself.
pub enum Payload {
    Data(Vec<u8>),
    Stream(BodyReader),
}

impl Payload {
    /// The data (a stream is read to the end).
    pub fn into_bytes(self) -> io::Result<Vec<u8>> {
        match self {
            Payload::Data(d) => Ok(d),
            Payload::Stream(mut s) => {
                let mut out = Vec::new();
                s.read_to_end(&mut out)?;
                Ok(out)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

enum Transport {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Transport::Plain(s) => s.read(buf),
            // Many servers close without a TLS close_notify: that's the end too.
            Transport::Tls(s) => match s.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
                other => other,
            },
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Transport::Plain(s) => s.write(buf),
            Transport::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Transport::Plain(s) => s.flush(),
            Transport::Tls(s) => s.flush(),
        }
    }
}

/// One keep-alive connection.
pub struct Connection {
    io: BufReader<Transport>,
}

#[derive(Clone, Copy, Debug)]
enum Framing {
    Empty,
    Length(u64),
    Chunked { left: u64 },
    UntilClose,
}

/// Options of an [`HttpClient`].
pub struct ClientOptions {
    pub secure: bool,
    /// The fingerprint of a user-trusted certificate: exactly that one is accepted.
    pub pin: Option<String>,
    pub timeout: Duration,
    pub authorizer: Option<Box<dyn Authorizer>>,
    /// Testing: connect to this address over plain HTTP instead (Host headers unchanged).
    pub connect_to: Option<(String, u16)>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions { secure: false, pin: None, timeout: TIMEOUT, authorizer: None, connect_to: None }
    }
}

struct Inner {
    host: String,
    port: u16,
    secure: bool,
    pin: Option<String>,
    timeout: Duration,
    authorizer: Option<Box<dyn Authorizer>>,
    connect_to: Option<(String, u16)>,
    tls: Mutex<Option<Arc<ClientConfig>>>,
    idle: Mutex<Vec<Connection>>,
    closed: AtomicBool,
    requests: AtomicU64,
}

/// Keep-alive connections to one server. Cheap to clone (shared).
#[derive(Clone)]
pub struct HttpClient(Arc<Inner>);

impl HttpClient {
    pub fn new(host: &str, port: u16, options: ClientOptions) -> Self {
        HttpClient(Arc::new(Inner {
            host: host.to_string(),
            port,
            secure: options.secure,
            pin: options.pin,
            timeout: options.timeout,
            authorizer: options.authorizer,
            connect_to: options.connect_to,
            tls: Mutex::new(None),
            idle: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            requests: AtomicU64::new(0),
        }))
    }

    pub fn host(&self) -> &str {
        &self.0.host
    }

    pub fn port(&self) -> u16 {
        self.0.port
    }

    pub fn secure(&self) -> bool {
        self.0.secure
    }

    pub fn authorizer(&self) -> Option<&dyn Authorizer> {
        self.0.authorizer.as_deref()
    }

    /// `host[:port]`, the port only if it isn't the scheme's default.
    pub fn netloc(&self) -> String {
        let default = if self.0.secure { 443 } else { 80 };
        if self.0.port == default { self.0.host.clone() } else { format!("{}:{}", self.0.host, self.0.port) }
    }

    /// `netloc` as URLs and Host headers need it (IPv6 addresses in brackets).
    pub fn authority(&self) -> String {
        let host = &self.0.host;
        let default = if self.0.secure { 443 } else { 80 };
        let host = if host.contains(':') { format!("[{host}]") } else { host.clone() };
        if self.0.port == default { host } else { format!("{host}:{}", self.0.port) }
    }

    pub fn origin(&self) -> String {
        format!("{}://{}", if self.0.secure { "https" } else { "http" }, self.authority())
    }

    pub fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::SeqCst)
    }

    /// Requests sent so far (attempts included).
    pub fn request_count(&self) -> u64 {
        self.0.requests.load(Ordering::Relaxed)
    }

    /// Close every idle connection; later requests fail. Streams still being
    /// read finish, but their connections aren't reused.
    pub fn close(&self) {
        self.0.closed.store(true, Ordering::SeqCst);
        self.0.idle.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn tls_config(&self) -> Result<Arc<ClientConfig>> {
        let mut slot = self.0.tls.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = slot.as_ref() {
            return Ok(c.clone());
        }
        let config = match &self.0.pin {
            Some(pin) => pinned_config(pin)?,
            None => system_config()?,
        };
        *slot = Some(config.clone());
        Ok(config)
    }

    fn connect_error(&self, e: &io::Error) -> Error {
        Error::Connection(describe_connect_error(&self.0.host, self.0.port, e))
    }

    fn certificate_changed(&self) -> Error {
        Error::CertificateChanged(format!(
            "The certificate of {} has changed. This could mean someone is intercepting the connection; \
             refusing to connect.",
            self.0.host
        ))
    }

    /// A new connection (TLS handshake done).
    fn open(&self) -> Result<Connection> {
        let timeout = self.0.timeout;
        if let Some((host, port)) = &self.0.connect_to {
            let sock = tcp_connect(host, *port, timeout).map_err(|e| self.connect_error(&e))?;
            return Ok(Connection { io: BufReader::with_capacity(READ_BUFFER, Transport::Plain(sock)) });
        }
        let mut sock = tcp_connect(&self.0.host, self.0.port, timeout).map_err(|e| self.connect_error(&e))?;
        if !self.0.secure {
            return Ok(Connection { io: BufReader::with_capacity(READ_BUFFER, Transport::Plain(sock)) });
        }
        let config = self.tls_config()?;
        let name = server_name(&self.0.host).map_err(|e| self.connect_error(&e))?;
        let mut conn = ClientConnection::new(config, name).map_err(|e| self.connect_error(&io::Error::other(e)))?;
        if let Err(e) = handshake(&mut conn, &mut sock) {
            if is_pin_mismatch(&e) {
                return Err(self.certificate_changed());
            }
            return Err(self.connect_error(&e));
        }
        let stream = StreamOwned::new(conn, sock);
        Ok(Connection { io: BufReader::with_capacity(READ_BUFFER, Transport::Tls(Box::new(stream))) })
    }

    fn take_idle(&self) -> Option<Connection> {
        self.0.idle.lock().unwrap_or_else(|e| e.into_inner()).pop()
    }

    fn put_idle(&self, conn: Connection) {
        if self.is_closed() {
            return;
        }
        let mut idle = self.0.idle.lock().unwrap_or_else(|e| e.into_inner());
        if idle.len() < MAX_IDLE {
            idle.push(conn);
        }
    }

    /// One request, its body read completely.
    pub fn request(&self, method: &str, url: &str, headers: &Headers, body: Body<'_>) -> Result<(Response, Vec<u8>)> {
        let (response, payload) = self.exchange(method, url, headers, body, false)?;
        let data = payload.into_bytes().map_err(|e| self.connect_error(&e))?;
        Ok((response, data))
    }

    /// One request whose body, if the status is below 300, is returned as a
    /// stream to read yourself (other bodies are read completely).
    pub fn request_stream(&self, method: &str, url: &str, headers: &Headers, body: Body<'_>) -> Result<(Response, Payload)> {
        self.exchange(method, url, headers, body, true)
    }

    /// The request, retried after a dropped keep-alive connection or an
    /// authentication challenge.
    fn exchange(&self, method: &str, url: &str, headers: &Headers, mut body: Body<'_>, stream: bool) -> Result<(Response, Payload)> {
        for attempt in 0..4 {
            if self.is_closed() {
                return Err(Error::Connection(format!("Connection to {} was closed", self.0.host)));
            }
            let (mut conn, reused) = match self.take_idle() {
                Some(c) => (c, true),
                None => (self.open()?, false),
            };
            let mut h = Headers::new().with("User-Agent", USER_AGENT);
            h.extend(headers);
            if let Some(a) = &self.0.authorizer {
                a.authorize(method, url, &mut h, body.kind());
            }
            self.0.requests.fetch_add(1, Ordering::Relaxed);
            let result: io::Result<(Response, Payload)> = (|| {
                let (response, framing, keep_alive) = self.round_trip(&mut conn, method, url, &h, &mut body)?;
                let mut reader = BodyReader { conn: Some(conn), framing, keep_alive, client: self.clone() };
                if stream && response.status < 300 {
                    return Ok((response, Payload::Stream(reader)));
                }
                let mut data = Vec::new();
                if response.status >= 300 {
                    (&mut reader).take(MAX_ERROR_BODY).read_to_end(&mut data)?;
                } else {
                    reader.read_to_end(&mut data)?;
                }
                Ok((response, Payload::Data(data)))
            })();
            match result {
                Ok((response, payload)) => {
                    if response.status == 401 && self.0.authorizer.as_ref().is_some_and(|a| a.challenged(&response)) {
                        continue;
                    }
                    return Ok((response, payload));
                }
                Err(e) => {
                    let timeout = matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock);
                    if reused && !timeout {
                        // The server closed an idle keep-alive connection: the
                        // other idle ones are likely gone too.
                        self.0.idle.lock().unwrap_or_else(|e| e.into_inner()).clear();
                        continue;
                    }
                    if attempt == 0 && !timeout {
                        continue;
                    }
                    return Err(self.connect_error(&e));
                }
            }
        }
        Err(Error::Connection(format!("{}: too many retries", self.0.host)))
    }

    fn round_trip(
        &self,
        conn: &mut Connection,
        method: &str,
        url: &str,
        headers: &Headers,
        body: &mut Body<'_>,
    ) -> io::Result<(Response, Framing, bool)> {
        let mut head = format!("{method} {url} HTTP/1.1\r\n");
        if !headers.contains("Host") {
            head.push_str(&format!("Host: {}\r\n", self.authority()));
        }
        for (k, v) in headers.iter() {
            if k.contains(['\r', '\n']) || v.contains(['\r', '\n']) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("invalid header {k}")));
            }
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        if !headers.contains("Content-Length") {
            match body.len() {
                Some(n) => head.push_str(&format!("Content-Length: {n}\r\n")),
                None if matches!(method, "PUT" | "POST" | "PATCH") => head.push_str("Content-Length: 0\r\n"),
                None => {}
            }
        }
        if !headers.contains("Accept-Encoding") {
            head.push_str("Accept-Encoding: identity\r\n");
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        let w = conn.io.get_mut();
        match body {
            Body::None => w.write_all(&out)?,
            Body::Bytes(b) if b.len() <= 64 * 1024 => {
                out.extend_from_slice(b);
                w.write_all(&out)?;
            }
            Body::Bytes(b) => {
                w.write_all(&out)?;
                w.write_all(b)?;
            }
            Body::Stream(reader, len) => {
                let src: &mut dyn ReadSeek = &mut **reader;
                src.seek(SeekFrom::Start(0))?;
                w.write_all(&out)?;
                let mut chunk = vec![0u8; READ_BUFFER];
                let mut left = *len;
                while left > 0 {
                    let n = src.read(&mut chunk[..left.min(READ_BUFFER as u64) as usize])?;
                    if n == 0 {
                        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the upload source ended early"));
                    }
                    w.write_all(&chunk[..n])?;
                    left -= n as u64;
                }
            }
        }
        w.flush()?;
        for _ in 0..=MAX_INTERIM {
            let (response, version) = read_head(&mut conn.io)?;
            if (100..200).contains(&response.status) && response.status != 101 {
                continue; // 100 Continue and friends
            }
            let framing = if method == "HEAD" || matches!(response.status, 100..=199 | 204 | 304) {
                Framing::Empty
            } else if response.header("Transfer-Encoding").is_some_and(|t| t.to_ascii_lowercase().contains("chunked")) {
                Framing::Chunked { left: 0 }
            } else if let Some(n) = response.header("Content-Length").and_then(|v| v.trim().parse::<u64>().ok()) {
                Framing::Length(n)
            } else {
                Framing::UntilClose
            };
            let connection = response.header("Connection").unwrap_or("").to_ascii_lowercase();
            let keep_alive = !matches!(framing, Framing::UntilClose)
                && if version == 0 { connection.contains("keep-alive") } else { !connection.contains("close") };
            return Ok((response, framing, keep_alive));
        }
        Err(io::Error::new(io::ErrorKind::InvalidData, "invalid response from the server: endless interim answers"))
    }
}

fn read_head(io: &mut BufReader<Transport>) -> io::Result<(Response, u8)> {
    let mut buf = Vec::with_capacity(1024);
    loop {
        let start = buf.len();
        let n = io.by_ref().take((MAX_HEAD - start) as u64 + 1).read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the server closed the connection"));
        }
        if buf.len() > MAX_HEAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "response head too long"));
        }
        let line = &buf[start..];
        if line == b"\r\n" || line == b"\n" {
            if start == 0 {
                buf.clear(); // tolerate stray empty lines before the status line
                continue;
            }
            break;
        }
        if !line.ends_with(b"\n") {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the server closed the connection"));
        }
    }
    let mut storage = [httparse::EMPTY_HEADER; 128];
    let mut parsed = httparse::Response::new(&mut storage);
    let invalid = |what: String| io::Error::new(io::ErrorKind::InvalidData, format!("invalid response from the server: {what}"));
    match parsed.parse(&buf) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => return Err(invalid("incomplete".into())),
        Err(e) => return Err(invalid(e.to_string())),
    }
    let mut headers = Headers::new();
    for h in parsed.headers.iter() {
        headers.push(h.name, String::from_utf8_lossy(h.value).trim().to_string());
    }
    let response = Response {
        status: parsed.code.unwrap_or(0),
        reason: parsed.reason.unwrap_or("").to_string(),
        headers,
    };
    Ok((response, parsed.version.unwrap_or(1)))
}

/// A response body being read. When read to the end, the connection goes
/// back to the client's pool; dropped earlier, it's closed.
pub struct BodyReader {
    conn: Option<Connection>,
    framing: Framing,
    keep_alive: bool,
    client: HttpClient,
}

impl BodyReader {
    fn finish(&mut self) {
        self.framing = Framing::Empty;
        if let Some(conn) = self.conn.take()
            && self.keep_alive
        {
            self.client.put_idle(conn);
        }
    }

    fn read_line(io: &mut BufReader<Transport>) -> io::Result<String> {
        let mut line = Vec::new();
        io.by_ref().take(4096).read_until(b'\n', &mut line)?;
        if !line.ends_with(b"\n") {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the server closed the connection mid-body"));
        }
        Ok(String::from_utf8_lossy(&line).trim().to_string())
    }
}

impl Read for BodyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let Some(conn) = self.conn.as_mut() else { return Ok(0) };
        let io = &mut conn.io;
        match self.framing {
            Framing::Empty | Framing::Length(0) => {
                self.finish();
                Ok(0)
            }
            Framing::Length(left) => {
                let want = left.min(buf.len() as u64) as usize;
                let n = io.read(&mut buf[..want])?;
                if n == 0 {
                    self.conn = None;
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the server closed the connection mid-body"));
                }
                self.framing = Framing::Length(left - n as u64);
                if left == n as u64 {
                    self.finish();
                }
                Ok(n)
            }
            Framing::Chunked { mut left } => {
                if left == 0 {
                    let line = Self::read_line(io)?;
                    let size = line.split(';').next().unwrap_or("").trim();
                    left = u64::from_str_radix(size, 16)
                        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("bad chunk size {size:?}")))?;
                    if left == 0 {
                        while !Self::read_line(io)?.is_empty() {} // trailers
                        self.finish();
                        return Ok(0);
                    }
                }
                let want = left.min(buf.len() as u64) as usize;
                let n = io.read(&mut buf[..want])?;
                if n == 0 {
                    self.conn = None;
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the server closed the connection mid-body"));
                }
                left -= n as u64;
                if left == 0 {
                    Self::read_line(io)?; // the CRLF after the chunk
                }
                self.framing = Framing::Chunked { left };
                Ok(n)
            }
            Framing::UntilClose => {
                let n = io.read(buf)?;
                if n == 0 {
                    self.conn = None;
                }
                Ok(n)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Seekable downloads
// ---------------------------------------------------------------------------

/// Starts a GET with these extra headers (a Range). Returns the response
/// and its body if the status is 200 or 206, or (416) at or past the end.
pub type RangeGet = Box<dyn FnMut(&Headers) -> Result<(Response, Payload)> + Send>;
/// The file's size, when it isn't known from a response yet.
pub type SizeOf = Box<dyn FnMut() -> Result<u64> + Send>;

/// A seekable download: reading streams one GET, seeking starts a new one at
/// that offset (Range), so media and archives on a server can be read in
/// place. A server that ignores Range is read up to the position.
pub struct RangeReader {
    get: RangeGet,
    size_of: SizeOf,
    pos: u64,
    size: Option<u64>,
    body: Option<BodyReader>,
}

impl RangeReader {
    pub fn new(get: RangeGet, size_of: SizeOf) -> Self {
        RangeReader { get, size_of, pos: 0, size: None, body: None }
    }

    fn size_now(&mut self) -> io::Result<u64> {
        if let Some(s) = self.size {
            return Ok(s);
        }
        let s = (self.size_of)()?;
        self.size = Some(s);
        Ok(s)
    }

    /// Start the GET at the position; false at or past the end.
    fn open(&mut self) -> io::Result<bool> {
        let headers = if self.pos > 0 { Headers::new().with("Range", format!("bytes={}-", self.pos)) } else { Headers::new() };
        let (response, payload) = (self.get)(&headers)?;
        if response.status == 416 {
            return Ok(false);
        }
        let Payload::Stream(mut body) = payload else { return Ok(false) };
        if response.status == 206 {
            let total = response.header("Content-Range").and_then(|r| r.rsplit('/').next()).unwrap_or("");
            if let Ok(n) = total.trim().parse() {
                self.size = Some(n);
            }
        } else {
            if let Some(n) = response.header("Content-Length").and_then(|v| v.trim().parse().ok()) {
                self.size = Some(n);
            }
            // The server ignored Range: read up to the position.
            io::copy(&mut (&mut body).take(self.pos), &mut io::sink())?;
        }
        self.body = Some(body);
        Ok(true)
    }
}

impl Read for RangeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.size.is_some_and(|s| self.pos >= s) {
            return Ok(0);
        }
        if self.body.is_none() && !self.open()? {
            return Ok(0);
        }
        let Some(body) = self.body.as_mut() else { return Ok(0) };
        let n = body.read(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for RangeReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => self.size_now()?.checked_add_signed(d),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "negative seek position"))?;
        if target != self.pos {
            let skip = target.wrapping_sub(self.pos);
            let mut kept = false;
            if target > self.pos
                && skip <= SKIP_LIMIT
                && let Some(body) = self.body.as_mut()
            {
                kept = io::copy(&mut body.take(skip), &mut io::sink()).is_ok_and(|n| n == skip);
            }
            if !kept {
                self.body = None; // an unfinished response: its connection is closed
            }
            self.pos = target;
        }
        Ok(self.pos)
    }
}

impl oc_vfs::ReadStream for RangeReader {}
