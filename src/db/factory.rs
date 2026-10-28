//! Production backend factory: builds the right driver for a connection.

use super::{
    backend::{BackendFactory, DbBackend, install_factory},
    model::{ConnectParams, DbError},
    mysql, postgres, sqlite,
};
use futures::{FutureExt, future::BoxFuture};
use gpui_kit::App;
use std::sync::Arc;

/// Factory dispatching each connection to its `sqlx` driver.
pub struct SqlxFactory;

impl BackendFactory for SqlxFactory {
    fn connect(
        &self,
        params: ConnectParams,
    ) -> BoxFuture<'static, Result<Arc<dyn DbBackend>, DbError>> {
        match params {
            ConnectParams::Postgres(server) => postgres::connect(server),
            ConnectParams::MySql(server) => mysql::connect(server),
            ConnectParams::Sqlite(file) => sqlite::connect(file),
        }
    }
}

/// Installs the production factory as the global backend registry.
pub fn install(cx: &mut App) {
    install_factory(cx, Arc::new(SqlxFactory));
}

/// Connects, reads the server version and closes the connection.
///
/// # Errors
/// Returns the connection error, or the version query or close error.
pub fn test_connection(
    factory: Arc<dyn BackendFactory>,
    params: ConnectParams,
) -> BoxFuture<'static, Result<String, DbError>> {
    async move {
        let backend = factory.connect(params).await?;
        let version = backend.server_version().await;
        backend.close().await?;
        version
    }
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{
        fake::{FakeEndpoint, FakeFactory, FakeServer},
        model::{Dialect, ServerParams, SqliteParams, TlsMode},
    };
    use gpui_kit::TestAppContext;
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};

    fn server_params() -> ServerParams {
        ServerParams {
            host: "db.test".into(),
            port: 5432,
            user: "user".into(),
            password: None,
            database: None,
            tls: TlsMode::default(),
        }
    }

    #[gpui_kit::test]
    fn test_connection_returns_the_server_version(_cx: &mut TestAppContext) {
        // Given: a fake factory with a registered PostgreSQL server.
        let factory = FakeFactory::new();
        factory.register(
            FakeEndpoint::Host("db.test".into()),
            Arc::new(FakeServer::sample(Dialect::Postgres)),
        );
        // When: testing the connection.
        let version = futures::executor::block_on(test_connection(
            Arc::new(factory),
            ConnectParams::Postgres(server_params()),
        ))
        .unwrap();
        // Then: the backend's version is returned.
        assert_eq!(version, "Fake Postgres 1.0");
    }

    #[gpui_kit::test]
    fn test_connection_propagates_the_connect_error(_cx: &mut TestAppContext) {
        // Given: a fake factory whose next connect fails.
        let factory = FakeFactory::new();
        factory.fail_connect(DbError::Auth("bad password".into()));
        // When: testing the connection.
        let result = futures::executor::block_on(test_connection(
            Arc::new(factory),
            ConnectParams::Postgres(server_params()),
        ));
        // Then: the scripted error is propagated.
        assert_eq!(result, Err(DbError::Auth("bad password".into())));
    }

    #[tokio::test]
    async fn sqlx_factory_connects_to_a_sqlite_file() {
        // Given: an existing SQLite database file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.db");
        let conn = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true),
        )
        .await
        .unwrap();
        conn.close().await.unwrap();
        // When: testing the connection through the production factory.
        let version = test_connection(
            Arc::new(SqlxFactory),
            ConnectParams::Sqlite(SqliteParams { path }),
        )
        .await
        .unwrap();
        // Then: the SQLite version is reported.
        assert!(version.starts_with("SQLite "));
    }
}
