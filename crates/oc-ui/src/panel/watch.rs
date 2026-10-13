//! Watching the shown folder (local, watchable file systems) so the panel
//! refreshes when something changes on disk, like `QFileSystemWatcher` in
//! the Python panel. Notifications arrive on notify's thread; at most one
//! wake-up is in flight per watcher, the panel debounces the reload.
//!
//! Starting and stopping a watcher touches the disk (the path is resolved)
//! and, with FSEvents on macOS, starts or joins a thread, so both happen on
//! a worker: the UI thread only records which folder it wants watched.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use notify::{EventKind, RecursiveMode, Watcher};

use crate::app::App;
use crate::tasks::Tasks;

pub struct DirWatch {
    pub path: PathBuf,
    _watcher: notify::RecommendedWatcher,
}

/// Watch the panel's folder if it can be watched (and isn't already).
pub fn update(app: &mut App, side: usize) {
    let p = &mut app.main.sides[side].panel;
    let path = (p.location.fs.capabilities().watchable).then(|| p.location.local_path()).flatten();
    if p.watch_wanted == path {
        return;
    }
    p.watch_wanted = path.clone();
    let old = p.watch.take();
    let tasks = app.tasks.clone();
    app.tasks.run_in_background(
        move |_| {
            drop(old);
            path.and_then(|path| start(path, tasks, side))
        },
        move |app, watch| {
            let p = &mut app.main.sides[side].panel;
            if p.watch_wanted.is_some() && p.watch_wanted.as_ref() == watch.as_ref().map(|w| &w.path) {
                p.watch = watch;
            } else if let Some(stale) = watch {
                // The panel moved on meanwhile: stop it off the UI thread too.
                app.tasks.spawn(move || drop(stale), |_, ()| {});
            }
        },
    );
}

fn start(path: PathBuf, tasks: Tasks, side: usize) -> Option<DirWatch> {
    let pending = Arc::new(AtomicBool::new(false));
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        if !pending.swap(true, Ordering::SeqCst) {
            let pending = pending.clone();
            tasks.post(move |app| {
                pending.store(false, Ordering::SeqCst);
                super::directory_changed(app, side);
            });
        }
    });
    let mut watcher = match watcher {
        Ok(w) => w,
        Err(e) => {
            log::warn!("cannot watch folders: {e}");
            return None;
        }
    };
    if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
        log::debug!("cannot watch {}: {e}", path.display());
        return None;
    }
    Some(DirWatch { path, _watcher: watcher })
}
