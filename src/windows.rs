//! Workspace window registry and guarded window lifecycle.

use crate::catalog::model::ConnectionId;
use crate::prompts::{self, ConfirmChoice};
use crate::settings::WindowMode;
use crate::workspace::{
    tabs::{GlobalTabStore, TabId, TabStoreEvent, WindowKey},
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
    switching: bool,
    #[cfg(test)]
    counts: Vec<usize>,
}
impl Global for WindowRegistry {}

impl WindowRegistry {
    pub(crate) fn forget(&mut self, id: WindowId) {
        self.main = self.main.filter(|w| w.window_id() != id);
        self.connections.retain(|_, w| w.window_id() != id);
        self.closing.remove(&id);
        #[cfg(test)]
        self.counts
            .push(usize::from(self.main.is_some()) + self.connections.len());
    }
}

/// Installs routing from app-level tab events to workspace windows.
pub fn init(cx: &mut App) {
    cx.set_global(WindowRegistry::default());
    let tabs = cx.global::<GlobalTabStore>().0.clone();
    cx.subscribe(&tabs, |_, event: &TabStoreEvent, cx| {
        let tab = match event {
            TabStoreEvent::Opened {
                tab,
                activate: true,
            }
            | TabStoreEvent::Activated { tab: Some(tab), .. } => Some(*tab),
            TabStoreEvent::Opened {
                activate: false, ..
            }
            | TabStoreEvent::Activated { tab: None, .. }
            | TabStoreEvent::Closed(_)
            | TabStoreEvent::Reordered
            | TabStoreEvent::ModeChanged(_) => None,
        };
        if let Some(tab) = tab {
            cx.defer(move |cx| {
                let store = cx.global::<GlobalTabStore>().0.read(cx);
                let Some(connection) = store.tab(tab).map(|t| t.connection) else {
                    return;
                };
                let result = match store.mode() {
                    WindowMode::Single => open_main_window(cx),
                    WindowMode::PerConnection => ensure_connection_window(connection, cx),
                };
                if let Err(error) = result {
                    crate::app::notify(cx, crate::app::warning(error.to_string()));
                }
            });
        }
    })
    .detach();
}

/// Opens or activates the workspace belonging to a saved connection.
///
/// # Errors
/// Returns an error when the connection is missing or the platform cannot open its window.
pub fn ensure_connection_window(
    id: ConnectionId,
    cx: &mut App,
) -> gpui_kit::Result<AnyWindowHandle> {
    if let Some(handle) = cx.global::<WindowRegistry>().connections.get(&id).copied() {
        handle.update(cx, |_, w, _| w.activate_window())?;
        return Ok(handle);
    }
    let title = cx
        .global::<crate::catalog::store::GlobalConnectionStore>()
        .0
        .read(cx)
        .profile(id)
        .map(|_| crate::workspace::view::window_title(WindowKey::Connection(id), cx))
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "connection was removed")
        })?;
    let mut options = TitleBar::window_options();
    if let Some(titlebar) = options.titlebar.as_mut() {
        titlebar.title = Some(title.into());
    }
    let mut bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
    for _ in 0..cx.global::<WindowRegistry>().connections.len() {
        bounds.origin.x += px(28.);
        bounds.origin.y += px(28.);
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(640.), px(480.))),
        app_id: Some(crate::app::APP_ID.to_owned()),
        ..options
    };
    let (handle, _) = gpui_kit::open_window(options, cx, |window, cx| {
        window.on_window_should_close(cx, should_close);
        cx.new(|cx| WorkspaceView::new(WindowKey::Connection(id), window, cx))
    })?;
    cx.global_mut::<WindowRegistry>()
        .connections
        .insert(id, handle);
    Ok(handle)
}

/// Migrates window presentation without closing or recreating tab state.
///
/// # Errors
/// Returns the platform error when a destination window cannot be opened or updated.
pub fn apply_window_mode(mode: WindowMode, cx: &mut App) -> gpui_kit::Result<()> {
    let tabs = cx.global::<GlobalTabStore>().0.clone();
    if tabs.read(cx).mode() == mode {
        return Ok(());
    }
    match mode {
        WindowMode::PerConnection => {
            let active = tabs
                .read(cx)
                .active(WindowKey::Main)
                .and_then(|id| tabs.read(cx).tab(id))
                .map(|t| t.connection);
            open_main_window(cx)?;
            tabs.update(cx, |s, cx| s.set_mode(mode, cx));
            let mut seen = HashSet::new();
            let connections: Vec<_> = tabs
                .read(cx)
                .tabs()
                .iter()
                .map(|t| t.connection)
                .filter(|id| seen.insert(*id))
                .collect();
            for id in connections {
                ensure_connection_window(id, cx)?;
            }
            if let Some(id) = active {
                ensure_connection_window(id, cx)?;
            }
        }
        WindowMode::Single => {
            open_main_window(cx)?;
            tabs.update(cx, |s, cx| s.set_mode(mode, cx));
            let handles: Vec<_> = cx
                .global::<WindowRegistry>()
                .connections
                .values()
                .copied()
                .collect();
            cx.global_mut::<WindowRegistry>().switching = true;
            let result: gpui_kit::Result<()> = handles.into_iter().try_for_each(|handle| {
                handle.update(cx, |_, w, cx| request_window_close(w, cx))?;
                cx.global_mut::<WindowRegistry>().forget(handle.window_id());
                Ok(())
            });
            cx.global_mut::<WindowRegistry>().switching = false;
            result?;
        }
    }
    Ok(())
}

pub(crate) fn connection_removed(id: ConnectionId, cx: &mut App) {
    if let Some(handle) = cx.global::<WindowRegistry>().connections.get(&id).copied() {
        // The catalog command already confirmed deletion and closed this connection's tabs.
        drop(handle.update(cx, |_, w, _| w.remove_window()));
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
    let store = cx.global::<GlobalTabStore>().0.read(cx);
    match key {
        WindowKey::Main => store.visible_tabs(key).iter().map(|t| t.id).collect(),
        WindowKey::Connection(id) => store
            .tabs()
            .iter()
            .filter(|t| t.connection == id)
            .map(|t| t.id)
            .collect(),
    }
}

fn should_close(window: &mut Window, cx: &mut App) -> bool {
    if cx.global::<WindowRegistry>().switching {
        return true;
    }
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
    if cx.global::<WindowRegistry>().switching {
        window.remove_window();
        return;
    }
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
                let pending = cx.global_mut::<WindowRegistry>().closing.remove(&id);
                if confirmed && pending {
                    store.update(cx, |s, cx| s.close(&tabs, cx));
                    window.remove_window();
                }
            }));
        })
        .detach();
}

// allow: SIZE_OK - fixed lifecycle ownership includes adjacent multi-window regression scenarios.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{TestServices, init_for_tests};
    use crate::catalog::{
        model::{ConnectionProfile, Driver, Endpoint, PasswordStorage},
        ops::{DropPosition, NodeRef},
        store::GlobalConnectionStore,
    };
    use crate::db::{
        fake::{FakeEndpoint, FakeServer},
        model::{Dialect, ObjectKind, StatementResult, TlsMode},
    };
    use crate::query_tab::{
        state::{QueryTabs, RunState},
        view::QueryTabView,
    };
    use crate::settings::WindowMode;
    use crate::sidebar::node::{ObjectNodeKey, SidebarNodeKey};
    use crate::workspace::{
        tabs::TabStore,
        view_factory::{self, TabKindTag},
    };
    use gpui_kit::component::input::EditorState;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{Entity, TestAppContext, VisualTestContext};
    use std::{cell::RefCell, rc::Rc, sync::Arc};

    type Editors = Rc<RefCell<HashMap<WindowId, Entity<EditorState>>>>;
    struct Fx {
        _dir: tempfile::TempDir,
        services: TestServices,
        main: AnyWindowHandle,
        editors: Editors,
    }

    fn setup(cx: &mut TestAppContext) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let services = cx.update(|cx| init_for_tests(cx, dir.path()));
        let editors = Editors::default();
        let captured = editors.clone();
        cx.update(|cx| {
            view_factory::register(cx, TabKindTag::Query, move |tab, window, cx| {
                let state = QueryTabs::ensure(cx, tab).unwrap();
                let view = cx.new(|cx| QueryTabView::new(state, window, cx));
                captured.borrow_mut().insert(
                    window.window_handle().window_id(),
                    view.read(cx).editor().clone(),
                );
                view.into()
            });
        });
        let main = cx.update(open_main_window).unwrap();
        Fx {
            _dir: dir,
            services,
            main,
            editors,
        }
    }

    fn add(cx: &mut TestAppContext, name: &str) -> ConnectionId {
        cx.update(|cx| {
            let store = cx.global::<GlobalConnectionStore>().0.clone();
            store
                .update(cx, |s, cx| {
                    s.add_connection(
                        cx,
                        ConnectionProfile::new(
                            name,
                            Driver::Postgres,
                            Endpoint::Server {
                                host: "db.test".into(),
                                port: 5432,
                                user: "ada".into(),
                                database: None,
                                tls: TlsMode::default(),
                            },
                            PasswordStorage::None,
                        ),
                        DropPosition::RootEnd,
                    )
                })
                .unwrap()
        })
    }

    fn tabs<R>(
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut TabStore, &mut gpui_kit::Context<TabStore>) -> R,
    ) -> R {
        cx.update(|cx| {
            let store = cx.global::<GlobalTabStore>().0.clone();
            store.update(cx, f)
        })
    }

    fn switch(cx: &mut TestAppContext, mode: WindowMode) {
        cx.update(|cx| apply_window_mode(mode, cx)).unwrap();
    }

    fn connection_window(cx: &mut TestAppContext, id: ConnectionId) -> AnyWindowHandle {
        cx.update(|cx| cx.global::<WindowRegistry>().connections[&id])
    }

    fn ui<R>(
        cx: &mut TestAppContext,
        handle: AnyWindowHandle,
        f: impl FnOnce(&mut Window, &mut App) -> R,
    ) -> R {
        cx.update_window(handle, |_, w, cx| {
            w.render_frame(cx);
            let value = f(w, cx);
            w.render_frame(cx);
            value
        })
        .unwrap()
    }

    fn type_sql(cx: &mut TestAppContext, fx: &Fx, handle: AnyWindowHandle, sql: &str) {
        ui(cx, handle, |_, _| {});
        let editor = fx.editors.borrow()[&handle.window_id()].clone();
        ui(cx, handle, |w, cx| {
            w.click(("input", editor.entity_id()), cx);
            w.press("secondary-a", cx);
            w.input(sql, cx);
        });
    }

    fn editor_value(cx: &mut TestAppContext, fx: &Fx, handle: AnyWindowHandle) -> String {
        ui(cx, handle, |_, _| {});
        let editor = fx.editors.borrow()[&handle.window_id()].clone();
        ui(cx, handle, |w, _| {
            w.find(("input", editor.entity_id()))
                .value()
                .unwrap()
                .to_owned()
        })
    }

    #[gpui_kit::test]
    fn per_connection_switch_preserves_groups_and_active_tabs(cx: &mut TestAppContext) {
        // Given: interleaved activation history across two connection groups.
        let fx = setup(cx);
        let a = add(cx, "A");
        let b = add(cx, "B");
        let (a1, a2, b1) = tabs(cx, |s, cx| {
            let a1 = s.open_query(a, None, cx);
            let a2 = s.open_query(a, None, cx);
            let b1 = s.open_query(b, None, cx);
            s.activate(a1, cx);
            (a1, a2, b1)
        });
        // When: switching to connection windows.
        switch(cx, WindowMode::PerConnection);
        // Then: the launcher and two independently active groups remain.
        assert_eq!(cx.windows().len(), 3);
        cx.update(|cx| {
            let store = cx.global::<GlobalTabStore>().0.read(cx);
            assert_eq!(store.visible_tabs(WindowKey::Main).len(), 0);
            assert_eq!(
                store
                    .visible_tabs(WindowKey::Connection(a))
                    .iter()
                    .map(|t| t.id)
                    .collect::<Vec<_>>(),
                [a1, a2]
            );
            assert_eq!(
                store
                    .visible_tabs(WindowKey::Connection(b))
                    .iter()
                    .map(|t| t.id)
                    .collect::<Vec<_>>(),
                [b1]
            );
            assert_eq!(store.active(WindowKey::Connection(a)), Some(a1));
            assert_eq!(store.active(WindowKey::Connection(b)), Some(b1));
            assert_eq!(
                cx.active_window(),
                cx.global::<WindowRegistry>().connections.get(&a).copied()
            );
        });
        ui(cx, fx.main, |w, _| {
            assert!(w.find("workspace-empty").visible());
        });
    }

    #[gpui_kit::test]
    fn typed_query_is_rebuilt_in_connection_editor(cx: &mut TestAppContext) {
        // Given: SQL entered into the real main-window editor.
        let fx = setup(cx);
        let a = add(cx, "A");
        tabs(cx, |s, cx| s.open_query(a, None, cx));
        type_sql(cx, &fx, fx.main, "SELECT 42");
        // When: moving presentation to a connection window.
        switch(cx, WindowMode::PerConnection);
        let handle = connection_window(cx, a);
        // Then: the new editor, not just shared state, contains the typed SQL.
        assert_eq!(editor_value(cx, &fx, handle), "SELECT 42");
    }

    #[gpui_kit::test]
    fn connection_edits_and_results_replace_cached_main_view(cx: &mut TestAppContext) {
        // Given: a previously rendered main editor and a fake query server.
        let fx = setup(cx);
        let a = add(cx, "A");
        let tab = tabs(cx, |s, cx| s.open_query(a, None, cx));
        let server = Arc::new(FakeServer::sample(Dialect::Postgres));
        server.script_query(
            "SELECT 77".into(),
            Ok(vec![StatementResult::Affected { rows: 77 }]),
        );
        fx.services
            .factory
            .register(FakeEndpoint::Host("db.test".into()), server.clone());
        type_sql(cx, &fx, fx.main, "SELECT 1");
        switch(cx, WindowMode::PerConnection);
        let handle = connection_window(cx, a);
        type_sql(cx, &fx, handle, "SELECT 77");
        ui(cx, handle, |w, cx| w.click("query-run", cx));
        cx.run_until_parked();
        // When: switching back after running SQL in the connection window.
        switch(cx, WindowMode::Single);
        // Then: the main editor and results reflect that latest run.
        assert_eq!(editor_value(cx, &fx, fx.main), "SELECT 77");
        assert_eq!(server.executed_sql(), ["SELECT 77"]);
        ui(cx, fx.main, |w, _| {
            assert!(w.find("query-affected").visible());
        });
        cx.update(|cx| assert!(matches!(QueryTabs::state(cx, tab).unwrap().read(cx).run_state(), RunState::Done { results, .. } if matches!(results.as_slice(), [StatementResult::Affected { rows: 77 }]))));
    }

    #[gpui_kit::test]
    fn running_query_survives_connection_window_removal(cx: &mut TestAppContext) {
        // Given: a held query executing in a connection window.
        let fx = setup(cx);
        let a = add(cx, "A");
        let tab = tabs(cx, |s, cx| s.open_query(a, None, cx));
        let server = Arc::new(FakeServer::sample(Dialect::Postgres));
        let gate = server.hold_queries();
        fx.services
            .factory
            .register(FakeEndpoint::Host("db.test".into()), server.clone());
        switch(cx, WindowMode::PerConnection);
        let handle = connection_window(cx, a);
        type_sql(cx, &fx, handle, "SELECT 9");
        ui(cx, handle, |w, cx| w.click("query-run", cx));
        cx.run_until_parked();
        assert_eq!(gate.pending(), 1);
        let state = cx.update(|cx| QueryTabs::state(cx, tab).unwrap());
        // When: switching back while execution is held.
        switch(cx, WindowMode::Single);
        // Then: the same state remains running and completes when released.
        cx.update(|cx| assert!(state.read(cx).is_running()));
        gate.release_all();
        cx.run_until_parked();
        cx.update(|cx| assert!(matches!(state.read(cx).run_state(), RunState::Done { .. })));
        assert_eq!(editor_value(cx, &fx, fx.main), "SELECT 9");
        assert_eq!(fx.services.prompts.confirm_requests().len(), 0);
    }

    #[gpui_kit::test]
    fn single_switch_preserves_order_without_confirmation(cx: &mut TestAppContext) {
        // Given: dirty tabs distributed over two windows.
        let fx = setup(cx);
        let a = add(cx, "A");
        let b = add(cx, "B");
        tabs(cx, |s, cx| {
            s.open_query(a, None, cx);
            s.open_query(b, None, cx);
            s.open_query(a, None, cx);
        });
        let before = tabs(cx, |s, _| s.tabs().to_vec());
        switch(cx, WindowMode::PerConnection);
        let handle = connection_window(cx, a);
        type_sql(cx, &fx, handle, "SELECT 123");
        // When: consolidating into the main window.
        switch(cx, WindowMode::Single);
        cx.run_until_parked();
        // Then: no close guard is consulted and all original records survive in order.
        assert_eq!(cx.windows().len(), 1);
        assert_eq!(tabs(cx, |s, _| s.tabs().to_vec()), before);
        assert_eq!(fx.services.prompts.confirm_requests().len(), 0);
    }

    #[gpui_kit::test]
    fn switching_reopens_launcher_before_removing_last_connection_window(cx: &mut TestAppContext) {
        // Given: the launcher was closed, leaving only a connection window.
        let fx = setup(cx);
        let a = add(cx, "A");
        tabs(cx, |s, cx| s.open_query(a, None, cx));
        switch(cx, WindowMode::PerConnection);
        ui(cx, fx.main, request_window_close);
        cx.run_until_parked();
        assert_eq!(cx.windows().len(), 1);
        // When: switching back with no launcher available.
        switch(cx, WindowMode::Single);
        // Then: the registry never visits zero during migration.
        cx.update(|cx| {
            let registry = cx.global::<WindowRegistry>();
            assert!(registry.main.is_some());
            assert_eq!(registry.connections.len(), 0);
            assert!(
                registry.counts.iter().all(|count| *count > 0),
                "{:?}",
                registry.counts
            );
        });
        assert_eq!(cx.windows().len(), 1);
    }

    #[gpui_kit::test]
    fn opening_table_routes_to_new_connection_window(cx: &mut TestAppContext) {
        // Given: a per-connection launcher and a third connection without tabs.
        let fx = setup(cx);
        let id = add(cx, "C");
        fx.services.factory.register(
            FakeEndpoint::Host("db.test".into()),
            Arc::new(FakeServer::sample(Dialect::Postgres)),
        );
        switch(cx, WindowMode::PerConnection);
        let keys = [
            SidebarNodeKey::Connection(id),
            SidebarNodeKey::Object {
                connection: id,
                node: ObjectNodeKey {
                    kind: ObjectKind::Database,
                    database: None,
                    schema: None,
                    name: "app".into(),
                },
            },
            SidebarNodeKey::Object {
                connection: id,
                node: ObjectNodeKey {
                    kind: ObjectKind::Schema,
                    database: Some("app".into()),
                    schema: None,
                    name: "public".into(),
                },
            },
        ];
        for key in keys {
            ui(cx, fx.main, |w, cx| {
                w.click(format!("sidebar-toggle:{key}"), cx);
            });
            cx.run_until_parked();
        }
        let users = SidebarNodeKey::Object {
            connection: id,
            node: ObjectNodeKey {
                kind: ObjectKind::Table,
                database: Some("app".into()),
                schema: Some("public".into()),
                name: "users".into(),
            },
        };
        // When: opening a table from the launcher's real sidebar.
        ui(cx, fx.main, |w, cx| {
            w.double_click(format!("sidebar-row:{users}"), cx);
        });
        // Then: C's window exists and is active.
        let handle = connection_window(cx, id);
        cx.update(|cx| assert_eq!(cx.active_window(), Some(handle)));
        assert_eq!(cx.windows().len(), 2);
    }

    fn dirty_connection(
        cx: &mut TestAppContext,
        fx: &Fx,
    ) -> (ConnectionId, TabId, AnyWindowHandle) {
        let id = add(cx, "A");
        let tab = tabs(cx, |s, cx| s.open_query(id, None, cx));
        switch(cx, WindowMode::PerConnection);
        let handle = connection_window(cx, id);
        type_sql(cx, fx, handle, "SELECT 1");
        (id, tab, handle)
    }

    #[gpui_kit::test]
    fn cancel_close_keeps_connection_window_and_tabs(cx: &mut TestAppContext) {
        // Given: dirty SQL and a cancelled confirmation.
        let fx = setup(cx);
        let (id, tab, handle) = dirty_connection(cx, &fx);
        fx.services.prompts.queue_confirm(ConfirmChoice::Cancel);
        // When: the platform requests a close.
        assert!(!VisualTestContext::from_window(handle, cx).simulate_close());
        cx.run_until_parked();
        // Then: both window and tab survive.
        assert_eq!(connection_window(cx, id), handle);
        assert!(tabs(cx, |s, _| s.tab(tab).is_some()));
        assert_eq!(fx.services.prompts.confirm_requests().len(), 1);
    }

    #[gpui_kit::test]
    fn confirm_close_removes_only_connection_window_and_tabs(cx: &mut TestAppContext) {
        // Given: dirty SQL and an accepted confirmation.
        let fx = setup(cx);
        let (id, tab, handle) = dirty_connection(cx, &fx);
        fx.services.prompts.queue_confirm(ConfirmChoice::Confirm);
        // When: requesting the shared client-side close flow.
        ui(cx, handle, request_window_close);
        cx.run_until_parked();
        // Then: the launcher survives but that connection's work is closed.
        cx.update(|cx| assert!(!cx.global::<WindowRegistry>().connections.contains_key(&id)));
        assert!(!tabs(cx, |s, _| s.tab(tab).is_some()));
        assert_eq!(cx.windows().len(), 1);
        assert_eq!(fx.services.prompts.confirm_requests().len(), 1);
    }

    #[gpui_kit::test]
    fn deleting_connection_removes_its_window(cx: &mut TestAppContext) {
        // Given: a connection window and its tab.
        let fx = setup(cx);
        let (id, _, _) = dirty_connection(cx, &fx);
        // When: deletion has been confirmed by the catalog command.
        cx.update(|cx| {
            let store = cx.global::<GlobalConnectionStore>().0.clone();
            store
                .update(cx, |s, cx| s.remove(cx, NodeRef::Connection(id)))
                .unwrap();
        });
        cx.run_until_parked();
        // Then: the window is gone without a second discard prompt.
        cx.update(|cx| assert!(!cx.global::<WindowRegistry>().connections.contains_key(&id)));
        assert_eq!(cx.windows().len(), 1);
        assert_eq!(fx.services.prompts.confirm_requests().len(), 0);
    }

    #[gpui_kit::test]
    fn connection_window_title_tracks_rename(cx: &mut TestAppContext) {
        // Given: a rendered connection window.
        let _fx = setup(cx);
        let id = add(cx, "Original");
        let handle = cx.update(|cx| ensure_connection_window(id, cx)).unwrap();
        ui(cx, handle, |_, cx| {
            assert_eq!(
                crate::workspace::view::window_title(WindowKey::Connection(id), cx),
                "Original — Colonnade"
            );
        });
        // When: renaming the saved connection.
        cx.update(|cx| {
            let store = cx.global::<GlobalConnectionStore>().0.clone();
            let mut profile = store.read(cx).profile(id).unwrap().clone();
            profile.name = "Renamed".into();
            store
                .update(cx, |s, cx| s.update_connection(cx, profile))
                .unwrap();
        });
        // Then: rendering updates the native title, preserving the application suffix.
        ui(cx, handle, |_, cx| {
            assert_eq!(
                crate::workspace::view::window_title(WindowKey::Connection(id), cx),
                "Renamed — Colonnade"
            );
            assert_eq!(
                crate::workspace::view::window_title(WindowKey::Main, cx),
                "Colonnade"
            );
        });
    }

    #[gpui_kit::test]
    fn ensure_reuses_window_and_activation_routes_existing_tabs(cx: &mut TestAppContext) {
        // Given: two connection windows with existing query tabs.
        let _fx = setup(cx);
        let a = add(cx, "A");
        let b = add(cx, "B");
        let tab = tabs(cx, |s, cx| {
            let tab = s.open_query(a, None, cx);
            s.open_query(b, None, cx);
            tab
        });
        switch(cx, WindowMode::PerConnection);
        let handle = connection_window(cx, a);
        // When: activating A's existing tab.
        tabs(cx, |s, cx| s.activate(tab, cx));
        // Then: it reuses and activates the existing window.
        assert_eq!(
            cx.update(|cx| ensure_connection_window(a, cx)).unwrap(),
            handle
        );
        cx.update(|cx| assert_eq!(cx.active_window(), Some(handle)));
        assert_eq!(cx.windows().len(), 3);
    }
}
