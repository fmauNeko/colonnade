//! Query tab view with dialect highlighting, run, results and stop.
//!
//! The SQL text lives in the app-level [`QueryTabState`]; the editor only mirrors it, so a view
//! rebuilt from the [`TabViewFactory`](crate::workspace::view_factory) shows the same text and
//! the last results.

// allow: SIZE_OK - the plan assigns the toolbar, editor, results and their tests to this file.
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, InputEvent};
use gpui_kit::component::resizable::{resizable_panel, v_resizable};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, Context, Entity, FocusHandle, IntoElement, KeyBinding, Render, SharedString,
    Subscription, Task, TestSupportExt as _, Window, div, px,
};

use super::state::{QueryTabState, QueryTabs, RunState, install};
use crate::db::model::{ROW_CAP, StatementResult};
use crate::grid::RowsDelegate;
use crate::workspace::view_factory::{self, TabKindTag};

gpui_kit::actions!(query_tab, [RunQuery, CancelQuery]);

/// Key context the query tab sets so its shortcuts apply.
pub const KEY_CONTEXT: &str = "QueryTab";

const EDITOR_PANEL_HEIGHT: gpui_kit::Pixels = px(330.);
const STATUS_REFRESH: Duration = Duration::from_millis(500);

/// Binds the query shortcuts, registers the query tab view and installs the tab state handling.
///
/// Needs the tab store, the view factory, the connection store and the sessions.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-enter", RunQuery, Some(KEY_CONTEXT)),
        KeyBinding::new("secondary-.", CancelQuery, Some(KEY_CONTEXT)),
        // The editor binds both chords itself (a newline, code actions); these later bindings
        // for an editor inside the tab take precedence there.
        KeyBinding::new("secondary-enter", RunQuery, Some("QueryTab > Input")),
        KeyBinding::new("secondary-.", CancelQuery, Some("QueryTab > Input")),
    ]);
    install(cx);
    view_factory::register(
        cx,
        TabKindTag::Query,
        |tab, window, cx| match QueryTabs::ensure(cx, tab) {
            Some(state) => cx.new(|cx| QueryTabView::new(state, window, cx)).into(),
            None => cx.new(|_| MissingState).into(),
        },
    );
}

/// Shown for a query tab whose connection no longer exists.
struct MissingState;

impl Render for MissingState {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().id("query-missing").test_support().size_full()
    }
}

struct GridCache {
    results: Arc<Vec<StatementResult>>,
    index: usize,
    table: Entity<TableState<RowsDelegate>>,
}

/// The view of one query tab.
pub struct QueryTabView {
    state: Entity<QueryTabState>,
    editor: Entity<EditorState>,
    focus: FocusHandle,
    grid: Option<GridCache>,
    ticker: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl QueryTabView {
    pub fn new(state: Entity<QueryTabState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (language, text) = {
            let state = state.read(cx);
            (state.dialect.language_name(), state.text.clone())
        };
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(language)
                .default_value(text)
        });
        let subscriptions = vec![
            cx.subscribe_in(
                &editor,
                window,
                |this, editor, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        let value = editor.read(cx).value().to_string();
                        this.state.update(cx, |state, _| state.text = value);
                    }
                },
            ),
            cx.observe(&state, |_, _, cx| cx.notify()),
        ];
        Self {
            state,
            editor,
            focus: cx.focus_handle(),
            grid: None,
            ticker: None,
            _subscriptions: subscriptions,
        }
    }

    #[must_use]
    pub const fn editor(&self) -> &Entity<EditorState> {
        &self.editor
    }

    /// The selection when it holds more than whitespace, else the whole text.
    fn sql_to_run(&self, cx: &App) -> String {
        let selected = self.editor.read(cx).selected_text().to_string();
        if selected.trim().is_empty() {
            self.state.read(cx).text.clone()
        } else {
            selected
        }
    }

    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sql = self.sql_to_run(cx);
        // The disabled Run button drops focus; keep the shortcuts alive in the editor.
        self.editor
            .update(cx, |editor, cx| editor.focus(window, cx));
        self.state
            .update(cx, |state, cx| state.run(&sql, window, cx));
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, QueryTabState::stop);
    }

    /// Re-renders twice a second while running so the elapsed time moves.
    fn sync_ticker(&mut self, running: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !running {
            self.ticker = None;
        } else if self.ticker.is_none() {
            self.ticker = Some(cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(STATUS_REFRESH).await;
                drop(this.update(cx, |this, cx| {
                    this.ticker = None;
                    cx.notify();
                }));
            }));
        }
    }

    fn status_text(run: &RunState) -> SharedString {
        match run {
            RunState::Idle => "Ready".into(),
            RunState::Running { started, .. } => {
                format!("Running… {}s", started.elapsed().as_secs()).into()
            }
            RunState::Done { results, elapsed } => {
                let count = results.len();
                let unit = if count == 1 {
                    "statement"
                } else {
                    "statements"
                };
                format!("{count} {unit} · {} ms", elapsed.as_millis()).into()
            }
            RunState::Failed { .. } => "Failed".into(),
            RunState::Cancelled { .. } => "Cancelled".into(),
        }
    }

    fn toolbar(&self, run: &RunState, cx: &mut Context<Self>) -> impl IntoElement {
        let running = matches!(run, RunState::Running { .. });
        let database = self.state.read(cx).database.clone().map_or_else(
            || SharedString::from("Default database"),
            SharedString::from,
        );
        let theme = cx.theme();
        let (border, muted) = (theme.border, theme.muted_foreground);
        h_flex()
            .gap_2()
            .p_2()
            .items_center()
            .border_b_1()
            .border_color(border)
            .child(
                Button::new("query-run")
                    .primary()
                    .icon(IconName::Play)
                    .label("Run")
                    .disabled(running)
                    .on_click(cx.listener(|this, _, window, cx| this.run(window, cx))),
            )
            .when(running, |bar| {
                bar.child(
                    Button::new("query-stop")
                        .icon(IconName::SquareStop)
                        .label("Stop")
                        .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                )
            })
            .child(
                div()
                    .id("query-database")
                    .test_support()
                    .text_color(muted)
                    .child(database),
            )
            .child(
                div()
                    .id("query-status")
                    .test_support()
                    .ml_auto()
                    .text_color(muted)
                    .child(Self::status_text(run)),
            )
    }

    fn grid_for(
        &mut self,
        results: &Arc<Vec<StatementResult>>,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TableState<RowsDelegate>>> {
        if let Some(cache) = &self.grid
            && cache.index == index
            && Arc::ptr_eq(&cache.results, results)
        {
            return Some(cache.table.clone());
        }
        let StatementResult::Rows { columns, rows, .. } = results.get(index)? else {
            return None;
        };
        let delegate = RowsDelegate {
            columns: columns.clone(),
            rows: Arc::new(rows.clone()),
        };
        let table = cx.new(|cx| TableState::new(delegate, window, cx));
        self.grid = Some(GridCache {
            results: results.clone(),
            index,
            table: table.clone(),
        });
        Some(table)
    }

    fn statement_tabs(&self, count: usize, selected: usize) -> impl IntoElement + use<> {
        let state = self.state.clone();
        TabBar::new("query-result-tabs")
            .underline()
            .selected_index(selected)
            .children((0..count).map(|ix| {
                Tab::new().child(
                    div()
                        .id(SharedString::from(format!("query-result-tab:{ix}")))
                        .test_support()
                        .child(format!("Result {}", ix + 1)),
                )
            }))
            .on_click(move |ix, _, cx| {
                let ix = *ix;
                state.update(cx, |state, cx| state.select_result(ix, cx));
            })
    }

    fn statements(
        &mut self,
        results: &Arc<Vec<StatementResult>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let last = results.len().saturating_sub(1);
        let selected = self.state.read(cx).result_tab().min(last);
        let body = match results.get(selected) {
            Some(StatementResult::Rows { truncated, .. }) => {
                let grid = self.grid_for(results, selected, window, cx);
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .when_some(grid, |body, table| {
                        body.child(
                            div()
                                .id("query-result-grid")
                                .test_support()
                                .flex_1()
                                .min_h_0()
                                .child(DataTable::new(&table)),
                        )
                    })
                    .when(*truncated, |body| {
                        body.child(
                            div()
                                .id("query-truncated")
                                .test_support()
                                .p_2()
                                .text_color(cx.theme().warning)
                                .child(format!("Only the first {ROW_CAP} rows are shown.")),
                        )
                    })
                    .into_any_element()
            }
            Some(StatementResult::Affected { rows }) => div()
                .id("query-affected")
                .test_support()
                .p_3()
                .child(format!("{rows} rows affected"))
                .into_any_element(),
            None => div().into_any_element(),
        };
        v_flex()
            .size_full()
            .when(results.len() > 1, |column| {
                column.child(self.statement_tabs(results.len(), selected))
            })
            .child(body)
            .into_any_element()
    }

    fn results(
        &mut self,
        run: &RunState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (danger, muted) = (theme.danger, theme.muted_foreground);
        match run {
            RunState::Idle | RunState::Running { .. } => div().size_full().into_any_element(),
            RunState::Done { results, .. } => self.statements(results, window, cx),
            RunState::Failed { error, .. } => div()
                .id("query-error")
                .test_support()
                .size_full()
                .min_w_0()
                .overflow_y_scroll()
                .p_3()
                .text_color(danger)
                .child(error.to_string())
                .into_any_element(),
            RunState::Cancelled { .. } => div()
                .id("query-cancelled")
                .test_support()
                .p_3()
                .text_color(muted)
                .child("The query was cancelled.")
                .into_any_element(),
        }
    }
}

impl Render for QueryTabView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let run = self.state.read(cx).run_state().clone();
        self.sync_ticker(matches!(run, RunState::Running { .. }), window, cx);
        let results = self.results(&run, window, cx);
        v_flex()
            .size_full()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &RunQuery, window, cx| this.run(window, cx)))
            .on_action(cx.listener(|this, _: &CancelQuery, _, cx| this.stop(cx)))
            .child(self.toolbar(&run, cx))
            .child(
                v_resizable("query-split")
                    .child(
                        resizable_panel()
                            .size(EDITOR_PANEL_HEIGHT)
                            .child(Editor::new(&self.editor).size_full()),
                    )
                    .child(resizable_panel().child(results)),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::input::EditorState;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyView, AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, IntoElement,
        ParentElement as _, Point, Render, Styled as _, TestAppContext, Window, WindowBounds,
        WindowOptions, div, px, size,
    };

    use super::QueryTabView;
    use crate::catalog::model::Driver;
    use crate::db::fake::ExecutedQuery;
    use crate::db::model::{Cell, ColumnInfo, DbError, ROW_CAP, StatementResult};
    use crate::query_tab::state::harness::{Fx, setup};
    use crate::query_tab::state::{QueryTabs, RunState};
    use crate::workspace::tabs::TabId;
    use crate::workspace::view_factory;

    struct Host {
        view: AnyView,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.view.clone())
        }
    }

    struct Open {
        window: AnyWindowHandle,
        host: Entity<Host>,
        tab: TabId,
    }

    fn open_view(cx: &mut TestAppContext, fx: &Fx) -> Open {
        let (tab, _) = fx.open_tab(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                Point::default(),
                size(px(900.), px(700.)),
            ))),
            ..Default::default()
        };
        let (window, host) = cx
            .update(|cx| {
                gpui_kit::open_window(options, cx, |window, cx| {
                    let view = view_factory::build(tab, window, cx).expect("a query view");
                    cx.new(|_| Host { view })
                })
            })
            .expect("window opens");
        Open { window, host, tab }
    }

    fn ui<R>(
        cx: &mut TestAppContext,
        open: &Open,
        f: impl FnOnce(&mut Window, &mut App) -> R,
    ) -> R {
        cx.update_window(open.window, |_, window, cx| {
            window.render_frame(cx);
            let out = f(window, cx);
            window.render_frame(cx);
            out
        })
        .expect("window")
    }

    fn query_view(cx: &mut TestAppContext, open: &Open) -> Entity<QueryTabView> {
        cx.update(|cx| open.host.read(cx).view.clone().downcast::<QueryTabView>())
            .expect("a QueryTabView")
    }

    fn editor(cx: &mut TestAppContext, open: &Open) -> Entity<EditorState> {
        let view = query_view(cx, open);
        cx.update(|cx| view.read(cx).editor().clone())
    }

    fn click(cx: &mut TestAppContext, open: &Open, id: &'static str) {
        ui(cx, open, |window, cx| window.click(id, cx));
        cx.run_until_parked();
    }

    fn type_sql(cx: &mut TestAppContext, open: &Open, sql: &str) {
        let editor = editor(cx, open);
        ui(cx, open, |window, cx| {
            window.click(("input", editor.entity_id()), cx);
            window.input(sql, cx);
        });
    }

    fn shown(cx: &mut TestAppContext, open: &Open, id: &'static str) -> bool {
        ui(cx, open, |window, _| {
            window.try_find(id).is_some_and(|found| found.visible())
        })
    }

    /// Kit gives an enabled button a Focus action and a disabled one none, so only an
    /// enabled button reports a focus state.
    fn enabled(cx: &mut TestAppContext, open: &Open, id: &'static str) -> bool {
        ui(cx, open, |window, _| window.find(id).focused().is_some())
    }

    fn rows_result(truncated: bool) -> StatementResult {
        StatementResult::Rows {
            columns: vec![ColumnInfo {
                name: "n".into(),
                type_name: "integer".into(),
                numeric: true,
            }],
            rows: vec![vec![Cell::text("1".into())], vec![Cell::text("2".into())]],
            truncated,
        }
    }

    #[gpui_kit::test]
    fn editor_language_follows_the_connection_dialect(cx: &mut TestAppContext) {
        for (driver, language) in [
            (Driver::MySql, "sql-mysql"),
            (Driver::Postgres, "sql-postgres"),
            (Driver::Sqlite, "sql-sqlite"),
        ] {
            // Given: a query tab of a connection with this driver.
            let fx = setup(cx, driver);
            // When: its view is built.
            let open = open_view(cx, &fx);
            // Then: the editor highlights with that dialect's language.
            let editor = editor(cx, &open);
            assert_eq!(cx.update(|cx| editor.read(cx).language_name()), language);
        }
    }

    #[gpui_kit::test]
    fn run_button_sends_the_whole_text(cx: &mut TestAppContext) {
        // Given: typed SQL with several statements.
        let fx = setup(cx, Driver::Postgres);
        let open = open_view(cx, &fx);
        type_sql(cx, &open, "SELECT 1; SELECT 2");
        // When: clicking Run.
        click(cx, &open, "query-run");
        // Then: exactly that text reached the server, with the row cap.
        assert_eq!(
            fx.server.executed_queries(),
            [ExecutedQuery {
                database: None,
                sql: "SELECT 1; SELECT 2".into(),
                row_cap: ROW_CAP
            }]
        );
    }

    #[gpui_kit::test]
    fn run_shortcut_sends_only_the_selection(cx: &mut TestAppContext) {
        // Given: typed SQL with part of it selected.
        let fx = setup(cx, Driver::Postgres);
        let open = open_view(cx, &fx);
        type_sql(cx, &open, "SELECT 1; SELECT 2");
        let editor = editor(cx, &open);
        cx.update(|cx| editor.update(cx, |state, cx| state.set_selected_range(10..18, cx)));
        // When: pressing the run shortcut.
        ui(cx, &open, |window, cx| window.press("secondary-enter", cx));
        cx.run_until_parked();
        // Then: only the selection ran and the text is untouched.
        assert_eq!(fx.server.executed_sql(), ["SELECT 2"]);
        assert_eq!(
            cx.update(|cx| editor.read(cx).value().to_string()),
            "SELECT 1; SELECT 2"
        );
    }

    #[gpui_kit::test]
    fn blank_selection_runs_the_whole_text(cx: &mut TestAppContext) {
        // Given: typed SQL with only whitespace selected.
        let fx = setup(cx, Driver::Postgres);
        let open = open_view(cx, &fx);
        type_sql(cx, &open, "SELECT 1; SELECT 2");
        let editor = editor(cx, &open);
        cx.update(|cx| editor.update(cx, |state, cx| state.set_selected_range(9..10, cx)));
        // When: pressing the run shortcut.
        ui(cx, &open, |window, cx| window.press("secondary-enter", cx));
        cx.run_until_parked();
        // Then: the whole text ran.
        assert_eq!(fx.server.executed_sql(), ["SELECT 1; SELECT 2"]);
    }

    #[gpui_kit::test]
    fn several_statements_show_one_result_tab_each(cx: &mut TestAppContext) {
        // Given: a script of rows, rows and an affected count.
        let fx = setup(cx, Driver::Postgres);
        fx.server.script_query(
            "S".into(),
            Ok(vec![
                rows_result(false),
                rows_result(false),
                StatementResult::Affected { rows: 3 },
            ]),
        );
        let open = open_view(cx, &fx);
        type_sql(cx, &open, "S");
        // When: running it.
        click(cx, &open, "query-run");
        // Then: three result tabs exist and the first shows a grid.
        assert!(shown(cx, &open, "query-result-tab:0"));
        assert!(shown(cx, &open, "query-result-tab:1"));
        assert!(shown(cx, &open, "query-result-tab:2"));
        assert!(shown(cx, &open, "query-result-grid"));
        assert!(!shown(cx, &open, "query-affected"));
        // When: selecting the affected statement.
        click(cx, &open, "query-result-tab:2");
        // Then: the affected count replaces the grid.
        assert!(shown(cx, &open, "query-affected"));
        assert!(!shown(cx, &open, "query-result-grid"));
    }

    #[gpui_kit::test]
    fn query_error_is_shown(cx: &mut TestAppContext) {
        // Given: a statement that fails.
        let fx = setup(cx, Driver::Postgres);
        let error = DbError::Query {
            message: "boom".into(),
            code: None,
        };
        fx.server.script_query("BAD".into(), Err(error));
        let open = open_view(cx, &fx);
        type_sql(cx, &open, "BAD");
        // When: running it.
        click(cx, &open, "query-run");
        // Then: the error is visible and Run is available again.
        assert!(shown(cx, &open, "query-error"));
        assert!(!shown(cx, &open, "query-result-grid"));
        assert!(enabled(cx, &open, "query-run"));
    }

    #[gpui_kit::test]
    fn long_query_error_wraps_inside_the_window(cx: &mut TestAppContext) {
        // Given: a statement failing with a very long, multi-word message.
        let fx = setup(cx, Driver::Postgres);
        let message = "relation does not exist near token ".repeat(60);
        let error = DbError::Query {
            message,
            code: None,
        };
        fx.server.script_query("BAD".into(), Err(error));
        let open = open_view(cx, &fx);
        type_sql(cx, &open, "BAD");
        // When: running it.
        click(cx, &open, "query-run");
        // Then: the error stays inside the 900 px window instead of running past its edge.
        let bounds = ui(cx, &open, |window, _| window.find("query-error").bounds());
        assert!(bounds.right() <= px(900.), "error bounds: {bounds:?}");
    }

    #[gpui_kit::test]
    fn truncated_result_shows_a_notice_only_when_truncated(cx: &mut TestAppContext) {
        // Given: a truncated result and a complete one.
        let fx = setup(cx, Driver::Postgres);
        fx.server
            .script_query("CUT".into(), Ok(vec![rows_result(true)]));
        fx.server
            .script_query("ALL".into(), Ok(vec![rows_result(false)]));
        let open = open_view(cx, &fx);
        // When: running the truncated one.
        type_sql(cx, &open, "CUT");
        click(cx, &open, "query-run");
        // Then: the notice shows.
        assert!(shown(cx, &open, "query-truncated"));
        // When: running the complete one.
        let editor = editor(cx, &open);
        ui(cx, &open, |window, cx| {
            window.click(("input", editor.entity_id()), cx);
            window.press("secondary-a", cx);
            window.input("ALL", cx);
        });
        click(cx, &open, "query-run");
        // Then: it does not.
        assert!(shown(cx, &open, "query-result-grid"));
        assert!(!shown(cx, &open, "query-truncated"));
    }

    fn state_of(
        cx: &mut TestAppContext,
        open: &Open,
    ) -> (TabId, Entity<crate::query_tab::state::QueryTabState>) {
        let state = cx
            .update(|cx| QueryTabs::state(cx, open.tab))
            .expect("state");
        (open.tab, state)
    }

    #[gpui_kit::test]
    fn stop_cancels_a_running_query(cx: &mut TestAppContext) {
        // Given: a query held by the server and a second tab of the same connection.
        let fx = setup(cx, Driver::Postgres);
        let _queries = fx.server.hold_queries();
        let open = open_view(cx, &fx);
        type_sql(cx, &open, "SELECT pg_sleep(60)");
        // When: running it.
        click(cx, &open, "query-run");
        // Then: it is running: Run is disabled and Stop is offered.
        assert!(!enabled(cx, &open, "query-run"));
        assert!(shown(cx, &open, "query-stop"));
        // And: the tab store still reacts meanwhile.
        let (second, _) = fx.open_tab(cx);
        cx.update(|cx| fx.tabs.update(cx, |tabs, cx| tabs.activate(open.tab, cx)));
        assert_eq!(
            cx.update(|cx| fx
                .tabs
                .read(cx)
                .active(crate::workspace::tabs::WindowKey::Main)),
            Some(open.tab)
        );
        assert_ne!(second, open.tab);
        // When: pressing Stop.
        click(cx, &open, "query-stop");
        // Then: the run ends as cancelled.
        assert!(shown(cx, &open, "query-cancelled"));
        assert!(!shown(cx, &open, "query-stop"));
        let (_, state) = state_of(cx, &open);
        assert!(cx.update(|cx| matches!(state.read(cx).run_state(), RunState::Cancelled { .. })));
    }

    #[gpui_kit::test]
    fn cancel_shortcut_stops_a_running_query(cx: &mut TestAppContext) {
        // Given: a held query started from the editor.
        let fx = setup(cx, Driver::Postgres);
        let _queries = fx.server.hold_queries();
        let open = open_view(cx, &fx);
        type_sql(cx, &open, "SELECT 1");
        click(cx, &open, "query-run");
        assert!(shown(cx, &open, "query-stop"));
        // When: pressing the cancel shortcut.
        ui(cx, &open, |window, cx| window.press("secondary-.", cx));
        cx.run_until_parked();
        // Then: the query was cancelled through its connection.
        assert!(shown(cx, &open, "query-cancelled"));
        assert_eq!(fx.server.lifecycle_counts().2, 1);
    }

    #[gpui_kit::test]
    fn rebuilding_the_view_keeps_text_and_results(cx: &mut TestAppContext) {
        // Given: a finished run in a built view.
        let fx = setup(cx, Driver::Postgres);
        fx.server
            .script_query("SELECT 1".into(), Ok(vec![rows_result(false)]));
        let open = open_view(cx, &fx);
        type_sql(cx, &open, "SELECT 1");
        click(cx, &open, "query-run");
        // When: the view is dropped and rebuilt from the factory.
        let tab = open.tab;
        cx.update_window(open.window, |_, window, cx| {
            let view = view_factory::build(tab, window, cx).expect("a query view");
            open.host.update(cx, |host, cx| {
                host.view = view;
                cx.notify();
            });
        })
        .expect("window");
        // Then: the new editor holds the text and the results are still shown.
        let editor = editor(cx, &open);
        assert_eq!(
            cx.update(|cx| editor.read(cx).value().to_string()),
            "SELECT 1"
        );
        assert!(shown(cx, &open, "query-result-grid"));
    }
}
