//! Servers in the UI: saved connections (⌘K), connecting on a worker with
//! the user answering host-key / certificate / password questions, live
//! sessions shared per account, ⇧⌘K disconnect, server addresses in ⌘G and
//! the ⌥F1/⌥F2 chooser, reconnecting when a session dropped. Port of
//! `connections.py`, `ui/connect_dialogs.py` and the connection part of
//! `ui/main_window.py`.
//!
//! * Connection types are the `oc_provider` registry (the binary registers
//!   SFTP, SMB, WebDAV and S3 at start). A [`Connection`] (connections.json,
//!   via `oc_config`) is a type plus that type's params; its *account*
//!   (`provider.account(params)`, e.g. `sftp://me@host`) identifies the
//!   session and is the keychain user name under service
//!   "Orchard Commander" — the exact strings 1.x uses.
//! * All I/O (connections.json, the keychain, the network) runs on workers.
//!   State lives in [`Connections`] (`app.state::<Connections>()`).
//! * Other features use [`api`] (`resolve_server` for bookmarks).

pub mod api;
mod dialogs;
mod sessions;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oc_config::connections as saved_store;
use oc_config::{Connection, CredentialError, CredentialStore, OsKeyring, Passwords};
use oc_provider::{ConnectContext, ConnectError, FileSystemProvider};
use oc_vfs::{VPath, VfsError};

use crate::app::App;
use crate::commands::{Command, Registry};
use crate::features::navigation::LocationItem;
use crate::main_window::{self, SideMode};
use crate::menus::Menu;
use crate::panel::{self, LoadOptions};
use crate::tasks::Tasks;

pub use dialogs::Credentials;
pub use sessions::{Sessions, root_filesystem};

/// An automatic reconnect of the same account at most this often (a server
/// that keeps dropping the connection must not loop).
const RECONNECT_INTERVAL: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Gets the location once connected (None: failed or cancelled).
type Then = Box<dyn FnOnce(&mut App, Option<VPath>)>;
/// Waits for the saved list.
type Waiting = Box<dyn FnOnce(&mut App)>;

/// Where a connection's start folder goes once connected.
enum Target {
    /// Show it in this side's file panel (and focus it).
    Panel(usize),
    /// Hand it over (None: failed or cancelled; the user was told).
    Then(Then),
}

/// The feature's app-wide state: `app.state::<Connections>()`.
pub struct Connections {
    /// Open file systems by account (shared with workers).
    pub sessions: Arc<Sessions>,
    /// Saved connections (connections.json, file order), loaded in the
    /// background at start and refreshed when the manager opens.
    pub saved: Vec<Connection>,
    pub loaded: bool,
    /// Where passwords are (the OS keychain; memory in tests).
    store: Arc<dyn CredentialStore>,
    /// Targets of connects in flight (not `Send`, so they wait here).
    pending: HashMap<u64, Target>,
    next_request: u64,
    /// Keeps connections.json writes in order.
    write_lock: Arc<Mutex<()>>,
    /// Last automatic reconnect per account.
    reconnected: HashMap<String, Instant>,
    /// Work waiting for the saved list to be loaded.
    waiting: Vec<Waiting>,
    /// The connection highlighted in the manager (by id).
    pub(crate) manager_selected: Option<String>,
}

impl Default for Connections {
    fn default() -> Self {
        Connections {
            sessions: Arc::default(),
            saved: Vec::new(),
            loaded: false,
            store: Arc::new(OsKeyring),
            pending: HashMap::new(),
            next_request: 1,
            write_lock: Arc::default(),
            reconnected: HashMap::new(),
            waiting: Vec::new(),
            manager_selected: None,
        }
    }
}

/// Use `store` for passwords instead of the OS keychain (tests).
pub fn use_credential_store(app: &mut App, store: Arc<dyn CredentialStore>) {
    app.state::<Connections>().store = store;
}

/// `Passwords` over a shared store.
struct SharedStore(Arc<dyn CredentialStore>);

impl CredentialStore for SharedStore {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, CredentialError> {
        self.0.get(service, account)
    }
    fn set(&self, service: &str, account: &str, password: &str) -> Result<(), CredentialError> {
        self.0.set(service, account, password)
    }
    fn delete(&self, service: &str, account: &str) -> Result<(), CredentialError> {
        self.0.delete(service, account)
    }
}

fn passwords(store: &Arc<dyn CredentialStore>) -> Passwords<SharedStore> {
    Passwords::new(SharedStore(store.clone()))
}

// ---------------------------------------------------------------------------
// Connection helpers (Python's Connection properties)
// ---------------------------------------------------------------------------

pub(crate) fn provider_of(conn: &Connection) -> Result<Arc<dyn FileSystemProvider>, String> {
    oc_provider::get(&conn.kind).ok_or_else(|| format!("No support for {}:// connections (add-on not installed?)", conn.kind))
}

/// Identifies the server account: shared sessions, the keychain user name.
pub fn account(conn: &Connection) -> String {
    provider_of(conn).map_or_else(|_| conn.fallback_account(), |p| p.account(&conn.params))
}

/// The connection's address.
pub fn url(conn: &Connection) -> String {
    provider_of(conn).map_or_else(|_| format!("{}://", conn.kind), |p| p.format_url(&conn.params))
}

/// The name, or the address.
pub fn label(conn: &Connection) -> String {
    if conn.name.is_empty() { url(conn) } else { conn.name.clone() }
}

/// Short, e.g. "root@server" (password prompts, the location chooser).
pub fn description(conn: &Connection) -> String {
    provider_of(conn).map_or_else(|_| label(conn), |p| p.describe(&conn.params))
}

/// Whether `text` is an address of a registered connection type.
pub fn is_url(text: &str) -> bool {
    oc_provider::scheme_of(text).is_some()
}

/// Parse an address of any registered connection type.
pub fn from_url(url: &str) -> Result<Connection, String> {
    let Some(scheme) = oc_provider::scheme_of(url) else {
        return Err(format!("Unsupported address (use {}): {url}", oc_provider::address_examples()));
    };
    let provider = oc_provider::get(&scheme).ok_or_else(|| format!("Unsupported address: {url}"))?;
    Ok(Connection::new(scheme, provider.parse_url(url)?, ""))
}

/// The same connection starting in `path`.
fn with_path(conn: &Connection, path: &str) -> Connection {
    conn.with_params([("path".to_string(), serde_json::Value::String(path.to_string()))])
}

/// A declined host key / certificate, worded like 1.x's exceptions.
fn not_trusted(conn: &Connection) -> String {
    let host = conn.get_str("host");
    let host = if host.is_empty() { "the server" } else { host };
    if conn.kind == "sftp" { format!("Host key for {host} was not trusted") } else { format!("The certificate of {host} was not trusted") }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(reg: &mut Registry) {
    reg.add(
        Command::new("conn.manager", "Connect to server", show_manager)
            .help("Saved server connections (SFTP, SMB, WebDAV, S3)")
            .keys(&["Primary+K"])
            .footer_right("Connect")
            .menu(Menu::Commands, "servers"),
    );
    reg.add(
        Command::new("conn.disconnect", "Disconnect", disconnect)
            .help("Close the connection of the active panel's server")
            .keys(&["Primary+Shift+K"])
            .menu(Menu::Commands, "servers"),
    );
    reg.on_start(load_saved);
    reg.add_location_source(location_items);
    reg.on_load_error(load_failed);
    reg.on_shutdown(close_all);
}

// ---------------------------------------------------------------------------
// Saved connections
// ---------------------------------------------------------------------------

/// Re-read connections.json in the background.
pub(crate) fn load_saved(app: &mut App) {
    let path = app.paths.connections_file();
    app.tasks.spawn(move || saved_store::load_saved(&path), set_saved);
}

fn set_saved(app: &mut App, items: Vec<Connection>) {
    let st = app.state::<Connections>();
    st.saved = items;
    st.loaded = true;
    let waiting = std::mem::take(&mut st.waiting);
    for f in waiting {
        f(app);
    }
    app.redraw();
}

/// Run `f` once the saved list is known.
fn with_saved(app: &mut App, f: impl FnOnce(&mut App) + 'static) {
    let st = app.state::<Connections>();
    if st.loaded {
        f(app);
        return;
    }
    let first = st.waiting.is_empty();
    st.waiting.push(Box::new(f));
    if first {
        load_saved(app);
    }
}

/// Saved connections sorted by label, as the manager lists them.
pub(crate) fn sorted_saved(app: &App) -> Vec<Connection> {
    let mut items = app.try_state::<Connections>().map(|s| s.saved.clone()).unwrap_or_default();
    items.sort_by_cached_key(|c| label(c).to_lowercase());
    items
}

/// Add or update (by id) a saved connection; then remember or forget its
/// password as the editor said (port of `ConnectionManager._apply`). Unlike
/// 1.x, an edit that changes the account (server, port, user…) with
/// "Remember" still ticked and no new password typed moves the stored
/// password along instead of silently losing it.
pub(crate) fn save_connection(app: &mut App, conn: Connection, creds: Option<Credentials>) {
    let st = app.state::<Connections>();
    let before = st.saved.iter().find(|c| c.id == conn.id).map(account);
    st.saved.retain(|c| c.id != conn.id);
    // Accounts the other saved connections still use (their passwords stay).
    let others: Vec<String> = st.saved.iter().map(account).collect();
    st.saved.push(conn.clone());
    st.manager_selected = Some(conn.id.clone());
    let (lock, store, path) = (st.write_lock.clone(), st.store.clone(), app.paths.connections_file());
    app.tasks.spawn(
        move || {
            let _order = lock.lock().unwrap_or_else(|p| p.into_inner());
            let result = saved_store::save(&path, &conn);
            if let Some(creds) = creds {
                let pw = passwords(&store);
                let acc = account(&conn);
                let old = before.filter(|b| *b != acc);
                let forget_old = |pw: &Passwords<SharedStore>| {
                    if let Some(old) = &old
                        && !others.contains(old)
                    {
                        pw.forget(old);
                    }
                };
                match (creds.remember, creds.password.as_deref().filter(|p| !p.is_empty())) {
                    (true, Some(p)) => {
                        pw.set(&acc, p);
                    }
                    (true, None) => {
                        if let Some(moved) = old.as_deref().and_then(|o| pw.get(o)) {
                            if pw.get(&acc).is_none() {
                                pw.set(&acc, &moved);
                            }
                            forget_old(&pw);
                        }
                    }
                    (false, _) => {
                        pw.forget(&acc);
                        forget_old(&pw);
                    }
                }
            }
            (result, saved_store::load_saved(&path))
        },
        |app, (result, items)| {
            if let Err(e) = result {
                app.error(format!("Could not save connections: {e}"));
            }
            set_saved(app, items);
        },
    );
}

/// Remove a saved connection, and its password unless another saved
/// connection uses the same account (e.g. a duplicate with another start
/// folder: 1.x forgot the password both shared).
pub(crate) fn delete_connection(app: &mut App, conn: Connection) {
    let st = app.state::<Connections>();
    st.saved.retain(|c| c.id != conn.id);
    let acc = account(&conn);
    let shared = st.saved.iter().any(|c| account(c) == acc);
    let (lock, store, path) = (st.write_lock.clone(), st.store.clone(), app.paths.connections_file());
    app.tasks.spawn(
        move || {
            let _order = lock.lock().unwrap_or_else(|p| p.into_inner());
            let result = saved_store::remove(&path, &conn.id);
            if !shared {
                passwords(&store).forget(&acc);
            }
            (result, saved_store::load_saved(&path))
        },
        |app, (result, items)| {
            if let Err(e) = result {
                app.error(format!("Could not save connections: {e}"));
            }
            set_saved(app, items);
        },
    );
}

/// Whether a password is stored for `conn` (keychain read, on a worker).
pub(crate) fn has_password(app: &mut App, conn: &Connection, then: impl FnOnce(&mut App, bool) + Send + 'static) {
    let store = app.state::<Connections>().store.clone();
    let acc = account(conn);
    app.tasks.spawn(move || passwords(&store).get(&acc).is_some(), then);
}

// ---------------------------------------------------------------------------
// Connecting
// ---------------------------------------------------------------------------

/// What providers may use while connecting: questions go to the UI thread
/// (`ask_ui`), files to the config dir (known_hosts, trusted_certificates.json).
struct UiContext {
    tasks: Tasks,
    data_dir: PathBuf,
}

impl ConnectContext for UiContext {
    fn data_dir(&self) -> PathBuf {
        self.data_dir.clone()
    }

    fn confirm(&self, title: &str, message: &str, ok_label: &str) -> bool {
        let (title, message, ok) = (title.to_string(), message.to_string(), ok_label.to_string());
        self.tasks.ask_ui(move |app, reply| dialogs::ask_question(app, &title, &message, &ok, reply)).unwrap_or(false)
    }
}

/// ⌘K: the saved connections.
pub fn show_manager(app: &mut App) {
    load_saved(app);
    dialogs::show_manager(app);
}

/// Connect `conn` and show its start folder in panel `side`.
pub fn connect_in_panel(app: &mut App, side: usize, conn: Connection, secret: Option<String>) {
    open_connection(app, conn, Target::Panel(side), secret, false);
}

/// ⌘G with a server address.
pub fn open_url(app: &mut App, side: usize, url: &str) {
    match from_url(url) {
        Ok(conn) => connect_in_panel(app, side, conn, None),
        Err(e) => app.error(e),
    }
}

/// Connect in the background (asking for host-key trust / a password as
/// needed), then browse the start folder or hand it to the target. A stored
/// password is used unless `secret` is given; with `remember`, a password
/// that worked is stored.
fn open_connection(app: &mut App, conn: Connection, target: Target, secret: Option<String>, remember: bool) {
    if let Target::Panel(side) = target
        && app.main.sides[side].mode != SideMode::Files
    {
        main_window::set_mode(app, side, SideMode::Files);
    }
    let text = format!("Connecting to {}…", label(&conn));
    app.info(text.clone());
    let st = app.state::<Connections>();
    let id = st.next_request;
    st.next_request += 1;
    st.pending.insert(id, target);
    let (sessions, store) = (st.sessions.clone(), st.store.clone());
    let ctx = UiContext { tasks: app.tasks.clone(), data_dir: app.paths.data_dir().to_path_buf() };
    let worker_conn = conn.clone();
    app.tasks.spawn(
        move || {
            let conn = worker_conn;
            let pw = passwords(&store);
            let acc = account(&conn);
            // A live session needs no secret: don't touch the keychain.
            let password = match secret.filter(|s| !s.is_empty()) {
                Some(s) => Some(s),
                None if sessions.find(&conn).is_some() => None,
                None => pw.get(&acc),
            };
            let result = sessions.connect(&conn, password.as_deref(), &ctx);
            if result.is_ok()
                && remember
                && let Some(p) = &password
            {
                pw.set(&acc, p);
            }
            (result, password.is_some())
        },
        move |app, (result, had_password)| connected(app, id, conn, &text, result, had_password),
    );
}

fn connected(app: &mut App, id: u64, conn: Connection, text: &str, result: Result<VPath, ConnectError>, had_password: bool) {
    let Some(target) = app.state::<Connections>().pending.remove(&id) else {
        return;
    };
    if app.toast.as_ref().is_some_and(|t| t.text == text) {
        app.toast = None;
    }
    match result {
        Ok(location) => arrive(app, target, Some(location)),
        Err(ConnectError::AuthenticationRequired(_)) => {
            let reason = if had_password { "The server didn't accept the password." } else { "" };
            dialogs::ask_password(app, &conn.clone(), reason, move |app, creds| match creds {
                Some(Credentials { password: Some(p), remember }) if !p.is_empty() => {
                    open_connection(app, conn, target, Some(p), remember);
                }
                _ => arrive(app, target, None),
            });
        }
        Err(ConnectError::Cancelled) => {
            app.warn(not_trusted(&conn));
            arrive(app, target, None);
        }
        Err(e) => {
            app.error(e.to_string());
            arrive(app, target, None);
        }
    }
}

fn arrive(app: &mut App, target: Target, location: Option<VPath>) {
    match target {
        Target::Panel(side) => {
            if let Some(location) = location {
                app.main.active = side;
                panel::load(app, side, location, LoadOptions::default());
            }
        }
        Target::Then(f) => f(app, location),
    }
}

// ---------------------------------------------------------------------------
// Disconnecting, quitting
// ---------------------------------------------------------------------------

/// ⇧⌘K: close the active panel's server; panels on it go to the home folder.
pub fn disconnect(app: &mut App) {
    let side = app.main.files_side();
    let server = root_filesystem(&main_window::location(app, side));
    let sessions = app.state::<Connections>().sessions.clone();
    if sessions.owner_of(&*server).is_none() {
        app.warn("The active panel isn't on a server");
        return;
    }
    for s in 0..2 {
        let p = &app.main.sides[s].panel;
        let here = root_filesystem(&p.location);
        let going = root_filesystem(&p.loading_location);
        if here.identity() == server.identity() || going.identity() == server.identity() {
            panel::load(app, s, oc_local::local(oc_local::home_dir()), LoadOptions::default());
        }
    }
    // Forget it now (a new connect must not reuse it); closing may wait for the network.
    let fs = sessions.forget(&*server);
    app.tasks.spawn(
        move || {
            if let Some(fs) = fs {
                fs.close();
            }
        },
        |app, ()| app.info("Disconnected"),
    );
}

/// Quitting: close every session, waiting at most a moment.
fn close_all(app: &mut App) {
    let Some(st) = app.try_state::<Connections>() else { return };
    let sessions = st.sessions.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = std::thread::Builder::new().name("oc-disconnect".into()).spawn(move || {
        sessions.close_all();
        let _ = tx.send(());
    });
    let _ = rx.recv_timeout(Duration::from_secs(2));
}

// ---------------------------------------------------------------------------
// The ⌥F1 / ⌥F2 chooser
// ---------------------------------------------------------------------------

/// Saved connections ("⇄ name"), then open sessions that aren't saved (e.g. from ⌘G).
fn location_items(app: &App, _side: usize) -> Vec<LocationItem> {
    let Some(st) = app.try_state::<Connections>() else { return Vec::new() };
    let mut items = Vec::new();
    let saved_accounts: Vec<String> = st.saved.iter().map(account).collect();
    for conn in &st.saved {
        let shown = if conn.name.is_empty() { description(conn) } else { conn.name.clone() };
        let c = conn.clone();
        items.push(LocationItem {
            label: format!("⇄ {shown}"),
            hint: url(conn),
            open: Box::new(move |app, side| connect_in_panel(app, side, c, None)),
        });
    }
    for (acc, conn) in st.sessions.list() {
        if saved_accounts.contains(&acc) {
            continue;
        }
        let c = conn.clone();
        items.push(LocationItem {
            label: format!("⇄ {}", description(&conn)),
            hint: url(&conn),
            open: Box::new(move |app, side| connect_in_panel(app, side, c, None)),
        });
    }
    items
}

// ---------------------------------------------------------------------------
// A session that broke while browsing
// ---------------------------------------------------------------------------

/// Whether an error means the session itself is broken: the connection was
/// lost, or the server stopped accepting the credentials (a changed
/// certificate is an alarm, not something to retry).
fn session_broken(error: &VfsError) -> bool {
    let VfsError::Io(e) = error else { return false };
    if let Some(remote) = e.get_ref().and_then(|inner| inner.downcast_ref::<oc_remote_http::RemoteError>()) {
        use oc_remote_http::RemoteError::*;
        return matches!(remote, AuthenticationRequired(_) | Connection(_));
    }
    use std::io::ErrorKind::*;
    matches!(e.kind(), NotConnected | ConnectionAborted | ConnectionReset | BrokenPipe | UnexpectedEof)
}

/// Listing a server folder failed: if the session broke, reconnect (with
/// the stored password; if the server wants another, the password prompt
/// follows) and list the folder again.
fn load_failed(app: &mut App, side: usize, location: &VPath, error: &VfsError) -> bool {
    if location.fs.container().is_some() {
        return false;
    }
    let Some(st) = app.try_state::<Connections>() else { return false };
    let Some(conn) = st.sessions.connection_of(&*location.fs) else { return false };
    let alive = provider_of(&conn).is_ok_and(|p| p.is_connected(&location.fs));
    if alive && !session_broken(error) {
        return false;
    }
    let acc = account(&conn);
    let now = Instant::now();
    let st = app.state::<Connections>();
    if st.reconnected.get(&acc).is_some_and(|t| now.duration_since(*t) < RECONNECT_INTERVAL) {
        return false; // just tried: show the error
    }
    st.reconnected.insert(acc, now);
    // A new session, not the broken one.
    if let Some(old) = st.sessions.forget(&*location.fs) {
        app.tasks.spawn(move || old.close(), |_, ()| {});
    }
    let path = location.path.clone();
    let broken = location.fs.clone();
    // Only if the panel wasn't sent elsewhere while reconnecting.
    let request = app.main.sides[side].panel.request;
    let target = Target::Then(Box::new(move |app, loc: Option<VPath>| {
        let Some(loc) = loc else { return };
        let walk_up = || LoadOptions { walk_up: true, ..Default::default() };
        // The other panel on the dropped session follows (its session is gone).
        let other = 1 - side;
        let there = &app.main.sides[other].panel.location;
        if there.fs.identity() == broken.identity() {
            let moved = VPath::new(loc.fs.clone(), there.path.clone());
            panel::load(app, other, moved, walk_up());
        }
        if app.main.sides[side].panel.request == request {
            panel::load(app, side, VPath::new(loc.fs, path), walk_up());
        }
    }));
    open_connection(app, conn, target, None, false);
    true
}

#[cfg(test)]
mod tests;
