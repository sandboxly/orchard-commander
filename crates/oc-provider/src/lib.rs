//! Connection types for file systems: SFTP, SMB, WebDAV, S3, and whatever
//! else is registered. Port of `fsproviders.py`.
//!
//! A [`FileSystemProvider`] describes one kind of connection: its address
//! scheme (`sftp://`), the fields of its connection dialog, how to parse and
//! format addresses, and how to connect. Connecting returns an
//! `Arc<dyn FileSystem>`; from then on the app only uses that generic
//! interface. Params are the JSON objects stored in `connections.json`
//! (`oc_config::connections::Connection::params`), so values are strings or
//! integers exactly as the Python version wrote them.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use oc_vfs::FileSystem;
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
pub use serde_json::{Map, Value};

/// Connection params as stored in connections.json.
pub type Params = Map<String, Value>;

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The server rejected the credentials (or needs a secret we don't have).
    #[error("{0}")]
    AuthenticationRequired(String),
    /// The user declined a question while connecting (e.g. didn't trust a host key).
    #[error("Cancelled")]
    Cancelled,
    #[error("{0}")]
    Connection(String),
    #[error("{}", oc_vfs::io_display(.0))]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    Int,
    /// A local file, e.g. a key.
    Path,
}

/// One input in the connection dialog. Values are stored as connection params.
#[derive(Clone, Debug)]
pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: FieldKind,
    pub required: bool,
    pub placeholder: &'static str,
    pub default: Option<Value>,
}

impl Field {
    pub const fn text(key: &'static str, label: &'static str) -> Self {
        Field { key, label, kind: FieldKind::Text, required: false, placeholder: "", default: None }
    }
}

/// What a provider may use while connecting (on a worker thread).
pub trait ConnectContext: Send + Sync {
    /// Where the provider may keep files (e.g. trusted host keys) — the config dir.
    fn data_dir(&self) -> PathBuf;
    /// (title, message, ok label) -> whether the user agreed; Cancel is the default.
    /// Blocks the worker while the UI asks.
    fn confirm(&self, title: &str, message: &str, ok_label: &str) -> bool;
}

/// Read a param as a string ("" if missing; integers are formatted).
pub fn param_str(params: &Params, key: &str) -> String {
    match params.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

/// Read a param as an integer (strings of digits are accepted).
pub fn param_int(params: &Params, key: &str) -> Option<i64> {
    match params.get(key) {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

pub trait FileSystemProvider: Send + Sync {
    /// Address scheme, also the stored connection type ("sftp").
    fn scheme(&self) -> &'static str;
    /// Shown in the connection dialog.
    fn title(&self) -> &'static str;
    fn fields(&self) -> Vec<Field> {
        Vec::new()
    }
    fn secret_label(&self) -> &'static str {
        "Password"
    }
    fn secret_hint(&self) -> &'static str {
        "empty: ask when connecting"
    }

    /// `scheme://…` -> params. Errors with a message for malformed addresses.
    fn parse_url(&self, url: &str) -> Result<Params, String>;
    fn format_url(&self, params: &Params) -> String;

    /// Identifies the server account: which sessions are shared and under
    /// which name the secret is stored in the keychain. Default: the address
    /// without the start folder. Must match the Python version exactly.
    fn account(&self, params: &Params) -> String {
        let mut p = params.clone();
        p.insert("path".into(), Value::String(String::new()));
        self.format_url(&p)
    }

    /// Short human description, e.g. for password prompts: "root@server".
    fn describe(&self, params: &Params) -> String {
        let user = param_str(params, "username");
        let host = param_str(params, "host");
        if user.is_empty() { host } else { format!("{user}@{host}") }
    }

    /// An error message for the connection dialog, or None if the params are fine.
    fn validate(&self, params: &Params) -> Option<String> {
        for f in self.fields() {
            let v = param_str(params, f.key);
            if f.required && v.trim().is_empty() {
                return Some(format!("Enter the {}", f.label.to_lowercase()));
            }
            if f.kind == FieldKind::Int && !v.is_empty() && !v.chars().all(|c| c.is_ascii_digit()) {
                return Some(format!("{} must be a number", f.label));
            }
        }
        None
    }

    /// Open a file system (worker thread).
    fn connect(
        &self,
        params: &Params,
        secret: Option<&str>,
        ctx: &dyn ConnectContext,
    ) -> Result<Arc<dyn FileSystem>, ConnectError>;

    /// Where to start browsing after connecting.
    fn start_path(&self, params: &Params, _fs: &Arc<dyn FileSystem>) -> String {
        let p = param_str(params, "path");
        if p.is_empty() { "/".into() } else { p }
    }

    /// Whether an open file system can still be used (else it's reconnected).
    fn is_connected(&self, _fs: &Arc<dyn FileSystem>) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Helpers for typical server URLs: scheme://[user@]host[:port]/path
// ---------------------------------------------------------------------------

/// Characters Python's `urllib.parse.quote` (safe="/") escapes, minus "/".
const QUOTE: &AsciiSet = &CONTROLS
    .add(b' ').add(b'"').add(b'#').add(b'%').add(b'<').add(b'>').add(b'?').add(b'@')
    .add(b'[').add(b'\\').add(b']').add(b'^').add(b'`').add(b'{').add(b'|').add(b'}')
    .add(b'!').add(b'$').add(b'&').add(b'\'').add(b'(').add(b')').add(b'*').add(b'+')
    .add(b',').add(b':').add(b';').add(b'=').add(b'/');

/// Python `quote(s)` with its default safe="/" — used for user names in URLs.
pub fn quote(s: &str, safe_slash: bool) -> String {
    const QUOTE_SAFE_SLASH: &AsciiSet = &QUOTE.remove(b'/');
    utf8_percent_encode(s, if safe_slash { QUOTE_SAFE_SLASH } else { QUOTE }).to_string()
}

pub fn unquote(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// Parse the common parts; returns (params, path segments). Port of
/// `split_server_url` (urlsplit semantics: host lower-cased, port numeric).
pub fn split_server_url(url: &str, scheme: &str) -> Result<(Params, Vec<String>), String> {
    let url = url.trim();
    let (head, rest) = url.split_once("://").ok_or_else(|| format!("Not a {scheme}:// address: {url}"))?;
    if !head.eq_ignore_ascii_case(scheme) {
        return Err(format!("Not a {scheme}:// address: {url}"));
    }
    let (netloc, path) = match rest.find(['/', '?', '#']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    let (userinfo, hostport) = match netloc.rfind('@') {
        Some(i) => (Some(&netloc[..i]), &netloc[i + 1..]),
        None => (None, netloc),
    };
    let (host, port) = if let Some(stripped) = hostport.strip_prefix('[') {
        let end = stripped.find(']').ok_or_else(|| format!("No server in address: {url}"))?;
        let after = &stripped[end + 1..];
        (&stripped[..end], after.strip_prefix(':'))
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (hostport, None),
        }
    };
    if host.is_empty() {
        return Err(format!("No server in address: {url}"));
    }
    let mut params = Params::new();
    params.insert("host".into(), Value::String(host.to_lowercase()));
    if let Some(user) = userinfo.map(|u| u.split(':').next().unwrap_or(""))
        && !user.is_empty()
    {
        params.insert("username".into(), Value::String(unquote(user)));
    }
    if let Some(p) = port.filter(|p| !p.is_empty()) {
        let n: u16 = p.parse().map_err(|_| format!("Port must be a number: {p}"))?;
        if n != 0 {
            params.insert("port".into(), Value::from(n));
        }
    }
    let segments = path.split('/').filter(|s| !s.is_empty()).map(unquote).collect();
    Ok((params, segments))
}

/// Port of `join_server_url`.
pub fn join_server_url(scheme: &str, params: &Params, default_port: Option<i64>, path: &str) -> String {
    let user = param_str(params, "username");
    let user = if user.is_empty() { String::new() } else { format!("{}@", quote(&user, true)) };
    let port = match param_int(params, "port") {
        Some(p) if p != 0 && Some(p) != default_port => format!(":{p}"),
        _ => String::new(),
    };
    format!("{scheme}://{user}{}{port}{path}", param_str(params, "host"))
}

// ---------------------------------------------------------------------------
// Connection error messages
// ---------------------------------------------------------------------------

/// Why connecting failed, in words (lower case, for after a colon), from the
/// socket error's kind; `os_text` (the OS's own words) for other kinds.
pub fn describe_network_error(kind: std::io::ErrorKind, os_text: &str) -> String {
    use std::io::ErrorKind as K;
    match kind {
        K::TimedOut | K::WouldBlock => "no answer — check the address and your network".into(),
        K::HostUnreachable | K::NetworkUnreachable => {
            "the server can't be reached — check the address and your network".into()
        }
        K::ConnectionRefused => "the connection was refused — check the port and that the server is running".into(),
        K::ConnectionReset | K::ConnectionAborted => "the server closed the connection".into(),
        _ if os_text.is_empty() => kind.to_string(),
        _ => {
            let mut chars = os_text.chars();
            chars.next().map(|c| c.to_lowercase().chain(chars).collect()).unwrap_or_default()
        }
    }
}

/// "Can't connect to host:port: why". With `unreachable_local` (a LAN
/// address that couldn't be reached) on macOS, a hint about its Local
/// Network privacy setting, which silently blocks programs without that
/// permission from reaching devices on the LAN ("No route to host").
pub fn connect_error_message(host: &str, port: u16, reason: &str, unreachable_local: bool) -> String {
    let mut message = format!("Can't connect to {host}:{port}: {reason}");
    if cfg!(target_os = "macos") && unreachable_local {
        message.push_str(
            ". macOS may be blocking local network access: allow Orchard Commander in \
             System Settings → Privacy & Security → Local Network, then try again.",
        );
    }
    message
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

fn registry() -> &'static RwLock<Vec<Arc<dyn FileSystemProvider>>> {
    static R: OnceLock<RwLock<Vec<Arc<dyn FileSystemProvider>>>> = OnceLock::new();
    R.get_or_init(|| RwLock::new(Vec::new()))
}

/// Register a provider; with `replace`, an existing one for the same scheme is replaced.
pub fn register(provider: Arc<dyn FileSystemProvider>, replace: bool) -> Result<(), String> {
    let mut r = registry().write().unwrap();
    if let Some(i) = r.iter().position(|p| p.scheme() == provider.scheme()) {
        if !replace {
            return Err(format!("A provider for {}:// is already registered", provider.scheme()));
        }
        r[i] = provider;
    } else {
        r.push(provider);
    }
    Ok(())
}

pub fn unregister(scheme: &str) {
    registry().write().unwrap().retain(|p| p.scheme() != scheme);
}

/// Registered providers in registration order.
pub fn providers() -> Vec<Arc<dyn FileSystemProvider>> {
    registry().read().unwrap().clone()
}

pub fn get(scheme: &str) -> Option<Arc<dyn FileSystemProvider>> {
    let scheme = scheme.to_lowercase();
    registry().read().unwrap().iter().find(|p| p.scheme() == scheme).cloned()
}

/// The registered address schemes for hint texts: "sftp://, smb://, …".
pub fn address_examples() -> String {
    providers().iter().map(|p| format!("{}://", p.scheme())).collect::<Vec<_>>().join(", ")
}

/// The registered scheme `url` starts with, if any.
pub fn scheme_of(url: &str) -> Option<String> {
    let (head, _) = url.trim().split_once("://")?;
    get(head).map(|p| p.scheme().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_join() {
        let (p, segs) = split_server_url("sftp://Me%20X@Host.Example:2222/a/b%20c", "sftp").unwrap();
        assert_eq!(param_str(&p, "host"), "host.example");
        assert_eq!(param_str(&p, "username"), "Me X");
        assert_eq!(param_int(&p, "port"), Some(2222));
        assert_eq!(segs, ["a", "b c"]);
        assert_eq!(join_server_url("sftp", &p, Some(22), ""), "sftp://Me%20X@host.example:2222");
        assert!(split_server_url("smb://x", "sftp").is_err());
        assert!(split_server_url("sftp:///path", "sftp").is_err());
    }
}
