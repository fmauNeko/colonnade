//! MySQL / MariaDB backend.
// allow: SIZE_OK — The task's single-file ownership keeps this driver and its dual-server regression tests together.

use super::{
    backend::{DbBackend, QueryConnection},
    model::{
        Cell, ColumnInfo, DbError, Dialect, ObjectKind, ObjectNode, ObjectPath, ObjectRef, Page,
        PageRequest, ServerParams, StatementResult, TableKind, TlsMode, is_numeric_type,
    },
    sql::select_page_sql,
};
use futures::{FutureExt, TryStreamExt, future::BoxFuture};
use sqlx::{
    AssertSqlSafe, Column, Connection, Either, MySqlConnection, MySqlPool, Row, TypeInfo, ValueRef,
    mysql::{MySqlConnectOptions, MySqlDatabaseError, MySqlPoolOptions, MySqlRow, MySqlSslMode},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

const fn ssl_mode(tls: TlsMode) -> MySqlSslMode {
    match tls {
        TlsMode::Disable => MySqlSslMode::Disabled,
        TlsMode::Prefer => MySqlSslMode::Preferred,
        TlsMode::Require => MySqlSslMode::Required,
        TlsMode::VerifyCa => MySqlSslMode::VerifyCa,
        TlsMode::VerifyFull => MySqlSslMode::VerifyIdentity,
    }
}

/// Explains SHA-2 authentication failures when TLS has been disabled.
#[must_use]
pub fn auth_hint(tls: TlsMode, message: &str) -> Option<&'static str> {
    match tls {
        TlsMode::Disable if message.contains("caching_sha2_password") => {
            Some("Enable TLS (Preferred) or use an account configured with mysql_native_password.")
        }
        TlsMode::Disable
        | TlsMode::Prefer
        | TlsMode::Require
        | TlsMode::VerifyCa
        | TlsMode::VerifyFull => None,
    }
}

fn database_error(number: u16, message: &str, tls: TlsMode) -> DbError {
    match number {
        1045 => DbError::Auth(
            auth_hint(tls, message)
                .map_or_else(|| message.into(), |hint| format!("{message}. {hint}")),
        ),
        1317 => DbError::Cancelled,
        _ => DbError::Query {
            message: message.into(),
            code: Some(number.to_string()),
        },
    }
}

fn map_error(error: sqlx::Error, tls: TlsMode) -> DbError {
    match error {
        sqlx::Error::Database(error) => match error.try_downcast_ref::<MySqlDatabaseError>() {
            Some(mysql) => database_error(mysql.number(), mysql.message(), tls),
            None => DbError::Query {
                message: error.message().into(),
                code: error.code().map(std::borrow::Cow::into_owned),
            },
        },
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::Configuration(_) => DbError::Connect(error.to_string()),
        sqlx::Error::Protocol(message)
            if message.contains("RSA auth backend disabled")
                || message.contains("caching_sha2_password") =>
        {
            database_error(1045, &format!("caching_sha2_password: {message}"), tls)
        }
        other => DbError::Query {
            message: other.to_string(),
            code: None,
        },
    }
}

#[derive(Clone)]
struct MySqlBackend {
    pool: MySqlPool,
    options: MySqlConnectOptions,
    tls: TlsMode,
}

/// Connects to a MySQL or MariaDB server and validates its credentials.
///
/// # Errors
/// Returns authentication, connection, or ten-second timeout errors.
pub fn connect(params: ServerParams) -> BoxFuture<'static, Result<Arc<dyn DbBackend>, DbError>> {
    async move {
        let mut options = MySqlConnectOptions::new()
            .host(&params.host)
            .port(params.port)
            .username(&params.user)
            .charset("utf8mb4")
            .ssl_mode(ssl_mode(params.tls));
        if let Some(password) = &params.password {
            options = options.password(password.expose());
        }
        if let Some(database) = &params.database {
            options = options.database(database);
        }
        let pool = tokio::time::timeout(
            CONNECT_TIMEOUT,
            MySqlPoolOptions::new()
                .max_connections(4)
                .min_connections(0)
                .acquire_timeout(CONNECT_TIMEOUT)
                .connect_with(options.clone()),
        )
        .await
        .map_err(|_| DbError::Timeout { seconds: 10 })?
        .map_err(|error| map_error(error, params.tls))?;
        Ok(Arc::new(MySqlBackend {
            pool,
            options,
            tls: params.tls,
        }) as Arc<dyn DbBackend>)
    }
    .boxed()
}

fn row_columns(row: &MySqlRow) -> Vec<ColumnInfo> {
    row.columns()
        .iter()
        .map(|column| {
            let type_name = column.type_info().name().to_owned();
            ColumnInfo {
                name: column.name().into(),
                numeric: is_numeric_type(&type_name),
                type_name,
            }
        })
        .collect()
}

fn row_cells(row: &MySqlRow) -> Result<Vec<Cell>, sqlx::Error> {
    row.columns()
        .iter()
        .enumerate()
        .map(|(index, column)| {
            if row.try_get_raw(index)?.is_null() {
                return Ok(Cell::Null);
            }
            let bytes = row.try_get_unchecked::<Vec<u8>, _>(index)?;
            Ok(match column.type_info().name() {
                "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" | "BINARY" | "VARBINARY"
                | "GEOMETRY" | "BIT" => Cell::Binary { len: bytes.len() },
                _ => Cell::text(String::from_utf8_lossy(&bytes).into_owned()),
            })
        })
        .collect()
}

impl DbBackend for MySqlBackend {
    fn dialect(&self) -> Dialect {
        Dialect::MySql
    }

    fn server_version(&self) -> BoxFuture<'static, Result<String, DbError>> {
        let this = self.clone();
        async move {
            sqlx::query_scalar("SELECT VERSION()")
                .fetch_one(&this.pool)
                .await
                .map_err(|error| map_error(error, this.tls))
        }
        .boxed()
    }

    fn list_children(
        &self,
        path: ObjectPath,
    ) -> BoxFuture<'static, Result<Vec<ObjectNode>, DbError>> {
        let this = self.clone();
        async move {
            let nodes: Result<Vec<_>, sqlx::Error> = match path {
                ObjectPath::Root => sqlx::query("SELECT schema_name FROM information_schema.schemata ORDER BY schema_name")
                    .fetch_all(&this.pool).await.map_err(|error| map_error(error, this.tls))?
                    .into_iter().map(|row| {
                        let name: String = row.try_get(0)?;
                        Ok(ObjectNode { kind: ObjectKind::Database, children: Some(ObjectPath::Database { name: name.clone() }), name, object: None })
                    }).collect(),
                ObjectPath::Database { name: database } => sqlx::query("SELECT table_name, table_type FROM information_schema.tables WHERE table_schema = ? ORDER BY table_type = 'VIEW', table_name")
                    .bind(&database).fetch_all(&this.pool).await.map_err(|error| map_error(error, this.tls))?
                    .into_iter().map(|row| {
                        let name: String = row.try_get(0)?;
                        let table_type: String = row.try_get(1)?;
                        let (kind, table_kind) = match table_type.as_str() {
                            "VIEW" | "SYSTEM VIEW" => (ObjectKind::View, TableKind::View),
                            _ => (ObjectKind::Table, TableKind::Table),
                        };
                        Ok(ObjectNode { kind, name: name.clone(), children: None,
                            object: Some(ObjectRef { database: Some(database.clone()), schema: None, name, kind: table_kind }) })
                    }).collect(),
                ObjectPath::Schema { .. } => return Err(DbError::Unsupported("MySQL has no separate schema layer".into())),
            };
            nodes.map_err(|error| map_error(error, this.tls))
        }.boxed()
    }

    fn table_columns(
        &self,
        object: ObjectRef,
    ) -> BoxFuture<'static, Result<Vec<ColumnInfo>, DbError>> {
        let this = self.clone();
        async move {
            let rows = sqlx::query("SELECT column_name, column_type FROM information_schema.columns WHERE table_schema = ? AND table_name = ? ORDER BY ordinal_position")
                .bind(object.database.as_deref().or_else(|| this.options.get_database()))
                .bind(&object.name).fetch_all(&this.pool).await.map_err(|error| map_error(error, this.tls))?;
            rows.into_iter().map(|row| {
                let type_name: String = row.try_get(1).map_err(|error| map_error(error, this.tls))?;
                Ok(ColumnInfo { name: row.try_get(0).map_err(|error| map_error(error, this.tls))?, numeric: is_numeric_type(&type_name), type_name })
            }).collect()
        }.boxed()
    }

    fn fetch_page(&self, request: PageRequest) -> BoxFuture<'static, Result<Page, DbError>> {
        let this = self.clone();
        async move {
            let rows = sqlx::raw_sql(AssertSqlSafe(select_page_sql(Dialect::MySql, &request)))
                .fetch_all(&this.pool)
                .await
                .map_err(|error| map_error(error, this.tls))?;
            let columns = match rows.first() {
                Some(row) => row_columns(row),
                None => this.table_columns(request.object).await?,
            };
            let limit = usize::try_from(request.limit).unwrap_or(usize::MAX);
            let has_more = rows.len() > limit;
            let rows = rows
                .iter()
                .take(limit)
                .map(row_cells)
                .collect::<Result<_, _>>()
                .map_err(|error| map_error(error, this.tls))?;
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
        let this = self.clone();
        async move {
            let options = match database {
                Some(database) => this.options.database(&database),
                None => this.options,
            };
            let connection = async {
                let mut connection = MySqlConnection::connect_with(&options).await?;
                let id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
                    .fetch_one(&mut connection)
                    .await?;
                Ok::<_, sqlx::Error>((connection, id))
            };
            let (connection, id) = tokio::time::timeout(CONNECT_TIMEOUT, connection)
                .await
                .map_err(|_| DbError::Timeout { seconds: 10 })?
                .map_err(|error| map_error(error, this.tls))?;
            Ok(Arc::new(MySqlQuery {
                connection: Arc::new(Mutex::new(Some(connection))),
                id,
                pool: this.pool,
                tls: this.tls,
            }) as Arc<dyn QueryConnection>)
        }
        .boxed()
    }

    fn close(&self) -> BoxFuture<'static, Result<(), DbError>> {
        let pool = self.pool.clone();
        async move {
            pool.close().await;
            Ok(())
        }
        .boxed()
    }
}

#[derive(Clone)]
struct MySqlQuery {
    connection: Arc<Mutex<Option<MySqlConnection>>>,
    id: u64,
    pool: MySqlPool,
    tls: TlsMode,
}

impl QueryConnection for MySqlQuery {
    fn execute(
        &self,
        sql: String,
        row_cap: usize,
    ) -> BoxFuture<'static, Result<Vec<StatementResult>, DbError>> {
        let this = self.clone();
        async move {
            let mut guard = this.connection.lock().await;
            let connection = guard
                .as_mut()
                .ok_or_else(|| DbError::Connect("Query connection is closed".into()))?;
            let mut stream = sqlx::raw_sql(AssertSqlSafe(sql)).fetch_many(connection);
            let mut results = Vec::new();
            let mut columns = None;
            let mut rows = Vec::new();
            let mut truncated = false;
            while let Some(item) = stream
                .try_next()
                .await
                .map_err(|error| map_error(error, this.tls))?
            {
                match item {
                    Either::Right(row) => {
                        columns.get_or_insert_with(|| row_columns(&row));
                        if rows.len() < row_cap {
                            rows.push(row_cells(&row).map_err(|error| map_error(error, this.tls))?);
                        } else {
                            truncated = true;
                        }
                    }
                    Either::Left(result) => {
                        // No row means no metadata, including for a zero-row SELECT.
                        results.push(match columns.take() {
                            Some(columns) => StatementResult::Rows {
                                columns,
                                rows: std::mem::take(&mut rows),
                                truncated,
                            },
                            None => StatementResult::Affected {
                                rows: result.rows_affected(),
                            },
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
        let this = self.clone();
        async move {
            sqlx::raw_sql(AssertSqlSafe(format!("KILL QUERY {}", this.id)))
                .execute(&this.pool)
                .await
                .map_err(|error| map_error(error, this.tls))?;
            Ok(())
        }
        .boxed()
    }

    fn close(&self) -> BoxFuture<'static, Result<(), DbError>> {
        let this = self.clone();
        async move {
            if let Some(connection) = this.connection.lock().await.take() {
                connection
                    .close()
                    .await
                    .map_err(|error| map_error(error, this.tls))?;
            }
            Ok(())
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::model::{ROW_CAP, Secret, SortDirection, test_server_params};

    const SERVERS: [&str; 2] = ["COLONNADE_TEST_MYSQL_URL", "COLONNADE_TEST_MARIADB_URL"];

    struct Fixture {
        backend: Arc<dyn DbBackend>,
        query: Arc<dyn QueryConnection>,
        prefix: String,
    }
    impl Fixture {
        async fn new(server: &str) -> Self {
            let backend = connect(test_server_params(server)).await.unwrap();
            let query = backend
                .open_query_connection(Some("colonnade".into()))
                .await
                .unwrap();
            let prefix = format!("ct_{}_", uuid::Uuid::new_v4().simple());
            query.execute(format!(
                "CREATE TABLE {prefix}z_table (id INT PRIMARY KEY, name VARCHAR(64), data BLOB, doc JSON, amount DECIMAL(10,2)); \
                 CREATE VIEW {prefix}a_view AS SELECT * FROM {prefix}z_table; \
                 INSERT INTO {prefix}z_table VALUES {}",
                (1..=450).map(|id| format!("({id},{},X'00FF01','{{}}',12.34)",
                    if id == 1 { "NULL" } else { "''" })).collect::<Vec<_>>().join(",")
            ), ROW_CAP).await.unwrap();
            Self {
                backend,
                query,
                prefix,
            }
        }
        fn request(&self) -> PageRequest {
            PageRequest {
                object: ObjectRef {
                    database: Some("colonnade".into()),
                    schema: None,
                    name: format!("{}z_table", self.prefix),
                    kind: TableKind::Table,
                },
                order_by: Some(("id".into(), SortDirection::Ascending)),
                offset: 0,
                limit: 200,
            }
        }
        async fn finish(self) {
            self.query
                .execute(
                    format!(
                        "DROP VIEW {}a_view; DROP TABLE {}z_table",
                        self.prefix, self.prefix
                    ),
                    ROW_CAP,
                )
                .await
                .unwrap();
            self.query.close().await.unwrap();
            self.backend.close().await.unwrap();
        }
    }

    #[test]
    fn tls_modes_preserve_verification_policy() {
        // Given: every supported TLS policy.
        for (mode, expected) in [
            (TlsMode::Disable, MySqlSslMode::Disabled),
            (TlsMode::Prefer, MySqlSslMode::Preferred),
            (TlsMode::Require, MySqlSslMode::Required),
            (TlsMode::VerifyCa, MySqlSslMode::VerifyCa),
            (TlsMode::VerifyFull, MySqlSslMode::VerifyIdentity),
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
    #[test]
    fn database_error_numbers_choose_error_kind() {
        // Given: authentication, interruption, and SQL failure error numbers.
        for (number, expected) in [
            (1045, DbError::Auth("rejected".into())),
            (1317, DbError::Cancelled),
            (
                1064,
                DbError::Query {
                    message: "rejected".into(),
                    code: Some("1064".into()),
                },
            ),
        ] {
            // When: mapping the server error.
            let actual = database_error(number, "rejected", TlsMode::Prefer);
            // Then: callers receive the actionable category and numeric code.
            assert_eq!(actual, expected);
        }
    }
    #[test]
    fn transport_errors_are_connection_errors() {
        // Given: failures before a query can reach a server.
        for error in [
            sqlx::Error::Io(std::io::Error::other("offline")),
            sqlx::Error::Tls("TLS failed".into()),
            sqlx::Error::PoolClosed,
            sqlx::Error::PoolTimedOut,
            sqlx::Error::Configuration("invalid option".into()),
        ] {
            // When: mapping a driver failure.
            let actual = map_error(error, TlsMode::Prefer);
            // Then: the error belongs to connection handling.
            assert!(matches!(actual, DbError::Connect(_)));
        }
    }
    #[test]
    fn auth_hint_mentions_tls() {
        // Given: SHA-2 authentication over an explicitly unencrypted connection.
        let message = "caching_sha2_password authentication unavailable";
        // When: producing actionable authentication guidance.
        let hint = auth_hint(TlsMode::Disable, message).unwrap();
        let error = database_error(1045, message, TlsMode::Disable);
        // Then: the guidance reaches the authentication error.
        assert!(
            hint.contains("TLS")
                && hint.contains("Preferred")
                && hint.contains("mysql_native_password")
        );
        assert!(matches!(error, DbError::Auth(text) if text.ends_with(hint)));
    }
    #[test]
    fn auth_hint_is_specific_to_unencrypted_sha2() {
        // Given: other authentication failures or an encrypted policy.
        for (tls, message) in [
            (TlsMode::Disable, "access denied"),
            (TlsMode::Prefer, "caching_sha2_password"),
            (TlsMode::Require, "caching_sha2_password"),
            (TlsMode::VerifyCa, "caching_sha2_password"),
            (TlsMode::VerifyFull, "caching_sha2_password"),
        ] {
            // When: requesting guidance.
            let hint = auth_hint(tls, message);
            // Then: no misleading recommendation is added.
            assert_eq!(hint, None);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires both database test servers"]
    async fn connects_and_reports_server_version() {
        for server in SERVERS {
            // Given: a live server with isolated fixtures.
            let fixture = Fixture::new(server).await;
            // When: querying its version.
            let version = fixture.backend.server_version().await.unwrap();
            // Then: the expected engine answers, whatever its release.
            assert!(version.starts_with(|c: char| c.is_ascii_digit()));
            assert_eq!(version.contains("MariaDB"), server == SERVERS[1]);
            assert_eq!(fixture.backend.dialect(), Dialect::MySql);
            fixture.finish().await;
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires both database test servers"]
    async fn databases_include_colonnade() {
        for server in SERVERS {
            // Given: a server granting access to the fixture database.
            let fixture = Fixture::new(server).await;
            // When: traversing the root catalog.
            let nodes = fixture
                .backend
                .list_children(ObjectPath::Root)
                .await
                .unwrap();
            // Then: the database has a navigable path.
            assert!(nodes.iter().any(|node| node.name == "colonnade"
                && node.kind == ObjectKind::Database
                && node.children
                    == Some(ObjectPath::Database {
                        name: "colonnade".into()
                    })));
            fixture.finish().await;
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires both database test servers"]
    async fn tables_precede_views() {
        for server in SERVERS {
            // Given: a view whose name sorts before its table.
            let fixture = Fixture::new(server).await;
            // When: listing the database.
            let nodes = fixture
                .backend
                .list_children(ObjectPath::Database {
                    name: "colonnade".into(),
                })
                .await
                .unwrap();
            let own: Vec<_> = nodes
                .iter()
                .filter(|node| node.name.starts_with(&fixture.prefix))
                .collect();
            // Then: object kind takes precedence over name order.
            assert_eq!(
                own.iter().map(|node| node.kind).collect::<Vec<_>>(),
                [ObjectKind::Table, ObjectKind::View]
            );
            assert_eq!(own[1].object.as_ref().unwrap().kind, TableKind::View);
            fixture.finish().await;
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires both database test servers"]
    async fn column_types_are_engine_native() {
        for server in SERVERS {
            // Given: a table containing representative SQL types.
            let fixture = Fixture::new(server).await;
            // When: retrieving ordinal metadata.
            let columns = fixture
                .backend
                .table_columns(fixture.request().object)
                .await
                .unwrap();
            // Then: precision and engine-specific JSON storage survive.
            let types: Vec<_> = columns
                .iter()
                .map(|column| column.type_name.as_str())
                .collect();
            assert!(types[0].starts_with("int"));
            assert_eq!(
                &types[1..],
                [
                    "varchar(64)",
                    "blob",
                    if server == SERVERS[0] {
                        "json"
                    } else {
                        "longtext"
                    },
                    "decimal(10,2)"
                ]
            );
            fixture.finish().await;
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires both database test servers"]
    async fn paging_covers_450_rows() {
        for server in SERVERS {
            // Given: 450 fixture rows.
            let fixture = Fixture::new(server).await;
            for (offset, count, more) in [
                (0, 200, true),
                (200, 200, true),
                (400, 50, false),
                (450, 0, false),
            ] {
                // When: fetching one page.
                let page = fixture
                    .backend
                    .fetch_page(PageRequest {
                        offset,
                        ..fixture.request()
                    })
                    .await
                    .unwrap();
                // Then: the extra row determines whether another page exists.
                assert_eq!((page.rows.len(), page.has_more), (count, more));
                assert_eq!(page.columns.len(), 5);
            }
            fixture.finish().await;
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires both database test servers"]
    async fn descending_order_is_server_side() {
        for server in SERVERS {
            // Given: ascending fixture IDs.
            let fixture = Fixture::new(server).await;
            // When: requesting descending order.
            let page = fixture
                .backend
                .fetch_page(PageRequest {
                    order_by: Some(("id".into(), SortDirection::Descending)),
                    ..fixture.request()
                })
                .await
                .unwrap();
            // Then: numeric ordering determines the first row.
            assert_eq!(page.rows[0][0], Cell::text("450".into()));
            fixture.finish().await;
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires both database test servers"]
    async fn null_and_empty_are_distinct() {
        for server in SERVERS {
            // Given: NULL followed by an empty string.
            let fixture = Fixture::new(server).await;
            // When: decoding a text-protocol page.
            let page = fixture.backend.fetch_page(fixture.request()).await.unwrap();
            // Then: SQL NULL is not collapsed into an empty value.
            assert_eq!(page.rows[0][1], Cell::Null);
            assert_eq!(page.rows[1][1], Cell::text(String::new()));
            fixture.finish().await;
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires both database test servers"]
    async fn blobs_preserve_byte_lengths() {
        for server in SERVERS {
            // Given: a blob containing non-UTF-8 bytes.
            let fixture = Fixture::new(server).await;
            // When: decoding a page.
            let page = fixture.backend.fetch_page(fixture.request()).await.unwrap();
            // Then: binary content is represented by length, not lossy text.
            assert_eq!(page.rows[0][2], Cell::Binary { len: 3 });
            fixture.finish().await;
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires both database test servers"]
    async fn numeric_flags_follow_types() {
        for server in SERVERS {
            // Given: numeric and textual fixture columns.
            let fixture = Fixture::new(server).await;
            // When: fetching query metadata.
            let page = fixture.backend.fetch_page(fixture.request()).await.unwrap();
            // Then: only integer and decimal columns are numeric.
            assert_eq!(
                page.columns
                    .iter()
                    .map(|column| column.numeric)
                    .collect::<Vec<_>>(),
                [true, false, false, false, true]
            );
            fixture.finish().await;
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires both database test servers"]
    async fn multi_statement_results_preserve_session() {
        for server in SERVERS {
            // Given: a dedicated connection and a unique temporary table name.
            let fixture = Fixture::new(server).await;
            let name = format!("{}temp", fixture.prefix);
            // When: executing a batch and then reusing the same session.
            let results = fixture.query.execute(format!("CREATE TEMPORARY TABLE {name}(x INT); INSERT INTO {name} VALUES (1),(2); SELECT * FROM {name}"), ROW_CAP).await.unwrap();
            let next = fixture
                .query
                .execute(
                    format!("SELECT * FROM {name}; SELECT * FROM {name} WHERE 0"),
                    ROW_CAP,
                )
                .await
                .unwrap();
            // Then: each statement has its own result and temporary state survives.
            assert_eq!(
                &results[..2],
                [
                    StatementResult::Affected { rows: 0 },
                    StatementResult::Affected { rows: 2 }
                ]
            );
            assert!(
                matches!(&results[2], StatementResult::Rows { rows, truncated: false, .. } if rows.len() == 2)
            );
            assert_eq!(next[0], results[2]);
            assert_eq!(next[1], StatementResult::Affected { rows: 0 });
            fixture.finish().await;
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires both database test servers"]
    async fn row_cap_drains_remaining_rows_and_statements() {
        for server in SERVERS {
            // Given: recursion settings supporting 20,000 rows on each engine.
            let fixture = Fixture::new(server).await;
            fixture
                .query
                .execute(
                    if server == SERVERS[0] {
                        "SET SESSION cte_max_recursion_depth = 20000"
                    } else {
                        "SET SESSION max_recursive_iterations = 20000"
                    }
                    .into(),
                    ROW_CAP,
                )
                .await
                .unwrap();
            // When: executing an oversized result followed by a second SELECT.
            let results = fixture.query.execute("WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n<20000) SELECT n FROM seq; SELECT 42".into(), ROW_CAP).await.unwrap();
            // Then: storage is capped without losing later result boundaries.
            assert!(
                matches!(&results[0], StatementResult::Rows { rows, truncated: true, .. } if rows.len() == ROW_CAP)
            );
            assert!(
                matches!(&results[1], StatementResult::Rows { rows, truncated: false, .. } if rows == &vec![vec![Cell::text("42".into())]])
            );
            fixture.finish().await;
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires both database test servers"]
    async fn cancellation_interrupts_only_running_query() {
        for server in SERVERS {
            // Given: a dedicated session executing an interruptible long-running query.
            let fixture = Fixture::new(server).await;
            let start = std::time::Instant::now();
            let query = fixture.query.execute(
                "SELECT a FROM (SELECT 1 AS a UNION ALL SELECT 2) AS t WHERE SLEEP(30) = 0".into(),
                ROW_CAP,
            );
            // When: sending KILL QUERY after the prescribed 200 ms.
            let cancel = async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                fixture.query.cancel().await.unwrap();
            };
            let (result, ()) =
                tokio::time::timeout(Duration::from_secs(5), futures::future::join(query, cancel))
                    .await
                    .unwrap();
            // Then: cancellation is surfaced promptly and the session remains usable.
            assert_eq!(result, Err(DbError::Cancelled));
            assert!(start.elapsed() < Duration::from_secs(5));
            fixture
                .query
                .execute("SELECT 1".into(), ROW_CAP)
                .await
                .unwrap();
            fixture.finish().await;
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires both database test servers"]
    async fn wrong_password_is_authentication_error() {
        for server in SERVERS {
            // Given: an existing account with an incorrect password.
            let fixture = Fixture::new(server).await;
            let mut params = test_server_params(server);
            params.password = Some(Secret::new("deliberately-wrong".into()));
            // When: validating credentials.
            let result = connect(params).await;
            // Then: callers can prompt for new credentials.
            assert!(matches!(result, Err(DbError::Auth(_))));
            fixture.finish().await;
        }
    }
}
