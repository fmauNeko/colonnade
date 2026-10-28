//! Grouped tab strip in connection colors.
//!
//! Consecutive tabs of one connection form a group: a header in the connection color above the
//! group's tabs, like DbGate. Tabs reorder inside their group, groups reorder as a block.

// allow: SIZE_OK - the plan assigns the strip, its drag and drop, close paths and tests to this file.
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::keybindings;
use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, Bounds, Context, DragMoveEvent, Entity, IntoElement, MouseButton, Pixels,
    Render, ScrollHandle, SharedString, TestSupportExt as _, Window, canvas, div, hsla, point, px,
};

use super::tabs::{
    GlobalTabStore, Side, TabId, TabKind, TabRecord, TabStore, TabStoreEvent, WindowKey,
};
use crate::catalog::model::ConnectionId;
use crate::catalog::store::{ConnectionStore, GlobalConnectionStore};
use crate::color::{Rgb, generated_color, readable_foreground};
use crate::db::model::TableKind;
use crate::prompts::{self, ConfirmChoice, ConfirmRequest};

gpui_kit::actions!(workspace, [CloseActiveTab, NextTab, PreviousTab]);

/// Key context the workspace view sets so the tab shortcuts apply.
pub const KEY_CONTEXT: &str = "Workspace";

/// Binds the tab shortcuts in the [`KEY_CONTEXT`] key context.
pub fn init(cx: &mut App) {
    keybindings::bind(cx, "secondary-w", CloseActiveTab, Some(KEY_CONTEXT));
    keybindings::bind(cx, "ctrl-tab", NextTab, Some(KEY_CONTEXT));
    keybindings::bind(cx, "ctrl-shift-tab", PreviousTab, Some(KEY_CONTEXT));
}

/// One header and its tabs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupModel {
    pub connection: ConnectionId,
    pub label: SharedString,
    pub color: Rgb,
    pub foreground: Rgb,
    pub tabs: Vec<TabId>,
}

/// Splits the visible tabs into consecutive same-connection groups.
#[must_use]
pub fn group_models(visible: &[TabRecord], store: &ConnectionStore) -> Vec<GroupModel> {
    let mut groups: Vec<GroupModel> = Vec::new();
    for tab in visible {
        if let Some(group) = groups.last_mut().filter(|g| g.connection == tab.connection) {
            group.tabs.push(tab.id);
            continue;
        }
        let (label, color) = store.profile(tab.connection).map_or_else(
            || {
                let seed = tab.connection.as_uuid().to_string();
                (SharedString::from("?"), generated_color(&seed))
            },
            |profile| {
                (
                    SharedString::from(profile.name.clone()),
                    profile.color.effective(),
                )
            },
        );
        groups.push(GroupModel {
            connection: tab.connection,
            label,
            color,
            foreground: readable_foreground(color),
            tabs: vec![tab.id],
        });
    }
    groups
}

/// Drag payload of a tab.
#[derive(Debug, Clone)]
pub struct DraggedTab {
    pub tab: TabId,
    pub connection: ConnectionId,
    title: SharedString,
}

/// Drag payload of a group header.
#[derive(Debug, Clone)]
pub struct DraggedGroup {
    pub connection: ConnectionId,
    label: SharedString,
}

struct DragPreview(SharedString);

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px_2()
            .py_1()
            .bg(theme.tab_active)
            .text_color(theme.tab_active_foreground)
            .border_1()
            .border_color(theme.border)
            .child(self.0.clone())
    }
}

fn tab_icon(kind: &TabKind) -> IconName {
    match kind {
        TabKind::Table(object) => match object.kind {
            TableKind::Table => IconName::Table,
            TableKind::View => IconName::Eye,
        },
        TabKind::Query { .. } => IconName::SquareTerminal,
    }
}

fn tab_store(cx: &App) -> Entity<TabStore> {
    cx.global::<GlobalTabStore>().0.clone()
}

/// Closes tabs, asking for confirmation first when a close guard objects.
pub fn request_close(tabs: Vec<TabId>, window: &mut Window, cx: &mut App) {
    let store = tab_store(cx);
    if store.read(cx).close_reasons(&tabs, cx).is_empty() {
        store.update(cx, |s, cx| s.close(&tabs, cx));
        return;
    }
    let answer = prompts::confirm(
        window,
        cx,
        ConfirmRequest {
            title: "Close tabs".into(),
            message: "Closing discards unsaved or running work. Close anyway?".into(),
            confirm_label: "Close".into(),
            destructive: true,
            alternatives: Vec::new(),
        },
    );
    cx.spawn(async move |cx| {
        if let Ok(ConfirmChoice::Confirm) = answer.await {
            cx.update(|cx| store.update(cx, |s, cx| s.close(&tabs, cx)));
        }
    })
    .detach();
}

/// Closes the active tab of a window through [`request_close`].
pub fn close_active_tab(key: WindowKey, window: &mut Window, cx: &mut App) {
    if let Some(tab) = tab_store(cx).read(cx).active(key) {
        request_close(vec![tab], window, cx);
    }
}

fn activate_step(key: WindowKey, forward: bool, cx: &mut App) {
    let store = tab_store(cx);
    let (ids, active) = {
        let s = store.read(cx);
        let ids: Vec<TabId> = s.visible_tabs(key).iter().map(|t| t.id).collect();
        (ids, s.active(key))
    };
    let count = ids.len();
    if count == 0 {
        return;
    }
    let current = active.and_then(|a| ids.iter().position(|id| *id == a));
    let next = match (current, forward) {
        (Some(i), true) => (i + 1) % count,
        (Some(i), false) => (i + count - 1) % count,
        (None, true) => 0,
        (None, false) => count - 1,
    };
    store.update(cx, |s, cx| s.activate(ids[next], cx));
}

/// Activates the tab after the active one, wrapping around.
pub fn activate_next(key: WindowKey, cx: &mut App) {
    activate_step(key, true, cx);
}

/// Activates the tab before the active one, wrapping around.
pub fn activate_previous(key: WindowKey, cx: &mut App) {
    activate_step(key, false, cx);
}

/// The strip of grouped tabs of one window.
pub struct TabStrip {
    window_key: WindowKey,
    scroll: ScrollHandle,
    reveal: Option<TabId>,
    bounds: Rc<RefCell<HashMap<TabId, Bounds<Pixels>>>>,
    hover: Option<(TabId, Side)>,
}

impl TabStrip {
    /// A strip showing the tabs of `window_key`.
    pub fn new(window_key: WindowKey, cx: &mut Context<Self>) -> Self {
        let tabs = tab_store(cx);
        cx.observe(&tabs, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&tabs, |this, _, event: &TabStoreEvent, _| {
            if let TabStoreEvent::Activated {
                key,
                tab: Some(tab),
            } = event
                && *key == this.window_key
            {
                this.reveal = Some(*tab);
            }
        })
        .detach();
        let connections = cx.global::<GlobalConnectionStore>().0.clone();
        cx.observe(&connections, |_, _, cx| cx.notify()).detach();
        Self {
            window_key,
            scroll: ScrollHandle::new(),
            reveal: tab_store(cx).read(cx).active(window_key),
            bounds: Rc::default(),
            hover: None,
        }
    }

    /// Scrolls the pending tab horizontally into view once its bounds are known.
    fn apply_reveal(&mut self, visible: &[TabRecord], window: &mut Window, cx: &mut Context<Self>) {
        self.bounds
            .borrow_mut()
            .retain(|id, _| visible.iter().any(|t| t.id == *id));
        let Some(tab) = self.reveal else { return };
        if !visible.iter().any(|t| t.id == tab) {
            self.reveal = None;
            return;
        }
        let container = self.scroll.bounds();
        let found = self.bounds.borrow().get(&tab).copied();
        let Some(target) = found.filter(|_| container.size.width > px(0.)) else {
            let this = cx.entity();
            window.on_next_frame(move |_, cx| this.update(cx, |_, cx| cx.notify()));
            return;
        };
        self.reveal = None;
        let (left, right) = (
            target.left() - container.left(),
            target.right() - container.right(),
        );
        let shift = if left < px(0.) {
            -left
        } else if right > px(0.) {
            -right
        } else {
            return;
        };
        let offset = self.scroll.offset();
        let max = self.scroll.max_offset().x.abs();
        let x = (offset.x + shift).clamp(-max, px(0.));
        self.scroll.set_offset(point(x, offset.y));
    }

    fn close_button(id: String, tabs: Vec<TabId>) -> impl IntoElement {
        div()
            .id(SharedString::from(id))
            .test_support()
            .cursor_pointer()
            .flex_none()
            .child(Icon::new(IconName::Close).xsmall())
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                request_close(tabs.clone(), window, cx);
            })
    }

    fn tab_element(
        &self,
        record: &TabRecord,
        group: &GroupModel,
        active: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (id, connection) = (record.id, record.connection);
        let theme = cx.theme();
        let (bg, fg) = if active {
            (theme.tab_active, theme.tab_active_foreground)
        } else {
            (theme.tab, theme.tab_foreground)
        };
        let accent = theme.primary;
        let underline = if active {
            group.color.to_hsla()
        } else {
            hsla(0., 0., 0., 0.)
        };
        let drop_side = self
            .hover
            .filter(|(tab, _)| *tab == id)
            .filter(|_| cx.has_active_drag())
            .map(|(_, side)| side);
        let bounds = self.bounds.clone();
        let store = tab_store(cx);
        let dragged = DraggedTab {
            tab: id,
            connection,
            title: record.title.clone(),
        };
        div()
            .id(SharedString::from(format!("tab:{}", id.0)))
            .test_support()
            .relative()
            .flex()
            .items_center()
            .gap_1()
            .h(px(30.))
            .min_w(px(120.))
            .px_2()
            .bg(bg)
            .text_color(fg)
            .border_b(px(2.))
            .border_color(underline)
            .child(
                canvas(
                    move |area, _, _| {
                        bounds.borrow_mut().insert(id, area);
                    },
                    |_, (), _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(Icon::new(tab_icon(&record.kind)).small())
            .child(div().flex_1().truncate().child(record.title.clone()))
            .child(Self::close_button(format!("tab-close:{}", id.0), vec![id]))
            .when_some(drop_side, |tab, side| {
                let marker = div().absolute().top_0().bottom_0().w(px(2.)).bg(accent);
                tab.child(match side {
                    Side::Before => marker.left_0(),
                    Side::After => marker.right_0(),
                })
            })
            .on_click(move |_, _, cx| store.update(cx, |s, cx| s.activate(id, cx)))
            .on_mouse_up(MouseButton::Middle, move |_, window, cx| {
                request_close(vec![id], window, cx);
            })
            .on_drag(dragged, |dragged, _, _, cx| {
                cx.new(|_| DragPreview(dragged.title.clone()))
            })
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<DraggedTab>, _, cx| {
                    let over = event.bounds.contains(&event.event.position)
                        && event.drag(cx).connection == connection;
                    let next = over.then(|| {
                        let side = if event.event.position.x < event.bounds.center().x {
                            Side::Before
                        } else {
                            Side::After
                        };
                        (id, side)
                    });
                    if over && this.hover != next {
                        this.hover = next;
                        cx.notify();
                    } else if !over && this.hover.is_some_and(|(tab, _)| tab == id) {
                        this.hover = None;
                        cx.notify();
                    }
                }),
            )
            .on_drop(cx.listener(move |this, dragged: &DraggedTab, _, cx| {
                let side = this
                    .hover
                    .take()
                    .filter(|(tab, _)| *tab == id)
                    .map_or(Side::Before, |(_, side)| side);
                if dragged.connection == connection {
                    let moved = dragged.tab;
                    tab_store(cx).update(cx, |s, cx| s.move_tab(moved, id, side, cx).ok());
                }
                cx.notify();
            }))
            .into_any_element()
    }

    fn header_element(group: &GroupModel, cx: &mut Context<Self>) -> AnyElement {
        let connection = group.connection;
        let uuid = connection.as_uuid();
        let dragged = DraggedGroup {
            connection,
            label: group.label.clone(),
        };
        div()
            .id(SharedString::from(format!("tab-group:{uuid}")))
            .test_support()
            .flex()
            .items_center()
            .gap_1()
            .h(px(22.))
            .px_2()
            .bg(group.color.to_hsla())
            .text_color(group.foreground.to_hsla())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(group.label.clone()),
            )
            .child(Self::close_button(
                format!("tab-group-close:{uuid}"),
                group.tabs.clone(),
            ))
            .on_drag(dragged, |dragged, _, _, cx| {
                cx.new(|_| DragPreview(dragged.label.clone()))
            })
            .on_drop(cx.listener(move |_, dragged: &DraggedGroup, _, cx| {
                let moved = dragged.connection;
                tab_store(cx).update(cx, |s, cx| {
                    let first = |c: ConnectionId| s.tabs().iter().position(|t| t.connection == c);
                    let side = if first(moved) < first(connection) {
                        Side::After
                    } else {
                        Side::Before
                    };
                    s.move_group(moved, connection, side, cx).ok()
                });
            }))
            .into_any_element()
    }
}

impl Render for TabStrip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = tab_store(cx);
        let (visible, active) = {
            let s = store.read(cx);
            let visible: Vec<TabRecord> = s
                .visible_tabs(self.window_key)
                .into_iter()
                .cloned()
                .collect();
            (visible, s.active(self.window_key))
        };
        let groups = {
            let connections = cx.global::<GlobalConnectionStore>().0.read(cx);
            group_models(&visible, connections)
        };
        self.apply_reveal(&visible, window, cx);
        let mut columns = Vec::with_capacity(groups.len());
        for group in &groups {
            let mut tabs = Vec::with_capacity(group.tabs.len());
            for record in group
                .tabs
                .iter()
                .filter_map(|id| visible.iter().find(|t| t.id == *id))
            {
                tabs.push(self.tab_element(record, group, active == Some(record.id), cx));
            }
            columns.push(
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .mr_1()
                    .child(Self::header_element(group, cx))
                    .child(h_flex().children(tabs)),
            );
        }
        h_flex()
            .id("tab-strip")
            .test_support()
            .w_full()
            .items_start()
            .overflow_x_scroll()
            .track_scroll(&self.scroll)
            .bg(cx.theme().tab_bar)
            .children(columns)
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyWindowHandle, App, AppContext as _, Bounds, Entity, InputEvent as _, Modifiers,
        MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Point, TestAppContext, Window,
        WindowBounds, WindowOptions, point, px, size,
    };

    use super::{
        GroupModel, TabStrip, activate_next, activate_previous, close_active_tab, group_models,
    };
    use crate::catalog::model::{
        ConnectionId, ConnectionProfile, Driver, Endpoint, HexColor, PasswordStorage,
    };
    use crate::catalog::ops::DropPosition;
    use crate::catalog::store::{ConnectionStore, GlobalConnectionStore};
    use crate::color::{Rgb, contrast_ratio};
    use crate::db::model::{ObjectRef, TableKind};
    use crate::paths::ConfigPaths;
    use crate::prompts::{self, ConfirmChoice, ScriptedPrompts};
    use crate::settings::WindowMode;
    use crate::workspace::tabs::{CloseReason, GlobalTabStore, TabId, TabStore, WindowKey};
    use crate::workspace::view_factory::TabKindTag;

    struct Fx {
        _dir: tempfile::TempDir,
        window: AnyWindowHandle,
        tabs: Entity<TabStore>,
        conns: Entity<ConnectionStore>,
        ids: [ConnectionId; 3],
        prompts: Rc<ScriptedPrompts>,
    }

    fn object(name: &str) -> ObjectRef {
        ObjectRef {
            database: None,
            schema: None,
            name: name.to_owned(),
            kind: TableKind::Table,
        }
    }

    fn profile(name: &str) -> ConnectionProfile {
        ConnectionProfile::new(
            name,
            Driver::Sqlite,
            Endpoint::File {
                path: format!("/tmp/{name}.db").into(),
            },
            PasswordStorage::None,
        )
    }

    /// Opens tabs a1(1) b1(2) c1(3) a2(4) b2(5): the order becomes a1 a2 b1 b2 c1.
    fn setup(cx: &mut TestAppContext) -> Fx {
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = ConfigPaths::at(dir.path().to_path_buf());
        let prompts = cx.update(|cx| {
            gpui_kit::init(cx);
            crate::workspace::tabs::init(cx, WindowMode::Single);
            crate::catalog::store::init(cx, &paths);
            prompts::install_scripted(cx)
        });
        let tabs = cx.update(|cx| cx.global::<GlobalTabStore>().0.clone());
        let conns = cx.update(|cx| cx.global::<GlobalConnectionStore>().0.clone());
        let profiles = [profile("alpha"), profile("beta"), profile("gamma")];
        let ids = [profiles[0].id, profiles[1].id, profiles[2].id];
        for p in profiles {
            cx.update(|cx| {
                conns.update(cx, |s, cx| {
                    s.add_connection(cx, p, DropPosition::RootEnd)
                        .expect("added");
                });
            });
        }
        cx.update(|cx| {
            tabs.update(cx, |s, cx| {
                s.open_table(ids[0], object("a1"), cx);
                s.open_table(ids[1], object("b1"), cx);
                s.open_query(ids[2], None, cx);
                s.open_query(ids[0], None, cx);
                s.open_table(ids[1], object("b2"), cx);
            });
        });
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                Point::default(),
                size(px(800.), px(400.)),
            ))),
            ..Default::default()
        };
        let (window, _) = cx
            .update(|cx| {
                gpui_kit::open_window(options, cx, |_, cx| {
                    cx.new(|cx| TabStrip::new(WindowKey::Main, cx))
                })
            })
            .expect("window opens");
        Fx {
            _dir: dir,
            window,
            tabs,
            conns,
            ids,
            prompts,
        }
    }

    fn ui<R>(cx: &mut TestAppContext, fx: &Fx, f: impl FnOnce(&mut Window, &mut App) -> R) -> R {
        cx.update_window(fx.window, |_, window, cx| {
            frames(window, cx);
            let out = f(window, cx);
            frames(window, cx);
            out
        })
        .expect("window open")
    }

    fn frames(window: &mut Window, cx: &mut App) {
        for _ in 0..3 {
            window.refresh();
            window.render_frame(cx);
        }
    }

    fn order(cx: &mut TestAppContext, fx: &Fx) -> Vec<u64> {
        cx.update(|cx| {
            fx.tabs
                .read(cx)
                .visible_tabs(WindowKey::Main)
                .iter()
                .map(|t| t.id.0)
                .collect()
        })
    }

    fn active(cx: &mut TestAppContext, fx: &Fx) -> Option<u64> {
        cx.update(|cx| fx.tabs.read(cx).active(WindowKey::Main).map(|t| t.0))
    }

    fn models(cx: &mut TestAppContext, fx: &Fx) -> Vec<GroupModel> {
        cx.update(|cx| {
            let visible: Vec<_> = fx
                .tabs
                .read(cx)
                .visible_tabs(WindowKey::Main)
                .into_iter()
                .cloned()
                .collect();
            group_models(&visible, fx.conns.read(cx))
        })
    }

    fn tab(id: u64) -> String {
        format!("tab:{id}")
    }

    fn header(connection: ConnectionId) -> String {
        format!("tab-group:{}", connection.as_uuid())
    }

    fn group_close(connection: ConnectionId) -> String {
        format!("tab-group-close:{}", connection.as_uuid())
    }

    #[gpui_kit::test]
    fn groups_follow_dbgate_order(cx: &mut TestAppContext) {
        // Given: tabs opened in interleaved order across three connections.
        let fx = setup(cx);
        // When: building the group models.
        let groups = models(cx, &fx);
        // Then: three groups in first-open order, each holding its own tabs.
        let got: Vec<_> = groups
            .iter()
            .map(|g| (g.connection, g.tabs.iter().map(|t| t.0).collect::<Vec<_>>()))
            .collect();
        assert_eq!(
            got,
            [
                (fx.ids[0], vec![1, 4]),
                (fx.ids[1], vec![2, 5]),
                (fx.ids[2], vec![3])
            ]
        );
        // And: every header is rendered.
        ui(cx, &fx, |window, _| {
            for id in fx.ids {
                assert!(window.find(header(id)).visible());
            }
        });
    }

    #[gpui_kit::test]
    fn group_colors_are_the_effective_connection_colors(cx: &mut TestAppContext) {
        // Given: a custom color on the second connection.
        let fx = setup(cx);
        let custom = Rgb {
            r: 10,
            g: 200,
            b: 30,
        };
        cx.update(|cx| {
            fx.conns.update(cx, |s, cx| {
                s.set_custom_color(cx, fx.ids[1], Some(HexColor::new(custom)))
                    .expect("set");
            });
        });
        // When: building the group models.
        let groups = models(cx, &fx);
        // Then: colors equal the effective colors.
        let expected: Vec<Rgb> = cx.update(|cx| {
            fx.ids
                .iter()
                .map(|id| {
                    fx.conns
                        .read(cx)
                        .profile(*id)
                        .expect("profile")
                        .color
                        .effective()
                })
                .collect()
        });
        assert_eq!(groups.iter().map(|g| g.color).collect::<Vec<_>>(), expected);
        assert_eq!(groups[1].color, custom);
    }

    #[gpui_kit::test]
    fn group_foregrounds_are_readable(cx: &mut TestAppContext) {
        // Given: three generated connection colors.
        let fx = setup(cx);
        // When: reading the models.
        let groups = models(cx, &fx);
        // Then: each foreground reaches AA contrast on its background.
        for group in groups {
            assert!(contrast_ratio(group.color, group.foreground) >= 4.5);
        }
    }

    #[gpui_kit::test]
    fn changing_a_custom_color_updates_the_header_model(cx: &mut TestAppContext) {
        // Given: the strip and its models.
        let fx = setup(cx);
        let before = models(cx, &fx)[0].color;
        let custom = Rgb { r: 1, g: 2, b: 3 };
        // When: the first connection gets a custom color.
        cx.update(|cx| {
            fx.conns.update(cx, |s, cx| {
                s.set_custom_color(cx, fx.ids[0], Some(HexColor::new(custom)))
                    .expect("set");
            });
        });
        // Then: its header model follows.
        let after = models(cx, &fx)[0].clone();
        assert_ne!(before, custom);
        assert_eq!(after.color, custom);
        assert!(contrast_ratio(after.color, after.foreground) >= 4.5);
    }

    #[gpui_kit::test]
    fn clicking_a_tab_activates_it(cx: &mut TestAppContext) {
        // Given: tab 5 is active.
        let fx = setup(cx);
        assert_eq!(active(cx, &fx), Some(5));
        // When: clicking tab 1.
        ui(cx, &fx, |window, cx| window.click(tab(1), cx));
        // Then: it becomes active.
        assert_eq!(active(cx, &fx), Some(1));
    }

    #[gpui_kit::test]
    fn close_button_closes_a_guard_free_tab(cx: &mut TestAppContext) {
        // Given: five open tabs.
        let fx = setup(cx);
        // When: clicking the close button of tab 3.
        ui(cx, &fx, |window, cx| window.click("tab-close:3", cx));
        // Then: the tab is gone, without any confirmation.
        assert_eq!(order(cx, &fx), [1, 4, 2, 5]);
        assert_eq!(fx.prompts.confirm_requests().len(), 0);
    }

    fn guard_queries(cx: &mut TestAppContext, fx: &Fx) {
        cx.update(|cx| {
            fx.tabs.update(cx, |s, _| {
                s.register_guard(TabKindTag::Query, |_, _| {
                    vec![CloseReason::UnsavedQueryText]
                });
            });
        });
    }

    #[gpui_kit::test]
    fn guarded_close_can_be_cancelled(cx: &mut TestAppContext) {
        // Given: query tabs guarded by unsaved text and a scripted Cancel.
        let fx = setup(cx);
        guard_queries(cx, &fx);
        fx.prompts.queue_confirm(ConfirmChoice::Cancel);
        // When: closing query tab 3.
        ui(cx, &fx, |window, cx| window.click("tab-close:3", cx));
        cx.run_until_parked();
        // Then: it asked once and the tab stays.
        assert_eq!(fx.prompts.confirm_requests().len(), 1);
        assert!(fx.prompts.confirm_requests()[0].destructive);
        assert_eq!(order(cx, &fx), [1, 4, 2, 5, 3]);
    }

    #[gpui_kit::test]
    fn guarded_close_proceeds_on_confirm(cx: &mut TestAppContext) {
        // Given: the same guard and a scripted Confirm.
        let fx = setup(cx);
        guard_queries(cx, &fx);
        fx.prompts.queue_confirm(ConfirmChoice::Confirm);
        // When: closing query tab 3.
        ui(cx, &fx, |window, cx| window.click("tab-close:3", cx));
        cx.run_until_parked();
        // Then: the tab is closed.
        assert_eq!(order(cx, &fx), [1, 4, 2, 5]);
    }

    #[gpui_kit::test]
    fn middle_click_closes_the_tab(cx: &mut TestAppContext) {
        // Given: five open tabs.
        let fx = setup(cx);
        // When: middle-clicking tab 2.
        ui(cx, &fx, |window, cx| {
            let position = window.find(tab(2)).bounds().center();
            window.dispatch_event(
                MouseMoveEvent {
                    position,
                    pressed_button: None,
                    modifiers: Modifiers::default(),
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(
                MouseDownEvent {
                    button: MouseButton::Middle,
                    position,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                    first_mouse: false,
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(
                MouseUpEvent {
                    button: MouseButton::Middle,
                    position,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                }
                .to_platform_input(),
                cx,
            );
        });
        // Then: it is closed.
        assert_eq!(order(cx, &fx), [1, 4, 5, 3]);
    }

    fn drag_between(cx: &mut TestAppContext, fx: &Fx, from: &str, to: &str, x_fraction: f32) {
        ui(cx, fx, |window, cx| {
            let from = window.find(from.to_owned()).bounds().center();
            let target = window.find(to.to_owned()).bounds();
            let to = point(
                target.origin.x + target.size.width * x_fraction,
                target.center().y,
            );
            window.drag(from, to, cx);
        });
    }

    #[gpui_kit::test]
    fn dropping_on_the_left_half_moves_before_the_sibling(cx: &mut TestAppContext) {
        // Given: group A holds tabs 1 and 4; open a third A tab.
        let fx = setup(cx);
        cx.update(|cx| {
            fx.tabs.update(cx, |s, cx| {
                s.open_query(fx.ids[0], None, cx);
            });
        });
        assert_eq!(order(cx, &fx), [1, 4, 6, 2, 5, 3]);
        // When: dragging tab 6 onto the left half of tab 1.
        drag_between(cx, &fx, "tab:6", "tab:1", 0.25);
        // Then: it lands before tab 1.
        assert_eq!(order(cx, &fx), [6, 1, 4, 2, 5, 3]);
    }

    #[gpui_kit::test]
    fn dropping_on_the_right_half_moves_after_the_sibling(cx: &mut TestAppContext) {
        // Given: group A holds tabs 1, 4 and 6.
        let fx = setup(cx);
        cx.update(|cx| {
            fx.tabs.update(cx, |s, cx| {
                s.open_query(fx.ids[0], None, cx);
            });
        });
        // When: dragging tab 6 onto the right half of tab 1.
        drag_between(cx, &fx, "tab:6", "tab:1", 0.75);
        // Then: it lands right after tab 1.
        assert_eq!(order(cx, &fx), [1, 6, 4, 2, 5, 3]);
    }

    #[gpui_kit::test]
    fn cross_group_drop_is_ignored(cx: &mut TestAppContext) {
        // Given: tabs of three groups.
        let fx = setup(cx);
        // When: dragging tab 1 (group A) onto tab 2 (group B).
        drag_between(cx, &fx, "tab:1", "tab:2", 0.75);
        // Then: nothing moves.
        assert_eq!(order(cx, &fx), [1, 4, 2, 5, 3]);
    }

    #[gpui_kit::test]
    fn dragging_a_header_moves_its_group_before_the_target(cx: &mut TestAppContext) {
        // Given: groups A, B, C.
        let fx = setup(cx);
        // When: dragging header B onto header A.
        let (b, a) = (header(fx.ids[1]), header(fx.ids[0]));
        drag_between(cx, &fx, &b, &a, 0.5);
        // Then: group B comes first.
        assert_eq!(order(cx, &fx), [2, 5, 1, 4, 3]);
    }

    #[gpui_kit::test]
    fn group_close_button_closes_the_group(cx: &mut TestAppContext) {
        // Given: groups A, B, C.
        let fx = setup(cx);
        // When: clicking the close button of group B.
        let button = group_close(fx.ids[1]);
        ui(cx, &fx, |window, cx| window.click(button, cx));
        // Then: only B's tabs are closed.
        assert_eq!(order(cx, &fx), [1, 4, 3]);
    }

    #[gpui_kit::test]
    fn activating_the_last_of_many_tabs_scrolls_it_into_view(cx: &mut TestAppContext) {
        // Given: 30 tabs of one connection with the first one active.
        let fx = setup(cx);
        cx.update(|cx| {
            fx.tabs.update(cx, |s, cx| {
                for _ in 0..30 {
                    s.open_query(fx.ids[2], None, cx);
                }
            });
        });
        let all = order(cx, &fx);
        let (first, last) = (all[0], *all.last().expect("tabs"));
        cx.update(|cx| fx.tabs.update(cx, |s, cx| s.activate(TabId(first), cx)));
        ui(cx, &fx, |window, _| {
            assert!(!window.find(tab(last)).visible());
        });
        // When: activating the last tab.
        cx.update(|cx| fx.tabs.update(cx, |s, cx| s.activate(TabId(last), cx)));
        // Then: it scrolls into view.
        ui(cx, &fx, |window, _| {
            assert!(window.find(tab(last)).visible());
        });
    }

    #[gpui_kit::test]
    fn next_and_previous_wrap_around(cx: &mut TestAppContext) {
        // Given: order 1 4 2 5 3 with tab 3 (last) active.
        let fx = setup(cx);
        cx.update(|cx| fx.tabs.update(cx, |s, cx| s.activate(TabId(3), cx)));
        // When: going next.
        cx.update(|cx| activate_next(WindowKey::Main, cx));
        // Then: it wraps to the first tab.
        assert_eq!(active(cx, &fx), Some(1));
        // When: going previous.
        cx.update(|cx| activate_previous(WindowKey::Main, cx));
        // Then: it wraps back to the last tab.
        assert_eq!(active(cx, &fx), Some(3));
    }

    #[gpui_kit::test]
    fn close_active_tab_goes_through_the_guard(cx: &mut TestAppContext) {
        // Given: a guarded active query tab and a scripted Cancel.
        let fx = setup(cx);
        guard_queries(cx, &fx);
        cx.update(|cx| fx.tabs.update(cx, |s, cx| s.activate(TabId(3), cx)));
        fx.prompts.queue_confirm(ConfirmChoice::Cancel);
        // When: closing the active tab.
        ui(cx, &fx, |window, cx| {
            close_active_tab(WindowKey::Main, window, cx);
        });
        cx.run_until_parked();
        // Then: it was asked about and kept.
        assert_eq!(fx.prompts.confirm_requests().len(), 1);
        assert_eq!(order(cx, &fx).len(), 5);
    }
}
