//! Workspace window registry and guarded window lifecycle.

use crate::catalog::model::ConnectionId;
use crate::prompts::{self, ConfirmChoice};
use crate::workspace::{
    tabs::{GlobalTabStore, TabId, WindowKey},
    view::WorkspaceView,
};
use gpui_kit::component::TitleBar;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Global, Window, WindowBounds, WindowId,
    WindowOptions, px, size,
};
use std::collections::{HashMap, HashSet};

/// App-level window handles; query state never lives in this registry.
#[derive(Default)]
pub struct WindowRegistry {
    pub main: Option<AnyWindowHandle>,
    pub connections: HashMap<ConnectionId, AnyWindowHandle>,
    closing: HashSet<WindowId>,
}
impl Global for WindowRegistry {}

impl WindowRegistry {
    pub(crate) fn forget(&mut self, id: WindowId) {
        self.main = self.main.filter(|w| w.window_id() != id);
        self.connections.retain(|_, w| w.window_id() != id);
        self.closing.remove(&id);
    }
}

/// Opens the main workspace, or activates the existing one.
///
/// # Errors
/// Returns the platform error if opening the window fails.
pub fn open_main_window(cx: &mut App) -> gpui_kit::Result<AnyWindowHandle> {
    if let Some(main) = cx.global::<WindowRegistry>().main {
        main.update(cx, |_, w, _| w.activate_window())?;
        return Ok(main);
    }
    let mut options = TitleBar::window_options();
    if let Some(titlebar) = options.titlebar.as_mut() {
        titlebar.title = Some("Colonnade".into());
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1200.), px(800.)),
            cx,
        ))),
        window_min_size: Some(size(px(640.), px(480.))),
        app_id: Some(crate::app::APP_ID.to_owned()),
        ..options
    };
    let (handle, _) = gpui_kit::open_window(options, cx, |window, cx| {
        window.on_window_should_close(cx, should_close);
        cx.new(|cx| WorkspaceView::new(WindowKey::Main, window, cx))
    })?;
    cx.global_mut::<WindowRegistry>().main = Some(handle);
    handle.update(cx, |_, window, cx| {
        crate::app::show_load_notices(window, cx);
    })?;
    Ok(handle)
}

/// The active window, falling back to the main workspace.
#[must_use]
pub fn notice_target(cx: &App) -> Option<AnyWindowHandle> {
    cx.active_window()
        .or_else(|| cx.global::<WindowRegistry>().main)
}

fn visible_ids(window: &Window, cx: &App) -> Vec<TabId> {
    let registry = cx.global::<WindowRegistry>();
    let key = registry
        .connections
        .iter()
        .find(|(_, w)| w.window_id() == window.window_handle().window_id())
        .map_or(WindowKey::Main, |(id, _)| WindowKey::Connection(*id));
    cx.global::<GlobalTabStore>()
        .0
        .read(cx)
        .visible_tabs(key)
        .iter()
        .map(|t| t.id)
        .collect()
}

fn should_close(window: &mut Window, cx: &mut App) -> bool {
    let tabs = visible_ids(window, cx);
    let store = cx.global::<GlobalTabStore>().0.clone();
    if store.read(cx).close_reasons(&tabs, cx).is_empty() {
        store.update(cx, |s, cx| s.close(&tabs, cx));
        true
    } else {
        request_window_close(window, cx);
        false
    }
}

/// Closes a workspace through the guard shared by native and client-side controls.
pub fn request_window_close(window: &mut Window, cx: &mut App) {
    let id = window.window_handle().window_id();
    if cx.global::<WindowRegistry>().closing.contains(&id) {
        return;
    }
    let tabs = visible_ids(window, cx);
    let store = cx.global::<GlobalTabStore>().0.clone();
    let reasons = store.read(cx).close_reasons(&tabs, cx);
    if reasons.is_empty() {
        store.update(cx, |s, cx| s.close(&tabs, cx));
        window.remove_window();
        return;
    }
    cx.global_mut::<WindowRegistry>().closing.insert(id);
    let answer = prompts::confirm(
        window,
        cx,
        crate::app::discard_request("Close window", reasons.len()),
    );
    window
        .spawn(cx, async move |cx| {
            let confirmed = matches!(answer.await, Ok(ConfirmChoice::Confirm));
            // An OS-forced removal may have already destroyed this window.
            drop(cx.update(|window, cx| {
                cx.global_mut::<WindowRegistry>().closing.remove(&id);
                if confirmed {
                    store.update(cx, |s, cx| s.close(&tabs, cx));
                    window.remove_window();
                }
            }));
        })
        .detach();
}
