//! Settings dialog and theme selector.
//!
//! The dialog edits the appearance mode, the light and dark themes and the
//! window mode. Every change goes through [`SettingsStore::update`]; theme
//! changes are applied right away with [`catalog::apply_selection`].

use gpui_kit::component::button::Button;
use gpui_kit::component::radio::Radio;
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IndexPath, Selectable as _, ThemeMode, WindowExt as _,
};
use gpui_kit::prelude::*;
use gpui_kit::{
    App, Context, Entity, SharedString, Subscription, TestSupportExt as _, Window, div, px,
};

use crate::paths;
use crate::prompts::{Notice, NoticeLevel, notify};
use crate::settings::{
    GlobalSettings, Settings, SettingsEvent, SettingsStore, ThemeModeSetting, WindowMode,
};
use crate::themes::catalog::{self, ThemeCatalog, ThemeEntry};
use crate::themes::selector;

type ThemeSelect = Entity<SelectState<SearchableVec<SharedString>>>;

/// Names of the catalog themes of `mode`, in catalog order.
#[must_use]
pub fn theme_names(entries: &[ThemeEntry], mode: ThemeMode) -> Vec<SharedString> {
    entries
        .iter()
        .filter(|entry| entry.mode == mode)
        .map(|entry| entry.name.clone())
        .collect()
}

struct SettingsView {
    store: Entity<SettingsStore>,
    light: ThemeSelect,
    dark: ThemeSelect,
    _subscriptions: Vec<Subscription>,
}

fn theme_select(
    mode: ThemeMode,
    selected: &str,
    window: &mut Window,
    cx: &mut Context<SettingsView>,
) -> ThemeSelect {
    let names = theme_names(ThemeCatalog::global(cx).entries(), mode);
    let index = names.iter().position(|name| name.as_ref() == selected);
    cx.new(|cx| {
        SelectState::new(
            SearchableVec::new(names),
            index.map(IndexPath::new),
            window,
            cx,
        )
    })
}

impl SettingsView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = cx.global::<GlobalSettings>().0.clone();
        let theme = store.read(cx).settings().theme.clone();
        let light = theme_select(ThemeMode::Light, &theme.light, window, cx);
        let dark = theme_select(ThemeMode::Dark, &theme.dark, window, cx);
        let subscriptions = vec![
            cx.subscribe_in(&light, window, |this, _, event, window, cx| {
                let SelectEvent::Confirm(name) = event;
                if let Some(name) = name.clone() {
                    this.apply(window, cx, move |settings| {
                        settings.theme.light = name.to_string();
                    });
                }
            }),
            cx.subscribe_in(&dark, window, |this, _, event, window, cx| {
                let SelectEvent::Confirm(name) = event;
                if let Some(name) = name.clone() {
                    this.apply(window, cx, move |settings| {
                        settings.theme.dark = name.to_string();
                    });
                }
            }),
            cx.subscribe_in(&store, window, |this, _, _: &SettingsEvent, window, cx| {
                this.sync_selects(window, cx);
            }),
        ];
        Self {
            store,
            light,
            dark,
            _subscriptions: subscriptions,
        }
    }

    fn sync_selects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let theme = self.store.read(cx).settings().theme.clone();
        for (select, mode, selected) in [
            (&self.light, ThemeMode::Light, theme.light),
            (&self.dark, ThemeMode::Dark, theme.dark),
        ] {
            let names = theme_names(ThemeCatalog::global(cx).entries(), mode);
            let index = names.iter().position(|name| name.as_ref() == selected);
            select.update(cx, |state, cx| {
                state.set_selected_index(index.map(IndexPath::new), window, cx);
            });
        }
        cx.notify();
    }

    fn apply(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut Settings),
    ) {
        let result = self.store.update(cx, |store, cx| store.update(cx, change));
        match result {
            Ok(()) => {
                let theme = self.store.read(cx).settings().theme.clone();
                catalog::apply_selection(cx, &theme, window.appearance());
            }
            Err(error) => notify(
                window,
                cx,
                Notice {
                    level: NoticeLevel::Error,
                    message: error.to_string().into(),
                },
            ),
        }
        cx.notify();
    }

    fn mode_button(
        (id, label, mode): (&'static str, &'static str, ThemeModeSetting),
        current: ThemeModeSetting,
        read_only: bool,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(id)
            .label(label)
            .selected(current == mode)
            .disabled(read_only)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.apply(window, cx, |settings| settings.theme.mode = mode);
            }))
    }

    fn window_radio(
        (id, label, mode): (&'static str, &'static str, WindowMode),
        current: WindowMode,
        read_only: bool,
        cx: &mut Context<Self>,
    ) -> Radio {
        Radio::new(id)
            .label(label)
            .checked(current == mode)
            .disabled(read_only)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.apply(window, cx, |settings| settings.window_mode = mode);
            }))
    }
}

fn open_themes_folder(window: &mut Window, cx: &mut App) {
    let opened = paths::resolve()
        .map_err(|error| error.to_string())
        .and_then(|paths| {
            paths.ensure_dirs().map_err(|error| error.to_string())?;
            cx.reveal_path(&paths.themes_dir());
            Ok(())
        });
    if let Err(message) = opened {
        notify(
            window,
            cx,
            Notice {
                level: NoticeLevel::Error,
                message: message.into(),
            },
        );
    }
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let read_only = store.is_read_only();
        let settings = store.settings().clone();
        let muted = cx.theme().muted_foreground;
        let modes = [
            ("settings-mode-system", "System", ThemeModeSetting::System),
            ("settings-mode-light", "Light", ThemeModeSetting::Light),
            ("settings-mode-dark", "Dark", ThemeModeSetting::Dark),
        ];
        let windows = [
            (
                "settings-window-single",
                "Single window",
                WindowMode::Single,
            ),
            (
                "settings-window-per-connection",
                "One window per connection",
                WindowMode::PerConnection,
            ),
        ];
        let mode_row = div().flex().gap_1().children(
            modes.map(|item| Self::mode_button(item, settings.theme.mode, read_only, cx)),
        );
        let window_row = div().flex().flex_col().gap_1().children(
            windows.map(|item| Self::window_radio(item, settings.window_mode, read_only, cx)),
        );
        div()
            .flex()
            .flex_col()
            .gap_3()
            .when(read_only, |this| {
                this.child(
                    div()
                        .id("settings-read-only")
                        .test_support()
                        .text_sm()
                        .text_color(muted)
                        .child(
                            "The settings file comes from a newer version of Colonnade and is read-only.",
                        ),
                )
            })
            .child(mode_row)
            .child(
                Select::new(&self.light)
                    .id("settings-light-theme")
                    .title_prefix("Light theme: ")
                    .disabled(read_only),
            )
            .child(
                Select::new(&self.dark)
                    .id("settings-dark-theme")
                    .title_prefix("Dark theme: ")
                    .disabled(read_only),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("settings-browse-themes")
                            .label("Browse themes…")
                            .disabled(read_only)
                            .on_click(|_, window, cx| selector::open(window, cx)),
                    )
                    .child(
                        Button::new("settings-open-themes-folder")
                            .label("Open themes folder")
                            .on_click(|_, window, cx| open_themes_folder(window, cx)),
                    ),
            )
            .child(window_row)
    }
}

/// Opens the settings dialog.
pub fn open(window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| SettingsView::new(window, cx));
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.title("Settings").width(px(480.)).child(view.clone())
    });
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::{ActiveTheme as _, ThemeMode};
    use gpui_kit::prelude::*;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, Context, TestAppContext, Window, WindowOptions, div};

    use super::{open, theme_names};
    use crate::paths::ConfigPaths;
    use crate::prompts::install_scripted;
    use crate::settings::{GlobalSettings, Settings, ThemeModeSetting, WindowMode};
    use crate::themes::catalog::{self, ThemeCatalog};

    const KIT: &str = include_str!("themes/fixtures/kit-sample.json");
    const ZED: &str = include_str!("themes/fixtures/zed-sample.json");

    struct Blank;

    impl Render for Blank {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full()
        }
    }

    struct Env {
        _dir: tempfile::TempDir,
        window: AnyWindowHandle,
        paths: ConfigPaths,
    }

    fn start(cx: &mut TestAppContext, settings_file: &str) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::at(dir.path().to_path_buf());
        std::fs::create_dir_all(paths.themes_dir()).unwrap();
        std::fs::write(paths.themes_dir().join("a.json"), KIT).unwrap();
        std::fs::write(paths.themes_dir().join("b.json"), ZED).unwrap();
        std::fs::write(paths.settings_file(), settings_file).unwrap();
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
            crate::settings::init(cx, &paths);
            catalog::init(cx, &paths);
            install_scripted(cx);
        });
        let (window, _) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| cx.new(|_| Blank)).unwrap()
        });
        cx.update_window(window, |_, window, cx| {
            open(window, cx);
            window.render_frame(cx);
        })
        .unwrap();
        Env {
            _dir: dir,
            window,
            paths,
        }
    }

    fn click(cx: &mut TestAppContext, env: &Env, id: &str) {
        cx.update_window(env.window, |_, window, cx| {
            window.within("dialog").click(id.to_owned(), cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    fn saved(cx: &mut TestAppContext) -> Settings {
        cx.update(|cx| cx.global::<GlobalSettings>().0.read(cx).settings().clone())
    }

    #[gpui_kit::test]
    fn choosing_dark_mode_updates_the_file_and_the_theme(cx: &mut TestAppContext) {
        // Given: the settings dialog with System mode under a light appearance.
        let env = start(cx, "{}");
        assert!(!cx.update(|cx| cx.theme().is_dark()));
        // When: choosing the Dark mode.
        click(cx, &env, "settings-mode-dark");
        // Then: the file records it and the dark theme is active.
        assert_eq!(saved(cx).theme.mode, ThemeModeSetting::Dark);
        let text = std::fs::read_to_string(env.paths.settings_file()).unwrap();
        assert!(text.contains("dark"));
        assert!(cx.update(|cx| cx.theme().is_dark()));
    }

    #[gpui_kit::test]
    fn selects_list_only_themes_of_their_mode(cx: &mut TestAppContext) {
        // Given: a catalog with built-in, Kit and Zed themes.
        let env = start(cx, "{}");
        let entries = cx.update(|cx| ThemeCatalog::global(cx).entries().to_vec());
        // When: building the option lists of both selects.
        let light = theme_names(&entries, ThemeMode::Light);
        let dark = theme_names(&entries, ThemeMode::Dark);
        // Then: each lists only its mode and together they cover the catalog.
        assert_eq!(light, ["Default Light", "Sample Day"]);
        assert_eq!(dark, ["Default Dark", "Kit Night", "Sample Night"]);
        assert!(
            cx.update_window(env.window, |_, window, _| {
                window.try_find("settings-light-theme").is_some()
                    && window.try_find("settings-dark-theme").is_some()
            })
            .unwrap()
        );
    }

    #[gpui_kit::test]
    fn per_connection_window_mode_persists(cx: &mut TestAppContext) {
        // Given: the settings dialog with the default single window mode.
        let env = start(cx, "{}");
        // When: selecting one window per connection.
        click(cx, &env, "settings-window-per-connection");
        // Then: the setting and the file hold per_connection.
        assert_eq!(saved(cx).window_mode, WindowMode::PerConnection);
        let text = std::fs::read_to_string(env.paths.settings_file()).unwrap();
        assert!(text.contains("per_connection"));
    }

    #[gpui_kit::test]
    fn read_only_settings_disable_the_controls(cx: &mut TestAppContext) {
        // Given: a settings file from a newer version.
        let env = start(cx, r#"{"version": 99}"#);
        let before = saved(cx);
        // When: trying to change the mode and the window mode.
        click(cx, &env, "settings-mode-dark");
        click(cx, &env, "settings-window-per-connection");
        // Then: an explanation is shown and nothing changed.
        assert_eq!(saved(cx), before);
        assert!(
            cx.update_window(env.window, |_, window, _| {
                window.try_find("settings-read-only").is_some()
            })
            .unwrap()
        );
    }
}
