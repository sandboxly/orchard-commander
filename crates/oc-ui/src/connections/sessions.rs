//! Live server sessions: one open file system per account, shared by both
//! panels, previews and copies (port of `connections.Sessions`).
//!
//! Thread-safe: connecting happens on workers. Two connects to the same
//! account at once don't open two sessions: the second waits for the first
//! and reuses its file system.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use oc_config::Connection;
use oc_provider::{ConnectContext, ConnectError};
use oc_vfs::{FileSystem, VPath};

use super::{account, provider_of};

struct Session {
    conn: Connection,
    fs: Arc<dyn FileSystem>,
}

#[derive(Default)]
pub struct Sessions {
    open: Mutex<HashMap<String, Session>>,
    /// One lock per account while a connection is being made.
    connecting: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn same(a: &dyn FileSystem, b: &dyn FileSystem) -> bool {
    std::ptr::addr_eq(a as *const dyn FileSystem, b as *const dyn FileSystem) || a.identity() == b.identity()
}

impl Sessions {
    /// The open, still usable file system of `conn`'s account.
    pub fn find(&self, conn: &Connection) -> Option<Arc<dyn FileSystem>> {
        let provider = provider_of(conn).ok()?;
        let fs = lock(&self.open).get(&account(conn)).map(|s| s.fs.clone())?;
        provider.is_connected(&fs).then_some(fs)
    }

    /// Open (or reuse) a session and return its start folder. Blocking: call
    /// on a worker. Errors: `AuthenticationRequired`, `Cancelled` (a question
    /// was declined) or whatever the provider reports.
    pub fn connect(&self, conn: &Connection, secret: Option<&str>, ctx: &dyn ConnectContext) -> Result<VPath, ConnectError> {
        let provider = provider_of(conn).map_err(ConnectError::Connection)?;
        let account = account(conn);
        let gate = lock(&self.connecting).entry(account.clone()).or_default().clone();
        let _connecting = lock(&gate);
        let fs = match self.find(conn) {
            Some(fs) => fs,
            None => {
                let fs = provider.connect(&conn.params, secret, ctx)?;
                let old = lock(&self.open).insert(account, Session { conn: conn.clone(), fs: fs.clone() });
                if let Some(old) = old {
                    old.fs.close();
                }
                fs
            }
        };
        let start = provider.start_path(&conn.params, &fs);
        Ok(VPath::new(fs, start))
    }

    /// The account of an open server file system (for bookmarks: `spec_of`).
    pub fn owner_of(&self, fs: &dyn FileSystem) -> Option<String> {
        lock(&self.open).iter().find(|(_, s)| same(&*s.fs, fs)).map(|(a, _)| a.clone())
    }

    /// The connection a server file system was opened with.
    pub fn connection_of(&self, fs: &dyn FileSystem) -> Option<Connection> {
        lock(&self.open).values().find(|s| same(&*s.fs, fs)).map(|s| s.conn.clone())
    }

    /// Forget a session without closing it here (it is broken anyway, or
    /// the caller closes it on a worker).
    pub fn forget(&self, fs: &dyn FileSystem) -> Option<Arc<dyn FileSystem>> {
        let mut open = lock(&self.open);
        let account = open.iter().find(|(_, s)| same(&*s.fs, fs)).map(|(a, _)| a.clone())?;
        open.remove(&account).map(|s| s.fs)
    }

    /// Close a session (blocking: network).
    pub fn disconnect(&self, fs: &dyn FileSystem) {
        if let Some(fs) = self.forget(fs) {
            fs.close();
        }
    }

    /// Every open session: (account, connection), sorted by account.
    pub fn list(&self) -> Vec<(String, Connection)> {
        let mut all: Vec<_> = lock(&self.open).iter().map(|(a, s)| (a.clone(), s.conn.clone())).collect();
        all.sort_by(|a, b| a.0.cmp(&b.0));
        all
    }

    /// Close everything (quitting). Blocking.
    pub fn close_all(&self) {
        let all: Vec<_> = lock(&self.open).drain().map(|(_, s)| s.fs).collect();
        for fs in all {
            fs.close();
        }
    }
}

/// The outermost file system of a location (the server a nested archive lives on).
pub fn root_filesystem(location: &VPath) -> Arc<dyn FileSystem> {
    let mut fs = location.fs.clone();
    while let Some(container) = fs.container() {
        fs = container.fs;
    }
    fs
}
