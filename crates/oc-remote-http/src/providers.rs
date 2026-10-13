//! The WebDAV and S3 connection types. Port of `WebDAVProvider`,
//! `PlainWebDAVProvider`, `S3Provider` and `PlainS3Provider` from
//! `remote/providers.py`: same schemes, fields, labels and addresses, so
//! connections.json entries and keychain accounts carry over from 1.x.

use std::sync::Arc;

use oc_provider::{
    ConnectContext, ConnectError, Field, FieldKind, FileSystemProvider, Params, Value, join_server_url, param_int,
    param_str, quote, split_server_url, unquote,
};

use oc_vfs::FileSystem;

use crate::tls::CertificateQuestion;
use crate::{APP_NAME, live, s3, webdav};

/// Register the four connection types (keeps providers registered before,
/// e.g. add-ons, for the same schemes).
pub fn register_builtin() {
    let providers: [Arc<dyn FileSystemProvider>; 4] =
        [Arc::new(WebDavProvider), Arc::new(PlainWebDavProvider), Arc::new(S3Provider), Arc::new(PlainS3Provider)];
    for provider in providers {
        let _ = oc_provider::register(provider, false);
    }
}

/// Port from params; `default` if unset.
fn port(params: &Params, default: u16) -> Result<u16, ConnectError> {
    if param_str(params, "port").trim().is_empty() {
        return Ok(default);
    }
    param_int(params, "port")
        .and_then(|p| u16::try_from(p).ok())
        .ok_or_else(|| ConnectError::Connection(format!("Invalid port: {}", param_str(params, "port"))))
}

/// A param as an optional, non-empty string.
fn opt(params: &Params, key: &str) -> Option<String> {
    Some(param_str(params, key)).filter(|s| !s.is_empty())
}

/// Asks whether to trust a certificate the system doesn't (`tls::check_certificate`).
fn certificate_prompt(ctx: &dyn ConnectContext, default_port: u16) -> impl Fn(&CertificateQuestion) -> bool + '_ {
    move |q: &CertificateQuestion| {
        let shown = if q.port == default_port { q.host.clone() } else { format!("{}:{}", q.host, q.port) };
        ctx.confirm(
            "Untrusted certificate",
            &format!(
                "The certificate of {shown} isn't trusted by this computer ({}). \
                 Servers on a home network often use such self-made certificates.\n\n\
                 Certificate fingerprint:\n  {}\n\n\
                 If you know the server's fingerprint, check that it matches. Trusting it remembers this \
                 certificate; {APP_NAME} will refuse to connect if it ever changes.",
                q.problem, q.fingerprint
            ),
            "Trust and connect",
        )
    }
}

/// Runs the trait's default `validate` / `describe` for overriding providers.
struct Defaults(Vec<Field>);

impl FileSystemProvider for Defaults {
    fn scheme(&self) -> &'static str {
        ""
    }
    fn title(&self) -> &'static str {
        ""
    }
    fn fields(&self) -> Vec<Field> {
        self.0.clone()
    }
    fn parse_url(&self, _url: &str) -> Result<Params, String> {
        Err(String::new())
    }
    fn format_url(&self, _params: &Params) -> String {
        String::new()
    }
    fn connect(&self, _: &Params, _: Option<&str>, _: &dyn ConnectContext) -> Result<Arc<dyn FileSystem>, ConnectError> {
        Err(ConnectError::Cancelled)
    }
}

fn field(key: &'static str, label: &'static str, kind: FieldKind, required: bool, placeholder: &'static str) -> Field {
    Field { key, label, kind, required, placeholder, default: None }
}

fn start_path(params: &Params) -> String {
    format!("/{}", param_str(params, "path").trim_matches('/'))
}

// ---------------------------------------------------------------------------
// WebDAV
// ---------------------------------------------------------------------------

fn webdav_fields(port_placeholder: &'static str) -> Vec<Field> {
    vec![
        field("host", "Server", FieldKind::Text, true, "e.g. cloud.example.com"),
        field("port", "Port", FieldKind::Int, false, port_placeholder),
        field("root", "Server path", FieldKind::Text, false, "e.g. /remote.php/dav/files/USER for Nextcloud"),
        field("username", "User", FieldKind::Text, false, ""),
        field("path", "Start folder", FieldKind::Text, false, "below the server path"),
    ]
}

fn webdav_parse(url: &str, scheme: &str) -> Result<Params, String> {
    let (mut params, segments) = split_server_url(url, scheme)?;
    if !segments.is_empty() {
        // A typed address: its whole path is the root.
        params.insert("root".into(), Value::String(format!("/{}", segments.join("/"))));
    }
    Ok(params)
}

fn webdav_format(params: &Params, scheme: &str, default_port: u16) -> String {
    let root = param_str(params, "root");
    let path = param_str(params, "path");
    let full: Vec<&str> = [root.trim_matches('/'), path.trim_matches('/')].into_iter().filter(|p| !p.is_empty()).collect();
    let full = full.join("/");
    let suffix = if full.is_empty() { String::new() } else { format!("/{}", quote(&full, true)) };
    join_server_url(scheme, params, Some(i64::from(default_port)), &suffix)
}

fn webdav_validate(params: &Params, fields: Vec<Field>) -> Option<String> {
    let host = param_str(params, "host");
    if host.contains("://") || host.contains('/') {
        return Some("Enter just the server name; the rest of the address goes into Server path.".into());
    }
    Defaults(fields).validate(params)
}

fn webdav_connect(
    params: &Params,
    secret: Option<&str>,
    ctx: &dyn ConnectContext,
    secure: bool,
    default_port: u16,
) -> Result<Arc<dyn FileSystem>, ConnectError> {
    let options = webdav::WebDavOptions {
        secure,
        port: Some(port(params, default_port)?),
        root: opt(params, "root").unwrap_or_else(|| "/".into()),
        username: opt(params, "username"),
        password: secret.map(str::to_string),
        trusted_certificates: ctx.data_dir().join("trusted_certificates.json"),
        ..Default::default()
    };
    let prompt = certificate_prompt(ctx, 443);
    let fs = webdav::connect(&param_str(params, "host"), &options, &prompt)?;
    Ok(Arc::new(fs))
}

/// WebDAV over HTTPS: `webdavs://[user@]host[:port]/server/path/start/folder`
pub struct WebDavProvider;

impl FileSystemProvider for WebDavProvider {
    fn scheme(&self) -> &'static str {
        "webdavs"
    }
    fn title(&self) -> &'static str {
        "WebDAV (HTTPS)"
    }
    fn fields(&self) -> Vec<Field> {
        webdav_fields("default 443")
    }
    fn secret_hint(&self) -> &'static str {
        "empty: ask if the server wants one"
    }
    fn parse_url(&self, url: &str) -> Result<Params, String> {
        webdav_parse(url, self.scheme())
    }
    fn format_url(&self, params: &Params) -> String {
        webdav_format(params, self.scheme(), 443)
    }
    fn validate(&self, params: &Params) -> Option<String> {
        webdav_validate(params, self.fields())
    }
    fn connect(&self, params: &Params, secret: Option<&str>, ctx: &dyn ConnectContext) -> Result<Arc<dyn FileSystem>, ConnectError> {
        webdav_connect(params, secret, ctx, true, 443)
    }
    fn start_path(&self, params: &Params, _fs: &Arc<dyn FileSystem>) -> String {
        start_path(params)
    }
    fn is_connected(&self, fs: &Arc<dyn FileSystem>) -> bool {
        live::is_connected(&fs.identity())
    }
}

/// WebDAV over unencrypted HTTP (only sensible on a trusted local network).
pub struct PlainWebDavProvider;

impl FileSystemProvider for PlainWebDavProvider {
    fn scheme(&self) -> &'static str {
        "webdav"
    }
    fn title(&self) -> &'static str {
        "WebDAV (HTTP, unencrypted)"
    }
    fn fields(&self) -> Vec<Field> {
        webdav_fields("default 80")
    }
    fn secret_hint(&self) -> &'static str {
        "empty: ask if the server wants one"
    }
    fn parse_url(&self, url: &str) -> Result<Params, String> {
        webdav_parse(url, self.scheme())
    }
    fn format_url(&self, params: &Params) -> String {
        webdav_format(params, self.scheme(), 80)
    }
    fn validate(&self, params: &Params) -> Option<String> {
        webdav_validate(params, self.fields())
    }
    fn connect(&self, params: &Params, secret: Option<&str>, ctx: &dyn ConnectContext) -> Result<Arc<dyn FileSystem>, ConnectError> {
        webdav_connect(params, secret, ctx, false, 80)
    }
    fn start_path(&self, params: &Params, _fs: &Arc<dyn FileSystem>) -> String {
        start_path(params)
    }
    fn is_connected(&self, fs: &Arc<dyn FileSystem>) -> bool {
        live::is_connected(&fs.identity())
    }
}

// ---------------------------------------------------------------------------
// S3
// ---------------------------------------------------------------------------

fn s3_fields(host_placeholder: &'static str, port_placeholder: &'static str) -> Vec<Field> {
    vec![
        field("host", "Server", FieldKind::Text, false, host_placeholder),
        field("port", "Port", FieldKind::Int, false, port_placeholder),
        field("region", "Region", FieldKind::Text, false, "empty: automatic"),
        field("username", "Access key ID", FieldKind::Text, false, "empty: public buckets only"),
        field("path", "Start folder", FieldKind::Text, false, "bucket/folder · empty: list of buckets"),
    ]
}

/// The `region` of a query string, as `parse_qs(query).get("region")[0]`.
fn region_param(url: &str) -> Option<String> {
    let query = url.trim().split_once('?')?.1;
    let query = query.split('#').next().unwrap_or("");
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        let decode = |s: &str| unquote(&s.replace('+', " "));
        (decode(k) == "region" && !v.is_empty()).then(|| decode(v)).filter(|v| !v.is_empty())
    })
}

fn s3_parse(url: &str, scheme: &str) -> Result<Params, String> {
    let (mut params, mut segments) = split_server_url(url, scheme)?;
    let host = param_str(&params, "host");
    if !host.contains('.') && !host.contains(':') && !params.contains_key("port") && host != "localhost" {
        // s3://bucket/folder, as the AWS CLI writes it.
        params.insert("host".into(), Value::String(s3::AWS_HOST.into()));
        segments.insert(0, host);
    }
    if !segments.is_empty() {
        params.insert("path".into(), Value::String(segments.join("/")));
    }
    if let Some(region) = region_param(url) {
        params.insert("region".into(), Value::String(region));
    }
    Ok(params)
}

fn with_host(params: &Params) -> Params {
    let mut p = params.clone();
    if param_str(params, "host").is_empty() {
        p.insert("host".into(), Value::String(s3::AWS_HOST.into()));
    }
    p
}

fn s3_format(params: &Params, scheme: &str, default_port: u16) -> String {
    let path = param_str(params, "path");
    let path = path.trim_matches('/');
    let suffix = if path.is_empty() { String::new() } else { format!("/{}", quote(path, true)) };
    let mut url = join_server_url(scheme, &with_host(params), Some(i64::from(default_port)), &suffix);
    if let Some(region) = opt(params, "region") {
        url.push_str(&format!("?region={}", quote(&region, true)));
    }
    url
}

fn s3_validate(params: &Params, fields: Vec<Field>) -> Option<String> {
    let host = param_str(params, "host");
    if host.contains("://") || host.contains('/') {
        return Some("Enter just the server name (e.g. minio.local); the bucket goes into Start folder.".into());
    }
    Defaults(fields).validate(params)
}

fn s3_connect(
    params: &Params,
    secret: Option<&str>,
    ctx: &dyn ConnectContext,
    secure: bool,
    default_port: u16,
) -> Result<Arc<dyn FileSystem>, ConnectError> {
    let options = s3::S3Options {
        secure,
        port: Some(port(params, default_port)?),
        region: opt(params, "region"),
        access_key: opt(params, "username"),
        secret_key: secret.map(str::to_string),
        trusted_certificates: ctx.data_dir().join("trusted_certificates.json"),
        ..Default::default()
    };
    let host = opt(params, "host").unwrap_or_else(|| s3::AWS_HOST.into());
    let prompt = certificate_prompt(ctx, default_port);
    let fs = s3::connect(&host, &options, &prompt)?;
    Ok(Arc::new(fs))
}

/// Amazon S3 and S3-compatible storage: `s3://[access-key@]server[:port]/bucket/folder[?region=…]`
///
/// The server is s3.amazonaws.com for Amazon. As in the AWS CLI,
/// s3://bucket/folder (a bucket name instead of a server: no dot, no port)
/// means a bucket on Amazon.
pub struct S3Provider;

impl FileSystemProvider for S3Provider {
    fn scheme(&self) -> &'static str {
        "s3"
    }
    fn title(&self) -> &'static str {
        "S3 (Amazon S3 and compatible)"
    }
    fn fields(&self) -> Vec<Field> {
        s3_fields("empty: Amazon S3 · or e.g. minio.local", "default 443")
    }
    fn secret_label(&self) -> &'static str {
        "Secret key"
    }
    fn secret_hint(&self) -> &'static str {
        "empty: ask when connecting"
    }
    fn parse_url(&self, url: &str) -> Result<Params, String> {
        s3_parse(url, self.scheme())
    }
    fn format_url(&self, params: &Params) -> String {
        s3_format(params, self.scheme(), 443)
    }
    fn describe(&self, params: &Params) -> String {
        Defaults(Vec::new()).describe(&with_host(params))
    }
    fn validate(&self, params: &Params) -> Option<String> {
        s3_validate(params, self.fields())
    }
    fn connect(&self, params: &Params, secret: Option<&str>, ctx: &dyn ConnectContext) -> Result<Arc<dyn FileSystem>, ConnectError> {
        s3_connect(params, secret, ctx, true, 443)
    }
    fn start_path(&self, params: &Params, _fs: &Arc<dyn FileSystem>) -> String {
        start_path(params)
    }
    fn is_connected(&self, fs: &Arc<dyn FileSystem>) -> bool {
        live::is_connected(&fs.identity())
    }
}

/// S3 over unencrypted HTTP, e.g. a MinIO on the local network.
pub struct PlainS3Provider;

impl FileSystemProvider for PlainS3Provider {
    fn scheme(&self) -> &'static str {
        "s3+http"
    }
    fn title(&self) -> &'static str {
        "S3 (HTTP, unencrypted)"
    }
    fn fields(&self) -> Vec<Field> {
        s3_fields("e.g. minio.local or 192.168.1.20", "default 80")
    }
    fn secret_label(&self) -> &'static str {
        "Secret key"
    }
    fn secret_hint(&self) -> &'static str {
        "empty: ask when connecting"
    }
    fn parse_url(&self, url: &str) -> Result<Params, String> {
        s3_parse(url, self.scheme())
    }
    fn format_url(&self, params: &Params) -> String {
        s3_format(params, self.scheme(), 80)
    }
    fn describe(&self, params: &Params) -> String {
        Defaults(Vec::new()).describe(&with_host(params))
    }
    fn validate(&self, params: &Params) -> Option<String> {
        s3_validate(params, self.fields())
    }
    fn connect(&self, params: &Params, secret: Option<&str>, ctx: &dyn ConnectContext) -> Result<Arc<dyn FileSystem>, ConnectError> {
        s3_connect(params, secret, ctx, false, 80)
    }
    fn start_path(&self, params: &Params, _fs: &Arc<dyn FileSystem>) -> String {
        start_path(params)
    }
    fn is_connected(&self, fs: &Arc<dyn FileSystem>) -> bool {
        live::is_connected(&fs.identity())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params(v: serde_json::Value) -> Params {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn webdav_addresses() {
        let (dav, plain) = (WebDavProvider, PlainWebDavProvider);
        let p = dav.parse_url("webdavs://me@cloud.example.com/remote.php/dav/files/me").unwrap();
        assert_eq!(p, params(json!({"host": "cloud.example.com", "username": "me", "root": "/remote.php/dav/files/me"})));
        let mut saved = p.clone();
        saved.insert("path".into(), json!("Photos/2025"));
        assert_eq!(dav.format_url(&saved), "webdavs://me@cloud.example.com/remote.php/dav/files/me/Photos/2025");
        assert_eq!(dav.account(&saved), "webdavs://me@cloud.example.com/remote.php/dav/files/me");
        assert_eq!(param_str(&dav.parse_url(&dav.account(&saved)).unwrap(), "root"), "/remote.php/dav/files/me");
        assert_eq!(plain.format_url(&params(json!({"host": "nas", "port": 8080}))), "webdav://nas:8080");
        assert_eq!(plain.format_url(&params(json!({"host": "nas", "port": 80}))), "webdav://nas");
        assert_eq!(dav.format_url(&params(json!({"host": "nas", "port": "443", "root": "/a b/", "path": "ü"}))), "webdavs://nas/a%20b/%C3%BC");
        assert!(dav.validate(&params(json!({"host": "https://cloud.example.com"}))).is_some());
        assert_eq!(dav.validate(&params(json!({"host": "cloud.example.com"}))), None);
        assert_eq!(dav.validate(&params(json!({}))).as_deref(), Some("Enter server."));
        assert_eq!(dav.validate(&params(json!({"host": "x", "port": "8o"}))).as_deref(), Some("Port must be a number."));
        assert_eq!(dav.describe(&saved), "me@cloud.example.com");
        assert_eq!(dav.secret_label(), "Password");
        let fields: Vec<_> = plain.fields().iter().map(|f| (f.key, f.label, f.placeholder)).collect();
        assert_eq!(fields[1], ("port", "Port", "default 80"));
        assert_eq!(fields[2], ("root", "Server path", "e.g. /remote.php/dav/files/USER for Nextcloud"));
        let dummy: Arc<dyn FileSystem> = Arc::new(oc_vfs::testing::MemFS::new());
        assert_eq!(dav.start_path(&params(json!({"path": "docs/"})), &dummy), "/docs");
        assert_eq!(dav.start_path(&params(json!({})), &dummy), "/");
    }

    #[test]
    fn s3_addresses() {
        let (s3p, plain) = (S3Provider, PlainS3Provider);
        assert_eq!(
            s3p.parse_url("s3://my-bucket/some/folder").unwrap(),
            params(json!({"host": "s3.amazonaws.com", "path": "my-bucket/some/folder"}))
        );
        let p = s3p.parse_url("s3://AKIDEXAMPLE@fsn1.your-objectstorage.com/media/2025?region=fsn1").unwrap();
        assert_eq!(
            p,
            params(json!({"host": "fsn1.your-objectstorage.com", "username": "AKIDEXAMPLE", "path": "media/2025", "region": "fsn1"}))
        );
        assert_eq!(s3p.format_url(&p), "s3://AKIDEXAMPLE@fsn1.your-objectstorage.com/media/2025?region=fsn1");
        assert_eq!(s3p.account(&p), "s3://AKIDEXAMPLE@fsn1.your-objectstorage.com?region=fsn1");
        assert_eq!(param_str(&s3p.parse_url(&s3p.account(&p)).unwrap(), "region"), "fsn1");
        assert_eq!(s3p.format_url(&params(json!({"username": "AKIDEXAMPLE"}))), "s3://AKIDEXAMPLE@s3.amazonaws.com");
        assert_eq!(s3p.account(&params(json!({"host": "", "path": "b"}))), "s3://s3.amazonaws.com");
        assert_eq!(plain.parse_url("s3+http://minio:9000/bucket").unwrap(), params(json!({"host": "minio", "port": 9000, "path": "bucket"})));
        assert_eq!(plain.parse_url("s3+http://localhost/bucket").unwrap(), params(json!({"host": "localhost", "path": "bucket"})));
        assert_eq!(plain.format_url(&params(json!({"host": "nas", "port": 80}))), "s3+http://nas");
        assert!(s3p.validate(&params(json!({"host": "https://minio.local"}))).is_some());
        assert_eq!(s3p.validate(&params(json!({}))), None);
        assert_eq!(s3p.describe(&params(json!({"username": "AK"}))), "AK@s3.amazonaws.com");
        assert_eq!(s3p.secret_label(), "Secret key");
        assert_eq!(region_param("s3://h/b?x=1&region=eu%2Dwest+1"), Some("eu-west 1".into()));
        assert_eq!(region_param("s3://h/b?region="), None);
        let fields: Vec<_> = plain.fields().iter().map(|f| (f.key, f.placeholder)).collect();
        assert_eq!(fields[0], ("host", "e.g. minio.local or 192.168.1.20"));
    }

    #[test]
    fn registration() {
        register_builtin();
        register_builtin(); // twice is fine
        for scheme in ["webdavs", "webdav", "s3", "s3+http"] {
            assert_eq!(oc_provider::get(scheme).map(|p| p.scheme()), Some(scheme));
        }
        assert_eq!(oc_provider::scheme_of("S3+HTTP://minio/x").as_deref(), Some("s3+http"));
    }
}
