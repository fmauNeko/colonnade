//! Single-window workspace view.

// allow: SIZE_OK - window presentation stays in this file under the plan's fixed four-file ownership.

use super::{
    status_bar,
    tab_strip::{self, CloseActiveTab, NextTab, PreviousTab, TabStrip},
    tabs::{GlobalTabStore, TabId, TabKind, TabStoreEvent, WindowKey},
    view_factory,
};
use crate::catalog::{ops::DropPosition, store::GlobalConnectionStore};
use crate::sessions::GlobalSessions;
use crate::settings::{GlobalSettings, ThemeModeSetting};
use crate::sidebar::view::SidebarView;
use crate::tint::{chrome_tint, title_bar_colors};
use crate::{app, dialogs, settings_ui, themes, windows};
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::{ActiveTheme as _, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyView, Context, Entity, FocusHandle, IntoElement, Render, TestSupportExt as _, Window, div,
    px,
};
use std::collections::HashMap;

/// Window-bound workspace presentation; persistent tab state stays app-level.
pub struct WorkspaceView {
    window_key: WindowKey,
    sidebar: Entity<SidebarView>,
    tab_strip: Entity<TabStrip>,
    views: HashMap<TabId, AnyView>,
    focus_handle: FocusHandle,
}

impl WorkspaceView {
    /// Creates a workspace for a routing key in its host window.
    pub fn new(window_key: WindowKey, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(SidebarView::new);
        let tab_strip = cx.new(|cx| TabStrip::new(window_key, cx));
        let tabs = cx.global::<GlobalTabStore>().0.clone();
        cx.subscribe(&tabs, |this, tabs, event: &TabStoreEvent, cx| {
            match event {
                TabStoreEvent::Closed(_)
                | TabStoreEvent::ModeChanged(_)
                | TabStoreEvent::Reordered => {
                    let visible: Vec<_> = tabs
                        .read(cx)
                        .visible_tabs(this.window_key)
                        .iter()
                        .map(|t| t.id)
                        .collect();
                    this.views.retain(|tab, _| visible.contains(tab));
                }
                TabStoreEvent::Opened { .. } | TabStoreEvent::Activated { .. } => {}
            }
            cx.notify();
        })
        .detach();
        let connections = cx.global::<GlobalConnectionStore>().0.clone();
        cx.subscribe(
            &connections,
            |_, _, _: &crate::catalog::store::ConnectionStoreEvent, cx| cx.notify(),
        )
        .detach();
        let sessions = cx.global::<GlobalSessions>().0.clone();
        cx.subscribe(&sessions, |_, _, _: &crate::sessions::SessionEvent, cx| {
            cx.notify();
        })
        .detach();
        cx.observe_window_activation(window, |_, _, cx| cx.notify())
            .detach();
        window
            .observe_window_appearance(|window, cx| {
                let mode = cx
                    .global::<GlobalSettings>()
                    .0
                    .read(cx)
                    .settings()
                    .theme
                    .mode;
                match mode {
                    ThemeModeSetting::System => {
                        app::apply_theme(cx, window.appearance());
                    }
                    ThemeModeSetting::Light | ThemeModeSetting::Dark => {}
                }
            })
            .detach();
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        Self {
            window_key,
            sidebar,
            tab_strip,
            views: HashMap::new(),
            focus_handle,
        }
    }

    /// The window's tab-routing key.
    #[must_use]
    pub const fn window_key(&self) -> WindowKey {
        self.window_key
    }

    /// The sidebar's current accent (tint) color.
    #[cfg(test)]
    pub(crate) fn sidebar_accent(&self, cx: &gpui_kit::App) -> Option<gpui_kit::Hsla> {
        self.sidebar.read(cx).accent()
    }

    fn new_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tabs = cx.global::<GlobalTabStore>().0.clone();
        let active = tabs
            .read(cx)
            .active(self.window_key)
            .and_then(|id| tabs.read(cx).tab(id))
            .cloned();
        let connection = active
            .as_ref()
            .map(|t| t.connection)
            .or_else(|| self.sidebar.read(cx).selected_connection());
        match connection {
            Some(connection) => {
                let database = active.and_then(|t| match t.kind {
                    TabKind::Table(object) => object.database,
                    TabKind::Query { database, .. } => database,
                });
                tabs.update(cx, |s, cx| {
                    s.open_query(connection, database, cx);
                });
                self.focus_handle.focus(window, cx);
            }
            None => crate::prompts::notify(
                window,
                cx,
                crate::prompts::Notice {
                    level: crate::prompts::NoticeLevel::Info,
                    message: "Select a connection before opening a query".into(),
                },
            ),
        }
    }

    fn active_content(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let active = cx
            .global::<GlobalTabStore>()
            .0
            .read(cx)
            .active(self.window_key);
        if let Some(tab) = active {
            if !self.views.contains_key(&tab)
                && let Some(view) = view_factory::build(tab, window, cx)
            {
                match cx
                    .global::<GlobalTabStore>()
                    .0
                    .read(cx)
                    .tab(tab)
                    .map(|t| &t.kind)
                {
                    Some(TabKind::Table(_)) => {
                        if let Some(state) = cx
                            .global::<crate::table_tab::state::TableTabs>()
                            .get(tab)
                            .cloned()
                        {
                            cx.observe(&state, |_, _, cx| cx.notify()).detach();
                        }
                    }
                    Some(TabKind::Query { .. }) => {
                        if let Some(state) = crate::query_tab::state::QueryTabs::state(cx, tab) {
                            cx.observe(&state, |_, _, cx| cx.notify()).detach();
                        }
                    }
                    None => {}
                }
                self.views.insert(tab, view);
            }
            if let Some(view) = self.views.get(&tab) {
                return view.clone().into_any_element();
            }
        }
        v_flex()
            .id("workspace-empty")
            .test_support()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .text_color(cx.theme().muted_foreground)
            .child("Open a table or start a query")
            .into_any_element()
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, color) = match self.window_key {
            WindowKey::Main => ("Colonnade".to_owned(), None),
            WindowKey::Connection(id) => cx
                .global::<GlobalConnectionStore>()
                .0
                .read(cx)
                .profile(id)
                .map_or_else(
                    || ("Colonnade".to_owned(), None),
                    |p| (p.name.clone(), Some(p.color.effective())),
                ),
        };
        window.set_window_title(&title);
        let tint = chrome_tint(self.window_key, cx);
        let accent = tint.map(|t| t.accent);
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.set_accent(accent, cx));
        let title_colors = tint.map(|t| title_bar_colors(&t, window.is_window_active()));
        let content = self.active_content(window, cx);
        let status = status_bar::render(self.window_key, cx);
        v_flex()
            .size_full()
            .key_context(tab_strip::KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|this, _: &CloseActiveTab, w, cx| {
                tab_strip::close_active_tab(this.window_key, w, cx);
            }))
            .on_action(cx.listener(|this, _: &NextTab, _, cx| {
                tab_strip::activate_next(this.window_key, cx);
            }))
            .on_action(cx.listener(|this, _: &PreviousTab, _, cx| {
                tab_strip::activate_previous(this.window_key, cx);
            }))
            .on_action(cx.listener(|this, _: &app::NewQuery, w, cx| this.new_query(w, cx)))
            .on_action(cx.listener(|_, _: &app::NewConnection, w, cx| {
                dialogs::connection::open_new(DropPosition::RootEnd, w, cx);
            }))
            .on_action(cx.listener(|_, _: &app::NewFolder, w, cx| {
                dialogs::folder::open_new(DropPosition::RootEnd, w, cx);
            }))
            .on_action(cx.listener(|_, _: &app::OpenSettings, w, cx| settings_ui::open(w, cx)))
            .on_action(cx.listener(|_, _: &app::SelectTheme, w, cx| themes::selector::open(w, cx)))
            .child(
                TitleBar::new()
                    .when_some(title_colors, |bar, (background, foreground)| {
                        bar.bg(background).text_color(foreground)
                    })
                    .on_close_window(|_, w, cx| windows::request_window_close(w, cx))
                    .child(
                        h_flex()
                            .gap_2()
                            .when_some(color, |row, color| {
                                row.child(div().size(px(8.)).rounded_full().bg(color.to_hsla()))
                            })
                            .child(title),
                    ),
            )
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("workspace-split")
                        .child(
                            resizable_panel()
                                .size(px(260.))
                                .size_range(px(180.)..px(f32::MAX))
                                .child(self.sidebar.clone()),
                        )
                        .child(
                            resizable_panel().child(
                                v_flex()
                                    .size_full()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .flex()
                                            .items_stretch()
                                            .when_some(accent, |strip, accent| {
                                                strip.border_b(px(2.)).border_color(accent)
                                            })
                                            .flex_none()
                                            .min_h(px(36.))
                                            .child(self.tab_strip.clone()),
                                    )
                                    .child(
                                        div()
                                            .id("workspace-content")
                                            .test_support()
                                            .flex_1()
                                            .min_h_0()
                                            .child(content),
                                    ),
                            ),
                        ),
                ),
            )
            .child(status)
    }
}
