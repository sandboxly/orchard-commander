//! What other features use of the connections feature (bookmarks, history…).
//! Agreed API — keep these signatures stable:
//!
//! ```ignore
//! use oc_ui::connections::api;
//!
//! // A bookmark's {"kind": "server", "account": …, "path": …} → a live location.
//! api::resolve_server(app, &account, &path, move |app, location| {
//!     if let Some(location) = location { panel::load(app, side, location, LoadOptions::default()) }
//! });
//!
//! // The other way, for `oc_config::locations::spec_of`:
//! let spec = oc_config::locations::spec_of(&location, &api::spec_context(app));
//! let account = api::account_of(app, &*location.fs);
//! ```

use std::sync::Arc;

use oc_config::SpecContext;
use oc_vfs::{FileSystem, VPath};

use super::{Connections, Sessions, Target, from_url, open_connection, with_path, with_saved};
use crate::app::App;

/// Connect to the server of `account` (a connection account as stored in
/// bookmarks: `provider.account(params)`, e.g. `sftp://me@host`) and resolve
/// `path` on it. Uses the saved connection with that account if there is one
/// (its key file, region, name and keychain password), else the account
/// parsed as an address. Reuses a live session; otherwise connects on a
/// worker, asking the user for host-key trust and passwords as needed.
///
/// `on_done` runs on the UI thread with the location, or `None` if
/// connecting failed or was cancelled (the user has been told why). It does
/// not touch the panels: the caller decides where the location goes. Port
/// of the "server" branch of 1.x's `MainWindow.resolve`.
pub fn resolve_server(app: &mut App, account: &str, path: &str, on_done: impl FnOnce(&mut App, Option<VPath>) + 'static) {
    let (account, path) = (account.to_string(), path.to_string());
    with_saved(app, move |app| {
        let saved = app.state::<Connections>().saved.iter().find(|c| super::account(c) == account).cloned();
        let conn = match saved.map(Ok).unwrap_or_else(|| from_url(&account)) {
            Ok(conn) => conn,
            Err(e) => {
                app.error(e);
                on_done(app, None);
                return;
            }
        };
        open_connection(app, with_path(&conn, &path), Target::Then(Box::new(on_done)), None, false);
    });
}

/// The connection account of an open server file system (the outermost
/// one: pass `root_filesystem(&location)` for archives on servers), or
/// `None` if it isn't one of the app's sessions.
pub fn account_of(app: &App, fs: &dyn FileSystem) -> Option<String> {
    app.try_state::<Connections>()?.sessions.owner_of(fs)
}

/// A [`SpecContext`] that knows the app's server sessions, for
/// `oc_config::locations::spec_of` (bookmarking a folder on a server). Cheap;
/// usable on workers.
pub fn spec_context(app: &App) -> ServerAccounts {
    ServerAccounts(app.try_state::<Connections>().map(|s| s.sessions.clone()))
}

/// See [`spec_context`].
#[derive(Clone)]
pub struct ServerAccounts(Option<Arc<Sessions>>);

impl SpecContext for ServerAccounts {
    fn account_of(&self, fs: &dyn FileSystem) -> Option<String> {
        self.0.as_ref()?.owner_of(fs)
    }
}

/// Is `location` on a server (also inside an archive on one)?
pub fn is_on_server(app: &App, location: &VPath) -> bool {
    account_of(app, &*super::root_filesystem(location)).is_some()
}
