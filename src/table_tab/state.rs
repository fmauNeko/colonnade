//! Table tab state: columns, loaded rows, paging and server-side sort of one table.

// allow: SIZE_OK - the plan keeps the state, the shared test harness and its tests in this file.
use std::collections::HashMap;

use gpui_kit::{App, AppContext as _, Context, Entity, EventEmitter, Global, Window};

use crate::catalog::model::ConnectionId;
use crate::db::model::{
    Cell, ColumnInfo, DbError, ObjectRef, PAGE_SIZE, Page, PageRequest, SortDirection,
};
use crate::sessions::GlobalSessions;
use crate::workspace::tabs::{GlobalTabStore, TabId, TabKind};

/// What the view must react to besides a plain re-render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableTabEvent {
    /// The column list changed: Kit only reads columns when refreshed.
    ColumnsChanged,
    /// The loaded rows were dropped (sort or refresh): scroll back to the top.
    RowsReset,
}

/// Rows loaded so far of one table or view, loaded one page at a time.
pub struct TableTabState {
    pub tab: TabId,
    pub connection: ConnectionId,
    pub object: ObjectRef,
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Vec<Cell>>,
    pub has_more: bool,
    pub loading: bool,
    /// Index of the sorted column and its direction.
    pub sort: Option<(usize, SortDirection)>,
    pub error: Option<DbError>,
    /// Bumped whenever loaded rows are dropped: older responses are ignored.
    pub generation: u64,
    pub started: bool,
}

impl EventEmitter<TableTabEvent> for TableTabState {}

type Outcome = Result<Page, DbError>;

impl TableTabState {
    #[must_use]
    pub const fn new(tab: TabId, connection: ConnectionId, object: ObjectRef) -> Self {
        Self {
            tab,
            connection,
            object,
            columns: Vec::new(),
            rows: Vec::new(),
            has_more: false,
            loading: false,
            sort: None,
            error: None,
            generation: 0,
            started: false,
        }
    }

    /// Starts the first load (columns, then page 0); later calls do nothing.
    pub fn load_first(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.started {
            self.started = true;
            self.fetch(window, cx);
        }
    }

    /// Loads the next page; does nothing while loading, after a failure or when all rows are in.
    pub fn load_more(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.started && self.has_more && !self.loading && self.error.is_none() {
            self.fetch(window, cx);
        }
    }

    /// Sorts on the server by a column (or clears the sort) and reloads from the first row.
    pub fn set_sort(
        &mut self,
        sort: Option<(usize, SortDirection)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sort = sort;
        self.reload(window, cx);
    }

    /// Drops the loaded rows and loads the first page again, keeping the sort.
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.started = true;
        self.reload(window, cx);
    }

    /// Repeats the load that failed: the first page, or the page after the loaded rows.
    pub fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.error.is_some() && !self.loading {
            self.fetch(window, cx);
        }
    }

    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.generation += 1;
        self.rows.clear();
        self.has_more = false;
        cx.emit(TableTabEvent::RowsReset);
        self.fetch(window, cx);
    }

    fn request(&self) -> PageRequest {
        PageRequest {
            object: self.object.clone(),
            order_by: self.sort.and_then(|(index, direction)| {
                Some((self.columns.get(index)?.name.clone(), direction))
            }),
            offset: self.rows.len() as u64,
            limit: PAGE_SIZE,
        }
    }

    /// Loads the columns when still unknown, then the page after the loaded rows.
    fn fetch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sessions = cx.global::<GlobalSessions>().0.clone();
        let (generation, connection) = (self.generation, self.connection);
        let object = self.columns.is_empty().then(|| self.object.clone());
        let request = self.request();
        self.loading = true;
        self.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let outcome: Outcome = async {
                if let Some(object) = object {
                    let task = cx
                        .update(|window, cx| {
                            sessions.update(cx, |sessions, cx| {
                                sessions.table_columns(connection, object, window, cx)
                            })
                        })
                        .map_err(|_| DbError::Cancelled)?;
                    let columns = task.await?;
                    this.update(cx, |this, cx| this.set_columns(columns, cx))
                        .map_err(|_| DbError::Cancelled)?;
                }
                let request = PageRequest {
                    order_by: this
                        .read_with(cx, |this, _| this.request().order_by)
                        .map_err(|_| DbError::Cancelled)?,
                    ..request
                };
                let task = cx
                    .update(|window, cx| {
                        sessions.update(cx, |sessions, cx| {
                            sessions.fetch_page(connection, request, window, cx)
                        })
                    })
                    .map_err(|_| DbError::Cancelled)?;
                task.await
            }
            .await;
            drop(this.update(cx, |this, cx| this.apply(generation, outcome, cx)));
        })
        .detach();
    }

    /// Columns do not depend on the sort or the rows, so they apply as soon as they arrive.
    fn set_columns(&mut self, columns: Vec<ColumnInfo>, cx: &mut Context<Self>) {
        self.columns = columns;
        cx.emit(TableTabEvent::ColumnsChanged);
        cx.notify();
    }

    fn apply(&mut self, generation: u64, outcome: Outcome, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        self.loading = false;
        match outcome {
            Ok(page) => {
                self.rows.extend(page.rows);
                self.has_more = page.has_more;
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }
}

/// The states of the open table tabs.
#[derive(Default)]
pub struct TableTabs {
    states: HashMap<TabId, Entity<TableTabState>>,
}

impl Global for TableTabs {}

impl TableTabs {
    /// Installs the empty registry unless it is already there.
    pub fn init(cx: &mut App) {
        if !cx.has_global::<Self>() {
            cx.set_global(Self::default());
        }
    }

    /// The state of an open table tab, created from its tab record on first use.
    pub fn ensure(tab: TabId, cx: &mut App) -> Option<Entity<TableTabState>> {
        if let Some(state) = cx.global::<Self>().states.get(&tab) {
            return Some(state.clone());
        }
        let store = cx.global::<GlobalTabStore>().0.clone();
        let record = store.read(cx).tab(tab)?;
        let TabKind::Table(object) = &record.kind else {
            return None;
        };
        let state = TableTabState::new(tab, record.connection, object.clone());
        let state = cx.new(|_| state);
        cx.global_mut::<Self>().states.insert(tab, state.clone());
        Some(state)
    }

    /// The state of a tab, when it has one.
    #[must_use]
    pub fn get(&self, tab: TabId) -> Option<&Entity<TableTabState>> {
        self.states.get(&tab)
    }

    /// Drops the state of a closed tab.
    pub fn remove(tab: TabId, cx: &mut App) {
        cx.global_mut::<Self>().states.remove(&tab);
    }
}

#[cfg(test)]
pub(crate) mod harness {
    use std::sync::Arc;

    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Context, Entity, IntoElement, Render, TestAppContext,
        Window, WindowOptions, div,
    };

    use crate::catalog::model::{
        ConnectionId, ConnectionProfile, Driver, Endpoint, PasswordStorage,
    };
    use crate::catalog::ops::DropPosition;
    use crate::catalog::store::{self, GlobalConnectionStore};
    use crate::db::backend::install_factory;
    use crate::db::fake::{FakeEndpoint, FakeFactory, FakeServer};
    use crate::db::model::{Dialect, ObjectRef, TableKind, TlsMode};
    use crate::db::runtime::DbRuntime;
    use crate::paths::ConfigPaths;
    use crate::secrets::{self, MemorySecretStore};
    use crate::settings::WindowMode;
    use crate::workspace::tabs::{self, GlobalTabStore, TabId, TabStore};
    use crate::workspace::view_factory;

    struct Blank;

    impl Render for Blank {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    pub struct Fx {
        pub _dir: tempfile::TempDir,
        pub connection: ConnectionId,
        pub server: Arc<FakeServer>,
        pub tabs: Entity<TabStore>,
        pub window: AnyWindowHandle,
    }

    pub fn table(name: &str) -> ObjectRef {
        ObjectRef {
            database: Some("app".into()),
            schema: Some("public".into()),
            name: name.into(),
            kind: TableKind::Table,
        }
    }

    /// Every global a table tab needs, over a fake PostgreSQL server and an empty window.
    pub fn setup(cx: &mut TestAppContext) -> Fx {
        let dir = tempfile::tempdir().expect("tempdir");
        let factory = Arc::new(FakeFactory::new());
        let server = Arc::new(FakeServer::sample(Dialect::Postgres));
        factory.register(FakeEndpoint::Host("db.test".into()), server.clone());
        let profile = ConnectionProfile::new(
            "Fake",
            Driver::Postgres,
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
        let tabs = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
            store::init(cx, &ConfigPaths::at(dir.path().to_path_buf()));
            secrets::install(cx, Arc::new(MemorySecretStore::default()));
            cx.set_global(DbRuntime::inline());
            install_factory(cx, factory);
            tabs::init(cx, WindowMode::Single);
            view_factory::init(cx);
            super::TableTabs::init(cx);
            crate::sessions::init(cx);
            crate::prompts::install_scripted(cx);
            let catalog = cx.global::<GlobalConnectionStore>().0.clone();
            catalog.update(cx, |catalog, cx| {
                catalog
                    .add_connection(cx, profile, DropPosition::RootEnd)
                    .expect("added");
            });
            cx.global::<GlobalTabStore>().0.clone()
        });
        let (window, _) = cx
            .update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| cx.new(|_| Blank))
            })
            .expect("window opens");
        Fx {
            _dir: dir,
            connection,
            server,
            tabs,
            window,
        }
    }

    impl Fx {
        pub fn open_table(&self, name: &str, cx: &mut TestAppContext) -> TabId {
            let (connection, object) = (self.connection, table(name));
            cx.update(|cx| {
                self.tabs
                    .update(cx, |tabs, cx| tabs.open_table(connection, object, cx))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, TestAppContext};

    use super::harness::{Fx, setup};
    use super::{TableTabState, TableTabs};
    use crate::db::model::SortDirection;
    use crate::workspace::tabs::TabId;

    fn state_of(cx: &mut TestAppContext, tab: TabId) -> gpui_kit::Entity<TableTabState> {
        cx.update(|cx| TableTabs::ensure(tab, cx)).expect("state")
    }

    fn start(cx: &mut TestAppContext, fx: &Fx, state: &gpui_kit::Entity<TableTabState>) {
        let window = cx.update(|_| fx.window);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            state.update(cx, |state, cx| state.load_first(window, cx));
        })
        .expect("window");
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn stale_page_is_ignored(cx: &mut TestAppContext) {
        // Given: a table whose first page is held on the server.
        let fx = setup(cx);
        let gate = fx.server.hold_pages();
        let tab = fx.open_table("users", cx);
        let state = state_of(cx, tab);
        start(cx, &fx, &state);
        assert_eq!(gate.pending(), 1);
        // When: sorting while it is held, then answering the old request first.
        let window = fx.window;
        cx.update_window(window, |_, window, cx| {
            state.update(cx, |state, cx| {
                state.set_sort(Some((1, SortDirection::Descending)), window, cx);
            });
        })
        .expect("window");
        cx.run_until_parked();
        gate.release_next();
        cx.run_until_parked();
        // Then: its rows are ignored and the table keeps loading.
        let (rows, loading) = cx.update(|cx| {
            let state = state.read(cx);
            (state.rows.len(), state.loading)
        });
        assert_eq!((rows, loading), (0, true));
        // And: the answer to the sorted request fills the grid.
        gate.release_all();
        cx.run_until_parked();
        let (rows, order) = cx.update(|cx| {
            let state = state.read(cx);
            (state.rows.len(), fx.server.pages().last().cloned())
        });
        assert_eq!(rows, 200);
        assert_eq!(
            order.and_then(|request| request.order_by),
            Some(("name".to_owned(), SortDirection::Descending))
        );
    }

    #[gpui_kit::test]
    fn load_more_is_ignored_while_loading_and_when_all_loaded(cx: &mut TestAppContext) {
        // Given: a table with 450 rows and its first page held.
        let fx = setup(cx);
        let gate = fx.server.hold_pages();
        let tab = fx.open_table("users", cx);
        let state = state_of(cx, tab);
        start(cx, &fx, &state);
        let window = fx.window;
        let more = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                state.update(cx, |state, cx| state.load_more(window, cx));
            })
            .expect("window");
            cx.run_until_parked();
        };
        // When: asking for more while the first page is still loading.
        more(cx);
        // Then: no second page is requested.
        assert_eq!(fx.server.pages().len(), 1);
        // When: loading everything, then asking again.
        gate.release_all();
        cx.run_until_parked();
        more(cx);
        more(cx);
        more(cx);
        // Then: pages come at offsets 0, 200 and 400 only.
        let offsets: Vec<u64> = fx.server.pages().iter().map(|r| r.offset).collect();
        assert_eq!(offsets, [0, 200, 400]);
        assert!(!cx.update(|cx| state.read(cx).has_more));
    }
}
