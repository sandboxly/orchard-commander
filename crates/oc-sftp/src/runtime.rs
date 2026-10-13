//! The async machinery behind the blocking facade: one small tokio runtime
//! shared by all SFTP connections, and [`block_on`] to wait for a future from
//! any thread (also from threads that run another async runtime).

use std::future::Future;
use std::io;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::runtime::Runtime;

pub(crate) fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("oc-sftp")
            .enable_all()
            .build()
            .unwrap_or_else(|e| panic!("cannot start the SFTP runtime: {e}"))
    })
}

/// Run `fut` on the SFTP runtime and wait for it. Network futures passed here
/// must bound their own waiting (timeouts); this only waits for the task.
pub(crate) fn block_on<F>(fut: F) -> io::Result<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let task = runtime().spawn(fut);
    join(task)
}

fn join<T>(task: tokio::task::JoinHandle<T>) -> io::Result<T> {
    match futures::executor::block_on(task) {
        Ok(v) => Ok(v),
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(_) => Err(io::Error::other("SFTP request was cancelled")),
    }
}

/// [`block_on`], giving up after `limit` (`Ok(None)`).
pub(crate) fn block_on_within<F>(limit: Duration, fut: F) -> io::Result<Option<F::Output>>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    block_on(async move { tokio::time::timeout(limit, fut).await.ok() })
}

/// Wait for a task spawned on the SFTP runtime at most `limit` (`Ok(None)`;
/// the task is aborted then).
pub(crate) fn join_within<T: Send + 'static>(
    task: tokio::task::JoinHandle<T>,
    limit: Duration,
) -> io::Result<Option<T>> {
    let abort = task.abort_handle();
    match block_on(async move { tokio::time::timeout(limit, task).await })? {
        Ok(Ok(v)) => Ok(Some(v)),
        Ok(Err(e)) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Ok(Err(_)) => Err(io::Error::other("SFTP request was cancelled")),
        Err(_) => {
            abort.abort();
            Ok(None)
        }
    }
}

/// Start `fut` in the background (closing handles from `Drop`).
pub(crate) fn spawn<F>(fut: F)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    drop(runtime().spawn(fut));
}
