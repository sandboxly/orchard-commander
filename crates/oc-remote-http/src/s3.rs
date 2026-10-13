//! Amazon S3 and S3-compatible object storage (MinIO, Cloudflare R2, Backblaze
//! B2, Wasabi, Hetzner, DigitalOcean Spaces, Ceph, Garage, …). Port of
//! `remote/s3.py`.
//!
//! Plain HTTP via [`crate::http`] with AWS Signature Version 4, no SDK. The
//! file system's root lists the buckets; below that, "folders" are key
//! prefixes up to a "/", as in the AWS console:
//!
//! ```text
//! /                       buckets
//! /photos                 bucket "photos"
//! /photos/2025/beach.jpg  object "2025/beach.jpg" in it
//! ```
//!
//! S3 has no real folders. A folder exists while something is in it, or while
//! it has a marker object ("2025/", empty) — which is what mkdir creates, and
//! what deleting the last item of a folder leaves behind so the folder stays,
//! as on a disk. Renaming copies on the server and deletes the original.
//! Uploads go up in parts while they're written (multipart upload), so large
//! files need no local spool and a cancelled copy leaves nothing behind.
//!
//! Amazon itself is addressed per bucket (bucket.s3.REGION.amazonaws.com),
//! the region found out once per bucket. Other servers get path-style
//! requests (server/bucket/key) with the configured region; a server that
//! names the right region in its error is asked again with that one.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use base64::Engine;
use hmac::{Hmac, Mac};
use md5::Md5;
use oc_vfs::{Capabilities, Entry, FileSystem, ListOptions, ReadStream, RelPath, WalkIter, WriteStream, sort_entries};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use sha2::{Digest, Sha256};

use crate::dates::{amz_stamp, parse_http_date, parse_iso8601};
use crate::http::{Authorizer, Body, BodyKind, ClientOptions, Headers, HttpClient, Payload, RangeReader, Response, TIMEOUT};
use crate::tls::{CertificateQuestion, check_certificate};
use crate::webdav::hex;
use crate::{Error, Result, base_name, live, norm_abs, parent_abs, posix_join};

pub const AWS_HOST: &str = "s3.amazonaws.com";
pub const DEFAULT_REGION: &str = "us-east-1";
/// Upload part size; doubles every 1000 parts (S3 allows 10,000 parts).
pub const PART_SIZE: u64 = 16 * 1024 * 1024;
/// CopyObject's limit; bigger objects are copied in parts.
pub const COPY_LIMIT: u64 = 5 * 1024 * 1024 * 1024;
pub const COPY_PART_SIZE: u64 = 512 * 1024 * 1024;
/// Keys per DeleteObjects request.
pub const DELETE_BATCH: usize = 1000;

const NS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
const AUTH_ERRORS: [&str; 6] =
    ["InvalidAccessKeyId", "SignatureDoesNotMatch", "InvalidToken", "ExpiredToken", "InvalidSecurity", "TokenRefreshRequired"];
const REGION_ERRORS: [&str; 5] =
    ["PermanentRedirect", "AuthorizationHeaderMalformed", "IllegalLocationConstraintException", "TemporaryRedirect", "InvalidRegion"];

/// Python's `quote(s, safe="-_.~")`: everything but letters, digits and "-_.~" escaped.
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');
const KEY: &AsciiSet = &UNRESERVED.remove(b'/');

pub fn is_aws(host: &str) -> bool {
    host.ends_with(".amazonaws.com") || host.ends_with(".amazonaws.com.cn")
}

/// The region an endpoint names: s3.eu-central-1.amazonaws.com,
/// s3.us-west-004.backblazeb2.com, s3.eu-central-1.wasabisys.com; None for
/// global endpoints like s3.amazonaws.com.
pub fn region_of_host(host: &str) -> Option<String> {
    let rest = host.strip_prefix("s3")?.strip_prefix(['.', '-'])?;
    let rest = rest.strip_prefix("dualstack.").unwrap_or(rest);
    let (region, _) = rest.split_once('.')?;
    let parts: Vec<&str> = region.split('-').collect();
    let lower = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase());
    let ok = parts.len() >= 3
        && parts[0].len() == 2
        && lower(parts[0])
        && parts[1..parts.len() - 1].iter().all(|p| lower(p))
        && parts.last().is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    ok.then(|| region.to_string())
}

/// Whether the bucket can be a host name label under TLS (no dots: wildcard certificates).
fn dns_compatible(bucket: &str) -> bool {
    let b = bucket.as_bytes();
    let edge = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    (3..=63).contains(&b.len())
        && edge(b[0])
        && edge(b[b.len() - 1])
        && b.iter().all(|&c| edge(c) || c == b'-')
}

/// URI-encode an object key for the request path (S3 rules: "/" stays).
pub fn encode_key(key: &str) -> String {
    utf8_percent_encode(key, KEY).to_string()
}

fn quote_unreserved(s: &str) -> String {
    utf8_percent_encode(s, UNRESERVED).to_string()
}

fn unquote(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

fn unquote_plus(s: &str) -> String {
    unquote(&s.replace('+', " "))
}

// ---------------------------------------------------------------------------
// AWS Signature Version 4
// ---------------------------------------------------------------------------

type Clock = Box<dyn Fn() -> SystemTime + Send + Sync>;

/// Signs requests for one host and region.
pub struct Signer {
    /// The Host header: `host[:port]`.
    pub host: String,
    pub region: String,
    access_key: Option<String>,
    secret_key: Option<String>,
    now: Clock,
    /// The signing key of the last date.
    keys: Mutex<Option<(String, Vec<u8>)>>,
}

impl Signer {
    pub fn new(host: &str, region: &str, access_key: Option<&str>, secret_key: Option<&str>) -> Self {
        Signer {
            host: host.to_string(),
            region: region.to_string(),
            access_key: access_key.map(str::to_string),
            secret_key: secret_key.map(str::to_string),
            now: Box::new(SystemTime::now),
            keys: Mutex::new(None),
        }
    }

    /// With another clock (tests sign at fixed times).
    pub fn with_clock(mut self, now: impl Fn() -> SystemTime + Send + Sync + 'static) -> Self {
        self.now = Box::new(now);
        self
    }

    /// The Authorization header for a request whose headers are complete.
    pub fn authorization(&self, method: &str, url: &str, headers: &Headers, payload: &str, stamp: &str) -> String {
        let (path, query) = url.split_once('?').unwrap_or((url, ""));
        let mut signed: Vec<(String, String)> = Vec::new();
        for (k, v) in headers.iter() {
            let k = k.to_lowercase();
            if matches!(k.as_str(), "host" | "range" | "content-type" | "content-md5") || k.starts_with("x-amz-") {
                let v = v.split_whitespace().collect::<Vec<_>>().join(" ");
                match signed.iter_mut().find(|(name, _)| *name == k) {
                    Some(slot) => slot.1 = v,
                    None => signed.push((k, v)),
                }
            }
        }
        signed.sort();
        let names = signed.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
        let canonical_headers: String = signed.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
        let canonical = [method, path, &canonical_query(query), &canonical_headers, &names, payload].join("\n");
        let date = &stamp[..stamp.len().min(8)];
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let to_sign = format!("AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}", hex(&Sha256::digest(canonical.as_bytes())));
        let signature = hex(&hmac(&self.signing_key(date), to_sign.as_bytes()));
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={names}, Signature={signature}",
            self.access_key.as_deref().unwrap_or("")
        )
    }

    fn signing_key(&self, date: &str) -> Vec<u8> {
        let mut keys = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((d, key)) = keys.as_ref()
            && d == date
        {
            return key.clone();
        }
        let mut key = format!("AWS4{}", self.secret_key.as_deref().unwrap_or("")).into_bytes();
        for part in [date, self.region.as_str(), "s3", "aws4_request"] {
            key = hmac(&key, part.as_bytes());
        }
        *keys = Some((date.to_string(), key.clone()));
        key
    }
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

impl Authorizer for Signer {
    fn authorize(&self, method: &str, url: &str, headers: &mut Headers, body: BodyKind<'_>) {
        headers.set("Host", self.host.clone()); // sent as signed
        let payload = match body {
            BodyKind::Bytes(b) => hex(&Sha256::digest(b)),
            BodyKind::None => EMPTY_SHA256.to_string(),
            BodyKind::Stream => UNSIGNED_PAYLOAD.to_string(),
        };
        headers.set("x-amz-content-sha256", payload.clone());
        if self.access_key.as_deref().unwrap_or("").is_empty() {
            return; // anonymous: public buckets
        }
        let stamp = amz_stamp((self.now)());
        headers.set("x-amz-date", stamp.clone());
        let authorization = self.authorization(method, url, headers, &payload, &stamp);
        headers.set("Authorization", authorization);
    }
}

fn canonical_query(query: &str) -> String {
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|item| !item.is_empty())
        .map(|item| {
            let (k, v) = item.split_once('=').unwrap_or((item, ""));
            (quote_unreserved(&unquote(k)), quote_unreserved(&unquote(v)))
        })
        .collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

fn query_string(params: &[(&str, String)]) -> String {
    if params.is_empty() {
        return String::new();
    }
    let joined: Vec<String> = params.iter().map(|(k, v)| format!("{}={}", quote_unreserved(k), quote_unreserved(v))).collect();
    format!("?{}", joined.join("&"))
}

// ---------------------------------------------------------------------------
// Errors and XML
// ---------------------------------------------------------------------------

/// An S3 error document's code, message and region.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct S3Error {
    pub code: String,
    pub message: String,
    pub region: String,
}

impl S3Error {
    pub fn parse(data: &[u8]) -> S3Error {
        let text = String::from_utf8_lossy(data);
        let Ok(doc) = roxmltree::Document::parse(&text) else { return S3Error::default() };
        let mut root = doc.root_element();
        if !is_tag(&root, "Error")
            && let Some(nested) = root.children().find(|n| is_tag(n, "Error"))
        {
            root = nested;
        }
        S3Error { code: text_of(&root, "Code"), message: text_of(&root, "Message"), region: text_of(&root, "Region") }
    }
}

/// An element with this name, in the S3 namespace or none (servers differ).
fn is_tag(node: &roxmltree::Node, name: &str) -> bool {
    node.is_element() && node.tag_name().name() == name && matches!(node.tag_name().namespace(), None | Some(NS))
}

/// A child's text, with or without the S3 namespace.
fn text_of(node: &roxmltree::Node, name: &str) -> String {
    node.children().find(|n| is_tag(n, name)).and_then(|n| n.text()).unwrap_or("").to_string()
}

fn parse_xml<'a>(text: &'a str, what: &str) -> Result<roxmltree::Document<'a>> {
    roxmltree::Document::parse(text).map_err(|e| Error::os(io::ErrorKind::Other, format!("{what} sent an invalid answer: {e}")))
}

fn s3_error(status: u16, reason: &str, err: &S3Error, what: &str) -> Error {
    let detail = [err.message.as_str(), err.code.as_str(), reason].into_iter().find(|s| !s.is_empty()).unwrap_or("");
    let message = format!("{what}: {status} {detail}");
    let code = err.code.as_str();
    if AUTH_ERRORS.contains(&code) {
        return Error::AuthenticationRequired(format!("The server didn't accept the access key: {detail}"));
    }
    let kind = if status == 404 || matches!(code, "NoSuchKey" | "NoSuchBucket" | "NoSuchUpload") {
        io::ErrorKind::NotFound
    } else if status == 403 {
        io::ErrorKind::PermissionDenied
    } else if matches!(code, "BucketAlreadyExists" | "BucketAlreadyOwnedByYou") {
        io::ErrorKind::AlreadyExists
    } else if code == "BucketNotEmpty" {
        io::ErrorKind::DirectoryNotEmpty
    } else if matches!(code, "InvalidBucketName" | "KeyTooLongError") {
        io::ErrorKind::InvalidInput
    } else if matches!(code, "QuotaExceeded" | "EntityTooLarge") || status == 507 {
        io::ErrorKind::StorageFull
    } else {
        io::ErrorKind::Other
    };
    Error::os(kind, message)
}

/// Copies and completed uploads can fail after "200 OK", with an error in the body.
fn raise_embedded_error(data: &[u8], what: &str) -> Result<()> {
    let head = &data[..data.len().min(200)];
    if head.windows(7).any(|w| w == b"<Error>") {
        return Err(s3_error(500, "", &S3Error::parse(data), what));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Connecting
// ---------------------------------------------------------------------------

/// How to reach an S3 server.
#[derive(Clone, Debug)]
pub struct S3Options {
    pub secure: bool,
    /// Default: 443 / 80.
    pub port: Option<u16>,
    /// None: from the server name, or found out per bucket (Amazon), else us-east-1.
    pub region: Option<String>,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub trusted_certificates: PathBuf,
    pub timeout: Duration,
}

impl Default for S3Options {
    fn default() -> Self {
        S3Options {
            secure: true,
            port: None,
            region: None,
            access_key: None,
            secret_key: None,
            trusted_certificates: PathBuf::from("trusted_certificates.json"),
            timeout: TIMEOUT,
        }
    }
}

/// Connect and check the credentials. Fails with `AuthenticationRequired`,
/// `CertificateChanged`, `CertificateRejected` or `Connection`.
pub fn connect(host: &str, options: &S3Options, confirm_certificate: &dyn Fn(&CertificateQuestion) -> bool) -> Result<S3FileSystem> {
    let access_key = options.access_key.as_deref().filter(|k| !k.is_empty());
    let secret_key = options.secret_key.as_deref().filter(|k| !k.is_empty());
    if let (Some(key), None) = (access_key, secret_key) {
        return Err(Error::AuthenticationRequired(format!("The secret key for {key} is needed")));
    }
    let port = options.port.unwrap_or(if options.secure { 443 } else { 80 });
    let pin = if options.secure {
        check_certificate(host, port, &options.trusted_certificates, confirm_certificate, options.timeout)?
    } else {
        None
    };
    let fs = S3FileSystem::new(host, port, options.secure, options.region.as_deref(), access_key, secret_key, pin, options.timeout);
    if let Err(e) = fs.check_credentials() {
        fs.close();
        return Err(e);
    }
    Ok(fs)
}

// ---------------------------------------------------------------------------
// The file system
// ---------------------------------------------------------------------------

/// Sizes of multipart uploads and copies (tests use S3's minimum of 5 MiB).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tuning {
    pub part_size: u64,
    pub copy_limit: u64,
    pub copy_part_size: u64,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning { part_size: PART_SIZE, copy_limit: COPY_LIMIT, copy_part_size: COPY_PART_SIZE }
    }
}

/// An S3 account. Cheap to clone (shared).
#[derive(Clone)]
pub struct S3FileSystem {
    inner: Arc<S3>,
}

struct S3 {
    host: String,
    port: u16,
    secure: bool,
    aws: bool,
    /// "" = find out (Amazon) / DEFAULT_REGION.
    region: String,
    access_key: Option<String>,
    secret_key: Option<String>,
    pin: Option<String>,
    timeout: Duration,
    clients: Mutex<HashMap<(String, String), HttpClient>>,
    /// bucket -> region
    regions: Mutex<HashMap<String, String>>,
    closed: Arc<AtomicBool>,
    label: String,
    type_name: String,
    identity: String,
    tuning: Mutex<Tuning>,
    route: Mutex<Option<(String, u16)>>,
    requests: AtomicU64,
}

/// One request to the S3 API.
struct Req<'a> {
    method: &'a str,
    bucket: Option<&'a str>,
    key: &'a str,
    query: Vec<(&'static str, String)>,
    headers: Headers,
    body: Option<&'a [u8]>,
    ok: &'static [u16],
    what: Option<String>,
    stream: bool,
}

impl<'a> Req<'a> {
    fn new(method: &'a str, bucket: Option<&'a str>, key: &'a str) -> Self {
        Req { method, bucket, key, query: Vec::new(), headers: Headers::new(), body: None, ok: &[200], what: None, stream: false }
    }

    fn query(mut self, name: &'static str, value: impl ToString) -> Self {
        self.query.push((name, value.to_string()));
        self
    }

    fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.set(name, value);
        self
    }

    fn body(mut self, body: &'a [u8]) -> Self {
        self.body = Some(body);
        self
    }

    fn ok(mut self, ok: &'static [u16]) -> Self {
        self.ok = ok;
        self
    }

    fn what(mut self, what: impl Into<String>) -> Self {
        self.what = Some(what.into());
        self
    }
}

/// A listed object or common prefix.
#[derive(Clone, Debug)]
struct Item {
    is_prefix: bool,
    name: String,
    size: String,
    last_modified: String,
}

impl S3FileSystem {
    /// A file system for a checked server (see [`connect`]).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host: &str,
        port: u16,
        secure: bool,
        region: Option<&str>,
        access_key: Option<&str>,
        secret_key: Option<&str>,
        pin: Option<String>,
        timeout: Duration,
    ) -> Self {
        let default = if secure { 443 } else { 80 };
        let access_key = access_key.filter(|k| !k.is_empty());
        let label = format!(
            "{}{host}{}",
            access_key.map(|k| format!("{k}@")).unwrap_or_default(),
            if port != default { format!(":{port}") } else { String::new() }
        );
        let closed = Arc::new(AtomicBool::new(false));
        S3FileSystem {
            inner: Arc::new(S3 {
                host: host.to_string(),
                port,
                secure,
                aws: is_aws(host),
                region: region.filter(|r| !r.is_empty()).map(str::to_string).or_else(|| region_of_host(host)).unwrap_or_default(),
                access_key: access_key.map(str::to_string),
                secret_key: secret_key.map(str::to_string),
                pin,
                timeout,
                clients: Mutex::new(HashMap::new()),
                regions: Mutex::new(HashMap::new()),
                identity: live::register("s3", &closed),
                closed,
                type_name: format!("{}://{label}", if secure { "s3" } else { "s3+http" }),
                label,
                tuning: Mutex::new(Tuning::default()),
                route: Mutex::new(None),
                requests: AtomicU64::new(0),
            }),
        }
    }

    /// Amazon's S3 (s3.amazonaws.com or a regional endpoint).
    pub fn is_aws(&self) -> bool {
        self.inner.aws
    }

    /// `access-key@host[:port]`.
    pub fn label(&self) -> &str {
        &self.inner.label
    }

    pub fn port(&self) -> u16 {
        self.inner.port
    }

    pub fn is_connected(&self) -> bool {
        !self.inner.closed.load(Ordering::SeqCst)
    }

    pub fn set_tuning(&self, tuning: Tuning) {
        *self.inner.tuning.lock().unwrap_or_else(|e| e.into_inner()) = tuning;
    }

    /// List the buckets once: wrong keys fail here, not at the first folder.
    /// Keys limited to some buckets may not list them, which is fine.
    pub fn check_credentials(&self) -> Result<()> {
        let what = format!("Connecting to {}", self.inner.label);
        match self.inner.request(Req::new("GET", None, "").what(what)) {
            Ok(_) => Ok(()),
            Err(e) if e.is_permission() => Ok(()),
            // Anonymous (public buckets): there are no keys to check, and
            // Amazon answers a redirect to its web site. The server answered.
            Err(Error::Io(_)) if self.inner.access_key.is_none() => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// The regions found out so far, by bucket.
    pub fn bucket_regions(&self) -> HashMap<String, String> {
        self.inner.regions.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Testing: send every request to this address over plain HTTP, keeping
    /// the Host headers and signatures (so Amazon's addressing can be tested
    /// against a local server).
    #[doc(hidden)]
    pub fn route_all_to(&self, host: &str, port: u16) {
        *self.inner.route.lock().unwrap_or_else(|e| e.into_inner()) = Some((host.to_string(), port));
    }

    /// Testing: the Host headers of the servers talked to so far.
    #[doc(hidden)]
    pub fn hosts_used(&self) -> Vec<String> {
        let clients = self.inner.clients.lock().unwrap_or_else(|e| e.into_inner());
        clients.values().filter_map(|c| c.authorizer().map(|_| c.host().to_string())).collect()
    }

    /// Testing: HTTP requests sent so far.
    #[doc(hidden)]
    pub fn request_count(&self) -> u64 {
        self.inner.requests.load(Ordering::Relaxed)
    }

    /// Testing: one raw request (e.g. listing multipart uploads).
    #[doc(hidden)]
    pub fn raw_request(&self, method: &str, bucket: Option<&str>, key: &str, query: &[(&'static str, &str)]) -> Result<(Response, Vec<u8>)> {
        let mut req = Req::new(method, bucket, key);
        for (k, v) in query {
            req = req.query(k, v);
        }
        let (response, payload) = self.inner.request(req)?;
        Ok((response, payload.into_bytes()?))
    }
}

impl S3 {
    fn display(&self, path: &str) -> String {
        format!("{}{}", self.type_name, norm_abs(path))
    }

    fn basename(&self, path: &str) -> String {
        let name = base_name(path);
        if name.is_empty() { self.host.clone() } else { name.to_string() }
    }

    fn tuning(&self) -> Tuning {
        *self.tuning.lock().unwrap_or_else(|e| e.into_inner())
    }

    // -- requests ------------------------------------------------------------------

    fn client(&self, host: &str, region: &str) -> Result<HttpClient> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::Connection(format!("Connection to {} was closed", self.label)));
        }
        let route = self.route.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        let key = (host.to_string(), region.to_string());
        if let Some(c) = clients.get(&key) {
            return Ok(c.clone());
        }
        let own = host == self.host;
        let default = if self.secure { 443 } else { 80 };
        // The Host header (signed): IPv6 addresses in brackets.
        let bracketed = if host.contains(':') { format!("[{host}]") } else { host.to_string() };
        let netloc = if own && self.port != default { format!("{bracketed}:{}", self.port) } else { bracketed };
        let signer = Signer::new(&netloc, region, self.access_key.as_deref(), self.secret_key.as_deref());
        let client = HttpClient::new(
            host,
            if own { self.port } else { 443 },
            ClientOptions {
                secure: if own { self.secure } else { true },
                pin: if own { self.pin.clone() } else { None },
                timeout: self.timeout,
                authorizer: Some(Box::new(signer)),
                connect_to: route,
            },
        );
        clients.insert(key, client.clone());
        Ok(client)
    }

    fn bucket_region(&self, bucket: &str) -> Result<String> {
        if let Some(r) = self.regions.lock().unwrap_or_else(|e| e.into_inner()).get(bucket) {
            return Ok(r.clone());
        }
        let mut region = if self.region.is_empty() { DEFAULT_REGION.to_string() } else { self.region.clone() };
        if self.aws && self.region.is_empty() {
            // Amazon tells, even without permission to the bucket.
            let host = if self.host.ends_with(".cn") { self.host.as_str() } else { AWS_HOST };
            let client = self.client(host, DEFAULT_REGION)?;
            self.requests.fetch_add(1, Ordering::Relaxed);
            let (response, _) = client.request("HEAD", &format!("/{}", encode_key(bucket)), &Headers::new(), Body::None)?;
            if let Some(r) = response.header("x-amz-bucket-region").filter(|r| !r.is_empty()) {
                region = r.to_string();
            }
        }
        self.regions.lock().unwrap_or_else(|e| e.into_inner()).insert(bucket.to_string(), region.clone());
        Ok(region)
    }

    /// The client and URL path for a bucket (None: the service) and key.
    fn target(&self, bucket: Option<&str>, key: &str) -> Result<(HttpClient, String)> {
        let Some(bucket) = bucket else {
            let region = if self.region.is_empty() { DEFAULT_REGION } else { &self.region };
            return Ok((self.client(&self.host, region)?, "/".into()));
        };
        let region = self.bucket_region(bucket)?;
        if self.aws {
            let suffix = if self.host.ends_with(".cn") { "amazonaws.com.cn" } else { "amazonaws.com" };
            let regional = format!("s3.{region}.{suffix}");
            if dns_compatible(bucket) {
                return Ok((self.client(&format!("{bucket}.{regional}"), &region)?, format!("/{}", encode_key(key))));
            }
            return Ok((self.client(&regional, &region)?, format!("/{}/{}", encode_key(bucket), encode_key(key))));
        }
        let path = if key.is_empty() { format!("/{}", encode_key(bucket)) } else { format!("/{}/{}", encode_key(bucket), encode_key(key)) };
        Ok((self.client(&self.host, &region)?, path))
    }

    fn request(&self, req: Req<'_>) -> Result<(Response, Payload)> {
        let query = query_string(&req.query);
        for attempt in 0..2 {
            let (client, url) = self.target(req.bucket, req.key)?;
            let url = format!("{url}{query}");
            let body = req.body.map_or(Body::None, Body::Bytes);
            self.requests.fetch_add(1, Ordering::Relaxed);
            let (response, payload) = if req.stream {
                client.request_stream(req.method, &url, &req.headers, body)?
            } else {
                let (r, d) = client.request(req.method, &url, &req.headers, body)?;
                (r, Payload::Data(d))
            };
            if req.ok.contains(&response.status) {
                return Ok((response, payload));
            }
            let data = payload.into_bytes().unwrap_or_default();
            let mut err = S3Error::parse(&data);
            let region = response.header("x-amz-bucket-region").map(str::to_string).filter(|r| !r.is_empty()).unwrap_or(err.region.clone());
            let wrong_region = matches!(response.status, 301 | 307 | 400) || REGION_ERRORS.contains(&err.code.as_str());
            if let Some(bucket) = req.bucket {
                let known = self.regions.lock().unwrap_or_else(|e| e.into_inner()).get(bucket).cloned();
                if attempt == 0 && !region.is_empty() && wrong_region && known.as_deref() != Some(region.as_str()) {
                    // Ask again where the server says the bucket is.
                    self.regions.lock().unwrap_or_else(|e| e.into_inner()).insert(bucket.to_string(), region);
                    continue;
                }
                if matches!(response.status, 301 | 307) && err.code.is_empty() {
                    err = S3Error {
                        code: "PermanentRedirect".into(),
                        message: format!("the bucket is in another region than {}", known.as_deref().unwrap_or("None")),
                        region: String::new(),
                    };
                }
            }
            let what = req.what.clone().unwrap_or_else(|| {
                format!("{} {}", req.method, self.display(&posix_join("/", &[req.bucket.unwrap_or(""), req.key])))
            });
            return Err(s3_error(response.status, &response.reason, &err, &what));
        }
        unreachable!("the loop returns or continues once")
    }

    fn request_data(&self, req: Req<'_>) -> Result<(Response, Vec<u8>)> {
        let (response, payload) = self.request(req)?;
        let data = payload.into_bytes().map_err(|e| Error::Connection(format!("{}: {e}", self.label)))?;
        Ok((response, data))
    }

    // -- reading -------------------------------------------------------------------

    /// Prefixes and objects below `prefix`, page by page, names decoded.
    fn list(self: &Arc<Self>, bucket: &str, prefix: &str, delimiter: bool, max_keys: Option<u32>) -> Lister {
        Lister {
            fs: self.clone(),
            bucket: bucket.to_string(),
            prefix: prefix.to_string(),
            delimiter,
            max_keys,
            token: None,
            items: VecDeque::new(),
            done: false,
        }
    }

    fn any_below(self: &Arc<Self>, bucket: &str, prefix: &str) -> Result<bool> {
        self.list(bucket, prefix, false, Some(1)).next().transpose().map(|item| item.is_some())
    }

    fn object_entry(name: &str, item: &Item) -> Entry {
        let size = if !item.size.is_empty() && item.size.bytes().all(|b| b.is_ascii_digit()) { item.size.parse().unwrap_or(0) } else { 0 };
        Entry::file(name, size, parse_iso8601(&item.last_modified).unwrap_or(0.0))
    }

    fn list_buckets(&self) -> Result<Vec<Entry>> {
        let (_, data) = self.request_data(Req::new("GET", None, "").what(format!("Listing the buckets of {}", self.label)))?;
        let text = String::from_utf8_lossy(&data);
        let doc = parse_xml(&text, &self.type_name)?;
        let root = doc.root_element();
        let Some(buckets) = root.children().find(|n| is_tag(n, "Buckets")) else { return Ok(Vec::new()) };
        Ok(buckets
            .children()
            .filter(|n| n.is_element())
            .filter_map(|b| {
                let name = text_of(&b, "Name");
                (!name.is_empty()).then(|| Entry::dir(name, parse_iso8601(&text_of(&b, "CreationDate")).unwrap_or(0.0)))
            })
            .collect())
    }

    fn list_dir(self: &Arc<Self>, path: &str) -> Result<Vec<Entry>> {
        let (bucket, key) = split(path);
        let Some(bucket) = bucket else { return self.list_buckets() };
        let prefix = if key.is_empty() { String::new() } else { format!("{key}/") };
        let mut entries = Vec::new();
        let mut found = prefix.is_empty();
        for item in self.list(bucket, &prefix, true, None) {
            let item = item?;
            found = true;
            let rest = item.name.strip_prefix(prefix.as_str()).map_or("", |r| r.trim_end_matches('/'));
            if rest.is_empty() || rest.contains('/') {
                continue; // the folder's own marker, or a name S3 allows but paths can't show
            }
            entries.push(if item.is_prefix { Entry::dir(rest, 0.0) } else { Self::object_entry(rest, &item) });
        }
        if !found {
            return Err(Error::os(io::ErrorKind::NotFound, format!("No such folder: {}", self.display(path))));
        }
        Ok(entries)
    }

    fn stat(self: &Arc<Self>, path: &str) -> Result<Entry> {
        let (bucket, key) = split(path);
        let Some(bucket) = bucket else { return Ok(Entry::dir(self.host.clone(), 0.0)) };
        if key.is_empty() {
            match self.request(Req::new("HEAD", Some(bucket), "").what(format!("Bucket {bucket}"))) {
                Ok(_) => {}
                Err(e) if e.is_permission() => {} // it exists, the key just may not look at it as a whole
                Err(e) => return Err(e),
            }
            return Ok(Entry::dir(bucket, 0.0));
        }
        let response = match self.request(Req::new("HEAD", Some(bucket), &key)) {
            Ok((response, _)) => response,
            Err(e) if e.is_not_found() => {
                if self.any_below(bucket, &format!("{key}/"))? {
                    return Ok(Entry::dir(self.basename(path), 0.0));
                }
                return Err(Error::os(io::ErrorKind::NotFound, format!("Not found: {}", self.display(path))));
            }
            Err(e) => return Err(e),
        };
        let size = response.header("Content-Length").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        let mtime = response.header("Last-Modified").and_then(parse_http_date).unwrap_or(0.0);
        Ok(Entry::file(self.basename(path), size, mtime))
    }

    /// Whether `path` exists; errors other than "not found" are errors, as in
    /// Python's `exists` (a rename must not overwrite a file it couldn't see).
    fn try_exists(self: &Arc<Self>, path: &str) -> Result<bool> {
        match self.stat(path) {
            Ok(_) => Ok(true),
            Err(e) if e.is_not_found() => Ok(false),
            Err(e) => Err(e),
        }
    }

    // -- writing -------------------------------------------------------------------

    fn put_marker(&self, bucket: &str, folder: &str) -> Result<()> {
        let key = format!("{folder}/");
        self.request(Req::new("PUT", Some(bucket), &key).body(b"").header("Content-Length", "0")).map(drop)
    }

    fn mkdir(self: &Arc<Self>, path: &str, exist_ok: bool) -> Result<()> {
        let (bucket, key) = split(path);
        let Some(bucket) = bucket else {
            return Err(Error::os(io::ErrorKind::AlreadyExists, format!("File exists: {}", self.display(path))));
        };
        if key.is_empty() {
            return self.create_bucket(bucket, exist_ok);
        }
        match self.stat(path) {
            Ok(entry) => {
                if exist_ok && entry.is_dir {
                    return Ok(());
                }
                return Err(Error::os(io::ErrorKind::AlreadyExists, format!("File exists: {}", self.display(path))));
            }
            Err(e) if e.is_not_found() => {}
            Err(e) => return Err(e),
        }
        match self.put_marker(bucket, &key) {
            Err(e) if e.is_not_found() => {
                // No such bucket: missing parents are created.
                self.create_bucket(bucket, true)?;
                self.put_marker(bucket, &key)
            }
            other => other,
        }
    }

    fn create_bucket(self: &Arc<Self>, bucket: &str, exist_ok: bool) -> Result<()> {
        if !exist_ok && self.try_exists(&format!("/{bucket}"))? {
            // us-east-1 answers "200 OK" to a second create.
            return Err(Error::os(io::ErrorKind::AlreadyExists, format!("File exists: {}", self.display(&format!("/{bucket}")))));
        }
        let region = if self.region.is_empty() { DEFAULT_REGION.to_string() } else { self.region.clone() };
        self.regions.lock().unwrap_or_else(|e| e.into_inner()).insert(bucket.to_string(), region.clone());
        let body = if region != DEFAULT_REGION && self.aws {
            // Elsewhere the server decides.
            format!("<CreateBucketConfiguration xmlns=\"{NS}\"><LocationConstraint>{region}</LocationConstraint></CreateBucketConfiguration>")
        } else {
            String::new()
        };
        let req = Req::new("PUT", Some(bucket), "")
            .body(body.as_bytes())
            .header("Content-Length", body.len().to_string())
            .what(format!("Creating bucket {bucket}"));
        match self.request(req) {
            Err(e) if e.is_exists() && exist_ok => Ok(()),
            other => other.map(drop),
        }
    }

    fn remove(self: &Arc<Self>, path: &str) -> Result<()> {
        let (bucket, key) = split(path);
        let Some(bucket) = bucket else {
            return Err(Error::os(io::ErrorKind::PermissionDenied, format!("Can't delete the list of buckets: {}", self.display(path))));
        };
        let entry = self.stat(path)?;
        if key.is_empty() {
            // A whole bucket: empty it, then delete it.
            let keys = self.list(bucket, "", false, None).map(|i| i.map(|i| i.name)).collect::<Result<Vec<_>>>()?;
            self.delete_keys(bucket, &keys)?;
            self.request(Req::new("DELETE", Some(bucket), "").ok(&[200, 204]).what(format!("Deleting bucket {bucket}")))?;
            self.regions.lock().unwrap_or_else(|e| e.into_inner()).remove(bucket);
            return Ok(());
        }
        if entry.is_dir {
            let keys = self.list(bucket, &format!("{key}/"), false, None).map(|i| i.map(|i| i.name)).collect::<Result<Vec<_>>>()?;
            self.delete_keys(bucket, &keys)?;
        } else {
            self.request(Req::new("DELETE", Some(bucket), &key).ok(&[200, 204]))?;
        }
        self.keep_folder(bucket, key_dirname(&key))
    }

    fn delete_keys(&self, bucket: &str, keys: &[String]) -> Result<()> {
        for batch in keys.chunks(DELETE_BATCH) {
            let objects: String = batch.iter().map(|k| format!("<Object><Key>{}</Key></Object>", xml_escape(k))).collect();
            let body = format!("<Delete><Quiet>true</Quiet>{objects}</Delete>");
            let md5 = base64::engine::general_purpose::STANDARD.encode(Md5::digest(body.as_bytes()));
            let req = Req::new("POST", Some(bucket), "")
                .query("delete", "")
                .body(body.as_bytes())
                .header("Content-MD5", md5)
                .header("Content-Type", "application/xml");
            let data = match self.request_data(req) {
                Ok((_, data)) => data,
                Err(e) if e.io_kind() == Some(io::ErrorKind::Other) => {
                    // A server without DeleteObjects: one at a time.
                    for k in batch {
                        self.request(Req::new("DELETE", Some(bucket), k).ok(&[200, 204]))?;
                    }
                    continue;
                }
                Err(e) => return Err(e),
            };
            if data.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let text = String::from_utf8_lossy(&data);
            let doc = parse_xml(&text, &self.type_name)?;
            if let Some(first) = doc.root_element().children().find(|n| is_tag(n, "Error")) {
                let code = text_of(&first, "Code");
                let status = if code == "AccessDenied" { 403 } else { 500 };
                let err = S3Error { code, message: text_of(&first, "Message"), region: String::new() };
                return Err(s3_error(status, "", &err, &format!("Deleting {}", text_of(&first, "Key"))));
            }
        }
        Ok(())
    }

    /// After deleting its last item, a folder keeps existing (as a marker).
    fn keep_folder(self: &Arc<Self>, bucket: &str, folder: &str) -> Result<()> {
        if folder.is_empty() || self.any_below(bucket, &format!("{folder}/"))? {
            return Ok(());
        }
        self.put_marker(bucket, folder)
    }

    fn rename(self: &Arc<Self>, src: &str, dst: &str) -> Result<()> {
        let ((sb, sk), (db, dk)) = (split(src), split(dst));
        let (Some(sb), Some(db)) = (sb, db) else {
            return Err(Error::os(io::ErrorKind::PermissionDenied, format!("Buckets can't be renamed: {}", self.display(src))));
        };
        if sk.is_empty() || dk.is_empty() {
            return Err(Error::os(io::ErrorKind::PermissionDenied, format!("Buckets can't be renamed: {}", self.display(src))));
        }
        if self.try_exists(dst)? {
            return Err(Error::os(io::ErrorKind::AlreadyExists, format!("File exists: {}", self.display(dst))));
        }
        let entry = self.stat(src)?;
        if entry.is_dir {
            let prefix = format!("{sk}/");
            let sources = self
                .list(sb, &prefix, false, None)
                .map(|i| i.map(|i| (i.size.parse::<u64>().unwrap_or(0), i.name)))
                .collect::<Result<Vec<_>>>()?;
            for (size, name) in &sources {
                // (A listing has only keys below the prefix; a server sending
                // others mustn't make us copy them somewhere odd.)
                let Some(rest) = name.strip_prefix(prefix.as_str()) else {
                    return Err(Error::os(io::ErrorKind::Other, format!("{} listed {name} below {prefix}", self.type_name)));
                };
                self.copy(sb, name, db, &format!("{dk}/{rest}"), *size)?;
            }
            let names: Vec<String> = sources.into_iter().map(|(_, n)| n).collect();
            self.delete_keys(sb, &names)?;
        } else {
            self.copy(sb, &sk, db, &dk, entry.size)?;
            self.request(Req::new("DELETE", Some(sb), &sk).ok(&[200, 204]))?;
        }
        self.keep_folder(sb, key_dirname(&sk))
    }

    /// A copy on the server: in one request up to 5 GB, in parts above.
    fn copy(self: &Arc<Self>, src_bucket: &str, src_key: &str, dst_bucket: &str, dst_key: &str, size: u64) -> Result<()> {
        let source = encode_key(&format!("{src_bucket}/{src_key}"));
        let tuning = self.tuning();
        if size <= tuning.copy_limit {
            let req = Req::new("PUT", Some(dst_bucket), dst_key).header("x-amz-copy-source", source).body(b"");
            let (_, data) = self.request_data(req)?;
            return raise_embedded_error(&data, &format!("Copying to {}", self.display(&format!("/{dst_bucket}/{dst_key}"))));
        }
        let mut upload = Multipart::start(self, dst_bucket, dst_key, Headers::new())?;
        let result = (|| {
            let mut start = 0;
            while start < size {
                let end = (start + tuning.copy_part_size).min(size) - 1;
                let req = Req::new("PUT", Some(dst_bucket), dst_key)
                    .query("partNumber", upload.etags.len() + 1)
                    .query("uploadId", &upload.id)
                    .body(b"")
                    .header("x-amz-copy-source", source.clone())
                    .header("x-amz-copy-source-range", format!("bytes={start}-{end}"));
                let (_, data) = self.request_data(req)?;
                raise_embedded_error(&data, "Copying a part")?;
                let text = String::from_utf8_lossy(&data);
                let doc = parse_xml(&text, &self.type_name)?;
                upload.etags.push(text_of(&doc.root_element(), "ETag"));
                start = end + 1;
            }
            upload.complete()
        })();
        if result.is_err() {
            upload.abort();
        }
        result
    }
}

/// "/bucket/a/b" -> (Some("bucket"), "a/b"); "/" -> (None, "").
fn split(path: &str) -> (Option<&str>, String) {
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    match parts.next() {
        Some(bucket) => (Some(bucket), parts.collect::<Vec<_>>().join("/")),
        None => (None, String::new()),
    }
}

/// `posixpath.dirname` of a key ("a/b" -> "a", "a" -> "").
fn key_dirname(key: &str) -> &str {
    key.rfind('/').map_or("", |i| &key[..i])
}

/// `xml.sax.saxutils.escape`.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Pages of a ListObjectsV2, fetched as the iteration gets there.
struct Lister {
    fs: Arc<S3>,
    bucket: String,
    prefix: String,
    delimiter: bool,
    max_keys: Option<u32>,
    token: Option<String>,
    items: VecDeque<Item>,
    done: bool,
}

impl Lister {
    fn fetch(&mut self) -> Result<()> {
        let mut req = Req::new("GET", Some(&self.bucket), "").query("list-type", 2).query("encoding-type", "url");
        if self.delimiter {
            req = req.query("delimiter", "/");
        }
        if !self.prefix.is_empty() {
            req = req.query("prefix", &self.prefix);
        }
        if let Some(n) = self.max_keys {
            req = req.query("max-keys", n);
        }
        if let Some(token) = &self.token {
            req = req.query("continuation-token", token);
        }
        let (_, data) = self.fs.request_data(req)?;
        let text = String::from_utf8_lossy(&data);
        let doc = roxmltree::Document::parse(&text)
            .map_err(|e| Error::os(io::ErrorKind::Other, format!("{} sent an invalid listing: {e}", self.fs.type_name)))?;
        let root = doc.root_element();
        let encoded = text_of(&root, "EncodingType") == "url";
        let decode = |s: String| if encoded { unquote_plus(&s) } else { s };
        for element in root.children() {
            let item = if is_tag(&element, "CommonPrefixes") {
                let name = decode(text_of(&element, "Prefix"));
                Item { is_prefix: true, name, size: String::new(), last_modified: String::new() }
            } else if is_tag(&element, "Contents") {
                Item {
                    is_prefix: false,
                    name: decode(text_of(&element, "Key")),
                    size: text_of(&element, "Size"),
                    last_modified: text_of(&element, "LastModified"),
                }
            } else {
                continue;
            };
            // Only what was asked for: removing a folder deletes what its
            // listing names, so a server ignoring the prefix mustn't widen it.
            if item.name.starts_with(self.prefix.as_str()) {
                self.items.push_back(item);
            }
        }
        let token = text_of(&root, "NextContinuationToken");
        // (The same token again would ask for the same page forever.)
        let repeated = self.token.as_deref() == Some(token.as_str());
        self.done = self.max_keys.is_some() || text_of(&root, "IsTruncated") != "true" || token.is_empty() || repeated;
        self.token = Some(token);
        Ok(())
    }
}

impl Iterator for Lister {
    type Item = Result<Item>;

    fn next(&mut self) -> Option<Result<Item>> {
        while self.items.is_empty() {
            if self.done {
                return None;
            }
            if let Err(e) = self.fetch() {
                self.done = true;
                return Some(Err(e));
            }
        }
        self.items.pop_front().map(Ok)
    }
}

/// One flat listing of every key below a folder (not one request per folder).
struct FlatWalk {
    lister: Lister,
    prefix: String,
    seen: HashSet<RelPath>,
    pending: VecDeque<(RelPath, Entry)>,
}

impl Iterator for FlatWalk {
    type Item = oc_vfs::Result<(RelPath, Entry)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(next) = self.pending.pop_front() {
                return Some(Ok(next));
            }
            let item = match self.lister.next()? {
                Ok(item) => item,
                Err(e) => return Some(Err(e.into())),
            };
            let Some(rest) = item.name.strip_prefix(self.prefix.as_str()) else { continue };
            let parts: Vec<String> = rest.split('/').map(str::to_string).collect();
            if parts[..parts.len() - 1].iter().any(String::is_empty) {
                continue; // "a//b": not reachable as a path
            }
            for i in 1..parts.len() {
                // Folders, before what's in them.
                if self.seen.insert(parts[..i].to_vec()) {
                    self.pending.push_back((parts[..i].to_vec(), Entry::dir(parts[i - 1].clone(), 0.0)));
                }
            }
            if let Some(name) = parts.last().filter(|n| !n.is_empty()) {
                // Not just a folder marker.
                let entry = S3::object_entry(name, &item);
                self.pending.push_back((parts, entry));
            }
        }
    }
}

/// A multipart upload: started, parts added, then completed or aborted.
struct Multipart {
    fs: Arc<S3>,
    bucket: String,
    key: String,
    id: String,
    etags: Vec<String>,
}

impl Multipart {
    fn start(fs: &Arc<S3>, bucket: &str, key: &str, headers: Headers) -> Result<Multipart> {
        let mut req = Req::new("POST", Some(bucket), key).query("uploads", "").body(b"");
        req.headers = headers;
        let (_, data) = fs.request_data(req)?;
        let text = String::from_utf8_lossy(&data);
        let id = parse_xml(&text, &fs.type_name).map(|doc| text_of(&doc.root_element(), "UploadId")).unwrap_or_default();
        if id.is_empty() {
            return Err(Error::os(io::ErrorKind::Other, format!("{} started no upload for {key}", fs.type_name)));
        }
        Ok(Multipart { fs: fs.clone(), bucket: bucket.into(), key: key.into(), id, etags: Vec::new() })
    }

    fn upload(&mut self, data: &[u8]) -> Result<()> {
        let req = Req::new("PUT", Some(&self.bucket), &self.key)
            .query("partNumber", self.etags.len() + 1)
            .query("uploadId", &self.id)
            .body(data)
            .header("Content-Length", data.len().to_string());
        let (response, _) = self.fs.request_data(req)?;
        self.etags.push(response.header("ETag").unwrap_or("").to_string());
        Ok(())
    }

    fn complete(&self) -> Result<()> {
        let parts: String = self
            .etags
            .iter()
            .enumerate()
            .map(|(i, etag)| format!("<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>", i + 1, xml_escape(etag)))
            .collect();
        let body = format!("<CompleteMultipartUpload>{parts}</CompleteMultipartUpload>");
        let req = Req::new("POST", Some(&self.bucket), &self.key)
            .query("uploadId", &self.id)
            .body(body.as_bytes())
            .header("Content-Type", "application/xml");
        let (_, data) = self.fs.request_data(req)?;
        raise_embedded_error(&data, &format!("Uploading {}", self.fs.display(&format!("/{}/{}", self.bucket, self.key))))
    }

    /// Best effort: the server drops abandoned uploads eventually (or a lifecycle rule does).
    fn abort(&self) {
        let req = Req::new("DELETE", Some(&self.bucket), &self.key).query("uploadId", &self.id).ok(&[200, 204]);
        let _ = self.fs.request(req);
    }
}

/// Writes go up in parts as they come (no local spool); small files in one
/// PUT when finished. Dropping it without `finish` uploads nothing.
struct Upload {
    fs: Arc<S3>,
    bucket: String,
    key: String,
    buffer: Vec<u8>,
    multipart: Option<Multipart>,
    content_type: String,
}

impl Upload {
    fn part_size(&self) -> usize {
        let parts = self.multipart.as_ref().map_or(0, |m| m.etags.len());
        let base = self.fs.tuning().part_size;
        (base << (parts / 1000).min(16)) as usize
    }

    fn send_parts(&mut self) -> Result<()> {
        while self.buffer.len() >= self.part_size() {
            let size = self.part_size();
            if self.multipart.is_none() {
                let headers = Headers::new().with("Content-Type", self.content_type.clone());
                self.multipart = Some(Multipart::start(&self.fs, &self.bucket, &self.key, headers)?);
            }
            if let Some(m) = self.multipart.as_mut() {
                m.upload(&self.buffer[..size])?; // kept until it's up
            }
            self.buffer.drain(..size);
        }
        Ok(())
    }

    fn complete(&mut self) -> Result<()> {
        match self.multipart.as_mut() {
            None => {
                let req = Req::new("PUT", Some(&self.bucket), &self.key)
                    .body(&self.buffer)
                    .header("Content-Type", self.content_type.clone())
                    .header("Content-Length", self.buffer.len().to_string());
                self.fs.request(req).map(drop)
            }
            Some(m) => {
                if !self.buffer.is_empty() {
                    m.upload(&self.buffer)?;
                }
                m.complete()
            }
        }
    }
}

impl Write for Upload {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(data);
        self.send_parts()?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl WriteStream for Upload {
    fn finish(mut self: Box<Self>) -> oc_vfs::Result<()> {
        let result = self.complete();
        if result.is_ok() {
            self.multipart = None; // done: nothing to abort
        }
        Ok(result?)
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        if let Some(m) = self.multipart.take() {
            m.abort();
        }
    }
}

impl FileSystem for S3FileSystem {
    /// "s3://key@host" — the server, as in titles and messages.
    fn type_name(&self) -> String {
        self.inner.type_name.clone()
    }

    /// Renaming is a copy on the server, then a delete: still no download.
    fn capabilities(&self) -> Capabilities {
        Capabilities { writable: true, can_trash: false, can_rename: true, watchable: false }
    }

    fn identity(&self) -> String {
        self.inner.identity.clone()
    }

    fn list_dir(&self, path: &str, opts: ListOptions) -> oc_vfs::Result<Vec<Entry>> {
        Ok(sort_entries(self.inner.list_dir(path)?, opts))
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
        let (bucket, key) = split(path);
        let Some(bucket) = bucket.filter(|_| !key.is_empty()).map(str::to_string) else {
            return Err(Error::os(io::ErrorKind::IsADirectory, format!("Is a folder: {}", self.inner.display(path))).into());
        };
        let fs = self.inner.clone();
        let get = Box::new(move |headers: &Headers| {
            let mut req = Req::new("GET", Some(&bucket), &key).ok(&[200, 206, 416]);
            req.headers = headers.clone();
            req.stream = true;
            fs.request(req)
        });
        let (fs, path) = (self.inner.clone(), path.to_string());
        let size_of = Box::new(move || fs.stat(&path).map(|e| e.size));
        Ok(Box::new(RangeReader::new(get, size_of)))
    }

    fn walk<'a>(&'a self, path: &str) -> oc_vfs::Result<WalkIter<'a>> {
        let root = self.inner.stat(path)?;
        let first = std::iter::once(Ok((RelPath::new(), root.clone())));
        if !root.is_dir {
            return Ok(Box::new(first));
        }
        let (bucket, key) = split(path);
        let Some(bucket) = bucket else {
            // Every bucket, each with one listing, as the walk gets there.
            let buckets = self.inner.list_buckets()?;
            let below = buckets.into_iter().flat_map(move |entry| {
                let name = entry.name.clone();
                let head = std::iter::once(Ok((vec![name.clone()], entry)));
                let rest: WalkIter<'a> = match self.walk(&format!("/{name}")) {
                    Ok(walk) => Box::new(walk.skip(1).map(move |item| {
                        item.map(|(rel, child)| {
                            let mut full = vec![name.clone()];
                            full.extend(rel);
                            (full, child)
                        })
                    })),
                    Err(e) => Box::new(std::iter::once(Err(e))),
                };
                head.chain(rest)
            });
            return Ok(Box::new(first.chain(below)));
        };
        let prefix = if key.is_empty() { String::new() } else { format!("{key}/") };
        let lister = self.inner.list(bucket, &prefix, false, None);
        let mut seen = HashSet::new();
        seen.insert(RelPath::new());
        Ok(Box::new(first.chain(FlatWalk { lister, prefix, seen, pending: VecDeque::new() })))
    }

    fn exists(&self, path: &str) -> bool {
        self.inner.try_exists(path).unwrap_or(false)
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
        let (bucket, key) = split(path);
        let Some(bucket) = bucket.filter(|_| !key.is_empty()) else {
            return Err(Error::os(
                io::ErrorKind::PermissionDenied,
                format!("Files go into a bucket, not next to the buckets: {}", self.inner.display(path)),
            )
            .into());
        };
        let content_type = mime_guess::from_path(&key).first_raw().unwrap_or("application/octet-stream").to_string();
        Ok(Box::new(Upload {
            fs: self.inner.clone(),
            bucket: bucket.to_string(),
            key: key.clone(),
            buffer: Vec::new(),
            multipart: None,
            content_type,
        }))
    }

    fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        let clients: Vec<HttpClient> = self.inner.clients.lock().unwrap_or_else(|e| e.into_inner()).drain().map(|(_, c)| c).collect();
        for client in clients {
            client.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn at(secs: u64) -> impl Fn() -> SystemTime + Send + Sync {
        move || UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// The credentials and time of the examples in AWS's S3 Signature Version 4 documentation.
    fn aws_signer() -> Signer {
        Signer::new("examplebucket.s3.amazonaws.com", "us-east-1", Some("AKIAIOSFODNN7EXAMPLE"), Some("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"))
            .with_clock(at(1_369_353_600)) // 2013-05-24T00:00:00Z
    }

    fn signature(headers: &Headers) -> String {
        headers.get("Authorization").unwrap_or("").rsplit("Signature=").next().unwrap_or("").to_string()
    }

    #[test]
    fn signatures_match_the_aws_documentation() {
        let mut headers = Headers::new().with("Range", "bytes=0-9");
        aws_signer().authorize("GET", "/test.txt", &mut headers, BodyKind::None);
        assert_eq!(signature(&headers), "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41");
        assert!(headers.get("Authorization").unwrap().contains("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,"));
        let mut headers = Headers::new();
        aws_signer().authorize("GET", "/?max-keys=2&prefix=J", &mut headers, BodyKind::None);
        assert_eq!(signature(&headers), "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7");
        let mut headers = Headers::new();
        aws_signer().authorize("GET", "/?lifecycle", &mut headers, BodyKind::None);
        assert_eq!(signature(&headers), "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543");
    }

    /// (method, url, extra headers, body, expected signature)
    type Case = (&'static str, String, &'static [(&'static str, &'static str)], Option<Vec<u8>>, &'static str);

    /// Signatures computed by botocore's S3SigV4Auth (and the Python port) for the same requests.
    #[test]
    fn signatures_match_botocore() {
        let cases: [Case; 8] = [
            ("GET", format!("/bucket/{}", encode_key("a b/ü+%.txt")), &[("Range", "bytes=5-")], None,
             "8b4c00fd8ededdfa419a08cb42d63d669b2562e74900f64a1431253651ae0c79"),
            ("GET", "/bucket?list-type=2&encoding-type=url&delimiter=%2F&prefix=a%20b%2F&continuation-token=x%2By%3D".into(), &[], None,
             "33512254e209ef95f6eb20241407199544fce90144e3549979349fc4ca8f33b7"),
            ("PUT", "/bucket/key.txt".into(), &[("Content-Type", "text/plain")], Some(b"data".to_vec()),
             "686cc9554ce29c1bb6681c43c02ffc2a3f55148e430646e79fd69a7b512028f9"),
            ("PUT", "/bucket/copy".into(), &[("x-amz-copy-source", "bucket/a%20b/%C3%BC.txt")], Some(Vec::new()),
             "809418ada1ee2d9282b325ca08695e5be0758ed2cea7a688fae4e734df395a49"),
            ("POST", "/bucket/big.bin?uploads=".into(), &[("Content-Type", "application/octet-stream")], Some(Vec::new()),
             "4ddf2a6e8fcf1fc263ef62dc905087c19c1b9efdcfbee061bdc0c4215e75d061"),
            ("PUT", "/bucket/big.bin?partNumber=2&uploadId=abc%2Bdef".into(), &[], Some(vec![b'x'; 1000]),
             "48c41999d8c751d5b2e613e3147095b85c342e36d5559e91c90fc8cde198d4dc"),
            ("DELETE", "/bucket/k?uploadId=abc".into(), &[], None,
             "e956d72ce140f663c4b628c5ec0f2ec4267768a26a116f89060dad708b099e44"),
            ("POST", "/bucket?delete=".into(), &[("Content-MD5", "abc=="), ("Content-Type", "application/xml")], Some(b"<Delete/>".to_vec()),
             "f5061321179766b3c5cbe0ec3b9f731de1526dea28916a03cb9d228a5ba20599"),
        ];
        for (method, url, extra, body, expected) in cases {
            let mut headers = Headers::new();
            for (k, v) in extra {
                headers.set(k, *v);
            }
            let signer = Signer::new("127.0.0.1:9000", "eu-central-1", Some("AKIDEXAMPLE"), Some("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"))
                .with_clock(at(1_791_793_815)); // 2026-10-12T08:30:15Z
            let kind = body.as_deref().map_or(BodyKind::None, BodyKind::Bytes);
            signer.authorize(method, &url, &mut headers, kind);
            assert_eq!(signature(&headers), expected, "{method} {url}");
            assert!(headers.get("Authorization").unwrap().starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20261012/eu-central-1/s3/aws4_request, SignedHeaders="));
        }
    }

    #[test]
    fn anonymous_requests_are_unsigned() {
        let mut headers = Headers::new();
        Signer::new("s3.amazonaws.com", "us-east-1", None, None).authorize("GET", "/", &mut headers, BodyKind::None);
        assert_eq!(headers.get("Authorization"), None);
        assert_eq!(headers.get("Host"), Some("s3.amazonaws.com"));
        assert_eq!(headers.get("x-amz-content-sha256"), Some(EMPTY_SHA256));
    }

    #[test]
    fn region_from_the_server_name() {
        assert_eq!(region_of_host("s3.eu-central-1.amazonaws.com").as_deref(), Some("eu-central-1"));
        assert_eq!(region_of_host("s3.us-west-004.backblazeb2.com").as_deref(), Some("us-west-004"));
        assert_eq!(region_of_host("s3-eu-west-1.amazonaws.com").as_deref(), Some("eu-west-1"));
        assert_eq!(region_of_host("s3.dualstack.ap-southeast-2.amazonaws.com").as_deref(), Some("ap-southeast-2"));
        assert_eq!(region_of_host("s3.amazonaws.com"), None);
        assert_eq!(region_of_host("minio.local"), None);
        assert_eq!(region_of_host("s3.fsn1.your-objectstorage.com"), None);
    }

    #[test]
    fn keys_buckets_and_errors() {
        assert_eq!(encode_key("a b/ü+%.txt"), "a%20b/%C3%BC%2B%25.txt");
        assert!(dns_compatible("euro-bucket") && !dns_compatible("dotted.bucket") && !dns_compatible("ab") && !dns_compatible("-ab"));
        assert_eq!(split("/b/a/c/"), (Some("b"), "a/c".to_string()));
        assert_eq!(split("/"), (None, String::new()));
        assert_eq!(key_dirname("a/b"), "a");
        assert_eq!(key_dirname("a"), "");
        let err = S3Error::parse(b"<?xml version=\"1.0\"?><Error><Code>NoSuchKey</Code><Message>gone</Message></Error>");
        assert_eq!((err.code.as_str(), err.message.as_str()), ("NoSuchKey", "gone"));
        let nested = S3Error::parse(format!("<ErrorResponse xmlns=\"{NS}\"><Error><Code>X</Code><Region>eu-west-1</Region></Error></ErrorResponse>").as_bytes());
        assert_eq!((nested.code.as_str(), nested.region.as_str()), ("X", "eu-west-1"));
        assert_eq!(S3Error::parse(b"not xml"), S3Error::default());
        let e = s3_error(404, "Not Found", &S3Error::default(), "GET s3://x/b/k");
        assert!(e.is_not_found() && e.to_string() == "GET s3://x/b/k: 404 Not Found");
        assert!(matches!(s3_error(403, "", &S3Error { code: "SignatureDoesNotMatch".into(), message: "Check your key".into(), region: String::new() }, "x"),
            Error::AuthenticationRequired(m) if m == "The server didn't accept the access key: Check your key"));
        assert!(raise_embedded_error(b"<?xml version=\"1.0\"?>\n<Error><Code>InternalError</Code></Error>", "Copying").is_err());
        assert!(raise_embedded_error(b"<CopyObjectResult><ETag>x</ETag></CopyObjectResult>", "Copying").is_ok());
        assert_eq!(canonical_query("b=2&a=1&a=0&c"), "a=0&a=1&b=2&c=");
        assert_eq!(query_string(&[("prefix", "a b/".into()), ("delete", String::new())]), "?prefix=a%20b%2F&delete=");
    }
}
