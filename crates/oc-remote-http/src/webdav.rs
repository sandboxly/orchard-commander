//! WebDAV file system over HTTP(S): Nextcloud, ownCloud, NAS boxes,
//! Apache/nginx shares. Port of `remote/webdav.py`.
//!
//! Paths are absolute POSIX paths below the address's root folder: "/" is the
//! root, e.g. `https://cloud.example.com/remote.php/dav/files/me/`.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use md5::Md5;
use oc_vfs::{Capabilities, Entry, FileSystem, ListOptions, ReadStream, RelPath, WalkIter, WriteStream, sort_entries};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use sha2::{Digest, Sha256};

use crate::dates::{format_http_date, parse_http_date};
use crate::http::{Authorizer, Body, BodyKind, ClientOptions, Headers, HttpClient, Payload, RangeReader, Response, TIMEOUT};
use crate::tls::{CertificateQuestion, check_certificate};
use crate::{Error, Result, base_name, dirname, live, norm_abs, parent_abs, posix_join};

/// Uploads larger than this are spooled to a temp file.
pub const SPOOL_IN_MEMORY: usize = 16 * 1024 * 1024;

const DAV: &str = "DAV:";
const PROPFIND: &[u8] = b"<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<d:propfind xmlns:d=\"DAV:\"><d:prop>\
<d:resourcetype/><d:getcontentlength/><d:getlastmodified/><d:quota-available-bytes/>\
</d:prop></d:propfind>";

/// Left unescaped in URL paths (besides letters, digits and "_.-~"): "/!$&'()*+,;=:@~".
const PATH: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'_').remove(b'.').remove(b'-').remove(b'~')
    .remove(b'/').remove(b'!').remove(b'$').remove(b'&').remove(b'\'').remove(b'(').remove(b')')
    .remove(b'*').remove(b'+').remove(b',').remove(b';').remove(b'=').remove(b':').remove(b'@');

fn unquote(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// The path of an href (`urlsplit(href).path`): a full URL's scheme and server dropped.
fn href_path(href: &str) -> &str {
    let mut rest = href.trim();
    if let Some(i) = rest.find("://").filter(|&i| !rest[..i].contains('/')) {
        rest = &rest[i + 3..];
        rest = rest.find('/').map_or("", |j| &rest[j..]);
    } else if let Some(r) = rest.strip_prefix("//") {
        rest = r.find('/').map_or("", |j| &r[j..]);
    }
    rest.split(['?', '#']).next().unwrap_or("")
}

// ---------------------------------------------------------------------------
// Authentication: Basic, or Digest when the server asks for it
// ---------------------------------------------------------------------------

struct DigestState {
    challenge: Option<HashMap<String, String>>,
    count: u32,
}

/// Basic authentication, switching to Digest when the server challenges with it.
pub struct DavAuth {
    username: String,
    password: String,
    state: Mutex<DigestState>,
}

impl DavAuth {
    pub fn new(username: Option<&str>, password: Option<&str>) -> Self {
        DavAuth {
            username: username.unwrap_or("").to_string(),
            password: password.unwrap_or("").to_string(),
            state: Mutex::new(DigestState { challenge: None, count: 0 }),
        }
    }

    fn digest_header(&self, method: &str, uri: &str, c: &HashMap<String, String>, count: u32) -> String {
        let get = |k: &str| c.get(k).map(String::as_str).unwrap_or("");
        let algorithm = c.get("algorithm").map(String::as_str).unwrap_or("MD5");
        let sha = algorithm.to_uppercase().starts_with("SHA-256");
        let h = |text: &str| -> String {
            if sha { hex(&Sha256::digest(text.as_bytes())) } else { hex(&Md5::digest(text.as_bytes())) }
        };
        let (realm, nonce) = (get("realm"), get("nonce"));
        let cnonce = random_hex();
        let nc = format!("{count:08x}");
        let mut ha1 = h(&format!("{}:{realm}:{}", self.username, self.password));
        if algorithm.to_lowercase().ends_with("-sess") {
            ha1 = h(&format!("{ha1}:{nonce}:{cnonce}"));
        }
        let ha2 = h(&format!("{method}:{uri}"));
        let qop = if get("qop").split(',').any(|q| q.trim() == "auth") { "auth" } else { "" };
        let response = if qop.is_empty() {
            h(&format!("{ha1}:{nonce}:{ha2}"))
        } else {
            h(&format!("{ha1}:{nonce}:{nc}:{cnonce}:{qop}:{ha2}"))
        };
        let mut parts = vec![
            format!("username=\"{}\"", self.username),
            format!("realm=\"{realm}\""),
            format!("nonce=\"{nonce}\""),
            format!("uri=\"{uri}\""),
            format!("algorithm={algorithm}"),
            format!("response=\"{response}\""),
        ];
        if !qop.is_empty() {
            parts.extend([format!("qop={qop}"), format!("nc={nc}"), format!("cnonce=\"{cnonce}\"")]);
        }
        if let Some(opaque) = c.get("opaque") {
            parts.push(format!("opaque=\"{opaque}\""));
        }
        format!("Digest {}", parts.join(", "))
    }
}

impl Authorizer for DavAuth {
    fn authorize(&self, method: &str, url: &str, headers: &mut Headers, _body: BodyKind<'_>) {
        if self.username.is_empty() {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(challenge) = state.challenge.clone() else {
            let token = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", self.username, self.password));
            headers.set("Authorization", format!("Basic {token}"));
            return;
        };
        state.count += 1;
        let count = state.count;
        drop(state);
        headers.set("Authorization", self.digest_header(method, url, &challenge, count));
    }

    fn challenged(&self, response: &Response) -> bool {
        if self.username.is_empty() {
            return false;
        }
        for challenge in response.headers.get_all("WWW-Authenticate") {
            let challenge = challenge.trim();
            let (scheme, rest) = challenge.split_once(' ').unwrap_or((challenge, ""));
            if scheme.eq_ignore_ascii_case("digest") {
                let params = auth_params(rest);
                let stale = params.get("stale").is_some_and(|s| s.eq_ignore_ascii_case("true"));
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let retry = state.challenge.is_none() || stale;
                state.challenge = Some(params);
                state.count = 0;
                return retry;
            }
        }
        false
    }
}

/// `key=value` / `key="value"` pairs of an authentication challenge (keys lower-cased),
/// as Python's `re.findall(r'(\w+)=(?:"([^"]*)"|([^,\s]*))', rest)`.
fn auth_params(text: &str) -> HashMap<String, String> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = HashMap::new();
    let mut i = 0;
    while i < chars.len() {
        let start = i;
        let mut j = i;
        while j < chars.len() && is_word(chars[j].1) {
            j += 1;
        }
        if j == start || j >= chars.len() || chars[j].1 != '=' {
            i = if j > start { j } else { i + 1 };
            continue;
        }
        let key = text[chars[start].0..chars[j].0].to_lowercase();
        let value_start = j + 1;
        let byte_at = |k: usize| chars.get(k).map_or(text.len(), |c| c.0);
        if chars.get(value_start).is_some_and(|c| c.1 == '"')
            && let Some(close) = (value_start + 1..chars.len()).find(|&k| chars[k].1 == '"')
        {
            out.insert(key, text[byte_at(value_start + 1)..byte_at(close)].to_string());
            i = close + 1;
            continue;
        }
        let mut k = value_start;
        while k < chars.len() && chars[k].1 != ',' && !chars[k].1.is_whitespace() {
            k += 1;
        }
        out.insert(key, text[byte_at(value_start)..byte_at(k)].to_string());
        i = k.max(value_start);
    }
    out
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_hex() -> String {
    let mut h = RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    format!("{:016x}", h.finish())
}

// ---------------------------------------------------------------------------
// Connecting
// ---------------------------------------------------------------------------

/// How to reach a WebDAV server.
#[derive(Clone, Debug)]
pub struct WebDavOptions {
    pub secure: bool,
    /// Default: 443 / 80.
    pub port: Option<u16>,
    /// The server path of the share, e.g. "/remote.php/dav/files/me".
    pub root: String,
    pub username: Option<String>,
    pub password: Option<String>,
    /// The file of user-trusted certificates.
    pub trusted_certificates: PathBuf,
    pub timeout: Duration,
}

impl Default for WebDavOptions {
    fn default() -> Self {
        WebDavOptions {
            secure: true,
            port: None,
            root: "/".into(),
            username: None,
            password: None,
            trusted_certificates: PathBuf::from("trusted_certificates.json"),
            timeout: TIMEOUT,
        }
    }
}

/// Connect and check the root folder. Fails with `AuthenticationRequired`,
/// `CertificateChanged`, `CertificateRejected` or `Connection`.
pub fn connect(
    host: &str,
    options: &WebDavOptions,
    confirm_certificate: &dyn Fn(&CertificateQuestion) -> bool,
) -> Result<WebDavFileSystem> {
    let port = options.port.unwrap_or(if options.secure { 443 } else { 80 });
    let pin = if options.secure {
        check_certificate(host, port, &options.trusted_certificates, confirm_certificate, options.timeout)?
    } else {
        None
    };
    let fs = WebDavFileSystem::new(
        host,
        port,
        options.secure,
        &options.root,
        options.username.as_deref(),
        options.password.as_deref(),
        pin,
        options.timeout,
    );
    if let Err(e) = fs.inner.check_root() {
        fs.close();
        return Err(e);
    }
    Ok(fs)
}

// ---------------------------------------------------------------------------
// The file system
// ---------------------------------------------------------------------------

/// A WebDAV share. Cheap to clone (shared).
#[derive(Clone)]
pub struct WebDavFileSystem {
    inner: Arc<Dav>,
}

struct Dav {
    host: String,
    /// URL path prefix, no trailing slash ("" for the server's root).
    root: String,
    http: HttpClient,
    quota: Mutex<HashMap<String, u64>>,
    /// How this server accepts modification times; Some("") = it doesn't.
    mtime_method: Mutex<Option<&'static str>>,
    label: String,
    type_name: String,
    identity: String,
    closed: Arc<AtomicBool>,
}

/// One PROPFIND answer: the properties this file system uses.
#[derive(Debug, Default)]
struct Props {
    resourcetype: Option<bool>,
    length: Option<String>,
    modified: Option<String>,
    quota: Option<String>,
}

impl WebDavFileSystem {
    /// A file system for an already checked server (see [`connect`]).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host: &str,
        port: u16,
        secure: bool,
        root: &str,
        username: Option<&str>,
        password: Option<&str>,
        pin: Option<String>,
        timeout: Duration,
    ) -> Self {
        let trimmed = root.trim_matches('/');
        let http = HttpClient::new(
            host,
            port,
            ClientOptions {
                secure,
                pin,
                timeout,
                authorizer: Some(Box::new(DavAuth::new(username, password))),
                connect_to: None,
            },
        );
        let label = format!("{}{}", username.filter(|u| !u.is_empty()).map(|u| format!("{u}@")).unwrap_or_default(), http.netloc());
        let type_name = format!("{}://{label}", if secure { "webdavs" } else { "webdav" });
        let closed = Arc::new(AtomicBool::new(false));
        WebDavFileSystem {
            inner: Arc::new(Dav {
                host: host.to_string(),
                root: if trimmed.is_empty() { String::new() } else { format!("/{trimmed}") },
                http,
                quota: Mutex::new(HashMap::new()),
                mtime_method: Mutex::new(None),
                identity: live::register("webdav", &closed),
                closed,
                label,
                type_name,
            }),
        }
    }

    /// `user@host[:port]`.
    pub fn label(&self) -> &str {
        &self.inner.label
    }

    pub fn is_connected(&self) -> bool {
        !self.inner.http.is_closed()
    }

    /// The URL path of a file system path (quoted).
    pub fn url_path(&self, path: &str) -> String {
        self.inner.url_path(path)
    }
}

impl Dav {
    fn url_path(&self, path: &str) -> String {
        utf8_percent_encode(&format!("{}{}", self.root, norm_abs(path)), PATH).to_string()
    }

    fn display(&self, path: &str) -> String {
        format!("{}{}{}", self.type_name, self.root, norm_abs(path))
    }

    fn send(&self, method: &str, url: &str, headers: &Headers, body: Body<'_>) -> Result<(Response, Vec<u8>)> {
        let (response, data) = self.http.request(method, url, headers, body)?;
        self.check_auth(&response)?;
        Ok((response, data))
    }

    fn check_auth(&self, response: &Response) -> Result<()> {
        if response.status == 401 {
            return Err(Error::AuthenticationRequired(format!("{} didn't accept the user name and password", self.type_name)));
        }
        Ok(())
    }

    fn request(&self, method: &str, path: &str, ok: &[u16], collection: bool, headers: &Headers, body: Option<&[u8]>) -> Result<Vec<u8>> {
        let mut url = self.url_path(path);
        if collection && !url.ends_with('/') {
            url.push('/');
        }
        let (response, data) = self.send(method, &url, headers, body.map_or(Body::None, Body::Bytes))?;
        if matches!(response.status, 301 | 302 | 307 | 308) && !collection && matches!(method, "PROPFIND" | "DELETE" | "MOVE") {
            let location = href_path(response.header("Location").unwrap_or(""));
            if location.trim_end_matches('/') == url.trim_end_matches('/') {
                // Just the trailing slash of a folder.
                return self.request(method, path, ok, true, headers, body);
            }
        }
        if !ok.contains(&response.status) {
            return Err(dav_error(response.status, &response.reason, &format!("{method} {}", self.display(path))));
        }
        Ok(data)
    }

    // -- reading -------------------------------------------------------------------

    fn check_root(&self) -> Result<()> {
        if !self.stat("/")?.is_dir {
            return Err(Error::Connection(format!("{} is not a folder", self.display("/"))));
        }
        Ok(())
    }

    fn propfind(&self, path: &str, depth: &str) -> Result<Vec<(String, Props)>> {
        let headers = Headers::new().with("Depth", depth).with("Content-Type", "application/xml; charset=utf-8");
        let data = self.request("PROPFIND", path, &[207], depth == "1", &headers, Some(PROPFIND))?;
        let invalid = |e: String| Error::os(io::ErrorKind::Other, format!("{} sent an invalid listing: {e}", self.type_name));
        let text = String::from_utf8_lossy(&data);
        let doc = roxmltree::Document::parse(&text).map_err(|e| invalid(e.to_string()))?;
        let dav = |n: &roxmltree::Node, name: &str| n.is_element() && n.tag_name().namespace() == Some(DAV) && n.tag_name().name() == name;
        let mut result = Vec::new();
        for response in doc.descendants().filter(|n| dav(n, "response")) {
            let href = response.children().find(|n| dav(n, "href")).and_then(|n| n.text()).unwrap_or("");
            let mut props = Props::default();
            for propstat in response.children().filter(|n| dav(n, "propstat")) {
                let status = propstat.children().find(|n| dav(n, "status")).and_then(|n| n.text()).unwrap_or("");
                if !status.contains(" 200") {
                    continue;
                }
                for prop in propstat.children().filter(|n| dav(n, "prop")) {
                    for child in prop.children().filter(|n| n.is_element() && n.tag_name().namespace() == Some(DAV)) {
                        let text = || child.text().map(str::to_string);
                        match child.tag_name().name() {
                            "resourcetype" => props.resourcetype = Some(child.children().any(|n| dav(&n, "collection"))),
                            "getcontentlength" => props.length = text(),
                            "getlastmodified" => props.modified = text(),
                            "quota-available-bytes" => props.quota = text(),
                            _ => {}
                        }
                    }
                }
            }
            result.push((unquote(href_path(href)), props));
        }
        Ok(result)
    }

    fn entry(name: &str, props: &Props) -> Entry {
        let is_dir = props.resourcetype == Some(true);
        let size = if is_dir { 0 } else { props.length.as_deref().and_then(|t| t.trim().parse().ok()).unwrap_or(0) };
        let mtime = props.modified.as_deref().and_then(parse_http_date).unwrap_or(0.0);
        let mut entry = Entry::file(name, size, mtime);
        entry.is_dir = is_dir;
        entry
    }

    fn list(&self, path: &str) -> Result<Vec<Entry>> {
        let own = unquote(&self.url_path(path)).trim_end_matches('/').to_string();
        let mut entries = Vec::new();
        for (href, props) in self.propfind(path, "1")? {
            if href.trim_end_matches('/') == own {
                // The folder itself.
                if let Some(q) = props.quota.as_deref().map(str::trim).filter(|q| !q.is_empty() && q.bytes().all(|b| b.is_ascii_digit()))
                    && let Ok(q) = q.parse()
                {
                    self.quota.lock().unwrap_or_else(|e| e.into_inner()).insert(norm_abs(path), q);
                }
                continue;
            }
            let name = base_name(&href);
            if !name.is_empty() {
                entries.push(Self::entry(name, &props));
            }
        }
        Ok(entries)
    }

    fn stat(&self, path: &str) -> Result<Entry> {
        let responses = self.propfind(path, "0")?;
        let Some((_, props)) = responses.first() else {
            return Err(Error::os(io::ErrorKind::NotFound, format!("Not found: {}", self.display(path))));
        };
        Ok(Self::entry(&self.basename(path), props))
    }

    fn basename(&self, path: &str) -> String {
        let name = base_name(path);
        if name.is_empty() { self.host.clone() } else { name.to_string() }
    }

    fn exists(&self, path: &str) -> bool {
        self.stat(path).is_ok()
    }

    fn is_dir(&self, path: &str) -> bool {
        self.stat(path).is_ok_and(|e| e.is_dir)
    }

    // -- writing -------------------------------------------------------------------

    fn mkdir(&self, path: &str, exist_ok: bool) -> Result<()> {
        let mut missing = Vec::new();
        let mut p = norm_abs(path);
        while p != "/" && !self.exists(&p) {
            let parent = dirname(&p).to_string();
            missing.push(std::mem::replace(&mut p, parent));
        }
        if missing.is_empty() {
            if exist_ok && self.is_dir(path) {
                return Ok(());
            }
            return Err(Error::os(io::ErrorKind::AlreadyExists, format!("File exists: {}", self.display(path))));
        }
        for p in missing.iter().rev() {
            if let Err(e) = self.request("MKCOL", p, &[200, 201, 204], true, &Headers::new(), None) {
                if !e.is_permission() || !self.is_dir(p) {
                    return Err(e); // 405: something is in the way (or a race)
                }
                if !exist_ok && *p == missing[0] {
                    return Err(Error::os(io::ErrorKind::AlreadyExists, format!("File exists: {}", self.display(p))));
                }
            }
        }
        Ok(())
    }

    fn remove(&self, path: &str) -> Result<()> {
        self.request("DELETE", path, &[200, 202, 204], self.is_dir(path), &Headers::new(), None).map(drop)
    }

    fn rename(&self, src: &str, dst: &str) -> Result<()> {
        let collection = self.is_dir(src);
        let target = format!("{}{}{}", self.http.origin(), self.url_path(dst), if collection { "/" } else { "" });
        let headers = Headers::new().with("Destination", target).with("Overwrite", "F");
        self.request("MOVE", src, &[201, 204], collection, &headers, None).map(drop)
    }

    fn set_mtime(&self, path: &str, mtime: f64) -> Result<()> {
        let known = *self.mtime_method.lock().unwrap_or_else(|e| e.into_inner());
        let methods: Vec<&'static str> = match known {
            Some(m) => vec![m],
            None => MTIME_PROPERTIES.iter().map(|(name, _)| *name).collect(),
        };
        let headers = Headers::new().with("Content-Type", "application/xml; charset=utf-8");
        for method in methods {
            let Some((_, body)) = MTIME_PROPERTIES.iter().find(|(name, _)| *name == method) else {
                return Ok(()); // this server accepts none of them
            };
            let data = match self.request("PROPPATCH", path, &[207], false, &headers, Some(&body(mtime))) {
                Ok(data) => data,
                Err(Error::Io(_) | Error::Connection(_)) => continue,
                Err(e) => return Err(e),
            };
            let contains = |needle: &[u8]| data.windows(needle.len()).any(|w| w == needle);
            if !contains(b" 200") || [b" 403", b" 409", b" 424"].iter().any(|c| contains(*c)) {
                continue;
            }
            if known.is_none() {
                // Finding out: some servers just store the property and leave the file's date alone.
                match self.stat(path) {
                    Ok(e) if (e.mtime - mtime).abs() < 2.0 => {}
                    _ => continue,
                }
                *self.mtime_method.lock().unwrap_or_else(|e| e.into_inner()) = Some(method);
            }
            return Ok(());
        }
        if known.is_none() {
            *self.mtime_method.lock().unwrap_or_else(|e| e.into_inner()) = Some("");
        }
        Ok(())
    }
}

fn proppatch(namespace: &str, prefix: &str, name: &str, value: &str) -> Vec<u8> {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><d:propertyupdate xmlns:d=\"DAV:\" xmlns:{prefix}=\"{namespace}\">\
         <d:set><d:prop><{prefix}:{name}>{value}</{prefix}:{name}></d:prop></d:set></d:propertyupdate>"
    )
    .into_bytes()
}

type MtimeBody = fn(f64) -> Vec<u8>;

/// Ways servers accept modification times, tried in this order.
const MTIME_PROPERTIES: [(&str, MtimeBody); 3] = [
    // RFC 4918's own property (read-only on many servers, writable on some).
    ("getlastmodified", |t| proppatch("DAV:", "x", "getlastmodified", &format_http_date(t))),
    // Nextcloud / ownCloud: seconds since the epoch.
    ("lastmodified", |t| proppatch("DAV:", "x", "lastmodified", &(t as i64).to_string())),
    // IIS and Windows' WebDAV client.
    ("win32", |t| proppatch("urn:schemas-microsoft-com:", "z", "Win32LastModifiedTime", &format_http_date(t))),
];

fn dav_error(status: u16, reason: &str, what: &str) -> Error {
    let message = format!("{what}: {status} {reason}");
    let kind = match status {
        404 | 409 => io::ErrorKind::NotFound, // 409: e.g. the parent folder doesn't exist
        403 | 405 => io::ErrorKind::PermissionDenied,
        412 => io::ErrorKind::AlreadyExists,
        507 => io::ErrorKind::StorageFull,
        _ => io::ErrorKind::Other,
    };
    Error::os(kind, message)
}

/// Collects the written data and uploads it with one PUT when finished —
/// with a known length, which every server accepts (some refuse chunked
/// uploads). Dropping it without `finish` uploads nothing.
struct Upload {
    fs: Arc<Dav>,
    path: String,
    spool: tempfile::SpooledTempFile,
    len: u64,
}

impl Write for Upload {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let n = self.spool.write(data)?;
        self.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl WriteStream for Upload {
    fn finish(mut self: Box<Self>) -> oc_vfs::Result<()> {
        self.spool.seek(SeekFrom::Start(0))?;
        let url = self.fs.url_path(&self.path);
        let headers = Headers::new().with("Content-Type", "application/octet-stream");
        let len = self.len;
        let (response, _) = self.fs.send("PUT", &url, &headers, Body::Stream(&mut self.spool, len))?;
        if !matches!(response.status, 200 | 201 | 204) {
            return Err(dav_error(response.status, &response.reason, &format!("PUT {}", self.fs.display(&self.path))).into());
        }
        Ok(())
    }
}

/// A lazy walk: one PROPFIND per folder, when the walk gets there.
struct Walk {
    fs: Arc<Dav>,
    first: Option<Entry>,
    expand: Option<(String, RelPath)>,
    stack: Vec<(String, RelPath, std::vec::IntoIter<Entry>)>,
}

impl Iterator for Walk {
    type Item = oc_vfs::Result<(RelPath, Entry)>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(first) = self.first.take() {
            return Some(Ok((RelPath::new(), first)));
        }
        loop {
            if let Some((path, rel)) = self.expand.take() {
                match self.fs.list(&path) {
                    Ok(mut entries) => {
                        entries.sort_by(|a, b| a.name.cmp(&b.name));
                        self.stack.push((path, rel, entries.into_iter()));
                    }
                    Err(e) => {
                        self.stack.clear();
                        return Some(Err(e.into()));
                    }
                }
            }
            let (path, rel, entries) = self.stack.last_mut()?;
            let Some(entry) = entries.next() else {
                self.stack.pop();
                continue;
            };
            let mut child = rel.clone();
            child.push(entry.name.clone());
            if entry.is_dir {
                self.expand = Some((posix_join(path, &[&entry.name]), child.clone()));
            }
            return Some(Ok((child, entry)));
        }
    }
}

impl FileSystem for WebDavFileSystem {
    /// "webdavs://user@host" — the server, as in titles and messages.
    fn type_name(&self) -> String {
        self.inner.type_name.clone()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { writable: true, can_trash: false, can_rename: true, watchable: false }
    }

    fn identity(&self) -> String {
        self.inner.identity.clone()
    }

    fn list_dir(&self, path: &str, opts: ListOptions) -> oc_vfs::Result<Vec<Entry>> {
        Ok(sort_entries(self.inner.list(path)?, opts))
    }

    fn stat(&self, path: &str) -> oc_vfs::Result<Entry> {
        Ok(self.inner.stat(path)?)
    }

    fn parent_path(&self, path: &str) -> Option<String> {
        parent_abs(path)
    }

    fn join(&self, path: &str, names: &[&str]) -> String {
        posix_join(path, names)
    }

    fn basename(&self, path: &str) -> String {
        self.inner.basename(path)
    }

    fn display(&self, path: &str) -> String {
        self.inner.display(path)
    }

    fn open_read(&self, path: &str) -> oc_vfs::Result<Box<dyn ReadStream>> {
        let (fs, url, path) = (self.inner.clone(), self.inner.url_path(path), path.to_string());
        let stat_fs = self.inner.clone();
        let stat_path = path.clone();
        let get = Box::new(move |headers: &Headers| -> Result<(Response, Payload)> {
            let (response, payload) = fs.http.request_stream("GET", &url, headers, Body::None)?;
            fs.check_auth(&response)?;
            if !matches!(response.status, 200 | 206 | 416) {
                return Err(dav_error(response.status, &response.reason, &format!("GET {}", fs.display(&path))));
            }
            Ok((response, payload))
        });
        let size_of = Box::new(move || stat_fs.stat(&stat_path).map(|e| e.size));
        Ok(Box::new(RangeReader::new(get, size_of)))
    }

    fn walk<'a>(&'a self, path: &str) -> oc_vfs::Result<WalkIter<'a>> {
        let root = self.inner.stat(path)?;
        let expand = root.is_dir.then(|| (path.to_string(), RelPath::new()));
        Ok(Box::new(Walk { fs: self.inner.clone(), first: Some(root), expand, stack: Vec::new() }))
    }

    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }

    fn is_dir(&self, path: &str) -> bool {
        self.inner.is_dir(path)
    }

    /// From the folder's last listing (RFC 4331), if the server says.
    fn free_space(&self, path: &str) -> Option<u64> {
        self.inner.quota.lock().unwrap_or_else(|e| e.into_inner()).get(&norm_abs(path)).copied()
    }

    fn mkdir(&self, path: &str, exist_ok: bool) -> oc_vfs::Result<()> {
        Ok(self.inner.mkdir(path, exist_ok)?)
    }

    fn remove(&self, path: &str) -> oc_vfs::Result<()> {
        Ok(self.inner.remove(path)?)
    }

    fn rename(&self, src: &str, dst: &str) -> oc_vfs::Result<()> {
        Ok(self.inner.rename(src, dst)?)
    }

    fn open_write(&self, path: &str) -> oc_vfs::Result<Box<dyn WriteStream>> {
        Ok(Box::new(Upload {
            fs: self.inner.clone(),
            path: path.to_string(),
            spool: tempfile::spooled_tempfile(SPOOL_IN_MEMORY),
            len: 0,
        }))
    }

    /// Best effort: servers differ in which property, if any, they let clients set.
    fn set_mtime(&self, path: &str, mtime: f64) -> oc_vfs::Result<()> {
        Ok(self.inner.set_mtime(path, mtime)?)
    }

    fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        self.inner.http.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_hrefs() {
        let fs = WebDavFileSystem::new("nas", 80, false, "/dav/", Some("me"), None, None, TIMEOUT);
        assert_eq!(fs.url_path("/a b ü %.txt"), "/dav/a%20b%20%C3%BC%20%25.txt");
        assert_eq!(fs.url_path("x/../y/"), "/dav/y");
        assert_eq!(fs.url_path("/"), "/dav/");
        assert_eq!(fs.url_path("/!$&'()*+,;=:@~"), "/dav/!$&'()*+,;=:@~");
        assert_eq!(fs.display("/docs/"), "webdav://me@nas/dav/docs");
        assert_eq!(fs.basename("/"), "nas");
        assert_eq!(href_path("http://nas:8080/dav/x%20y/?a=b"), "/dav/x%20y/");
        assert_eq!(href_path("/dav/x"), "/dav/x");
        assert_eq!(href_path("https://nas"), "");
        let root = WebDavFileSystem::new("nas", 443, true, "", None, None, None, TIMEOUT);
        assert_eq!(root.url_path("/"), "/");
        assert_eq!(root.display("/a"), "webdavs://nas/a");
    }

    #[test]
    fn digest_challenges() {
        let params = auth_params(r#"realm="a b", nonce="xyz", qop="auth,auth-int", algorithm=MD5, stale=TRUE, Opaque="" "#);
        assert_eq!(params["realm"], "a b");
        assert_eq!(params["qop"], "auth,auth-int");
        assert_eq!(params["algorithm"], "MD5");
        assert_eq!(params["stale"], "TRUE");
        assert_eq!(params["opaque"], "");
        // RFC 2617's example.
        let auth = DavAuth::new(Some("Mufasa"), Some("Circle Of Life"));
        let c: HashMap<String, String> = [("realm", "testrealm@host.com"), ("nonce", "dcd98b7102dd2f0e8b11d0f600bfb0c093")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let header = auth.digest_header("GET", "/dir/index.html", &c, 1);
        // Without qop: response = MD5(HA1:nonce:HA2).
        assert!(header.contains("response=\"670fd8c2df070c60b045671b8b24ff02\""), "{header}");
    }
}
