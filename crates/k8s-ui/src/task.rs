//! Spawning work that must not outlive the view that asked for it.
//!
//! gpui drops an `Entity` when its tab closes, and a spawned task that is still
//! running when that happens will try to update a window that is gone. The guard
//! is what makes the task's lifetime the view's lifetime: dropping the guard
//! aborts the task, and the guard is dropped at the end of the `await` however
//! that `await` ends.

use std::future::Future;

use tokio::runtime::Handle;

pub(crate) struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Runs `future` on `handle`, aborting it if the guard is dropped before it finishes.
///
/// A guard rather than a bare `JoinHandle`, because dropping a `JoinHandle`
/// detaches the task and lets it outlive whatever asked for it.
pub(crate) async fn join_abortable<T>(
    handle: &Handle,
    future: impl Future<Output = T> + Send + 'static,
) -> Result<T, tokio::task::JoinError>
where
    T: Send + 'static,
{
    let task = handle.spawn(future);
    let _abort = AbortOnDrop(task.abort_handle());
    task.await
}
