//! Sidebar tree nodes: connections, folders and lazy schema nodes.

use std::collections::HashSet;
use std::fmt;
use std::hash::{BuildHasher, Hash, Hasher};

use gpui_kit::SharedString;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use crate::catalog::model::{CatalogItem, ConnectionId, ConnectionProfile, FolderId};
use crate::catalog::store::ConnectionStore;
use crate::db::model::{ObjectKind, ObjectNode, ObjectPath, ObjectRef, TableKind};
use crate::sessions::{ChildrenState, ConnectionSessions};

/// Why a placeholder row stands in for children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlaceholderKind {
    Loading,
    Error,
    Empty,
}

impl PlaceholderKind {
    const fn tag(self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Error => "error",
            Self::Empty => "empty",
        }
    }
}

/// A database object below a connection: its ancestors and its own name.
///
/// A database has no ancestors; a schema may have a database; tables and views
/// may have both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectNodeKey {
    pub kind: ObjectKind,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub name: String,
}

impl Hash for ObjectNodeKey {
    // `ObjectKind` does not implement `Hash`, so hash its variant by name.
    fn hash<H: Hasher>(&self, state: &mut H) {
        let kind: u8 = match self.kind {
            ObjectKind::Database => 0,
            ObjectKind::Schema => 1,
            ObjectKind::Table => 2,
            ObjectKind::View => 3,
        };
        (kind, &self.database, &self.schema, &self.name).hash(state);
    }
}

impl ObjectNodeKey {
    /// The key of `node` listed under `parent`.
    #[must_use]
    pub fn of(node: &ObjectNode, parent: &ObjectPath) -> Self {
        let (database, schema) = match parent {
            ObjectPath::Root => (None, None),
            ObjectPath::Database { name } => (Some(name.clone()), None),
            ObjectPath::Schema { database, name } => (database.clone(), Some(name.clone())),
        };
        Self {
            kind: node.kind,
            database,
            schema,
            name: node.name.clone(),
        }
    }

    /// The catalog branch below this node; `None` for tables and views.
    #[must_use]
    pub fn children_path(&self) -> Option<ObjectPath> {
        match self.kind {
            ObjectKind::Database => Some(ObjectPath::Database {
                name: self.name.clone(),
            }),
            ObjectKind::Schema => Some(ObjectPath::Schema {
                database: self.database.clone(),
                name: self.name.clone(),
            }),
            ObjectKind::Table | ObjectKind::View => None,
        }
    }

    /// The branch listing this node's siblings.
    #[must_use]
    pub fn parent_path(&self) -> ObjectPath {
        match (&self.database, &self.schema) {
            (_, Some(schema)) => ObjectPath::Schema {
                database: self.database.clone(),
                name: schema.clone(),
            },
            (Some(database), None) => ObjectPath::Database {
                name: database.clone(),
            },
            (None, None) => ObjectPath::Root,
        }
    }

    /// The table or view this node stands for.
    #[must_use]
    pub fn object_ref(&self) -> Option<ObjectRef> {
        let kind = match self.kind {
            ObjectKind::Table => TableKind::Table,
            ObjectKind::View => TableKind::View,
            ObjectKind::Database | ObjectKind::Schema => return None,
        };
        Some(ObjectRef {
            database: self.database.clone(),
            schema: self.schema.clone(),
            name: self.name.clone(),
            kind,
        })
    }

    /// The database a new query on this node targets.
    #[must_use]
    pub fn query_database(&self) -> Option<String> {
        match self.kind {
            ObjectKind::Database => Some(self.name.clone()),
            ObjectKind::Schema | ObjectKind::Table | ObjectKind::View => self.database.clone(),
        }
    }
}

/// Stable identity of a sidebar row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SidebarNodeKey {
    Folder(FolderId),
    Connection(ConnectionId),
    Object {
        connection: ConnectionId,
        node: ObjectNodeKey,
    },
    Placeholder {
        connection: ConnectionId,
        parent: ObjectPath,
        kind: PlaceholderKind,
    },
}

fn encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '/' => out.push_str("%2F"),
            ':' => out.push_str("%3A"),
            other => out.push(other),
        }
    }
    out
}

fn decode(text: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut input = text.bytes();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let hi = input.next()?;
            let lo = input.next()?;
            let hex = std::str::from_utf8(&[hi, lo]).ok()?.to_owned();
            bytes.push(u8::from_str_radix(&hex, 16).ok()?);
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).ok()
}

fn parse_id<T: DeserializeOwned>(text: &str) -> Option<T> {
    let uuid = Uuid::parse_str(text).ok()?;
    serde_json::from_value(serde_json::Value::String(uuid.to_string())).ok()
}

fn path_segments(path: &ObjectPath) -> String {
    match path {
        ObjectPath::Root => String::new(),
        ObjectPath::Database { name } => format!("/db:{}", encode(name)),
        ObjectPath::Schema { database, name } => {
            let database = database
                .as_ref()
                .map_or_else(String::new, |d| format!("/db:{}", encode(d)));
            format!("{database}/schema:{}", encode(name))
        }
    }
}

impl fmt::Display for SidebarNodeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Folder(id) => write!(f, "folder:{}", id.as_uuid()),
            Self::Connection(id) => write!(f, "conn:{}", id.as_uuid()),
            Self::Object { connection, node } => {
                let leaf = match node.kind {
                    ObjectKind::Database => "db",
                    ObjectKind::Schema => "schema",
                    ObjectKind::Table => "table",
                    ObjectKind::View => "view",
                };
                write!(f, "conn:{}", connection.as_uuid())?;
                if node.kind != ObjectKind::Database
                    && let Some(database) = &node.database
                {
                    write!(f, "/db:{}", encode(database))?;
                }
                if let (Some(schema), ObjectKind::Table | ObjectKind::View) =
                    (&node.schema, node.kind)
                {
                    write!(f, "/schema:{}", encode(schema))?;
                }
                write!(f, "/{leaf}:{}", encode(&node.name))
            }
            Self::Placeholder {
                connection,
                parent,
                kind,
            } => write!(
                f,
                "conn:{}{}/{}",
                connection.as_uuid(),
                path_segments(parent),
                kind.tag()
            ),
        }
    }
}

impl SidebarNodeKey {
    /// Parses the string form produced by `Display`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        if let Some(id) = text.strip_prefix("folder:") {
            return parse_id(id).map(Self::Folder);
        }
        let rest = text.strip_prefix("conn:")?;
        let mut segments = rest.split('/');
        let connection: ConnectionId = parse_id(segments.next()?)?;
        let rest: Vec<&str> = segments.collect();
        let Some((last, ancestors)) = rest.split_last() else {
            return Some(Self::Connection(connection));
        };
        let mut database = None;
        let mut schema = None;
        for segment in ancestors {
            let (tag, name) = segment.split_once(':')?;
            match tag {
                "db" => database = Some(decode(name)?),
                "schema" => schema = Some(decode(name)?),
                _ => return None,
            }
        }
        let placeholder = [
            PlaceholderKind::Loading,
            PlaceholderKind::Error,
            PlaceholderKind::Empty,
        ]
        .into_iter()
        .find(|kind| kind.tag() == *last);
        if let Some(kind) = placeholder {
            let parent = match (database, schema) {
                (None, None) => ObjectPath::Root,
                (Some(name), None) => ObjectPath::Database { name },
                (database, Some(name)) => ObjectPath::Schema { database, name },
            };
            return Some(Self::Placeholder {
                connection,
                parent,
                kind,
            });
        }
        let (tag, name) = last.split_once(':')?;
        let kind = match tag {
            "db" => ObjectKind::Database,
            "schema" => ObjectKind::Schema,
            "table" => ObjectKind::Table,
            "view" => ObjectKind::View,
            _ => return None,
        };
        Some(Self::Object {
            connection,
            node: ObjectNodeKey {
                kind,
                database,
                schema,
                name: decode(name)?,
            },
        })
    }

    /// The connection this node belongs to, if any.
    #[must_use]
    pub const fn connection(&self) -> Option<ConnectionId> {
        match self {
            Self::Folder(_) => None,
            Self::Connection(connection)
            | Self::Object { connection, .. }
            | Self::Placeholder { connection, .. } => Some(*connection),
        }
    }
}

/// What a row shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowKind {
    Folder,
    Connection,
    Database,
    Schema,
    Table,
    View,
    Placeholder(PlaceholderKind),
}

/// One visible line of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub key: SidebarNodeKey,
    pub depth: usize,
    pub label: SharedString,
    pub kind: RowKind,
    pub expandable: bool,
    pub expanded: bool,
}

struct Flattener<'a, S> {
    sessions: &'a ConnectionSessions,
    expanded: &'a HashSet<SidebarNodeKey, S>,
    rows: Vec<Row>,
}

impl<S: BuildHasher> Flattener<'_, S> {
    fn placeholder(
        &mut self,
        connection: ConnectionId,
        parent: &ObjectPath,
        kind: PlaceholderKind,
        label: String,
        depth: usize,
    ) {
        self.rows.push(Row {
            key: SidebarNodeKey::Placeholder {
                connection,
                parent: parent.clone(),
                kind,
            },
            depth,
            label: label.into(),
            kind: RowKind::Placeholder(kind),
            expandable: false,
            expanded: false,
        });
    }

    /// The rows below an expanded node whose children live at `path`.
    fn children(&mut self, connection: ConnectionId, path: &ObjectPath, depth: usize) {
        match self.sessions.children(connection, path) {
            None | Some(ChildrenState::Loading) => {
                self.placeholder(
                    connection,
                    path,
                    PlaceholderKind::Loading,
                    String::new(),
                    depth,
                );
            }
            Some(ChildrenState::Failed(error)) => {
                self.placeholder(
                    connection,
                    path,
                    PlaceholderKind::Error,
                    error.to_string(),
                    depth,
                );
            }
            Some(ChildrenState::Loaded(nodes)) if nodes.is_empty() => {
                self.placeholder(
                    connection,
                    path,
                    PlaceholderKind::Empty,
                    String::new(),
                    depth,
                );
            }
            Some(ChildrenState::Loaded(nodes)) => {
                for node in nodes.iter() {
                    self.object(connection, path, node, depth);
                }
            }
        }
    }

    fn object(
        &mut self,
        connection: ConnectionId,
        parent: &ObjectPath,
        node: &ObjectNode,
        depth: usize,
    ) {
        let key = ObjectNodeKey::of(node, parent);
        let kind = match node.kind {
            ObjectKind::Database => RowKind::Database,
            ObjectKind::Schema => RowKind::Schema,
            ObjectKind::Table => RowKind::Table,
            ObjectKind::View => RowKind::View,
        };
        let path = key.children_path();
        let sidebar_key = SidebarNodeKey::Object {
            connection,
            node: key,
        };
        let expanded = path.is_some() && self.expanded.contains(&sidebar_key);
        self.rows.push(Row {
            key: sidebar_key,
            depth,
            label: node.name.clone().into(),
            kind,
            expandable: path.is_some(),
            expanded,
        });
        if let (true, Some(path)) = (expanded, path) {
            self.children(connection, &path, depth + 1);
        }
    }

    fn connection(&mut self, profile: &ConnectionProfile, depth: usize) {
        let key = SidebarNodeKey::Connection(profile.id);
        let expanded = self.expanded.contains(&key);
        self.rows.push(Row {
            key,
            depth,
            label: profile.name.clone().into(),
            kind: RowKind::Connection,
            expandable: true,
            expanded,
        });
        if expanded {
            self.children(profile.id, &ObjectPath::Root, depth + 1);
        }
    }

    fn items(&mut self, items: &[CatalogItem], depth: usize, only: Option<ConnectionId>) {
        for item in items {
            match item {
                CatalogItem::Connection(profile) if only.is_none_or(|id| id == profile.id) => {
                    self.connection(profile, depth);
                }
                CatalogItem::Connection(_) => {}
                CatalogItem::Folder(folder) if only.is_none() => {
                    self.rows.push(Row {
                        key: SidebarNodeKey::Folder(folder.id),
                        depth,
                        label: folder.name.clone().into(),
                        kind: RowKind::Folder,
                        expandable: true,
                        expanded: folder.expanded,
                    });
                    if folder.expanded {
                        self.items(&folder.items, depth + 1, None);
                    }
                }
                CatalogItem::Folder(folder) => self.items(&folder.items, depth, only),
            }
        }
    }
}

/// The visible rows: folders use their persisted `expanded` flag, connections
/// and database or schema nodes the per-view `expanded` set. With a filter,
/// only that connection is listed, at depth 0.
#[must_use]
pub fn flatten<S: BuildHasher>(
    store: &ConnectionStore,
    sessions: &ConnectionSessions,
    expanded: &HashSet<SidebarNodeKey, S>,
    connection_id_filter: Option<ConnectionId>,
) -> Vec<Row> {
    let mut flattener = Flattener {
        sessions,
        expanded,
        rows: Vec::new(),
    };
    flattener.items(store.items(), 0, connection_id_filter);
    flattener.rows
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use gpui_kit::TestAppContext;

    use super::{ObjectNodeKey, PlaceholderKind, RowKind, SidebarNodeKey, flatten};
    use crate::catalog::model::{
        ConnectionId, ConnectionProfile, Driver, Endpoint, FolderId, PasswordStorage,
    };
    use crate::catalog::ops::DropPosition;
    use crate::catalog::store::{GlobalConnectionStore, init};
    use crate::db::model::{ObjectKind, ObjectPath, TlsMode};
    use crate::paths::ConfigPaths;
    use crate::sessions::ConnectionSessions;

    fn object(kind: ObjectKind, database: Option<&str>, name: &str) -> ObjectNodeKey {
        ObjectNodeKey {
            kind,
            database: database.map(str::to_owned),
            schema: None,
            name: name.to_owned(),
        }
    }

    #[test]
    fn keys_round_trip_with_reserved_characters() {
        // Given: keys whose names contain the reserved characters.
        let connection = ConnectionId::new();
        let tricky = "we/ird:na%me";
        let keys = [
            SidebarNodeKey::Folder(FolderId::new()),
            SidebarNodeKey::Connection(connection),
            SidebarNodeKey::Object {
                connection,
                node: object(ObjectKind::Database, None, tricky),
            },
            SidebarNodeKey::Object {
                connection,
                node: ObjectNodeKey {
                    kind: ObjectKind::Table,
                    database: Some(tricky.to_owned()),
                    schema: Some("pub/lic".to_owned()),
                    name: "t:1".to_owned(),
                },
            },
            SidebarNodeKey::Placeholder {
                connection,
                parent: ObjectPath::Schema {
                    database: Some("a%".to_owned()),
                    name: "s/1".to_owned(),
                },
                kind: PlaceholderKind::Error,
            },
            SidebarNodeKey::Placeholder {
                connection,
                parent: ObjectPath::Root,
                kind: PlaceholderKind::Loading,
            },
        ];
        for key in keys {
            // When: formatting then parsing the key.
            let text = key.to_string();
            // Then: the same key comes back.
            assert_eq!(SidebarNodeKey::parse(&text), Some(key), "{text}");
        }
    }

    #[test]
    fn string_forms_follow_the_documented_shape() {
        // Given: a table under a database and schema.
        let connection = ConnectionId::new();
        let key = SidebarNodeKey::Object {
            connection,
            node: ObjectNodeKey {
                kind: ObjectKind::View,
                database: Some("a/b".to_owned()),
                schema: Some("public".to_owned()),
                name: "v".to_owned(),
            },
        };
        // When: formatting.
        let text = key.to_string();
        // Then: it is the percent-encoded path.
        assert_eq!(
            text,
            format!(
                "conn:{}/db:a%2Fb/schema:public/view:v",
                connection.as_uuid()
            )
        );
    }

    #[test]
    fn malformed_keys_do_not_parse() {
        for text in ["", "folder:nope", "conn:", "x:1", "conn:not-a-uuid/db:a"] {
            assert_eq!(SidebarNodeKey::parse(text), None, "{text}");
        }
    }

    fn profile(name: &str) -> ConnectionProfile {
        ConnectionProfile::new(
            name,
            Driver::Postgres,
            Endpoint::Server {
                host: "db.test".into(),
                port: 5432,
                user: "u".into(),
                database: None,
                tls: TlsMode::default(),
            },
            PasswordStorage::Ask,
        )
    }

    #[gpui_kit::test]
    fn flatten_lists_nested_folders_in_catalog_order(cx: &mut TestAppContext) {
        // Given: root connection `a`, folder F (expanded) holding folder G (collapsed) and `c`.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = ConfigPaths::at(dir.path().to_path_buf());
        let rows = cx.update(|cx| {
            init(cx, &paths);
            let store = cx.global::<GlobalConnectionStore>().0.clone();
            store.update(cx, |s, cx| {
                s.add_connection(cx, profile("a"), DropPosition::RootEnd)
                    .expect("a");
                let f = s.add_folder(cx, "F", DropPosition::RootEnd).expect("F");
                s.set_folder_expanded(cx, f, true).expect("expand");
                let g = s.add_folder(cx, "G", DropPosition::Into(f)).expect("G");
                s.add_connection(cx, profile("hidden"), DropPosition::Into(g))
                    .expect("hidden");
                s.add_connection(cx, profile("c"), DropPosition::Into(f))
                    .expect("c");
            });
            flatten(
                store.read(cx),
                &ConnectionSessions::default(),
                &HashSet::new(),
                None,
            )
        });
        // Then: depth-first order with depth, collapsed contents hidden.
        let shape: Vec<_> = rows
            .iter()
            .map(|r| {
                (
                    r.label.to_string(),
                    r.depth,
                    r.kind,
                    r.expandable,
                    r.expanded,
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                ("a".to_owned(), 0, RowKind::Connection, true, false),
                ("F".to_owned(), 0, RowKind::Folder, true, true),
                ("G".to_owned(), 1, RowKind::Folder, true, false),
                ("c".to_owned(), 1, RowKind::Connection, true, false),
            ]
        );
    }

    #[gpui_kit::test]
    fn expanded_connection_without_data_shows_a_loading_row(cx: &mut TestAppContext) {
        // Given: one connection that the view expanded before anything loaded.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = ConfigPaths::at(dir.path().to_path_buf());
        let (rows, id) = cx.update(|cx| {
            init(cx, &paths);
            let store = cx.global::<GlobalConnectionStore>().0.clone();
            let p = profile("a");
            let id = p.id;
            store.update(cx, |s, cx| {
                s.add_connection(cx, p, DropPosition::RootEnd).expect("a");
            });
            let expanded = HashSet::from([SidebarNodeKey::Connection(id)]);
            (
                flatten(
                    store.read(cx),
                    &ConnectionSessions::default(),
                    &expanded,
                    None,
                ),
                id,
            )
        });
        // Then: the connection is followed by one Loading placeholder one level deeper.
        assert_eq!(rows.len(), 2);
        assert!(rows[0].expanded);
        assert_eq!(rows[1].kind, RowKind::Placeholder(PlaceholderKind::Loading));
        assert_eq!(rows[1].depth, 1);
        assert_eq!(
            rows[1].key,
            SidebarNodeKey::Placeholder {
                connection: id,
                parent: ObjectPath::Root,
                kind: PlaceholderKind::Loading,
            }
        );
    }

    #[gpui_kit::test]
    fn connection_filter_keeps_only_that_connection(cx: &mut TestAppContext) {
        // Given: two connections.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = ConfigPaths::at(dir.path().to_path_buf());
        let (rows, id) = cx.update(|cx| {
            init(cx, &paths);
            let store = cx.global::<GlobalConnectionStore>().0.clone();
            let (a, b) = (profile("a"), profile("b"));
            let id = b.id;
            store.update(cx, |s, cx| {
                s.add_connection(cx, a, DropPosition::RootEnd).expect("a");
                s.add_connection(cx, b, DropPosition::RootEnd).expect("b");
            });
            (
                flatten(
                    store.read(cx),
                    &ConnectionSessions::default(),
                    &HashSet::new(),
                    Some(id),
                ),
                id,
            )
        });
        // Then: only `b` is listed.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, SidebarNodeKey::Connection(id));
    }
}
