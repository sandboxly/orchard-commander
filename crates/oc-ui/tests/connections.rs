//! Servers in the UI, end to end against real servers: the in-process SSH
//! server of `oc_sftp::testing`, and WsgiDAV / moto from the Python
//! version's test venv (skipped when it isn't there). Ports of 1.x's
//! test_ui.py server tests (connect via Go to, host key + password
//! prompts, remembered passwords, the location chooser, bookmarks on
//! servers, disconnect) plus the connection manager and reconnecting.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_config::{CredentialStore, MemoryCredentials};
use oc_sftp::testing::{PASSWORD, ServerOptions, SftpServer, USER, start_sftp_server_with};
use oc_ui::connections::{self, Connections, api};
use oc_ui::testing::{headless_app, press, press_with, settle, type_text, wait_until};
use oc_ui::{App, Key, MAIN_WINDOW, Mods};
use serde_json::json;

const SERVICE: &str = "Orchard Commander";

struct Env {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    left: PathBuf,
    right: PathBuf,
    server_root: PathBuf,
}

fn env() -> Env {
    oc_sftp::register_builtin();
    oc_smb::register_builtin();
    oc_remote_http::register_builtin();
    let tmp = tempfile::tempdir().unwrap();
    let base = oc_local::absolute(&fs::canonicalize(tmp.path()).unwrap());
    let (config, left, right, server_root) = (base.join("config"), base.join("left"), base.join("right"), base.join("server"));
    for d in [&config, &left, &right, &server_root.join("pub")] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(server_root.join("pub/hello.txt"), "remote hello").unwrap();
    fs::write(server_root.join("pub/old.txt"), "delete me").unwrap();
    Env { _tmp: tmp, config, left, right, server_root }
}

fn sftp_server(e: &Env) -> SftpServer {
    // Agent keys and ~/.ssh/id_* of whoever runs the tests are offered
    // before the password prompt: allow plenty of attempts.
    start_sftp_server_with(&e.server_root, ServerOptions { max_auth_attempts: 100, ..Default::default() })
}

fn app(e: &Env) -> (App, Arc<MemoryCredentials>) {
    let mut app = headless_app(&e.config, &e.left, &e.right);
    let store = Arc::new(MemoryCredentials::new());
    connections::use_credential_store(&mut app, store.clone() as Arc<dyn CredentialStore>);
    app.resize(MAIN_WINDOW, 120, 40);
    settle(&mut app);
    (app, store)
}

fn top(app: &App) -> Option<String> {
    app.modal_titles().last().cloned()
}

fn wait_dialog(app: &mut App, title: &str) {
    let t = title.to_owned();
    wait_until(app, &format!("dialog {title}"), Duration::from_secs(20), move |app| top(app).is_some_and(|x| x == t));
}

fn wait_no_dialogs(app: &mut App) {
    wait_until(app, "dialogs to close", Duration::from_secs(20), |app| app.modal_titles().is_empty());
}

/// Wait until panel `side` shows a server folder (listing done).
fn wait_remote(app: &mut App, side: usize) {
    wait_until(app, "a server listing", Duration::from_secs(20), move |app| {
        let p = &app.main.sides[side].panel;
        !p.location.is_local() && !p.loading() && app.modal_titles().is_empty()
    });
    settle(app);
}

fn names(app: &App, side: usize) -> Vec<String> {
    app.main.sides[side].panel.entries.iter().map(|e| e.name.clone()).collect()
}

fn primary_shift() -> Mods {
    Mods::primary().union(Mods::SHIFT)
}

/// ⌘G + an address + Enter.
fn go_to(app: &mut App, address: &str) {
    press_with(app, Key::Char('g'), Mods::primary());
    wait_dialog(app, "Go to");
    type_text(app, address);
    press(app, Key::Enter);
}

/// Answer a provider's question (Cancel is the default) with its OK button.
fn trust(app: &mut App) {
    press(app, Key::Left);
    press(app, Key::Enter);
}

fn sftp_url(server: &SftpServer, path: &str) -> String {
    format!("sftp://{USER}@{}:{}{path}", server.host, server.port)
}

fn sftp_account(server: &SftpServer) -> String {
    format!("sftp://{USER}@{}:{}", server.host, server.port)
}

fn save_connections(e: &Env, items: serde_json::Value) {
    fs::write(e.config.join("connections.json"), serde_json::to_string_pretty(&items).unwrap()).unwrap();
}

fn saved_json(e: &Env) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(e.config.join("connections.json")).unwrap()).unwrap()
}

/// Wait until connections.json says `check`.
fn wait_saved(app: &mut App, e: &Env, what: &str, check: impl Fn(&serde_json::Value) -> bool) {
    let path = e.config.join("connections.json");
    wait_until(app, what, Duration::from_secs(10), move |_| {
        fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).is_some_and(|v| check(&v))
    });
    settle(app);
}

// ---------------------------------------------------------------------------
// SFTP
// ---------------------------------------------------------------------------

#[test]
fn goto_sftp_address_asks_host_key_and_password_then_browses_and_disconnects() {
    let e = env();
    let server = sftp_server(&e);
    let (mut app, store) = app(&e);
    assert!(app.registry.get("nav.goto").unwrap().help.contains("server address"));
    go_to(&mut app, &sftp_url(&server, "/pub"));
    wait_dialog(&mut app, "Unknown server");
    let question = app.top_modal_text().unwrap();
    assert!(question.contains(&server.host_key_fingerprint), "{question}");
    trust(&mut app);
    wait_dialog(&mut app, "Password");
    assert_eq!(app.top_modal_text().unwrap(), format!("Password for {USER}@{}:", server.host));
    type_text(&mut app, PASSWORD);
    press(&mut app, Key::Tab);
    press(&mut app, Key::Space); // Remember in the system keychain
    press(&mut app, Key::Enter);
    wait_remote(&mut app, 0);
    assert_eq!(names(&app, 0), ["hello.txt", "old.txt"]);
    let title = app.main.sides[0].panel.display_path();
    assert!(title.ends_with(&format!(":{}/pub", server.port)), "panel title {title}");
    assert_eq!(store.entries(), [((SERVICE.into(), sftp_account(&server)), PASSWORD.into())], "remembered under the account");
    assert!(fs::read_to_string(e.config.join("known_hosts")).unwrap().contains(&format!("[{}]:{}", server.host, server.port)));
    // A server folder can be described for bookmarks.
    let location = app.main.sides[0].panel.location.clone();
    assert_eq!(api::account_of(&app, &*location.fs).as_deref(), Some(sftp_account(&server).as_str()));
    let spec = oc_config::locations::spec_of(&location, &api::spec_context(&app)).unwrap();
    assert_eq!(spec, oc_config::Spec::server(sftp_account(&server), "/pub"));
    assert!(api::is_on_server(&app, &location));
    assert!(app.toast.is_none(), "the Connecting… toast is gone");

    // ⇧⌘K: back to the local disk.
    press_with(&mut app, Key::Char('k'), primary_shift());
    settle(&mut app);
    assert!(app.main.sides[0].panel.location.is_local());
    assert_eq!(app.main.sides[0].panel.location.path, oc_local::home_dir().to_string_lossy());
    assert!(app.state::<Connections>().sessions.list().is_empty());
    assert_eq!(app.toast.as_ref().map(|t| t.text.as_str()), Some("Disconnected"));

    // Again: the host key is known and the password comes from the keychain.
    go_to(&mut app, &sftp_url(&server, "/pub"));
    wait_remote(&mut app, 0);
    assert_eq!(names(&app, 0), ["hello.txt", "old.txt"]);
}

#[test]
fn declining_the_host_key_connects_nothing() {
    let e = env();
    let server = sftp_server(&e);
    let (mut app, _) = app(&e);
    go_to(&mut app, &sftp_url(&server, "/pub"));
    wait_dialog(&mut app, "Unknown server");
    press(&mut app, Key::Enter); // Cancel is the default
    wait_until(&mut app, "the warning", Duration::from_secs(10), |app| app.toast.as_ref().is_some_and(|t| t.text.contains("was not trusted")));
    settle(&mut app);
    assert!(app.main.sides[0].panel.location.is_local());
    let trusted = fs::read_to_string(e.config.join("known_hosts")).unwrap_or_default();
    assert!(!trusted.contains(&format!("[{}]:{}", server.host, server.port)), "not remembered");
}

#[test]
fn bad_addresses_and_disconnect_without_server() {
    let e = env();
    let (mut app, _) = app(&e);
    go_to(&mut app, "sftp://");
    settle(&mut app);
    let toast = app.toast.clone().unwrap();
    assert!(toast.text.contains("No server in address"), "{}", toast.text);
    press_with(&mut app, Key::Char('k'), primary_shift());
    assert_eq!(app.toast.as_ref().map(|t| t.text.as_str()), Some("The active panel isn't on a server"));
    assert!(connections::is_url("SFTP://host") && !connections::is_url("~/x"));
}

#[test]
fn saved_connections_in_the_location_chooser() {
    let e = env();
    let server = sftp_server(&e);
    save_connections(
        &e,
        json!([{"kind": "sftp", "params": {"host": server.host, "username": USER, "port": server.port, "path": "/pub"},
                "name": "test box", "id": "abc"}]),
    );
    let (mut app, store) = app(&e);
    store.set(SERVICE, &sftp_account(&server), PASSWORD).unwrap();
    press_with(&mut app, Key::F(2), Mods::ALT);
    wait_dialog(&mut app, "Go to");
    let offered = app.top_modal_text().unwrap();
    assert!(offered.lines().any(|l| l == "⇄ test box"), "{offered}");
    type_text(&mut app, "test box");
    press(&mut app, Key::Enter);
    wait_dialog(&mut app, "Unknown server");
    trust(&mut app);
    wait_remote(&mut app, 1);
    assert_eq!(app.main.active, 1);
    assert_eq!(app.main.sides[1].panel.location.path, "/pub");
    assert_eq!(names(&app, 1), ["hello.txt", "old.txt"]);
}

#[test]
fn resolve_server_reconnects_a_bookmarked_folder() {
    let e = env();
    let server = sftp_server(&e);
    save_connections(
        &e,
        json!([{"kind": "sftp", "params": {"host": server.host, "username": USER, "port": server.port}, "name": "", "id": "b1"}]),
    );
    let (mut app, store) = app(&e);
    store.set(SERVICE, &sftp_account(&server), PASSWORD).unwrap();
    #[derive(Default)]
    struct Arrived(Option<Option<oc_vfs::VPath>>);
    api::resolve_server(&mut app, &sftp_account(&server), "/pub", |app, location| app.state::<Arrived>().0 = Some(location));
    wait_dialog(&mut app, "Unknown server");
    trust(&mut app);
    wait_until(&mut app, "the location", Duration::from_secs(20), |app| app.try_state::<Arrived>().is_some_and(|a| a.0.is_some()));
    let location = app.state::<Arrived>().0.take().unwrap().expect("connected");
    assert_eq!(location.path, "/pub");
    assert!(app.main.sides[0].panel.location.is_local(), "resolving doesn't touch the panels");
    let spec = oc_config::locations::spec_of(&location, &api::spec_context(&app)).unwrap();
    assert_eq!(spec, oc_config::Spec::server(sftp_account(&server), "/pub"), "and it round-trips");
    // A second resolve reuses the session (no questions, same file system).
    api::resolve_server(&mut app, &sftp_account(&server), "/", |app, location| app.state::<Arrived>().0 = Some(location));
    wait_until(&mut app, "the second location", Duration::from_secs(20), |app| app.try_state::<Arrived>().is_some_and(|a| a.0.is_some()));
    let again = app.state::<Arrived>().0.take().unwrap().unwrap();
    assert!(again.same_fs(&location));
    // An account nobody knows that isn't an address: told, and None.
    api::resolve_server(&mut app, "gopher://x", "/", |app, location| app.state::<Arrived>().0 = Some(location));
    settle(&mut app);
    assert!(app.state::<Arrived>().0.take().is_some_and(|l| l.is_none()));
    assert!(app.toast.as_ref().is_some_and(|t| t.text.starts_with("Unsupported address")));
}

#[test]
fn a_lost_session_reconnects_when_listing() {
    let e = env();
    let server = sftp_server(&e);
    let (mut app, store) = app(&e);
    store.set(SERVICE, &sftp_account(&server), PASSWORD).unwrap();
    go_to(&mut app, &sftp_url(&server, "/pub"));
    wait_dialog(&mut app, "Unknown server");
    trust(&mut app);
    wait_remote(&mut app, 0);
    let old = app.main.sides[0].panel.location.fs.clone();
    old.close(); // the connection drops
    fs::write(e.server_root.join("pub/new.txt"), "new").unwrap();
    press_with(&mut app, Key::Char('r'), Mods::primary());
    wait_until(&mut app, "the reconnected listing", Duration::from_secs(20), |app| {
        let p = &app.main.sides[0].panel;
        !p.loading() && p.entries.iter().any(|e| e.name == "new.txt")
    });
    let p = &app.main.sides[0].panel;
    assert_eq!(p.location.path, "/pub");
    assert!(p.location.fs.identity() != old.identity(), "a new session");
    assert_eq!(app.state::<Connections>().sessions.list().len(), 1);
}

#[test]
fn connection_manager_new_edit_duplicate_delete() {
    let e = env();
    let server = sftp_server(&e);
    let (mut app, store) = app(&e);
    press_with(&mut app, Key::Char('k'), Mods::primary());
    wait_dialog(&mut app, "Connect to server");
    assert!(app.top_modal_text().unwrap().contains("sftp://"));
    // Nothing saved: the focus is on New….
    press(&mut app, Key::Enter);
    wait_dialog(&mut app, "New connection");
    // An empty server is refused.
    press_with(&mut app, Key::Char('s'), Mods::NONE); // typed into Server…
    press(&mut app, Key::Backspace); // …and removed
    press(&mut app, Key::Enter);
    assert_eq!(top(&app).as_deref(), Some("New connection"));
    assert!(app.top_modal_text().unwrap().ends_with("Enter server."));
    // Fill in the form: Server, Port, User, Key file, Start folder, Password.
    type_text(&mut app, &server.host);
    press(&mut app, Key::Down);
    type_text(&mut app, &server.port.to_string());
    press(&mut app, Key::Down);
    type_text(&mut app, USER);
    press(&mut app, Key::Down);
    press(&mut app, Key::Down);
    type_text(&mut app, "/pub");
    press(&mut app, Key::Down);
    type_text(&mut app, PASSWORD);
    press(&mut app, Key::Down);
    press(&mut app, Key::Space); // remember
    // And a name.
    for _ in 0..7 {
        press(&mut app, Key::Up);
    }
    type_text(&mut app, "test");
    press(&mut app, Key::Enter);
    // Saved, and connecting right away.
    wait_dialog(&mut app, "Unknown server");
    trust(&mut app);
    wait_remote(&mut app, 0);
    assert_eq!(names(&app, 0), ["hello.txt", "old.txt"]);
    let saved = saved_json(&e);
    assert_eq!(saved[0]["kind"], "sftp");
    assert_eq!(saved[0]["name"], "test");
    assert_eq!(saved[0]["params"], json!({"host": server.host, "port": server.port, "username": USER, "path": "/pub"}));
    assert_eq!(saved[0]["id"].as_str().unwrap().len(), 32);
    assert_eq!(store.entries(), [((SERVICE.into(), sftp_account(&server)), PASSWORD.into())]);

    // Edit: the form shows what is saved, Remember is ticked (a password is stored).
    press_with(&mut app, Key::Char('k'), Mods::primary());
    wait_dialog(&mut app, "Connect to server");
    press(&mut app, Key::F(4));
    wait_dialog(&mut app, "Edit connection");
    let form = app.top_modal_text().unwrap();
    assert!(form.contains("Type: SFTP (SSH)") && form.contains(&format!("Port: {}", server.port)), "{form}");
    assert!(form.contains("[x] Remember in the system keychain"), "{form}");
    press(&mut app, Key::Up); // Name
    type_text(&mut app, "2");
    press(&mut app, Key::Enter);
    wait_saved(&mut app, &e, "the renamed connection", |v| v[0]["name"] == "test2");
    assert_eq!(top(&app).as_deref(), Some("Connect to server"), "back in the manager");
    assert_eq!(store.entries().len(), 1, "the stored password is kept");

    // Duplicate (Tab to the buttons: Connect, New…, Edit…, Duplicate).
    press(&mut app, Key::Tab);
    for _ in 0..3 {
        press(&mut app, Key::Right);
    }
    press(&mut app, Key::Enter);
    wait_saved(&mut app, &e, "the copy", |v| v.as_array().is_some_and(|a| a.len() == 2));
    let saved = saved_json(&e);
    assert_eq!(saved[1]["name"], "test2 (copy)");
    assert_eq!(saved[1]["params"], saved[0]["params"]);
    assert_ne!(saved[1]["id"], saved[0]["id"]);

    // Delete the selected one (the copy), after confirming.
    press(&mut app, Key::Tab);
    press(&mut app, Key::Delete);
    wait_dialog(&mut app, "Delete connection");
    assert_eq!(app.top_modal_text().unwrap(), "Delete 'test2 (copy)'?");
    press(&mut app, Key::Enter);
    wait_saved(&mut app, &e, "the deletion", |v| v.as_array().is_some_and(|a| a.len() == 1));
    assert_eq!(saved_json(&e)[0]["name"], "test2");
    assert!(store.entries().is_empty(), "deleting forgets the password (same account)");
    press(&mut app, Key::Escape);
    wait_no_dialogs(&mut app);
}

#[test]
fn editor_switches_types_and_keeps_typed_values() {
    let e = env();
    let (mut app, _) = app(&e);
    press_with(&mut app, Key::Char('k'), Mods::primary());
    wait_dialog(&mut app, "Connect to server");
    press(&mut app, Key::Enter);
    wait_dialog(&mut app, "New connection");
    type_text(&mut app, "nas.local");
    press(&mut app, Key::Up);
    press(&mut app, Key::Up); // Type
    press(&mut app, Key::Right); // SMB
    let form = app.top_modal_text().unwrap();
    assert!(form.starts_with("Type: SMB"), "{form}");
    assert!(form.contains("Server: nas.local") && form.contains("Share: "), "{form}");
    // The list (Enter) picks S3: the secret is a "Secret key" there.
    press(&mut app, Key::Enter);
    for _ in 0..3 {
        press(&mut app, Key::Down);
    }
    press(&mut app, Key::Enter);
    let form = app.top_modal_text().unwrap();
    assert!(form.starts_with("Type: S3 (Amazon S3 and compatible)") && form.contains("Secret key: "), "{form}");
    assert!(form.contains("Server: nas.local"));
    press(&mut app, Key::Escape);
    assert_eq!(top(&app).as_deref(), Some("Connect to server"));
    press(&mut app, Key::Escape);
    wait_no_dialogs(&mut app);
    assert!(!e.config.join("connections.json").exists(), "nothing saved");
}

// ---------------------------------------------------------------------------
// WebDAV and S3 (the Python version's WsgiDAV and moto)
// ---------------------------------------------------------------------------

/// The Python test venv: $OC_TEST_PYTHON, or `fcmd/.venv` next to an ancestor.
fn python() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("OC_TEST_PYTHON") {
        return Some(PathBuf::from(p)).filter(|p| p.exists());
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().map(|a| a.join("fcmd/.venv/bin/python")).find(|p| p.exists())
}

/// A server from oc-remote-http's `tests/support/servers.py`; stopped when dropped.
struct PyServer {
    child: Child,
    port: u16,
}

impl PyServer {
    fn start(args: &[&str]) -> Option<PyServer> {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../oc-remote-http/tests/support/servers.py");
        let mut child = Command::new(python()?)
            .arg("-I")
            .arg(script)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .ok()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take()?).read_line(&mut line).ok()?;
        match serde_json::from_str::<serde_json::Value>(&line).ok().and_then(|v| v["port"].as_u64()) {
            Some(port) => Some(PyServer { child, port: port as u16 }),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                None
            }
        }
    }
}

impl Drop for PyServer {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

macro_rules! require {
    ($e:expr) => {
        match $e {
            Some(v) => v,
            None => {
                eprintln!("skipped: {} unavailable", stringify!($e));
                return;
            }
        }
    };
}

#[test]
fn webdav_asks_again_after_a_wrong_password_and_remembers() {
    let e = env();
    let dav = require!(PyServer::start(&["webdav", e.server_root.to_str().unwrap()]));
    let (mut app, store) = app(&e);
    let url = format!("webdav://{USER}@127.0.0.1:{}/dav", dav.port);
    // A typed WebDAV address: its whole path is the server path (as in 1.x).
    go_to(&mut app, &url);
    wait_dialog(&mut app, "Password");
    assert_eq!(app.top_modal_text().unwrap(), format!("Password for {USER}@127.0.0.1:"));
    type_text(&mut app, "wrong");
    press(&mut app, Key::Enter);
    wait_until(&mut app, "the second prompt", Duration::from_secs(20), |app| {
        top(app).as_deref() == Some("Password") && app.top_modal_text().is_some_and(|t| t.starts_with("The server didn't accept the password."))
    });
    type_text(&mut app, PASSWORD);
    press(&mut app, Key::Tab);
    press(&mut app, Key::Space);
    press(&mut app, Key::Enter);
    wait_remote(&mut app, 0);
    assert_eq!(names(&app, 0), ["pub"]);
    assert_eq!(store.entries(), [((SERVICE.into(), url.clone()), PASSWORD.into())], "keychain user = the account");
    assert!(app.main.sides[0].panel.display_path().starts_with("webdav://"));
}

#[test]
fn webdavs_with_a_self_signed_certificate_asks_to_trust_it() {
    let e = env();
    let certs = e.config.parent().unwrap().join("certs");
    fs::create_dir_all(&certs).unwrap();
    let dav = require!(PyServer::start(&["webdav", e.server_root.to_str().unwrap(), "--tls", certs.to_str().unwrap()]));
    let (mut app, store) = app(&e);
    let account = format!("webdavs://{USER}@127.0.0.1:{}/dav", dav.port);
    store.set(SERVICE, &account, PASSWORD).unwrap();
    go_to(&mut app, &account);
    wait_dialog(&mut app, "Untrusted certificate");
    assert!(app.top_modal_text().unwrap().contains("Certificate fingerprint:"));
    trust(&mut app);
    wait_remote(&mut app, 0);
    assert_eq!(names(&app, 0), ["pub"]);
    assert!(e.config.join("trusted_certificates.json").exists(), "trusted in the config dir");
}

#[test]
fn s3_asks_for_the_secret_key() {
    let e = env();
    let s3 = require!(PyServer::start(&["s3"]));
    let (mut app, store) = app(&e);
    let account = format!("s3+http://testing@127.0.0.1:{}", s3.port);
    go_to(&mut app, &account);
    wait_dialog(&mut app, "Secret key");
    assert_eq!(app.top_modal_text().unwrap(), "Secret key for testing@127.0.0.1:");
    type_text(&mut app, "testing");
    press(&mut app, Key::Tab);
    press(&mut app, Key::Space);
    press(&mut app, Key::Enter);
    wait_remote(&mut app, 0);
    assert_eq!(store.entries(), [((SERVICE.into(), account), "testing".into())]);
}

// ---------------------------------------------------------------------------
// Pictures for reviewers:
// OC_SCREENSHOT_DIR=/some/dir cargo test -p oc-ui --test connections screenshots
// ---------------------------------------------------------------------------

fn snap(app: &mut App, dir: &Path, name: &str) {
    let mut r = oc_grid::HeadlessRenderer::new(app.theme.font.clone(), 1.0).expect("renderer");
    let m = r.renderer().metrics();
    app.cell_px = (m.width as f32, m.height as f32);
    let (cols, rows) = app.size_of(MAIN_WINDOW);
    let mut frame = oc_grid::Frame::new(cols, rows, app.theme.colors.background);
    app.draw(MAIN_WINDOW, &mut frame, app.cell_px);
    r.render(&frame).expect("render").save_png(dir.join(name)).expect("png");
}

#[test]
fn screenshots() {
    let Some(dir) = std::env::var_os("OC_SCREENSHOT_DIR").map(PathBuf::from) else { return };
    let e = env();
    let server = sftp_server(&e);
    let (mut app, _) = app(&e);
    go_to(&mut app, &sftp_url(&server, "/pub"));
    wait_dialog(&mut app, "Unknown server");
    snap(&mut app, &dir, "conn-host-key.png");
    trust(&mut app);
    wait_dialog(&mut app, "Password");
    type_text(&mut app, "s3c");
    snap(&mut app, &dir, "conn-password.png");
    type_text(&mut app, "ret");
    press(&mut app, Key::Enter);
    wait_remote(&mut app, 0);
    snap(&mut app, &dir, "conn-panel.png");
    press_with(&mut app, Key::F(2), Mods::ALT);
    wait_dialog(&mut app, "Go to");
    snap(&mut app, &dir, "conn-locations.png");
}
