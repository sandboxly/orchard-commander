//! The `smb://` connection type (port of `SMBProvider` in `remote/providers.py`).

use std::sync::Arc;
use std::time::Duration;

use oc_provider::{
    ConnectContext, ConnectError, Field, FieldKind, FileSystemProvider, Params, Value, join_server_url, param_int,
    param_str, quote, split_server_url,
};
use oc_vfs::FileSystem;

use crate::fs::{ConnectOptions, DEFAULT_PORT, connect};

/// smb://[user@]host[:port]/share[/path] — one share on a Windows / Samba server.
#[derive(Clone, Copy, Debug, Default)]
pub struct SmbProvider;

fn field(key: &'static str, label: &'static str, kind: FieldKind, required: bool, placeholder: &'static str) -> Field {
    Field { key, label, kind, required, placeholder, default: None }
}

impl FileSystemProvider for SmbProvider {
    fn scheme(&self) -> &'static str {
        "smb"
    }

    fn title(&self) -> &'static str {
        "SMB (Windows / Samba share)"
    }

    fn fields(&self) -> Vec<Field> {
        vec![
            field("host", "Server", FieldKind::Text, true, "server name or address"),
            field("port", "Port", FieldKind::Int, false, "default 445"),
            field("share", "Share", FieldKind::Text, true, "e.g. media"),
            field("username", "User", FieldKind::Text, false, ""),
            field("path", "Start folder", FieldKind::Text, false, "folder inside the share"),
        ]
    }

    fn parse_url(&self, url: &str) -> Result<Params, String> {
        let (mut params, segments) = split_server_url(url, self.scheme())?;
        let Some((share, rest)) = segments.split_first() else {
            return Err("SMB addresses need a share: smb://server/share".into());
        };
        params.insert("share".into(), Value::String(share.clone()));
        if !rest.is_empty() {
            params.insert("path".into(), Value::String(rest.join("/")));
        }
        Ok(params)
    }

    fn format_url(&self, params: &Params) -> String {
        let share = param_str(params, "share");
        let share = share.trim_matches(['/', '\\']);
        let path = param_str(params, "path");
        let path = path.trim_matches('/');
        let mut suffix = format!("/{}", quote(share, true));
        if !path.is_empty() {
            suffix.push('/');
            suffix.push_str(&quote(path, true));
        }
        join_server_url(self.scheme(), params, Some(i64::from(DEFAULT_PORT)), &suffix)
    }

    fn connect(
        &self,
        params: &Params,
        secret: Option<&str>,
        _ctx: &dyn ConnectContext,
    ) -> Result<Arc<dyn FileSystem>, ConnectError> {
        let port = match param_int(params, "port") {
            None => DEFAULT_PORT,
            Some(p) => u16::try_from(p).map_err(|_| ConnectError::Connection(format!("Invalid port: {p}")))?,
        };
        let username = param_str(params, "username");
        let opts = ConnectOptions {
            port,
            username: (!username.is_empty()).then_some(username),
            password: secret.map(str::to_string),
            timeout: Duration::from_secs(15),
        };
        let share = param_str(params, "share");
        let fs = connect(&param_str(params, "host"), share.trim_matches(['/', '\\']), &opts)?;
        Ok(Arc::new(fs))
    }

    fn start_path(&self, params: &Params, _fs: &Arc<dyn FileSystem>) -> String {
        format!("/{}", param_str(params, "path").trim_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_provider::Map;
    use serde_json::json;

    fn params(v: serde_json::Value) -> Params {
        match v {
            Value::Object(m) => m,
            _ => Map::new(),
        }
    }

    /// Golden values computed with the Python version's SMBProvider.
    #[test]
    fn urls_round_trip_like_python() {
        let p = SmbProvider;
        let cases: &[(&str, serde_json::Value, &str, &str, &str)] = &[
            ("smb://nas/media", json!({"host": "nas", "share": "media"}), "smb://nas/media", "smb://nas/media", "nas"),
            (
                "smb://me%40corp@nas.local:4455/data/a%20b/c",
                json!({"host": "nas.local", "username": "me@corp", "port": 4455, "share": "data", "path": "a b/c"}),
                "smb://me%40corp@nas.local:4455/data/a%20b/c",
                "smb://me%40corp@nas.local:4455/data",
                "me@corp@nas.local",
            ),
            (
                "SMB://Alice@NAS:445/Media/x/",
                json!({"host": "nas", "username": "Alice", "port": 445, "share": "Media", "path": "x"}),
                "smb://Alice@nas/Media/x",
                "smb://Alice@nas/Media",
                "Alice@nas",
            ),
            (
                "smb://CORP%5Cbob@[fe80::1]:139/s%20h/d/e%23f",
                json!({"host": "fe80::1", "username": "CORP\\bob", "port": 139, "share": "s h", "path": "d/e#f"}),
                "smb://CORP%5Cbob@fe80::1:139/s%20h/d/e%23f",
                "smb://CORP%5Cbob@fe80::1:139/s%20h",
                "CORP\\bob@fe80::1",
            ),
            (
                "smb://u:pw@host/sh",
                json!({"host": "host", "username": "u", "share": "sh"}),
                "smb://u@host/sh",
                "smb://u@host/sh",
                "u@host",
            ),
        ];
        for (url, expected, formatted, account, described) in cases {
            let parsed = p.parse_url(url).unwrap();
            assert_eq!(Value::Object(parsed.clone()), *expected, "{url}");
            assert_eq!(p.format_url(&parsed), *formatted, "{url}");
            assert_eq!(p.account(&parsed), *account, "{url}");
            assert_eq!(p.describe(&parsed), *described, "{url}");
            assert_eq!(p.format_url(&p.parse_url(formatted).unwrap()), *formatted, "{url}");
        }
    }

    #[test]
    fn formatting_stored_params_like_python() {
        let p = SmbProvider;
        let a = params(json!({"host": "h", "share": "/a b/", "path": "/x/y z/", "username": "DOM\\u", "port": "445"}));
        assert_eq!(p.format_url(&a), "smb://DOM%5Cu@h/a%20b/x/y%20z");
        assert_eq!(p.account(&a), "smb://DOM%5Cu@h/a%20b");
        let b = params(json!({"host": "h", "share": "s", "port": "", "username": ""}));
        assert_eq!(p.format_url(&b), "smb://h/s");
        let c = params(json!({"host": "h", "share": "s", "port": 4455, "path": "ü/ß?"}));
        assert_eq!(p.format_url(&c), "smb://h:4455/s/%C3%BC/%C3%9F%3F");
        assert_eq!(p.account(&c), "smb://h:4455/s");
        // Saved connection from test_fsproviders.py.
        let saved = params(json!({"host": "192.168.1.20", "username": "alice", "share": "Media"}));
        assert_eq!(p.format_url(&saved), "smb://alice@192.168.1.20/Media");
        // The old connections.json format stored unset ports as null.
        let legacy = params(json!({"host": "192.168.1.20", "username": "alice", "port": null, "share": "Media"}));
        assert_eq!(p.format_url(&legacy), "smb://alice@192.168.1.20/Media");
        assert_eq!(p.account(&legacy), "smb://alice@192.168.1.20/Media");
        // Host case is kept for stored params; IPv6 hosts are not bracketed
        // (keychain accounts must match Python's byte for byte).
        let d = params(json!({"host": "H.Local", "share": "S", "username": "A"}));
        assert_eq!(
            (p.format_url(&d), p.account(&d), p.describe(&d)),
            ("smb://A@H.Local/S".to_string(), "smb://A@H.Local/S".to_string(), "A@H.Local".to_string())
        );
        let v6 = params(json!({"host": "fe80::1", "share": "s", "port": 4455}));
        assert_eq!(
            (p.format_url(&v6), p.account(&v6), p.describe(&v6)),
            ("smb://fe80::1:4455/s".to_string(), "smb://fe80::1:4455/s".to_string(), "fe80::1".to_string())
        );
    }

    #[test]
    fn accounts_identify_server_and_share() {
        let p = SmbProvider;
        let a = p.parse_url("smb://u@nas/share1/dir").unwrap();
        assert_eq!(p.account(&a), p.account(&p.parse_url("smb://u@nas:445/share1").unwrap()));
        assert_ne!(p.account(&a), p.account(&p.parse_url("smb://u@nas/share2").unwrap()));
    }

    #[test]
    fn bad_urls_and_validation() {
        let p = SmbProvider;
        assert_eq!(p.parse_url("smb://server").unwrap_err(), "SMB addresses need a share: smb://server/share");
        assert_eq!(p.parse_url("smb://server/").unwrap_err(), "SMB addresses need a share: smb://server/share");
        assert_eq!(p.parse_url("smb://").unwrap_err(), "No server in address: smb://");
        assert_eq!(p.parse_url("smb:///share").unwrap_err(), "No server in address: smb:///share");
        assert!(p.parse_url("sftp://server/x").is_err());
        assert_eq!(p.validate(&params(json!({"host": "", "share": "x"}))).as_deref(), Some("Enter server."));
        assert_eq!(p.validate(&params(json!({"host": "h"}))).as_deref(), Some("Enter share."));
        assert_eq!(
            p.validate(&params(json!({"host": "h", "share": "s", "port": "x1"}))).as_deref(),
            Some("Port must be a number.")
        );
        assert_eq!(p.validate(&params(json!({"host": "h", "share": "s"}))), None);
    }

    #[test]
    fn start_paths() {
        let p = SmbProvider;
        let fs: Arc<dyn FileSystem> = Arc::new(oc_vfs::testing::MemFS::new());
        assert_eq!(p.start_path(&params(json!({"path": "a b/c"})), &fs), "/a b/c");
        assert_eq!(p.start_path(&params(json!({"path": "/x/y z/"})), &fs), "/x/y z");
        assert_eq!(p.start_path(&params(json!({})), &fs), "/");
    }

    #[test]
    fn registration() {
        crate::register_builtin();
        crate::register_builtin(); // idempotent
        assert_eq!(oc_provider::get("SMB").map(|p| p.scheme()), Some("smb"));
        assert_eq!(oc_provider::scheme_of("SMB://nas/x").as_deref(), Some("smb"));
    }
}
