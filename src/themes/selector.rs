//! Theme selector dialog.
//!
//! A Zed-like picker: a filter input and the catalog's themes grouped Light
//! then Dark. Up/Down move the highlight and preview it without saving;
//! Enter or a click confirms and saves; Escape (or any other close) re-applies
//! the saved selection.

use gpui_kit::TestSupportExt as _;
use gpui_kit::base::actions::{Confirm, SelectDown, SelectUp};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, ThemeMode, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::{
    App, ClickEvent, Context, Entity, Focusable as _, SharedString, Subscription, Window,
    WindowAppearance, div, px,
};

use crate::prompts::{Notice, NoticeLevel, notify};
use crate::settings::{GlobalSettings, ThemeModeSetting, ThemeSettings};
use crate::themes::catalog::{self, ThemeCatalog, ThemeEntry};

/// One selectable row of the selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeRow {
    /// Theme name.
    pub name: SharedString,
    /// Light or dark.
    pub mode: ThemeMode,
}

/// Rows whose name contains `filter` (case-insensitive), Light then Dark,
/// each group in catalog order.
#[must_use]
pub fn filtered_rows(entries: &[ThemeEntry], filter: &str) -> Vec<ThemeRow> {
    let needle = filter.trim().to_lowercase();
    let matching = |entry: &&ThemeEntry| entry.name.to_lowercase().contains(&needle);
    let group = |dark: bool| {
        entries
            .iter()
            .filter(move |entry| entry.mode.is_dark() == dark)
            .filter(matching)
            .map(|entry| ThemeRow {
                name: entry.name.clone(),
                mode: entry.mode,
            })
    };
    group(false).chain(group(true)).collect()
}

/// Settings after confirming theme `name` of `mode`: its slot is set, and a
/// static mode setting follows the theme (System stays System).
#[must_use]
pub fn confirmed_theme(current: &ThemeSettings, name: &str, mode: ThemeMode) -> ThemeSettings {
    let mut next = current.clone();
    match mode {
        ThemeMode::Light => {
            name.clone_into(&mut next.light);
            if next.mode != ThemeModeSetting::System {
                next.mode = ThemeModeSetting::Light;
            }
        }
        ThemeMode::Dark => {
            name.clone_into(&mut next.dark);
            if next.mode != ThemeModeSetting::System {
                next.mode = ThemeModeSetting::Dark;
            }
        }
    }
    next
}

/// Whether a confirmed theme of `mode` stays invisible until the system
/// appearance changes (System mode under the other appearance).
#[must_use]
pub fn applies_later(
    setting: ThemeModeSetting,
    appearance: WindowAppearance,
    mode: ThemeMode,
) -> bool {
    match setting {
        ThemeModeSetting::System => ThemeMode::from(appearance) != mode,
        ThemeModeSetting::Light | ThemeModeSetting::Dark => false,
    }
}

/// Re-applies the saved selection, dropping any preview.
pub fn revert_preview(window: &Window, cx: &mut App) {
    let theme = cx
        .global::<GlobalSettings>()
        .0
        .read(cx)
        .settings()
        .theme
        .clone();
    catalog::apply_selection(cx, &theme, window.appearance());
}

#[derive(Clone, Copy)]
enum Step {
    Up,
    Down,
}

struct ThemeSelector {
    filter: Entity<InputState>,
    rows: Vec<ThemeRow>,
    highlighted: Option<usize>,
    _filter_subscription: Subscription,
}

impl ThemeSelector {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx));
        let subscription = cx.subscribe(&filter, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.refilter(cx);
            }
        });
        let mut selector = Self {
            filter,
            rows: Vec::new(),
            highlighted: None,
            _filter_subscription: subscription,
        };
        selector.refilter(cx);
        let current = cx.theme().theme_name().clone();
        selector.highlighted = selector.rows.iter().position(|row| row.name == current);
        selector
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let kept = self
            .highlighted
            .and_then(|ix| self.rows.get(ix))
            .map(|row| row.name.clone());
        let text = self.filter.read(cx).value();
        self.rows = filtered_rows(ThemeCatalog::global(cx).entries(), &text);
        self.highlighted = kept.and_then(|name| self.rows.iter().position(|row| row.name == name));
        cx.notify();
    }

    fn step(&mut self, step: Step, cx: &mut Context<Self>) {
        let Some(last) = self.rows.len().checked_sub(1) else {
            return;
        };
        let next = match (self.highlighted, step) {
            (None, Step::Down) | (Some(0), Step::Up) => 0,
            (None, Step::Up) => last,
            (Some(ix), Step::Up) => ix - 1,
            (Some(ix), Step::Down) => (ix + 1).min(last),
        };
        self.highlighted = Some(next);
        let entry = self
            .rows
            .get(next)
            .and_then(|row| ThemeCatalog::global(cx).find(&row.name).cloned());
        if let Some(entry) = entry {
            catalog::preview(cx, &entry);
        }
        cx.notify();
    }

    fn confirm(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(index).cloned() else {
            return;
        };
        let store = cx.global::<GlobalSettings>().0.clone();
        let appearance = window.appearance();
        let mut later = false;
        let result = store.update(cx, |store, cx| {
            store.update(cx, |settings| {
                later = applies_later(settings.theme.mode, appearance, row.mode);
                settings.theme = confirmed_theme(&settings.theme, &row.name, row.mode);
            })
        });
        revert_preview(window, cx);
        match result {
            Ok(()) if later => notify(
                window,
                cx,
                Notice {
                    level: NoticeLevel::Info,
                    message: format!(
                        "\"{}\" will apply when the system switches to its {} appearance.",
                        row.name,
                        if row.mode.is_dark() { "dark" } else { "light" }
                    )
                    .into(),
                },
            ),
            Ok(()) => {}
            Err(error) => notify(
                window,
                cx,
                Notice {
                    level: NoticeLevel::Error,
                    message: error.to_string().into(),
                },
            ),
        }
        window.close_dialog(cx);
    }
}

impl Render for ThemeSelector {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (accent, muted) = (theme.accent, theme.muted_foreground);
        let mut list = div()
            .id("theme-selector-list")
            .test_support()
            .flex()
            .flex_col()
            .max_h(px(320.))
            .overflow_y_scroll();
        let mut previous: Option<ThemeMode> = None;
        for (index, row) in self.rows.iter().enumerate() {
            if previous != Some(row.mode) {
                previous = Some(row.mode);
                let label = if row.mode.is_dark() { "Dark" } else { "Light" };
                list = list.child(div().px_3().py_1().text_xs().text_color(muted).child(label));
            }
            let id = SharedString::from(format!("theme-selector-item:{}", row.name));
            list = list.child(
                div()
                    .id(id)
                    .test_support()
                    .px_3()
                    .py_1()
                    .cursor_pointer()
                    .when(self.highlighted == Some(index), |this| this.bg(accent))
                    .child(row.name.clone())
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.confirm(index, window, cx);
                    })),
            );
        }
        let errors = ThemeCatalog::global(cx).errors();
        let footer = (!errors.is_empty()).then(|| {
            div()
                .id("theme-selector-errors")
                .test_support()
                .flex()
                .flex_col()
                .gap_1()
                .pt_2()
                .text_xs()
                .text_color(muted)
                .children(errors.iter().enumerate().map(|(index, error)| {
                    let file = error.file.file_name().map_or_else(
                        || error.file.display().to_string(),
                        |name| name.to_string_lossy().into_owned(),
                    );
                    div()
                        .id(("theme-selector-error", index))
                        .test_support()
                        .child(format!("{file}: {}", error.message))
                }))
        });
        div()
            .key_context("List")
            .flex()
            .flex_col()
            .gap_2()
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.step(Step::Up, cx)))
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.step(Step::Down, cx)))
            .on_action(cx.listener(|this, _: &Confirm, window, cx| {
                if let Some(index) = this.highlighted {
                    this.confirm(index, window, cx);
                }
            }))
            .child(Input::new(&self.filter).id("theme-selector-filter"))
            .child(list)
            .children(footer)
    }
}

/// Opens the theme selector dialog.
pub fn open(window: &mut Window, cx: &mut App) {
    let selector = cx.new(|cx| ThemeSelector::new(window, cx));
    let focus_pending = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if focus_pending.replace(false) {
            selector.read(cx).filter.focus_handle(cx).focus(window, cx);
        }
        dialog
            .title("Theme")
            .width(px(480.))
            .child(selector.clone())
            .on_close(|_, window, cx| revert_preview(window, cx))
    });
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::time::Duration;

    use gpui_kit::component::{ActiveTheme as _, ThemeMode};
    use gpui_kit::prelude::*;
    use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
    use gpui_kit::{AnyWindowHandle, Context, TestAppContext, Window, WindowOptions, div};

    use super::{ThemeRow, applies_later, confirmed_theme, filtered_rows, open};
    use crate::paths::ConfigPaths;
    use crate::prompts::{NoticeLevel, ScriptedPrompts, install_scripted};
    use crate::settings::{GlobalSettings, Settings, ThemeModeSetting, ThemeSettings};
    use crate::themes::catalog::{self, ThemeCatalog};

    const KIT: &str = include_str!("fixtures/kit-sample.json");
    const ZED: &str = include_str!("fixtures/zed-sample.json");
    const BROKEN: &str = include_str!("fixtures/zed-broken.json");

    struct Blank;

    impl Render for Blank {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full()
        }
    }

    struct Env {
        _dir: tempfile::TempDir,
        prompts: Rc<ScriptedPrompts>,
        window: AnyWindowHandle,
        paths: ConfigPaths,
    }

    fn start(cx: &mut TestAppContext, files: &[(&str, &str)], saved: &ThemeSettings) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::at(dir.path().to_path_buf());
        std::fs::create_dir_all(paths.themes_dir()).unwrap();
        for (name, text) in files {
            std::fs::write(paths.themes_dir().join(name), text).unwrap();
        }
        let settings = Settings {
            theme: saved.clone(),
            ..Settings::default()
        };
        std::fs::write(
            paths.settings_file(),
            serde_json::to_string(&settings).unwrap(),
        )
        .unwrap();
        let prompts = cx.update(|cx| {
            gpui_kit::init(cx);
            crate::settings::init(cx, &paths);
            catalog::init(cx, &paths);
            let prompts = install_scripted(cx);
            catalog::apply_selection(cx, saved, gpui_kit::WindowAppearance::Light);
            prompts
        });
        let (window, _) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| cx.new(|_| Blank)).unwrap()
        });
        Env {
            _dir: dir,
            prompts,
            window,
            paths,
        }
    }

    fn theme(mode: ThemeModeSetting) -> ThemeSettings {
        ThemeSettings {
            mode,
            light: "Default Light".to_owned(),
            dark: "Default Dark".to_owned(),
        }
    }

    fn open_selector(cx: &mut TestAppContext, env: &Env) {
        cx.update_window(env.window, |_, window, cx| {
            open(window, cx);
            window.render_frame(cx);
        })
        .unwrap();
    }

    fn visible_name(cx: &mut TestAppContext) -> String {
        cx.update(|cx| cx.theme().theme_name().to_string())
    }

    fn saved(cx: &mut TestAppContext) -> Settings {
        cx.update(|cx| cx.global::<GlobalSettings>().0.read(cx).settings().clone())
    }

    fn press(cx: &mut TestAppContext, env: &Env, key: &str) {
        cx.update_window(env.window, |_, window, cx| window.press(key, cx))
            .unwrap();
        cx.run_until_parked();
    }

    fn row_visible(cx: &mut TestAppContext, env: &Env, name: &str) -> bool {
        let id = format!("theme-selector-item:{name}");
        cx.update_window(env.window, |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        })
        .unwrap()
    }

    #[gpui_kit::test]
    fn rows_group_light_then_dark_and_filter_case_insensitively(cx: &mut TestAppContext) {
        // Given: a catalog with built-ins and the Kit and Zed fixtures.
        let env = start(
            cx,
            &[("a.json", KIT), ("b.json", ZED)],
            &theme(ThemeModeSetting::Light),
        );
        let entries = cx.update(|cx| ThemeCatalog::global(cx).entries().to_vec());
        drop(env);
        // When: listing rows with and without a filter.
        let all = filtered_rows(&entries, "");
        let narrowed = filtered_rows(&entries, "NIGHT");
        // Then: light rows precede dark rows and the filter keeps matching names only.
        let modes: Vec<bool> = all.iter().map(|row| row.mode.is_dark()).collect();
        assert!(modes.windows(2).all(|pair| pair[0] <= pair[1]));
        let names: Vec<&str> = narrowed.iter().map(|row| row.name.as_ref()).collect();
        assert_eq!(names, ["Kit Night", "Sample Night"]);
        assert!(
            narrowed
                .iter()
                .all(|row: &ThemeRow| row.mode == ThemeMode::Dark)
        );
    }

    #[test]
    fn confirm_rules_follow_the_mode_setting() {
        // Given: saved settings in each mode.
        let system = theme(ThemeModeSetting::System);
        let light = theme(ThemeModeSetting::Light);
        // When: confirming a dark theme.
        let from_system = confirmed_theme(&system, "Night", ThemeMode::Dark);
        let from_light = confirmed_theme(&light, "Night", ThemeMode::Dark);
        // Then: the dark slot is set; a static mode follows the theme, System stays.
        assert_eq!(from_system.dark, "Night");
        assert_eq!(from_system.mode, ThemeModeSetting::System);
        assert_eq!(from_light.dark, "Night");
        assert_eq!(from_light.mode, ThemeModeSetting::Dark);
        assert!(applies_later(
            ThemeModeSetting::System,
            gpui_kit::WindowAppearance::Light,
            ThemeMode::Dark
        ));
        assert!(!applies_later(
            ThemeModeSetting::Light,
            gpui_kit::WindowAppearance::Light,
            ThemeMode::Dark
        ));
    }

    #[gpui_kit::test]
    fn filter_narrows_rows(cx: &mut TestAppContext) {
        // Given: an open selector listing every theme.
        let env = start(
            cx,
            &[("a.json", KIT), ("b.json", ZED)],
            &theme(ThemeModeSetting::Light),
        );
        open_selector(cx, &env);
        assert!(row_visible(cx, &env, "Sample Day"));
        // When: typing a filter in the input.
        cx.update_window(env.window, |_, window, cx| {
            window.within("dialog").click("theme-selector-filter", cx);
            window.within("dialog").input("kit", cx);
        })
        .unwrap();
        cx.run_until_parked();
        // Then: only the matching row remains.
        assert!(row_visible(cx, &env, "Kit Night"));
        assert!(!row_visible(cx, &env, "Sample Day"));
    }

    #[gpui_kit::test]
    fn down_previews_without_saving(cx: &mut TestAppContext) {
        // Given: an open selector on the saved Default Light theme.
        let env = start(cx, &[("b.json", ZED)], &theme(ThemeModeSetting::Light));
        let before = saved(cx);
        open_selector(cx, &env);
        // When: moving the highlight down.
        press(cx, &env, "down");
        // Then: another theme is applied while the settings stay unchanged.
        assert_ne!(visible_name(cx), "Default Light");
        assert_eq!(saved(cx), before);
    }

    #[gpui_kit::test]
    fn enter_saves_to_the_slot_and_switches_a_static_mode(cx: &mut TestAppContext) {
        // Given: static Light mode and an open selector.
        let env = start(cx, &[("b.json", ZED)], &theme(ThemeModeSetting::Light));
        open_selector(cx, &env);
        // When: filtering to a dark theme and confirming it with Enter.
        cx.update_window(env.window, |_, window, cx| {
            window.within("dialog").click("theme-selector-filter", cx);
            window.within("dialog").input("sample night", cx);
        })
        .unwrap();
        cx.run_until_parked();
        press(cx, &env, "down");
        press(cx, &env, "enter");
        // Then: the dark slot and mode are saved, the theme is visible and the file is written.
        let after = saved(cx);
        assert_eq!(after.theme.dark, "Sample Night");
        assert_eq!(after.theme.mode, ThemeModeSetting::Dark);
        assert_eq!(visible_name(cx), "Sample Night");
        let text = std::fs::read_to_string(env.paths.settings_file()).unwrap();
        assert!(text.contains("Sample Night"));
    }

    #[gpui_kit::test]
    fn escape_reverts_preview(cx: &mut TestAppContext) {
        // Given: an open selector with a previewed theme.
        let env = start(cx, &[("b.json", ZED)], &theme(ThemeModeSetting::Light));
        open_selector(cx, &env);
        press(cx, &env, "down");
        assert_ne!(visible_name(cx), "Default Light");
        // When: pressing Escape.
        press(cx, &env, "escape");
        // Then: the original theme is restored and nothing was saved.
        assert_eq!(visible_name(cx), "Default Light");
        assert_eq!(saved(cx).theme, theme(ThemeModeSetting::Light));
    }

    #[gpui_kit::test]
    fn system_mode_with_other_appearance_defers_with_one_notice(cx: &mut TestAppContext) {
        // Given: System mode under a light appearance and an open selector.
        let env = start(cx, &[("b.json", ZED)], &theme(ThemeModeSetting::System));
        open_selector(cx, &env);
        // When: confirming a dark theme.
        cx.update_window(env.window, |_, window, cx| {
            window.within("dialog").click("theme-selector-filter", cx);
            window.within("dialog").input("sample night", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(env.window, |_, window, cx| {
            window
                .within("dialog")
                .click("theme-selector-item:Sample Night", cx);
        })
        .unwrap();
        cx.run_until_parked();
        // Then: theme.dark is saved, the light theme stays visible and one Info notice is shown.
        let after = saved(cx);
        assert_eq!(after.theme.dark, "Sample Night");
        assert_eq!(after.theme.mode, ThemeModeSetting::System);
        assert_eq!(visible_name(cx), "Default Light");
        let notices = env.prompts.notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].level, NoticeLevel::Info);
    }

    #[gpui_kit::test]
    async fn load_errors_are_listed(cx: &mut TestAppContext) {
        // Given: a themes directory with a broken Zed family file.
        let env = start(cx, &[("c.json", BROKEN)], &theme(ThemeModeSetting::Light));
        let expected = cx.update(|cx| ThemeCatalog::global(cx).errors().len());
        assert_ne!(expected, 0);
        // When: opening the selector.
        open_selector(cx, &env);
        cx.wait_for(env.window, Duration::from_secs(1), |window, _| {
            window.try_find("theme-selector-errors").is_some()
        })
        .await;
        // Then: every load error has a row in the footer.
        cx.update_window(env.window, |_, window, _| {
            for index in 0..expected {
                assert!(window.try_find(("theme-selector-error", index)).is_some());
            }
        })
        .unwrap();
    }
}
