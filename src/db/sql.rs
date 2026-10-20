//! SQL builder for the generated statements.

use super::model::{Dialect, ObjectRef, PageRequest, SortDirection};

#[must_use]
pub fn quote_ident(dialect: Dialect, name: &str) -> String {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!("\"{}\"", name.replace('"', "\"\"")),
        Dialect::MySql => format!("`{}`", name.replace('`', "``")),
    }
}
#[must_use]
pub fn qualified_name(dialect: Dialect, object: &ObjectRef) -> String {
    let qualifier = match dialect {
        Dialect::Postgres => object.schema.as_ref(),
        Dialect::MySql => object.database.as_ref(),
        Dialect::Sqlite => None,
    };
    let name = quote_ident(dialect, &object.name);
    qualifier.map_or_else(
        || name.clone(),
        |qualifier| format!("{}.{name}", quote_ident(dialect, qualifier)),
    )
}
#[must_use]
pub fn select_page_sql(dialect: Dialect, request: &PageRequest) -> String {
    let order = request
        .order_by
        .as_ref()
        .map_or_else(String::new, |(column, direction)| {
            let direction = match direction {
                SortDirection::Ascending => "ASC",
                SortDirection::Descending => "DESC",
            };
            format!(" ORDER BY {} {direction}", quote_ident(dialect, column))
        });
    format!(
        "SELECT * FROM {}{order} LIMIT {} OFFSET {}",
        qualified_name(dialect, &request.object),
        u64::from(request.limit) + 1,
        request.offset
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> PageRequest {
        PageRequest {
            object: ObjectRef {
                database: Some("app".into()),
                schema: Some("public".into()),
                name: "user data".into(),
                kind: super::super::model::TableKind::Table,
            },
            order_by: None,
            offset: 400,
            limit: 200,
        }
    }
    #[test]
    fn quotes_postgres_identifiers() {
        // Given: embedded delimiters, spaces and Unicode.
        let name = "a\" bé";
        // When: quoting for PostgreSQL.
        let quoted = quote_ident(Dialect::Postgres, name);
        // Then: delimiters are doubled.
        assert_eq!(quoted, "\"a\"\" bé\"");
    }
    #[test]
    fn quotes_mysql_identifiers() {
        // Given: a backtick in an identifier.
        let name = "a` bé";
        // When: quoting for MySQL.
        let quoted = quote_ident(Dialect::MySql, name);
        // Then: backticks are doubled.
        assert_eq!(quoted, "`a`` bé`");
    }
    #[test]
    fn quotes_sqlite_identifiers() {
        // Given: a double quote in an identifier.
        let name = "a\" bé";
        // When: quoting for SQLite.
        let quoted = quote_ident(Dialect::Sqlite, name);
        // Then: quotes are doubled.
        assert_eq!(quoted, "\"a\"\" bé\"");
    }
    #[test]
    fn page_sql_without_order_for_each_dialect() {
        // Given: a page request.
        let request = request();
        for (dialect, qualified) in [
            (Dialect::Postgres, "\"public\".\"user data\""),
            (Dialect::MySql, "`app`.`user data`"),
            (Dialect::Sqlite, "\"user data\""),
        ] {
            // When: generating a page statement.
            let sql = select_page_sql(dialect, &request);
            // Then: qualification and the extra row are exact.
            assert_eq!(
                sql,
                format!("SELECT * FROM {qualified} LIMIT 201 OFFSET 400")
            );
        }
    }
    #[test]
    fn page_sql_with_order_for_each_dialect() {
        // Given: an ordered request.
        let request = PageRequest {
            order_by: Some(("name".into(), SortDirection::Descending)),
            ..request()
        };
        for (dialect, qualified, column) in [
            (Dialect::Postgres, "\"public\".\"user data\"", "\"name\""),
            (Dialect::MySql, "`app`.`user data`", "`name`"),
            (Dialect::Sqlite, "\"user data\"", "\"name\""),
        ] {
            // When: generating the statement.
            let sql = select_page_sql(dialect, &request);
            // Then: the sort is server-side.
            assert_eq!(
                sql,
                format!("SELECT * FROM {qualified} ORDER BY {column} DESC LIMIT 201 OFFSET 400")
            );
        }
    }
    #[test]
    fn maximum_limit_does_not_overflow() {
        // Given: the largest possible limit and ascending order.
        let request = PageRequest {
            limit: u32::MAX,
            order_by: Some(("id".into(), SortDirection::Ascending)),
            ..request()
        };
        // When: generating SQL.
        let sql = select_page_sql(Dialect::Sqlite, &request);
        // Then: the extra row is represented without overflow.
        assert_eq!(
            sql,
            "SELECT * FROM \"user data\" ORDER BY \"id\" ASC LIMIT 4294967296 OFFSET 400"
        );
    }
}
