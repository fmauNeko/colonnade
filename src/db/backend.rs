//! Backend traits shared by every database driver.

use super::model::{
    ColumnInfo, ConnectParams, DbError, Dialect, ObjectNode, ObjectPath, ObjectRef, Page,
    PageRequest, StatementResult,
};
use futures::future::BoxFuture;
use gpui_kit::{App, Global};
use std::sync::Arc;

pub trait DbBackend: Send + Sync {
    fn dialect(&self) -> Dialect;
    /// # Errors
    /// Returns the driver's connection or query error.
    fn server_version(&self) -> BoxFuture<'static, Result<String, DbError>>;
    /// # Errors
    /// Returns catalog lookup errors or an unsupported-path error.
    fn list_children(
        &self,
        path: ObjectPath,
    ) -> BoxFuture<'static, Result<Vec<ObjectNode>, DbError>>;
    /// # Errors
    /// Returns metadata lookup errors.
    fn table_columns(
        &self,
        object: ObjectRef,
    ) -> BoxFuture<'static, Result<Vec<ColumnInfo>, DbError>>;
    /// # Errors
    /// Returns page query errors.
    fn fetch_page(&self, request: PageRequest) -> BoxFuture<'static, Result<Page, DbError>>;
    /// # Errors
    /// Returns errors opening the dedicated connection.
    fn open_query_connection(
        &self,
        database: Option<String>,
    ) -> BoxFuture<'static, Result<Arc<dyn QueryConnection>, DbError>>;
    /// # Errors
    /// Returns errors closing backend resources.
    fn close(&self) -> BoxFuture<'static, Result<(), DbError>>;
}
pub trait QueryConnection: Send + Sync {
    /// # Errors
    /// Returns execution errors, including cancellation and timeout.
    fn execute(
        &self,
        sql: String,
        row_cap: usize,
    ) -> BoxFuture<'static, Result<Vec<StatementResult>, DbError>>;
    /// # Errors
    /// Returns errors sending a cancellation request.
    fn cancel(&self) -> BoxFuture<'static, Result<(), DbError>>;
    /// # Errors
    /// Returns errors closing the dedicated connection.
    fn close(&self) -> BoxFuture<'static, Result<(), DbError>>;
}
pub trait BackendFactory: Send + Sync {
    /// # Errors
    /// Returns authentication, connection, or timeout errors.
    fn connect(
        &self,
        params: ConnectParams,
    ) -> BoxFuture<'static, Result<Arc<dyn DbBackend>, DbError>>;
}
pub struct Backends(pub Arc<dyn BackendFactory>);
impl Global for Backends {}
pub fn install_factory(cx: &mut App, factory: Arc<dyn BackendFactory>) {
    cx.set_global(Backends(factory));
}
