//! Downloads of online-only local files (Dropbox, iCloud Drive, OneDrive
//! placeholders).
//!
//! The OS downloads a placeholder when it's first read, and the read blocks
//! until it's done — indefinitely if the cloud app is stuck, and a blocked
//! read can't be interrupted. So the read runs on its own thread (one per
//! file, shared by later requests for the same file) and callers only wait for
//! it with a timeout. Never on a shared worker pool, where stuck reads would
//! starve everything else.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use oc_vfs::{Result, VfsError};

/// How often a waiting caller checks `is_cancelled`.
const POLL: Duration = Duration::from_millis(100);

#[derive(Default)]
struct Done {
    finished: Mutex<bool>,
    cond: Condvar,
}

impl Done {
    fn set(&self) {
        *self.finished.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.cond.notify_all();
    }

    /// Wait up to `dur`; true once finished.
    fn wait(&self, dur: Duration) -> bool {
        let guard = self.finished.lock().unwrap_or_else(PoisonError::into_inner);
        let (guard, _) = self.cond.wait_timeout_while(guard, dur, |f| !*f).unwrap_or_else(PoisonError::into_inner);
        *guard
    }
}

#[derive(Default)]
pub(crate) struct CloudFetches {
    running: Arc<Mutex<HashMap<PathBuf, Arc<Done>>>>,
}

impl CloudFetches {
    /// Start (or join) the download of `path` and wait for it at most
    /// `timeout`. FetchTimeout / Cancelled leave the download running, so a
    /// later call picks it up where it is.
    pub(crate) fn fetch(&self, path: &Path, timeout: Duration, is_cancelled: &dyn Fn() -> bool) -> Result<()> {
        let done = {
            let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
            match running.get(path) {
                Some(d) => d.clone(),
                None => {
                    let d = Arc::new(Done::default());
                    self.spawn(path.to_path_buf(), d.clone())?;
                    running.insert(path.to_path_buf(), d.clone());
                    d
                }
            }
        };
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if done.wait(left.min(POLL)) {
                return Ok(());
            }
            if is_cancelled() {
                return Err(VfsError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(VfsError::FetchTimeout(path.display().to_string()));
            }
        }
    }

    fn spawn(&self, path: PathBuf, done: Arc<Done>) -> Result<()> {
        let running = self.running.clone();
        let name = format!("cloud-fetch {}", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default());
        std::thread::Builder::new().name(name).spawn(move || {
            // The OS downloads the whole file before the first byte arrives.
            // Errors are left to the real read (by the previewer), which reports them.
            if let Ok(mut f) = File::open(&path) {
                let _ = f.read(&mut [0u8; 1]);
            }
            running.lock().unwrap_or_else(PoisonError::into_inner).remove(&path);
            done.set();
        })?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> usize {
        self.running.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_file_is_available_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "x").unwrap();
        let fetches = CloudFetches::default();
        fetches.fetch(&f, Duration::from_secs(5), &|| false).unwrap();
        // Missing files "succeed" too: the real read reports the error.
        fetches.fetch(&dir.path().join("missing"), Duration::from_secs(5), &|| false).unwrap();
        assert_eq!(fetches.in_flight(), 0);
    }

    /// A FIFO blocks `open` until a writer shows up — a stand-in for a stuck cloud app.
    #[cfg(unix)]
    #[test]
    fn stuck_read_times_out_or_cancels_then_completes() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("placeholder");
        let c = crate::system::c_path(&fifo).unwrap();
        // SAFETY: c is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let fetches = CloudFetches::default();

        let t0 = Instant::now();
        let err = fetches.fetch(&fifo, Duration::from_millis(150), &|| false).unwrap_err();
        assert!(matches!(err, VfsError::FetchTimeout(ref p) if p == &fifo.display().to_string()), "{err:?}");
        assert!(t0.elapsed() < Duration::from_secs(2));
        assert_eq!(fetches.in_flight(), 1);

        let cancelled = AtomicBool::new(true);
        let err = fetches.fetch(&fifo, Duration::from_secs(30), &|| cancelled.load(Ordering::Relaxed)).unwrap_err();
        assert!(matches!(err, VfsError::Cancelled), "{err:?}");
        // Still the same single download thread.
        assert_eq!(fetches.in_flight(), 1);

        // "The cloud app delivers": a writer unblocks the reader.
        let writer = std::thread::spawn({
            let fifo = fifo.clone();
            move || {
                use std::io::Write;
                let mut w = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
                w.write_all(b"x").unwrap();
            }
        });
        fetches.fetch(&fifo, Duration::from_secs(10), &|| false).unwrap();
        writer.join().unwrap();
        assert_eq!(fetches.in_flight(), 0);
    }
}
