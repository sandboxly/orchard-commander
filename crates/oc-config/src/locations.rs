//! Serializable descriptions of places ("specs"), for bookmarks and the like.
//!
//! A live location is an [`oc_vfs::VPath`], which holds open file systems and
//! can't be saved. A spec describes how to get there again:
//!
//! ```json
//! {"kind": "local", "path": "/Users/me/Projects"}
//! {"kind": "server", "account": "sftp://root@example.com", "path": "/var/www"}
//! {"kind": "archive", "archive": <spec of the archive file>, "path": "docs/2026"}
//! ```
//!
//! `account` is the connection's account, so a saved connection with its
//! keychain password is found again, and any connection type works. Archives
//! nest: the archive file's spec may itself be inside another archive or on a
//! server.
//!
//! A spec is kept as the JSON object it was read from (unknown kinds and
//! extra keys survive a load/save round trip); [`SpecRef::target`] gives the
//! typed view. Getting back to a live location is the app's job (connecting
//! may need a password prompt, opening an archive a worker thread), so this
//! module only describes; see [`Target`].

use std::path::{Component, Path};

use oc_vfs::{FileSystem, VPath};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::pyjson::py_str;

/// An owned location spec (a JSON object).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Spec(pub Map<String, Value>);

/// A borrowed spec: what all the reading functions work on, so a nested
/// archive spec needn't be copied out of its parent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpecRef<'a>(pub &'a Map<String, Value>);

/// What a valid spec points at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target<'a> {
    Local { path: &'a str },
    Server { account: &'a str, path: &'a str },
    Archive { archive: SpecRef<'a>, path: &'a str },
}

impl Spec {
    pub fn local(path: impl Into<String>) -> Spec {
        let mut m = Map::with_capacity(2);
        m.insert("kind".into(), "local".into());
        m.insert("path".into(), Value::String(path.into()));
        Spec(m)
    }

    pub fn server(account: impl Into<String>, path: impl Into<String>) -> Spec {
        let mut m = Map::with_capacity(3);
        m.insert("kind".into(), "server".into());
        m.insert("account".into(), Value::String(account.into()));
        m.insert("path".into(), Value::String(path.into()));
        Spec(m)
    }

    pub fn archive(archive: Spec, path: impl Into<String>) -> Spec {
        let mut m = Map::with_capacity(3);
        m.insert("kind".into(), "archive".into());
        m.insert("archive".into(), Value::Object(archive.0));
        m.insert("path".into(), Value::String(path.into()));
        Spec(m)
    }

    /// The spec in a JSON value, if it is an object (bookmark files keep only those).
    pub fn from_value(value: &Value) -> Option<Spec> {
        value.as_object().map(|m| Spec(m.clone()))
    }

    pub fn as_ref(&self) -> SpecRef<'_> {
        SpecRef(&self.0)
    }

    pub fn into_value(self) -> Value {
        Value::Object(self.0)
    }

    pub fn target(&self) -> Option<Target<'_>> {
        self.as_ref().target()
    }

    pub fn describe(&self) -> String {
        self.as_ref().describe()
    }

    pub fn name(&self) -> String {
        self.as_ref().name()
    }

    pub fn is_valid(&self) -> bool {
        self.as_ref().is_valid()
    }
}

impl From<Spec> for Value {
    fn from(spec: Spec) -> Value {
        spec.into_value()
    }
}

impl<'a> SpecRef<'a> {
    pub fn kind(self) -> Option<&'a str> {
        self.0.get("kind").and_then(Value::as_str)
    }

    pub fn to_owned(self) -> Spec {
        Spec(self.0.clone())
    }

    /// `str(spec.get(key, ""))`.
    fn text(self, key: &str) -> String {
        self.0.get(key).map(py_str).unwrap_or_default()
    }

    /// The nested archive spec; an empty one if missing or not an object.
    fn inner(self) -> SpecRef<'a> {
        static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
        match self.0.get("archive") {
            Some(Value::Object(m)) => SpecRef(m),
            _ => SpecRef(EMPTY.get_or_init(Map::new)),
        }
    }

    /// The typed view of a valid spec (see [`is_valid`](Self::is_valid)).
    pub fn target(self) -> Option<Target<'a>> {
        let path = self.0.get("path").and_then(Value::as_str);
        match self.kind()? {
            "local" => Some(Target::Local { path: path? }),
            "server" => {
                let account = self
                    .0
                    .get("account")
                    .and_then(Value::as_str)
                    .filter(|a| a.contains("://"))?;
                // A server spec without a path is valid; it means the account's start folder.
                Some(Target::Server {
                    account,
                    path: path.unwrap_or(""),
                })
            }
            "archive" => {
                let archive = self.inner();
                archive.is_valid().then_some(Target::Archive {
                    archive,
                    path: path?,
                })
            }
            _ => None,
        }
    }

    /// Human-readable: `~/Projects`, `sftp://root@host/var/www`, `~/a.zip/docs`.
    pub fn describe(self) -> String {
        self.describe_with_home(dirs::home_dir().as_deref())
    }

    /// [`describe`](Self::describe) with an explicit home directory (for tests).
    pub fn describe_with_home(self, home: Option<&Path>) -> String {
        match self.kind() {
            Some("local") => {
                let text = self.text("path");
                // `str(Path.home())`: pathlib drops a trailing slash of $HOME.
                let home = home.map(|h| h.components().collect::<std::path::PathBuf>());
                if let Some(home) = home.as_deref().and_then(Path::to_str) {
                    if text == home {
                        return "~".into();
                    }
                    if let Some(rest) = text.strip_prefix(home)
                        && rest.starts_with(std::path::MAIN_SEPARATOR)
                    {
                        return format!("~{rest}");
                    }
                }
                text
            }
            Some("server") => {
                let raw = self.text("path");
                let path = raw.trim_end_matches('/');
                let mut out = self.text("account");
                if !path.is_empty() && !path.starts_with('/') {
                    out.push('/');
                }
                out.push_str(path);
                out
            }
            Some("archive") => {
                let raw = self.text("path");
                let inner = raw.trim_matches('/');
                let mut out = self.inner().describe_with_home(home);
                if !inner.is_empty() {
                    out.push('/');
                    out.push_str(inner);
                }
                out
            }
            _ => "?".into(),
        }
    }

    /// A default title: the folder's (or archive's) name.
    pub fn name(self) -> String {
        let path = self.text("path");
        match self.kind() {
            // pathlib's `name`: like `file_name()`, except that ".." is a name there.
            Some("local") => match Path::new(&path).components().next_back() {
                Some(Component::Normal(name)) => name.to_string_lossy().into_owned(),
                Some(Component::ParentDir) => "..".into(),
                _ => path,
            },
            Some("archive") => match posix_basename(path.trim_matches('/')) {
                "" => self.inner().name(),
                name => name.to_owned(),
            },
            Some("server") => match posix_basename(path.trim_end_matches('/')) {
                "" => {
                    let account = self.text("account");
                    match account.split_once("://") {
                        Some((_, rest)) => rest.to_owned(),
                        None => account,
                    }
                }
                name => name.to_owned(),
            },
            _ => "?".into(),
        }
    }

    /// Whether the app can follow this spec.
    pub fn is_valid(self) -> bool {
        match self.kind() {
            Some("local") => matches!(self.0.get("path"), Some(Value::String(_))),
            Some("server") => self
                .0
                .get("account")
                .and_then(Value::as_str)
                .is_some_and(|a| a.contains("://")),
            Some("archive") => {
                matches!(self.0.get("archive"), Some(Value::Object(_)))
                    && self.inner().is_valid()
                    && matches!(self.0.get("path"), Some(Value::String(_)))
            }
            _ => false,
        }
    }

    /// A comparable form: equal for the same place written differently.
    fn normal(self) -> Normal {
        if self.0.is_empty() {
            return Normal::Nothing;
        }
        match self.kind() {
            Some("local") => Normal::Local(normcase(&normpath(&self.text("path")))),
            Some("server") => Normal::Server(
                self.text("account").to_lowercase(),
                posix_normpath(&format!("/{}", self.text("path"))),
            ),
            Some("archive") => Normal::Archive(
                Box::new(self.inner().normal()),
                self.text("path").trim_matches('/').to_owned(),
            ),
            _ => Normal::Other(self.0.clone()),
        }
    }
}

#[derive(PartialEq)]
enum Normal {
    Nothing,
    Local(String),
    Server(String, String),
    Archive(Box<Normal>, String),
    /// serde_json's map equality ignores key order, like comparing sorted items.
    Other(Map<String, Value>),
}

/// Whether two specs are the same place (trailing slashes, `..`, the account's
/// case and — on Windows — the path's case don't matter).
pub fn same_location(a: Option<SpecRef<'_>>, b: Option<SpecRef<'_>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.normal() == b.normal(),
        _ => false,
    }
}

/// Whether a JSON value is a valid spec.
pub fn is_valid(value: &Value) -> bool {
    value.as_object().is_some_and(|m| SpecRef(m).is_valid())
}

// ---------------------------------------------------------------------------
// From live locations
// ---------------------------------------------------------------------------

/// What the app knows about its file systems that [`FileSystem`] doesn't say.
pub trait SpecContext {
    /// Whether this is the local disk (paths are OS paths). Default: what
    /// the file system says ([`FileSystem::is_local`]).
    fn is_local(&self, fs: &dyn FileSystem) -> bool {
        fs.is_local()
    }
    /// The connection account of a server file system, if it is one the app
    /// knows (and can therefore reconnect to).
    fn account_of(&self, fs: &dyn FileSystem) -> Option<String>;
}

/// A spec for `location`, or `None` if it can't be described (e.g. a server
/// connection the context doesn't know).
pub fn spec_of(location: &VPath, ctx: &dyn SpecContext) -> Option<Spec> {
    let fs = location.fs.as_ref();
    if let Some(container) = fs.container() {
        let inner = spec_of(&container, ctx)?;
        return Some(Spec::archive(inner, location.path.clone()));
    }
    if ctx.is_local(fs) {
        return Some(Spec::local(location.path.clone()));
    }
    ctx.account_of(fs)
        .map(|account| Spec::server(account, location.path.clone()))
}

// ---------------------------------------------------------------------------
// Path helpers (Python's posixpath / ntpath)
// ---------------------------------------------------------------------------

/// `posixpath.basename`.
fn posix_basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `posixpath.normpath`.
pub(crate) fn posix_normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    // POSIX allows "//" to mean something else; three or more is "/".
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else if path.starts_with('/') {
        1
    } else {
        0
    };
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." if initial == 0 && parts.last().is_none_or(|p| *p == "..") => parts.push(".."),
            ".." => {
                parts.pop();
            }
            _ => parts.push(comp),
        }
    }
    let mut out = "/".repeat(initial);
    out.push_str(&parts.join("/"));
    if out.is_empty() { ".".into() } else { out }
}

/// `os.path.normpath` for local paths.
fn normpath(path: &str) -> String {
    if cfg!(windows) {
        nt_normpath(path)
    } else {
        posix_normpath(path)
    }
}

/// `os.path.normcase`: lower case with backslashes on Windows, unchanged elsewhere.
fn normcase(path: &str) -> String {
    if cfg!(windows) {
        path.replace('/', "\\").to_lowercase()
    } else {
        path.to_owned()
    }
}

/// `ntpath.normpath`, for drive-letter and UNC paths.
#[cfg_attr(not(windows), allow(dead_code))]
fn nt_normpath(path: &str) -> String {
    let path = path.replace('/', "\\");
    // Drive ("C:") or UNC share ("\\server\share") prefix.
    let (prefix, rest) = if let Some(unc) = path.strip_prefix("\\\\") {
        let parts: Vec<&str> = unc.splitn(3, '\\').collect();
        if parts.len() >= 2 {
            let len = 2 + parts[0].len() + 1 + parts[1].len();
            (&path[..len], &path[len..])
        } else {
            // "\\server" without a share: ntpath takes all of it as the drive.
            (path.as_str(), "")
        }
    } else if let Some(c) = path.chars().next()
        && path[c.len_utf8()..].starts_with(':')
    {
        // ntpath takes any character before the colon as a drive letter.
        path.split_at(c.len_utf8() + 1)
    } else {
        ("", path.as_str())
    };
    let rooted = rest.starts_with('\\');
    let mut parts: Vec<&str> = Vec::new();
    for comp in rest.split('\\') {
        match comp {
            "" | "." => {}
            ".." if rooted => {
                parts.pop();
            }
            ".." if parts.last().is_none_or(|p| *p == "..") => parts.push(".."),
            ".." => {
                parts.pop();
            }
            _ => parts.push(comp),
        }
    }
    let mut out = String::from(prefix);
    if rooted {
        out.push('\\');
    }
    out.push_str(&parts.join("\\"));
    if out.is_empty() { ".".into() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_vfs::{Entry, ListOptions, ReadStream, RelPath};
    use serde_json::json;
    use std::sync::Arc;

    fn local(p: &str) -> Spec {
        Spec::local(p)
    }

    fn spec(v: Value) -> Spec {
        Spec::from_value(&v).unwrap()
    }

    #[test]
    fn same_location_and_validation() {
        assert!(same_location(
            Some(local("/a/b").as_ref()),
            Some(local("/a/b/").as_ref())
        ));
        assert!(same_location(
            Some(local("/a/x/../b").as_ref()),
            Some(local("/a/./b").as_ref())
        ));
        assert!(!same_location(
            Some(local("/a/b").as_ref()),
            Some(local("/a/c").as_ref())
        ));
        let a = spec(json!({"kind": "server", "account": "SFTP://h", "path": "/x/"}));
        let b = spec(json!({"kind": "server", "account": "sftp://h", "path": "/x"}));
        assert!(same_location(Some(a.as_ref()), Some(b.as_ref())));
        assert!(!same_location(None, Some(local("/a").as_ref())));
        assert!(!same_location(None, None));
        let z1 = Spec::archive(local("/a.zip"), "/docs/");
        let z2 = Spec::archive(local("/a.zip/"), "docs");
        assert!(same_location(Some(z1.as_ref()), Some(z2.as_ref())));
        let o1 = spec(json!({"kind": "teleport", "a": 1, "b": 2}));
        let o2 = spec(json!({"b": 2, "a": 1, "kind": "teleport"}));
        assert!(same_location(Some(o1.as_ref()), Some(o2.as_ref())));

        assert!(is_valid(
            &json!({"kind": "archive", "archive": {"kind": "local", "path": "/a.zip"}, "path": ""})
        ));
        assert!(!is_valid(
            &json!({"kind": "server", "account": "nohost", "path": "/"})
        ));
        assert!(!is_valid(&json!({"kind": "teleport"})));
        assert!(!is_valid(&json!({"kind": "local", "path": 5})));
        assert!(!is_valid(
            &json!({"kind": "archive", "archive": {"kind": "x"}, "path": ""})
        ));
        assert!(!is_valid(&json!("local")));
    }

    #[test]
    fn targets() {
        assert_eq!(local("/a").target(), Some(Target::Local { path: "/a" }));
        let s = Spec::server("sftp://u@h", "/srv");
        assert_eq!(
            s.target(),
            Some(Target::Server {
                account: "sftp://u@h",
                path: "/srv"
            })
        );
        let z = Spec::archive(Spec::archive(s.clone(), "inner.zip"), "docs");
        let Some(Target::Archive { archive, path }) = z.target() else {
            panic!()
        };
        assert_eq!(path, "docs");
        let Some(Target::Archive { archive, path }) = archive.target() else {
            panic!()
        };
        assert_eq!((archive.to_owned(), path), (s, "inner.zip"));
        assert_eq!(spec(json!({"kind": "teleport"})).target(), None);
    }

    #[test]
    fn describe_and_names() {
        let home = Path::new("/home/me");
        let d = |s: &Spec| s.as_ref().describe_with_home(Some(home));
        assert_eq!(d(&local("/home/me")), "~");
        assert_eq!(d(&local("/home/me/Projects")), "~/Projects");
        assert_eq!(d(&local("/home/meier")), "/home/meier");
        let slash = Path::new("/home/me/"); // HOME=/home/me/
        assert_eq!(
            local("/home/me/x").as_ref().describe_with_home(Some(slash)),
            "~/x"
        );
        let root = Path::new("/"); // HOME=/ (e.g. in a container)
        assert_eq!(local("/").as_ref().describe_with_home(Some(root)), "~");
        assert_eq!(local("/x").as_ref().describe_with_home(Some(root)), "/x");
        assert_eq!(
            d(&Spec::server("sftp://root@web", "/var/www/")),
            "sftp://root@web/var/www"
        );
        assert_eq!(
            d(&Spec::server("mem://me@bucket", "projects")),
            "mem://me@bucket/projects"
        );
        assert_eq!(d(&Spec::server("sftp://h", "")), "sftp://h");
        let zip = Spec::archive(local("/home/me/pack.zip"), "docs/");
        assert_eq!(d(&zip), "~/pack.zip/docs");
        assert_eq!(d(&Spec::archive(local("/x/a.zip"), "")), "/x/a.zip");
        assert_eq!(d(&spec(json!({"kind": "?"}))), "?");

        assert_eq!(zip.name(), "docs");
        assert_eq!(Spec::archive(local("/x/a.zip"), "/").name(), "a.zip");
        assert_eq!(local("/a/b/").name(), "b");
        assert_eq!(local("/").name(), "/");
        assert_eq!(Spec::server("sftp://root@web", "/var/www/").name(), "www");
        assert_eq!(Spec::server("sftp://root@web", "/").name(), "root@web");
        assert_eq!(spec(json!({})).name(), "?");
    }

    #[test]
    fn normpath_like_python() {
        assert_eq!(posix_normpath(""), ".");
        assert_eq!(posix_normpath("/"), "/");
        assert_eq!(posix_normpath("//x/"), "//x");
        assert_eq!(posix_normpath("///x//y/"), "/x/y");
        assert_eq!(posix_normpath("/../a/./b/.."), "/a");
        assert_eq!(posix_normpath("../a/../../b"), "../../b");
        assert_eq!(posix_normpath("a/.."), ".");
        assert_eq!(nt_normpath("C:/Users//me/../you/"), "C:\\Users\\you");
        assert_eq!(nt_normpath("\\\\srv\\share\\a\\..\\b"), "\\\\srv\\share\\b");
        assert_eq!(nt_normpath("C:\\..\\x"), "C:\\x");
        // Checked against CPython's ntpath.normpath.
        assert_eq!(nt_normpath("\\\\srv"), "\\\\srv");
        assert_eq!(nt_normpath("\\\\srv\\share"), "\\\\srv\\share");
        assert_eq!(nt_normpath("C:a\\..\\..\\b"), "C:..\\b");
        assert_eq!(nt_normpath("\\a\\..\\.."), "\\");
        assert_eq!(nt_normpath(""), ".");
        assert_eq!(nt_normpath("é:\\x"), "é:\\x");
        assert_eq!(nt_normpath("é:a\\..\\.."), "é:..");
    }

    #[test]
    fn local_names_like_pathlib() {
        assert_eq!(local("/a/..").name(), "..");
        assert_eq!(local("/a/.").name(), "a");
        assert_eq!(local("").name(), "");
        assert_eq!(local("relative").name(), "relative");
    }

    #[test]
    fn damaged_specs_never_panic() {
        for v in [
            json!({"kind": "archive", "archive": "x", "path": 5}),
            json!({"kind": "archive"}),
            json!({"kind": "server", "account": null, "path": null}),
            json!({"kind": 5}),
            json!({"kind": "local", "path": ["a"]}),
        ] {
            let s = spec(v);
            let _ = (s.describe(), s.name(), s.is_valid(), s.target());
            assert!(same_location(Some(s.as_ref()), Some(s.as_ref())));
        }
        let s = spec(json!({"kind": "server", "account": null, "path": null}));
        assert_eq!(s.describe(), "None/None"); // Python's str(None)
        assert!(same_location(
            Some(spec(json!({})).as_ref()),
            Some(spec(json!({})).as_ref())
        ));
    }

    #[test]
    fn unknown_keys_survive_a_round_trip() {
        let v = json!({"kind": "local", "path": "/a", "note": "kept"});
        let s: Spec = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(serde_json::to_value(&s).unwrap(), v);
        assert_eq!(
            serde_json::to_string(&Spec::server("a://b", "/p")).unwrap(),
            r#"{"kind":"server","account":"a://b","path":"/p"}"#
        );
    }

    // -- spec_of with stand-in file systems -------------------------------------

    struct Fake {
        name: &'static str,
        container: Option<VPath>,
    }

    impl FileSystem for Fake {
        fn type_name(&self) -> String {
            self.name.into()
        }
        fn container(&self) -> Option<VPath> {
            self.container.clone()
        }
        fn list_dir(&self, _: &str, _: ListOptions) -> oc_vfs::Result<Vec<Entry>> {
            Ok(Vec::new())
        }
        fn stat(&self, p: &str) -> oc_vfs::Result<Entry> {
            Err(oc_vfs::VfsError::NotFound(p.into()))
        }
        fn parent_path(&self, _: &str) -> Option<String> {
            None
        }
        fn join(&self, path: &str, names: &[&str]) -> String {
            std::iter::once(path)
                .chain(names.iter().copied())
                .collect::<Vec<_>>()
                .join("/")
        }
        fn basename(&self, path: &str) -> String {
            posix_basename(path).into()
        }
        fn display(&self, path: &str) -> String {
            path.into()
        }
        fn is_local(&self) -> bool {
            self.name == "local"
        }
        fn open_read(&self, p: &str) -> oc_vfs::Result<Box<dyn ReadStream>> {
            Err(oc_vfs::VfsError::NotFound(p.into()))
        }
        fn walk<'a>(
            &'a self,
            _: &str,
        ) -> oc_vfs::Result<Box<dyn Iterator<Item = oc_vfs::Result<(RelPath, Entry)>> + 'a>>
        {
            Ok(Box::new(std::iter::empty()))
        }
    }

    struct Ctx(Option<&'static str>);

    impl SpecContext for Ctx {
        fn account_of(&self, _: &dyn FileSystem) -> Option<String> {
            self.0.map(String::from)
        }
    }

    fn fs(name: &'static str, container: Option<VPath>) -> Arc<dyn FileSystem> {
        Arc::new(Fake { name, container })
    }

    #[test]
    fn specs_for_local_archive_and_server() {
        let disk = fs("local", None);
        let none = Ctx(None);
        assert_eq!(
            spec_of(&VPath::new(disk.clone(), "/tmp/x"), &none),
            Some(local("/tmp/x"))
        );

        let zip = fs("zip", Some(VPath::new(disk, "/tmp/x/pack.zip")));
        let inside = spec_of(&VPath::new(zip, "docs"), &none).unwrap();
        assert_eq!(inside, Spec::archive(local("/tmp/x/pack.zip"), "docs"));
        assert!(inside.as_ref().describe().ends_with("pack.zip/docs"));
        assert_eq!(inside.name(), "docs");

        let server = fs("mem", None);
        let on_server = VPath::new(server.clone(), "projects");
        assert_eq!(spec_of(&on_server, &none), None); // unknown connection: can't be bookmarked
        let known = Ctx(Some("mem://me@bucket"));
        let s = spec_of(&on_server, &known).unwrap();
        assert_eq!(s, Spec::server("mem://me@bucket", "projects"));
        assert_eq!(s.describe(), "mem://me@bucket/projects");

        let nested = fs("zip", Some(VPath::new(server, "nested.zip")));
        let s = spec_of(&VPath::new(nested.clone(), ""), &known).unwrap();
        assert_eq!(
            s.0["archive"],
            Spec::server("mem://me@bucket", "nested.zip").into_value()
        );
        assert_eq!(spec_of(&VPath::new(nested, ""), &none), None);
    }
}
