//! Database model types: schemas, tables, columns and query results.
// allow: SIZE_OK — The fixed five-file contract keeps the database's declarative wire model and its adjacent tests together.

use serde::{Deserialize, Serialize};
use std::{fmt, path::PathBuf};

#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);
impl Secret {
    #[must_use]
    pub const fn new(value: String) -> Self {
        Self(value)
    }
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    Disable,
    #[default]
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerParams {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<Secret>,
    pub database: Option<String>,
    pub tls: TlsMode,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqliteParams {
    pub path: PathBuf,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectParams {
    Postgres(ServerParams),
    MySql(ServerParams),
    Sqlite(SqliteParams),
}
impl ConnectParams {
    #[must_use]
    pub const fn dialect(&self) -> Dialect {
        match self {
            Self::Postgres(_) => Dialect::Postgres,
            Self::MySql(_) => Dialect::MySql,
            Self::Sqlite(_) => Dialect::Sqlite,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dialect {
    Postgres,
    MySql,
    Sqlite,
}
impl Dialect {
    #[must_use]
    pub const fn language_name(self) -> &'static str {
        match self {
            Self::Postgres => "sql-postgres",
            Self::MySql => "sql-mysql",
            Self::Sqlite => "sql-sqlite",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TableKind {
    Table,
    View,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ObjectRef {
    pub database: Option<String>,
    pub schema: Option<String>,
    pub name: String,
    pub kind: TableKind,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ObjectPath {
    Root,
    Database {
        name: String,
    },
    Schema {
        database: Option<String>,
        name: String,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    Database,
    Schema,
    Table,
    View,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectNode {
    pub kind: ObjectKind,
    pub name: String,
    pub children: Option<ObjectPath>,
    pub object: Option<ObjectRef>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub type_name: String,
    pub numeric: bool,
}

#[must_use]
pub fn is_numeric_type(type_name: &str) -> bool {
    let lower = type_name.to_lowercase();
    let base = lower.split('(').next().unwrap_or_default();
    let normalized = base
        .split_whitespace()
        .filter(|word| !matches!(*word, "unsigned" | "zerofill"))
        .collect::<Vec<_>>()
        .join(" ");
    matches!(
        normalized.as_str(),
        "smallint"
            | "integer"
            | "int"
            | "int2"
            | "int4"
            | "int8"
            | "bigint"
            | "tinyint"
            | "mediumint"
            | "smallserial"
            | "serial"
            | "bigserial"
            | "serial2"
            | "serial4"
            | "serial8"
            | "numeric"
            | "decimal"
            | "dec"
            | "fixed"
            | "real"
            | "float"
            | "float4"
            | "float8"
            | "double"
            | "double precision"
            | "money"
    )
}

pub const TEXT_CELL_CAP: usize = 65_536;
pub const PAGE_SIZE: u32 = 200;
pub const ROW_CAP: usize = 10_000;
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cell {
    Null,
    Text { value: String, truncated: bool },
    Binary { len: usize },
}
impl Cell {
    #[must_use]
    pub fn text(mut value: String) -> Self {
        let truncated = value.len() > TEXT_CELL_CAP;
        if truncated {
            value.truncate(value.floor_char_boundary(TEXT_CELL_CAP));
        }
        Self::Text { value, truncated }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortDirection {
    Ascending,
    Descending,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageRequest {
    pub object: ObjectRef,
    pub order_by: Option<(String, SortDirection)>,
    pub offset: u64,
    pub limit: u32,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Vec<Cell>>,
    pub has_more: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatementResult {
    Rows {
        columns: Vec<ColumnInfo>,
        rows: Vec<Vec<Cell>>,
        truncated: bool,
    },
    Affected {
        rows: u64,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DbError {
    #[error("Connection failed: {0}")]
    Connect(String),
    #[error("Authentication failed: {0}")]
    Auth(String),
    #[error("Query failed: {message}")]
    Query {
        message: String,
        code: Option<String>,
    },
    #[error("Cancelled")]
    Cancelled,
    #[error("Timed out after {seconds} seconds")]
    Timeout { seconds: u64 },
    #[error("Database runtime stopped")]
    Runtime,
    #[error("Unsupported: {0}")]
    Unsupported(String),
}

/// Reads the driver integration-test URL without exposing credentials in failures.
///
/// # Panics
/// Panics when the variable is missing or its URL lacks a host or explicit port.
#[cfg(test)]
#[must_use]
pub fn test_server_params(env_var: &str) -> ServerParams {
    let raw = std::env::var(env_var)
        .unwrap_or_else(|_| panic!("set {env_var} (run mise run db:up, then mise run test:db)"));
    parse_test_server_params(&raw)
}
#[cfg(test)]
fn parse_test_server_params(raw: &str) -> ServerParams {
    let url = url::Url::parse(raw).unwrap_or_else(|_| panic!("invalid database test URL"));
    let decode = |value: &str| {
        let encoded = format!("v={}", value.replace('+', "%2B"));
        url::form_urlencoded::parse(encoded.as_bytes())
            .next()
            .unwrap()
            .1
            .into_owned()
    };
    ServerParams {
        host: url.host_str().expect("database test URL host").into(),
        port: url.port().expect("database test URL port"),
        user: decode(url.username()),
        password: url.password().map(|value| Secret::new(decode(value))),
        database: Some(decode(url.path().trim_start_matches('/'))),
        tls: TlsMode::Prefer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_url_decodes_credentials_without_form_plus_conversion() {
        // Given: a URL containing encoded delimiters and literal plus characters.
        let raw = "postgres://some%40user:secret%3A%2F+word@localhost:55432/app";
        // When: building test connection parameters.
        let params = parse_test_server_params(raw);
        // Then: credentials are decoded and explicit connection settings survive.
        assert_eq!(params.user, "some@user");
        assert_eq!(params.password.unwrap().expose(), "secret:/+word");
        assert_eq!(params.port, 55432);
        assert_eq!(params.database.as_deref(), Some("app"));
    }
    #[test]
    fn secret_debug_is_redacted() {
        // Given: a password.
        let secret = Secret::new("sensitive".into());
        // When: formatting it.
        let debug = format!("{secret:?}");
        // Then: only explicit access reveals the password.
        assert_eq!(debug, "Secret(***)");
        assert_eq!(secret.expose(), "sensitive");
    }
    #[test]
    fn text_caps_without_splitting_unicode() {
        // Given: a character straddling the cap.
        let input = format!("{}🦀tail", "a".repeat(TEXT_CELL_CAP - 1));
        // When: constructing a cell.
        let cell = Cell::text(input);
        // Then: the partial character is omitted.
        assert_eq!(
            cell,
            Cell::Text {
                value: "a".repeat(TEXT_CELL_CAP - 1),
                truncated: true
            }
        );
    }
    #[test]
    fn text_at_cap_is_complete() {
        // Given: an exactly sized string.
        let value = "é".repeat(TEXT_CELL_CAP / 2);
        // When: constructing a cell.
        let cell = Cell::text(value.clone());
        // Then: it is complete.
        assert_eq!(
            cell,
            Cell::Text {
                value,
                truncated: false
            }
        );
    }
    #[test]
    fn numeric_types_use_normalized_membership() {
        // Given: SQL numeric names and misleading names.
        let cases = [
            ("integer", true),
            ("bigint", true),
            ("numeric(10,2)", true),
            ("DOUBLE PRECISION", true),
            ("int unsigned", true),
            ("decimal(8,2) zerofill", true),
            ("text", false),
            ("point", false),
            ("interval", false),
            ("varchar(64)", false),
        ];
        for (name, expected) in cases {
            // When: classifying the name.
            let numeric = is_numeric_type(name);
            // Then: membership, not substrings, decides.
            assert_eq!(numeric, expected, "{name}");
        }
    }
    #[test]
    fn tls_serde_round_trip() {
        // Given: a TLS setting.
        let tls = TlsMode::VerifyFull;
        // When: serializing it.
        let json = serde_json::to_string(&tls).unwrap();
        // Then: snake case survives loading.
        assert_eq!(json, "\"verify_full\"");
        assert_eq!(serde_json::from_str::<TlsMode>(&json).unwrap(), tls);
    }
}
