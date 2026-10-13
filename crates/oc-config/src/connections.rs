//! Saved server connections and their passwords.
//!
//! A [`Connection`] describes how to reach a server (no secrets): a
//! connection type (its URL scheme, e.g. "sftp") and that type's params.
//! Saved connections live in `connections.json` in the config directory;
//! passwords go to the OS credential store (macOS Keychain, Windows Credential
//! Manager, Secret Service on Linux), and only if the user asks for it.
//!
//! Parsing addresses, accounts and live sessions belong to the connection
//! providers, not here: the password functions take the account string the
//! provider computed (`sftp://user@host`), which is the keychain user name.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::bookmarks::new_id;
use crate::pyjson::{self, py_str, truthy};

/// The keychain service passwords are stored under.
pub const KEYRING_SERVICE: &str = "Orchard Commander";
/// Passwords saved before the rename; moved over when first read.
pub const LEGACY_KEYRING_SERVICE: &str = "fcmd";

/// How to reach a server: a provider type plus that provider's params (no secrets).
/// Field order is the file's key order.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Connection {
    /// Provider scheme, e.g. "sftp".
    pub kind: String,
    pub params: Map<String, Value>,
    pub name: String,
    pub id: String,
}

/// Empty strings and nulls are "not set" and never stored.
fn is_unset(v: &Value) -> bool {
    matches!(v, Value::Null) || v.as_str() == Some("")
}

impl Connection {
    /// A new connection with a fresh id; unset params are dropped.
    pub fn new(
        kind: impl Into<String>,
        params: Map<String, Value>,
        name: impl Into<String>,
    ) -> Self {
        let params = params.into_iter().filter(|(_, v)| !is_unset(v)).collect();
        Connection {
            kind: kind.into(),
            params,
            name: name.into(),
            id: new_id(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.params.get(key)
    }

    /// A string param ("" if missing or not a string).
    pub fn get_str(&self, key: &str) -> &str {
        self.params.get(key).and_then(Value::as_str).unwrap_or("")
    }

    /// A copy with params changed; setting one to "" or null removes it.
    pub fn with_params(&self, changes: impl IntoIterator<Item = (String, Value)>) -> Self {
        let mut params = self.params.clone();
        params.extend(changes);
        params.retain(|_, v| !is_unset(v));
        Connection {
            params,
            ..self.clone()
        }
    }

    /// The account of a connection whose type isn't installed (an add-on):
    /// unique, so it never shares a session or password with another.
    pub fn fallback_account(&self) -> String {
        format!("{}://{}", self.kind, self.id)
    }

    fn from_json(item: &Value) -> Option<Connection> {
        let item = item.as_object()?;
        let kind = py_str(item.get("kind")?);
        let params = match item.get("params") {
            Some(Value::Object(p)) => p.clone(),
            // Saved before providers: params were top-level fields.
            _ => item
                .iter()
                .filter(|(k, v)| !matches!(k.as_str(), "kind" | "name" | "id") && !is_unset(v))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        };
        // Python kept a null name as None, which (being falsy) labels the
        // connection by its URL; "" does the same here, "None" would not.
        let name = match item.get("name") {
            None | Some(Value::Null) => String::new(),
            Some(v) => py_str(v),
        };
        let id = match item.get("id") {
            Some(v) if truthy(v) => py_str(v),
            _ => new_id(),
        };
        Some(Connection {
            kind,
            params,
            name,
            id,
        })
    }
}

// ---------------------------------------------------------------------------
// Saved connections
// ---------------------------------------------------------------------------

/// Parse a connections file's text; damaged files and entries are tolerated.
pub fn parse(text: &str) -> Vec<Connection> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(items)) => items.iter().filter_map(Connection::from_json).collect(),
        _ => Vec::new(),
    }
}

/// The saved connections in `path` (empty if there is no readable file).
pub fn load_saved(path: &Path) -> Vec<Connection> {
    fs::read_to_string(path)
        .map(|t| parse(&t))
        .unwrap_or_default()
}

/// Replace all saved connections.
pub fn save_all(path: &Path, items: &[Connection]) -> io::Result<()> {
    crate::paths::write_atomic(path, pyjson::dumps(items, true).as_bytes())
}

/// Add or update (by id); an updated connection moves to the end, as in the Python version.
pub fn save(path: &Path, conn: &Connection) -> io::Result<()> {
    let mut items = load_saved(path);
    items.retain(|c| c.id != conn.id);
    items.push(conn.clone());
    save_all(path, &items)
}

/// Remove a saved connection. Its password is the caller's to forget
/// ([`Passwords::forget`]), since only the provider knows its account.
pub fn remove(path: &Path, id: &str) -> io::Result<()> {
    let mut items = load_saved(path);
    items.retain(|c| c.id != id);
    save_all(path, &items)
}

// ---------------------------------------------------------------------------
// Credential stores
// ---------------------------------------------------------------------------

/// A failure of the credential store itself (locked, no backend, denied, …).
#[derive(Debug, thiserror::Error)]
#[error("credential store: {0}")]
pub struct CredentialError(pub String);

/// Where passwords live: the OS keychain ([`OsKeyring`]) or, in tests, memory.
pub trait CredentialStore: Send + Sync {
    /// The password for (service, account); `Ok(None)` if there is none.
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, CredentialError>;
    fn set(&self, service: &str, account: &str, password: &str) -> Result<(), CredentialError>;
    /// Deleting a missing entry is not an error.
    fn delete(&self, service: &str, account: &str) -> Result<(), CredentialError>;
}

/// Stands in for the OS keychain so tests never touch the real one.
#[derive(Debug, Default)]
pub struct MemoryCredentials {
    data: Mutex<HashMap<(String, String), String>>,
}

impl MemoryCredentials {
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything stored, as ((service, account), password), sorted.
    pub fn entries(&self) -> Vec<((String, String), String)> {
        let mut all: Vec<_> = self
            .lock()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        all.sort();
        all
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), String>> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl CredentialStore for MemoryCredentials {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, CredentialError> {
        Ok(self
            .lock()
            .get(&(service.to_owned(), account.to_owned()))
            .cloned())
    }

    fn set(&self, service: &str, account: &str, password: &str) -> Result<(), CredentialError> {
        self.lock().insert(
            (service.to_owned(), account.to_owned()),
            password.to_owned(),
        );
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), CredentialError> {
        self.lock()
            .remove(&(service.to_owned(), account.to_owned()));
        Ok(())
    }
}

/// The OS credential store, laid out like Python's `keyring` package so
/// passwords saved by the Python version are found (and vice versa):
/// macOS generic passwords (service, account) and Secret Service items
/// with `service`/`username` attributes map directly; Windows needs
/// [`windows_vault`] to follow Python's target-name scheme.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsKeyring;

fn cred_err(e: keyring::Error) -> CredentialError {
    CredentialError(e.to_string())
}

// Secret Service can hold several items for one (service, account): one
// written by Python's keyring (no `target` attribute) and found through
// keyring-rs's legacy fallback, plus ones from other tools. keyring-rs then
// reports `Ambiguous`; Python used the first match. Reading takes the first,
// writing and deleting cover all of them, so a forgotten password can't
// resurface from a duplicate.
#[cfg(not(windows))]
impl CredentialStore for OsKeyring {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, CredentialError> {
        match keyring::Entry::new(service, account).and_then(|e| e.get_password()) {
            Ok(p) => Ok(Some(p)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(keyring::Error::Ambiguous(creds)) => match creds.first() {
                Some(cred) => cred.get_password().map(Some).map_err(cred_err),
                None => Ok(None),
            },
            Err(e) => Err(cred_err(e)),
        }
    }

    fn set(&self, service: &str, account: &str, password: &str) -> Result<(), CredentialError> {
        match keyring::Entry::new(service, account).and_then(|e| e.set_password(password)) {
            Err(keyring::Error::Ambiguous(creds)) => creds
                .iter()
                .try_for_each(|c| c.set_password(password))
                .map_err(cred_err),
            other => other.map_err(cred_err),
        }
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), CredentialError> {
        let entry = keyring::Entry::new(service, account).map_err(cred_err)?;
        // Each call removes one match (or all of an ambiguous set); a later
        // round finds what the first search didn't (e.g. a legacy item).
        for _ in 0..4 {
            match entry.delete_credential() {
                Ok(()) => {}
                Err(keyring::Error::NoEntry) => return Ok(()),
                Err(keyring::Error::Ambiguous(creds)) => {
                    for cred in &creds {
                        match cred.delete_credential() {
                            Ok(()) | Err(keyring::Error::NoEntry) => {}
                            Err(e) => return Err(cred_err(e)),
                        }
                    }
                }
                Err(e) => return Err(cred_err(e)),
            }
        }
        Ok(())
    }
}

/// Python keyring's Windows layout: a credential's target name is the
/// service, holding one user name; further accounts of the same service go
/// to the target "account@service".
#[cfg(windows)]
mod windows_vault {
    use super::{CredentialError, cred_err};
    use keyring::{Entry, Error};

    fn entry(target: &str, service: &str, account: &str) -> Result<Entry, CredentialError> {
        Entry::new_with_target(target, service, account).map_err(cred_err)
    }

    fn compound(service: &str, account: &str) -> String {
        format!("{account}@{service}")
    }

    /// The password and user name stored under a target, if any.
    fn read(target: &str, service: &str) -> Result<Option<(String, String)>, CredentialError> {
        let e = entry(target, service, "")?;
        match e.get_password() {
            Ok(password) => {
                let user = e
                    .get_attributes()
                    .map_err(cred_err)?
                    .remove("username")
                    .unwrap_or_default();
                Ok(Some((password, user)))
            }
            Err(Error::NoEntry) => Ok(None),
            Err(err) => Err(cred_err(err)),
        }
    }

    pub fn get(service: &str, account: &str) -> Result<Option<String>, CredentialError> {
        if let Some((password, user)) = read(service, service)?
            && user == account
        {
            return Ok(Some(password));
        }
        Ok(read(&compound(service, account), service)?.map(|(p, _)| p))
    }

    pub fn set(service: &str, account: &str, password: &str) -> Result<(), CredentialError> {
        // Like Python: move what's under the plain target to its compound name first.
        if let Some((old, user)) = read(service, service)? {
            entry(&compound(service, &user), service, &user)?
                .set_password(&old)
                .map_err(cred_err)?;
        }
        entry(service, service, account)?
            .set_password(password)
            .map_err(cred_err)
    }

    pub fn delete(service: &str, account: &str) -> Result<(), CredentialError> {
        for target in [service.to_owned(), compound(service, account)] {
            if let Some((_, user)) = read(&target, service)?
                && user == account
            {
                match entry(&target, service, account)?.delete_credential() {
                    Ok(()) | Err(Error::NoEntry) => {}
                    Err(e) => return Err(cred_err(e)),
                }
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
impl CredentialStore for OsKeyring {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, CredentialError> {
        windows_vault::get(service, account)
    }

    fn set(&self, service: &str, account: &str, password: &str) -> Result<(), CredentialError> {
        windows_vault::set(service, account, password)
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), CredentialError> {
        windows_vault::delete(service, account)
    }
}

// ---------------------------------------------------------------------------
// Passwords (every failure means "no stored password")
// ---------------------------------------------------------------------------

/// Connection passwords by account, in a credential store.
#[derive(Debug, Default)]
pub struct Passwords<S: CredentialStore = OsKeyring> {
    store: S,
}

impl Passwords<OsKeyring> {
    /// Passwords in the OS keychain.
    pub fn os() -> Self {
        Passwords { store: OsKeyring }
    }
}

impl<S: CredentialStore> Passwords<S> {
    pub fn new(store: S) -> Self {
        Passwords { store }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// The stored password, if any. One saved under the old app name is moved over.
    pub fn get(&self, account: &str) -> Option<String> {
        if let Some(password) = self.store.get(KEYRING_SERVICE, account).ok()? {
            return Some(password);
        }
        let password = self.store.get(LEGACY_KEYRING_SERVICE, account).ok()??;
        if self.store.set(KEYRING_SERVICE, account, &password).is_ok() {
            let _ = self.store.delete(LEGACY_KEYRING_SERVICE, account);
        }
        Some(password)
    }

    /// Save a password; false if the credential store refused.
    pub fn set(&self, account: &str, password: &str) -> bool {
        self.store.set(KEYRING_SERVICE, account, password).is_ok()
    }

    /// Forget a password (if the store works and has one).
    pub fn forget(&self, account: &str) {
        let _ = self.store.delete(KEYRING_SERVICE, account);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn unset_params_are_dropped() {
        let c = Connection::new(
            "sftp",
            params(json!({"host": "h", "username": "", "port": 22, "key": null})),
            "",
        );
        assert_eq!(
            Value::Object(c.params.clone()),
            json!({"host": "h", "port": 22})
        );
        assert_eq!(c.id.len(), 32);
        let c2 = c.with_params([("path".into(), json!("/srv")), ("port".into(), json!(""))]);
        assert_eq!(
            Value::Object(c2.params.clone()),
            json!({"host": "h", "path": "/srv"})
        );
        assert_eq!(
            (c2.id.as_str(), c2.get_str("path"), c2.get_str("port")),
            (c.id.as_str(), "/srv", "")
        );
        assert_eq!(c.fallback_account(), format!("sftp://{}", c.id));
    }

    #[test]
    fn saved_connections_and_passwords() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cfg/connections.json");
        let passwords = Passwords::new(MemoryCredentials::new());
        let account = "sftp://alice@example.com";
        let conn = Connection::new(
            "sftp",
            params(json!({"host": "example.com", "username": "alice", "path": "/srv"})),
            "Prod",
        );
        save(&path, &conn).unwrap();
        save(
            &path,
            &Connection {
                name: "Production".into(),
                ..conn.clone()
            },
        )
        .unwrap(); // update, not duplicate
        let loaded = load_saved(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            (loaded[0].name.as_str(), loaded[0].id.as_str()),
            ("Production", conn.id.as_str())
        );
        assert_eq!(loaded[0].get_str("path"), "/srv");
        assert_eq!(passwords.get(account), None);
        assert!(passwords.set(account, "pw"));
        assert_eq!(passwords.get(account).as_deref(), Some("pw"));
        assert!(!fs::read_to_string(&path).unwrap().contains("pw")); // never in the JSON file
        remove(&path, &loaded[0].id).unwrap();
        passwords.forget(account);
        assert!(load_saved(&path).is_empty());
        assert_eq!(passwords.get(account), None);
    }

    #[test]
    fn password_saved_before_rename_is_moved() {
        let passwords = Passwords::new(MemoryCredentials::new());
        let account = "smb://alice@nas/Media";
        passwords
            .store()
            .set(LEGACY_KEYRING_SERVICE, account, "old-pw")
            .unwrap();
        assert_eq!(passwords.get(account).as_deref(), Some("old-pw"));
        assert_eq!(
            passwords.store().entries(),
            [(
                (KEYRING_SERVICE.to_string(), account.to_string()),
                "old-pw".to_string()
            )]
        );
    }

    struct Broken;

    impl CredentialStore for Broken {
        fn get(&self, _: &str, _: &str) -> Result<Option<String>, CredentialError> {
            Err(CredentialError("locked".into()))
        }
        fn set(&self, _: &str, _: &str, _: &str) -> Result<(), CredentialError> {
            Err(CredentialError("locked".into()))
        }
        fn delete(&self, _: &str, _: &str) -> Result<(), CredentialError> {
            Err(CredentialError("locked".into()))
        }
    }

    #[test]
    fn a_broken_store_means_no_password() {
        let passwords = Passwords::new(Broken);
        assert_eq!(passwords.get("a://b"), None);
        assert!(!passwords.set("a://b", "pw"));
        passwords.forget("a://b");
    }

    #[test]
    fn file_format_matches_python() {
        let conn = Connection {
            kind: "sftp".into(),
            params: params(json!({"host": "höst", "port": 22})),
            name: "N".into(),
            id: "abc".into(),
        };
        // json.dumps([asdict(c)], indent=2)
        let expected = "[\n  {\n    \"kind\": \"sftp\",\n    \"params\": {\n      \"host\": \"h\\u00f6st\",\n      \"port\": 22\n    },\n    \"name\": \"N\",\n    \"id\": \"abc\"\n  }\n]";
        assert_eq!(pyjson::dumps(&[conn], true), expected);
        assert_eq!(pyjson::dumps(&Vec::<Connection>::new(), true), "[]");
    }

    #[test]
    fn old_and_damaged_files() {
        let items = parse(
            r#"[{"kind": "sftp", "host": "h", "username": "", "port": 22, "name": "Old", "id": "x1"},
                {"name": "no kind"}, 5, {"kind": "smb", "params": {"host": "nas"}, "id": ""}]"#,
        );
        assert_eq!(items.len(), 2);
        assert_eq!(
            Value::Object(items[0].params.clone()),
            json!({"host": "h", "port": 22})
        );
        assert_eq!(
            (items[0].name.as_str(), items[0].id.as_str()),
            ("Old", "x1")
        );
        assert_eq!(
            (
                items[1].kind.as_str(),
                items[1].name.as_str(),
                items[1].id.len()
            ),
            ("smb", "", 32)
        );
        assert!(parse("{}").is_empty() && parse("garbage").is_empty());
        let items =
            parse(r#"[{"kind": "sftp", "params": {"host": "h"}, "name": null, "id": "i"}]"#);
        assert_eq!(items[0].name, ""); // labelled by its URL, not "None"
        // Python only filters unset values of old-style top-level params.
        let items = parse(r#"[{"kind": "sftp", "params": {"host": "h", "user": ""}}]"#);
        assert_eq!(items[0].get("user"), Some(&json!("")));
        assert!(load_saved(Path::new("/nonexistent/connections.json")).is_empty());
    }
}
