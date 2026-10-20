//! Table data tab view: a toolbar, an error panel and a virtualized, server-sorted grid.

// allow: SIZE_OK - the plan keeps the view, its grid delegate and its tests in this file.
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::table::{ColumnSort, DataTable, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, Context, Empty, Entity, IntoElement, Render, SharedString, Subscription,
    TestSupportExt as _, Window, div,
};

use super::state::{TableTabEvent, TableTabState, TableTabs};
use crate::db::model::{ObjectRef, SortDirection};
use crate::grid::{self, column_for};
use crate::workspace::tabs::{GlobalTabStore, TabStoreEvent};
use crate::workspace::view_factory::{self, TabKindTag};

/// Registers the table tab view and keeps one [`TableTabState`] per open table tab.
///
/// Needs the tab store and the view factory to be installed.
pub fn init(cx: &mut App) {
    TableTabs::init(cx);
    view_factory::register(
        cx,
        TabKindTag::Table,
        |tab, window, cx| match TableTabs::ensure(tab, cx) {
            Some(state) => cx.new(|cx| TableTabView::new(state, window, cx)).into(),
            None => cx.new(|_| EmptyTab).into(),
        },
    );
    let store = cx.global::<GlobalTabStore>().0.clone();
    cx.subscribe(&store, |_, event: &TabStoreEvent, cx| match event {
        TabStoreEvent::Opened { tab, .. } => {
            TableTabs::ensure(*tab, cx);
        }
        TabStoreEvent::Closed(tab) => TableTabs::remove(*tab, cx),
        TabStoreEvent::Activated { .. }
        | TabStoreEvent::Reordered
        | TabStoreEvent::ModeChanged(_) => {}
    })
    .detach();
}

struct EmptyTab;

impl Render for EmptyTab {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// Kit table delegate over a [`TableTabState`].
pub struct GridDelegate {
    state: Entity<TableTabState>,
}

impl TableDelegate for GridDelegate {
    fn columns_count(&self, cx: &App) -> usize {
        self.state.read(cx).columns.len()
    }

    fn rows_count(&self, cx: &App) -> usize {
        self.state.read(cx).rows.len()
    }

    fn column(&self, col_ix: usize, cx: &App) -> gpui_kit::component::table::Column {
        self.state
            .read(cx)
            .columns
            .get(col_ix)
            .map_or_else(Default::default, |info| column_for(info, col_ix, true))
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let sort = match sort {
            ColumnSort::Ascending => Some((col_ix, SortDirection::Ascending)),
            ColumnSort::Descending => Some((col_ix, SortDirection::Descending)),
            ColumnSort::Default => None,
        };
        self.state
            .update(cx, |state, cx| state.set_sort(sort, window, cx));
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let state = self.state.read(cx);
        let cell = state.rows.get(row_ix).and_then(|row| row.get(col_ix));
        let numeric = state.columns.get(col_ix).is_some_and(|info| info.numeric);
        match cell {
            Some(cell) => grid::render_cell(&grid::cell_display(cell, numeric), cx),
            None => Empty.into_any_element(),
        }
    }

    fn has_more(&self, cx: &App) -> bool {
        let state = self.state.read(cx);
        state.has_more && state.error.is_none()
    }

    fn load_more(&mut self, window: &mut Window, cx: &mut Context<TableState<Self>>) {
        self.state
            .update(cx, |state, cx| state.load_more(window, cx));
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, cx: &App) -> String {
        self.state
            .read(cx)
            .rows
            .get(row_ix)
            .and_then(|row| row.get(col_ix))
            .map(grid::copy_text)
            .unwrap_or_default()
    }
}

/// A table tab: toolbar, optional error panel and the paged grid.
pub struct TableTabView {
    state: Entity<TableTabState>,
    table: Entity<TableState<GridDelegate>>,
    _subscription: Subscription,
}

impl TableTabView {
    #[must_use]
    pub fn new(state: Entity<TableTabState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let table = cx.new(|cx| {
            TableState::new(
                GridDelegate {
                    state: state.clone(),
                },
                window,
                cx,
            )
        });
        let subscription = cx.subscribe(&state, |this, _, event: &TableTabEvent, cx| {
            this.table.update(cx, |table, cx| match event {
                TableTabEvent::ColumnsChanged => table.refresh(cx),
                TableTabEvent::RowsReset => table.scroll_to_row(0, cx),
            });
        });
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        Self {
            state,
            table,
            _subscription: subscription,
        }
    }

    fn qualified_name(object: &ObjectRef) -> SharedString {
        [&object.database, &object.schema]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .chain([object.name.as_str()])
            .collect::<Vec<_>>()
            .join(".")
            .into()
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = self.state.read(cx);
        let count = state.rows.len();
        let count = if state.has_more {
            format!("{count} rows loaded, more available")
        } else {
            format!("{count} rows, all loaded")
        };
        h_flex()
            .gap_2()
            .px_3()
            .py_1()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(Self::qualified_name(&state.object))
            .child(
                Button::new("table-refresh")
                    .icon(IconName::RefreshCw)
                    .ghost()
                    .xsmall()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.state.update(cx, |state, cx| state.refresh(window, cx));
                    })),
            )
            .child(
                div()
                    .id("table-row-count")
                    .test_support()
                    .text_color(cx.theme().muted_foreground)
                    .child(count),
            )
            .when(state.loading, |bar| bar.child(Spinner::new().xsmall()))
            .into_any_element()
    }

    fn error_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let message = self.state.read(cx).error.as_ref()?.to_string();
        Some(
            h_flex()
                .id("table-error")
                .test_support()
                .gap_2()
                .px_3()
                .py_2()
                .items_center()
                .text_color(cx.theme().danger)
                .child(Icon::new(IconName::CircleAlert).xsmall())
                .child(div().flex_1().child(message))
                .child(
                    Button::new("table-retry")
                        .label("Retry")
                        .xsmall()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.state.update(cx, |state, cx| state.retry(window, cx));
                        })),
                )
                .into_any_element(),
        )
    }
}

impl Render for TableTabView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.state.read(cx).started {
            self.state
                .update(cx, |state, cx| state.load_first(window, cx));
        }
        v_flex()
            .size_full()
            .child(self.toolbar(cx))
            .children(self.error_panel(cx))
            .child(
                div()
                    .id("table-grid")
                    .test_support()
                    .flex_1()
                    .min_h_0()
                    .child(DataTable::new(&self.table)),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyView, AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, IntoElement,
        ParentElement as _, Point, Render, ScrollDelta, Styled as _, TestAppContext, Window,
        WindowBounds, WindowOptions, div, point, px, size,
    };

    use super::init;
    use crate::db::model::{DbError, SortDirection};
    use crate::grid::{CellTone, cell_display};
    use crate::table_tab::state::harness::{Fx, setup};
    use crate::table_tab::state::{TableTabState, TableTabs};
    use crate::workspace::tabs::{TabId, WindowKey};
    use crate::workspace::view_factory;

    struct Host {
        view: AnyView,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.view.clone())
        }
    }

    struct Tab {
        fx: Fx,
        id: TabId,
        window: AnyWindowHandle,
        state: Entity<TableTabState>,
    }

    fn open(cx: &mut TestAppContext) -> Tab {
        open_with(cx, |_| {})
    }

    /// Opens a tab on `users` in a window that renders it through the view factory,
    /// after `script` prepared the fake server.
    fn open_with(cx: &mut TestAppContext, script: impl FnOnce(&Fx)) -> Tab {
        let fx = setup(cx);
        script(&fx);
        cx.update(init);
        let id = fx.open_table("users", cx);
        let bounds = Bounds::new(Point::default(), size(px(800.), px(400.)));
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        };
        let (window, _) = cx
            .update(|cx| {
                gpui_kit::open_window(options, cx, |window, cx| {
                    let view = view_factory::build(id, window, cx).expect("a table view");
                    cx.new(|_| Host { view })
                })
            })
            .expect("window opens");
        let state = cx.update(|cx| TableTabs::ensure(id, cx)).expect("state");
        let tab = Tab {
            fx,
            id,
            window,
            state,
        };
        settle(cx, &tab);
        tab
    }

    fn ui<R>(cx: &mut TestAppContext, tab: &Tab, f: impl FnOnce(&mut Window, &mut App) -> R) -> R {
        cx.update_window(tab.window, |_, window, cx| {
            window.render_frame(cx);
            let out = f(window, cx);
            window.render_frame(cx);
            out
        })
        .expect("window")
    }

    /// Renders and lets every pending load finish.
    fn settle(cx: &mut TestAppContext, tab: &Tab) {
        for _ in 0..3 {
            ui(cx, tab, |_, _| {});
            cx.run_until_parked();
        }
        ui(cx, tab, |_, _| {});
    }

    fn rows(cx: &mut TestAppContext, tab: &Tab) -> usize {
        cx.update(|cx| tab.state.read(cx).rows.len())
    }

    fn scroll_to_bottom(cx: &mut TestAppContext, tab: &Tab) {
        ui(cx, tab, |window, cx| {
            window.scroll(
                "table-grid",
                ScrollDelta::Pixels(point(px(0.), px(-400_000.))),
                cx,
            );
        });
        settle(cx, tab);
    }

    #[gpui_kit::test]
    fn opening_a_table_shows_its_first_page(cx: &mut TestAppContext) {
        // Given: a table with 450 rows.
        // When: opening its tab.
        let tab = open(cx);
        // Then: 200 rows are loaded with more to come, from one request at offset 0.
        assert_eq!(rows(cx, &tab), 200);
        assert!(cx.update(|cx| tab.state.read(cx).has_more));
        let pages = tab.fx.server.pages();
        assert_eq!(pages.len(), 1);
        assert_eq!((pages[0].offset, pages[0].limit), (0, 200));
        // And: the grid shows rows and the toolbar its row count.
        ui(cx, &tab, |window, _| {
            assert!(window.find(("row", 0usize)).visible());
            assert!(window.find("table-row-count").visible());
            assert!(window.try_find("table-error").is_none());
        });
    }

    #[gpui_kit::test]
    fn scrolling_to_the_bottom_loads_the_remaining_pages(cx: &mut TestAppContext) {
        // Given: a table with 450 rows and its first 200 loaded.
        let tab = open(cx);
        // When: scrolling to the bottom twice.
        scroll_to_bottom(cx, &tab);
        let after_first = rows(cx, &tab);
        scroll_to_bottom(cx, &tab);
        // Then: 400 rows load, then 450, and nothing is left to load.
        assert_eq!(after_first, 400);
        assert_eq!(rows(cx, &tab), 450);
        assert!(!cx.update(|cx| tab.state.read(cx).has_more));
        let offsets: Vec<u64> = tab.fx.server.pages().iter().map(|r| r.offset).collect();
        assert_eq!(offsets, [0, 200, 400]);
    }

    #[gpui_kit::test]
    fn sorting_a_column_reloads_from_the_server(cx: &mut TestAppContext) {
        // Given: a loaded table.
        let tab = open(cx);
        // When: clicking the sort icon of the name column.
        ui(cx, &tab, |window, cx| {
            window.click(("icon-sort", 1usize), cx);
        });
        settle(cx, &tab);
        // Then: the server got an offset-0 request ordered by name, descending.
        let request = tab.fx.server.pages().last().cloned().expect("a request");
        assert_eq!(request.offset, 0);
        assert_eq!(
            request.order_by,
            Some(("name".to_owned(), SortDirection::Descending))
        );
        // And: the rows were replaced by the sorted first page.
        assert_eq!(rows(cx, &tab), 200);
        let first_id = cx.update(|cx| tab.state.read(cx).rows[0][0].clone());
        assert_eq!(first_id, crate::db::model::Cell::text("450".into()));
    }

    #[gpui_kit::test]
    fn page_failure_can_be_retried(cx: &mut TestAppContext) {
        // Given: a server whose next page fails.
        let tab = open_with(cx, |fx| {
            fx.server.fail_page(DbError::Query {
                message: "boom".into(),
                code: None,
            });
        });
        // Then: the error panel and its retry button show.
        ui(cx, &tab, |window, _| {
            assert!(window.find("table-error").visible());
            assert!(window.find("table-retry").visible());
        });
        assert_eq!(rows(cx, &tab), 0);
        // When: retrying.
        ui(cx, &tab, |window, cx| window.click("table-retry", cx));
        settle(cx, &tab);
        // Then: the first page loads and the error goes away.
        assert_eq!(rows(cx, &tab), 200);
        ui(cx, &tab, |window, _| {
            assert!(window.try_find("table-error").is_none());
        });
    }

    #[gpui_kit::test]
    fn refreshing_reloads_from_the_first_page(cx: &mut TestAppContext) {
        // Given: a table with 400 of its 450 rows loaded.
        let tab = open(cx);
        scroll_to_bottom(cx, &tab);
        assert_eq!(rows(cx, &tab), 400);
        // When: clicking refresh.
        ui(cx, &tab, |window, cx| window.click("table-refresh", cx));
        settle(cx, &tab);
        // Then: only the first page is loaded again, from offset 0.
        assert_eq!(rows(cx, &tab), 200);
        assert_eq!(tab.fx.server.pages().last().map(|r| r.offset), Some(0));
    }

    #[gpui_kit::test]
    fn other_tabs_stay_usable_while_a_page_is_held(cx: &mut TestAppContext) {
        // Given: a table tab whose first page is held on the server.
        let mut gate = None;
        let tab = open_with(cx, |fx| gate = Some(fx.server.hold_pages()));
        let gate = gate.expect("gate");
        assert!(cx.update(|cx| tab.state.read(cx).loading));
        // When: opening and activating another tab meanwhile.
        let other = tab.fx.open_table("orders", cx);
        cx.update(|cx| tab.fx.tabs.update(cx, |tabs, cx| tabs.activate(other, cx)));
        // Then: it becomes the active tab although the grid is still empty.
        let active = cx.update(|cx| tab.fx.tabs.read(cx).active(WindowKey::Main));
        assert_eq!(active, Some(other));
        assert_eq!(rows(cx, &tab), 0);
        // When: the server answers the held request.
        gate.release_all();
        settle(cx, &tab);
        // Then: the first tab's grid fills.
        assert_eq!(rows(cx, &tab), 200);
    }

    #[gpui_kit::test]
    fn null_and_empty_names_are_displayed_differently(cx: &mut TestAppContext) {
        // Given: a loaded table whose 7th row has a NULL name and 11th an empty one.
        let tab = open(cx);
        let (null, empty) = cx.update(|cx| {
            let rows = &tab.state.read(cx).rows;
            (rows[6][1].clone(), rows[10][1].clone())
        });
        // When: displaying both cells.
        let (null, empty) = (cell_display(&null, false), cell_display(&empty, false));
        // Then: their text and tone differ.
        assert_ne!(null.text, empty.text);
        assert_eq!(null.tone, CellTone::MutedItalic);
        assert_eq!(empty.tone, CellTone::Normal);
    }

    #[gpui_kit::test]
    fn closing_a_tab_removes_its_state(cx: &mut TestAppContext) {
        // Given: an open table tab with a state.
        let tab = open(cx);
        assert!(cx.update(|cx| cx.global::<TableTabs>().get(tab.id).is_some()));
        // When: closing the tab.
        cx.update(|cx| tab.fx.tabs.update(cx, |tabs, cx| tabs.close(&[tab.id], cx)));
        // Then: its state is gone.
        assert!(cx.update(|cx| cx.global::<TableTabs>().get(tab.id).is_none()));
    }

    #[gpui_kit::test]
    fn opening_a_table_tab_creates_its_state(cx: &mut TestAppContext) {
        // Given: the table tab module is initialized.
        let tab = open(cx);
        // When: opening another table tab.
        let other = tab.fx.open_table("orders", cx);
        // Then: its state exists before any view was built.
        assert!(cx.update(|cx| cx.global::<TableTabs>().get(other).is_some()));
    }
}
