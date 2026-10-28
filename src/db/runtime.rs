//! Tokio runtime bridge for the database drivers.

use super::model::DbError;
use futures::future::BoxFuture;
use gpui_kit::{App, Global, Task, prelude::*};
use std::io;

pub struct DbRuntime {
    tokio: Option<tokio::runtime::Runtime>,
}
impl Global for DbRuntime {}
impl DbRuntime {
    /// Creates the dedicated two-worker database runtime.
    ///
    /// # Errors
    /// Returns an I/O error if runtime workers cannot be started.
    pub fn tokio() -> io::Result<Self> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("colonnade-db")
            .enable_all()
            .build()
            .map(|tokio| Self { tokio: Some(tokio) })
    }
    #[must_use]
    pub const fn inline() -> Self {
        Self { tokio: None }
    }
    #[must_use]
    pub fn handle(&self) -> Option<&tokio::runtime::Handle> {
        self.tokio.as_ref().map(tokio::runtime::Runtime::handle)
    }
}
/// Schedules database work on Tokio, or on the GPUI executor in tests.
///
/// # Panics
/// Panics if the application has not installed `DbRuntime`.
pub fn spawn_db<T: Send + 'static>(
    cx: &App,
    fut: BoxFuture<'static, Result<T, DbError>>,
) -> Task<Result<T, DbError>> {
    match cx.global::<DbRuntime>().handle() {
        Some(runtime) => {
            let handle = runtime.spawn(fut);
            cx.background_spawn(async move { handle.await.map_err(|_| DbError::Runtime)? })
        }
        None => cx.background_spawn(fut),
    }
}
pub fn shutdown(cx: &mut App) {
    if cx.has_global::<DbRuntime>()
        && let Some(runtime) = cx.remove_global::<DbRuntime>().tokio
    {
        runtime.shutdown_background();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::TestAppContext;
    #[test]
    fn tokio_handle_drives_io() {
        // Given: a real runtime outside headless GPUI.
        let runtime = DbRuntime::tokio().unwrap();
        // When: a task uses the timer driver.
        let task = runtime.handle().unwrap().spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            42
        });
        // Then: it completes on the dedicated runtime.
        assert_eq!(futures::executor::block_on(task).unwrap(), 42);
    }
    #[gpui_kit::test]
    async fn inline_spawn_resolves(cx: &mut TestAppContext) {
        // Given: the inline runtime.
        cx.update(|cx| cx.set_global(DbRuntime::inline()));
        // When: scheduling database work.
        let task = cx.update(|cx| spawn_db(cx, Box::pin(async { Ok(42) })));
        // Then: the headless executor resolves it.
        assert_eq!(task.await, Ok(42));
    }
    #[gpui_kit::test]
    fn shutdown_is_idempotent(cx: &mut TestAppContext) {
        // Given: an installed inline runtime.
        cx.update(|cx| cx.set_global(DbRuntime::inline()));
        // When: shutdown is requested twice.
        cx.update(|cx| {
            shutdown(cx);
            shutdown(cx);
        });
        // Then: the global is absent without a panic.
        cx.update(|cx| assert!(!cx.has_global::<DbRuntime>()));
    }
}
