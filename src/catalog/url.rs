//! Database URL import for connections.

use std::path::{Path, PathBuf};

use url::{Host, Url};

use crate::catalog::model::{Driver, Endpoint};
use crate::db::model::{Secret, TlsMode};

/// A connection parsed from a URL. The password never goes into a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlImport {
    pub driver: Driver,
    pub endpoint: Endpoint,
    pub password: Option<Secret>,
    pub suggested_name: String,
}

/// Why a URL could not be imported.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UrlImportError {
    #[error("invalid URL: {0}")]
    Invalid(String),
    #[error("unsupported scheme: {0}")]
    UnknownScheme(String),
    #[error("the URL has no host")]
    MissingHost,
    #[error("the URL has no file path")]
    EmptyPath,
    #[error("unsupported TLS mode: {0}")]
    InvalidTlsMode(String),
}

fn decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        let hex = bytes
            .get(index + 1..index + 3)
            .and_then(|pair| std::str::from_utf8(pair).ok())
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        if let (b'%', Some(value)) = (byte, hex) {
            out.push(value);
            index += 3;
        } else {
            out.push(byte);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_drive_path(path: &str) -> bool {
    matches!(path.as_bytes(), [b'/', letter, b':', b'/' | b'\\', ..] if letter.is_ascii_alphabetic())
}

fn parse_sqlite(rest: &str, cwd: &Path) -> Result<UrlImport, UrlImportError> {
    let raw = rest.strip_prefix("//").unwrap_or(rest);
    let raw = raw.split_once('?').map_or(raw, |(path, _)| path);
    let path = decode(raw);
    let path = if path.is_empty() {
        return Err(UrlImportError::EmptyPath);
    } else if is_drive_path(&path) {
        PathBuf::from(&path[1..])
    } else if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        cwd.join(path)
    };
    let suggested_name = path
        .file_stem()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned();
    Ok(UrlImport {
        driver: Driver::Sqlite,
        endpoint: Endpoint::File { path },
        password: None,
        suggested_name,
    })
}

fn parse_tls(driver: Driver, url: &Url) -> Result<TlsMode, UrlImportError> {
    let key = match driver {
        Driver::MySql => "ssl-mode",
        Driver::Postgres | Driver::Sqlite => "sslmode",
    };
    let Some((_, value)) = url
        .query_pairs()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
    else {
        return Ok(TlsMode::default());
    };
    let normalized = value.to_ascii_lowercase().replace('_', "-");
    match (driver, normalized.as_str()) {
        (Driver::Postgres, "disable") | (Driver::MySql, "disabled") => Ok(TlsMode::Disable),
        (Driver::Postgres, "prefer") | (Driver::MySql, "preferred") => Ok(TlsMode::Prefer),
        (Driver::Postgres, "require") | (Driver::MySql, "required") => Ok(TlsMode::Require),
        (Driver::Postgres | Driver::MySql, "verify-ca") => Ok(TlsMode::VerifyCa),
        (Driver::Postgres, "verify-full") | (Driver::MySql, "verify-identity") => {
            Ok(TlsMode::VerifyFull)
        }
        _ => Err(UrlImportError::InvalidTlsMode(value.into_owned())),
    }
}

fn parse_server(driver: Driver, rest_url: &str) -> Result<UrlImport, UrlImportError> {
    let url = Url::parse(rest_url).map_err(|e| UrlImportError::Invalid(e.to_string()))?;
    let host = match url.host() {
        Some(Host::Domain(domain)) if !domain.is_empty() => decode(domain),
        Some(Host::Ipv4(addr)) => addr.to_string(),
        Some(Host::Ipv6(addr)) => addr.to_string(),
        Some(Host::Domain(_)) | None => return Err(UrlImportError::MissingHost),
    };
    let port = url.port().or(driver.default_port()).unwrap_or_default();
    let database = decode(url.path().trim_start_matches('/'));
    let tls = parse_tls(driver, &url)?;
    Ok(UrlImport {
        driver,
        password: url.password().map(|p| Secret::new(decode(p))),
        suggested_name: host.clone(),
        endpoint: Endpoint::Server {
            host,
            port,
            user: decode(url.username()),
            database: (!database.is_empty()).then_some(database),
            tls,
        },
    })
}

/// Parses a `postgres://`, `mysql://` or `sqlite:` URL. `cwd` resolves relative
/// SQLite paths so that stored paths are always absolute.
///
/// # Errors
/// [`UrlImportError`] for an unsupported scheme, a malformed URL, a missing
/// host or file path, or an unknown TLS mode.
pub fn parse_connection_url(url: &str, cwd: &Path) -> Result<UrlImport, UrlImportError> {
    let url = url.trim();
    let (scheme, rest) = url
        .split_once(':')
        .ok_or_else(|| UrlImportError::Invalid("missing scheme".to_owned()))?;
    match scheme.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" => parse_server(Driver::Postgres, url),
        "mysql" | "mariadb" => parse_server(Driver::MySql, url),
        "sqlite" => parse_sqlite(rest, cwd),
        other => Err(UrlImportError::UnknownScheme(other.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::model::{ConnectionProfile, Field, PasswordStorage};

    const CWD: &str = "/work/dir";

    fn parse(url: &str) -> Result<UrlImport, UrlImportError> {
        parse_connection_url(url, Path::new(CWD))
    }

    fn server_of(import: &UrlImport) -> (&str, u16, &str, Option<&str>, TlsMode) {
        match &import.endpoint {
            Endpoint::Server {
                host,
                port,
                user,
                database,
                tls,
            } => (host, *port, user, database.as_deref(), *tls),
            Endpoint::File { .. } => ("", 0, "", None, TlsMode::Prefer),
        }
    }

    fn path_of(import: &UrlImport) -> PathBuf {
        match &import.endpoint {
            Endpoint::File { path } => path.clone(),
            Endpoint::Server { .. } => PathBuf::new(),
        }
    }

    #[test]
    fn postgres_with_sslmode_require() {
        // Given a full PostgreSQL URL
        let i = parse("postgres://bob:secret@db.example.com:6543/shop?sslmode=require").unwrap();
        // Then every part is parsed
        assert_eq!(i.driver, Driver::Postgres);
        assert_eq!(
            server_of(&i),
            (
                "db.example.com",
                6543,
                "bob",
                Some("shop"),
                TlsMode::Require
            )
        );
        assert_eq!(i.password, Some(Secret::new("secret".to_owned())));
        assert_eq!(i.suggested_name, "db.example.com");
    }

    #[test]
    fn postgresql_alias_and_defaults() {
        // Given the long scheme without port, user or database
        let i = parse("postgresql://localhost").unwrap();
        // Then defaults apply
        assert_eq!(i.driver, Driver::Postgres);
        assert_eq!(
            server_of(&i),
            ("localhost", 5432, "", None, TlsMode::Prefer)
        );
        assert_eq!(i.password, None);
    }

    #[test]
    fn empty_database_is_none() {
        // Given a trailing slash
        let i = parse("postgres://u@h/").unwrap();
        // Then there is no database
        assert_eq!(server_of(&i).3, None);
    }

    #[test]
    fn all_postgres_sslmodes_map() {
        // Given each sslmode
        let cases = [
            ("disable", TlsMode::Disable),
            ("prefer", TlsMode::Prefer),
            ("require", TlsMode::Require),
            ("verify-ca", TlsMode::VerifyCa),
            ("verify-full", TlsMode::VerifyFull),
        ];
        for (value, expected) in cases {
            // When parsing
            let i = parse(&format!("postgres://h/db?sslmode={value}")).unwrap();
            // Then it maps
            assert_eq!(server_of(&i).4, expected, "{value}");
        }
    }

    #[test]
    fn mysql_ssl_mode_verify_identity_is_case_insensitive() {
        // Given MySQL ssl-mode values
        let cases = [
            ("DISABLED", TlsMode::Disable),
            ("preferred", TlsMode::Prefer),
            ("REQUIRED", TlsMode::Require),
            ("Verify_CA", TlsMode::VerifyCa),
            ("VERIFY_IDENTITY", TlsMode::VerifyFull),
        ];
        for (value, expected) in cases {
            // When parsing
            let i = parse(&format!("mysql://h/db?ssl-mode={value}")).unwrap();
            // Then it maps
            assert_eq!(i.driver, Driver::MySql);
            assert_eq!(server_of(&i).4, expected, "{value}");
        }
    }

    #[test]
    fn mariadb_alias_uses_mysql_default_port() {
        // Given a mariadb URL
        let i = parse("mariadb://root@maria.local/app").unwrap();
        // Then it is MySQL on 3306
        assert_eq!(i.driver, Driver::MySql);
        assert_eq!(
            server_of(&i),
            ("maria.local", 3306, "root", Some("app"), TlsMode::Prefer)
        );
    }

    #[test]
    fn percent_encoding_is_decoded() {
        // Given encoded user, password and database
        let i = parse("postgres://us%3Aer:p%40ss@h/my%20db").unwrap();
        // Then they are decoded
        assert_eq!(server_of(&i).2, "us:er");
        assert_eq!(server_of(&i).3, Some("my db"));
        assert_eq!(i.password, Some(Secret::new("p@ss".to_owned())));
    }

    #[test]
    fn ipv6_host_is_kept_without_brackets() {
        // Given an IPv6 host with a port
        let i = parse("postgres://u@[::1]:5433/db").unwrap();
        // Then brackets are dropped
        assert_eq!(server_of(&i).0, "::1");
        assert_eq!(server_of(&i).1, 5433);
    }

    #[test]
    fn unknown_query_keys_are_ignored() {
        // Given unrelated parameters
        let i = parse("postgres://h/db?application_name=x&sslmode=disable").unwrap();
        // Then only sslmode counts
        assert_eq!(server_of(&i).4, TlsMode::Disable);
    }

    #[test]
    fn invalid_tls_value_is_rejected() {
        // Given a bogus sslmode
        // When parsing
        // Then it errors instead of silently downgrading
        assert!(matches!(
            parse("postgres://h/db?sslmode=bogus"),
            Err(UrlImportError::InvalidTlsMode(_))
        ));
    }

    #[test]
    fn sqlite_absolute_triple_slash() {
        // Given sqlite:///abs
        let i = parse("sqlite:///var/data/app.db").unwrap();
        // Then the path is kept
        assert_eq!(i.driver, Driver::Sqlite);
        assert_eq!(path_of(&i), PathBuf::from("/var/data/app.db"));
        assert_eq!(i.suggested_name, "app");
        assert_eq!(i.password, None);
    }

    #[test]
    fn sqlite_relative_forms_resolve_against_injected_cwd() {
        // Given both relative forms
        // When parsing with the injected cwd
        // Then they join it
        assert_eq!(
            path_of(&parse("sqlite://rel.db").unwrap()),
            Path::new(CWD).join("rel.db")
        );
        assert_eq!(
            path_of(&parse("sqlite:rel.db").unwrap()),
            Path::new(CWD).join("rel.db")
        );
        assert_eq!(
            path_of(&parse_connection_url("sqlite:rel.db", Path::new("/other")).unwrap()),
            PathBuf::from("/other/rel.db")
        );
    }

    #[test]
    fn sqlite_drive_letter_form() {
        // Given a Windows path after three slashes
        let i = parse("sqlite:///C:/x.db").unwrap();
        // Then the leading slash is dropped
        assert_eq!(path_of(&i), PathBuf::from("C:/x.db"));
    }

    #[test]
    fn sqlite_empty_path_is_rejected() {
        // Given no path
        // Then it errors
        assert_eq!(parse("sqlite:"), Err(UrlImportError::EmptyPath));
        assert_eq!(parse("sqlite://"), Err(UrlImportError::EmptyPath));
    }

    #[test]
    fn unknown_scheme_is_rejected() {
        // Given an unsupported scheme
        // When parsing
        // Then it is reported with the scheme
        assert_eq!(
            parse("oracle://h/db"),
            Err(UrlImportError::UnknownScheme("oracle".to_owned()))
        );
        assert!(matches!(
            parse("not a url"),
            Err(UrlImportError::Invalid(_) | UrlImportError::UnknownScheme(_))
        ));
    }

    #[test]
    fn missing_host_is_rejected() {
        // Given server URLs without a host
        // Then MissingHost
        assert_eq!(parse("postgres:///db"), Err(UrlImportError::MissingHost));
        assert_eq!(parse("mysql://"), Err(UrlImportError::MissingHost));
    }

    #[test]
    fn relative_sqlite_import_is_absolute_and_validates() {
        // Given a relative SQLite URL
        let i = parse("sqlite:rel.db").unwrap();
        // When building a profile
        let profile = ConnectionProfile::new("x", i.driver, i.endpoint, PasswordStorage::None);
        // Then the path is absolute and validation passes
        assert!(profile.validate().is_ok());
    }

    #[test]
    fn relative_sqlite_path_fails_validation_on_path() {
        // Given a profile whose stored path is relative
        let profile = ConnectionProfile::new(
            "x",
            Driver::Sqlite,
            Endpoint::File {
                path: PathBuf::from("rel.db"),
            },
            PasswordStorage::None,
        );
        // When validating
        let issues = profile.validate().unwrap_err();
        // Then Path is the culprit
        assert_eq!(
            issues.iter().map(|i| i.field).collect::<Vec<_>>(),
            vec![Field::Path]
        );
    }

    #[test]
    fn same_absolute_sqlite_path_has_same_seed_whatever_the_cwd() {
        // Given the same absolute URL parsed under two cwds
        let seed = |cwd: &str| {
            let i = parse_connection_url("sqlite:///data/a.db", Path::new(cwd)).unwrap();
            ConnectionProfile::new("x", i.driver, i.endpoint, PasswordStorage::None).color_seed()
        };
        // When comparing seeds
        // Then they are equal
        assert_eq!(seed("/one"), seed("/two"));
    }
}
