//! Connection store: persistence of the connection catalog.

use std::io;
use std::path::PathBuf;

use gpui_kit::prelude::*;
use gpui_kit::{App, Context, Entity, EventEmitter, Global};

use crate::catalog::model::{
    CatalogFile, CatalogItem, ConnectionId, ConnectionProfile, Folder, FolderId, HexColor,
};
use crate::catalog::ops::{self, DropPosition, NodeRef, OpsError};
use crate::paths::ConfigPaths;
use crate::persist::{self, LoadOutcome};
use crate::settings::LoadNotice;

/// Change notification emitted by [`ConnectionStore`]. Every successful
/// mutation emits its specific events, then [`Self::Changed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStoreEvent {
    Changed,
    ConnectionAdded(ConnectionId),
    ConnectionUpdated {
        id: ConnectionId,
        /// Driver, endpoint or password storage differ from the old profile.
        endpoint_changed: bool,
    },
    ConnectionRemoved(ConnectionId),
    ColorChanged(ConnectionId),
}

impl EventEmitter<ConnectionStoreEvent> for ConnectionStore {}

/// Failures returned by store mutations.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("connections are read-only because the file is unusable or uses a newer version")]
    ReadOnly,
    #[error(transparent)]
    Ops(#[from] OpsError),
    #[error("could not save connections: {0}")]
    Io(#[from] io::Error),
}

/// GPUI entity holding the connection catalog and its persistence state.
pub struct ConnectionStore {
    file: PathBuf,
    catalog: CatalogFile,
    read_only: bool,
    load_notice: Option<LoadNotice>,
}

/// GPUI global handle to the connection store.
pub struct GlobalConnectionStore(pub Entity<ConnectionStore>);

impl Global for GlobalConnectionStore {}

/// Result of applying an operation: the value, whether the tree changed, and
/// the specific events to emit.
type Applied<T> = Result<(T, bool, Vec<ConnectionStoreEvent>), OpsError>;

impl ConnectionStore {
    /// The catalog tree in display order.
    #[must_use]
    pub fn items(&self) -> &[CatalogItem] {
        &self.catalog.items
    }

    /// The connection with this id, if any.
    #[must_use]
    pub fn profile(&self, id: ConnectionId) -> Option<&ConnectionProfile> {
        ops::get_profile(&self.catalog.items, id)
    }

    /// Whether the file cannot be written (newer version or unreadable).
    #[must_use]
    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Takes the pending load notice, if any.
    #[must_use]
    pub fn take_load_notice(&mut self) -> Option<LoadNotice> {
        self.load_notice.take()
    }

    /// Applies `apply`, saves, then emits its events followed by `Changed`.
    /// A save failure keeps the in-memory change and is returned after emitting.
    fn mutate<T>(
        &mut self,
        cx: &mut Context<Self>,
        apply: impl FnOnce(&mut Vec<CatalogItem>) -> Applied<T>,
    ) -> Result<T, StoreError> {
        if self.read_only {
            return Err(StoreError::ReadOnly);
        }
        let (value, changed, events) = apply(&mut self.catalog.items)?;
        if !changed {
            return Ok(value);
        }
        let saved = persist::save_json(&self.file, &self.catalog);
        for event in events {
            cx.emit(event);
        }
        cx.emit(ConnectionStoreEvent::Changed);
        saved?;
        Ok(value)
    }

    /// Adds a connection at `position`.
    ///
    /// # Errors
    /// [`StoreError::ReadOnly`], [`StoreError::Ops`] for an unknown position,
    /// or [`StoreError::Io`] when saving fails.
    pub fn add_connection(
        &mut self,
        cx: &mut Context<Self>,
        profile: ConnectionProfile,
        position: DropPosition,
    ) -> Result<ConnectionId, StoreError> {
        let id = profile.id;
        self.mutate(cx, |items| {
            ops::insert(items, CatalogItem::Connection(profile), position)?;
            Ok((id, true, vec![ConnectionStoreEvent::ConnectionAdded(id)]))
        })
    }

    /// Replaces the connection with the same id.
    ///
    /// # Errors
    /// [`StoreError::ReadOnly`], [`StoreError::Ops`] for an unknown
    /// connection, or [`StoreError::Io`] when saving fails.
    pub fn update_connection(
        &mut self,
        cx: &mut Context<Self>,
        profile: ConnectionProfile,
    ) -> Result<(), StoreError> {
        let id = profile.id;
        self.mutate(cx, |items| {
            let current = ops::get_profile_mut(items, id).ok_or(OpsError::NotFound)?;
            let endpoint_changed = current.driver != profile.driver
                || current.endpoint != profile.endpoint
                || current.password != profile.password;
            *current = profile;
            Ok((
                (),
                true,
                vec![ConnectionStoreEvent::ConnectionUpdated {
                    id,
                    endpoint_changed,
                }],
            ))
        })
    }

    /// Sets or clears the custom color of a connection.
    ///
    /// # Errors
    /// [`StoreError::ReadOnly`], [`StoreError::Ops`] for an unknown
    /// connection, or [`StoreError::Io`] when saving fails.
    pub fn set_custom_color(
        &mut self,
        cx: &mut Context<Self>,
        id: ConnectionId,
        color: Option<HexColor>,
    ) -> Result<(), StoreError> {
        self.mutate(cx, |items| {
            ops::get_profile_mut(items, id)
                .ok_or(OpsError::NotFound)?
                .color
                .custom = color;
            Ok(((), true, vec![ConnectionStoreEvent::ColorChanged(id)]))
        })
    }

    /// Regenerates the color of a connection and clears the custom one.
    ///
    /// # Errors
    /// [`StoreError::ReadOnly`], [`StoreError::Ops`] for an unknown
    /// connection, or [`StoreError::Io`] when saving fails.
    pub fn reset_color(
        &mut self,
        cx: &mut Context<Self>,
        id: ConnectionId,
    ) -> Result<(), StoreError> {
        self.mutate(cx, |items| {
            ops::get_profile_mut(items, id)
                .ok_or(OpsError::NotFound)?
                .reset_color();
            Ok(((), true, vec![ConnectionStoreEvent::ColorChanged(id)]))
        })
    }

    /// Adds an empty, collapsed folder at `position`.
    ///
    /// # Errors
    /// [`StoreError::ReadOnly`], [`StoreError::Ops`] for an unknown position,
    /// or [`StoreError::Io`] when saving fails.
    pub fn add_folder(
        &mut self,
        cx: &mut Context<Self>,
        name: impl Into<String>,
        position: DropPosition,
    ) -> Result<FolderId, StoreError> {
        let folder = Folder::new(name);
        let id = folder.id;
        self.mutate(cx, |items| {
            ops::insert(items, CatalogItem::Folder(folder), position)?;
            Ok((id, true, vec![]))
        })
    }

    /// Renames a folder.
    ///
    /// # Errors
    /// [`StoreError::ReadOnly`], [`StoreError::Ops`] for an unknown folder, or
    /// [`StoreError::Io`] when saving fails.
    pub fn rename_folder(
        &mut self,
        cx: &mut Context<Self>,
        id: FolderId,
        name: impl Into<String>,
    ) -> Result<(), StoreError> {
        self.mutate(cx, |items| {
            ops::rename_folder(items, id, name)?;
            Ok(((), true, vec![]))
        })
    }

    /// Expands or collapses a folder.
    ///
    /// # Errors
    /// [`StoreError::ReadOnly`], [`StoreError::Ops`] for an unknown folder, or
    /// [`StoreError::Io`] when saving fails.
    pub fn set_folder_expanded(
        &mut self,
        cx: &mut Context<Self>,
        id: FolderId,
        expanded: bool,
    ) -> Result<(), StoreError> {
        self.mutate(cx, |items| {
            ops::set_folder_expanded(items, id, expanded)?;
            Ok(((), true, vec![]))
        })
    }

    /// Moves a node to `position`.
    ///
    /// # Errors
    /// [`StoreError::ReadOnly`], [`StoreError::Ops`] for an unknown node or a
    /// folder moved into itself, or [`StoreError::Io`] when saving fails.
    pub fn move_node(
        &mut self,
        cx: &mut Context<Self>,
        node: NodeRef,
        position: DropPosition,
    ) -> Result<(), StoreError> {
        self.mutate(cx, |items| {
            ops::move_node(items, node, position)?;
            Ok(((), true, vec![]))
        })
    }

    /// Swaps a node with its previous sibling; `Ok(false)` at the top edge.
    ///
    /// # Errors
    /// [`StoreError::ReadOnly`], [`StoreError::Ops`] for an unknown node, or
    /// [`StoreError::Io`] when saving fails.
    pub fn move_up(&mut self, cx: &mut Context<Self>, node: NodeRef) -> Result<bool, StoreError> {
        self.mutate(cx, |items| {
            let moved = ops::move_up(items, node)?;
            Ok((moved, moved, vec![]))
        })
    }

    /// Swaps a node with its next sibling; `Ok(false)` at the bottom edge.
    ///
    /// # Errors
    /// [`StoreError::ReadOnly`], [`StoreError::Ops`] for an unknown node, or
    /// [`StoreError::Io`] when saving fails.
    pub fn move_down(&mut self, cx: &mut Context<Self>, node: NodeRef) -> Result<bool, StoreError> {
        self.mutate(cx, |items| {
            let moved = ops::move_down(items, node)?;
            Ok((moved, moved, vec![]))
        })
    }

    /// Duplicates a connection right after the original.
    ///
    /// # Errors
    /// [`StoreError::ReadOnly`], [`StoreError::Ops`] for an unknown
    /// connection, or [`StoreError::Io`] when saving fails.
    pub fn duplicate_connection(
        &mut self,
        cx: &mut Context<Self>,
        id: ConnectionId,
    ) -> Result<ConnectionId, StoreError> {
        self.mutate(cx, |items| {
            let copy = ops::duplicate_connection(items, id)?;
            Ok((
                copy,
                true,
                vec![ConnectionStoreEvent::ConnectionAdded(copy)],
            ))
        })
    }

    /// Removes a node and returns every removed connection: the connection
    /// itself, or all connections inside a removed folder. Keychain entries
    /// and open tabs are the listeners' business.
    ///
    /// # Errors
    /// [`StoreError::ReadOnly`], [`StoreError::Ops`] for an unknown node, or
    /// [`StoreError::Io`] when saving fails.
    pub fn remove(
        &mut self,
        cx: &mut Context<Self>,
        node: NodeRef,
    ) -> Result<Vec<ConnectionId>, StoreError> {
        self.mutate(cx, |items| {
            let removed = match ops::remove(items, node)? {
                CatalogItem::Connection(profile) => vec![profile.id],
                CatalogItem::Folder(folder) => ops::connections(&folder.items)
                    .into_iter()
                    .map(|profile| profile.id)
                    .collect(),
            };
            let events = removed
                .iter()
                .map(|id| ConnectionStoreEvent::ConnectionRemoved(*id))
                .collect();
            Ok((removed, true, events))
        })
    }

    /// Removes a folder, keeping its children where it was.
    ///
    /// # Errors
    /// [`StoreError::ReadOnly`], [`StoreError::Ops`] for an unknown folder, or
    /// [`StoreError::Io`] when saving fails.
    pub fn remove_folder_keep_contents(
        &mut self,
        cx: &mut Context<Self>,
        folder: FolderId,
    ) -> Result<(), StoreError> {
        self.mutate(cx, |items| {
            ops::remove_folder_keep_contents(items, folder)?;
            Ok(((), true, vec![]))
        })
    }
}

/// Loads the catalog and installs the store as a GPUI global.
///
/// A missing file gives an empty catalog; a corrupt one an empty catalog and a
/// notice; a newer-version or unreadable file is shown read-only when it still
/// parses leniently (else empty) and is never written.
pub fn init(cx: &mut App, paths: &ConfigPaths) {
    let file = paths.connections_file();
    let (catalog, read_only, load_notice) = match persist::load_json::<CatalogFile>(&file) {
        Ok(LoadOutcome::Missing) => (CatalogFile::default(), false, None),
        Ok(LoadOutcome::Loaded(catalog)) => (catalog, false, None),
        Ok(LoadOutcome::Corrupt { backup, .. }) => (
            CatalogFile::default(),
            false,
            Some(LoadNotice::Corrupt { backup }),
        ),
        Ok(LoadOutcome::NewerVersion { .. }) => {
            (lenient(&file), true, Some(LoadNotice::NewerVersion))
        }
        Err(_) => (CatalogFile::default(), true, None),
    };
    let entity = cx.new(|_| ConnectionStore {
        file,
        catalog,
        read_only,
        load_notice,
    });
    cx.set_global(GlobalConnectionStore(entity));
}

fn lenient(file: &std::path::Path) -> CatalogFile {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|text| serde_json::from_str::<CatalogFile>(&text).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use gpui_kit::{Context, Entity, TestAppContext};

    use super::{ConnectionStore, ConnectionStoreEvent, GlobalConnectionStore, StoreError, init};
    use crate::catalog::model::{
        CatalogItem, ConnectionId, ConnectionProfile, Driver, Endpoint, HexColor, PasswordStorage,
    };
    use crate::catalog::ops::{DropPosition, NodeRef};
    use crate::color::Rgb;
    use crate::db::model::TlsMode;
    use crate::paths::ConfigPaths;
    use crate::settings::LoadNotice;

    type Events = Arc<Mutex<Vec<ConnectionStoreEvent>>>;

    fn server(name: &str, host: &str) -> ConnectionProfile {
        ConnectionProfile::new(
            name,
            Driver::Postgres,
            Endpoint::Server {
                host: host.to_owned(),
                port: 5432,
                user: "u".to_owned(),
                database: None,
                tls: TlsMode::default(),
            },
            PasswordStorage::Keychain,
        )
    }

    fn open(cx: &mut TestAppContext, dir: &tempfile::TempDir) -> Entity<ConnectionStore> {
        let paths = ConfigPaths::at(dir.path().to_path_buf());
        cx.update(|cx| {
            init(cx, &paths);
            cx.global::<GlobalConnectionStore>().0.clone()
        })
    }

    fn watch(cx: &mut TestAppContext, entity: &Entity<ConnectionStore>) -> Events {
        let events: Events = Arc::default();
        let sink = events.clone();
        cx.update(|cx| {
            cx.subscribe(entity, move |_, event: &ConnectionStoreEvent, _| {
                sink.lock().expect("event lock").push(event.clone());
            })
            .detach();
        });
        events
    }

    fn drain(events: &Events) -> Vec<ConnectionStoreEvent> {
        std::mem::take(&mut *events.lock().expect("event lock"))
    }

    fn with<R>(
        cx: &mut TestAppContext,
        entity: &Entity<ConnectionStore>,
        f: impl FnOnce(&mut ConnectionStore, &mut Context<ConnectionStore>) -> R,
    ) -> R {
        cx.update(|cx| entity.update(cx, f))
    }

    fn names(cx: &mut TestAppContext, entity: &Entity<ConnectionStore>) -> Vec<String> {
        fn walk(items: &[CatalogItem], out: &mut Vec<String>) {
            for item in items {
                match item {
                    CatalogItem::Connection(profile) => out.push(profile.name.clone()),
                    CatalogItem::Folder(folder) => {
                        out.push(format!("[{}]", folder.name));
                        walk(&folder.items, out);
                    }
                }
            }
        }
        with(cx, entity, |store, _| {
            let mut out = Vec::new();
            walk(store.items(), &mut out);
            out
        })
    }

    fn config_file(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("connections.json")
    }

    #[gpui_kit::test]
    fn add_update_remove_persist_and_reload() {
        // Given an empty store
        let mut cx = TestAppContext::single();
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open(&mut cx, &dir);
        let mut profile = server("one", "db.local");
        let id = profile.id;
        // When adding, updating and adding a second connection
        with(&mut cx, &store, |s, cx| {
            s.add_connection(cx, profile.clone(), DropPosition::RootEnd)
        })
        .expect("add");
        profile.name = "renamed".to_owned();
        with(&mut cx, &store, |s, cx| {
            s.update_connection(cx, profile.clone())
        })
        .expect("update");
        let second = server("two", "other");
        let second_id = second.id;
        with(&mut cx, &store, |s, cx| {
            s.add_connection(cx, second, DropPosition::RootEnd)
        })
        .expect("add");
        // Then a fresh store reads the same items
        let fresh_dir_store = {
            let reopened = open(&mut cx, &dir);
            names(&mut cx, &reopened)
        };
        assert_eq!(fresh_dir_store, ["renamed", "two"]);
        // When removing the first connection
        let removed = with(&mut cx, &store, |s, cx| {
            s.remove(cx, NodeRef::Connection(id))
        })
        .expect("remove");
        // Then it is reported and the file holds only the second
        assert_eq!(removed, [id]);
        let reopened = open(&mut cx, &dir);
        assert!(with(&mut cx, &reopened, |s, _| s.profile(id).is_none()));
        assert!(with(&mut cx, &reopened, |s, _| s
            .profile(second_id)
            .is_some()));
    }

    #[gpui_kit::test]
    fn events_report_each_mutation() {
        // Given a store with a watcher
        let mut cx = TestAppContext::single();
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open(&mut cx, &dir);
        let events = watch(&mut cx, &store);
        let mut profile = server("one", "db.local");
        let id = profile.id;
        // When adding
        with(&mut cx, &store, |s, cx| {
            s.add_connection(cx, profile.clone(), DropPosition::RootEnd)
        })
        .expect("add");
        // Then ConnectionAdded then Changed
        assert_eq!(
            drain(&events),
            [
                ConnectionStoreEvent::ConnectionAdded(id),
                ConnectionStoreEvent::Changed
            ]
        );
        // When only the name changes
        profile.name = "new name".to_owned();
        with(&mut cx, &store, |s, cx| {
            s.update_connection(cx, profile.clone())
        })
        .expect("update");
        // Then the endpoint is reported unchanged
        assert_eq!(
            drain(&events),
            [
                ConnectionStoreEvent::ConnectionUpdated {
                    id,
                    endpoint_changed: false
                },
                ConnectionStoreEvent::Changed
            ]
        );
        // When the host, then the password storage, then the driver change
        for edit in [
            (|p: &mut ConnectionProfile| {
                p.endpoint = Endpoint::Server {
                    host: "elsewhere".to_owned(),
                    port: 5432,
                    user: "u".to_owned(),
                    database: None,
                    tls: TlsMode::default(),
                };
            }) as fn(&mut ConnectionProfile),
            |p| p.password = PasswordStorage::Ask,
            |p| p.driver = Driver::MySql,
        ] {
            edit(&mut profile);
            with(&mut cx, &store, |s, cx| {
                s.update_connection(cx, profile.clone())
            })
            .expect("update");
            // Then each is an endpoint change
            assert_eq!(
                drain(&events)[0],
                ConnectionStoreEvent::ConnectionUpdated {
                    id,
                    endpoint_changed: true
                }
            );
        }
        // When setting and resetting the color
        let color = HexColor::new(Rgb { r: 1, g: 2, b: 3 });
        with(&mut cx, &store, |s, cx| {
            s.set_custom_color(cx, id, Some(color))
        })
        .expect("color");
        assert_eq!(
            drain(&events),
            [
                ConnectionStoreEvent::ColorChanged(id),
                ConnectionStoreEvent::Changed
            ]
        );
        with(&mut cx, &store, |s, cx| s.reset_color(cx, id)).expect("reset");
        assert_eq!(
            drain(&events),
            [
                ConnectionStoreEvent::ColorChanged(id),
                ConnectionStoreEvent::Changed
            ]
        );
        assert_eq!(
            with(&mut cx, &store, |s, _| s
                .profile(id)
                .map(|p| p.color.custom)),
            Some(None)
        );
    }

    #[gpui_kit::test]
    fn folder_removal_reports_every_contained_connection() {
        // Given a store with a watcher and a folder holding two connections
        let mut cx = TestAppContext::single();
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open(&mut cx, &dir);
        let events = watch(&mut cx, &store);
        let folder = with(&mut cx, &store, |s, cx| {
            s.add_folder(cx, "f", DropPosition::RootEnd)
        })
        .expect("folder");
        drain(&events);
        let (a, b) = (server("a", "x"), server("b", "y"));
        let (a_id, b_id) = (a.id, b.id);
        for p in [a, b] {
            with(&mut cx, &store, |s, cx| {
                s.add_connection(cx, p, DropPosition::Into(folder))
            })
            .expect("add");
        }
        drain(&events);
        // When removing the folder
        let removed = with(&mut cx, &store, |s, cx| {
            s.remove(cx, NodeRef::Folder(folder))
        })
        .expect("remove");
        // Then every contained connection is reported removed
        assert_eq!(removed, [a_id, b_id]);
        assert_eq!(
            drain(&events),
            [
                ConnectionStoreEvent::ConnectionRemoved(a_id),
                ConnectionStoreEvent::ConnectionRemoved(b_id),
                ConnectionStoreEvent::Changed
            ]
        );
    }

    #[gpui_kit::test]
    fn read_only_store_rejects_mutations() {
        // Given a file from a newer version holding one connection
        let mut cx = TestAppContext::single();
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = server("kept", "db.local");
        let id = profile.id;
        let doc = serde_json::json!({
            "version": 99,
            "future": true,
            "items": [{ "connection": profile }],
        });
        let bytes = serde_json::to_vec(&doc).expect("json");
        fs::write(config_file(&dir), &bytes).expect("write");
        let store = open(&mut cx, &dir);
        let events = watch(&mut cx, &store);
        // Then its items are displayed read-only with a notice
        assert!(with(&mut cx, &store, |s, _| s.is_read_only()));
        assert!(with(&mut cx, &store, |s, _| s.profile(id).is_some()));
        assert_eq!(
            with(&mut cx, &store, |s, _| s.take_load_notice()),
            Some(LoadNotice::NewerVersion)
        );
        // When trying every mutation
        let folder = crate::catalog::model::FolderId::new();
        let node = NodeRef::Connection(id);
        let results = with(&mut cx, &store, |s, cx| {
            vec![
                s.add_connection(cx, server("x", "y"), DropPosition::RootEnd)
                    .map(drop),
                s.update_connection(cx, profile.clone()),
                s.set_custom_color(cx, id, None),
                s.reset_color(cx, id),
                s.add_folder(cx, "f", DropPosition::RootEnd).map(drop),
                s.rename_folder(cx, folder, "g"),
                s.set_folder_expanded(cx, folder, true),
                s.move_node(cx, node, DropPosition::RootEnd),
                s.move_up(cx, node).map(drop),
                s.move_down(cx, node).map(drop),
                s.duplicate_connection(cx, id).map(drop),
                s.remove(cx, node).map(drop),
                s.remove_folder_keep_contents(cx, folder),
            ]
        });
        // Then all are refused and nothing changes
        assert!(
            results
                .iter()
                .all(|r| matches!(r, Err(StoreError::ReadOnly)))
        );
        assert_eq!(drain(&events), []);
        assert_eq!(fs::read(config_file(&dir)).expect("read"), bytes);
        assert_eq!(names(&mut cx, &store), ["kept"]);
    }

    #[gpui_kit::test]
    fn newer_unparsable_file_is_empty_and_untouched() {
        // Given a newer file that does not match the catalog shape
        let mut cx = TestAppContext::single();
        let dir = tempfile::tempdir().expect("tempdir");
        let bytes = br#"{"version": 99, "items": "weird"}"#;
        fs::write(config_file(&dir), bytes).expect("write");
        // When opening and mutating
        let store = open(&mut cx, &dir);
        let result = with(&mut cx, &store, |s, cx| {
            s.add_folder(cx, "f", DropPosition::RootEnd)
        });
        // Then the catalog is empty, read-only and the file is untouched
        assert!(matches!(result, Err(StoreError::ReadOnly)));
        assert!(with(&mut cx, &store, |s, _| s.is_read_only()));
        assert_eq!(names(&mut cx, &store).len(), 0);
        assert_eq!(fs::read(config_file(&dir)).expect("read"), bytes);
    }

    #[gpui_kit::test]
    fn corrupt_file_gives_empty_catalog_and_notice() {
        // Given a corrupt file
        let mut cx = TestAppContext::single();
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(config_file(&dir), b"{ nope").expect("write");
        // When opening
        let store = open(&mut cx, &dir);
        // Then the catalog is empty, writable, with a corrupt notice taken once
        assert_eq!(names(&mut cx, &store).len(), 0);
        assert!(!with(&mut cx, &store, |s, _| s.is_read_only()));
        assert!(matches!(
            with(&mut cx, &store, |s, _| s.take_load_notice()),
            Some(LoadNotice::Corrupt { .. })
        ));
        assert_eq!(with(&mut cx, &store, |s, _| s.take_load_notice()), None);
    }

    #[gpui_kit::test]
    fn folder_expanded_rename_and_keep_contents_persist() {
        // Given a folder with a connection
        let mut cx = TestAppContext::single();
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open(&mut cx, &dir);
        let folder = with(&mut cx, &store, |s, cx| {
            s.add_folder(cx, "f", DropPosition::RootEnd)
        })
        .expect("folder");
        with(&mut cx, &store, |s, cx| {
            s.add_connection(cx, server("c", "h"), DropPosition::Into(folder))
        })
        .expect("add");
        // When expanding and renaming
        with(&mut cx, &store, |s, cx| {
            s.set_folder_expanded(cx, folder, true)
        })
        .expect("expand");
        with(&mut cx, &store, |s, cx| s.rename_folder(cx, folder, "g")).expect("rename");
        // Then a fresh store sees both
        let fresh = open(&mut cx, &dir);
        let state = with(&mut cx, &fresh, |s, _| match s.items() {
            [CatalogItem::Folder(f)] => Some((f.name.clone(), f.expanded)),
            _ => None,
        });
        assert_eq!(state, Some(("g".to_owned(), true)));
        // When removing the folder but keeping contents
        with(&mut cx, &store, |s, cx| {
            s.remove_folder_keep_contents(cx, folder)
        })
        .expect("keep");
        // Then the connection stays at the root, persisted
        let fresh = open(&mut cx, &dir);
        assert_eq!(names(&mut cx, &fresh), ["c"]);
    }

    #[gpui_kit::test]
    fn duplicate_and_moves_persist_order() {
        // Given three connections
        let mut cx = TestAppContext::single();
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open(&mut cx, &dir);
        let ids: Vec<ConnectionId> = ["a", "b", "c"]
            .into_iter()
            .map(|n| {
                let p = server(n, n);
                let id = p.id;
                with(&mut cx, &store, |s, cx| {
                    s.add_connection(cx, p, DropPosition::RootEnd)
                })
                .expect("add");
                id
            })
            .collect();
        // When duplicating a, moving c up, a down and a to the end
        let copy =
            with(&mut cx, &store, |s, cx| s.duplicate_connection(cx, ids[0])).expect("duplicate");
        assert!(with(&mut cx, &store, |s, _| s.profile(copy).is_some()));
        assert!(
            with(&mut cx, &store, |s, cx| s
                .move_up(cx, NodeRef::Connection(ids[2])))
            .expect("up")
        );
        assert!(
            with(&mut cx, &store, |s, cx| s
                .move_down(cx, NodeRef::Connection(ids[0])))
            .expect("down")
        );
        with(&mut cx, &store, |s, cx| {
            s.move_node(cx, NodeRef::Connection(ids[0]), DropPosition::RootEnd)
        })
        .expect("move");
        // Then a fresh store reads the same order
        let fresh = open(&mut cx, &dir);
        assert_eq!(names(&mut cx, &fresh), names(&mut cx, &store),);
        assert_eq!(names(&mut cx, &fresh), ["a (copy)", "c", "b", "a"]);
    }

    #[gpui_kit::test]
    fn edge_move_changes_nothing_and_emits_nothing() {
        // Given a single connection
        let mut cx = TestAppContext::single();
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open(&mut cx, &dir);
        let p = server("a", "a");
        let id = p.id;
        with(&mut cx, &store, |s, cx| {
            s.add_connection(cx, p, DropPosition::RootEnd)
        })
        .expect("add");
        let events = watch(&mut cx, &store);
        // When moving it up at the top edge
        let moved = with(&mut cx, &store, |s, cx| {
            s.move_up(cx, NodeRef::Connection(id))
        })
        .expect("up");
        // Then nothing happens
        assert!(!moved);
        assert_eq!(drain(&events), []);
    }

    #[gpui_kit::test]
    fn failed_save_keeps_change_and_next_mutation_saves_all() {
        // Given a store whose tmp path is blocked by a directory
        let mut cx = TestAppContext::single();
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open(&mut cx, &dir);
        let blocker = dir.path().join("connections.json.tmp");
        fs::create_dir(&blocker).expect("blocker");
        let events = watch(&mut cx, &store);
        // When adding a connection
        let result = with(&mut cx, &store, |s, cx| {
            s.add_connection(cx, server("a", "a"), DropPosition::RootEnd)
        });
        // Then Io is returned, the change stays and events were emitted
        assert!(matches!(result, Err(StoreError::Io(_))));
        assert_eq!(names(&mut cx, &store), ["a"]);
        assert_eq!(drain(&events).len(), 2);
        // When the obstacle is removed and another mutation succeeds
        fs::remove_dir(&blocker).expect("unblock");
        with(&mut cx, &store, |s, cx| {
            s.add_connection(cx, server("b", "b"), DropPosition::RootEnd)
        })
        .expect("add");
        // Then the file holds both
        let fresh = open(&mut cx, &dir);
        assert_eq!(names(&mut cx, &fresh), ["a", "b"]);
    }

    #[gpui_kit::test]
    fn unknown_node_is_an_ops_error() {
        // Given an empty store
        let mut cx = TestAppContext::single();
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open(&mut cx, &dir);
        // When updating a connection that does not exist
        let result = with(&mut cx, &store, |s, cx| {
            s.update_connection(cx, server("ghost", "h"))
        });
        // Then NotFound and no file is written
        assert!(matches!(result, Err(StoreError::Ops(_))));
        assert!(!config_file(&dir).exists());
    }
}
