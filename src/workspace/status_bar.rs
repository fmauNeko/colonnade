//! Workspace status bar.

use super::tabs::{GlobalTabStore, TabKind, WindowKey};
use crate::catalog::store::GlobalConnectionStore;
use crate::color::Rgb;
use crate::query_tab::state::{QueryTabs, RunState};
use crate::sessions::{GlobalSessions, SessionStatus};
use crate::table_tab::state::TableTabs;
use crate::tint::{ChromeTint, chrome_tint};
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, IntoElement, SharedString, TestSupportExt as _, div, px};

/// Observable status of the active tab in one workspace.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusModel {
    pub connection: Option<(SharedString, Rgb, SharedString)>,
    pub tab_info: Option<SharedString>,
    /// Chrome tint of a connection window; `None` keeps the theme colors.
    pub tint: Option<ChromeTint>,
}

/// Projects app-level tab/session state without creating window-bound views.
#[must_use]
pub fn status_model(window_key: WindowKey, cx: &App) -> StatusModel {
    let tabs = cx.global::<GlobalTabStore>().0.read(cx);
    let active = tabs.active(window_key).and_then(|id| tabs.tab(id));
    let tint = chrome_tint(window_key, cx);
    let Some(tab) = active else {
        return StatusModel {
            connection: None,
            tab_info: None,
            tint,
        };
    };
    let connection = cx
        .global::<GlobalConnectionStore>()
        .0
        .read(cx)
        .profile(tab.connection)
        .map(|profile| {
            let status = match cx
                .global::<GlobalSessions>()
                .0
                .read(cx)
                .status(tab.connection)
            {
                SessionStatus::Disconnected => "Disconnected",
                SessionStatus::Connecting => "Connecting",
                SessionStatus::Connected(_) => "Connected",
                SessionStatus::Failed(_) => "Failed",
            };
            (
                profile.name.clone().into(),
                profile.color.effective(),
                status.into(),
            )
        });
    let tab_info = match &tab.kind {
        TabKind::Table(_) => cx.global::<TableTabs>().get(tab.id).map(|state| {
            let state = state.read(cx);
            if state.has_more {
                format!("{} rows loaded", state.rows.len()).into()
            } else {
                format!("all {} rows", state.rows.len()).into()
            }
        }),
        TabKind::Query { .. } => {
            QueryTabs::state(cx, tab.id).map(|state| match state.read(cx).run_state() {
                RunState::Idle => "Ready".into(),
                RunState::Running {
                    cancel_requested, ..
                } => {
                    if *cancel_requested {
                        "Cancelling".into()
                    } else {
                        "Running".into()
                    }
                }
                RunState::Done { elapsed, .. } => {
                    format!("Done · {} ms", elapsed.as_millis()).into()
                }
                RunState::Failed { .. } => "Failed".into(),
                RunState::Cancelled { .. } => "Cancelled".into(),
            })
        }
    };
    StatusModel {
        connection,
        tab_info,
        tint,
    }
}

/// Renders the status bar: tinted in connection windows, else the global theme.
pub fn render(window_key: WindowKey, cx: &App) -> AnyElement {
    let model = status_model(window_key, cx);
    let theme = cx.theme();
    let (background, foreground) = model.tint.map_or_else(
        || (theme.status_bar, theme.foreground),
        |tint| (tint.status_bar_bg, tint.status_bar_fg),
    );
    h_flex()
        .h(px(26.))
        .flex_none()
        .px_2()
        .gap_3()
        .text_xs()
        .bg(background)
        .border_t_1()
        .border_color(theme.status_bar_border)
        .text_color(foreground)
        .child(
            h_flex()
                .id("status-connection")
                .test_support()
                .gap_2()
                .px_1()
                .rounded_sm()
                .when_some(model.tint, |row, tint| {
                    row.hover(move |style| style.bg(tint.status_bar_hover_bg))
                })
                .when_some(model.connection, |row, (name, color, status)| {
                    row.child(div().size(px(8.)).rounded_full().bg(color.to_hsla()))
                        .child(name)
                        .child(status)
                }),
        )
        .child(div().flex_1())
        .child(
            div()
                .id("status-tab-info")
                .test_support()
                .children(model.tab_info),
        )
        .into_any_element()
}
