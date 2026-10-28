//! Live theme reload from the themes directory.

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt as _;
use gpui_kit::{App, Global, Task};
use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::themes::catalog;

const DEBOUNCE: Duration = Duration::from_millis(250);

/// Errors raised while starting the directory watcher.
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// The OS watcher could not be created or attached to the directory.
    #[error("cannot watch the themes directory: {0}")]
    Notify(#[from] notify::Error),
}

/// Keeps the watcher and its reload task alive.
struct ThemeWatcher {
    _dir: PathBuf,
    _watcher: RecommendedWatcher,
    _task: Task<()>,
}

impl Global for ThemeWatcher {}

/// Watches `dir` (non-recursively) and reloads the catalog on any change.
///
/// Events are debounced by 250 ms; every burst triggers one full rescan and
/// then `on_reload`.
///
/// # Errors
/// Fails when the OS watcher cannot be created or `dir` cannot be watched.
pub fn start(
    cx: &mut App,
    dir: PathBuf,
    on_reload: impl Fn(&mut App) + 'static,
) -> Result<(), WatchError> {
    let (sender, mut receiver) = futures::channel::mpsc::unbounded::<()>();
    let mut watcher = notify::recommended_watcher(move |_event: notify::Result<notify::Event>| {
        // The receiver only closes when the watcher is being dropped.
        sender.unbounded_send(()).ok();
    })?;
    watcher.watch(&dir, RecursiveMode::NonRecursive)?;
    let task = cx.spawn(async move |cx| {
        while receiver.next().await.is_some() {
            cx.background_executor().timer(DEBOUNCE).await;
            while receiver.try_recv().is_ok() {}
            cx.update(|cx| {
                catalog::reload(cx);
                on_reload(cx);
            });
        }
    });
    cx.set_global(ThemeWatcher {
        _dir: dir,
        _watcher: watcher,
        _task: task,
    });
    Ok(())
}
