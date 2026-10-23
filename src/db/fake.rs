//! Fake database backend for headless tests.
// allow: SIZE_OK — The task fixes this test-only harness to one file; gates, catalog and assertions form its shared deterministic state machine.

use super::{
    backend::{BackendFactory, DbBackend, QueryConnection},
    model::{
        Cell, ColumnInfo, ConnectParams, DbError, Dialect, ObjectKind, ObjectNode, ObjectPath,
        ObjectRef, Page, PageRequest, SortDirection, StatementResult, TableKind,
    },
};
use futures::{FutureExt, channel::oneshot, future::BoxFuture};
use std::{
    cmp::Ordering,
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
type Signal = oneshot::Sender<Result<(), DbError>>;
#[derive(Default)]
struct GateState {
    held: bool,
    pending: VecDeque<(u64, Signal)>,
}

/// Explicit FIFO completion control. Dropping a handle does not release pending work.
#[derive(Clone, Default)]
pub struct HoldGate(Arc<Mutex<GateState>>);
impl HoldGate {
    fn hold(&self) -> Self {
        lock(&self.0).held = true;
        self.clone()
    }
    fn wait(&self, owner: u64) -> BoxFuture<'static, Result<(), DbError>> {
        let mut state = lock(&self.0);
        if !state.held {
            return futures::future::ready(Ok(())).boxed();
        }
        let (sender, receiver) = oneshot::channel();
        state.pending.push_back((owner, sender));
        async move { receiver.await.map_err(|_| DbError::Runtime)? }.boxed()
    }
    /// Completes the oldest operation, leaving subsequent operations held.
    pub fn release_next(&self) {
        loop {
            let pending = lock(&self.0).pending.pop_front();
            match pending {
                Some((_, sender)) => {
                    if sender.send(Ok(())).is_ok() {
                        break;
                    }
                }
                None => break,
            }
        }
    }
    /// Completes all pending operations and lets future operations run immediately.
    pub fn release_all(&self) {
        let pending = {
            let mut state = lock(&self.0);
            state.held = false;
            std::mem::take(&mut state.pending)
        };
        for (_, sender) in pending {
            drop(sender.send(Ok(())));
        }
    }
    /// Counts pending receivers that have not been dropped by their caller.
    #[must_use]
    pub fn pending(&self) -> usize {
        lock(&self.0)
            .pending
            .iter()
            .filter(|(_, sender)| !sender.is_canceled())
            .count()
    }
    fn cancel(&self, owner: u64) {
        let cancelled = {
            let mut state = lock(&self.0);
            let (cancelled, remaining) = std::mem::take(&mut state.pending)
                .into_iter()
                .partition::<VecDeque<_>, _>(|(id, _)| *id == owner);
            state.pending = remaining;
            cancelled
        };
        for (_, sender) in cancelled {
            drop(sender.send(Err(DbError::Cancelled)));
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FakeEndpoint {
    Host(String),
    Sqlite(PathBuf),
}
impl FakeEndpoint {
    fn from_params(params: &ConnectParams) -> Self {
        match params {
            ConnectParams::Postgres(server) | ConnectParams::MySql(server) => {
                Self::Host(server.host.clone())
            }
            ConnectParams::Sqlite(sqlite) => Self::Sqlite(sqlite.path.clone()),
        }
    }
}
/// Registry keyed by host or exact SQLite path; clones share scripts and recordings.
#[derive(Clone, Default)]
pub struct FakeFactory {
    state: Arc<Mutex<FactoryState>>,
    gate: HoldGate,
}
#[derive(Default)]
struct FactoryState {
    servers: HashMap<FakeEndpoint, Arc<FakeServer>>,
    connects: Vec<ConnectParams>,
    failures: VecDeque<DbError>,
}
impl FakeFactory {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// Registers a shared server under a host or SQLite path.
    pub fn register(&self, endpoint: FakeEndpoint, server: Arc<FakeServer>) {
        lock(&self.state).servers.insert(endpoint, server);
    }
    /// Returns connection attempts, including secrets available through `Secret::expose` only.
    #[must_use]
    pub fn connects(&self) -> Vec<ConnectParams> {
        lock(&self.state).connects.clone()
    }
    /// Fails the next connect; failures are consumed FIFO.
    pub fn fail_connect(&self, error: DbError) {
        lock(&self.state).failures.push_back(error);
    }
    /// Holds subsequent connect attempts until the returned gate is released.
    #[must_use]
    pub fn hold_connects(&self) -> HoldGate {
        self.gate.hold()
    }
}
impl BackendFactory for FakeFactory {
    fn connect(
        &self,
        params: ConnectParams,
    ) -> BoxFuture<'static, Result<Arc<dyn DbBackend>, DbError>> {
        let result = {
            let mut state = lock(&self.state);
            state.connects.push(params.clone());
            match state.failures.pop_front() {
                Some(error) => Err(error),
                None => state
                    .servers
                    .get(&FakeEndpoint::from_params(&params))
                    .cloned()
                    .ok_or_else(|| DbError::Connect("No fake server registered".into()))
                    .and_then(|server| {
                        if server.dialect == params.dialect() {
                            Ok(server as Arc<dyn DbBackend>)
                        } else {
                            Err(DbError::Connect("Fake server dialect mismatch".into()))
                        }
                    }),
            }
        };
        let wait = self.gate.wait(0);
        async move {
            wait.await?;
            result
        }
        .boxed()
    }
}
/// Query invocation context for SQL, database routing and row-cap assertions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutedQuery {
    pub database: Option<String>,
    pub sql: String,
    pub row_cap: usize,
}
#[derive(Default)]
struct ServerState {
    pages: Vec<PageRequest>,
    children: Vec<ObjectPath>,
    queries: Vec<ExecutedQuery>,
    query_databases: Vec<Option<String>>,
    next_connection: u64,
    children_failures: VecDeque<DbError>,
    page_results: VecDeque<Result<Page, DbError>>,
    query_results: HashMap<String, VecDeque<Result<Vec<StatementResult>, DbError>>>,
    backend_closes: usize,
    query_closes: usize,
    cancellations: usize,
}
/// Deterministic catalog and script controller; cloning preserves shared state.
#[derive(Clone)]
pub struct FakeServer {
    dialect: Dialect,
    state: Arc<Mutex<ServerState>>,
    queries: HoldGate,
    pages: HoldGate,
    extra_tables: Arc<Mutex<Vec<String>>>,
    extra_databases: Arc<Mutex<Vec<String>>>,
}
impl FakeServer {
    /// Builds `app` tables/views and `analytics` events sample data.
    #[must_use]
    pub fn sample(dialect: Dialect) -> Self {
        Self {
            dialect,
            state: Arc::default(),
            queries: HoldGate::default(),
            pages: HoldGate::default(),
            extra_tables: Arc::default(),
            extra_databases: Arc::default(),
        }
    }
    /// Makes later catalog lookups list one more table in every schema or database.
    pub fn add_table(&self, name: &str) {
        lock(&self.extra_tables).push(name.to_owned());
    }
    /// Makes later root lookups list one more database.
    pub fn add_database(&self, name: &str) {
        lock(&self.extra_databases).push(name.to_owned());
    }
    /// Records every requested page, including failed and held requests.
    #[must_use]
    pub fn pages(&self) -> Vec<PageRequest> {
        lock(&self.state).pages.clone()
    }
    /// Records every catalog path lookup.
    #[must_use]
    pub fn children_requests(&self) -> Vec<ObjectPath> {
        lock(&self.state).children.clone()
    }
    /// Records exact SQL in invocation order.
    #[must_use]
    pub fn executed_sql(&self) -> Vec<String> {
        lock(&self.state)
            .queries
            .iter()
            .map(|query| query.sql.clone())
            .collect()
    }
    /// Records SQL with routing database and requested cap.
    #[must_use]
    pub fn executed_queries(&self) -> Vec<ExecutedQuery> {
        lock(&self.state).queries.clone()
    }
    /// Records databases used when opening dedicated query connections.
    #[must_use]
    pub fn query_databases(&self) -> Vec<Option<String>> {
        lock(&self.state).query_databases.clone()
    }
    /// Returns backend-close, query-close and cancellation call counts.
    #[must_use]
    pub fn lifecycle_counts(&self) -> (usize, usize, usize) {
        let state = lock(&self.state);
        (
            state.backend_closes,
            state.query_closes,
            state.cancellations,
        )
    }
    /// Holds new queries. Cancellation affects only that query connection's waiters.
    #[must_use]
    pub fn hold_queries(&self) -> HoldGate {
        self.queries.hold()
    }
    /// Holds new page requests; release individual responses FIFO for stale-response tests.
    #[must_use]
    pub fn hold_pages(&self) -> HoldGate {
        self.pages.hold()
    }
    /// Queues one catalog failure, consumed by the next lookup.
    pub fn fail_children(&self, error: DbError) {
        lock(&self.state).children_failures.push_back(error);
    }
    /// Queues one page failure, consumed by the next page request.
    pub fn fail_page(&self, error: DbError) {
        self.script_page(Err(error));
    }
    /// Overrides the next page response; scripts are captured before waiting on gates.
    pub fn script_page(&self, result: Result<Page, DbError>) {
        lock(&self.state).page_results.push_back(result);
    }
    /// Queues an exact-SQL result or error; unscripted SQL affects zero rows.
    pub fn script_query(&self, sql: String, result: Result<Vec<StatementResult>, DbError>) {
        lock(&self.state)
            .query_results
            .entry(sql)
            .or_default()
            .push_back(result);
    }

    fn objects(&self, database: Option<&str>) -> Vec<ObjectNode> {
        let mut names = if database == Some("analytics") {
            vec![("events".to_owned(), TableKind::Table)]
        } else {
            vec![
                ("orders".to_owned(), TableKind::Table),
                ("users".to_owned(), TableKind::Table),
                ("active_users".to_owned(), TableKind::View),
            ]
        };
        names.extend(
            lock(&self.extra_tables)
                .iter()
                .map(|name| (name.clone(), TableKind::Table)),
        );
        names
            .into_iter()
            .map(|(name, kind)| ObjectNode {
                name: name.clone(),
                kind: match kind {
                    TableKind::Table => ObjectKind::Table,
                    TableKind::View => ObjectKind::View,
                },
                children: None,
                object: Some(ObjectRef {
                    database: database.map(str::to_owned),
                    schema: match self.dialect {
                        Dialect::Postgres => Some("public".into()),
                        Dialect::MySql | Dialect::Sqlite => None,
                    },
                    name,
                    kind,
                }),
            })
            .collect()
    }
    fn columns(object: &ObjectRef) -> Result<Vec<ColumnInfo>, DbError> {
        let types = match object.name.as_str() {
            "users" | "active_users" => vec![
                ("id", "integer", true),
                ("name", "text", false),
                ("avatar", "bytea", false),
            ],
            "orders" | "events" => vec![("id", "integer", true)],
            _ => {
                return Err(DbError::Query {
                    message: "Unknown fake object".into(),
                    code: None,
                });
            }
        };
        Ok(types
            .into_iter()
            .map(|(name, type_name, numeric)| ColumnInfo {
                name: name.into(),
                type_name: type_name.into(),
                numeric,
            })
            .collect())
    }
    fn page(request: &PageRequest) -> Result<Page, DbError> {
        let columns = Self::columns(&request.object)?;
        let count: u32 = if request.object.name == "events" {
            1_000
        } else {
            450
        };
        let mut rows: Vec<Vec<Cell>> = (1..=count)
            .map(|id| {
                let mut row = vec![Cell::text(id.to_string())];
                if columns.len() == 3 {
                    row.push(if id % 7 == 0 {
                        Cell::Null
                    } else {
                        Cell::text(if id % 11 == 0 {
                            String::new()
                        } else {
                            format!("User {id:03}")
                        })
                    });
                    row.push(Cell::Binary { len: 4 });
                }
                row
            })
            .collect();
        if let Some((column, direction)) = &request.order_by {
            let index = columns
                .iter()
                .position(|info| info.name == *column)
                .ok_or_else(|| DbError::Query {
                    message: "Unknown sort column".into(),
                    code: None,
                })?;
            rows.sort_by(|left, right| {
                let order = compare_cells(&left[index], &right[index], columns[index].numeric);
                match direction {
                    SortDirection::Ascending => order,
                    SortDirection::Descending => order.reverse(),
                }
            });
        }
        let offset = usize::try_from(request.offset).unwrap_or(usize::MAX);
        let limit = usize::try_from(request.limit).unwrap_or(usize::MAX);
        let has_more = rows.len().saturating_sub(offset) > limit;
        Ok(Page {
            columns,
            rows: rows.into_iter().skip(offset).take(limit).collect(),
            has_more,
        })
    }
}
fn compare_cells(left: &Cell, right: &Cell, numeric: bool) -> Ordering {
    match (left, right) {
        (Cell::Null, Cell::Null) => Ordering::Equal,
        (Cell::Null, Cell::Text { .. } | Cell::Binary { .. })
        | (Cell::Text { .. }, Cell::Binary { .. }) => Ordering::Less,
        (Cell::Text { .. } | Cell::Binary { .. }, Cell::Null)
        | (Cell::Binary { .. }, Cell::Text { .. }) => Ordering::Greater,
        (Cell::Text { value: left, .. }, Cell::Text { value: right, .. }) => {
            if numeric {
                match (left.parse::<f64>(), right.parse::<f64>()) {
                    (Ok(left), Ok(right)) => left.total_cmp(&right),
                    _ => left.cmp(right),
                }
            } else {
                left.cmp(right)
            }
        }
        (Cell::Binary { len: left }, Cell::Binary { len: right }) => left.cmp(right),
    }
}
impl DbBackend for FakeServer {
    fn dialect(&self) -> Dialect {
        self.dialect
    }
    fn server_version(&self) -> BoxFuture<'static, Result<String, DbError>> {
        futures::future::ready(Ok(format!("Fake {:?} 1.0", self.dialect))).boxed()
    }
    fn list_children(
        &self,
        path: ObjectPath,
    ) -> BoxFuture<'static, Result<Vec<ObjectNode>, DbError>> {
        let mut state = lock(&self.state);
        state.children.push(path.clone());
        let result = match state.children_failures.pop_front() {
            Some(error) => Err(error),
            None => match (self.dialect, path) {
                (Dialect::Postgres | Dialect::MySql, ObjectPath::Root) => Ok(["analytics", "app"]
                    .into_iter()
                    .map(str::to_owned)
                    .chain(lock(&self.extra_databases).iter().cloned())
                    .map(|name| ObjectNode {
                        kind: ObjectKind::Database,
                        children: Some(ObjectPath::Database { name: name.clone() }),
                        name,
                        object: None,
                    })
                    .collect()),
                (Dialect::Postgres, ObjectPath::Database { name })
                    if matches!(name.as_str(), "app" | "analytics") =>
                {
                    Ok(vec![ObjectNode {
                        kind: ObjectKind::Schema,
                        name: "public".into(),
                        children: Some(ObjectPath::Schema {
                            database: Some(name),
                            name: "public".into(),
                        }),
                        object: None,
                    }])
                }
                (Dialect::Postgres, ObjectPath::Schema { database, name }) if name == "public" => {
                    Ok(self.objects(database.as_deref()))
                }
                (Dialect::MySql, ObjectPath::Database { name })
                    if matches!(name.as_str(), "app" | "analytics") =>
                {
                    Ok(self.objects(Some(&name)))
                }
                (Dialect::Sqlite, ObjectPath::Root) => Ok(self.objects(None)),
                (Dialect::Postgres, ObjectPath::Database { .. } | ObjectPath::Schema { .. })
                | (Dialect::MySql, ObjectPath::Database { .. } | ObjectPath::Schema { .. })
                | (Dialect::Sqlite, ObjectPath::Database { .. } | ObjectPath::Schema { .. }) => {
                    Err(DbError::Unsupported("Unknown fake catalog path".into()))
                }
            },
        };
        futures::future::ready(result).boxed()
    }
    fn table_columns(
        &self,
        object: ObjectRef,
    ) -> BoxFuture<'static, Result<Vec<ColumnInfo>, DbError>> {
        futures::future::ready(Self::columns(&object)).boxed()
    }
    fn fetch_page(&self, request: PageRequest) -> BoxFuture<'static, Result<Page, DbError>> {
        let result = {
            let mut state = lock(&self.state);
            state.pages.push(request.clone());
            state
                .page_results
                .pop_front()
                .unwrap_or_else(|| Self::page(&request))
        };
        let wait = self.pages.wait(0);
        async move {
            wait.await?;
            result
        }
        .boxed()
    }
    fn open_query_connection(
        &self,
        database: Option<String>,
    ) -> BoxFuture<'static, Result<Arc<dyn QueryConnection>, DbError>> {
        let id = {
            let mut state = lock(&self.state);
            state.query_databases.push(database.clone());
            state.next_connection += 1;
            state.next_connection
        };
        let connection: Arc<dyn QueryConnection> = Arc::new(FakeQueryConnection {
            server: self.clone(),
            database,
            id,
        });
        futures::future::ready(Ok(connection)).boxed()
    }
    fn close(&self) -> BoxFuture<'static, Result<(), DbError>> {
        lock(&self.state).backend_closes += 1;
        futures::future::ready(Ok(())).boxed()
    }
}
struct FakeQueryConnection {
    server: FakeServer,
    database: Option<String>,
    id: u64,
}
impl QueryConnection for FakeQueryConnection {
    fn execute(
        &self,
        sql: String,
        row_cap: usize,
    ) -> BoxFuture<'static, Result<Vec<StatementResult>, DbError>> {
        let result = {
            let mut state = lock(&self.server.state);
            state.queries.push(ExecutedQuery {
                database: self.database.clone(),
                sql: sql.clone(),
                row_cap,
            });
            state
                .query_results
                .get_mut(&sql)
                .and_then(VecDeque::pop_front)
                .unwrap_or_else(|| Ok(vec![StatementResult::Affected { rows: 0 }]))
                .map(|mut results| {
                    for result in &mut results {
                        match result {
                            StatementResult::Rows {
                                rows, truncated, ..
                            } => {
                                *truncated |= rows.len() > row_cap;
                                rows.truncate(row_cap);
                            }
                            StatementResult::Affected { .. } => {}
                        }
                    }
                    results
                })
        };
        let wait = self.server.queries.wait(self.id);
        async move {
            wait.await?;
            result
        }
        .boxed()
    }
    fn cancel(&self) -> BoxFuture<'static, Result<(), DbError>> {
        lock(&self.server.state).cancellations += 1;
        self.server.queries.cancel(self.id);
        futures::future::ready(Ok(())).boxed()
    }
    fn close(&self) -> BoxFuture<'static, Result<(), DbError>> {
        lock(&self.server.state).query_closes += 1;
        self.server.queries.cancel(self.id);
        futures::future::ready(Ok(())).boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{FutureExt, executor::block_on};
    fn request() -> PageRequest {
        PageRequest {
            object: ObjectRef {
                database: Some("app".into()),
                schema: Some("public".into()),
                name: "users".into(),
                kind: TableKind::Table,
            },
            order_by: None,
            offset: 0,
            limit: 200,
        }
    }
    #[test]
    fn connect_gate_records_secret_without_debug_leak() {
        // Given: a registered server and a held factory.
        let factory = FakeFactory::new();
        factory.register(
            FakeEndpoint::Host("local".into()),
            Arc::new(FakeServer::sample(Dialect::Postgres)),
        );
        let gate = factory.hold_connects();
        let params = ConnectParams::Postgres(super::super::model::ServerParams {
            host: "local".into(),
            port: 5432,
            user: "user".into(),
            password: Some(super::super::model::Secret::new("password".into())),
            database: None,
            tls: super::super::model::TlsMode::Prefer,
        });
        let mut connect = factory.connect(params.clone());
        assert!(connect.as_mut().now_or_never().is_none());
        // When: releasing the connect.
        gate.release_next();
        // Then: it uses the registered server and preserves the captured parameters.
        assert_eq!(block_on(connect).unwrap().dialect(), Dialect::Postgres);
        assert_eq!(factory.connects(), vec![params]);
        assert!(!format!("{:?}", factory.connects()).contains("\"password\""));
    }
    #[test]
    fn scripted_connect_failure_precedes_registry_lookup() {
        // Given: no registered server but a scripted authentication error.
        let factory = FakeFactory::new();
        factory.fail_connect(DbError::Auth("rejected".into()));
        // When: connecting.
        let result = block_on(factory.connect(ConnectParams::Sqlite(
            super::super::model::SqliteParams {
                path: "missing.db".into(),
            },
        )));
        // Then: the scripted error is returned and the attempt is recorded.
        assert!(matches!(result, Err(DbError::Auth(message)) if message == "rejected"));
        assert_eq!(factory.connects().len(), 1);
    }
    #[test]
    fn held_pages_capture_scripts_before_release() {
        // Given: two held pages with different scripted responses.
        let server = FakeServer::sample(Dialect::Postgres);
        let gate = server.hold_pages();
        server.fail_page(DbError::Timeout { seconds: 3 });
        let first = server.fetch_page(request());
        let second = server.fetch_page(PageRequest {
            offset: 400,
            ..request()
        });
        // When: releasing both pages.
        gate.release_all();
        // Then: each response stays attached to its original request.
        assert_eq!(block_on(first), Err(DbError::Timeout { seconds: 3 }));
        assert_eq!(block_on(second).unwrap().rows.len(), 50);
    }
    #[test]
    fn cancelled_connection_leaves_other_queries_held() {
        // Given: two independent query connections.
        let server = FakeServer::sample(Dialect::Postgres);
        let gate = server.hold_queries();
        let first = block_on(server.open_query_connection(Some("app".into()))).unwrap();
        let second = block_on(server.open_query_connection(Some("analytics".into()))).unwrap();
        let first_query = first.execute("SELECT 1".into(), 1);
        let mut second_query = second.execute("SELECT 2".into(), 1);
        // When: cancelling only the first connection.
        block_on(first.cancel()).unwrap();
        // Then: only its query is cancelled.
        assert_eq!(block_on(first_query), Err(DbError::Cancelled));
        assert!(second_query.as_mut().now_or_never().is_none());
        assert_eq!(gate.pending(), 1);
        gate.release_all();
        assert!(block_on(second_query).is_ok());
    }
    #[test]
    fn query_scripts_enforce_row_cap_and_record_database() {
        // Given: a script exceeding the requested row cap.
        let server = FakeServer::sample(Dialect::Postgres);
        server.script_query(
            "SELECT id".into(),
            Ok(vec![
                StatementResult::Rows {
                    columns: vec![],
                    rows: vec![vec![Cell::Null]; 3],
                    truncated: false,
                },
                StatementResult::Affected { rows: 7 },
            ]),
        );
        let connection = block_on(server.open_query_connection(Some("analytics".into()))).unwrap();
        // When: executing with a cap of two.
        let results = block_on(connection.execute("SELECT id".into(), 2)).unwrap();
        // Then: row results are truncated without dropping statement outcomes.
        assert_eq!(
            results,
            vec![
                StatementResult::Rows {
                    columns: vec![],
                    rows: vec![vec![Cell::Null]; 2],
                    truncated: true
                },
                StatementResult::Affected { rows: 7 }
            ]
        );
        assert_eq!(
            server.executed_queries()[0].database.as_deref(),
            Some("analytics")
        );
    }
    #[test]
    fn release_next_skips_dropped_operations() {
        // Given: a dropped page followed by a live held page.
        let server = FakeServer::sample(Dialect::Postgres);
        let gate = server.hold_pages();
        drop(server.fetch_page(request()));
        let mut page = server.fetch_page(request());
        // When: releasing the next live operation.
        gate.release_next();
        // Then: the live page completes instead of consuming the release on a dead receiver.
        assert_eq!(
            page.as_mut().now_or_never().unwrap().unwrap().rows.len(),
            200
        );
    }
    #[test]
    fn sample_names_distinguish_null_and_empty() {
        // Given: the users fixture.
        let server = FakeServer::sample(Dialect::Postgres);
        // When: fetching the first page.
        let page = block_on(server.fetch_page(request())).unwrap();
        // Then: every seventh name is NULL, every eleventh is empty, and avatars are binary.
        assert_eq!(page.rows[6][1], Cell::Null);
        assert_eq!(page.rows[10][1], Cell::text(String::new()));
        assert_eq!(page.rows[0][2], Cell::Binary { len: 4 });
    }
    #[test]
    fn analytics_events_have_one_thousand_rows() {
        // Given: an events request.
        let server = FakeServer::sample(Dialect::MySql);
        let request = PageRequest {
            object: ObjectRef {
                database: Some("analytics".into()),
                schema: None,
                name: "events".into(),
                kind: TableKind::Table,
            },
            offset: 900,
            limit: 200,
            order_by: None,
        };
        // When: fetching the final page.
        let page = block_on(server.fetch_page(request)).unwrap();
        // Then: exactly one hundred rows remain.
        assert_eq!((page.rows.len(), page.has_more), (100, false));
    }
    #[test]
    fn postgres_children_follow_database_schema_table() {
        // Given: a PostgreSQL catalog.
        let server = FakeServer::sample(Dialect::Postgres);
        // When: traversing the catalog.
        let root = block_on(server.list_children(ObjectPath::Root)).unwrap();
        let schemas =
            block_on(server.list_children(ObjectPath::Database { name: "app".into() })).unwrap();
        let tables = block_on(server.list_children(ObjectPath::Schema {
            database: Some("app".into()),
            name: "public".into(),
        }))
        .unwrap();
        // Then: nodes carry navigable paths and view identity.
        assert_eq!(root.len(), 2);
        assert_eq!(schemas[0].kind, ObjectKind::Schema);
        assert!(
            tables
                .iter()
                .any(|node| node.name == "active_users" && node.kind == ObjectKind::View)
        );
    }
    #[test]
    fn mysql_children_skip_schemas() {
        // Given: a MySQL catalog.
        let server = FakeServer::sample(Dialect::MySql);
        // When: opening a database.
        let children =
            block_on(server.list_children(ObjectPath::Database { name: "app".into() })).unwrap();
        // Then: tables appear directly.
        assert_eq!(children.len(), 3);
        assert!(children.iter().all(|node| node.object.is_some()));
    }
    #[test]
    fn sqlite_children_are_at_root() {
        // Given: a SQLite catalog.
        let server = FakeServer::sample(Dialect::Sqlite);
        // When: opening the root.
        let children = block_on(server.list_children(ObjectPath::Root)).unwrap();
        // Then: tables and views are immediately available.
        assert_eq!(children.len(), 3);
        assert!(
            children
                .iter()
                .all(|node| node.object.as_ref().unwrap().database.is_none())
        );
    }
    #[test]
    fn pages_cover_all_rows() {
        // Given: 450 sample users.
        let server = FakeServer::sample(Dialect::Postgres);
        for (offset, count, more) in [(0, 200, true), (200, 200, true), (400, 50, false)] {
            // When: fetching one page.
            let page = block_on(server.fetch_page(PageRequest {
                offset,
                ..request()
            }))
            .unwrap();
            // Then: the terminal page is detected.
            assert_eq!((page.rows.len(), page.has_more), (count, more));
        }
        assert_eq!(server.pages().len(), 3);
    }
    #[test]
    fn ordering_is_numeric_for_numeric_columns() {
        // Given: users in ascending insertion order.
        let server = FakeServer::sample(Dialect::Postgres);
        // When: requesting descending IDs.
        let page = block_on(server.fetch_page(PageRequest {
            order_by: Some(("id".into(), SortDirection::Descending)),
            ..request()
        }))
        .unwrap();
        // Then: numeric ordering wins over lexical ordering.
        assert_eq!(page.rows[0][0], Cell::text("450".into()));
    }
    #[test]
    fn held_query_is_released() {
        // Given: a held query.
        let server = FakeServer::sample(Dialect::Postgres);
        let gate = server.hold_queries();
        let connection = block_on(server.open_query_connection(None)).unwrap();
        let mut query = connection.execute("UPDATE users SET name = 'x'".into(), 100);
        assert!(query.as_mut().now_or_never().is_none());
        // When: releasing it.
        gate.release_next();
        // Then: it completes and SQL was recorded.
        assert_eq!(
            block_on(query).unwrap(),
            vec![StatementResult::Affected { rows: 0 }]
        );
        assert_eq!(server.executed_sql(), vec!["UPDATE users SET name = 'x'"]);
    }
    #[test]
    fn held_query_is_cancelled() {
        // Given: a held query.
        let server = FakeServer::sample(Dialect::Postgres);
        let _gate = server.hold_queries();
        let connection = block_on(server.open_query_connection(None)).unwrap();
        let mut query = connection.execute("SELECT 1".into(), 100);
        assert!(query.as_mut().now_or_never().is_none());
        // When: cancelling the query connection.
        block_on(connection.cancel()).unwrap();
        // Then: the pending query resolves as cancelled.
        assert_eq!(block_on(query), Err(DbError::Cancelled));
    }
    #[test]
    fn scripted_failures_are_consumed_once() {
        // Given: a single scripted page failure.
        let server = FakeServer::sample(Dialect::Postgres);
        server.fail_page(DbError::Connect("offline".into()));
        // When: fetching and retrying.
        let failed = block_on(server.fetch_page(request()));
        let retry = block_on(server.fetch_page(request()));
        // Then: the retry uses real sample data.
        assert_eq!(failed, Err(DbError::Connect("offline".into())));
        assert_eq!(retry.unwrap().rows.len(), 200);
    }
}
