//! Tab store, close guards and tab view registry.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui_kit::prelude::*;
use gpui_kit::{App, Context, Entity, EventEmitter, Global, SharedString};

use super::view_factory::TabKindTag;
use crate::catalog::model::ConnectionId;
use crate::db::model::ObjectRef;
use crate::settings::WindowMode;

/// Identifier of an open tab, unique within a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabId(pub u64);

/// What a tab shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabKind {
    Table(ObjectRef),
    Query {
        database: Option<String>,
        number: u32,
    },
}

impl TabKind {
    /// The registry tag of this kind.
    #[must_use]
    pub const fn tag(&self) -> TabKindTag {
        match self {
            Self::Table(_) => TabKindTag::Table,
            Self::Query { .. } => TabKindTag::Query,
        }
    }
}

/// An open tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabRecord {
    pub id: TabId,
    pub connection: ConnectionId,
    pub kind: TabKind,
    pub title: SharedString,
}

/// The window a tab is shown in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowKey {
    Main,
    Connection(ConnectionId),
}

/// Why closing a tab deserves a confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloseReason {
    UnsavedQueryText,
    RunningQuery,
}

/// Which side of the target a moved tab or group lands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Before,
    After,
}

/// Why a move was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MoveError {
    #[error("tabs of different connections cannot be reordered across groups")]
    DifferentGroup,
    #[error("no such tab")]
    UnknownTab,
    #[error("no tabs for that connection")]
    UnknownConnection,
}

/// Change notification emitted by [`TabStore`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabStoreEvent {
    /// A tab was inserted; `Activated` follows when `activate` is set.
    Opened {
        tab: TabId,
        activate: bool,
    },
    Closed(TabId),
    Activated {
        key: WindowKey,
        tab: Option<TabId>,
    },
    Reordered,
    ModeChanged(WindowMode),
}

type Guard = Rc<dyn Fn(TabId, &App) -> Vec<CloseReason>>;

/// Open tabs, grouped by connection, with per-window active tabs.
pub struct TabStore {
    order: Vec<TabRecord>,
    mode: WindowMode,
    active: HashMap<WindowKey, TabId>,
    activation_history: Vec<TabId>,
    guards: HashMap<TabKindTag, Guard>,
    next_id: u64,
}

/// GPUI global handle to the tab store.
pub struct GlobalTabStore(pub Entity<TabStore>);

impl Global for GlobalTabStore {}

impl EventEmitter<TabStoreEvent> for TabStore {}

/// Installs the tab store as a GPUI global.
pub fn init(cx: &mut App, mode: WindowMode) {
    let entity = cx.new(|_| TabStore::new(mode));
    cx.set_global(GlobalTabStore(entity));
}

impl TabStore {
    #[must_use]
    pub fn new(mode: WindowMode) -> Self {
        Self {
            order: Vec::new(),
            mode,
            active: HashMap::new(),
            activation_history: Vec::new(),
            guards: HashMap::new(),
            next_id: 1,
        }
    }

    #[must_use]
    pub const fn mode(&self) -> WindowMode {
        self.mode
    }

    /// All open tabs in display order.
    #[must_use]
    pub fn tabs(&self) -> &[TabRecord] {
        &self.order
    }

    #[must_use]
    pub fn tab(&self, id: TabId) -> Option<&TabRecord> {
        self.order.iter().find(|tab| tab.id == id)
    }

    /// The window that shows a connection's tabs in the current mode.
    #[must_use]
    pub const fn window_key_for(&self, connection: ConnectionId) -> WindowKey {
        match self.mode {
            WindowMode::Single => WindowKey::Main,
            WindowMode::PerConnection => WindowKey::Connection(connection),
        }
    }

    /// Tabs shown in a window, in display order; none when the key does not fit the mode.
    #[must_use]
    pub fn visible_tabs(&self, key: WindowKey) -> Vec<&TabRecord> {
        self.order
            .iter()
            .filter(|tab| self.window_key_for(tab.connection) == key)
            .collect()
    }

    #[must_use]
    pub fn active(&self, key: WindowKey) -> Option<TabId> {
        self.active.get(&key).copied()
    }

    /// Whether every connection's tabs form one contiguous block.
    #[must_use]
    pub fn is_contiguous(&self) -> bool {
        let mut seen = HashSet::new();
        let mut previous = None;
        self.order.iter().all(|tab| {
            let continues = previous == Some(tab.connection);
            previous = Some(tab.connection);
            continues || seen.insert(tab.connection)
        })
    }

    fn check(&self) {
        debug_assert!(self.is_contiguous(), "tab groups must stay contiguous");
    }

    fn insert(
        &mut self,
        connection: ConnectionId,
        kind: TabKind,
        title: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> TabId {
        let id = TabId(self.next_id);
        self.next_id += 1;
        let position = self
            .order
            .iter()
            .rposition(|tab| tab.connection == connection)
            .map_or(self.order.len(), |last| last + 1);
        let title = title.into();
        self.order.insert(
            position,
            TabRecord {
                id,
                connection,
                kind,
                title,
            },
        );
        self.check();
        cx.emit(TabStoreEvent::Opened {
            tab: id,
            activate: true,
        });
        self.activate(id, cx);
        id
    }

    /// Focuses the tab of this table when open, else opens one; returns its id.
    pub fn open_table(
        &mut self,
        connection: ConnectionId,
        object: ObjectRef,
        cx: &mut Context<Self>,
    ) -> TabId {
        let existing = self.order.iter().find(|tab| {
            tab.connection == connection && matches!(&tab.kind, TabKind::Table(o) if *o == object)
        });
        if let Some(id) = existing.map(|tab| tab.id) {
            self.activate(id, cx);
            return id;
        }
        let title = object.name.clone();
        self.insert(connection, TabKind::Table(object), title, cx)
    }

    /// Opens a new query tab numbered after the connection's highest open query.
    pub fn open_query(
        &mut self,
        connection: ConnectionId,
        database: Option<String>,
        cx: &mut Context<Self>,
    ) -> TabId {
        let number = self
            .order
            .iter()
            .filter(|tab| tab.connection == connection)
            .filter_map(|tab| match &tab.kind {
                TabKind::Query { number, .. } => Some(*number),
                TabKind::Table(_) => None,
            })
            .max()
            .unwrap_or(0)
            + 1;
        self.insert(
            connection,
            TabKind::Query { database, number },
            format!("Query {number}"),
            cx,
        )
    }

    /// Makes a tab the active one of its window.
    pub fn activate(&mut self, tab: TabId, cx: &mut Context<Self>) {
        let Some(connection) = self.tab(tab).map(|record| record.connection) else {
            return;
        };
        let key = self.window_key_for(connection);
        self.set_active(key, Some(tab));
        cx.emit(TabStoreEvent::Activated {
            key,
            tab: Some(tab),
        });
        cx.notify();
    }

    fn set_active(&mut self, key: WindowKey, tab: Option<TabId>) {
        match tab {
            Some(tab) => {
                self.activation_history.retain(|id| *id != tab);
                self.activation_history.push(tab);
                self.active.insert(key, tab);
            }
            None => {
                self.active.remove(&key);
            }
        }
    }

    /// Closes tabs without consulting guards; closed active tabs hand over to a neighbor.
    pub fn close(&mut self, tabs: &[TabId], cx: &mut Context<Self>) {
        let closing: HashSet<TabId> = tabs
            .iter()
            .copied()
            .filter(|id| self.tab(*id).is_some())
            .collect();
        if closing.is_empty() {
            return;
        }
        let mut handovers = Vec::new();
        for (key, active) in &self.active {
            if !closing.contains(active) {
                continue;
            }
            let visible: Vec<TabId> = self.visible_tabs(*key).iter().map(|tab| tab.id).collect();
            let position = visible.iter().position(|id| id == active).unwrap_or(0);
            let open = |id: &&TabId| !closing.contains(*id);
            let next = visible[position + 1..]
                .iter()
                .find(open)
                .or_else(|| visible[..position].iter().rev().find(open))
                .copied();
            handovers.push((*key, next));
        }
        let closed: Vec<TabId> = self
            .order
            .iter()
            .map(|tab| tab.id)
            .filter(|id| closing.contains(id))
            .collect();
        self.order.retain(|tab| !closing.contains(&tab.id));
        self.activation_history.retain(|id| !closing.contains(id));
        for (key, next) in &handovers {
            self.set_active(*key, *next);
        }
        self.check();
        for id in closed {
            cx.emit(TabStoreEvent::Closed(id));
        }
        for (key, tab) in handovers {
            cx.emit(TabStoreEvent::Activated { key, tab });
        }
        cx.notify();
    }

    /// Closes every tab of a connection without consulting guards.
    pub fn close_connection_tabs(&mut self, connection: ConnectionId, cx: &mut Context<Self>) {
        let ids: Vec<TabId> = self
            .order
            .iter()
            .filter(|tab| tab.connection == connection)
            .map(|tab| tab.id)
            .collect();
        self.close(&ids, cx);
    }

    /// Moves a tab next to another tab of the same connection.
    ///
    /// # Errors
    /// [`MoveError::UnknownTab`] for a missing tab, [`MoveError::DifferentGroup`] when the
    /// tabs belong to different connections.
    pub fn move_tab(
        &mut self,
        tab: TabId,
        target: TabId,
        side: Side,
        cx: &mut Context<Self>,
    ) -> Result<(), MoveError> {
        let from = self.index_of(tab)?;
        let to = self.index_of(target)?;
        if self.order[from].connection != self.order[to].connection {
            return Err(MoveError::DifferentGroup);
        }
        if tab == target {
            return Ok(());
        }
        let record = self.order.remove(from);
        let target_index = self.index_of(target)?;
        let at = match side {
            Side::Before => target_index,
            Side::After => target_index + 1,
        };
        self.order.insert(at, record);
        self.check();
        cx.emit(TabStoreEvent::Reordered);
        cx.notify();
        Ok(())
    }

    fn index_of(&self, tab: TabId) -> Result<usize, MoveError> {
        self.order
            .iter()
            .position(|record| record.id == tab)
            .ok_or(MoveError::UnknownTab)
    }

    /// Moves a connection's whole block of tabs before or after another connection's block.
    ///
    /// # Errors
    /// [`MoveError::UnknownConnection`] when either connection has no open tab.
    pub fn move_group(
        &mut self,
        connection: ConnectionId,
        target: ConnectionId,
        side: Side,
        cx: &mut Context<Self>,
    ) -> Result<(), MoveError> {
        let has = |id: ConnectionId| self.order.iter().any(|tab| tab.connection == id);
        if !has(connection) || !has(target) {
            return Err(MoveError::UnknownConnection);
        }
        if connection == target {
            return Ok(());
        }
        let (block, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.order)
            .into_iter()
            .partition(|tab| tab.connection == connection);
        self.order = rest;
        let at = match side {
            Side::Before => self.order.iter().position(|tab| tab.connection == target),
            Side::After => self
                .order
                .iter()
                .rposition(|tab| tab.connection == target)
                .map(|last| last + 1),
        }
        .unwrap_or(self.order.len());
        self.order.splice(at..at, block);
        self.check();
        cx.emit(TabStoreEvent::Reordered);
        cx.notify();
        Ok(())
    }

    /// Switches the window mode and recomputes each window's active tab from history.
    pub fn set_mode(&mut self, mode: WindowMode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.active.clear();
        let history = self.activation_history.clone();
        for id in history {
            if let Some(connection) = self.tab(id).map(|tab| tab.connection) {
                let key = self.window_key_for(connection);
                self.active.insert(key, id);
            }
        }
        cx.emit(TabStoreEvent::ModeChanged(mode));
        cx.notify();
    }

    /// Registers the close guard of a tab kind, replacing any previous one.
    pub fn register_guard(
        &mut self,
        tag: TabKindTag,
        guard: impl Fn(TabId, &App) -> Vec<CloseReason> + 'static,
    ) {
        self.guards.insert(tag, Rc::new(guard));
    }

    /// Every close reason raised by the guards of the given tabs, in tab order.
    #[must_use]
    pub fn close_reasons(&self, tabs: &[TabId], cx: &App) -> Vec<(TabId, CloseReason)> {
        tabs.iter()
            .filter_map(|id| Some((*id, self.guards.get(&self.tab(*id)?.kind.tag())?)))
            .flat_map(|(id, guard)| guard(id, cx).into_iter().map(move |reason| (id, reason)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui_kit::{Entity, TestAppContext};

    use super::{
        CloseReason, GlobalTabStore, MoveError, Side, TabId, TabKind, TabStore, TabStoreEvent,
        WindowKey, init,
    };
    use crate::catalog::model::ConnectionId;
    use crate::color::fnv1a_64;
    use crate::db::model::{ObjectRef, TableKind};
    use crate::settings::WindowMode;
    use crate::workspace::view_factory::TabKindTag;

    fn object(name: &str) -> ObjectRef {
        ObjectRef {
            database: None,
            schema: None,
            name: name.to_owned(),
            kind: TableKind::Table,
        }
    }

    fn setup(cx: &mut TestAppContext, mode: WindowMode) -> Entity<TabStore> {
        cx.update(|cx| {
            init(cx, mode);
            cx.global::<GlobalTabStore>().0.clone()
        })
    }

    fn with<R>(
        cx: &mut TestAppContext,
        store: &Entity<TabStore>,
        f: impl FnOnce(&mut TabStore, &mut gpui_kit::Context<TabStore>) -> R,
    ) -> R {
        cx.update(|cx| store.update(cx, f))
    }

    fn titles(cx: &mut TestAppContext, store: &Entity<TabStore>) -> Vec<String> {
        cx.update(|cx| {
            store
                .read(cx)
                .tabs()
                .iter()
                .map(|tab| tab.title.to_string())
                .collect()
        })
    }

    fn ids(cx: &mut TestAppContext, store: &Entity<TabStore>) -> Vec<TabId> {
        cx.update(|cx| store.read(cx).tabs().iter().map(|tab| tab.id).collect())
    }

    fn active(cx: &mut TestAppContext, store: &Entity<TabStore>, key: WindowKey) -> Option<TabId> {
        cx.update(|cx| store.read(cx).active(key))
    }

    fn contiguous(cx: &mut TestAppContext, store: &Entity<TabStore>) -> bool {
        cx.update(|cx| store.read(cx).is_contiguous())
    }

    #[gpui_kit::test]
    fn new_tabs_are_inserted_after_their_connection_group(cx: &mut TestAppContext) {
        // Given: tabs A1, B1 then a second tab for A.
        let store = setup(cx, WindowMode::Single);
        let (a, b) = (ConnectionId::new(), ConnectionId::new());
        // When: opening A1, B1, A2.
        with(cx, &store, |s, cx| {
            s.open_table(a, object("a1"), cx);
            s.open_table(b, object("b1"), cx);
            s.open_table(a, object("a2"), cx);
        });
        // Then: the order is A1, A2, B1 and the newest tab is active.
        assert_eq!(titles(cx, &store), ["a1", "a2", "b1"]);
        assert_eq!(
            active(cx, &store, WindowKey::Main),
            Some(ids(cx, &store)[1])
        );
    }

    #[gpui_kit::test]
    fn opening_an_open_table_focuses_the_existing_tab(cx: &mut TestAppContext) {
        // Given: two open table tabs, the second active.
        let store = setup(cx, WindowMode::Single);
        let a = ConnectionId::new();
        let first = with(cx, &store, |s, cx| {
            let first = s.open_table(a, object("t1"), cx);
            s.open_table(a, object("t2"), cx);
            first
        });
        // When: opening the first table again.
        let again = with(cx, &store, |s, cx| s.open_table(a, object("t1"), cx));
        // Then: no tab is added and the first one is active.
        assert_eq!(again, first);
        assert_eq!(ids(cx, &store).len(), 2);
        assert_eq!(active(cx, &store, WindowKey::Main), Some(first));
    }

    #[gpui_kit::test]
    fn the_same_table_on_another_connection_gets_its_own_tab(cx: &mut TestAppContext) {
        // Given: one table tab on connection A.
        let store = setup(cx, WindowMode::Single);
        let (a, b) = (ConnectionId::new(), ConnectionId::new());
        // When: opening the same object on B.
        with(cx, &store, |s, cx| {
            s.open_table(a, object("t"), cx);
            s.open_table(b, object("t"), cx);
        });
        // Then: both tabs exist.
        assert_eq!(ids(cx, &store).len(), 2);
    }

    #[gpui_kit::test]
    fn query_numbers_follow_the_highest_open_number(cx: &mut TestAppContext) {
        // Given: a connection with queries 1 and 2.
        let store = setup(cx, WindowMode::Single);
        let a = ConnectionId::new();
        let (q1, q2) = with(cx, &store, |s, cx| {
            (s.open_query(a, None, cx), s.open_query(a, None, cx))
        });
        assert_eq!(titles(cx, &store), ["Query 1", "Query 2"]);
        // When: closing query 1 and opening another.
        with(cx, &store, |s, cx| s.close(&[q1], cx));
        let q3 = with(cx, &store, |s, cx| s.open_query(a, None, cx));
        // Then: the new one is query 3.
        assert_eq!(titles(cx, &store), ["Query 2", "Query 3"]);
        // When: closing everything and opening again.
        with(cx, &store, |s, cx| s.close(&[q2, q3], cx));
        with(cx, &store, |s, cx| s.open_query(a, None, cx));
        // Then: numbering restarts at 1.
        assert_eq!(titles(cx, &store), ["Query 1"]);
    }

    #[gpui_kit::test]
    fn closing_the_active_tab_activates_right_then_left_then_none(cx: &mut TestAppContext) {
        // Given: three tabs, the middle one active.
        let store = setup(cx, WindowMode::Single);
        let a = ConnectionId::new();
        let (t1, t2, t3) = with(cx, &store, |s, cx| {
            let t = (
                s.open_query(a, None, cx),
                s.open_query(a, None, cx),
                s.open_query(a, None, cx),
            );
            s.activate(t.1, cx);
            t
        });
        // When: closing the middle tab.
        with(cx, &store, |s, cx| s.close(&[t2], cx));
        // Then: the right neighbor is active.
        assert_eq!(active(cx, &store, WindowKey::Main), Some(t3));
        // When: closing the last tab.
        with(cx, &store, |s, cx| s.close(&[t3], cx));
        // Then: the left neighbor is active.
        assert_eq!(active(cx, &store, WindowKey::Main), Some(t1));
        // When: closing the only tab.
        with(cx, &store, |s, cx| s.close(&[t1], cx));
        // Then: nothing is active.
        assert_eq!(active(cx, &store, WindowKey::Main), None);
    }

    #[gpui_kit::test]
    fn closing_emits_events_for_closed_and_activated_tabs(cx: &mut TestAppContext) {
        // Given: two tabs and a recorder of events.
        let store = setup(cx, WindowMode::Single);
        let a = ConnectionId::new();
        let (t1, t2) = with(cx, &store, |s, cx| {
            (s.open_query(a, None, cx), s.open_query(a, None, cx))
        });
        let events = Rc::new(RefCell::new(Vec::new()));
        let log = events.clone();
        let _subscription = cx.update(|cx| {
            cx.subscribe(&store, move |_, event: &TabStoreEvent, _| {
                log.borrow_mut().push(event.clone());
            })
        });
        // When: closing the active tab.
        with(cx, &store, |s, cx| s.close(&[t2], cx));
        // Then: Closed and the neighbor's Activated are emitted.
        assert_eq!(
            *events.borrow(),
            [
                TabStoreEvent::Closed(t2),
                TabStoreEvent::Activated {
                    key: WindowKey::Main,
                    tab: Some(t1)
                },
            ]
        );
    }

    #[gpui_kit::test]
    fn moving_a_tab_within_a_group_reorders_it(cx: &mut TestAppContext) {
        // Given: a group of three query tabs.
        let store = setup(cx, WindowMode::Single);
        let a = ConnectionId::new();
        let (t1, _, t3) = with(cx, &store, |s, cx| {
            (
                s.open_query(a, None, cx),
                s.open_query(a, None, cx),
                s.open_query(a, None, cx),
            )
        });
        // When: moving the first after the last.
        let result = with(cx, &store, |s, cx| s.move_tab(t1, t3, Side::After, cx));
        // Then: it is last.
        assert_eq!(result, Ok(()));
        assert_eq!(titles(cx, &store), ["Query 2", "Query 3", "Query 1"]);
    }

    #[gpui_kit::test]
    fn moving_a_tab_across_groups_is_rejected(cx: &mut TestAppContext) {
        // Given: tabs of two connections.
        let store = setup(cx, WindowMode::Single);
        let (a, b) = (ConnectionId::new(), ConnectionId::new());
        let (ta, tb) = with(cx, &store, |s, cx| {
            (s.open_query(a, None, cx), s.open_query(b, None, cx))
        });
        let before = ids(cx, &store);
        // When: moving A's tab next to B's tab.
        let result = with(cx, &store, |s, cx| s.move_tab(ta, tb, Side::Before, cx));
        // Then: it is refused and nothing changes.
        assert_eq!(result, Err(MoveError::DifferentGroup));
        assert_eq!(ids(cx, &store), before);
    }

    #[gpui_kit::test]
    fn move_group_moves_the_whole_block(cx: &mut TestAppContext) {
        // Given: groups A (2 tabs), B (1 tab), C (1 tab).
        let store = setup(cx, WindowMode::Single);
        let (a, b, c) = (
            ConnectionId::new(),
            ConnectionId::new(),
            ConnectionId::new(),
        );
        with(cx, &store, |s, cx| {
            s.open_table(a, object("a1"), cx);
            s.open_table(a, object("a2"), cx);
            s.open_table(b, object("b1"), cx);
            s.open_table(c, object("c1"), cx);
        });
        // When: moving A after C, then C before B.
        with(cx, &store, |s, cx| s.move_group(a, c, Side::After, cx)).expect("move succeeds");
        assert_eq!(titles(cx, &store), ["b1", "c1", "a1", "a2"]);
        with(cx, &store, |s, cx| s.move_group(c, b, Side::Before, cx)).expect("move succeeds");
        // Then: blocks are intact in the new order.
        assert_eq!(titles(cx, &store), ["c1", "b1", "a1", "a2"]);
        let missing = with(cx, &store, |s, cx| {
            s.move_group(ConnectionId::new(), a, Side::Before, cx)
        });
        assert_eq!(missing, Err(MoveError::UnknownConnection));
    }

    #[gpui_kit::test]
    fn visible_tabs_depend_on_mode_and_key(cx: &mut TestAppContext) {
        // Given: one tab on each of two connections.
        let store = setup(cx, WindowMode::Single);
        let (a, b) = (ConnectionId::new(), ConnectionId::new());
        with(cx, &store, |s, cx| {
            s.open_query(a, None, cx);
            s.open_query(b, None, cx);
        });
        let count =
            |cx: &mut TestAppContext, key| cx.update(|cx| store.read(cx).visible_tabs(key).len());
        // Then: Single shows all tabs in Main and none for a connection key.
        assert_eq!(count(cx, WindowKey::Main), 2);
        assert_eq!(count(cx, WindowKey::Connection(a)), 0);
        // When: switching to per-connection windows.
        with(cx, &store, |s, cx| {
            s.set_mode(WindowMode::PerConnection, cx);
        });
        // Then: each connection key shows its own tab and Main shows none.
        assert_eq!(count(cx, WindowKey::Main), 0);
        assert_eq!(count(cx, WindowKey::Connection(a)), 1);
        assert_eq!(count(cx, WindowKey::Connection(b)), 1);
    }

    #[gpui_kit::test]
    fn set_mode_restores_actives_from_history_and_emits(cx: &mut TestAppContext) {
        // Given: A1, A2, B1 opened in order, then A1 activated.
        let store = setup(cx, WindowMode::Single);
        let (a, b) = (ConnectionId::new(), ConnectionId::new());
        let (a1, _a2, b1) = with(cx, &store, |s, cx| {
            let a1 = s.open_query(a, None, cx);
            let a2 = s.open_query(a, None, cx);
            let b1 = s.open_query(b, None, cx);
            s.activate(a1, cx);
            (a1, a2, b1)
        });
        let events = Rc::new(RefCell::new(Vec::new()));
        let log = events.clone();
        let _subscription = cx.update(|cx| {
            cx.subscribe(&store, move |_, event: &TabStoreEvent, _| {
                log.borrow_mut().push(event.clone());
            })
        });
        // When: switching to per-connection windows.
        with(cx, &store, |s, cx| {
            s.set_mode(WindowMode::PerConnection, cx);
        });
        // Then: each connection's most recent tab is active and ModeChanged is emitted.
        assert_eq!(active(cx, &store, WindowKey::Connection(a)), Some(a1));
        assert_eq!(active(cx, &store, WindowKey::Connection(b)), Some(b1));
        assert_eq!(
            *events.borrow(),
            [TabStoreEvent::ModeChanged(WindowMode::PerConnection)]
        );
        // When: switching back.
        with(cx, &store, |s, cx| s.set_mode(WindowMode::Single, cx));
        // Then: Main shows the most recently used tab.
        assert_eq!(active(cx, &store, WindowKey::Main), Some(a1));
    }

    #[gpui_kit::test]
    fn close_connection_tabs_closes_only_that_group(cx: &mut TestAppContext) {
        // Given: tabs on two connections.
        let store = setup(cx, WindowMode::Single);
        let (a, b) = (ConnectionId::new(), ConnectionId::new());
        with(cx, &store, |s, cx| {
            s.open_query(a, None, cx);
            s.open_query(a, None, cx);
            s.open_table(b, object("b1"), cx);
        });
        // When: closing A's tabs.
        with(cx, &store, |s, cx| s.close_connection_tabs(a, cx));
        // Then: only B's tab remains, and it is active.
        assert_eq!(titles(cx, &store), ["b1"]);
        assert_eq!(
            active(cx, &store, WindowKey::Main),
            Some(ids(cx, &store)[0])
        );
    }

    #[gpui_kit::test]
    fn guards_aggregate_reasons_per_tab(cx: &mut TestAppContext) {
        // Given: a query guard reporting two reasons and a table tab with no guard.
        let store = setup(cx, WindowMode::Single);
        let a = ConnectionId::new();
        let (table, query) = with(cx, &store, |s, cx| {
            s.register_guard(TabKindTag::Query, |_, _| {
                vec![CloseReason::UnsavedQueryText, CloseReason::RunningQuery]
            });
            (s.open_table(a, object("t"), cx), s.open_query(a, None, cx))
        });
        // When: asking for the reasons of closing both.
        let reasons = cx.update(|cx| store.read(cx).close_reasons(&[table, query], cx));
        // Then: only the query contributes.
        assert_eq!(
            reasons,
            [
                (query, CloseReason::UnsavedQueryText),
                (query, CloseReason::RunningQuery)
            ]
        );
    }

    #[gpui_kit::test]
    fn contiguity_survives_a_pseudo_random_operation_sequence(cx: &mut TestAppContext) {
        // Given: three connections and a seeded generator.
        let store = setup(cx, WindowMode::Single);
        let connections = [
            ConnectionId::new(),
            ConnectionId::new(),
            ConnectionId::new(),
        ];
        let mut state = fnv1a_64(b"colonnade-tabs");
        let mut next = move |bound: usize| {
            state = fnv1a_64(&state.to_le_bytes());
            usize::try_from(state >> 33).unwrap_or(0) % bound
        };
        // When: applying 200 random operations.
        for _ in 0..200 {
            let all = ids(cx, &store);
            let pick = |n: usize| {
                if all.is_empty() {
                    None
                } else {
                    Some(all[n % all.len()])
                }
            };
            let connection = connections[next(3)];
            let (x, y, side) = (
                next(100),
                next(100),
                if next(2) == 0 {
                    Side::Before
                } else {
                    Side::After
                },
            );
            let operation = next(8);
            with(cx, &store, |s, cx| match operation {
                0 | 1 => {
                    s.open_table(connection, object(&format!("t{}", x % 4)), cx);
                }
                2 => {
                    s.open_query(connection, None, cx);
                }
                3 => {
                    if let Some(tab) = pick(x) {
                        s.close(&[tab], cx);
                    }
                }
                4 => {
                    if let (Some(tab), Some(target)) = (pick(x), pick(y)) {
                        let _ = s.move_tab(tab, target, side, cx);
                    }
                }
                5 => {
                    let _ = s.move_group(connection, connections[y % 3], side, cx);
                }
                6 => {
                    if let Some(tab) = pick(x) {
                        s.activate(tab, cx);
                    }
                }
                _ => {
                    let mode = if x % 2 == 0 {
                        WindowMode::Single
                    } else {
                        WindowMode::PerConnection
                    };
                    s.set_mode(mode, cx);
                }
            });
            // Then: groups stay contiguous after every step.
            assert!(contiguous(cx, &store));
        }
    }

    #[gpui_kit::test]
    fn query_tabs_record_their_database_and_title(cx: &mut TestAppContext) {
        // Given: an empty store.
        let store = setup(cx, WindowMode::Single);
        let a = ConnectionId::new();
        // When: opening a query on a database.
        with(cx, &store, |s, cx| {
            s.open_query(a, Some("app".to_owned()), cx)
        });
        // Then: the record carries the database and number.
        let kind = cx.update(|cx| store.read(cx).tabs()[0].kind.clone());
        assert_eq!(
            kind,
            TabKind::Query {
                database: Some("app".to_owned()),
                number: 1
            }
        );
    }
}
