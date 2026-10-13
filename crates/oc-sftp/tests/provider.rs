//! The sftp:// connection type: addresses and accounts exactly as the Python
//! version (tests/test_connections.py, test_fsproviders.py), connecting
//! through a ConnectContext, start folders.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use oc_provider::{ConnectContext, FileSystemProvider, Params, Value, param_int, param_str};
use oc_sftp::SftpProvider;
use oc_sftp::testing::{PASSWORD, USER, start_sftp_server};
use oc_vfs::FileSystem;
use serde_json::json;

fn params(v: serde_json::Value) -> Params {
    v.as_object().unwrap().clone()
}

#[test]
fn url_parsing_round_trips() {
    let p = SftpProvider;
    let cases: [(&str, serde_json::Value); 3] = [
        ("sftp://alice@example.com", json!({"host": "example.com", "username": "alice", "path": ""})),
        ("sftp://example.com:2222/var/log", json!({"port": 2222, "path": "/var/log", "username": ""})),
        ("sftp://bob@host/~/projects/x", json!({"path": "projects/x"})),
    ];
    for (url, expected) in cases {
        let parsed = p.parse_url(url).unwrap();
        for (key, value) in expected.as_object().unwrap() {
            match value {
                Value::Number(n) => assert_eq!(param_int(&parsed, key), n.as_i64(), "{url}: {key}"),
                Value::String(s) => assert_eq!(&param_str(&parsed, key), s, "{url}: {key}"),
                _ => unreachable!(),
            }
        }
        let formatted = p.format_url(&parsed);
        assert_eq!(p.format_url(&p.parse_url(&formatted).unwrap()), formatted, "{url}");
    }
    assert_eq!(p.format_url(&p.parse_url("sftp://bob@host/~/projects/x").unwrap()), "sftp://bob@host/~/projects/x");
    assert_eq!(p.format_url(&p.parse_url("sftp://Host:22/a%20b/c").unwrap()), "sftp://host/a%20b/c");
    assert_eq!(p.format_url(&p.parse_url("sftp://h/~").unwrap()), "sftp://h");
    for bad in ["ftp://x", "sftp://", "/local/path"] {
        assert!(p.parse_url(bad).is_err(), "{bad}");
    }
}

#[test]
fn accounts_match_python() {
    let p = SftpProvider;
    assert_eq!(p.account(&params(json!({"host": "h", "username": "u", "port": 22}))), "sftp://u@h");
    assert_eq!(
        p.account(&params(json!({"host": "h", "username": "u", "port": 2222, "path": "/x"}))),
        "sftp://u@h:2222"
    );
    assert_eq!(p.account(&params(json!({"host": "h", "path": "rel"}))), "sftp://h");
    assert_eq!(
        p.account(&params(json!({"host": "h", "username": "me@corp", "port": "2200"}))),
        "sftp://me%40corp@h:2200"
    );
    // A 1.x connections.json entry: same key as before, so stored passwords still match.
    let old = params(json!({"host": "203.0.113.10", "username": "root", "port": null, "share": "",
                            "path": "", "key_file": "/Users/me/.ssh/id_ed25519"}));
    assert_eq!(p.account(&old), "sftp://root@203.0.113.10");
    assert_eq!(p.describe(&old), "root@203.0.113.10");
    assert_eq!(p.format_url(&params(json!({"host": "h", "path": "/a b"}))), "sftp://h/a%20b");
    assert_eq!(p.format_url(&params(json!({"host": "h", "path": "/"}))), "sftp://h/");
}

#[test]
fn fields_and_validation() {
    let p = SftpProvider;
    assert_eq!((p.scheme(), p.title()), ("sftp", "SFTP (SSH)"));
    let keys: Vec<_> = p.fields().iter().map(|f| f.key).collect();
    assert_eq!(keys, ["host", "port", "username", "key_file", "path"]);
    assert_eq!(p.secret_hint(), "empty: use SSH agent/keys, or ask");
    assert_eq!(p.validate(&params(json!({"host": "h", "port": "22a"}))).as_deref(), Some("Port must be a number."));
    assert_eq!(p.validate(&params(json!({}))).as_deref(), Some("Enter server."));
    assert_eq!(p.validate(&params(json!({"host": "h"}))), None);
}

#[test]
fn register_builtin_adds_sftp() {
    oc_sftp::register_builtin();
    oc_sftp::register_builtin(); // idempotent
    assert_eq!(oc_provider::get("SFTP").map(|p| p.scheme()), Some("sftp"));
}

struct Ctx {
    dir: PathBuf,
    asked: Mutex<Vec<(String, String, String)>>,
    answer: bool,
}

impl ConnectContext for Ctx {
    fn data_dir(&self) -> PathBuf {
        self.dir.clone()
    }

    fn confirm(&self, title: &str, message: &str, ok_label: &str) -> bool {
        self.asked.lock().unwrap().push((title.into(), message.into(), ok_label.into()));
        self.answer
    }
}

#[test]
fn connect_through_the_provider() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().canonicalize().unwrap();
    fs::create_dir_all(tmp.join("remote/home/tester/work")).unwrap();
    let server = start_sftp_server(&tmp.join("remote"));
    let config = tmp.join("config");
    let ctx = Ctx { dir: config.clone(), asked: Mutex::default(), answer: true };
    let p = SftpProvider;
    let conn = params(json!({"host": server.host, "username": USER, "port": server.port, "path": "/home/tester/work"}));

    let fs1: Arc<dyn FileSystem> = p.connect(&conn, Some(PASSWORD), &ctx).unwrap();
    let asked = ctx.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    let (title, message, ok) = &asked[0];
    assert_eq!((title.as_str(), ok.as_str()), ("Unknown server", "Trust and connect"));
    assert_eq!(
        message,
        &format!(
            "Orchard Commander hasn't connected to 127.0.0.1:{} before. Its identity can't be confirmed yet.\n\n\
             ssh-ed25519 key fingerprint:\n  {}\n\n\
             If you know the server's fingerprint, check that it matches. Trusting it remembers this key; \
             Orchard Commander will refuse to connect if it ever changes.",
            server.port, server.host_key_fingerprint
        )
    );
    assert!(fs::read_to_string(config.join("known_hosts")).unwrap().contains("ssh-ed25519"));

    assert_eq!(p.start_path(&conn, &fs1), "/home/tester/work");
    assert!(fs1.is_dir("/home/tester/work"));
    let mut rel = conn.clone();
    rel.insert("path".into(), json!("home/./tester/../tester"));
    assert_eq!(p.start_path(&rel, &fs1), "/home/tester");
    rel.insert("path".into(), json!(""));
    assert_eq!(p.start_path(&rel, &fs1), "/");
    assert!(p.is_connected(&fs1));
    assert_eq!(fs1.display("/x"), format!("sftp://{USER}@127.0.0.1:{}/x", server.port));

    // Known now: no question. An empty secret means "none" (agent/keys), here: needs a password.
    let ctx2 = Ctx { dir: config.clone(), asked: Mutex::default(), answer: true };
    let mut no_user = conn.clone();
    no_user.insert("key_file".into(), json!(""));
    match p.connect(&no_user, Some("wrong"), &ctx2) {
        Err(oc_provider::ConnectError::AuthenticationRequired(_)) => {}
        other => panic!("{:?}", other.err()),
    }
    assert!(ctx2.asked.lock().unwrap().is_empty());

    fs1.close();
    assert!(!p.is_connected(&fs1));
    // Not ours: assumed connected.
    let mem: Arc<dyn FileSystem> = Arc::new(oc_vfs::testing::MemFS::new());
    assert!(p.is_connected(&mem));
    assert_eq!(p.start_path(&params(json!({"path": "/x"})), &mem), "/x");

    // Declining the host key cancels.
    let ctx3 = Ctx { dir: tmp.join("other-config"), asked: Mutex::default(), answer: false };
    assert!(matches!(p.connect(&conn, Some(PASSWORD), &ctx3), Err(oc_provider::ConnectError::Cancelled)));
}
