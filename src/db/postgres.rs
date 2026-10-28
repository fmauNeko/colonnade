//! PostgreSQL backend.
// allow: SIZE_OK — Task 10 explicitly confines the driver and its adjacent unit/live integration tests to this file.

use super::{
    backend::{DbBackend, QueryConnection},
    model::{
        Cell, ColumnInfo, DbError, Dialect, ObjectKind, ObjectNode, ObjectPath, ObjectRef, Page,
        PageRequest, ServerParams, StatementResult, TableKind, TlsMode, is_numeric_type,
    },
    sql::{qualified_name, select_page_sql},
};
use futures::{FutureExt, TryStreamExt, future::BoxFuture};
use sqlx::{
    AssertSqlSafe, Column, Connection, Either, PgConnection, PgPool, Row, TypeInfo, ValueRef,
    postgres::{PgConnectOptions, PgPoolOptions, PgRow, PgSslMode},
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Mutex;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

fn ssl_mode(mode: TlsMode) -> PgSslMode {
    match mode {
        TlsMode::Disable => PgSslMode::Disable,
        TlsMode::Prefer => PgSslMode::Prefer,
        TlsMode::Require => PgSslMode::Require,
        TlsMode::VerifyCa => PgSslMode::VerifyCa,
        TlsMode::VerifyFull => PgSslMode::VerifyFull,
    }
}

fn map_error(error: sqlx::Error) -> DbError {
    match error {
        sqlx::Error::Database(error) => {
            let code = error.code().map(std::borrow::Cow::into_owned);
            match code.as_deref() {
                Some("57014") => DbError::Cancelled,
                Some("28P01" | "28000") => DbError::Auth(error.message().into()),
                _ => DbError::Query {
                    message: error.message().into(),
                    code,
                },
            }
        }
        error @ (sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::Configuration(_)) => DbError::Connect(error.to_string()),
        error => DbError::Query {
            message: error.to_string(),
            code: None,
        },
    }
}

#[derive(Default)]
struct Pools {
    databases: HashMap<String, PgPool>,
    closed: bool,
}
#[derive(Clone)]
struct PostgresBackend {
    options: PgConnectOptions,
    database: String,
    pools: Arc<Mutex<Pools>>,
}
impl PostgresBackend {
    async fn pool(&self, database: Option<&str>) -> Result<PgPool, DbError> {
        let database = database.unwrap_or(&self.database);
        let mut pools = self.pools.lock().await;
        if pools.closed {
            return Err(map_error(sqlx::Error::PoolClosed));
        }
        Ok(pools
            .databases
            .entry(database.into())
            .or_insert_with(|| {
                PgPoolOptions::new()
                    .max_connections(4)
                    .min_connections(0)
                    .acquire_timeout(CONNECT_TIMEOUT)
                    .idle_timeout(Duration::from_secs(300))
                    .connect_lazy_with(self.options.clone().database(database))
            })
            .clone())
    }
}

/// Connects to PostgreSQL and validates the default database credentials.
///
/// # Errors
/// Returns authentication, connection or ten-second validation timeout errors.
pub fn connect(params: ServerParams) -> BoxFuture<'static, Result<Arc<dyn DbBackend>, DbError>> {
    async move {
        let database = params.database.unwrap_or_else(|| "postgres".into());
        let mut options = PgConnectOptions::new()
            .host(&params.host)
            .port(params.port)
            .username(&params.user)
            .database(&database)
            .ssl_mode(ssl_mode(params.tls))
            .application_name("Colonnade");
        if let Some(password) = &params.password {
            options = options.password(password.expose());
        }
        let backend = PostgresBackend {
            options,
            database,
            pools: Arc::default(),
        };
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            let pool = backend.pool(None).await?;
            pool.acquire().await.map_err(map_error).map(drop)
        })
        .await
        .map_err(|_| DbError::Timeout { seconds: 10 })??;
        Ok(Arc::new(backend) as Arc<dyn DbBackend>)
    }
    .boxed()
}

fn column_info(name: String, type_name: String) -> ColumnInfo {
    ColumnInfo {
        numeric: is_numeric_type(&type_name),
        name,
        type_name,
    }
}
fn row_columns(row: &PgRow) -> Vec<ColumnInfo> {
    row.columns()
        .iter()
        .map(|column| column_info(column.name().into(), column.type_info().name().into()))
        .collect()
}
fn row_cells(row: &PgRow) -> Result<Vec<Cell>, DbError> {
    row.columns()
        .iter()
        .enumerate()
        .map(|(index, column)| {
            if row.try_get_raw(index).map_err(map_error)?.is_null() {
                return Ok(Cell::Null);
            }
            let text: String = row.try_get_unchecked(index).map_err(map_error)?;
            if column.type_info().name() == "BYTEA" {
                Ok(Cell::Binary {
                    len: text.len().saturating_sub(2) / 2,
                })
            } else {
                Ok(Cell::text(text))
            }
        })
        .collect()
}

impl DbBackend for PostgresBackend {
    fn dialect(&self) -> Dialect {
        Dialect::Postgres
    }
    fn server_version(&self) -> BoxFuture<'static, Result<String, DbError>> {
        let backend = self.clone();
        async move {
            let version: String = sqlx::query_scalar("SELECT version()")
                .fetch_one(&backend.pool(None).await?)
                .await
                .map_err(map_error)?;
            Ok(version.lines().next().unwrap_or_default().into())
        }
        .boxed()
    }
    fn list_children(
        &self,
        path: ObjectPath,
    ) -> BoxFuture<'static, Result<Vec<ObjectNode>, DbError>> {
        let backend = self.clone();
        async move {
            match path {
                ObjectPath::Root => {
                    let names: Vec<String> = sqlx::query_scalar("SELECT datname FROM pg_catalog.pg_database WHERE datallowconn AND NOT datistemplate ORDER BY datname")
                        .fetch_all(&backend.pool(None).await?).await.map_err(map_error)?;
                    Ok(names.into_iter().map(|name| ObjectNode { kind: ObjectKind::Database,
                        children: Some(ObjectPath::Database { name: name.clone() }), name, object: None }).collect())
                }
                ObjectPath::Database { name: database } => {
                    let names: Vec<String> = sqlx::query_scalar(r"SELECT nspname FROM pg_catalog.pg_namespace WHERE nspname NOT IN ('pg_catalog', 'information_schema') AND nspname NOT LIKE 'pg\_toast%' AND nspname NOT LIKE 'pg\_temp\_%' ORDER BY nspname")
                        .fetch_all(&backend.pool(Some(&database)).await?).await.map_err(map_error)?;
                    Ok(names.into_iter().map(|name| ObjectNode { kind: ObjectKind::Schema,
                        children: Some(ObjectPath::Schema { database: Some(database.clone()), name: name.clone() }), name, object: None }).collect())
                }
                ObjectPath::Schema { database, name: schema } => {
                    let rows = sqlx::query("SELECT c.relname, c.relkind::text FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relkind IN ('r','p','f','v','m') ORDER BY (c.relkind IN ('v','m')), c.relname")
                        .bind(&schema).fetch_all(&backend.pool(database.as_deref()).await?).await.map_err(map_error)?;
                    rows.into_iter().map(|row| {
                        let name: String = row.try_get(0).map_err(map_error)?;
                        let relkind: String = row.try_get(1).map_err(map_error)?;
                        let (kind, table_kind) = match relkind.as_str() {
                            "v" | "m" => (ObjectKind::View, TableKind::View),
                            _ => (ObjectKind::Table, TableKind::Table),
                        };
                        Ok(ObjectNode { kind, name: name.clone(), children: None,
                            object: Some(ObjectRef { database: database.clone(), schema: Some(schema.clone()), name, kind: table_kind }) })
                    }).collect()
                }
            }
        }.boxed()
    }
    fn table_columns(
        &self,
        object: ObjectRef,
    ) -> BoxFuture<'static, Result<Vec<ColumnInfo>, DbError>> {
        let backend = self.clone();
        async move {
            let rows = sqlx::query("SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod) FROM pg_catalog.pg_attribute a WHERE a.attrelid = $1::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum")
                .bind(qualified_name(Dialect::Postgres, &object))
                .fetch_all(&backend.pool(object.database.as_deref()).await?).await.map_err(map_error)?;
            rows.into_iter().map(|row| Ok(column_info(row.try_get(0).map_err(map_error)?, row.try_get(1).map_err(map_error)?))).collect()
        }.boxed()
    }
    fn fetch_page(&self, request: PageRequest) -> BoxFuture<'static, Result<Page, DbError>> {
        let backend = self.clone();
        async move {
            let rows = sqlx::raw_sql(AssertSqlSafe(select_page_sql(Dialect::Postgres, &request)))
                .fetch_all(&backend.pool(request.object.database.as_deref()).await?)
                .await
                .map_err(map_error)?;
            let columns = match rows.first() {
                Some(row) => row_columns(row),
                None => backend.table_columns(request.object).await?,
            };
            let limit = usize::try_from(request.limit).unwrap_or(usize::MAX);
            let has_more = rows.len() > limit;
            let rows = rows
                .iter()
                .take(limit)
                .map(row_cells)
                .collect::<Result<_, _>>()?;
            Ok(Page {
                columns,
                rows,
                has_more,
            })
        }
        .boxed()
    }
    fn open_query_connection(
        &self,
        database: Option<String>,
    ) -> BoxFuture<'static, Result<Arc<dyn QueryConnection>, DbError>> {
        let backend = self.clone();
        async move {
            let cancel_pool = backend.pool(None).await?;
            let options = backend
                .options
                .database(database.as_deref().unwrap_or(&backend.database));
            let mut connection =
                tokio::time::timeout(CONNECT_TIMEOUT, PgConnection::connect_with(&options))
                    .await
                    .map_err(|_| DbError::Timeout { seconds: 10 })?
                    .map_err(map_error)?;
            let pid = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut connection)
                .await
                .map_err(map_error)?;
            Ok(Arc::new(PostgresQuery {
                connection: Arc::new(Mutex::new(Some(connection))),
                cancel_pool,
                pid,
            }) as Arc<dyn QueryConnection>)
        }
        .boxed()
    }
    fn close(&self) -> BoxFuture<'static, Result<(), DbError>> {
        let backend = self.clone();
        async move {
            let pools = {
                let mut state = backend.pools.lock().await;
                state.closed = true;
                std::mem::take(&mut state.databases)
            };
            for pool in pools.into_values() {
                pool.close().await;
            }
            Ok(())
        }
        .boxed()
    }
}

#[derive(Clone)]
struct PostgresQuery {
    connection: Arc<Mutex<Option<PgConnection>>>,
    cancel_pool: PgPool,
    pid: i32,
}
impl QueryConnection for PostgresQuery {
    fn execute(
        &self,
        sql: String,
        row_cap: usize,
    ) -> BoxFuture<'static, Result<Vec<StatementResult>, DbError>> {
        let query = self.clone();
        async move {
            let mut guard = query.connection.lock().await;
            let connection = guard
                .as_mut()
                .ok_or_else(|| map_error(sqlx::Error::PoolClosed))?;
            let mut stream = sqlx::raw_sql(AssertSqlSafe(sql)).fetch_many(connection);
            let mut results = Vec::new();
            let mut columns = Vec::new();
            let mut rows = Vec::new();
            let mut truncated = false;
            while let Some(item) = stream.try_next().await.map_err(map_error)? {
                match item {
                    Either::Right(row) => {
                        if columns.is_empty() {
                            columns = row_columns(&row);
                        }
                        if rows.len() < row_cap {
                            rows.push(row_cells(&row)?);
                        } else {
                            truncated = true;
                        }
                    }
                    Either::Left(result) => {
                        // An empty SELECT has no Row metadata in the simple-protocol stream.
                        results.push(if columns.is_empty() {
                            StatementResult::Affected {
                                rows: result.rows_affected(),
                            }
                        } else {
                            StatementResult::Rows {
                                columns: std::mem::take(&mut columns),
                                rows: std::mem::take(&mut rows),
                                truncated,
                            }
                        });
                        truncated = false;
                    }
                }
            }
            Ok(results)
        }
        .boxed()
    }
    fn cancel(&self) -> BoxFuture<'static, Result<(), DbError>> {
        let query = self.clone();
        async move {
            sqlx::query("SELECT pg_cancel_backend($1)")
                .bind(query.pid)
                .execute(&query.cancel_pool)
                .await
                .map_err(map_error)?;
            Ok(())
        }
        .boxed()
    }
    fn close(&self) -> BoxFuture<'static, Result<(), DbError>> {
        let query = self.clone();
        async move {
            if let Some(connection) = query.connection.lock().await.take() {
                connection.close().await.map_err(map_error)?;
            }
            Ok(())
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::model::{Secret, SortDirection, test_server_params};
    use std::{borrow::Cow, error::Error, time::Instant};

    #[derive(Debug, thiserror::Error)]
    #[error("fixture database failure")]
    struct DatabaseFailure(Option<&'static str>);
    impl sqlx::error::DatabaseError for DatabaseFailure {
        fn message(&self) -> &'static str {
            "fixture database failure"
        }
        fn code(&self) -> Option<Cow<'_, str>> {
            self.0.map(Cow::Borrowed)
        }
        fn as_error(&self) -> &(dyn Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    #[test]
    fn sqlstate_57014_maps_to_cancelled() {
        // Given: PostgreSQL's query-cancelled SQLSTATE.
        let error = sqlx::Error::Database(Box::new(DatabaseFailure(Some("57014"))));
        // When: mapping the driver failure.
        let mapped = map_error(error);
        // Then: callers can distinguish cancellation.
        assert_eq!(mapped, DbError::Cancelled);
    }
    #[test]
    fn authentication_sqlstates_map_to_auth() {
        // Given: both PostgreSQL authentication SQLSTATEs.
        for code in ["28P01", "28000"] {
            // When: mapping the driver failure.
            let mapped = map_error(sqlx::Error::Database(Box::new(DatabaseFailure(Some(code)))));
            // Then: it is an authentication failure.
            assert!(matches!(mapped, DbError::Auth(_)));
        }
    }
    #[test]
    fn database_errors_preserve_message_and_optional_code() {
        // Given: ordinary database errors, including one without a SQLSTATE.
        for code in [Some("42P01"), None] {
            // When: mapping the driver failure.
            let mapped = map_error(sqlx::Error::Database(Box::new(DatabaseFailure(code))));
            // Then: query diagnostics survive.
            assert_eq!(
                mapped,
                DbError::Query {
                    message: "fixture database failure".into(),
                    code: code.map(str::to_owned),
                }
            );
        }
    }
    #[test]
    fn transport_and_pool_errors_map_to_connect() {
        // Given: failures before a query can run.
        let errors = [
            sqlx::Error::Io(std::io::Error::other("offline")),
            sqlx::Error::Tls(std::io::Error::other("tls").into()),
            sqlx::Error::Configuration(std::io::Error::other("config").into()),
            sqlx::Error::PoolTimedOut,
            sqlx::Error::PoolClosed,
        ];
        for error in errors {
            // When: mapping the driver failure.
            let mapped = map_error(error);
            // Then: it is a connection failure.
            assert!(matches!(mapped, DbError::Connect(_)));
        }
    }
    #[test]
    fn tls_modes_preserve_verification_policy() {
        // Given: every user-selectable TLS policy.
        for (mode, expected) in [
            (TlsMode::Disable, PgSslMode::Disable),
            (TlsMode::Prefer, PgSslMode::Prefer),
            (TlsMode::Require, PgSslMode::Require),
            (TlsMode::VerifyCa, PgSslMode::VerifyCa),
            (TlsMode::VerifyFull, PgSslMode::VerifyFull),
        ] {
            // When: translating to driver options.
            let actual = ssl_mode(mode);
            // Then: neither encryption nor verification is weakened.
            assert_eq!(
                std::mem::discriminant(&actual),
                std::mem::discriminant(&expected)
            );
        }
    }

    struct Fixture {
        backend: Arc<dyn DbBackend>,
        query: Arc<dyn QueryConnection>,
        schema: String,
        database: String,
    }
    impl Fixture {
        async fn new() -> Self {
            let params = test_server_params("COLONNADE_TEST_PG_URL");
            let database = params.database.clone().unwrap();
            let backend = connect(params).await.unwrap();
            let query = backend
                .open_query_connection(Some(database.clone()))
                .await
                .unwrap();
            let schema = format!("ct_{}", uuid::Uuid::new_v4().simple());
            query.execute(format!("CREATE SCHEMA {schema}; CREATE TABLE {schema}.items (id integer, label text, data bytea, doc jsonb, at timestamp with time zone); INSERT INTO {schema}.items SELECT n, CASE WHEN n = 1 THEN NULL ELSE '' END, decode('0001feff', 'hex'), '{{\"n\":1}}'::jsonb, '2026-01-01 UTC'::timestamptz FROM generate_series(1,450) n; CREATE TABLE {schema}.zebra(x int); CREATE VIEW {schema}.a_view AS SELECT * FROM {schema}.items; CREATE VIEW {schema}.z_view AS SELECT * FROM {schema}.items"), 0).await.unwrap();
            Self {
                backend,
                query,
                schema,
                database,
            }
        }
        fn request(&self) -> PageRequest {
            PageRequest {
                object: ObjectRef {
                    database: Some(self.database.clone()),
                    schema: Some(self.schema.clone()),
                    name: "items".into(),
                    kind: TableKind::Table,
                },
                order_by: Some(("id".into(), SortDirection::Ascending)),
                offset: 0,
                limit: 200,
            }
        }
        async fn finish(self) {
            self.query
                .execute(format!("DROP SCHEMA {} CASCADE", self.schema), 0)
                .await
                .unwrap();
            self.query.close().await.unwrap();
            self.backend.close().await.unwrap();
        }
    }

    #[ignore = "requires COLONNADE_TEST_PG_URL and a live PostgreSQL server"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connect_and_server_version() {
        // Given: an isolated schema on the live server.
        let f = Fixture::new().await;
        // When: asking for the connected server's version.
        let result = f.backend.server_version().await;
        f.finish().await;
        // Then: the server identifies itself as PostgreSQL.
        assert!(result.unwrap().contains("PostgreSQL"));
    }
    #[ignore = "requires COLONNADE_TEST_PG_URL and a live PostgreSQL server"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn databases_include_colonnade() {
        // Given: an isolated live fixture.
        let f = Fixture::new().await;
        // When: browsing server databases.
        let result = f.backend.list_children(ObjectPath::Root).await;
        f.finish().await;
        // Then: the application database is navigable.
        assert!(result.unwrap().iter().any(|n| n.name == "colonnade"
            && n.kind == ObjectKind::Database
            && n.children
                == Some(ObjectPath::Database {
                    name: "colonnade".into()
                })));
    }
    #[ignore = "requires COLONNADE_TEST_PG_URL and a live PostgreSQL server"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn schemas_include_fixture() {
        // Given: an isolated live fixture.
        let f = Fixture::new().await;
        let expected = f.schema.clone();
        // When: browsing database schemas.
        let result = f
            .backend
            .list_children(ObjectPath::Database {
                name: f.database.clone(),
            })
            .await;
        f.finish().await;
        // Then: the newly created schema is visible.
        assert!(
            result
                .unwrap()
                .iter()
                .any(|n| n.name == expected && n.kind == ObjectKind::Schema)
        );
    }
    #[ignore = "requires COLONNADE_TEST_PG_URL and a live PostgreSQL server"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tables_precede_views_alphabetically() {
        // Given: tables and views with interleaved names.
        let f = Fixture::new().await;
        // When: browsing the fixture schema.
        let result = f
            .backend
            .list_children(ObjectPath::Schema {
                database: Some(f.database.clone()),
                name: f.schema.clone(),
            })
            .await;
        f.finish().await;
        // Then: tables precede views, with alphabetical order in each group.
        let nodes = result.unwrap();
        assert_eq!(
            nodes
                .iter()
                .map(|n| (n.name.as_str(), n.kind))
                .collect::<Vec<_>>(),
            vec![
                ("items", ObjectKind::Table),
                ("zebra", ObjectKind::Table),
                ("a_view", ObjectKind::View),
                ("z_view", ObjectKind::View)
            ]
        );
        assert!(
            nodes
                .iter()
                .all(|n| n.object.is_some() && n.children.is_none())
        );
    }
    #[ignore = "requires COLONNADE_TEST_PG_URL and a live PostgreSQL server"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn column_types_use_postgres_display_names() {
        // Given: integer, text, binary, JSON and timestamp columns.
        let f = Fixture::new().await;
        // When: loading table metadata.
        let result = f.backend.table_columns(f.request().object).await;
        f.finish().await;
        // Then: PostgreSQL's formatted type names survive.
        assert_eq!(
            result
                .unwrap()
                .iter()
                .map(|c| c.type_name.as_str())
                .collect::<Vec<_>>(),
            [
                "integer",
                "text",
                "bytea",
                "jsonb",
                "timestamp with time zone"
            ]
        );
    }
    #[ignore = "requires COLONNADE_TEST_PG_URL and a live PostgreSQL server"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn paging_covers_450_rows() {
        // Given: 450 rows.
        let f = Fixture::new().await;
        // When: fetching the three pages.
        let mut pages = Vec::new();
        for offset in [0, 200, 400] {
            pages.push(
                f.backend
                    .fetch_page(PageRequest {
                        offset,
                        ..f.request()
                    })
                    .await,
            );
        }
        f.finish().await;
        // Then: the extra row detects only nonterminal pages.
        assert_eq!(
            pages
                .into_iter()
                .map(|p| {
                    let p = p.unwrap();
                    (p.rows.len(), p.has_more)
                })
                .collect::<Vec<_>>(),
            [(200, true), (200, true), (50, false)]
        );
    }
    #[ignore = "requires COLONNADE_TEST_PG_URL and a live PostgreSQL server"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn descending_order_is_honored() {
        // Given: ascending fixture IDs.
        let f = Fixture::new().await;
        // When: requesting descending order.
        let result = f
            .backend
            .fetch_page(PageRequest {
                order_by: Some(("id".into(), SortDirection::Descending)),
                ..f.request()
            })
            .await;
        f.finish().await;
        // Then: sorting is numeric and server-side.
        assert_eq!(result.unwrap().rows[0][0], Cell::text("450".into()));
    }
    #[ignore = "requires COLONNADE_TEST_PG_URL and a live PostgreSQL server"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn null_and_empty_text_are_distinct() {
        // Given: NULL and empty fixture labels.
        let f = Fixture::new().await;
        // When: decoding a page.
        let result = f.backend.fetch_page(f.request()).await;
        f.finish().await;
        // Then: NULL is not rendered as empty text.
        let page = result.unwrap();
        assert_eq!(page.rows[0][1], Cell::Null);
        assert_eq!(page.rows[1][1], Cell::text(String::new()));
    }
    #[ignore = "requires COLONNADE_TEST_PG_URL and a live PostgreSQL server"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bytea_has_decoded_binary_length() {
        // Given: four bytes including zero and non-ASCII values.
        let f = Fixture::new().await;
        // When: decoding a simple-protocol page.
        let result = f.backend.fetch_page(f.request()).await;
        f.finish().await;
        // Then: the length is bytes, not the hex text length.
        assert_eq!(result.unwrap().rows[0][2], Cell::Binary { len: 4 });
    }
    #[ignore = "requires COLONNADE_TEST_PG_URL and a live PostgreSQL server"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn numeric_columns_are_flagged() {
        // Given: numeric and nonnumeric columns.
        let f = Fixture::new().await;
        // When: fetching their metadata through a row.
        let result = f.backend.fetch_page(f.request()).await;
        f.finish().await;
        // Then: only the integer column is numeric.
        assert_eq!(
            result
                .unwrap()
                .columns
                .iter()
                .map(|c| c.numeric)
                .collect::<Vec<_>>(),
            [true, false, false, false, false]
        );
    }
    #[ignore = "requires COLONNADE_TEST_PG_URL and a live PostgreSQL server"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_page_retains_columns() {
        // Given: a request beyond the final row.
        let f = Fixture::new().await;
        // When: fetching the empty page.
        let result = f
            .backend
            .fetch_page(PageRequest {
                offset: 450,
                ..f.request()
            })
            .await;
        f.finish().await;
        // Then: metadata is still available.
        let page = result.unwrap();
        assert_eq!(page.rows, Vec::<Vec<Cell>>::new());
        assert_eq!(page.columns[0].type_name, "integer");
        assert!(!page.has_more);
    }
    #[ignore = "requires COLONNADE_TEST_PG_URL and a live PostgreSQL server"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multi_statement_results_keep_session_state() {
        // Given: a dedicated connection.
        let f = Fixture::new().await;
        // When: running a script then accessing its temporary table again.
        let result = f.query.execute("CREATE TEMP TABLE t(x int); INSERT INTO t VALUES (1),(2); SELECT * FROM t; UPDATE t SET x = 3".into(), 10).await;
        let next = f.query.execute("SELECT * FROM t".into(), 10).await;
        f.finish().await;
        // Then: each statement has one result and the session survives.
        let results = result.unwrap();
        assert_eq!(results[0], StatementResult::Affected { rows: 0 });
        assert_eq!(results[1], StatementResult::Affected { rows: 2 });
        assert!(
            matches!(&results[2], StatementResult::Rows { rows, truncated: false, .. } if rows.len() == 2)
        );
        assert_eq!(results[3], StatementResult::Affected { rows: 2 });
        assert_eq!(results.len(), 4);
        assert!(
            matches!(&next.unwrap()[0], StatementResult::Rows { rows, .. } if rows == &vec![vec![Cell::text("3".into())]; 2])
        );
    }
    #[ignore = "requires COLONNADE_TEST_PG_URL and a live PostgreSQL server"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_select_reports_zero_affected() {
        // Given: a dedicated connection.
        let f = Fixture::new().await;
        // When: executing a SELECT without rows.
        let result = f.query.execute("SELECT 1 WHERE false".into(), 10).await;
        f.finish().await;
        // Then: absent stream metadata is represented as zero affected rows.
        assert_eq!(result.unwrap(), [StatementResult::Affected { rows: 0 }]);
    }
    #[ignore = "requires COLONNADE_TEST_PG_URL and a live PostgreSQL server"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capped_rows_are_drained_per_statement() {
        // Given: a result larger than the requested cap.
        let f = Fixture::new().await;
        // When: streaming it and a following statement.
        let result = f
            .query
            .execute("SELECT generate_series(1, 20000); SELECT 42".into(), 10_000)
            .await;
        f.finish().await;
        // Then: memory is bounded while following results remain available.
        let results = result.unwrap();
        assert!(
            matches!(&results[0], StatementResult::Rows { rows, truncated: true, .. } if rows.len() == 10_000)
        );
        assert!(
            matches!(&results[1], StatementResult::Rows { rows, truncated: false, .. } if rows == &vec![vec![Cell::text("42".into())]])
        );
    }
    #[ignore = "requires COLONNADE_TEST_PG_URL and a live PostgreSQL server"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zero_cap_still_reports_rows_and_drains() {
        // Given: a dedicated connection and a zero row cap.
        let f = Fixture::new().await;
        // When: selecting rows with no storage budget.
        let result = f.query.execute("SELECT 1; SELECT 2".into(), 0).await;
        f.finish().await;
        // Then: both statements remain row results marked truncated.
        let results = result.unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| matches!(r, StatementResult::Rows { columns, rows, truncated: true } if columns.len() == 1 && rows.is_empty())));
    }
    #[ignore = "requires COLONNADE_TEST_PG_URL and a live PostgreSQL server"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_long_query() {
        // Given: an isolated connection running a 30-second query.
        let f = Fixture::new().await;
        let start = Instant::now();
        // When: cancelling after the prescribed 200 ms, bounded by five seconds.
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            let (query, cancel) =
                tokio::join!(f.query.execute("SELECT pg_sleep(30)".into(), 10), async {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    f.query.cancel().await
                });
            cancel.unwrap();
            query
        })
        .await;
        f.finish().await;
        // Then: the server reports cancellation, not a query/connection error.
        assert_eq!(result.unwrap(), Err(DbError::Cancelled));
        assert!(start.elapsed() < Duration::from_secs(5));
    }
    #[ignore = "requires COLONNADE_TEST_PG_URL and a live PostgreSQL server"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bad_password_is_auth() {
        // Given: a working fixture and incorrect credentials.
        let f = Fixture::new().await;
        let mut params = test_server_params("COLONNADE_TEST_PG_URL");
        params.password = Some(Secret::new(uuid::Uuid::new_v4().to_string()));
        // When: validating a new backend connection.
        let result = connect(params).await;
        f.finish().await;
        // Then: authentication is classified independently of transport.
        assert!(matches!(result, Err(DbError::Auth(_))));
    }
    #[ignore = "requires COLONNADE_TEST_PG_URL and a live PostgreSQL server"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_table_preserves_sqlstate() {
        // Given: a schema without the requested table.
        let f = Fixture::new().await;
        // When: querying that missing object.
        let result = f
            .query
            .execute(format!("SELECT * FROM {}.missing", f.schema), 10)
            .await;
        f.finish().await;
        // Then: callers receive PostgreSQL's undefined-table SQLSTATE.
        assert!(matches!(result, Err(DbError::Query { code: Some(code), .. }) if code == "42P01"));
    }
}
