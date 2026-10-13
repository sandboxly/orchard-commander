//! The async runtime the SMB client runs on, and the bridge to the
//! synchronous [`FileSystem`](oc_vfs::FileSystem) interface.
//!
//! `smb2` is async (tokio). One small multi-threaded runtime, shared by every
//! SMB connection in the process, drives the sockets; callers on worker
//! threads hand it a future and wait on a channel. That works from any
//! thread, including one that is itself inside some other tokio runtime
//! (where `Runtime::block_on` would panic), and every wait is bounded.

use std::future::Future;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::Duration;

use tokio::runtime::{Handle, Runtime};
use tokio::task::AbortHandle;

fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("oc-smb")
            .enable_all()
            .build()
            .expect("failed to start the SMB I/O runtime")
    })
}

pub(crate) fn handle() -> Handle {
    runtime().handle().clone()
}

/// A future running on the SMB runtime; `wait` collects its result.
pub(crate) struct Pending<T> {
    rx: mpsc::Receiver<T>,
    abort: AbortHandle,
}

impl<T: Send + 'static> Pending<T> {
    /// Start `fut` on the SMB runtime now.
    pub(crate) fn spawn(fut: impl Future<Output = T> + Send + 'static) -> Self {
        let (tx, rx) = mpsc::sync_channel(1);
        let task = handle().spawn(async move {
            let _ = tx.send(fut.await);
        });
        Pending { rx, abort: task.abort_handle() }
    }

    /// Stop it without waiting: the result is no longer wanted. Safe for SMB
    /// requests, whose late answers the library discards.
    pub(crate) fn cancel(self) {
        self.abort.abort();
    }

    /// The result, or None if it took longer than `limit` (the task is then
    /// aborted) or the task died.
    pub(crate) fn wait(self, limit: Duration) -> Option<T> {
        match self.rx.recv_timeout(limit) {
            Ok(v) => Some(v),
            Err(_) => {
                self.abort.abort();
                None
            }
        }
    }
}

/// Run `fut` on the SMB runtime and wait at most `limit` for it.
pub(crate) fn run<T: Send + 'static>(limit: Duration, fut: impl Future<Output = T> + Send + 'static) -> Option<T> {
    Pending::spawn(fut).wait(limit)
}
