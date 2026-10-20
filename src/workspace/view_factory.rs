//! Tab view factory registry.

use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::{AnyView, App, Global, Window};

use super::tabs::{GlobalTabStore, TabId};

/// Kind of tab content, used to pick a view factory and close guards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TabKindTag {
    Table,
    Query,
}

type ViewBuilder = Rc<dyn Fn(TabId, &mut Window, &mut App) -> AnyView>;

/// GPUI global mapping tab kinds to the functions that build their views.
#[derive(Default)]
pub struct TabViewFactory {
    builders: HashMap<TabKindTag, ViewBuilder>,
}

impl Global for TabViewFactory {}

/// Installs an empty [`TabViewFactory`].
pub fn init(cx: &mut App) {
    cx.set_global(TabViewFactory::default());
}

/// Registers (or replaces) the view builder for a tab kind.
pub fn register(
    cx: &mut App,
    tag: TabKindTag,
    builder: impl Fn(TabId, &mut Window, &mut App) -> AnyView + 'static,
) {
    cx.global_mut::<TabViewFactory>()
        .builders
        .insert(tag, Rc::new(builder));
}

/// Builds the view of an open tab; `None` when the tab is unknown or its kind has no builder.
pub fn build(tab: TabId, window: &mut Window, cx: &mut App) -> Option<AnyView> {
    let tag = cx
        .global::<GlobalTabStore>()
        .0
        .read(cx)
        .tab(tab)?
        .kind
        .tag();
    let builder = cx.global::<TabViewFactory>().builders.get(&tag)?.clone();
    Some(builder(tab, window, cx))
}

#[cfg(test)]
mod tests {
    use gpui_kit::prelude::*;
    use gpui_kit::{Context, IntoElement, Render, TestAppContext, Window, div};

    use super::{TabKindTag, build, init, register};
    use crate::catalog::model::ConnectionId;
    use crate::settings::WindowMode;
    use crate::workspace::tabs;

    struct Dummy;

    impl Render for Dummy {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[gpui_kit::test]
    fn builds_a_registered_view_in_a_window(cx: &mut TestAppContext) {
        // Given: a query tab and a builder registered for the Query tag only.
        let tab = cx.update(|cx| {
            tabs::init(cx, WindowMode::Single);
            init(cx);
            register(cx, TabKindTag::Query, |_, _, cx| cx.new(|_| Dummy).into());
            let store = cx.global::<tabs::GlobalTabStore>().0.clone();
            store.update(cx, |store, cx| {
                store.open_query(ConnectionId::new(), None, cx)
            })
        });
        let window = cx.add_empty_window();
        // When: building the view.
        let built = window.update(|window, cx| build(tab, window, cx));
        // Then: a view is returned for it, and none for an unknown tab.
        assert!(built.is_some());
        let unknown = window.update(|window, cx| build(tabs::TabId(u64::MAX), window, cx));
        assert!(unknown.is_none());
    }

    #[gpui_kit::test]
    fn kinds_without_a_builder_build_nothing(cx: &mut TestAppContext) {
        // Given: a query tab and no registered builders.
        let tab = cx.update(|cx| {
            tabs::init(cx, WindowMode::Single);
            init(cx);
            let store = cx.global::<tabs::GlobalTabStore>().0.clone();
            store.update(cx, |store, cx| {
                store.open_query(ConnectionId::new(), None, cx)
            })
        });
        let window = cx.add_empty_window();
        // When: building the view.
        let built = window.update(|window, cx| build(tab, window, cx));
        // Then: nothing is built.
        assert!(built.is_none());
    }
}
