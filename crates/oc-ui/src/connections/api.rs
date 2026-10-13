//! What other features may ask of the connections feature.
//!
//! Placeholder until the connections feature lands: its version of this file
//! replaces this one (same signature).

use oc_vfs::VPath;

use crate::app::App;

/// Connect to the saved connection with this account (asking for a password
/// or host-key trust as needed) and hand back `path` on it.
pub fn resolve_server(
    app: &mut App,
    account: &str,
    path: &str,
    on_done: impl FnOnce(&mut App, Result<VPath, String>) + 'static,
) {
    let _ = (account, path);
    on_done(app, Err("Not connected".into()));
}
