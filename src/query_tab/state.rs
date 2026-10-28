//! Query tab state: SQL text, run state and results.
//!
//! The state is app-level (a [`QueryTabs`] global keyed by tab), so the SQL text, the last
//! results, the cached query connection and a running query survive window changes: views and
//! their editors are rebuilt from it at will.

// allow: SIZE_OK - the plan assigns the run state machine, its global and its tests to this file.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::prelude::*;
use gpui_kit::{App, Context, Entity, Global, Subscription, Task, Window};

use crate::catalog::model::ConnectionId;
use crate::catalog::store::GlobalConnectionStore;
use crate::db::backend::QueryConnection;
use crate::db::model::{DbError, Dialect, ROW_CAP, StatementResult};
use crate::db::runtime::spawn_db;
use crate::sessions::{GlobalSessions, SessionEvent, SessionStatus};
use crate::workspace::tabs::{
    CloseReason, GlobalTabStore, TabId, TabKind, TabRecord, TabStoreEvent,
};
use crate::workspace::view_factory::TabKindTag;

/// Where a query tab's last run stands.
#[derive(Clone, Debug)]
pub enum RunState {
    Idle,
    Running {
        started: Instant,
        cancel_requested: bool,
    },
    Done {
        results: Arc<Vec<StatementResult>>,
        elapsed: Duration,
    },
    Failed {
        error: DbError,
        elapsed: Duration,
    },
    Cancelled {
        elapsed: Duration,
    },
}

type Connection = Arc<dyn QueryConnection>;

/// One query tab: its text, its own database connection and its last run.
pub struct QueryTabState {
    pub tab: TabId,
    pub connection: ConnectionId,
    pub database: Option<String>,
    pub dialect: Dialect,
    pub text: String,
    connection_handle: Option<Connection>,
    run: RunState,
    run_id: u64,
    task: Option<Task<()>>,
    result_tab: usize,
    _sessions: Subscription,
}

/// Failures that say nothing about the next statement's chances: the connection is gone.
const fn is_connection_level(error: &DbError) -> bool {
    match error {
        DbError::Connect(_) | DbError::Auth(_) | DbError::Timeout { .. } | DbError::Runtime => true,
        DbError::Query { .. } | DbError::Cancelled | DbError::Unsupported(_) => false,
    }
}

impl QueryTabState {
    /// A fresh, idle state for a query tab of a connection speaking `dialect`.
    #[must_use]
    pub fn new(record: &TabRecord, dialect: Dialect, cx: &mut Context<Self>) -> Self {
        let database = match &record.kind {
            TabKind::Query { database, .. } => database.clone(),
            TabKind::Table(_) => None,
        };
        let sessions = cx.global::<GlobalSessions>().0.clone();
        let subscription = cx.subscribe(
            &sessions,
            |this: &mut Self, sessions, event: &SessionEvent, cx| match event {
                SessionEvent::StatusChanged(id) => {
                    let gone = matches!(sessions.read(cx).status(*id), SessionStatus::Disconnected);
                    if *id == this.connection && gone {
                        this.session_disconnected(cx);
                    }
                }
                SessionEvent::ChildrenChanged(..) | SessionEvent::Notice(_) => {}
            },
        );
        Self {
            tab: record.id,
            connection: record.connection,
            database,
            dialect,
            text: String::new(),
            connection_handle: None,
            run: RunState::Idle,
            run_id: 0,
            task: None,
            result_tab: 0,
            _sessions: subscription,
        }
    }

    #[must_use]
    pub const fn run_state(&self) -> &RunState {
        &self.run
    }

    #[must_use]
    pub const fn is_running(&self) -> bool {
        matches!(self.run, RunState::Running { .. })
    }

    /// Whether a query connection is cached for the next run.
    #[must_use]
    pub fn has_connection(&self) -> bool {
        self.connection_handle.is_some()
    }

    /// Index of the statement whose result is shown.
    #[must_use]
    pub const fn result_tab(&self) -> usize {
        self.result_tab
    }

    pub fn select_result(&mut self, index: usize, cx: &mut Context<Self>) {
        self.result_tab = index;
        cx.notify();
    }

    fn elapsed(&self) -> Duration {
        match self.run {
            RunState::Running { started, .. } => started.elapsed(),
            RunState::Idle
            | RunState::Done { .. }
            | RunState::Failed { .. }
            | RunState::Cancelled { .. } => Duration::ZERO,
        }
    }

    /// Executes `sql` on the tab's own connection, connecting first when needed.
    ///
    /// Ignored while a run is in progress or when `sql` is blank.
    pub fn run(&mut self, sql: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_running() || sql.trim().is_empty() {
            return;
        }
        self.run_id += 1;
        let run_id = self.run_id;
        self.run = RunState::Running {
            started: Instant::now(),
            cancel_requested: false,
        };
        self.result_tab = 0;
        cx.notify();
        let acquire = if let Some(handle) = self.connection_handle.clone() {
            Task::ready(Ok(handle))
        } else {
            let sessions = cx.global::<GlobalSessions>().0.clone();
            let (connection, database) = (self.connection, self.database.clone());
            sessions.update(cx, |sessions, cx| {
                sessions.open_query_connection(connection, database, window, cx)
            })
        };
        let sql = sql.to_owned();
        let task = cx.spawn(async move |this, cx| {
            let acquired = acquire.await;
            let orphan = acquired.clone().ok();
            let next = this.update(cx, |this, cx| this.acquired(run_id, acquired, cx));
            let connection = match next {
                Ok(Some(connection)) => connection,
                Ok(None) => return,
                Err(_) => {
                    if let Some(connection) = orphan {
                        cx.update(|cx| spawn_db(cx, connection.close()).detach());
                    }
                    return;
                }
            };
            let result = cx
                .update(|cx| spawn_db(cx, connection.execute(sql, ROW_CAP)))
                .await;
            drop(this.update(cx, |this, cx| this.finished(run_id, result, cx)));
        });
        if let Some(previous) = self.task.replace(task) {
            // A replaced run finishes on its own; its completion is ignored by id.
            previous.detach();
        }
    }

    /// The connection arrived (or failed): returns it when the statement should now execute.
    fn acquired(
        &mut self,
        run_id: u64,
        result: Result<Connection, DbError>,
        cx: &mut Context<Self>,
    ) -> Option<Connection> {
        if run_id != self.run_id {
            if let Ok(connection) = result {
                let cached = self
                    .connection_handle
                    .as_ref()
                    .is_some_and(|handle| Arc::ptr_eq(handle, &connection));
                if !cached {
                    spawn_db(cx, connection.close()).detach();
                }
            }
            return None;
        }
        match result {
            Ok(connection) => {
                self.connection_handle = Some(connection.clone());
                if matches!(
                    self.run,
                    RunState::Running {
                        cancel_requested: true,
                        ..
                    }
                ) {
                    self.run = RunState::Cancelled {
                        elapsed: self.elapsed(),
                    };
                    cx.notify();
                    None
                } else {
                    Some(connection)
                }
            }
            Err(error) => {
                self.end(Err(error), cx);
                None
            }
        }
    }

    fn finished(
        &mut self,
        run_id: u64,
        result: Result<Vec<StatementResult>, DbError>,
        cx: &mut Context<Self>,
    ) {
        if run_id == self.run_id {
            self.end(result, cx);
        }
    }

    fn end(&mut self, result: Result<Vec<StatementResult>, DbError>, cx: &mut Context<Self>) {
        let elapsed = self.elapsed();
        self.run = match result {
            Ok(results) => RunState::Done {
                results: Arc::new(results),
                elapsed,
            },
            Err(DbError::Cancelled) => RunState::Cancelled { elapsed },
            Err(error) => {
                if is_connection_level(&error) {
                    self.drop_connection(cx);
                }
                RunState::Failed { error, elapsed }
            }
        };
        cx.notify();
    }

    /// Requests cancellation of the running query; ignored when nothing runs.
    ///
    /// Before the connection exists, the run ends as cancelled once it arrives, without executing.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        let RunState::Running {
            cancel_requested, ..
        } = &mut self.run
        else {
            return;
        };
        *cancel_requested = true;
        if let Some(connection) = &self.connection_handle {
            spawn_db(cx, connection.cancel()).detach();
        }
        cx.notify();
    }

    fn drop_connection(&mut self, cx: &mut Context<Self>) {
        if let Some(connection) = self.connection_handle.take() {
            spawn_db(cx, connection.close()).detach();
        }
    }

    /// The session is gone (disconnect, endpoint edit or removal): the run and handle with it.
    fn session_disconnected(&mut self, cx: &mut Context<Self>) {
        if self.is_running() {
            let elapsed = self.elapsed();
            self.stop(cx);
            self.run = RunState::Cancelled { elapsed };
            self.run_id += 1;
            cx.notify();
        }
        self.drop_connection(cx);
    }

    /// The tab closed: cancel a running query and release the connection.
    fn shutdown(&mut self, cx: &mut Context<Self>) {
        if self.is_running() {
            self.stop(cx);
            self.run_id += 1;
        }
        self.drop_connection(cx);
    }
}

/// App-level registry of query tab states, so they outlive windows and views.
#[derive(Default)]
pub struct QueryTabs {
    states: HashMap<TabId, Entity<QueryTabState>>,
}

impl Global for QueryTabs {}

impl QueryTabs {
    /// The state of a query tab, when it exists.
    #[must_use]
    pub fn state(cx: &App, tab: TabId) -> Option<Entity<QueryTabState>> {
        cx.try_global::<Self>()?.states.get(&tab).cloned()
    }

    /// The state of a query tab, created from the tab store and catalog when missing.
    pub fn ensure(cx: &mut App, tab: TabId) -> Option<Entity<QueryTabState>> {
        if let Some(state) = Self::state(cx, tab) {
            return Some(state);
        }
        let record = cx.global::<GlobalTabStore>().0.read(cx).tab(tab)?.clone();
        if !matches!(record.kind, TabKind::Query { .. }) {
            return None;
        }
        let dialect = cx
            .global::<GlobalConnectionStore>()
            .0
            .read(cx)
            .profile(record.connection)?
            .driver
            .dialect();
        let state = cx.new(|cx| QueryTabState::new(&record, dialect, cx));
        cx.global_mut::<Self>().states.insert(tab, state.clone());
        Some(state)
    }
}

/// Installs the [`QueryTabs`] global, the query close guard and the tab lifecycle handling.
///
/// Needs the tab store, the connection store and the sessions to be initialized.
pub fn install(cx: &mut App) {
    cx.set_global(QueryTabs::default());
    let store = cx.global::<GlobalTabStore>().0.clone();
    store.update(cx, |store, _| {
        store.register_guard(TabKindTag::Query, |tab, cx| {
            let Some(state) = QueryTabs::state(cx, tab) else {
                return Vec::new();
            };
            let state = state.read(cx);
            let mut reasons = Vec::new();
            if !state.text.trim().is_empty() {
                reasons.push(CloseReason::UnsavedQueryText);
            }
            if state.is_running() {
                reasons.push(CloseReason::RunningQuery);
            }
            reasons
        });
    });
    cx.subscribe(&store, |_, event: &TabStoreEvent, cx| match event {
        TabStoreEvent::Opened { tab, .. } => {
            QueryTabs::ensure(cx, *tab);
        }
        TabStoreEvent::Closed(tab) => {
            if let Some(state) = cx.global_mut::<QueryTabs>().states.remove(tab) {
                state.update(cx, QueryTabState::shutdown);
            }
        }
        TabStoreEvent::Activated { .. }
        | TabStoreEvent::Reordered
        | TabStoreEvent::ModeChanged(_) => {}
    })
    .detach();
}

/// Shared headless fixture for the query tab tests.
#[cfg(test)]
pub(crate) mod harness {
    use std::sync::Arc;

    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Context, Entity, IntoElement, Render, TestAppContext,
        Window, WindowOptions, div,
    };

    use super::{QueryTabState, QueryTabs};
    use crate::catalog::model::{
        ConnectionId, ConnectionProfile, Driver, Endpoint, PasswordStorage,
    };
    use crate::catalog::ops::DropPosition;
    use crate::catalog::store::{self, ConnectionStore, GlobalConnectionStore};
    use crate::db::backend::install_factory;
    use crate::db::fake::{FakeEndpoint, FakeFactory, FakeServer};
    use crate::db::model::TlsMode;
    use crate::db::runtime::DbRuntime;
    use crate::paths::ConfigPaths;
    use crate::prompts;
    use crate::secrets::{self, MemorySecretStore};
    use crate::settings::WindowMode;
    use crate::workspace::tabs::{self, GlobalTabStore, TabId, TabStore};
    use crate::workspace::view_factory;

    pub(crate) struct Fx {
        pub(crate) _dir: tempfile::TempDir,
        pub(crate) connection: ConnectionId,
        pub(crate) store: Entity<ConnectionStore>,
        pub(crate) tabs: Entity<TabStore>,
        pub(crate) factory: Arc<FakeFactory>,
        pub(crate) server: Arc<FakeServer>,
    }

    /// Everything a query tab needs, over a fake server of the driver's dialect.
    pub(crate) fn setup(cx: &mut TestAppContext, driver: Driver) -> Fx {
        let dir = tempfile::tempdir().expect("tempdir");
        let factory = Arc::new(FakeFactory::new());
        let server = Arc::new(FakeServer::sample(driver.dialect()));
        factory.register(FakeEndpoint::Host("db.test".into()), server.clone());
        let profile = ConnectionProfile::new(
            "Test",
            driver,
            Endpoint::Server {
                host: "db.test".into(),
                port: 5432,
                user: "u".into(),
                database: None,
                tls: TlsMode::default(),
            },
            PasswordStorage::None,
        );
        let connection = profile.id;
        let (store, tabs) = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
            crate::sql_dialects::register_dialects().expect("dialects");
            cx.set_global(DbRuntime::inline());
            install_factory(cx, factory.clone());
            secrets::install(cx, Arc::new(MemorySecretStore::default()));
            prompts::install_scripted(cx);
            store::init(cx, &ConfigPaths::at(dir.path().to_owned()));
            let store = cx.global::<GlobalConnectionStore>().0.clone();
            store
                .update(cx, |store, cx| {
                    store.add_connection(cx, profile, DropPosition::RootEnd)
                })
                .expect("added");
            tabs::init(cx, WindowMode::Single);
            view_factory::init(cx);
            crate::sessions::init(cx);
            crate::query_tab::view::init(cx);
            (store, cx.global::<GlobalTabStore>().0.clone())
        });
        Fx {
            _dir: dir,
            connection,
            store,
            tabs,
            factory,
            server,
        }
    }

    struct Blank;

    impl Render for Blank {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// A window with nothing in it, to run states that need a window.
    pub(crate) fn open_blank_window(cx: &mut TestAppContext) -> AnyWindowHandle {
        cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| cx.new(|_| Blank))
        })
        .expect("window opens")
        .0
    }

    impl Fx {
        /// Opens a query tab on the connection and returns it with its state.
        pub(crate) fn open_tab(&self, cx: &mut TestAppContext) -> (TabId, Entity<QueryTabState>) {
            let tab = cx.update(|cx| {
                self.tabs
                    .update(cx, |tabs, cx| tabs.open_query(self.connection, None, cx))
            });
            let state = cx.update(|cx| QueryTabs::state(cx, tab).expect("state created on open"));
            (tab, state)
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

    use super::harness::{Fx, open_blank_window, setup};
    use super::{QueryTabState, QueryTabs, RunState};
    use crate::catalog::model::Driver;
    use crate::db::model::{DbError, ROW_CAP, StatementResult};
    use crate::sessions::GlobalSessions;
    use crate::workspace::tabs::CloseReason;

    fn run(
        cx: &mut TestAppContext,
        window: AnyWindowHandle,
        state: &Entity<QueryTabState>,
        sql: &str,
    ) {
        cx.update_window(window, |_, window, cx| {
            state.update(cx, |state, cx| state.run(sql, window, cx));
        })
        .expect("window");
    }

    fn is_running(cx: &mut TestAppContext, state: &Entity<QueryTabState>) -> bool {
        cx.update(|cx| matches!(state.read(cx).run_state(), RunState::Running { .. }))
    }

    fn is_cancelled(cx: &mut TestAppContext, state: &Entity<QueryTabState>) -> bool {
        cx.update(|cx| matches!(state.read(cx).run_state(), RunState::Cancelled { .. }))
    }

    fn is_done(cx: &mut TestAppContext, state: &Entity<QueryTabState>) -> bool {
        cx.update(|cx| matches!(state.read(cx).run_state(), RunState::Done { .. }))
    }

    fn has_connection(cx: &mut TestAppContext, state: &Entity<QueryTabState>) -> bool {
        cx.update(|cx| state.read(cx).has_connection())
    }

    #[gpui_kit::test]
    fn stop_during_connect_ends_cancelled_without_executing(cx: &mut TestAppContext) {
        // Given: a run waiting for its connection.
        let fx = setup(cx, Driver::Postgres);
        let (_, state) = fx.open_tab(cx);
        let connects = fx.factory.hold_connects();
        let window = open_blank_window(cx);
        run(cx, window, &state, "SELECT 1");
        cx.run_until_parked();
        assert!(is_running(cx, &state));
        // When: stopping before the connection arrives, then letting it arrive.
        cx.update(|cx| state.update(cx, QueryTabState::stop));
        connects.release_all();
        cx.run_until_parked();
        // Then: the run ends cancelled and no SQL reached the server.
        assert!(is_cancelled(cx, &state));
        assert_eq!(fx.server.executed_sql().len(), 0);
    }

    #[gpui_kit::test]
    fn completion_of_a_replaced_run_is_ignored(cx: &mut TestAppContext) {
        // Given: a held query that an endpoint edit cancels.
        let fx = setup(cx, Driver::Postgres);
        let (_, state) = fx.open_tab(cx);
        let queries = fx.server.hold_queries();
        let window = open_blank_window(cx);
        run(cx, window, &state, "SELECT 1");
        cx.run_until_parked();
        assert!(is_running(cx, &state));
        cx.update(|cx| {
            fx.store.update(cx, |store, cx| {
                let mut profile = store.profile(fx.connection).cloned().expect("profile");
                profile.endpoint = crate::catalog::model::Endpoint::Server {
                    host: "db.test".into(),
                    port: 5433,
                    user: "u".into(),
                    database: None,
                    tls: crate::db::model::TlsMode::default(),
                };
                store.update_connection(cx, profile).expect("updated");
            });
        });
        assert!(is_cancelled(cx, &state));
        // When: a new run starts before the first one's completion is delivered.
        run(cx, window, &state, "SELECT 2");
        cx.run_until_parked();
        // Then: the late completion does not touch the new run; it finishes normally.
        assert!(is_running(cx, &state));
        queries.release_all();
        cx.run_until_parked();
        assert!(is_done(cx, &state));
        assert_eq!(fx.server.executed_sql(), ["SELECT 1", "SELECT 2"]);
        assert_eq!(fx.factory.connects().len(), 2);
    }

    #[gpui_kit::test]
    fn disconnected_session_drops_the_handle_and_the_next_run_reconnects(cx: &mut TestAppContext) {
        // Given: a finished run that cached its connection.
        let fx = setup(cx, Driver::Postgres);
        let (_, state) = fx.open_tab(cx);
        let window = open_blank_window(cx);
        run(cx, window, &state, "SELECT 1");
        cx.run_until_parked();
        assert!(has_connection(cx, &state));
        assert_eq!(fx.factory.connects().len(), 1);
        // When: the session disconnects, then a new run starts.
        cx.update(|cx| {
            let sessions = cx.global::<GlobalSessions>().0.clone();
            sessions.update(cx, |sessions, cx| sessions.disconnect(fx.connection, cx));
        });
        cx.run_until_parked();
        assert!(!has_connection(cx, &state));
        run(cx, window, &state, "SELECT 2");
        cx.run_until_parked();
        // Then: it connected again and the old connection was closed.
        assert_eq!(fx.factory.connects().len(), 2);
        assert!(is_done(cx, &state));
        assert_eq!(fx.server.lifecycle_counts().1, 1);
    }

    #[gpui_kit::test]
    fn connection_level_error_drops_the_cached_handle(cx: &mut TestAppContext) {
        // Given: a query that fails with a lost connection.
        let fx = setup(cx, Driver::Postgres);
        let (_, state) = fx.open_tab(cx);
        fx.server
            .script_query("SELECT 1".into(), Err(DbError::Connect("lost".into())));
        let window = open_blank_window(cx);
        // When: running it, then running another statement.
        run(cx, window, &state, "SELECT 1");
        cx.run_until_parked();
        let dropped = !has_connection(cx, &state);
        run(cx, window, &state, "SELECT 2");
        cx.run_until_parked();
        // Then: the handle was dropped and a fresh query connection served the second run.
        assert!(dropped);
        assert_eq!(fx.server.query_databases().len(), 2);
    }

    #[gpui_kit::test]
    fn statement_error_keeps_the_cached_handle(cx: &mut TestAppContext) {
        // Given: a query that fails inside the database.
        let fx = setup(cx, Driver::Postgres);
        let (_, state) = fx.open_tab(cx);
        let error = DbError::Query {
            message: "syntax".into(),
            code: None,
        };
        fx.server.script_query("NOT SQL AT ALL".into(), Err(error));
        let window = open_blank_window(cx);
        // When: running it.
        run(cx, window, &state, "NOT SQL AT ALL");
        cx.run_until_parked();
        // Then: the run failed and the connection stays usable.
        assert!(cx.update(|cx| matches!(state.read(cx).run_state(), RunState::Failed { .. })));
        assert!(has_connection(cx, &state));
    }

    #[gpui_kit::test]
    fn blank_sql_and_a_second_run_while_running_are_ignored(cx: &mut TestAppContext) {
        // Given: an idle tab and a held server.
        let fx = setup(cx, Driver::Postgres);
        let (_, state) = fx.open_tab(cx);
        let queries = fx.server.hold_queries();
        let window = open_blank_window(cx);
        // When: running blank SQL.
        run(cx, window, &state, " \n\t ");
        cx.run_until_parked();
        // Then: nothing starts.
        assert!(!is_running(cx, &state));
        assert_eq!(fx.factory.connects().len(), 0);
        // When: running twice in a row.
        run(cx, window, &state, "SELECT 1");
        run(cx, window, &state, "SELECT 2");
        cx.run_until_parked();
        queries.release_all();
        cx.run_until_parked();
        // Then: only the first statement ever ran.
        assert_eq!(fx.server.executed_sql(), ["SELECT 1"]);
    }

    #[gpui_kit::test]
    fn executions_use_the_row_cap(cx: &mut TestAppContext) {
        // Given: a tab with a finished run.
        let fx = setup(cx, Driver::Postgres);
        let (_, state) = fx.open_tab(cx);
        fx.server.script_query(
            "SELECT 1".into(),
            Ok(vec![StatementResult::Affected { rows: 1 }]),
        );
        let window = open_blank_window(cx);
        // When: running a statement.
        run(cx, window, &state, "SELECT 1");
        cx.run_until_parked();
        // Then: the server was asked for at most ROW_CAP rows.
        assert_eq!(fx.server.executed_queries()[0].row_cap, ROW_CAP);
    }

    #[gpui_kit::test]
    fn close_guard_reports_unsaved_text_and_a_running_query(cx: &mut TestAppContext) {
        // Given: an empty query tab.
        let fx = setup(cx, Driver::Postgres);
        let (tab, state) = fx.open_tab(cx);
        let reasons =
            |cx: &mut TestAppContext| cx.update(|cx| fx.tabs.read(cx).close_reasons(&[tab], cx));
        assert_eq!(reasons(cx), []);
        // When: only whitespace is typed, then real SQL.
        cx.update(|cx| state.update(cx, |state, _| state.text = "  \n".into()));
        assert_eq!(reasons(cx), []);
        cx.update(|cx| state.update(cx, |state, _| state.text = "SELECT 1".into()));
        // Then: the text is a reason; a running query adds another.
        assert_eq!(reasons(cx), [(tab, CloseReason::UnsavedQueryText)]);
        let _queries = fx.server.hold_queries();
        let window = open_blank_window(cx);
        run(cx, window, &state, "SELECT 1");
        cx.run_until_parked();
        assert_eq!(
            reasons(cx),
            [
                (tab, CloseReason::UnsavedQueryText),
                (tab, CloseReason::RunningQuery)
            ]
        );
    }

    #[gpui_kit::test]
    fn closing_the_tab_cancels_the_run_and_drops_its_state(cx: &mut TestAppContext) {
        // Given: a running query.
        let fx: Fx = setup(cx, Driver::Postgres);
        let (tab, state) = fx.open_tab(cx);
        let _queries = fx.server.hold_queries();
        let window = open_blank_window(cx);
        run(cx, window, &state, "SELECT 1");
        cx.run_until_parked();
        assert!(is_running(cx, &state));
        // When: the tab closes.
        cx.update(|cx| fx.tabs.update(cx, |tabs, cx| tabs.close(&[tab], cx)));
        cx.run_until_parked();
        // Then: the state is gone, the query was cancelled and its connection closed.
        assert!(cx.update(|cx| QueryTabs::state(cx, tab).is_none()));
        let (_, closes, cancellations) = fx.server.lifecycle_counts();
        assert_eq!(closes, 1);
        assert!(cancellations >= 1);
    }
}
