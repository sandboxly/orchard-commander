use std::io;

use super::*;

fn register_all() {
    oc_remote_http::register_builtin();
    // SFTP is registered by the binary; a stand-in for address tests isn't
    // needed: the HTTP types cover parsing, accounts and labels.
}

#[test]
fn addresses_accounts_labels() {
    register_all();
    assert!(is_url("webdavs://me@cloud.example.com/remote.php/dav"));
    assert!(is_url("S3://bucket/folder"));
    assert!(!is_url("/Users/me"));
    assert!(!is_url("gopher://x"));
    let conn = from_url("webdavs://me@cloud.example.com/remote.php/dav/files/me").unwrap();
    assert_eq!(conn.kind, "webdavs");
    assert_eq!(account(&conn), "webdavs://me@cloud.example.com/remote.php/dav/files/me");
    assert_eq!(label(&conn), url(&conn), "no name: the address");
    assert_eq!(description(&conn), "me@cloud.example.com");
    let named = Connection { name: "Cloud".into(), ..conn.clone() };
    assert_eq!(label(&named), "Cloud");
    let err = from_url("gopher://x").unwrap_err();
    assert!(err.starts_with("Unsupported address (use ") && err.contains("webdavs://"), "{err}");
    // An add-on type that isn't installed: its own account, never shared.
    let addon = Connection::new("ftp", Default::default(), "");
    assert_eq!(account(&addon), format!("ftp://{}", addon.id));
    assert!(provider_of(&addon).err().is_some_and(|e| e.contains("No support for ftp:// connections")));
    let moved = with_path(&conn, "/sub");
    assert_eq!((moved.id.as_str(), moved.get_str("path")), (conn.id.as_str(), "/sub"));
}

#[test]
fn declined_questions_are_worded_like_1x() {
    let sftp = Connection::new("sftp", serde_json::json!({"host": "example.com"}).as_object().unwrap().clone(), "");
    assert_eq!(not_trusted(&sftp), "Host key for example.com was not trusted");
    let dav = Connection { kind: "webdavs".into(), ..sftp };
    assert_eq!(not_trusted(&dav), "The certificate of example.com was not trusted");
}

#[test]
fn broken_sessions_are_told_apart() {
    let remote = |e: oc_remote_http::RemoteError| VfsError::Io(io::Error::new(io::ErrorKind::PermissionDenied, e));
    assert!(session_broken(&remote(oc_remote_http::RemoteError::AuthenticationRequired("no".into()))));
    assert!(session_broken(&remote(oc_remote_http::RemoteError::Connection("gone".into()))));
    assert!(!session_broken(&remote(oc_remote_http::RemoteError::CertificateChanged("attack?".into()))));
    assert!(session_broken(&VfsError::Io(io::Error::new(io::ErrorKind::NotConnected, "Connection to x was lost"))));
    assert!(!session_broken(&VfsError::Io(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))));
    assert!(!session_broken(&VfsError::NotFound("x".into())));
}

#[test]
fn secret_fields_show_dots() {
    let mut input = crate::widgets::TextInput::new("päss");
    input.anchor = Some(1);
    let shown = dialogs::masked(&input);
    assert_eq!(shown.text, "••••");
    assert_eq!(shown.cursor, 4 * '•'.len_utf8());
    assert_eq!(shown.anchor, Some('•'.len_utf8()));
}
