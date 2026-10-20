//! Connections sidebar: custom tree, context menus and keyboard navigation.
//!
//! The tree is custom (not Kit's `Tree`, which toggles folders on mouse-down and would break
//! dragging) and virtualized with a `uniform_list` of fixed-height rows.

// allow: SIZE_OK - the plan assigns the whole sidebar view, its menus, keyboard and tests to this file.
use std::collections::HashSet;
use std::ops::Range;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, ClickEvent, Context, Div, DragMoveEvent, Entity, FocusHandle, Hsla,
    IntoElement, KeyBinding, MouseButton, Render, Role, ScrollStrategy, SharedString,
    TestSupportExt as _, UniformListScrollHandle, Window, div, px, uniform_list,
};

use super::dnd::{
    DragPreview, DraggedCatalogItem, DropZone, drop_position, is_valid_drop, zone_for,
};
use super::node::{PlaceholderKind, Row, RowKind, SidebarNodeKey, flatten};
use crate::catalog::model::{CatalogItem, ConnectionId};
use crate::catalog::ops::{self, DropPosition, NodeRef};
use crate::catalog::store::{ConnectionStore, GlobalConnectionStore};
use crate::color::Rgb;
use crate::db::model::DbError;
use crate::dialogs::{color, connection, folder};
use crate::prompts::{Notice, NoticeLevel, notify};
use crate::sessions::{ConnectionSessions, GlobalSessions, SessionStatus};
use crate::workspace::tabs::GlobalTabStore;

gpui_kit::actions!(
    sidebar,
    [
        SelectPrevious,
        SelectNext,
        CollapseOrParent,
        ExpandOrChild,
        Activate,
        DeleteSelected,
        MoveUp,
        MoveDown
    ]
);

/// Key context the sidebar sets so its shortcuts apply.
pub const KEY_CONTEXT: &str = "Sidebar";

/// Height of every row.
const ROW_HEIGHT: f32 = 28.;

/// Binds the sidebar keys in the [`KEY_CONTEXT`] key context.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevious, Some(KEY_CONTEXT)),
        KeyBinding::new("down", SelectNext, Some(KEY_CONTEXT)),
        KeyBinding::new("left", CollapseOrParent, Some(KEY_CONTEXT)),
        KeyBinding::new("right", ExpandOrChild, Some(KEY_CONTEXT)),
        KeyBinding::new("enter", Activate, Some(KEY_CONTEXT)),
        KeyBinding::new("delete", DeleteSelected, Some(KEY_CONTEXT)),
        KeyBinding::new("secondary-backspace", DeleteSelected, Some(KEY_CONTEXT)),
        KeyBinding::new("alt-up", MoveUp, Some(KEY_CONTEXT)),
        KeyBinding::new("alt-down", MoveDown, Some(KEY_CONTEXT)),
    ]);
}

/// An entry of a row's context menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    NewConnectionHere,
    NewFolderHere,
    Rename,
    Delete,
    ConnectToggle,
    NewQuery,
    Edit,
    Duplicate,
    ChangeColor,
    Refresh,
    Open,
}

impl MenuAction {
    /// Whether the action changes the catalog (and so needs a writable store).
    #[must_use]
    pub const fn mutates(self) -> bool {
        match self {
            Self::NewConnectionHere
            | Self::NewFolderHere
            | Self::Rename
            | Self::Delete
            | Self::Edit
            | Self::Duplicate
            | Self::ChangeColor => true,
            Self::ConnectToggle | Self::NewQuery | Self::Refresh | Self::Open => false,
        }
    }
}

/// One context-menu item and whether it is greyed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuItemSpec {
    pub action: MenuAction,
    pub label: &'static str,
    pub disabled: bool,
}

/// The context-menu items of a row kind; mutating ones are disabled when `read_only`.
#[must_use]
pub fn menu_items(kind: RowKind, read_only: bool) -> Vec<MenuItemSpec> {
    let entries: &[(MenuAction, &'static str)] = match kind {
        RowKind::Folder => &[
            (MenuAction::NewConnectionHere, "New Connection Here…"),
            (MenuAction::NewFolderHere, "New Folder Here…"),
            (MenuAction::Rename, "Rename…"),
            (MenuAction::Delete, "Delete…"),
        ],
        RowKind::Connection => &[
            (MenuAction::ConnectToggle, "Connect"),
            (MenuAction::NewQuery, "New Query"),
            (MenuAction::Edit, "Edit…"),
            (MenuAction::Duplicate, "Duplicate"),
            (MenuAction::ChangeColor, "Change Color…"),
            (MenuAction::Delete, "Delete…"),
        ],
        RowKind::Database | RowKind::Schema => &[
            (MenuAction::NewQuery, "New Query"),
            (MenuAction::Refresh, "Refresh"),
        ],
        RowKind::Table | RowKind::View => {
            &[(MenuAction::Open, "Open"), (MenuAction::Refresh, "Refresh")]
        }
        RowKind::Placeholder(_) => &[],
    };
    entries
        .iter()
        .map(|(action, label)| MenuItemSpec {
            action: *action,
            label,
            disabled: read_only && action.mutates(),
        })
        .collect()
}

/// What the trailing status of a connection row shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Idle,
    Connecting,
    Failed,
}

fn connection_store(cx: &App) -> Entity<ConnectionStore> {
    cx.global::<GlobalConnectionStore>().0.clone()
}

fn sessions(cx: &App) -> Entity<ConnectionSessions> {
    cx.global::<GlobalSessions>().0.clone()
}

fn node_ref(key: &SidebarNodeKey) -> Option<NodeRef> {
    match key {
        SidebarNodeKey::Folder(id) => Some(NodeRef::Folder(*id)),
        SidebarNodeKey::Connection(id) => Some(NodeRef::Connection(*id)),
        SidebarNodeKey::Object { .. } | SidebarNodeKey::Placeholder { .. } => None,
    }
}

/// The connections sidebar of one window.
pub struct SidebarView {
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    expanded: HashSet<SidebarNodeKey>,
    selected: Option<SidebarNodeKey>,
    hover: Option<(SidebarNodeKey, DropZone)>,
    visible: Vec<Row>,
}

impl SidebarView {
    /// A sidebar over the global connection store and sessions.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let store = connection_store(cx);
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        let sessions = sessions(cx);
        cx.observe(&sessions, |_, _, cx| cx.notify()).detach();
        Self {
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            expanded: HashSet::new(),
            selected: None,
            hover: None,
            visible: Vec::new(),
        }
    }

    /// The visible rows, top to bottom.
    #[must_use]
    pub fn rows(&self, cx: &App) -> Vec<Row> {
        flatten(
            connection_store(cx).read(cx),
            sessions(cx).read(cx),
            &self.expanded,
            None,
        )
    }

    /// The connection of the selected row (used by New Query without an active tab).
    #[must_use]
    pub fn selected_connection(&self) -> Option<ConnectionId> {
        self.selected.as_ref().and_then(SidebarNodeKey::connection)
    }

    fn read_only(cx: &App) -> bool {
        connection_store(cx).read(cx).is_read_only()
    }

    fn select(&mut self, key: SidebarNodeKey, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(key);
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn select_index(
        &mut self,
        rows: &[Row],
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = rows.get(index) {
            self.select(row.key.clone(), window, cx);
            self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
        }
    }

    fn report(
        task: gpui_kit::Task<Result<impl Send + 'static, DbError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |_, cx| {
            if let Err(error) = task.await
                && !matches!(error, DbError::Cancelled)
            {
                drop(cx.update(|window, cx| {
                    notify(
                        window,
                        cx,
                        Notice {
                            level: NoticeLevel::Error,
                            message: error.to_string().into(),
                        },
                    );
                }));
            }
        })
        .detach();
    }

    fn load(key: &SidebarNodeKey, window: &mut Window, cx: &mut Context<Self>) {
        let (connection, path) = match key {
            SidebarNodeKey::Connection(id) => (*id, crate::db::model::ObjectPath::Root),
            SidebarNodeKey::Object { connection, node } => {
                let Some(path) = node.children_path() else {
                    return;
                };
                (*connection, path)
            }
            SidebarNodeKey::Folder(_) | SidebarNodeKey::Placeholder { .. } => return,
        };
        let task = sessions(cx).update(cx, |s, cx| s.load_children(connection, path, window, cx));
        Self::report(task, window, cx);
    }

    fn toggle(&mut self, key: &SidebarNodeKey, window: &mut Window, cx: &mut Context<Self>) {
        match key {
            SidebarNodeKey::Folder(id) => {
                let open =
                    match ops::find(connection_store(cx).read(cx).items(), NodeRef::Folder(*id)) {
                        Some(CatalogItem::Folder(folder)) => folder.expanded,
                        Some(CatalogItem::Connection(_)) | None => return,
                    };
                // A read-only store refuses; the folder then simply stays as it is.
                drop(
                    connection_store(cx).update(cx, |s, cx| s.set_folder_expanded(cx, *id, !open)),
                );
            }
            SidebarNodeKey::Connection(_) | SidebarNodeKey::Object { .. } => {
                if let SidebarNodeKey::Object { node, .. } = key
                    && node.children_path().is_none()
                {
                    return;
                }
                if self.expanded.remove(key) {
                    cx.notify();
                } else {
                    self.expanded.insert(key.clone());
                    Self::load(key, window, cx);
                    cx.notify();
                }
            }
            SidebarNodeKey::Placeholder { .. } => {}
        }
    }

    fn open_table(key: &SidebarNodeKey, cx: &mut App) {
        if let SidebarNodeKey::Object { connection, node } = key
            && let Some(object) = node.object_ref()
            && cx.has_global::<GlobalTabStore>()
        {
            let tabs = cx.global::<GlobalTabStore>().0.clone();
            tabs.update(cx, |tabs, cx| {
                tabs.open_table(*connection, object, cx);
            });
        }
    }

    fn activate(&mut self, key: &SidebarNodeKey, window: &mut Window, cx: &mut Context<Self>) {
        match key {
            SidebarNodeKey::Object { node, .. } if node.object_ref().is_some() => {
                Self::open_table(key, cx);
            }
            SidebarNodeKey::Folder(_)
            | SidebarNodeKey::Connection(_)
            | SidebarNodeKey::Object { .. } => self.toggle(key, window, cx),
            SidebarNodeKey::Placeholder { .. } => {}
        }
    }

    fn open_query(key: &SidebarNodeKey, cx: &mut App) {
        let (Some(connection), database) = (
            key.connection(),
            match key {
                SidebarNodeKey::Object { node, .. } => node.query_database(),
                SidebarNodeKey::Folder(_)
                | SidebarNodeKey::Connection(_)
                | SidebarNodeKey::Placeholder { .. } => None,
            },
        ) else {
            return;
        };
        if cx.has_global::<GlobalTabStore>() {
            let tabs = cx.global::<GlobalTabStore>().0.clone();
            tabs.update(cx, |tabs, cx| {
                tabs.open_query(connection, database, cx);
            });
        }
    }

    fn refresh(key: &SidebarNodeKey, window: &mut Window, cx: &mut Context<Self>) {
        let SidebarNodeKey::Object { connection, node } = key else {
            return;
        };
        let path = node.children_path().unwrap_or_else(|| node.parent_path());
        let task = sessions(cx).update(cx, |s, cx| {
            s.refresh_children(*connection, path, window, cx)
        });
        Self::report(task, window, cx);
    }

    fn connect_toggle(&mut self, id: ConnectionId, window: &mut Window, cx: &mut Context<Self>) {
        let sessions = sessions(cx);
        let idle = matches!(
            sessions.read(cx).status(id),
            SessionStatus::Disconnected | SessionStatus::Failed(_)
        );
        if idle {
            let task = sessions.update(cx, |s, cx| s.ensure_connected(id, window, cx));
            Self::report(task, window, cx);
        } else {
            sessions.update(cx, |s, cx| s.disconnect(id, cx));
            self.expanded.retain(|key| key.connection() != Some(id));
            cx.notify();
        }
    }

    /// Runs a context-menu action on `key`; mutating actions do nothing on a read-only store.
    pub fn perform(
        &mut self,
        action: MenuAction,
        key: &SidebarNodeKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if action.mutates() && Self::read_only(cx) {
            return;
        }
        match (action, key) {
            (MenuAction::NewConnectionHere, SidebarNodeKey::Folder(id)) => {
                connection::open_new(DropPosition::Into(*id), window, cx);
            }
            (MenuAction::NewFolderHere, SidebarNodeKey::Folder(id)) => {
                folder::open_new(DropPosition::Into(*id), window, cx);
            }
            (MenuAction::Rename, SidebarNodeKey::Folder(id)) => {
                folder::open_rename(*id, window, cx);
            }
            (MenuAction::Delete, SidebarNodeKey::Folder(id)) => folder::delete(*id, window, cx),
            (MenuAction::Delete, SidebarNodeKey::Connection(id)) => {
                connection::delete(*id, window, cx);
            }
            (MenuAction::Edit, SidebarNodeKey::Connection(id)) => {
                connection::open_edit(*id, window, cx);
            }
            (MenuAction::Duplicate, SidebarNodeKey::Connection(id)) => {
                connection::duplicate(*id, window, cx);
            }
            (MenuAction::ChangeColor, SidebarNodeKey::Connection(id)) => {
                color::open(*id, window, cx);
            }
            (MenuAction::ConnectToggle, SidebarNodeKey::Connection(id)) => {
                self.connect_toggle(*id, window, cx);
            }
            (MenuAction::NewQuery, _) => Self::open_query(key, cx),
            (MenuAction::Refresh, _) => Self::refresh(key, window, cx),
            (MenuAction::Open, _) => Self::open_table(key, cx),
            (
                MenuAction::NewConnectionHere
                | MenuAction::NewFolderHere
                | MenuAction::Rename
                | MenuAction::Delete
                | MenuAction::Edit
                | MenuAction::Duplicate
                | MenuAction::ChangeColor
                | MenuAction::ConnectToggle,
                _,
            ) => {}
        }
    }

    fn status(connection: ConnectionId, cx: &App) -> Status {
        match sessions(cx).read(cx).status(connection) {
            SessionStatus::Disconnected | SessionStatus::Connected(_) => Status::Idle,
            SessionStatus::Connecting => Status::Connecting,
            SessionStatus::Failed(_) => Status::Failed,
        }
    }

    fn selected_index(&self, rows: &[Row]) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        rows.iter().position(|row| row.key == *selected)
    }

    fn step(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        if rows.is_empty() {
            return;
        }
        let last = rows.len() - 1;
        let next = match self.selected_index(&rows) {
            Some(current) => current.saturating_add_signed(delta).min(last),
            None => 0,
        };
        self.select_index(&rows, next, window, cx);
    }

    fn collapse_or_parent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        let Some(current) = self.selected_index(&rows) else {
            return;
        };
        if rows[current].expanded {
            let key = rows[current].key.clone();
            self.toggle(&key, window, cx);
        } else if let Some(parent) = rows[..current]
            .iter()
            .rposition(|row| row.depth < rows[current].depth)
        {
            self.select_index(&rows, parent, window, cx);
        }
    }

    fn expand_or_child(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        let Some(current) = self.selected_index(&rows) else {
            return;
        };
        if rows[current].expandable && !rows[current].expanded {
            let key = rows[current].key.clone();
            self.toggle(&key, window, cx);
        } else if rows[current].expanded {
            self.select_index(&rows, current + 1, window, cx);
        }
    }

    fn move_selected(&mut self, down: bool, cx: &mut Context<Self>) {
        let Some(node) = self.selected.as_ref().and_then(node_ref) else {
            return;
        };
        // An edge or read-only store simply leaves the order alone.
        drop(connection_store(cx).update(cx, |store, cx| {
            if down {
                store.move_down(cx, node)
            } else {
                store.move_up(cx, node)
            }
        }));
    }

    /// Commits a drop of `node` at `position`; invalid or refused moves do nothing.
    fn drop_at(node: NodeRef, position: Option<DropPosition>, window: &mut Window, cx: &mut App) {
        let Some(position) = position else {
            return;
        };
        let moved =
            connection_store(cx).update(cx, |store, cx| store.move_node(cx, node, position));
        if let Err(crate::catalog::store::StoreError::Io(error)) = moved {
            notify(
                window,
                cx,
                Notice {
                    level: NoticeLevel::Error,
                    message: error.to_string().into(),
                },
            );
        }
    }
}

fn row_icon(kind: RowKind, expanded: bool) -> Option<IconName> {
    match kind {
        RowKind::Folder if expanded => Some(IconName::FolderOpen),
        RowKind::Folder => Some(IconName::Folder),
        RowKind::Database => Some(IconName::Database),
        RowKind::Schema => Some(IconName::FolderTree),
        RowKind::Table => Some(IconName::Table),
        RowKind::View => Some(IconName::Eye),
        RowKind::Connection | RowKind::Placeholder(_) => None,
    }
}

impl SidebarView {
    fn leading(row: &Row, cx: &App) -> AnyElement {
        let theme = cx.theme();
        match row.kind {
            RowKind::Connection => {
                let color = row
                    .key
                    .connection()
                    .and_then(|id| {
                        connection_store(cx)
                            .read(cx)
                            .profile(id)
                            .map(|p| p.color.effective())
                    })
                    .map_or(theme.muted_foreground, Rgb::to_hsla);
                div().size_2().rounded_full().bg(color).into_any_element()
            }
            RowKind::Placeholder(PlaceholderKind::Loading) => {
                Spinner::new().xsmall().into_any_element()
            }
            RowKind::Placeholder(PlaceholderKind::Error) => Icon::new(IconName::CircleAlert)
                .xsmall()
                .text_color(theme.danger)
                .into_any_element(),
            RowKind::Placeholder(PlaceholderKind::Empty) => div().into_any_element(),
            RowKind::Folder
            | RowKind::Database
            | RowKind::Schema
            | RowKind::Table
            | RowKind::View => row_icon(row.kind, row.expanded).map_or_else(
                || div().into_any_element(),
                |icon| Icon::new(icon).xsmall().into_any_element(),
            ),
        }
    }

    fn trailing(row: &Row, cx: &App) -> Vec<AnyElement> {
        let theme = cx.theme();
        let Some(id) = row
            .key
            .connection()
            .filter(|_| row.kind == RowKind::Connection)
        else {
            return Vec::new();
        };
        let badge = connection_store(cx).read(cx).profile(id).map(|p| {
            div()
                .flex_none()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(p.driver.display_name())
                .into_any_element()
        });
        let status = match Self::status(id, cx) {
            Status::Idle => None,
            Status::Connecting => Some(Spinner::new().xsmall().into_any_element()),
            Status::Failed => Some(
                Icon::new(IconName::TriangleAlert)
                    .xsmall()
                    .text_color(theme.warning)
                    .into_any_element(),
            ),
        };
        badge.into_iter().chain(status).collect()
    }

    fn chevron(row: &Row, cx: &mut Context<Self>) -> AnyElement {
        let key = row.key.clone();
        let slot = div()
            .w(px(16.))
            .h_full()
            .flex()
            .flex_none()
            .items_center()
            .justify_center();
        if !row.expandable {
            return slot.into_any_element();
        }
        let icon = if row.expanded {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        };
        slot.id(SharedString::from(format!("sidebar-toggle:{key}")))
            .test_support()
            .cursor_pointer()
            .child(Icon::new(icon).xsmall())
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.toggle(&key, window, cx);
            }))
            .into_any_element()
    }

    fn on_drag_over(
        &mut self,
        key: &SidebarNodeKey,
        kind: RowKind,
        event: &DragMoveEvent<DraggedCatalogItem>,
        cx: &mut Context<Self>,
    ) {
        let over = event.bounds.contains(&event.event.position);
        if !over {
            if self
                .hover
                .as_ref()
                .is_some_and(|(hovered, _)| hovered == key)
            {
                self.hover = None;
                cx.notify();
            }
            return;
        }
        let dragged = event.drag(cx).node;
        let store = connection_store(cx).read(cx);
        let next = (!store.is_read_only())
            .then(|| zone_for(kind, event.event.position.y, event.bounds))
            .flatten()
            .filter(|zone| {
                drop_position(key, *zone)
                    .is_some_and(|position| is_valid_drop(store.items(), dragged, position))
            })
            .map(|zone| (key.clone(), zone));
        if self.hover != next {
            self.hover = next;
            cx.notify();
        }
    }

    fn menu_builder(
        view: Entity<Self>,
        key: SidebarNodeKey,
        specs: Vec<MenuItemSpec>,
        connected: bool,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        move |menu, _, _| {
            specs.iter().fold(menu, |menu, spec| {
                let (view, key, action) = (view.clone(), key.clone(), spec.action);
                let label = if action == MenuAction::ConnectToggle && connected {
                    "Disconnect"
                } else {
                    spec.label
                };
                menu.item(PopupMenuItem::new(label).disabled(spec.disabled).on_click(
                    move |_, window, cx| {
                        view.update(cx, |this, cx| this.perform(action, &key, window, cx));
                    },
                ))
            })
        }
    }

    fn drop_bar(zone: DropZone, color: Hsla) -> Option<Div> {
        let bar = div().absolute().left_0().right_0().h(px(2.)).bg(color);
        match zone {
            DropZone::Before => Some(bar.top_0()),
            DropZone::After => Some(bar.bottom_0()),
            DropZone::Into => None,
        }
    }

    fn row_element(&self, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        let key = row.key.clone();
        let kind = row.kind;
        let drop_zone = self
            .hover
            .as_ref()
            .filter(|(hovered, _)| *hovered == key && cx.has_active_drag())
            .map(|(_, zone)| *zone);
        let connected = key.connection().is_some_and(|id| {
            matches!(
                sessions(cx).read(cx).status(id),
                SessionStatus::Connecting | SessionStatus::Connected(_)
            )
        });
        let menu = Self::menu_builder(
            cx.entity(),
            key.clone(),
            menu_items(kind, Self::read_only(cx)),
            connected,
        );
        let (leading, trailing, chevron) = (
            Self::leading(row, cx),
            Self::trailing(row, cx),
            Self::chevron(row, cx),
        );
        let theme = cx.theme();
        let (accent, hover_bg, drop_bg, line) = (
            theme.list_active,
            theme.list_hover,
            theme.drop_target,
            theme.drag_border,
        );
        let muted = matches!(
            kind,
            RowKind::Placeholder(PlaceholderKind::Empty | PlaceholderKind::Loading)
        );
        let text_color = if muted {
            theme.muted_foreground
        } else {
            theme.sidebar_foreground
        };
        let selected = self.selected.as_ref() == Some(&key);
        let indent = f32::from(u16::try_from(row.depth).unwrap_or(u16::MAX));
        let (click_key, drag_key, drop_key, mouse_key) =
            (key.clone(), key.clone(), key.clone(), key.clone());
        div()
            .id(SharedString::from(format!("sidebar-row:{key}")))
            .role(Role::TreeItem)
            .aria_label(row.label.clone())
            .when(row.expandable, |this| this.aria_expanded(row.expanded))
            .relative()
            .flex()
            .items_center()
            .gap_1()
            .h(px(ROW_HEIGHT))
            .w_full()
            .pl(px(6. + 14. * indent))
            .pr_2()
            .text_sm()
            .text_color(text_color)
            .when(selected, |this| this.bg(accent))
            .when(!selected, |this| {
                this.hover(move |style| style.bg(hover_bg))
            })
            .when(drop_zone == Some(DropZone::Into), |this| this.bg(drop_bg))
            .children(drop_zone.and_then(|zone| Self::drop_bar(zone, line)))
            .child(chevron)
            .child(leading)
            .child(div().flex_1().min_w_0().truncate().child(row.label.clone()))
            .children(trailing)
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.select(click_key.clone(), window, cx);
                if event.click_count() >= 2 {
                    this.activate(&click_key, window, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, window, cx| this.select(mouse_key.clone(), window, cx)),
            )
            .when_some(DraggedCatalogItem::of(&key, &row.label), |this, item| {
                this.on_drag(item, |item, _, _, cx| {
                    cx.new(|_| DragPreview(item.label.clone()))
                })
            })
            .on_drag_move(cx.listener(
                move |this, event: &DragMoveEvent<DraggedCatalogItem>, _, cx| {
                    this.on_drag_over(&drag_key, kind, event, cx);
                },
            ))
            .on_drop(
                cx.listener(move |this, item: &DraggedCatalogItem, window, cx| {
                    let hovered = this.hover.take().filter(|(h, _)| *h == drop_key);
                    let position = hovered.and_then(|(target, zone)| drop_position(&target, zone));
                    Self::drop_at(item.node, position, window, cx);
                    cx.notify();
                }),
            )
            .test_support()
            .context_menu(menu)
            .into_any_element()
    }

    fn header(read_only: bool, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .h(px(32.))
            .px_2()
            .gap_1()
            .flex_none()
            .justify_end()
            .child(
                Button::new("sidebar-new-connection")
                    .icon(IconName::Plus)
                    .ghost()
                    .xsmall()
                    .disabled(read_only)
                    .on_click(cx.listener(|_, _, window, cx| {
                        connection::open_new(DropPosition::RootEnd, window, cx);
                    })),
            )
            .child(
                Button::new("sidebar-new-folder")
                    .icon(IconName::FolderPlus)
                    .ghost()
                    .xsmall()
                    .disabled(read_only)
                    .on_click(cx.listener(|_, _, window, cx| {
                        folder::open_new(DropPosition::RootEnd, window, cx);
                    })),
            )
    }

    fn empty_state(read_only: bool, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("sidebar-empty")
            .test_support()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_2()
            .text_color(cx.theme().muted_foreground)
            .child("No connections yet")
            .child(
                Button::new("sidebar-empty-new-connection")
                    .label("New connection")
                    .disabled(read_only)
                    .on_click(cx.listener(|_, _, window, cx| {
                        connection::open_new(DropPosition::RootEnd, window, cx);
                    })),
            )
    }

    fn list(&mut self, rows: Vec<Row>, cx: &mut Context<Self>) -> impl IntoElement {
        let count = rows.len();
        self.visible = rows;
        let list = uniform_list(
            "sidebar-rows",
            count,
            cx.processor(|this, range: Range<usize>, _, cx| {
                let rows: Vec<Row> = this.visible.get(range).unwrap_or_default().to_vec();
                rows.iter()
                    .map(|row| this.row_element(row, cx))
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.scroll)
        .size_full();
        div()
            .id("sidebar-list")
            .test_support()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(list)
            .on_drop(cx.listener(|this, item: &DraggedCatalogItem, window, cx| {
                // Only the empty area gets here: a row consumes its own drops.
                this.hover = None;
                Self::drop_at(item.node, Some(DropPosition::RootEnd), window, cx);
                cx.notify();
            }))
    }
}

impl Render for SidebarView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows(cx);
        let empty = connection_store(cx).read(cx).items().is_empty();
        let read_only = Self::read_only(cx);
        let body = if empty {
            Self::empty_state(read_only, cx).into_any_element()
        } else {
            self.list(rows, cx).into_any_element()
        };
        let background = cx.theme().sidebar;
        v_flex()
            .size_full()
            .bg(background)
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(
                cx.listener(|this, _: &SelectPrevious, window, cx| this.step(-1, window, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectNext, window, cx| this.step(1, window, cx)))
            .on_action(cx.listener(|this, _: &CollapseOrParent, window, cx| {
                this.collapse_or_parent(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &ExpandOrChild, window, cx| this.expand_or_child(window, cx)),
            )
            .on_action(cx.listener(|this, _: &Activate, window, cx| {
                if let Some(key) = this.selected.clone() {
                    this.activate(&key, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &DeleteSelected, window, cx| {
                if let Some(key) = this.selected.clone() {
                    this.perform(MenuAction::Delete, &key, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &MoveUp, _, cx| this.move_selected(false, cx)))
            .on_action(cx.listener(|this, _: &MoveDown, _, cx| this.move_selected(true, cx)))
            .child(Self::header(read_only, cx))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::sync::Arc;

    use futures::FutureExt as _;
    use futures::future::BoxFuture;
    use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
    use gpui_kit::{
        AnyWindowHandle, App, AppContext as _, Bounds, Entity, Point, ScrollDelta, TestAppContext,
        Window, WindowBounds, WindowOptions, point, px, size,
    };

    use super::{MenuAction, SidebarView, init as init_keys, menu_items};
    use crate::catalog::model::{
        CatalogFile, CatalogItem, ConnectionId, ConnectionProfile, Driver, Endpoint, FolderId,
        PasswordStorage,
    };
    use crate::catalog::ops::DropPosition;
    use crate::catalog::store::{ConnectionStore, GlobalConnectionStore, init};
    use crate::db::backend::{BackendFactory, DbBackend, QueryConnection, install_factory};
    use crate::db::fake::{FakeEndpoint, FakeFactory, FakeServer};
    use crate::db::model::{
        ColumnInfo, ConnectParams, DbError, Dialect, ObjectKind, ObjectNode, ObjectPath, ObjectRef,
        Page, PageRequest, TlsMode,
    };
    use crate::db::runtime::DbRuntime;
    use crate::paths::ConfigPaths;
    use crate::prompts::{ConfirmChoice, NoticeLevel, ScriptedPrompts, install_scripted};
    use crate::secrets::{self, MemorySecretStore};
    use crate::settings::WindowMode;
    use crate::sidebar::node::{ObjectNodeKey, PlaceholderKind, Row, RowKind, SidebarNodeKey};
    use crate::workspace::tabs::{self, GlobalTabStore};

    type Build = Box<dyn FnOnce(&mut ConnectionStore, &mut gpui_kit::Context<ConnectionStore>)>;

    struct Fx {
        dir: tempfile::TempDir,
        window: AnyWindowHandle,
        view: Entity<SidebarView>,
        store: Entity<ConnectionStore>,
        prompts: Rc<ScriptedPrompts>,
        server: Arc<FakeServer>,
    }

    fn pg(name: &str) -> ConnectionProfile {
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
            PasswordStorage::None,
        )
    }

    fn add(
        s: &mut ConnectionStore,
        cx: &mut gpui_kit::Context<ConnectionStore>,
        p: ConnectionProfile,
        at: DropPosition,
    ) -> ConnectionId {
        let id = p.id;
        s.add_connection(cx, p, at).expect("add");
        id
    }

    fn open(
        cx: &mut TestAppContext,
        factory: Arc<dyn BackendFactory>,
        read_only: bool,
        build: Build,
    ) -> Fx {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = ConfigPaths::at(dir.path().to_path_buf());
        let secret_store = Arc::new(MemorySecretStore::default());
        let prompts = cx.update(|cx| {
            gpui_kit::init(cx);
            init_keys(cx);
            cx.set_reduce_motion(true);
            init(cx, &paths);
            secrets::install(cx, secret_store);
            cx.set_global(DbRuntime::inline());
            install_factory(cx, factory);
            tabs::init(cx, WindowMode::Single);
            crate::sessions::init(cx);
            install_scripted(cx)
        });
        let store = cx.update(|cx| cx.global::<GlobalConnectionStore>().0.clone());
        cx.update(|cx| store.update(cx, build));
        let store = if read_only {
            let file = dir.path().join("connections.json");
            let text = std::fs::read_to_string(&file).expect("saved");
            let newer: CatalogFile = serde_json::from_str(&text).expect("parse");
            let mut value = serde_json::to_value(newer).expect("value");
            value["version"] = 99.into();
            std::fs::write(&file, value.to_string()).expect("rewrite");
            cx.update(|cx| {
                init(cx, &paths);
                cx.global::<GlobalConnectionStore>().0.clone()
            })
        } else {
            store
        };
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                Point::default(),
                size(px(320.), px(700.)),
            ))),
            ..Default::default()
        };
        let (window, view) = cx
            .update(|cx| gpui_kit::open_window(options, cx, |_, cx| cx.new(SidebarView::new)))
            .expect("window opens");
        Fx {
            dir,
            window,
            view,
            store,
            prompts,
            server: Arc::new(FakeServer::sample(Dialect::Postgres)),
        }
    }

    /// A window over a fake PostgreSQL server (`db.test`).
    fn setup(cx: &mut TestAppContext, build: Build) -> Fx {
        let factory = FakeFactory::new();
        let server = Arc::new(FakeServer::sample(Dialect::Postgres));
        factory.register(FakeEndpoint::Host("db.test".into()), server.clone());
        let mut fx = open(cx, Arc::new(factory), false, build);
        fx.server = server;
        fx
    }

    fn ui<R>(cx: &mut TestAppContext, fx: &Fx, f: impl FnOnce(&mut Window, &mut App) -> R) -> R {
        cx.update_window(fx.window, |_, window, cx| {
            window.render_frame(cx);
            let out = f(window, cx);
            window.render_frame(cx);
            out
        })
        .expect("window")
    }

    fn rows(cx: &mut TestAppContext, fx: &Fx) -> Vec<Row> {
        cx.update(|cx| fx.view.read(cx).rows(cx))
    }

    fn names(cx: &mut TestAppContext, fx: &Fx) -> Vec<String> {
        rows(cx, fx).iter().map(|r| r.label.to_string()).collect()
    }

    fn row_id(key: &SidebarNodeKey) -> String {
        format!("sidebar-row:{key}")
    }

    fn toggle_id(key: &SidebarNodeKey) -> String {
        format!("sidebar-toggle:{key}")
    }

    fn click(cx: &mut TestAppContext, fx: &Fx, id: String) {
        ui(cx, fx, |window, cx| window.click(id, cx));
        cx.run_until_parked();
    }

    fn toggle(cx: &mut TestAppContext, fx: &Fx, key: &SidebarNodeKey) {
        click(cx, fx, toggle_id(key));
    }

    fn table(
        connection: ConnectionId,
        name: &str,
        database: Option<&str>,
        schema: Option<&str>,
    ) -> SidebarNodeKey {
        SidebarNodeKey::Object {
            connection,
            node: ObjectNodeKey {
                kind: ObjectKind::Table,
                database: database.map(str::to_owned),
                schema: schema.map(str::to_owned),
                name: name.to_owned(),
            },
        }
    }

    fn db_key(connection: ConnectionId, name: &str) -> SidebarNodeKey {
        SidebarNodeKey::Object {
            connection,
            node: ObjectNodeKey {
                kind: ObjectKind::Database,
                database: None,
                schema: None,
                name: name.to_owned(),
            },
        }
    }

    fn schema_key(connection: ConnectionId, database: &str, name: &str) -> SidebarNodeKey {
        SidebarNodeKey::Object {
            connection,
            node: ObjectNodeKey {
                kind: ObjectKind::Schema,
                database: Some(database.to_owned()),
                schema: None,
                name: name.to_owned(),
            },
        }
    }

    fn one_connection(cx: &mut TestAppContext) -> (Fx, ConnectionId) {
        let id = Rc::new(std::cell::Cell::new(None));
        let sink = id.clone();
        let fx = setup(
            cx,
            Box::new(move |s, cx| sink.set(Some(add(s, cx, pg("main"), DropPosition::RootEnd)))),
        );
        let id = id.get().expect("added");
        (fx, id)
    }

    fn expand_to_tables(cx: &mut TestAppContext, fx: &Fx, id: ConnectionId) {
        toggle(cx, fx, &SidebarNodeKey::Connection(id));
        toggle(cx, fx, &db_key(id, "app"));
        toggle(cx, fx, &schema_key(id, "app", "public"));
    }

    #[gpui_kit::test]
    fn rows_expose_tree_item_semantics(cx: &mut TestAppContext) {
        // Given: one connection and an empty-folder-free catalog.
        let (fx, id) = one_connection(cx);
        // Then: its row is a tree item that reports it is collapsed.
        ui(cx, &fx, |window, _| {
            let row = window.find(row_id(&SidebarNodeKey::Connection(id)));
            assert_eq!(row.role(), Some(gpui_kit::Role::TreeItem));
            assert_eq!(row.expanded(), Some(false));
        });
    }

    #[gpui_kit::test]
    fn chevron_toggles_but_row_click_only_selects(cx: &mut TestAppContext) {
        // Given: one collapsed connection.
        let (fx, id) = one_connection(cx);
        let key = SidebarNodeKey::Connection(id);
        // When: clicking the row.
        click(cx, &fx, row_id(&key));
        // Then: it is selected and still collapsed.
        assert_eq!(
            cx.update(|cx| fx.view.read(cx).selected_connection()),
            Some(id)
        );
        assert_eq!(rows(cx, &fx).len(), 1);
        // When: clicking the chevron.
        toggle(cx, &fx, &key);
        // Then: it expands.
        assert!(rows(cx, &fx)[0].expanded);
    }

    #[gpui_kit::test]
    fn folder_toggle_persists_expanded(cx: &mut TestAppContext) {
        // Given: a collapsed folder with a connection.
        let folder = Rc::new(std::cell::Cell::new(None));
        let sink = folder.clone();
        let fx = setup(
            cx,
            Box::new(move |s, cx| {
                let f = s.add_folder(cx, "F", DropPosition::RootEnd).expect("F");
                add(s, cx, pg("in"), DropPosition::Into(f));
                sink.set(Some(f));
            }),
        );
        let key = SidebarNodeKey::Folder(folder.get().expect("folder"));
        assert_eq!(rows(cx, &fx).len(), 1);
        // When: toggling it open.
        toggle(cx, &fx, &key);
        // Then: the connection shows and the store persisted the flag.
        assert_eq!(rows(cx, &fx).len(), 2);
        let saved: CatalogFile = serde_json::from_str(
            &std::fs::read_to_string(fx.dir.path().join("connections.json")).expect("file"),
        )
        .expect("parse");
        assert!(matches!(&saved.items[0], CatalogItem::Folder(f) if f.expanded));
        // When: toggling again.
        toggle(cx, &fx, &key);
        // Then: it collapses.
        assert_eq!(rows(cx, &fx).len(), 1);
    }

    #[gpui_kit::test]
    fn expanding_postgres_shows_databases_schemas_then_tables_before_views(
        cx: &mut TestAppContext,
    ) {
        // Given: a PostgreSQL connection.
        let (fx, id) = one_connection(cx);
        // When: expanding connection, then database `app`.
        toggle(cx, &fx, &SidebarNodeKey::Connection(id));
        // Then: both databases are listed.
        assert_eq!(names(cx, &fx), ["main", "analytics", "app"]);
        toggle(cx, &fx, &db_key(id, "app"));
        assert_eq!(names(cx, &fx), ["main", "analytics", "app", "public"]);
        // When: expanding the schema.
        toggle(cx, &fx, &schema_key(id, "app", "public"));
        // Then: tables come before the view.
        let kinds: Vec<_> = rows(cx, &fx).iter().skip(4).map(|r| r.kind).collect();
        assert_eq!(kinds, [RowKind::Table, RowKind::Table, RowKind::View]);
        assert_eq!(names(cx, &fx)[4..], ["orders", "users", "active_users"]);
    }

    #[gpui_kit::test]
    fn children_failure_shows_error_row(cx: &mut TestAppContext) {
        // Given: a server whose next catalog lookup fails.
        let (fx, id) = one_connection(cx);
        fx.server.fail_children(DbError::Query {
            message: "boom".into(),
            code: None,
        });
        let notices_before = fx.prompts.notices().len();
        // When: expanding the connection.
        toggle(cx, &fx, &SidebarNodeKey::Connection(id));
        // Then: an error placeholder row and an error notice appear.
        let all = rows(cx, &fx);
        assert_eq!(all[1].kind, RowKind::Placeholder(PlaceholderKind::Error));
        let notices = fx.prompts.notices();
        assert_eq!(notices.len(), notices_before + 1);
        assert_eq!(notices[notices_before].level, NoticeLevel::Error);
    }

    #[gpui_kit::test]
    fn double_click_on_a_table_opens_one_tab(cx: &mut TestAppContext) {
        // Given: the `orders` table is visible.
        let (fx, id) = one_connection(cx);
        expand_to_tables(cx, &fx, id);
        let orders = row_id(&table(id, "orders", Some("app"), Some("public")));
        let tab_count = |cx: &mut TestAppContext| {
            cx.update(|cx| cx.global::<GlobalTabStore>().0.read(cx).tabs().len())
        };
        // When: double-clicking it.
        ui(cx, &fx, |window, cx| {
            window.double_click(orders.clone(), cx);
        });
        cx.run_until_parked();
        // Then: one tab exists; a second double-click keeps it.
        assert_eq!(tab_count(cx), 1);
        ui(cx, &fx, |window, cx| {
            window.double_click(orders.clone(), cx);
        });
        cx.run_until_parked();
        assert_eq!(tab_count(cx), 1);
    }

    fn two_roots(cx: &mut TestAppContext) -> (Fx, Vec<SidebarNodeKey>) {
        let keys = Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = keys.clone();
        let fx = setup(
            cx,
            Box::new(move |s, cx| {
                let mut keys = sink.borrow_mut();
                keys.push(SidebarNodeKey::Connection(add(
                    s,
                    cx,
                    pg("a"),
                    DropPosition::RootEnd,
                )));
                keys.push(SidebarNodeKey::Connection(add(
                    s,
                    cx,
                    pg("b"),
                    DropPosition::RootEnd,
                )));
                keys.push(SidebarNodeKey::Folder(
                    s.add_folder(cx, "F", DropPosition::RootEnd).expect("F"),
                ));
            }),
        );
        let keys = keys.borrow().clone();
        (fx, keys)
    }

    fn root_names(cx: &mut TestAppContext, fx: &Fx) -> Vec<String> {
        cx.update(|cx| {
            fx.store
                .read(cx)
                .items()
                .iter()
                .map(|item| match item {
                    CatalogItem::Connection(p) => p.name.clone(),
                    CatalogItem::Folder(f) => format!("[{}:{}]", f.name, f.items.len()),
                })
                .collect()
        })
    }

    #[gpui_kit::test]
    fn dragging_onto_a_folder_center_moves_the_connection_inside(cx: &mut TestAppContext) {
        // Given: connections a, b and folder F.
        let (fx, keys) = two_roots(cx);
        // When: dragging `a` onto the center of F.
        ui(cx, &fx, |window, cx| {
            window.drag_to(row_id(&keys[0]), row_id(&keys[2]), cx);
        });
        // Then: F holds a, persisted.
        assert_eq!(root_names(cx, &fx), ["b", "[F:1]"]);
        let saved: CatalogFile = serde_json::from_str(
            &std::fs::read_to_string(fx.dir.path().join("connections.json")).expect("file"),
        )
        .expect("parse");
        assert!(matches!(&saved.items[1], CatalogItem::Folder(f) if f.items.len() == 1));
    }

    #[gpui_kit::test]
    fn dragging_onto_the_top_of_a_row_inserts_before_it(cx: &mut TestAppContext) {
        // Given: connections a, b and folder F.
        let (fx, keys) = two_roots(cx);
        // When: dragging `b` to the top quarter of `a`.
        ui(cx, &fx, |window, cx| {
            let from = window.find(row_id(&keys[1])).bounds().center();
            let target = window.find(row_id(&keys[0])).bounds();
            let to = point(
                target.center().x,
                target.origin.y + target.size.height * 0.1,
            );
            window.drag(from, to, cx);
        });
        // Then: b comes first.
        assert_eq!(root_names(cx, &fx), ["b", "a", "[F:0]"]);
    }

    #[gpui_kit::test]
    fn dropping_below_the_rows_moves_to_the_root_end(cx: &mut TestAppContext) {
        // Given: connections a, b and folder F.
        let (fx, keys) = two_roots(cx);
        // When: dragging `a` into the empty area under the rows.
        ui(cx, &fx, |window, cx| {
            let from = window.find(row_id(&keys[0])).bounds().center();
            let list = window.find("sidebar-list").bounds();
            window.drag(from, point(list.center().x, list.bottom() - px(10.)), cx);
        });
        // Then: a is last.
        assert_eq!(root_names(cx, &fx), ["b", "[F:0]", "a"]);
    }

    #[gpui_kit::test]
    fn folder_cannot_be_dropped_into_its_child(cx: &mut TestAppContext) {
        // Given: folder F (expanded) containing folder G.
        let ids = Rc::new(std::cell::RefCell::new(Vec::<FolderId>::new()));
        let sink = ids.clone();
        let fx = setup(
            cx,
            Box::new(move |s, cx| {
                let f = s.add_folder(cx, "F", DropPosition::RootEnd).expect("F");
                s.set_folder_expanded(cx, f, true).expect("expand");
                let g = s.add_folder(cx, "G", DropPosition::Into(f)).expect("G");
                sink.borrow_mut().extend([f, g]);
            }),
        );
        let (f, g) = (ids.borrow()[0], ids.borrow()[1]);
        let before = root_names(cx, &fx);
        // When: dragging F onto the center of G.
        ui(cx, &fx, |window, cx| {
            window.drag_to(
                row_id(&SidebarNodeKey::Folder(f)),
                row_id(&SidebarNodeKey::Folder(g)),
                cx,
            );
        });
        // Then: nothing moved.
        assert_eq!(root_names(cx, &fx), before);
        assert_eq!(rows(cx, &fx).len(), 2);
    }

    #[gpui_kit::test]
    fn alt_down_moves_the_selected_connection(cx: &mut TestAppContext) {
        // Given: connections a, b with `a` selected.
        let (fx, keys) = two_roots(cx);
        click(cx, &fx, row_id(&keys[0]));
        // When: pressing alt-down.
        ui(cx, &fx, |window, cx| window.press("alt-down", cx));
        // Then: a moved below b.
        assert_eq!(root_names(cx, &fx), ["b", "a", "[F:0]"]);
    }

    #[gpui_kit::test]
    fn arrow_keys_move_the_selection(cx: &mut TestAppContext) {
        // Given: `a` selected.
        let (fx, keys) = two_roots(cx);
        click(cx, &fx, row_id(&keys[0]));
        // When: pressing down.
        ui(cx, &fx, |window, cx| window.press("down", cx));
        // Then: `b` is selected.
        let selected = cx.update(|cx| fx.view.read(cx).selected_connection());
        assert_eq!(
            selected,
            Some(match keys[1] {
                SidebarNodeKey::Connection(id) => id,
                _ => unreachable!(),
            })
        );
    }

    #[gpui_kit::test]
    fn delete_key_removes_a_confirmed_connection(cx: &mut TestAppContext) {
        // Given: connections a, b with `a` selected and a scripted Confirm.
        let (fx, keys) = two_roots(cx);
        click(cx, &fx, row_id(&keys[0]));
        fx.prompts.queue_confirm(ConfirmChoice::Confirm);
        // When: pressing delete.
        ui(cx, &fx, |window, cx| window.press("delete", cx));
        cx.run_until_parked();
        // Then: it asked once and removed `a`.
        assert_eq!(fx.prompts.confirm_requests().len(), 1);
        assert_eq!(root_names(cx, &fx), ["b", "[F:0]"]);
    }

    #[gpui_kit::test]
    fn read_only_store_blocks_drag_and_menu_mutations(cx: &mut TestAppContext) {
        // Given: a newer-version (read-only) catalog with connections a, b and folder F.
        let factory = FakeFactory::new();
        let fx = open(
            cx,
            Arc::new(factory),
            true,
            Box::new(|s, cx| {
                add(s, cx, pg("a"), DropPosition::RootEnd);
                add(s, cx, pg("b"), DropPosition::RootEnd);
                s.add_folder(cx, "F", DropPosition::RootEnd).expect("F");
            }),
        );
        let keys: Vec<_> = rows(cx, &fx).iter().map(|r| r.key.clone()).collect();
        assert_eq!(keys.len(), 3);
        let before = root_names(cx, &fx);
        // When: dragging `a` onto F, and running the menu's Delete on `a`.
        ui(cx, &fx, |window, cx| {
            window.drag_to(row_id(&keys[0]), row_id(&keys[2]), cx);
        });
        fx.prompts.queue_confirm(ConfirmChoice::Confirm);
        let key = keys[0].clone();
        cx.update_window(fx.window, |_, window, cx| {
            fx.view.update(cx, |view, cx| {
                view.perform(MenuAction::Delete, &key, window, cx);
            });
        })
        .expect("window");
        cx.run_until_parked();
        // Then: nothing changed, no confirmation was asked, and mutating items are disabled.
        assert_eq!(root_names(cx, &fx), before);
        assert_eq!(fx.prompts.confirm_requests().len(), 0);
        let items = menu_items(RowKind::Connection, true);
        assert!(
            items
                .iter()
                .filter(|i| i.action == MenuAction::Delete)
                .all(|i| i.disabled)
        );
        assert!(
            items
                .iter()
                .any(|i| i.action == MenuAction::NewQuery && !i.disabled)
        );
    }

    #[gpui_kit::test]
    async fn empty_catalog_shows_the_empty_state(cx: &mut TestAppContext) {
        // Given: no connections.
        let fx = setup(cx, Box::new(|_, _| {}));
        // Then: the empty state and its button are visible, the list is not.
        ui(cx, &fx, |window, _| {
            assert!(window.find("sidebar-empty").visible());
            assert!(window.find("sidebar-empty-new-connection").visible());
            assert!(window.try_find("sidebar-list").is_none());
        });
        // When: clicking the button.
        click(cx, &fx, "sidebar-empty-new-connection".to_owned());
        // Then: the new-connection dialog opens.
        cx.wait_for(fx.window, std::time::Duration::from_secs(2), |window, _| {
            window.try_find("dialog").is_some()
        })
        .await;
    }

    // --- 1,000 tables -------------------------------------------------------------------------

    struct Bulk;

    impl DbBackend for Bulk {
        fn dialect(&self) -> Dialect {
            Dialect::Sqlite
        }
        fn server_version(&self) -> BoxFuture<'static, Result<String, DbError>> {
            async { Ok("bulk".to_owned()) }.boxed()
        }
        fn list_children(
            &self,
            _: ObjectPath,
        ) -> BoxFuture<'static, Result<Vec<ObjectNode>, DbError>> {
            let nodes = (0..1000)
                .map(|i| ObjectNode {
                    kind: ObjectKind::Table,
                    name: format!("t{i:04}"),
                    children: None,
                    object: None,
                })
                .collect();
            async move { Ok(nodes) }.boxed()
        }
        fn table_columns(
            &self,
            _: ObjectRef,
        ) -> BoxFuture<'static, Result<Vec<ColumnInfo>, DbError>> {
            async { Ok(Vec::new()) }.boxed()
        }
        fn fetch_page(&self, _: PageRequest) -> BoxFuture<'static, Result<Page, DbError>> {
            async { Err(DbError::Unsupported("bulk".into())) }.boxed()
        }
        fn open_query_connection(
            &self,
            _: Option<String>,
        ) -> BoxFuture<'static, Result<Arc<dyn QueryConnection>, DbError>> {
            async { Err(DbError::Unsupported("bulk".into())) }.boxed()
        }
        fn close(&self) -> BoxFuture<'static, Result<(), DbError>> {
            async { Ok(()) }.boxed()
        }
    }

    struct BulkFactory;

    impl BackendFactory for BulkFactory {
        fn connect(
            &self,
            _: ConnectParams,
        ) -> BoxFuture<'static, Result<Arc<dyn DbBackend>, DbError>> {
            async { Ok(Arc::new(Bulk) as Arc<dyn DbBackend>) }.boxed()
        }
    }

    #[gpui_kit::test]
    fn a_thousand_tables_are_virtualized_and_scrollable(cx: &mut TestAppContext) {
        // Given: a SQLite connection exposing 1,000 tables, expanded.
        let id = Rc::new(std::cell::Cell::new(None));
        let sink = id.clone();
        let fx = open(
            cx,
            Arc::new(BulkFactory),
            false,
            Box::new(move |s, cx| {
                let p = ConnectionProfile::new(
                    "bulk",
                    Driver::Sqlite,
                    Endpoint::File {
                        path: "/tmp/bulk.db".into(),
                    },
                    PasswordStorage::None,
                );
                sink.set(Some(add(s, cx, p, DropPosition::RootEnd)));
            }),
        );
        let id = id.get().expect("id");
        toggle(cx, &fx, &SidebarNodeKey::Connection(id));
        assert_eq!(rows(cx, &fx).len(), 1001);
        let first = row_id(&table(id, "t0000", None, None));
        let last = row_id(&table(id, "t0999", None, None));
        // Then: only a window-sized slice of rows exists.
        ui(cx, &fx, |window, _| {
            assert!(window.try_find(first.clone()).is_some());
            assert!(window.try_find(last.clone()).is_none());
        });
        // When: scrolling to the bottom.
        ui(cx, &fx, |window, cx| {
            window.scroll(
                "sidebar-list",
                ScrollDelta::Pixels(point(px(0.), px(-40_000.))),
                cx,
            );
        });
        // Then: the last table is rendered and the first is not.
        ui(cx, &fx, |window, _| {
            assert!(window.try_find(last.clone()).is_some());
            assert!(window.try_find(first.clone()).is_none());
        });
    }
}
