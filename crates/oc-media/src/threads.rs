//! Background threads that outlive their owner briefly (they are never joined
//! on drop) and a way to see when they are gone.

use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Counts the threads of one player or analyzer. Stays valid after its owner
/// is dropped, so an app quitting (or a test) can wait for them to finish.
#[derive(Clone, Default)]
pub struct ShutdownWatch(Arc<(Mutex<usize>, Condvar)>);

struct Running(ShutdownWatch);

impl Drop for Running {
    fn drop(&mut self) {
        let (count, cond) = &*self.0.0;
        let mut n = count.lock().unwrap_or_else(|e| e.into_inner());
        *n = n.saturating_sub(1);
        cond.notify_all();
    }
}

impl ShutdownWatch {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Spawn a counted, detached thread.
    pub(crate) fn spawn<F: FnOnce() + Send + 'static>(&self, name: &str, f: F) -> std::io::Result<()> {
        *self.0.0.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        let running = Running(self.clone());
        let spawned = thread::Builder::new().name(name.to_string()).spawn(move || {
            let _running = running; // counts down when the thread ends, also on panic
            f();
        });
        spawned.map(|_| ())
    }

    /// How many of the threads are still running.
    pub fn running(&self) -> usize {
        *self.0.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wait until all threads have ended; false on timeout.
    pub fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let (count, cond) = &*self.0;
        let mut n = count.lock().unwrap_or_else(|e| e.into_inner());
        while *n > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            n = cond.wait_timeout(n, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        true
    }
}

impl std::fmt::Debug for ShutdownWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ShutdownWatch({} running)", self.running())
    }
}
