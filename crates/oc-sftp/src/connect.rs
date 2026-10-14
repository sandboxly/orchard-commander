//! Connecting: TCP, SSH handshake with host-key verification, authentication,
//! and the first SFTP channel. Port of `remote/sftp.py::connect` (paramiko).

use std::borrow::Cow;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use oc_provider::ConnectError;
use russh::client::{self, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{Algorithm, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::{MethodKind, cipher, kex, mac};

use crate::fs::SftpFileSystem;
use crate::known_hosts;
use crate::runtime::block_on;
use crate::session::{Conn, open_sftp};

pub const DEFAULT_PORT: u16 = 22;
pub(crate) const APP_NAME: &str = "Orchard Commander";

/// What the user is asked about an unknown server ("trust this host key?").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyQuestion {
    pub host: String,
    pub port: u16,
    /// e.g. "ssh-ed25519"
    pub key_type: String,
    /// "SHA256:…" as shown by OpenSSH
    pub fingerprint: String,
}

/// Where SSH agent keys come from (only used without a password, see
/// [`ConnectOptions::password`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Agent {
    /// `SSH_AUTH_SOCK` (Unix), the OpenSSH agent pipe or Pageant (Windows).
    #[default]
    Auto,
    Off,
    /// A specific agent socket (Unix).
    Socket(PathBuf),
}

/// Everything [`connect`] needs. Start from [`ConnectOptions::new`].
#[derive(Clone, Debug)]
pub struct ConnectOptions {
    pub host: String,
    pub port: u16,
    /// None: the local user name (as OpenSSH and paramiko do).
    pub username: Option<String>,
    /// With a password, only the password is tried (plus the key file if one
    /// is given, with the password as its passphrase): offering every agent
    /// key first can exhaust the server's MaxAuthTries before the password is tried.
    pub password: Option<String>,
    /// A private key file (OpenSSH or PEM); the password is its passphrase.
    pub key_file: Option<PathBuf>,
    /// Our trusted host keys (`<config dir>/known_hosts`); new ones are added here.
    pub known_hosts: PathBuf,
    pub agent: Agent,
    /// The home folder whose `.ssh/` holds default keys (`id_rsa`, `id_ecdsa`,
    /// `id_ed25519`) and the system `known_hosts`. None: the user's home.
    pub home: Option<PathBuf>,
    /// For connecting, the handshake and each authentication step.
    pub timeout: Duration,
}

impl ConnectOptions {
    pub fn new(host: impl Into<String>, known_hosts: impl Into<PathBuf>) -> Self {
        ConnectOptions {
            host: host.into(),
            port: DEFAULT_PORT,
            username: None,
            password: None,
            key_file: None,
            known_hosts: known_hosts.into(),
            agent: Agent::Auto,
            home: None,
            timeout: Duration::from_secs(15),
        }
    }
}

/// The user's home folder, as Python's `os.path.expanduser("~")` finds it.
pub(crate) fn user_home() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// `os.path.expanduser` for "~" and "~/…".
pub fn expand_user(path: &str) -> PathBuf {
    let rest = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"));
    match (path == "~", rest, user_home()) {
        (true, _, Some(home)) => home,
        (_, Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

/// The local login name (Python's `getpass.getuser()`).
fn local_user() -> String {
    ["LOGNAME", "USER", "LNAME", "USERNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "root".into())
}

// ---------------------------------------------------------------------------
// Error messages
// ---------------------------------------------------------------------------

/// "Can't connect to host:port: …", with a hint for macOS's Local Network
/// privacy, which silently blocks programs without permission from reaching
/// devices on the LAN ("No route to host"). Port of `describe_connect_error`.
pub fn describe_connect_error(host: &str, port: u16, error: &dyn fmt::Display, unreachable: bool) -> String {
    oc_provider::connect_error_message(host, port, &error.to_string(), unreachable && is_local(host))
}

/// Why a socket operation failed, in words ("the connection was refused — …").
pub(crate) fn network_error_text(err: &io::Error) -> String {
    oc_provider::describe_network_error(err.kind(), &oc_vfs::os_text(err))
}

fn is_unreachable(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::HostUnreachable || err.to_string().contains("No route to host")
}

/// Whether `host` is on the local network (private, link-local or loopback address).
fn is_local(host: &str) -> bool {
    use std::net::{IpAddr, ToSocketAddrs};
    let Ok(addrs) = (host, 0).to_socket_addrs() else {
        return host.ends_with(".local");
    };
    addrs.into_iter().any(|a| match a.ip() {
        IpAddr::V4(ip) => ip.is_private() || ip.is_link_local() || ip.is_loopback(),
        IpAddr::V6(ip) => {
            ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00 || (ip.segments()[0] & 0xffc0) == 0xfe80
        }
    })
}

// ---------------------------------------------------------------------------
// Host keys
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
enum HostCheck {
    #[default]
    NotSeen,
    Known,
    Unknown(PublicKey),
    Changed(PublicKey),
}

pub(crate) struct ClientHandler {
    /// Keys on record; the key the user trusts is added, so a re-key with
    /// another key later in the session is refused.
    known: Arc<Mutex<Vec<PublicKey>>>,
    check: Arc<Mutex<HostCheck>>,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, server_key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        // We advertise no certificate algorithms, so servers present plain keys.
        let PublicKeyOrCertificate::PublicKey { key, .. } = server_key else {
            return Ok(false);
        };
        let known = self.known.lock().unwrap_or_else(|e| e.into_inner());
        let (verdict, ok) = if known.is_empty() {
            // Asked about after the handshake, before any credentials are sent.
            (HostCheck::Unknown(key.clone()), true)
        } else if known.iter().any(|k| k.key_data() == key.key_data()) {
            (HostCheck::Known, true)
        } else {
            (HostCheck::Changed(key.clone()), false)
        };
        *self.check.lock().unwrap_or_else(|e| e.into_inner()) = verdict;
        Ok(ok)
    }
}

/// Algorithms: russh's safe defaults first, then what paramiko also accepts
/// (older NAS boxes and routers), and the known host key types first so the
/// server proves the key we have on record (as paramiko does).
fn client_config(known: &[PublicKey]) -> client::Config {
    let mut preferred = russh::Preferred::default();
    let mut keys: Vec<Algorithm> = Vec::new();
    for k in known {
        let alg = k.algorithm();
        let algs = if alg.clone().is_rsa() {
            vec![
                Algorithm::Rsa { hash: Some(HashAlg::Sha512) },
                Algorithm::Rsa { hash: Some(HashAlg::Sha256) },
                Algorithm::Rsa { hash: None },
            ]
        } else {
            vec![alg]
        };
        for a in algs {
            if !keys.contains(&a) {
                keys.push(a);
            }
        }
    }
    for a in preferred.key.iter() {
        if !keys.contains(a) {
            keys.push(a.clone());
        }
    }
    preferred.key = Cow::Owned(keys);

    let mut kexes: Vec<kex::Name> = preferred
        .kex
        .iter()
        .copied()
        .filter(|k| !k.as_ref().starts_with("ext-info") && !k.as_ref().starts_with("kex-strict"))
        .collect();
    for extra in [
        kex::ECDH_SHA2_NISTP256,
        kex::ECDH_SHA2_NISTP384,
        kex::ECDH_SHA2_NISTP521,
        kex::DH_GEX_SHA1,
        kex::DH_G14_SHA1,
        kex::DH_G1_SHA1,
    ] {
        if !kexes.contains(&extra) {
            kexes.push(extra);
        }
    }
    kexes.extend(
        preferred
            .kex
            .iter()
            .copied()
            .filter(|k| k.as_ref().starts_with("ext-info") || k.as_ref().starts_with("kex-strict")),
    );
    preferred.kex = Cow::Owned(kexes);

    // AES-GCM first: hardware-accelerated (AES-NI / ARMv8 crypto) it is much
    // faster than ChaCha20-Poly1305, and the cipher is what limits fast links.
    let mut ciphers: Vec<cipher::Name> = vec![cipher::AES_128_GCM, cipher::AES_256_GCM];
    for extra in preferred.cipher.iter().copied().chain([cipher::AES_256_CBC, cipher::AES_192_CBC, cipher::AES_128_CBC])
    {
        if !ciphers.contains(&extra) {
            ciphers.push(extra);
        }
    }
    preferred.cipher = Cow::Owned(ciphers);

    let mut macs: Vec<mac::Name> = preferred.mac.to_vec();
    for extra in [mac::HMAC_SHA1_ETM, mac::HMAC_SHA1] {
        if !macs.contains(&extra) {
            macs.push(extra);
        }
    }
    preferred.mac = Cow::Owned(macs);

    client::Config {
        preferred,
        // Notice dead connections (sleep, network change) so they get reconnected.
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 3,
        inactivity_timeout: None,
        window_size: 8 * 1024 * 1024,
        nodelay: true,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

enum AuthError {
    /// Nothing could be tried: no password, no agent keys, no key files.
    NothingTried,
    Rejected,
    KeyFile(PathBuf, String),
    Timeout,
    Ssh(russh::Error),
}

impl From<russh::Error> for AuthError {
    fn from(e: russh::Error) -> Self {
        AuthError::Ssh(e)
    }
}

struct AuthPlan {
    user: String,
    password: Option<String>,
    key_file: Option<PathBuf>,
    use_keys: bool,
    agent: Agent,
    home: Option<PathBuf>,
    timeout: Duration,
}

async fn timed<T, E: Into<AuthError>>(
    limit: Duration,
    fut: impl Future<Output = Result<T, E>>,
) -> Result<T, AuthError> {
    match tokio::time::timeout(limit, fut).await {
        Ok(r) => r.map_err(Into::into),
        Err(_) => Err(AuthError::Timeout),
    }
}

async fn rsa_hash(h: &Handle<ClientHandler>, alg: &Algorithm) -> Option<HashAlg> {
    if !alg.clone().is_rsa() {
        return None;
    }
    h.best_supported_rsa_hash().await.ok().flatten().flatten()
}

async fn try_key(h: &mut Handle<ClientHandler>, plan: &AuthPlan, key: PrivateKey) -> Result<bool, AuthError> {
    let hash = rsa_hash(h, &key.algorithm()).await;
    let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
    Ok(timed(plan.timeout, h.authenticate_publickey(plan.user.clone(), key)).await?.success())
}

type DynAgent = AgentClient<Box<dyn AgentStream + Send + Unpin + 'static>>;

async fn agent_client(mode: &Agent) -> Option<DynAgent> {
    match mode {
        Agent::Off => None,
        #[cfg(unix)]
        Agent::Socket(path) => AgentClient::connect_uds(path).await.ok().map(AgentClient::dynamic),
        #[cfg(unix)]
        Agent::Auto => AgentClient::connect_env().await.ok().map(AgentClient::dynamic),
        #[cfg(windows)]
        Agent::Socket(path) => AgentClient::connect_named_pipe(path).await.ok().map(AgentClient::dynamic),
        #[cfg(windows)]
        Agent::Auto => match AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
            Ok(a) => Some(a.dynamic()),
            Err(_) => AgentClient::connect_pageant().await.ok().map(AgentClient::dynamic),
        },
        #[cfg(not(any(unix, windows)))]
        _ => None,
    }
}

/// Try, in paramiko's order: the key file, agent keys, default key files,
/// then the password (falling back to keyboard-interactive when the server
/// doesn't take passwords directly).
async fn authenticate(h: &mut Handle<ClientHandler>, plan: &AuthPlan) -> Result<(), AuthError> {
    let mut tried = false;
    let passphrase = plan.password.as_deref();

    if let Some(path) = &plan.key_file {
        let text = std::fs::read_to_string(path).map_err(|e| AuthError::KeyFile(path.clone(), oc_vfs::io_reason(&e)))?;
        tried = true;
        // An encrypted key without (or with a wrong) passphrase counts as a
        // failed attempt: the user is asked for the password, which unlocks it.
        if let Ok(key) = russh::keys::decode_secret_key(&text, passphrase)
            && try_key(h, plan, key).await?
        {
            return Ok(());
        }
    }

    if plan.use_keys {
        if let Some(mut agent) = agent_client(&plan.agent).await {
            let identities = tokio::time::timeout(plan.timeout, agent.request_identities())
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            for identity in identities {
                let AgentIdentity::PublicKey { key, .. } = identity else { continue };
                tried = true;
                let hash = rsa_hash(h, &key.algorithm()).await;
                let res = tokio::time::timeout(
                    plan.timeout,
                    h.authenticate_publickey_with(plan.user.clone(), key, hash, &mut agent),
                )
                .await;
                match res {
                    Ok(Ok(r)) if r.success() => return Ok(()),
                    Ok(_) => {}
                    Err(_) => return Err(AuthError::Timeout),
                }
            }
        }
        if let Some(home) = &plan.home {
            for name in ["rsa", "ecdsa", "ed25519"] {
                for dir in [".ssh", "ssh"] {
                    let path = home.join(dir).join(format!("id_{name}"));
                    let Ok(text) = std::fs::read_to_string(&path) else { continue };
                    tried = true;
                    if let Ok(key) = russh::keys::decode_secret_key(&text, passphrase)
                        && try_key(h, plan, key).await?
                    {
                        return Ok(());
                    }
                }
            }
        }
    }

    if let Some(password) = &plan.password {
        tried = true;
        let res = timed(plan.timeout, h.authenticate_password(plan.user.clone(), password.clone())).await?;
        match res {
            client::AuthResult::Success => return Ok(()),
            client::AuthResult::Failure { remaining_methods, .. } => {
                if !remaining_methods.contains(&MethodKind::Password)
                    && remaining_methods.contains(&MethodKind::KeyboardInteractive)
                    && keyboard_interactive(h, plan, password).await?
                {
                    return Ok(());
                }
            }
        }
    }

    if tried { Err(AuthError::Rejected) } else { Err(AuthError::NothingTried) }
}

/// paramiko's `auth_interactive_dumb` fallback: answer the (single) prompt with the password.
async fn keyboard_interactive(
    h: &mut Handle<ClientHandler>,
    plan: &AuthPlan,
    password: &str,
) -> Result<bool, AuthError> {
    let mut reply = timed(plan.timeout, h.authenticate_keyboard_interactive_start(plan.user.clone(), None)).await?;
    for _ in 0..8 {
        match reply {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                if prompts.len() > 1 {
                    return Ok(false); // not a plain password prompt (e.g. two-factor)
                }
                let answers = prompts.iter().map(|_| password.to_string()).collect();
                reply = timed(plan.timeout, h.authenticate_keyboard_interactive_respond(answers)).await?;
            }
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// connect
// ---------------------------------------------------------------------------

enum HandshakeError {
    Timeout,
    Io(io::Error),
    Ssh(russh::Error),
}

/// Connect and authenticate: with the password if given (or key file +
/// passphrase), otherwise with ssh-agent / default keys. Unknown host keys
/// are confirmed through `confirm` (and then remembered); a changed one is refused.
///
/// Errors: `AuthenticationRequired` (wrong or missing credentials),
/// `Cancelled` (host key not trusted), `Connection` (with a message).
pub fn connect(
    opts: &ConnectOptions,
    confirm: &dyn Fn(&HostKeyQuestion) -> bool,
) -> Result<Arc<SftpFileSystem>, ConnectError> {
    let host = opts.host.clone();
    let port = opts.port;
    let fail = |e: &dyn fmt::Display, unreachable: bool| {
        ConnectError::Connection(describe_connect_error(&host, port, e, unreachable))
    };
    let home = opts.home.clone().or_else(user_home);
    let name = known_hosts::host_key_name(&host, port);

    // paramiko: the system's known_hosts wins; ours is created if missing.
    if let Some(parent) = opts.known_hosts.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::OpenOptions::new().create(true).append(true).open(&opts.known_hosts);
    let mut known =
        home.as_ref().map(|h| known_hosts::lookup(&h.join(".ssh").join("known_hosts"), &name)).unwrap_or_default();
    if known.is_empty() {
        known = known_hosts::lookup(&opts.known_hosts, &name);
    }

    let check = Arc::new(Mutex::new(HostCheck::NotSeen));
    let config = Arc::new(client_config(&known));
    let known = Arc::new(Mutex::new(known));
    let handler = ClientHandler { known: known.clone(), check: check.clone() };
    let timeout = opts.timeout;
    let (h2, p2) = (host.clone(), port);
    let handshake = block_on(async move {
        let tcp = match tokio::time::timeout(timeout, tokio::net::TcpStream::connect((h2.as_str(), p2))).await {
            Err(_) => return Err(HandshakeError::Timeout),
            Ok(Err(e)) => return Err(HandshakeError::Io(e)),
            Ok(Ok(s)) => s,
        };
        match tokio::time::timeout(timeout, client::connect_stream(config, tcp, handler)).await {
            Err(_) => Err(HandshakeError::Timeout),
            Ok(Err(e)) => Err(HandshakeError::Ssh(e)),
            Ok(Ok(h)) => Ok(h),
        }
    })
    .map_err(|e| fail(&e, false))?;

    let verdict = std::mem::take(&mut *check.lock().unwrap_or_else(|e| e.into_inner()));
    if let HostCheck::Changed(key) = &verdict {
        return Err(ConnectError::Connection(format!(
            "The host key of {host} has changed (now {}). Someone may be intercepting the connection, \
             so {APP_NAME} didn't connect. If the server was reinstalled, confirm the new key with its \
             administrator, then remove the old one from known_hosts.",
            known_hosts::fingerprint(key)
        )));
    }
    let mut handle = match handshake {
        Ok(h) => h,
        Err(HandshakeError::Timeout) => {
            return Err(fail(&oc_provider::describe_network_error(io::ErrorKind::TimedOut, ""), false));
        }
        Err(HandshakeError::Io(e)) => return Err(fail(&network_error_text(&e), is_unreachable(&e))),
        Err(HandshakeError::Ssh(russh::Error::IO(e))) => return Err(fail(&network_error_text(&e), is_unreachable(&e))),
        Err(HandshakeError::Ssh(e)) => return Err(fail(&format!("the secure connection couldn't be set up ({e})"), false)),
    };
    if matches!(verdict, HostCheck::NotSeen) {
        disconnect(handle);
        return Err(fail(&"the server didn't identify itself", false));
    }

    if let HostCheck::Unknown(key) = &verdict {
        let question = HostKeyQuestion {
            host: host.clone(),
            port,
            key_type: known_hosts::key_type(key),
            fingerprint: known_hosts::fingerprint(key),
        };
        if !confirm(&question) {
            disconnect(handle);
            return Err(ConnectError::Cancelled);
        }
        known.lock().unwrap_or_else(|e| e.into_inner()).push(key.clone());
        if let Err(e) = known_hosts::append(&opts.known_hosts, &name, key) {
            log::warn!("could not remember the host key of {name} in {}: {e}", opts.known_hosts.display());
        }
    }

    let plan = AuthPlan {
        user: opts.username.clone().filter(|u| !u.is_empty()).unwrap_or_else(local_user),
        password: opts.password.clone(),
        key_file: opts.key_file.clone(),
        use_keys: opts.password.is_none() || opts.key_file.is_some(),
        agent: opts.agent.clone(),
        home,
        timeout,
    };
    let auth = block_on(async move {
        let result = authenticate(&mut handle, &plan).await;
        (handle, result)
    })
    .map_err(|e| fail(&e, false))?;
    let (handle, result) = auth;
    let shown_user = opts.username.clone().unwrap_or_default();
    if let Err(e) = result {
        let closed = handle.is_closed();
        disconnect(handle);
        return Err(match e {
            AuthError::NothingTried => {
                ConnectError::AuthenticationRequired(format!("{shown_user}@{host} needs a password"))
            }
            AuthError::Rejected => {
                ConnectError::AuthenticationRequired(format!("The server didn't accept the login for {shown_user}@{host}"))
            }
            // A server that hangs up after too many attempts rejected us.
            AuthError::Ssh(_) if closed => {
                ConnectError::AuthenticationRequired(format!("The server didn't accept the login for {shown_user}@{host}"))
            }
            AuthError::Ssh(e) => fail(&e, false),
            AuthError::Timeout => fail(&"the server didn't answer the login in time", false),
            AuthError::KeyFile(path, e) => {
                ConnectError::Connection(format!("Can't read the key file {}: {e}", path.display()))
            }
        });
    }

    let mut label = match opts.username.as_deref().filter(|u| !u.is_empty()) {
        Some(user) => format!("{user}@{host}"),
        None => host.clone(),
    };
    if port != DEFAULT_PORT {
        label.push_str(&format!(":{port}"));
    }
    let ssh = Arc::new(handle);
    let first = {
        let ssh = ssh.clone();
        block_on(async move { open_sftp(&ssh, timeout, false).await }).map_err(|e| fail(&e, false))?
    };
    let sftp = first.map_err(|e| fail(&e, false))?;
    let conn = Arc::new(Conn::new(ssh, label, timeout));
    let fs = Arc::new(SftpFileSystem::start(conn, Arc::new(sftp)).map_err(|e| fail(&e, false))?);
    SftpFileSystem::register(&fs);
    Ok(fs)
}

fn disconnect(handle: Handle<ClientHandler>) {
    crate::runtime::spawn(async move {
        let _ =
            tokio::time::timeout(Duration::from_secs(3), handle.disconnect(russh::Disconnect::ByApplication, "", "en"))
                .await;
    });
}
