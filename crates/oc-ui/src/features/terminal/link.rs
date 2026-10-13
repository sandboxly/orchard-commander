//! Linked folders: a side's shell and panel follow each other (Settings →
//! Terminal → "Link each side's terminal and panel folders", `terminal_link`).
//! Port of `MainWindow._sync_terminals` / `_panel_moved` / `_as_seen_from`.
//!
//! A "terminal link" thread looks at the shells every 300 ms — where each is
//! (OSC 7, else the process's folder), whether it waits at its prompt, how
//! many commands ran — and posts to the UI thread only when that changed, so
//! an idle terminal costs no wake-ups and no redraws. On the UI thread:
//!
//! * the shell moved (`cd` in the terminal) → the panel opens that folder,
//!   written relative to the panel's path where it is the same place
//!   ([`as_seen_from`]: a panel in /tmp goes to /tmp/sub, not /private/tmp/sub);
//! * the panel moved → a `cd` is typed, but only while the shell waits at
//!   its prompt ([`oc_term::Session::at_prompt`]); otherwise once it does;
//! * a command finished → local panels reload (folder watching misses size changes).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use oc_term::Session;

use super::Terminal;
use crate::app::App;
use crate::panel::{self, LoadOptions};

/// How often the shells are looked at.
pub const INTERVAL: Duration = Duration::from_millis(300);

/// Per side: the shell's folder as last seen, a `cd` waiting for the prompt,
/// the commands counted when one last finished.
#[derive(Default, Debug)]
pub struct Link {
    shell_dirs: [Option<PathBuf>; 2],
    pending_cd: [Option<PathBuf>; 2],
    commands_seen: [u64; 2],
}

impl Link {
    /// A new shell on `side`: nothing seen yet.
    pub fn reset(&mut self, side: usize) {
        self.shell_dirs[side] = None;
        self.pending_cd[side] = None;
        self.commands_seen[side] = 0;
    }

    /// The shell's folder as last seen (tests).
    pub fn shell_dir(&self, side: usize) -> Option<&Path> {
        self.shell_dirs[side].as_deref()
    }
}

type Entries = [Option<(u64, Arc<Session>)>; 2];

/// The sessions the link thread looks at; the thread runs while this exists.
#[derive(Default)]
pub struct Watched {
    entries: Arc<Mutex<Entries>>,
    running: bool,
}

impl Watched {
    pub fn set(&mut self, side: usize, generation: u64, session: Option<Arc<Session>>) {
        let mut e = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        e[side] = session.map(|s| (generation, s));
    }
}

/// What the thread saw of one shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    pub alive: bool,
    pub at_prompt: bool,
    pub commands: u64,
    pub cwd: Option<PathBuf>,
}

impl Probe {
    fn of(s: &Session) -> Probe {
        let alive = s.alive();
        Probe { alive, at_prompt: alive && s.at_prompt(), commands: s.commands(), cwd: if alive { s.current_dir() } else { None } }
    }
}

/// Start the link thread (once).
pub(super) fn watch(app: &mut App) {
    let tasks = app.tasks.clone();
    let t = app.state::<Terminal>();
    if t.watched.running {
        return;
    }
    t.watched.running = true;
    let entries: Weak<Mutex<Entries>> = Arc::downgrade(&t.watched.entries);
    let spawned = std::thread::Builder::new().name("terminal link".into()).spawn(move || {
        let mut last: [Option<(u64, Probe)>; 2] = [None, None];
        loop {
            std::thread::sleep(INTERVAL);
            let Some(entries) = entries.upgrade() else { return };
            let current = entries.lock().unwrap_or_else(|p| p.into_inner()).clone();
            drop(entries);
            for (side, entry) in current.into_iter().enumerate() {
                let Some((generation, session)) = entry else {
                    last[side] = None;
                    continue;
                };
                let probe = Probe::of(&session);
                drop(session);
                let seen = Some((generation, probe.clone()));
                if last[side] != seen {
                    last[side] = seen;
                    tasks.post(move |app| synced(app, side, generation, probe));
                }
            }
        }
    });
    if spawned.is_err() {
        log::error!("cannot start the terminal link thread");
        app.state::<Terminal>().watched.running = false;
    }
}

/// A shell changed (see the module docs): follow it, type a pending `cd`, refresh.
pub fn synced(app: &mut App, side: usize, generation: u64, p: Probe) {
    super::ensure_side(app);
    let t = app.state::<Terminal>();
    let Some(session) = t.session(side).cloned().filter(|_| t.generations[side] == generation) else {
        return;
    };
    if !p.alive {
        return;
    }
    let mut finished = false;
    if p.commands != t.link.commands_seen[side] && p.at_prompt {
        t.link.commands_seen[side] = p.commands;
        finished = true;
    }
    if app.settings.terminal_link_enabled()
        && let Some(now) = p.cwd
    {
        let t = app.state::<Terminal>();
        let seen = t.link.shell_dirs[side].clone();
        if !seen.as_deref().is_some_and(|s| same_dir(&now, s)) {
            // The shell moved (cd in the terminal).
            t.link.shell_dirs[side] = Some(now.clone());
            let panel_dir = panel_dir(app, side);
            if seen.is_some() && !panel_dir.as_deref().is_some_and(|d| same_dir(&now, d)) && now.is_dir() {
                app.state::<Terminal>().link.pending_cd[side] = None;
                let target = as_seen_from(&now, panel_dir.as_deref());
                panel::load(app, side, oc_local::local(target), LoadOptions::default());
            }
        } else if p.at_prompt
            && let Some(pending) = t.link.pending_cd[side].take()
            && !same_dir(&pending, &now)
        {
            // The panel moved while a program ran: catch up now.
            let cd = oc_term::cd_command(session.argv(), &pending);
            super::send(app, side, cd.into_bytes(), super::Input::Typed);
        }
    }
    if finished {
        // rm, touch, echo >> …: show the result (watching folders misses some).
        for s in 0..2 {
            if app.main.sides[s].panel.location.is_local() {
                panel::reload(app, s);
            }
        }
    }
}

/// The panel of `side` arrived in another folder: take that side's shell
/// along when it is idle at its prompt, else once it is (registered as a
/// location hook).
pub fn panel_moved(app: &mut App, side: usize) {
    if !app.settings.terminal_link_enabled() {
        return;
    }
    let location = app.main.sides[side].panel.location.clone();
    let Some(t) = app.try_state::<Terminal>() else { return };
    let Some(session) = t.session(side).cloned().filter(|s| s.alive()) else { return };
    if !location.is_local() {
        return; // servers, archives, search results: the shell stays where it is
    }
    let folder = PathBuf::from(&location.path);
    if t.link.shell_dirs[side].as_deref().is_some_and(|d| same_dir(&folder, d)) {
        return; // already there (e.g. the shell moved the panel)
    }
    if session.at_prompt() {
        app.state::<Terminal>().link.pending_cd[side] = None;
        let cd = oc_term::cd_command(session.argv(), &folder);
        super::send(app, side, cd.into_bytes(), super::Input::Typed);
    } else {
        app.state::<Terminal>().link.pending_cd[side] = Some(folder); // don't type into vim or a running build
    }
}

fn panel_dir(app: &App, side: usize) -> Option<PathBuf> {
    let location = &app.main.sides[side].panel.location;
    location.is_local().then(|| PathBuf::from(&location.path))
}

/// The real (symlink-free) path, without Windows' `\\?\` prefix.
fn real(path: &Path) -> Option<PathBuf> {
    let p = std::fs::canonicalize(path).ok()?;
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\")
            && !rest.starts_with("UNC\\")
        {
            return Some(PathBuf::from(rest));
        }
    }
    Some(p)
}

/// Whether two paths are the same folder (symlinks resolved).
pub fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (real(a), real(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// `folder` (a shell's physical path) written relative to the panel's path
/// where possible, so a panel in /tmp goes to /tmp/sub, not /private/tmp/sub.
pub fn as_seen_from(folder: &Path, panel_dir: Option<&Path>) -> PathBuf {
    let Some(panel_dir) = panel_dir else {
        return folder.to_path_buf();
    };
    if let Some(real_dir) = real(panel_dir) {
        if let Ok(rest) = folder.strip_prefix(&real_dir) {
            return if rest.as_os_str().is_empty() { panel_dir.to_path_buf() } else { panel_dir.join(rest) };
        }
        for ancestor in panel_dir.ancestors().skip(1) {
            if real(ancestor).as_deref() == Some(folder) {
                return ancestor.to_path_buf();
            }
        }
    }
    folder.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn symlinked_folders_keep_the_panels_path() {
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = std::fs::canonicalize(tmp.path()).unwrap().join("real");
        std::fs::create_dir_all(real_dir.join("sub")).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();
        // The shell reports physical paths; the panel keeps showing the link.
        assert_eq!(as_seen_from(&real_dir.join("sub"), Some(&link)), link.join("sub"));
        assert_eq!(as_seen_from(&real_dir, Some(&link)), link);
        assert_eq!(as_seen_from(&real_dir, Some(&link.join("sub"))), link);
        // A parent of the link, reported physically, keeps the panel's spelling too.
        let parent = std::fs::canonicalize(tmp.path()).unwrap();
        assert_eq!(as_seen_from(&parent, Some(&link)), tmp.path());
        let elsewhere = parent.join("other");
        std::fs::create_dir(&elsewhere).unwrap();
        assert_eq!(as_seen_from(&elsewhere, Some(&link)), elsewhere);
        assert_eq!(as_seen_from(&elsewhere, None), elsewhere);
        assert!(same_dir(&link, &real_dir) && !same_dir(&link, &elsewhere));
    }
}
