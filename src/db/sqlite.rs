//! SQLite backend.
// allow: SIZE_OK — The task's single-file ownership contract requires the SQLite adapter and its adjacent driver regression suite here.

use super::{
    backend::{DbBackend, QueryConnection},
    model::{
        Cell, ColumnInfo, DbError, Dialect, ObjectKind, ObjectNode, ObjectPath, ObjectRef, Page,
        PageRequest, SqliteParams, StatementResult, TableKind, is_numeric_type,
    },
    sql::select_page_sql,
};
use futures::{FutureExt, TryStreamExt, future::BoxFuture};
use sqlx::{
    AssertSqlSafe, Column, Connection, Either, Row, SqliteConnection, SqlitePool, TypeInfo,
    ValueRef,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;

/// Opens an existing SQLite database without changing its journal mode.
///
/// # Errors
/// Returns `DbError::Connect` with the database path if the connection fails.
pub fn connect(params: SqliteParams) -> BoxFuture<'static, Result<Arc<dyn DbBackend>, DbError>> {
    async move {
        let options = SqliteConnectOptions::new()
            .filename(&params.path)
            .create_if_missing(false)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options.clone())
            .await
            .map_err(|error| DbError::Connect(format!("{}: {error}", params.path.display())))?;
        Ok(Arc::new(SqliteBackend { pool, options }) as Arc<dyn DbBackend>)
    }
    .boxed()
}

struct SqliteBackend {
    pool: SqlitePool,
    options: SqliteConnectOptions,
}

fn query_error(error: sqlx::Error) -> DbError {
    match error {
        sqlx::Error::Database(error) => {
            let code = error.code().map(std::borrow::Cow::into_owned);
            if code.as_deref() == Some("9") || error.message().contains("interrupted") {
                DbError::Cancelled
            } else {
                DbError::Query {
                    message: error.to_string(),
                    code,
                }
            }
        }
        error => DbError::Query {
            message: error.to_string(),
            code: None,
        },
    }
}

fn columns(row: &SqliteRow) -> Vec<ColumnInfo> {
    row.columns()
        .iter()
        .map(|column| {
            let type_name = column.type_info().name().to_owned();
            ColumnInfo {
                name: column.name().to_owned(),
                numeric: is_numeric_type(&type_name),
                type_name,
            }
        })
        .collect()
}

fn cells(row: &SqliteRow) -> Result<Vec<Cell>, sqlx::Error> {
    (0..row.len())
        .map(|index| {
            let value = row.try_get_raw(index)?;
            if value.is_null() {
                Ok(Cell::Null)
            } else if value.type_info().name() == "BLOB" {
                Ok(Cell::Binary {
                    len: row.try_get_unchecked::<&[u8], _>(index)?.len(),
                })
            } else {
                Ok(Cell::text(row.try_get_unchecked::<String, _>(index)?))
            }
        })
        .collect()
}

impl DbBackend for SqliteBackend {
    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }

    fn server_version(&self) -> BoxFuture<'static, Result<String, DbError>> {
        let pool = self.pool.clone();
        async move {
            sqlx::query_scalar("SELECT 'SQLite ' || sqlite_version()")
                .fetch_one(&pool)
                .await
                .map_err(query_error)
        }
        .boxed()
    }

    fn list_children(
        &self,
        path: ObjectPath,
    ) -> BoxFuture<'static, Result<Vec<ObjectNode>, DbError>> {
        let pool = self.pool.clone();
        async move {
            match path {
                ObjectPath::Root => {},
                ObjectPath::Database { .. } | ObjectPath::Schema { .. } => return Err(DbError::Unsupported("SQLite objects are listed at the root".into())),
            }
            let rows = sqlx::query(r"SELECT name, type FROM sqlite_schema WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\_%' ESCAPE '\' ORDER BY type = 'view', name")
                .fetch_all(&pool).await.map_err(query_error)?;
            rows.iter().map(|row| {
                let name: String = row.try_get("name").map_err(query_error)?;
                let view = row.try_get::<String, _>("type").map_err(query_error)? == "view";
                Ok(ObjectNode {
                    kind: if view { ObjectKind::View } else { ObjectKind::Table },
                    object: Some(ObjectRef { database: None, schema: None, name: name.clone(), kind: if view { TableKind::View } else { TableKind::Table } }),
                    name, children: None,
                })
            }).collect()
        }.boxed()
    }

    fn table_columns(
        &self,
        object: ObjectRef,
    ) -> BoxFuture<'static, Result<Vec<ColumnInfo>, DbError>> {
        let pool = self.pool.clone();
        async move {
            let rows = sqlx::query("SELECT name, type FROM pragma_table_info(?1) ORDER BY cid")
                .bind(object.name)
                .fetch_all(&pool)
                .await
                .map_err(query_error)?;
            rows.iter()
                .map(|row| {
                    let type_name: String = row.try_get("type").map_err(query_error)?;
                    Ok(ColumnInfo {
                        name: row.try_get("name").map_err(query_error)?,
                        numeric: is_numeric_type(&type_name),
                        type_name,
                    })
                })
                .collect()
        }
        .boxed()
    }

    fn fetch_page(&self, request: PageRequest) -> BoxFuture<'static, Result<Page, DbError>> {
        let pool = self.pool.clone();
        let fallback = self.table_columns(request.object.clone());
        async move {
            let rows = sqlx::raw_sql(AssertSqlSafe(select_page_sql(Dialect::Sqlite, &request)))
                .fetch_all(&pool)
                .await
                .map_err(query_error)?;
            let columns = match rows.first() {
                Some(row) => columns(row),
                None => fallback.await?,
            };
            let limit = usize::try_from(request.limit).unwrap_or(usize::MAX);
            let has_more = rows.len() > limit;
            let rows = rows
                .iter()
                .take(limit)
                .map(cells)
                .collect::<Result<_, _>>()
                .map_err(query_error)?;
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
        _database: Option<String>,
    ) -> BoxFuture<'static, Result<Arc<dyn QueryConnection>, DbError>> {
        let options = self.options.clone();
        async move {
            let mut conn = SqliteConnection::connect_with(&options)
                .await
                .map_err(|error| DbError::Connect(error.to_string()))?;
            let cancelled = Arc::new(AtomicBool::new(false));
            let flag = cancelled.clone();
            conn.lock_handle()
                .await
                .map_err(query_error)?
                .set_progress_handler(1000, move || !flag.load(Ordering::Relaxed));
            Ok(Arc::new(SqliteQueryConnection {
                conn: Arc::new(Mutex::new(Some(conn))),
                cancelled,
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

struct SqliteQueryConnection {
    conn: Arc<Mutex<Option<SqliteConnection>>>,
    cancelled: Arc<AtomicBool>,
}

impl QueryConnection for SqliteQueryConnection {
    fn execute(
        &self,
        sql: String,
        row_cap: usize,
    ) -> BoxFuture<'static, Result<Vec<StatementResult>, DbError>> {
        let conn = self.conn.clone();
        let cancelled = self.cancelled.clone();
        async move {
            let mut guard = conn.lock().await;
            let conn = guard
                .as_mut()
                .ok_or_else(|| DbError::Connect("SQLite query connection is closed".into()))?;
            cancelled.store(false, Ordering::Relaxed);
            let mut stream = sqlx::raw_sql(AssertSqlSafe(sql)).fetch_many(&mut *conn);
            let mut results = Vec::new();
            let mut metadata = None;
            let mut rows = Vec::new();
            let mut truncated = false;
            while let Some(item) = stream.try_next().await.map_err(query_error)? {
                match item {
                    Either::Right(row) => {
                        metadata.get_or_insert_with(|| columns(&row));
                        if rows.len() < row_cap {
                            rows.push(cells(&row).map_err(query_error)?);
                        } else {
                            truncated = true;
                        }
                    }
                    Either::Left(result) => {
                        results.push(match metadata.take() {
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
        self.cancelled.store(true, Ordering::Relaxed);
        futures::future::ready(Ok(())).boxed()
    }

    fn close(&self) -> BoxFuture<'static, Result<(), DbError>> {
        let conn = self.conn.clone();
        self.cancelled.store(true, Ordering::Relaxed);
        async move {
            if let Some(conn) = conn.lock().await.take() {
                conn.close().await.map_err(query_error)?;
            }
            Ok(())
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{backend::DbBackend, model::*};
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    use std::{sync::Arc, time::Duration};

    async fn fixture(sql: &str) -> (tempfile::TempDir, SqliteParams) {
        let dir = tempfile::tempdir().unwrap();
        let params = SqliteParams {
            path: dir.path().join("fixture.db"),
        };
        let mut conn = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&params.path)
                .create_if_missing(true),
        )
        .await
        .unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned()))
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        (dir, params)
    }
    async fn backend(sql: &str) -> (tempfile::TempDir, Arc<dyn DbBackend>) {
        let (dir, params) = fixture(sql).await;
        (dir, connect(params).await.unwrap())
    }
    fn request() -> PageRequest {
        PageRequest {
            object: ObjectRef {
                database: None,
                schema: None,
                name: "items".into(),
                kind: TableKind::Table,
            },
            order_by: None,
            offset: 0,
            limit: PAGE_SIZE,
        }
    }
    async fn select(sql: &str) -> Vec<StatementResult> {
        let (_dir, db) = backend("CREATE TABLE items(id INTEGER)").await;
        let query = db.open_query_connection(None).await.unwrap();
        query.execute(sql.into(), ROW_CAP).await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_file_is_not_created() {
        // Given: a path that does not exist.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.db");
        // When: connecting without provisioning a database.
        let result = connect(SqliteParams { path: path.clone() }).await;
        // Then: the error identifies the path without creating it.
        assert!(
            matches!(result, Err(DbError::Connect(message)) if message.contains(&path.display().to_string()))
        );
        assert!(!path.exists());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tables_precede_views_in_name_order() {
        // Given: unordered tables, views and an internal sqlite_sequence table.
        let (_dir, db) = backend("CREATE TABLE zebra(id INTEGER PRIMARY KEY AUTOINCREMENT); CREATE TABLE alpha(id TEXT); CREATE VIEW aardvark AS SELECT * FROM alpha; CREATE VIEW zview AS SELECT * FROM zebra").await;
        // When: listing the root.
        let nodes = db.list_children(ObjectPath::Root).await.unwrap();
        // Then: only user objects appear, with navigable object identities.
        assert_eq!(
            nodes
                .iter()
                .map(|n| (n.name.as_str(), n.kind))
                .collect::<Vec<_>>(),
            vec![
                ("alpha", ObjectKind::Table),
                ("zebra", ObjectKind::Table),
                ("aardvark", ObjectKind::View),
                ("zview", ObjectKind::View)
            ]
        );
        assert!(
            nodes
                .iter()
                .all(|n| n.children.is_none() && n.object.as_ref().unwrap().name == n.name)
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nested_catalog_paths_are_unsupported() {
        // Given: a SQLite backend with no database/schema hierarchy.
        let (_dir, db) = backend("SELECT 1").await;
        for path in [
            ObjectPath::Database {
                name: "main".into(),
            },
            ObjectPath::Schema {
                database: None,
                name: "main".into(),
            },
        ] {
            // When: looking below the root.
            let result = db.list_children(path).await;
            // Then: the path is explicitly unsupported.
            assert!(matches!(result, Err(DbError::Unsupported(_))));
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn column_types_preserve_declarations() {
        // Given: an empty table with distinct declared affinities.
        let (_dir, db) =
            backend("CREATE TABLE items(id INTEGER, label TEXT, price NUMERIC(10,2), data BLOB)")
                .await;
        // When: reading metadata.
        let columns = db.table_columns(request().object).await.unwrap();
        // Then: column order, declared types and numeric classification survive.
        assert_eq!(
            columns
                .iter()
                .map(|c| (c.name.as_str(), c.type_name.as_str(), c.numeric))
                .collect::<Vec<_>>(),
            vec![
                ("id", "INTEGER", true),
                ("label", "TEXT", false),
                ("price", "NUMERIC(10,2)", true),
                ("data", "BLOB", false)
            ]
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pages_cover_450_rows() {
        // Given: 450 stored records.
        let (_dir, db) = backend("CREATE TABLE items(id INTEGER); WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<450) INSERT INTO items SELECT x FROM c").await;
        for (offset, count, more) in [(0, 200, true), (200, 200, true), (400, 50, false)] {
            // When: fetching a page.
            let page = db
                .fetch_page(PageRequest {
                    offset,
                    ..request()
                })
                .await
                .unwrap();
            // Then: the extra probe row never leaks into the page.
            assert_eq!((page.rows.len(), page.has_more), (count, more));
            assert_eq!(page.rows[0][0], Cell::text((offset + 1).to_string()));
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ordering_is_applied_before_limit() {
        // Given: IDs whose numeric and lexical orders differ.
        let (_dir, db) =
            backend("CREATE TABLE items(id INTEGER); INSERT INTO items VALUES (2),(42),(9)").await;
        // When: requesting descending IDs with a single-row limit.
        let page = db
            .fetch_page(PageRequest {
                order_by: Some(("id".into(), SortDirection::Descending)),
                limit: 1,
                ..request()
            })
            .await
            .unwrap();
        // Then: server-side numeric sorting selects 42.
        assert_eq!(page.rows, vec![vec![Cell::text("42".into())]]);
        assert!(page.has_more);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_page_retains_columns() {
        // Given: an empty table.
        let (_dir, db) = backend("CREATE TABLE items(id INTEGER)").await;
        // When: fetching its page.
        let page = db.fetch_page(request()).await.unwrap();
        // Then: declared metadata remains available without any row.
        assert_eq!(
            page,
            Page {
                columns: vec![ColumnInfo {
                    name: "id".into(),
                    type_name: "INTEGER".into(),
                    numeric: true
                }],
                rows: vec![],
                has_more: false
            }
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn null_and_empty_text_are_distinct() {
        // Given: NULL and empty text in the same result.
        let sql = "SELECT NULL AS absent, '' AS empty";
        // When: executing.
        let results = select(sql).await;
        // Then: NULL is not converted to an empty string.
        assert!(
            matches!(&results[0], StatementResult::Rows { rows, .. } if rows == &vec![vec![Cell::Null, Cell::text(String::new())]])
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blobs_use_runtime_value_types() {
        // Given: BLOB expressions without declared column types.
        let sql = "SELECT x'00ff80', x''";
        // When: executing.
        let results = select(sql).await;
        // Then: binary sizes are reported instead of attempting UTF-8 decoding.
        assert!(
            matches!(&results[0], StatementResult::Rows { rows, .. } if rows == &vec![vec![Cell::Binary { len: 3 }, Cell::Binary { len: 0 }]])
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn integers_decode_as_text() {
        // Given: an integer expression.
        let sql = "SELECT 42 AS answer";
        // When: executing.
        let results = select(sql).await;
        // Then: SQLite converts its value to display text.
        assert!(
            matches!(&results[0], StatementResult::Rows { rows, .. } if rows == &vec![vec![Cell::text("42".into())]])
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multi_statement_session_retains_temp_tables() {
        // Given: a dedicated connection with session-local data.
        let (_dir, db) = backend("SELECT 1").await;
        let query = db.open_query_connection(None).await.unwrap();
        query
            .execute(
                "CREATE TEMP TABLE scratch(id INTEGER); INSERT INTO scratch VALUES(42)".into(),
                ROW_CAP,
            )
            .await
            .unwrap();
        // When: running a later multi-statement script in that session.
        let results = query
            .execute(
                "SELECT id FROM scratch; UPDATE scratch SET id=9; SELECT id FROM scratch".into(),
                ROW_CAP,
            )
            .await
            .unwrap();
        // Then: each statement has its own correctly shaped outcome.
        assert_eq!(results.len(), 3);
        assert!(
            matches!(&results[0], StatementResult::Rows { rows, truncated: false, .. } if rows == &vec![vec![Cell::text("42".into())]])
        );
        assert_eq!(results[1], StatementResult::Affected { rows: 1 });
        assert!(
            matches!(&results[2], StatementResult::Rows { rows, .. } if rows == &vec![vec![Cell::text("9".into())]])
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn row_cap_drains_before_next_statement() {
        // Given: a query with more rows than the display cap followed by another query.
        let sql = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<10001) SELECT x FROM c; SELECT 42";
        // When: executing with the normal cap.
        let results = select(sql).await;
        // Then: only retained rows are capped; later statements still complete.
        assert!(
            matches!(&results[0], StatementResult::Rows { rows, truncated: true, .. } if rows.len() == ROW_CAP)
        );
        assert!(
            matches!(&results[1], StatementResult::Rows { rows, truncated: false, .. } if rows == &vec![vec![Cell::text("42".into())]])
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zero_row_select_is_affected_zero() {
        // Given: a SELECT that produces no rows.
        let sql = "SELECT 42 WHERE 0";
        // When: executing.
        let results = select(sql).await;
        // Then: no row metadata is invented.
        assert_eq!(results, vec![StatementResult::Affected { rows: 0 }]);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn infinite_query_is_cancelled() {
        // Given: a recursive query that never terminates on its own.
        let (_dir, db) = backend("SELECT 1").await;
        let query = db.open_query_connection(None).await.unwrap();
        let start = std::time::Instant::now();
        // When: cancelling after the specified 100 ms while execute is polled concurrently.
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                query.execute("WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c".into(), ROW_CAP),
                async { tokio::time::sleep(Duration::from_millis(100)).await; query.cancel().await.unwrap(); }
            )
        }).await.unwrap();
        // Then: SQLite interrupts promptly, and the next execution resets cancellation.
        assert_eq!(result, Err(DbError::Cancelled));
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(query.execute("SELECT 42".into(), ROW_CAP).await.is_ok());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn journal_mode_is_unchanged() {
        // Given: an existing WAL database, distinct from SQLite's default delete mode.
        let (_dir, params) =
            fixture("PRAGMA journal_mode=WAL; CREATE TABLE items(id INTEGER)").await;
        // When: opening the backend and its dedicated query connection.
        let db = connect(params).await.unwrap();
        let query = db.open_query_connection(None).await.unwrap();
        let results = query
            .execute("PRAGMA journal_mode".into(), ROW_CAP)
            .await
            .unwrap();
        // Then: connecting has not changed the persisted mode.
        assert!(
            matches!(&results[0], StatementResult::Rows { rows, .. } if rows == &vec![vec![Cell::text("wal".into())]])
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn version_reports_sqlite() {
        // Given: a live backend.
        let (_dir, db) = backend("SELECT 1").await;
        // When: reading its server version.
        let version = db.server_version().await.unwrap();
        // Then: the database identifies its dialect and version.
        assert_eq!(db.dialect(), Dialect::Sqlite);
        assert!(
            version
                .strip_prefix("SQLite ")
                .unwrap()
                .split('.')
                .all(|part| part.parse::<u32>().is_ok())
        );
    }
}
