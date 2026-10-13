//! Blocking work off the UI thread (port of `ui/tasks.py`).
//!
//! ```text
//! tasks.run_in_background(job, on_done)   job on a worker thread; on_done(app, result) on the UI thread
//! tasks.post(f)                            from any thread: run f(app) on the UI thread
//! tasks.ask_ui(prompt)                     from a worker: let the UI ask the user, block for the answer
//! ```
//!
//! Every job gets its own thread rather than a pool slot: listing a slow
//! server or reading a stuck cloud placeholder must never starve other work
//! (see the cloud gotcha in the release requirements). Results travel to the
//! UI thread through a channel; the platform layer is woken by `wake` (an
//! `EventLoopProxy` in the app, nothing in tests) and calls
//! [`App::process_tasks`](crate::App::process_tasks).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use crate::app::App;

/// Work for the UI thread.
pub type UiJob = Box<dyn FnOnce(&mut App) + Send>;

/// Wakes the UI thread's event loop when a job arrives.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Cooperative cancellation: workers poll [`is_cancelled`](Self::is_cancelled);
/// a cancelled job's `on_done` is not called.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        CancelToken::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// A closure for APIs that take `&dyn Fn() -> bool`.
    pub fn as_fn(&self) -> impl Fn() -> bool + Send + Sync + 'static {
        let t = self.clone();
        move || t.is_cancelled()
    }
}

/// The UI's answer channel for [`Tasks::ask_ui`]. Dropping it unanswered
/// (the dialog was torn down, the app is quitting) makes `ask_ui` return None.
pub struct Reply<T>(SyncSender<T>);

impl<T> Reply<T> {
    pub fn send(self, value: T) {
        let _ = self.0.send(value);
    }
}

struct Inner {
    tx: Mutex<Sender<UiJob>>,
    wake: Waker,
    ui_thread: ThreadId,
    /// Background jobs started but not yet delivered (or dropped).
    pending: AtomicUsize,
}

/// Handle for starting work and talking to the UI thread. Cheap to clone and
/// `Send`: workers keep one to post results.
#[derive(Clone)]
pub struct Tasks {
    inner: Arc<Inner>,
}

/// The receiving end, owned by the [`App`].
pub struct TaskQueue {
    rx: Receiver<UiJob>,
}

impl TaskQueue {
    /// Every job that has arrived, without blocking.
    pub fn drain(&self) -> Vec<UiJob> {
        self.rx.try_iter().collect()
    }
}

/// A connected [`Tasks`] / [`TaskQueue`] pair. Call on the UI thread.
pub fn channel(wake: Waker) -> (Tasks, TaskQueue) {
    let (tx, rx) = mpsc::channel();
    let inner = Inner { tx: Mutex::new(tx), wake, ui_thread: std::thread::current().id(), pending: AtomicUsize::new(0) };
    (Tasks { inner: Arc::new(inner) }, TaskQueue { rx })
}

impl Tasks {
    /// Run `f` on the UI thread soon (after the current event).
    pub fn post(&self, f: impl FnOnce(&mut App) + Send + 'static) {
        let sent = self.inner.tx.lock().unwrap_or_else(|p| p.into_inner()).send(Box::new(f)).is_ok();
        if sent {
            (self.inner.wake)();
        }
    }

    /// Run `job` on a worker thread, then `on_done(app, result)` on the UI
    /// thread — unless the returned token was cancelled meanwhile. A
    /// panicking job is logged and dropped.
    pub fn run_in_background<T: Send + 'static>(
        &self,
        job: impl FnOnce(&CancelToken) -> T + Send + 'static,
        on_done: impl FnOnce(&mut App, T) + Send + 'static,
    ) -> CancelToken {
        let token = CancelToken::new();
        self.run_with_token(token.clone(), job, on_done);
        token
    }

    /// [`run_in_background`](Self::run_in_background) with a token the caller made
    /// (e.g. shared with a progress dialog's Cancel button).
    pub fn run_with_token<T: Send + 'static>(
        &self,
        token: CancelToken,
        job: impl FnOnce(&CancelToken) -> T + Send + 'static,
        on_done: impl FnOnce(&mut App, T) + Send + 'static,
    ) {
        let check = token.clone();
        self.spawn(
            move || {
                let value = job(&token);
                (!token.is_cancelled()).then_some(value)
            },
            move |app, value| {
                if let Some(value) = value.filter(|_| !check.is_cancelled()) {
                    on_done(app, value);
                }
            },
        );
    }

    /// Run `job` on a worker thread and always deliver its result to
    /// `on_done` on the UI thread (cancellation is the job's business). A
    /// panicking job is logged and `on_done` is not called.
    pub fn spawn<T: Send + 'static>(&self, job: impl FnOnce() -> T + Send + 'static, on_done: impl FnOnce(&mut App, T) + Send + 'static) {
        self.inner.pending.fetch_add(1, Ordering::SeqCst);
        let tasks = self.clone();
        let spawned = std::thread::Builder::new().name("oc-task".into()).spawn(move || {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)) {
                Ok(value) => {
                    let done_tasks = tasks.clone();
                    tasks.post(move |app| {
                        done_tasks.inner.pending.fetch_sub(1, Ordering::SeqCst);
                        on_done(app, value);
                    });
                }
                Err(_) => {
                    log::error!("background task panicked");
                    tasks.inner.pending.fetch_sub(1, Ordering::SeqCst);
                    (tasks.inner.wake)();
                }
            }
        });
        if spawned.is_err() {
            self.inner.pending.fetch_sub(1, Ordering::SeqCst);
            log::error!("cannot start a worker thread");
        }
    }

    /// From a worker thread: run `prompt` on the UI thread and wait until it
    /// answers through the [`Reply`] (typically when a dialog closes). None
    /// if the reply was dropped, or if called on the UI thread itself (which
    /// can't block on its own dialogs).
    pub fn ask_ui<T: Send + 'static>(&self, prompt: impl FnOnce(&mut App, Reply<T>) + Send + 'static) -> Option<T> {
        if std::thread::current().id() == self.inner.ui_thread {
            debug_assert!(false, "ask_ui called on the UI thread");
            return None;
        }
        let (tx, rx) = mpsc::sync_channel(1);
        self.post(move |app| prompt(app, Reply(tx)));
        rx.recv().ok()
    }

    /// Background jobs whose results haven't been handled yet.
    pub fn pending(&self) -> usize {
        self.inner.pending.load(Ordering::SeqCst)
    }

    /// Whether the current thread is the UI thread.
    pub fn on_ui_thread(&self) -> bool {
        std::thread::current().id() == self.inner.ui_thread
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_token() {
        let t = CancelToken::new();
        let f = t.as_fn();
        assert!(!f());
        t.clone().cancel();
        assert!(t.is_cancelled() && f());
    }
}
