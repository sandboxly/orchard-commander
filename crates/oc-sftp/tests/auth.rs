//! Authentication like paramiko's: key file (+ passphrase), agent, default
//! keys, password (+ keyboard-interactive fallback) — and with a password,
//! nothing else (MaxAuthTries).

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use oc_provider::ConnectError;
use oc_sftp::testing::{PASSWORD, ServerOptions, SftpServer, USER, random_key, start_sftp_server_with};
use oc_sftp::{Agent, ConnectOptions, connect};
use oc_vfs::FileSystem as _;
use russh::keys::ssh_key::LineEnding;

struct Fixture {
    _dir: tempfile::TempDir,
    tmp: PathBuf,
    server: SftpServer,
}

fn fixture(opts: ServerOptions) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().canonicalize().unwrap();
    fs::create_dir_all(tmp.join("remote")).unwrap();
    fs::create_dir_all(tmp.join("home")).unwrap();
    let server = start_sftp_server_with(&tmp.join("remote"), opts);
    Fixture { tmp, server, _dir: dir }
}

impl Fixture {
    fn options(&self, password: Option<&str>) -> ConnectOptions {
        let mut o = ConnectOptions::new(self.server.host.clone(), self.tmp.join("known_hosts"));
        o.port = self.server.port;
        o.username = Some(USER.into());
        o.password = password.map(str::to_string);
        o.agent = Agent::Off;
        o.home = Some(self.tmp.join("home"));
        o.timeout = Duration::from_secs(5);
        o
    }
}

fn auth_failed(r: Result<std::sync::Arc<oc_sftp::SftpFileSystem>, ConnectError>) -> String {
    match r {
        Err(ConnectError::AuthenticationRequired(msg)) => msg,
        Err(e) => panic!("expected AuthenticationRequired, got {e:?}"),
        Ok(_) => panic!("expected AuthenticationRequired, got a connection"),
    }
}

#[test]
fn key_file_with_passphrase() {
    let key = random_key();
    let f = fixture(ServerOptions {
        password: None,
        authorized_keys: vec![key.public_key().clone()],
        ..Default::default()
    });
    let encrypted = key.encrypt(&mut rand::rng(), "phrase").unwrap();
    let path = f.tmp.join("id_test");
    fs::write(&path, encrypted.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();

    let mut o = f.options(Some("phrase"));
    o.key_file = Some(path.clone());
    connect(&o, &|_| true).unwrap().close();

    // Without the passphrase: the password (= passphrase) is asked for.
    let mut o = f.options(None);
    o.key_file = Some(path.clone());
    assert_eq!(auth_failed(connect(&o, &|_| true)), format!("Authentication failed for {USER}@127.0.0.1"));

    // A missing key file says so.
    let mut o = f.options(None);
    o.key_file = Some(f.tmp.join("nope"));
    match connect(&o, &|_| true) {
        Err(ConnectError::Connection(msg)) => assert!(msg.starts_with("Cannot read the key file"), "{msg}"),
        other => panic!("{:?}", other.err()),
    }
}

#[test]
fn default_keys_in_home() {
    let key = random_key();
    let f = fixture(ServerOptions {
        password: None,
        authorized_keys: vec![key.public_key().clone()],
        ..Default::default()
    });
    let ssh = f.tmp.join("home").join(".ssh");
    fs::create_dir_all(&ssh).unwrap();
    fs::write(ssh.join("id_ed25519"), key.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
    connect(&f.options(None), &|_| true).unwrap().close();
}

#[test]
fn keyboard_interactive_fallback() {
    let f = fixture(ServerOptions { keyboard_interactive_only: true, ..Default::default() });
    connect(&f.options(Some(PASSWORD)), &|_| true).unwrap().close();
    assert_eq!(f.server.auth_log().keyboard_interactive_attempts, 1);
    assert_eq!(
        auth_failed(connect(&f.options(Some("wrong")), &|_| true)),
        format!("Authentication failed for {USER}@127.0.0.1")
    );
}

#[cfg(unix)]
#[test]
fn agent_keys_without_password() {
    let key = random_key();
    let f = fixture(ServerOptions {
        password: None,
        authorized_keys: vec![key.public_key().clone()],
        ..Default::default()
    });
    let agent = oc_sftp::testing::start_agent(&[random_key(), key]);
    let mut o = f.options(None);
    o.agent = Agent::Socket(agent.socket.clone());
    connect(&o, &|_| true).unwrap().close();
    assert!(f.server.auth_log().publickey_offers >= 1);

    // An agent without a matching key: authentication failed (not "needs a password").
    let other = oc_sftp::testing::start_agent(&[random_key()]);
    let mut o = f.options(None);
    o.agent = Agent::Socket(other.socket.clone());
    assert_eq!(auth_failed(connect(&o, &|_| true)), format!("Authentication failed for {USER}@127.0.0.1"));
}

#[cfg(unix)]
#[test]
fn with_a_password_agent_keys_are_not_offered() {
    // MaxAuthTries 2 and an agent with 5 keys: offering them first would lock us out.
    let f = fixture(ServerOptions { max_auth_attempts: 2, ..Default::default() });
    let keys: Vec<_> = (0..5).map(|_| random_key()).collect();
    let agent = oc_sftp::testing::start_agent(&keys);
    let mut o = f.options(Some(PASSWORD));
    o.agent = Agent::Socket(agent.socket.clone());
    connect(&o, &|_| true).unwrap().close();
    let log = f.server.auth_log();
    assert_eq!(log.publickey_offers, 0, "{log:?}");
    assert_eq!(log.password_attempts, 1);
}
