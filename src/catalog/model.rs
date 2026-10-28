//! Connection catalog model: connections, folders and tree nodes.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::color::{ColorError, Rgb, generated_color};
use crate::db::model::{ConnectParams, Dialect, Secret, ServerParams, SqliteParams, TlsMode};
use crate::persist::Versioned;

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// A fresh random (v4) id.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// The underlying UUID.
            #[must_use]
            pub const fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

id_type!(
    /// Identifier of a saved connection.
    ConnectionId
);
id_type!(
    /// Identifier of a catalog folder.
    FolderId
);

/// Supported database drivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Driver {
    Postgres,
    #[serde(rename = "mysql")]
    MySql,
    Sqlite,
}

impl Driver {
    /// Default TCP port, if the driver uses one.
    #[must_use]
    pub const fn default_port(self) -> Option<u16> {
        match self {
            Self::Postgres => Some(5432),
            Self::MySql => Some(3306),
            Self::Sqlite => None,
        }
    }

    /// Human-readable name.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Postgres => "PostgreSQL",
            Self::MySql => "MySQL / MariaDB",
            Self::Sqlite => "SQLite",
        }
    }

    /// SQL dialect of the driver.
    #[must_use]
    pub const fn dialect(self) -> Dialect {
        match self {
            Self::Postgres => Dialect::Postgres,
            Self::MySql => Dialect::MySql,
            Self::Sqlite => Dialect::Sqlite,
        }
    }
}

/// Where a connection points to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Endpoint {
    Server {
        host: String,
        port: u16,
        user: String,
        #[serde(default)]
        database: Option<String>,
        #[serde(default)]
        tls: TlsMode,
    },
    File {
        path: PathBuf,
    },
}

/// How the password of a connection is obtained. Never the password itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PasswordStorage {
    Keychain,
    Ask,
    None,
}

/// A color serialized as `#RRGGBB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HexColor(Rgb);

impl HexColor {
    #[must_use]
    pub const fn new(rgb: Rgb) -> Self {
        Self(rgb)
    }

    #[must_use]
    pub const fn rgb(self) -> Rgb {
        self.0
    }
}

impl TryFrom<String> for HexColor {
    type Error = ColorError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Rgb::parse_hex(&value).map(Self)
    }
}

impl From<HexColor> for String {
    fn from(value: HexColor) -> Self {
        value.0.to_hex()
    }
}

/// The generated color of a connection and the user's optional override.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionColor {
    pub generated: HexColor,
    pub custom: Option<HexColor>,
}

impl ConnectionColor {
    /// The custom color if set, else the generated one.
    #[must_use]
    pub fn effective(&self) -> Rgb {
        self.custom.unwrap_or(self.generated).rgb()
    }
}

/// A field a [`ValidationIssue`] refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Host,
    Port,
    User,
    Path,
    Driver,
}

/// One problem found by [`ConnectionProfile::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationIssue {
    pub field: Field,
    pub message: String,
}

/// A saved connection. Holds no password, only how to get it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: ConnectionId,
    pub name: String,
    pub driver: Driver,
    pub endpoint: Endpoint,
    pub password: PasswordStorage,
    pub color: ConnectionColor,
}

impl ConnectionProfile {
    /// A profile with a fresh id and a color generated from its seed.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        driver: Driver,
        endpoint: Endpoint,
        password: PasswordStorage,
    ) -> Self {
        let mut profile = Self {
            id: ConnectionId::new(),
            name: name.into(),
            driver,
            endpoint,
            password,
            color: ConnectionColor {
                generated: HexColor::new(Rgb { r: 0, g: 0, b: 0 }),
                custom: None,
            },
        };
        profile.reset_color();
        profile
    }

    /// Seed of the generated color: driver, host and port, or the file path.
    #[must_use]
    pub fn color_seed(&self) -> String {
        match &self.endpoint {
            Endpoint::Server { host, port, .. } => {
                let token = match self.driver {
                    Driver::Postgres => "postgres",
                    Driver::MySql => "mysql",
                    Driver::Sqlite => "sqlite",
                };
                format!("{token}://{}:{port}", host.trim().to_lowercase())
            }
            Endpoint::File { path } => format!("sqlite://{}", path.display()),
        }
    }

    /// Regenerates the color from the current seed and clears the custom one.
    pub fn reset_color(&mut self) {
        self.color = ConnectionColor {
            generated: HexColor::new(generated_color(&self.color_seed())),
            custom: None,
        };
    }

    /// Connection parameters. Call [`Self::validate`] first: on a driver and
    /// endpoint mismatch a server endpoint maps to MySQL unless the driver is
    /// `PostgreSQL`, and a file endpoint to SQLite.
    #[must_use]
    pub fn connect_params(&self, password: Option<Secret>) -> ConnectParams {
        match &self.endpoint {
            Endpoint::Server {
                host,
                port,
                user,
                database,
                tls,
            } => {
                let params = ServerParams {
                    host: host.trim().to_owned(),
                    port: *port,
                    user: user.clone(),
                    password,
                    database: database.clone(),
                    tls: *tls,
                };
                match self.driver {
                    Driver::Postgres => ConnectParams::Postgres(params),
                    Driver::MySql | Driver::Sqlite => ConnectParams::MySql(params),
                }
            }
            Endpoint::File { path } => ConnectParams::Sqlite(SqliteParams { path: path.clone() }),
        }
    }

    /// Checks the profile.
    ///
    /// # Errors
    /// Returns every [`ValidationIssue`] found.
    pub fn validate(&self) -> Result<(), Vec<ValidationIssue>> {
        let mut issues = Vec::new();
        let mut issue = |field, message: &str| {
            issues.push(ValidationIssue {
                field,
                message: message.to_owned(),
            });
        };
        if self.name.trim().is_empty() {
            issue(Field::Name, "name is empty");
        }
        match (&self.endpoint, self.driver) {
            (Endpoint::Server { host, port, .. }, Driver::Postgres | Driver::MySql) => {
                if host.trim().is_empty() {
                    issue(Field::Host, "host is empty");
                }
                if *port == 0 {
                    issue(Field::Port, "port is 0");
                }
            }
            (Endpoint::File { path }, Driver::Sqlite) => {
                if path.as_os_str().is_empty() {
                    issue(Field::Path, "path is empty");
                } else if !path.is_absolute() {
                    issue(Field::Path, "path must be absolute");
                }
            }
            (Endpoint::Server { .. }, Driver::Sqlite)
            | (Endpoint::File { .. }, Driver::Postgres | Driver::MySql) => {
                issue(Field::Driver, "driver does not match the endpoint");
            }
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }
}

/// A folder of the catalog tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub id: FolderId,
    pub name: String,
    pub expanded: bool,
    pub items: Vec<CatalogItem>,
}

impl Folder {
    /// An empty, collapsed folder.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: FolderId::new(),
            name: name.into(),
            expanded: false,
            items: Vec::new(),
        }
    }
}

/// A node of the catalog tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogItem {
    Folder(Folder),
    Connection(ConnectionProfile),
}

/// The persisted catalog document (`connections.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CatalogFile {
    pub version: u32,
    pub items: Vec<CatalogItem>,
}

impl Default for CatalogFile {
    fn default() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            items: Vec::new(),
        }
    }
}

impl Versioned for CatalogFile {
    const CURRENT_VERSION: u32 = 1;
}

impl CatalogItem {
    /// Reference to this node.
    #[must_use]
    pub const fn node_ref(&self) -> crate::catalog::ops::NodeRef {
        match self {
            Self::Folder(folder) => crate::catalog::ops::NodeRef::Folder(folder.id),
            Self::Connection(profile) => crate::catalog::ops::NodeRef::Connection(profile.id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::generated_color;
    use serde_json::json;

    fn server(host: &str, port: u16) -> Endpoint {
        Endpoint::Server {
            host: host.to_owned(),
            port,
            user: "admin".to_owned(),
            database: Some("app".to_owned()),
            tls: TlsMode::Prefer,
        }
    }

    fn file(path: &str) -> Endpoint {
        Endpoint::File {
            path: PathBuf::from(path),
        }
    }

    fn pg(host: &str) -> ConnectionProfile {
        ConnectionProfile::new(
            "Prod",
            Driver::Postgres,
            server(host, 5432),
            PasswordStorage::Keychain,
        )
    }

    fn fields(profile: &ConnectionProfile) -> Vec<Field> {
        profile
            .validate()
            .err()
            .unwrap_or_default()
            .into_iter()
            .map(|issue| issue.field)
            .collect()
    }

    #[test]
    fn json_snapshot_of_nested_catalog() {
        // Given a folder with a PostgreSQL connection and a root SQLite connection
        let mut postgres = pg("db.example.com");
        postgres.id = ConnectionId(Uuid::from_u128(1));
        let mut sqlite = ConnectionProfile::new(
            "Local",
            Driver::Sqlite,
            file("/data/a.db"),
            PasswordStorage::None,
        );
        sqlite.id = ConnectionId(Uuid::from_u128(2));
        let catalog = CatalogFile {
            version: 1,
            items: vec![
                CatalogItem::Folder(Folder {
                    id: FolderId(Uuid::from_u128(3)),
                    name: "Work".to_owned(),
                    expanded: true,
                    items: vec![CatalogItem::Connection(postgres.clone())],
                }),
                CatalogItem::Connection(sqlite.clone()),
            ],
        };
        // When serializing
        let value = serde_json::to_value(&catalog).unwrap();
        // Then the field names and tags are the documented ones
        let pg_hex = generated_color("postgres://db.example.com:5432").to_hex();
        let sqlite_hex = generated_color("sqlite:///data/a.db").to_hex();
        assert_eq!(
            value,
            json!({
                "version": 1,
                "items": [
                    {"folder": {
                        "id": "00000000-0000-0000-0000-000000000003",
                        "name": "Work",
                        "expanded": true,
                        "items": [{"connection": {
                            "id": "00000000-0000-0000-0000-000000000001",
                            "name": "Prod",
                            "driver": "postgres",
                            "endpoint": {"server": {
                                "host": "db.example.com",
                                "port": 5432,
                                "user": "admin",
                                "database": "app",
                                "tls": "prefer"
                            }},
                            "password": "keychain",
                            "color": {"generated": pg_hex, "custom": null}
                        }}]
                    }},
                    {"connection": {
                        "id": "00000000-0000-0000-0000-000000000002",
                        "name": "Local",
                        "driver": "sqlite",
                        "endpoint": {"file": {"path": "/data/a.db"}},
                        "password": "none",
                        "color": {"generated": sqlite_hex, "custom": null}
                    }}
                ]
            })
        );
        // And it round-trips
        let back: CatalogFile = serde_json::from_value(value).unwrap();
        assert_eq!(back, catalog);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // Given a document with extra fields at every level
        let doc = json!({
            "version": 1,
            "future": true,
            "items": [{"folder": {
                "id": "00000000-0000-0000-0000-000000000003",
                "name": "F",
                "expanded": false,
                "extra": 1,
                "items": []
            }}]
        });
        // When parsing
        let parsed: CatalogFile = serde_json::from_value(doc).unwrap();
        // Then the known data is read
        assert_eq!(parsed.items.len(), 1);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        // Given an empty object
        // When parsing
        let parsed: CatalogFile = serde_json::from_value(json!({})).unwrap();
        // Then the current version and no items are used
        assert_eq!(parsed.version, CatalogFile::CURRENT_VERSION);
        assert_eq!(parsed.items.len(), 0);
    }

    #[test]
    fn seed_lowercases_and_trims_host_and_ignores_user_and_database() {
        // Given a mixed-case host with surrounding spaces
        let profile = pg(" DB.Example.com ");
        // When computing the seed
        // Then user and database are excluded
        assert_eq!(profile.color_seed(), "postgres://db.example.com:5432");
    }

    #[test]
    fn mysql_and_sqlite_seeds() {
        // Given a MySQL and a SQLite profile
        let mysql =
            ConnectionProfile::new("m", Driver::MySql, server("h", 3306), PasswordStorage::Ask);
        let sqlite =
            ConnectionProfile::new("s", Driver::Sqlite, file("/x/y.db"), PasswordStorage::None);
        // When computing seeds
        // Then they use their own tokens
        assert_eq!(mysql.color_seed(), "mysql://h:3306");
        assert_eq!(sqlite.color_seed(), "sqlite:///x/y.db");
    }

    #[test]
    fn generated_color_matches_seed() {
        // Given a new profile
        let profile = pg("db.example.com");
        // When reading its generated color
        // Then it equals the color generated from the seed
        assert_eq!(
            profile.color.generated.rgb(),
            generated_color(&profile.color_seed())
        );
        assert_eq!(profile.color.effective(), profile.color.generated.rgb());
    }

    #[test]
    fn editing_endpoint_keeps_color() {
        // Given a profile with a custom color
        let mut profile = pg("a.example.com");
        let before = profile.color.clone();
        profile.color.custom = Some(HexColor::new(Rgb { r: 1, g: 2, b: 3 }));
        // When the host changes
        profile.endpoint = server("b.example.com", 5432);
        // Then the generated color is untouched and the custom one wins
        assert_eq!(profile.color.generated, before.generated);
        assert_eq!(profile.color.effective(), Rgb { r: 1, g: 2, b: 3 });
    }

    #[test]
    fn reset_color_regenerates_and_clears_custom() {
        // Given a profile that moved host and has a custom color
        let mut profile = pg("a.example.com");
        profile.color.custom = Some(HexColor::new(Rgb { r: 1, g: 2, b: 3 }));
        profile.endpoint = server("b.example.com", 5432);
        // When resetting
        profile.reset_color();
        // Then the color follows the new seed and custom is cleared
        assert_eq!(
            profile.color.generated.rgb(),
            generated_color("postgres://b.example.com:5432")
        );
        assert_eq!(profile.color.custom, None);
    }

    #[test]
    fn hex_color_serde_rejects_garbage() {
        // Given an invalid color string
        // When deserializing
        let result = serde_json::from_value::<HexColor>(json!("nope"));
        // Then it fails
        assert!(result.is_err());
        assert_eq!(
            serde_json::to_value(HexColor::new(Rgb {
                r: 255,
                g: 0,
                b: 16
            }))
            .unwrap(),
            json!("#FF0010")
        );
    }

    #[test]
    fn driver_metadata() {
        // Given the drivers
        // When asking for metadata
        // Then it matches the spec
        assert_eq!(Driver::Postgres.default_port(), Some(5432));
        assert_eq!(Driver::MySql.default_port(), Some(3306));
        assert_eq!(Driver::Sqlite.default_port(), None);
        assert_eq!(Driver::MySql.display_name(), "MySQL / MariaDB");
        assert_eq!(Driver::Sqlite.dialect(), Dialect::Sqlite);
        assert_eq!(serde_json::to_value(Driver::MySql).unwrap(), json!("mysql"));
    }

    #[test]
    fn ids_serialize_transparently() {
        // Given an id
        let id = ConnectionId(Uuid::from_u128(7));
        // When serializing
        // Then it is a bare string
        assert_eq!(
            serde_json::to_value(id).unwrap(),
            json!("00000000-0000-0000-0000-000000000007")
        );
        assert_ne!(ConnectionId::new(), ConnectionId::new());
    }

    #[test]
    fn valid_profiles_pass_validation() {
        // Given valid server and file profiles
        // When validating
        // Then no issue is reported
        assert!(pg("h").validate().is_ok());
        let sqlite =
            ConnectionProfile::new("s", Driver::Sqlite, file("/a.db"), PasswordStorage::None);
        assert!(sqlite.validate().is_ok());
    }

    #[test]
    fn validation_reports_name_host_and_port() {
        // Given a profile with empty name, empty host and port 0
        let mut profile = pg("h");
        profile.name = "  ".to_owned();
        profile.endpoint = server(" ", 0);
        // When validating
        // Then each field is reported
        assert_eq!(
            fields(&profile),
            vec![Field::Name, Field::Host, Field::Port]
        );
    }

    #[test]
    fn validation_reports_empty_and_relative_sqlite_paths() {
        // Given SQLite profiles with an empty and a relative path
        let make =
            |path| ConnectionProfile::new("s", Driver::Sqlite, file(path), PasswordStorage::None);
        // When validating
        // Then Path is reported
        assert_eq!(fields(&make("")), vec![Field::Path]);
        assert_eq!(fields(&make("rel/x.db")), vec![Field::Path]);
    }

    #[test]
    fn validation_reports_driver_endpoint_mismatch() {
        // Given a SQLite driver with a server endpoint and vice versa
        let a = ConnectionProfile::new("a", Driver::Sqlite, server("h", 1), PasswordStorage::None);
        let b = ConnectionProfile::new("b", Driver::Postgres, file("/a.db"), PasswordStorage::None);
        // When validating
        // Then Driver is reported
        assert_eq!(fields(&a), vec![Field::Driver]);
        assert_eq!(fields(&b), vec![Field::Driver]);
    }

    #[test]
    fn connect_params_follow_driver_and_carry_password() {
        // Given profiles of each driver
        let secret = Secret::new("pw".to_owned());
        // When building connect params
        let postgres = pg("h").connect_params(Some(secret.clone()));
        let mysql =
            ConnectionProfile::new("m", Driver::MySql, server("h", 3306), PasswordStorage::Ask)
                .connect_params(None);
        let sqlite =
            ConnectionProfile::new("s", Driver::Sqlite, file("/a.db"), PasswordStorage::None)
                .connect_params(None);
        // Then each maps to its variant
        assert!(
            matches!(&postgres, ConnectParams::Postgres(p) if p.password == Some(secret)
            && p.host == "h" && p.port == 5432 && p.user == "admin")
        );
        assert!(matches!(&mysql, ConnectParams::MySql(p) if p.password.is_none()));
        assert_eq!(
            sqlite,
            ConnectParams::Sqlite(SqliteParams {
                path: PathBuf::from("/a.db")
            })
        );
    }

    #[test]
    fn same_absolute_sqlite_path_has_same_seed() {
        // Given two profiles of the same absolute path
        let a = ConnectionProfile::new("a", Driver::Sqlite, file("/d/x.db"), PasswordStorage::None);
        let b = ConnectionProfile::new("b", Driver::Sqlite, file("/d/x.db"), PasswordStorage::None);
        // When comparing seeds and colors
        // Then they agree (no cwd involved)
        assert_eq!(a.color_seed(), b.color_seed());
        assert_eq!(a.color.generated, b.color.generated);
    }
}
