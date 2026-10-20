//! Application bootstrap: global setup, key bindings, menus and the main window.

use crate::paths::ConfigPaths;
use crate::prompts::{ConfirmChoice, ConfirmRequest, Notice, NoticeLevel};
use crate::settings::{GlobalSettings, LoadNotice, SettingsEvent, WindowMode};
use crate::workspace::tabs::GlobalTabStore;
use crate::{
    catalog, db, dialogs, paths, prompts, query_tab, secrets, sessions, settings, sidebar,
    sql_dialects, table_tab, themes, windows, workspace,
};
use gpui_kit::{App, Global, KeyBinding, Menu, MenuItem, Task, WindowAppearance, actions};
use std::sync::Arc;

// allow: SIZE_OK - fixed four-file integration scope includes bootstrap and adjacent headless scenarios.

/// Human-readable application name, used for the window title and the app menu.
const APP_NAME: &str = "Colonnade";

/// Reverse-DNS identifier shared by the macOS bundle, the Wayland `app_id`
/// and the Linux desktop entry.
pub(crate) const APP_ID: &str = "ovh.dissidence.colonnade";

actions!(
    colonnade,
    [
        Quit,
        NewConnection,
        NewFolder,
        NewQuery,
        OpenSettings,
        SelectTheme,
        UseSingleWindow,
        UseWindowPerConnection
    ]
);

#[derive(Default)]
struct StartupNotices(Vec<Notice>);
impl Global for StartupNotices {}

/// Result of the cancellable, app-initiated quit flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitOutcome {
    Quit,
    Cancelled,
}

/// Configures the application and opens the main window.
pub fn run(cx: &mut App) {
    gpui_kit::init(cx);
    let paths = match paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("Colonnade could not resolve its config directory: {error}");
            cx.quit();
            return;
        }
    };
    if let Err(error) = paths.ensure_dirs() {
        eprintln!("Colonnade could not create its config directories: {error}");
    }
    init_settings_and_themes(cx, &paths);
    if let Err(error) = themes::watch::start(cx, paths.themes_dir(), |cx| {
        let notices = apply_theme(cx, cx.window_appearance());
        for notice in notices {
            notify(cx, notice);
        }
    }) {
        cx.global_mut::<StartupNotices>()
            .0
            .push(warning(error.to_string()));
    }
    match db::runtime::DbRuntime::tokio() {
        Ok(runtime) => cx.set_global(runtime),
        Err(error) => {
            eprintln!("Colonnade could not start its database runtime: {error}");
            cx.quit();
            return;
        }
    }
    db::factory::install(cx);
    secrets::install(cx, Arc::new(secrets::KeyringSecretStore));
    prompts::install_default(cx);
    dialogs::password::install(cx);
    dialogs::confirm::install(cx);
    init_workspace(cx, &paths);
    match windows::open_main_window(cx) {
        Ok(_) => cx.activate(true),
        Err(error) => {
            eprintln!("Colonnade could not open its main window: {error:#}");
            cx.quit();
        }
    }
}

fn init_settings_and_themes(cx: &mut App, paths: &ConfigPaths) {
    settings::init(cx, paths);
    if let Err(error) = sql_dialects::register_dialects() {
        eprintln!("Colonnade could not register SQL dialects: {error}");
    }
    themes::catalog::init(cx, paths);
    let notices = apply_theme(cx, cx.window_appearance());
    cx.set_global(StartupNotices(notices));
}

fn init_workspace(cx: &mut App, paths: &ConfigPaths) {
    catalog::store::init(cx, paths);
    sessions::init(cx);
    let mode = cx
        .global::<GlobalSettings>()
        .0
        .read(cx)
        .settings()
        .window_mode;
    workspace::tabs::init(cx, mode);
    workspace::view_factory::init(cx);
    table_tab::view::init(cx);
    query_tab::view::init(cx);
    sidebar::view::init(cx);
    workspace::tab_strip::init(cx);
    windows::init(cx);
    let connections = cx
        .global::<catalog::store::GlobalConnectionStore>()
        .0
        .clone();
    cx.subscribe(
        &connections,
        |_, event: &catalog::store::ConnectionStoreEvent, cx| match event {
            catalog::store::ConnectionStoreEvent::ConnectionRemoved(id) => {
                let tabs = cx.global::<GlobalTabStore>().0.clone();
                tabs.update(cx, |s, cx| s.close_connection_tabs(*id, cx));
                windows::connection_removed(*id, cx);
            }
            catalog::store::ConnectionStoreEvent::Changed
            | catalog::store::ConnectionStoreEvent::ConnectionAdded(_)
            | catalog::store::ConnectionStoreEvent::ConnectionUpdated { .. }
            | catalog::store::ConnectionStoreEvent::ColorChanged(_) => {}
        },
    )
    .detach();
    let settings = cx.global::<GlobalSettings>().0.clone();
    cx.subscribe(&settings, |settings, event: &SettingsEvent, cx| {
        let SettingsEvent::Changed { theme, window_mode } = event;
        if *theme {
            for notice in apply_theme(cx, cx.window_appearance()) {
                notify(cx, notice);
            }
        }
        if *window_mode {
            let mode = settings.read(cx).settings().window_mode;
            if let Err(error) = windows::apply_window_mode(mode, cx) {
                notify(cx, warning(error.to_string()));
            }
        }
    })
    .detach();
    let sessions = cx.global::<sessions::GlobalSessions>().0.clone();
    cx.subscribe(
        &sessions,
        |_, event: &sessions::SessionEvent, cx| match event {
            sessions::SessionEvent::Notice(notice) => notify(cx, notice.clone()),
            sessions::SessionEvent::StatusChanged(_)
            | sessions::SessionEvent::ChildrenChanged(..) => {}
        },
    )
    .detach();
    cx.on_app_quit(|cx| {
        db::runtime::shutdown(cx);
        std::future::ready(())
    })
    .detach();
    cx.on_window_closed(|cx, id| {
        cx.global_mut::<windows::WindowRegistry>().forget(id);
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
    init_actions(cx);
}

fn init_actions(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| {
        request_quit(cx).detach();
    });
    cx.on_action(|_: &UseSingleWindow, cx| set_window_mode(cx, WindowMode::Single));
    cx.on_action(|_: &UseWindowPerConnection, cx| set_window_mode(cx, WindowMode::PerConnection));
    cx.bind_keys([
        KeyBinding::new("secondary-q", Quit, None),
        KeyBinding::new("secondary-t", NewQuery, Some("Workspace")),
        KeyBinding::new("secondary-,", OpenSettings, None),
        KeyBinding::new("secondary-k secondary-t", SelectTheme, None),
    ]);
    cx.set_menus([
        Menu::new(APP_NAME).items([
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::action("Select Theme…", SelectTheme),
            MenuItem::separator(),
            MenuItem::action("Quit Colonnade", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("New Connection…", NewConnection),
            MenuItem::action("New Folder…", NewFolder),
            MenuItem::action("New Query", NewQuery),
            MenuItem::action("Close Tab", workspace::tab_strip::CloseActiveTab),
        ]),
        Menu::new("View").items([
            MenuItem::action("Single Window", UseSingleWindow),
            MenuItem::action("Window per Connection", UseWindowPerConnection),
        ]),
    ]);
}

fn set_window_mode(cx: &mut App, mode: WindowMode) {
    let settings = cx.global::<GlobalSettings>().0.clone();
    if let Err(error) = settings.update(cx, |s, cx| s.update(cx, |s| s.window_mode = mode)) {
        notify(cx, warning(error.to_string()));
    }
}

pub(crate) fn warning(message: String) -> Notice {
    Notice {
        level: NoticeLevel::Warning,
        message: message.into(),
    }
}

pub(crate) fn notify(cx: &mut App, notice: Notice) {
    if let Some(target) = windows::notice_target(cx) {
        // The target can disappear before a deferred notification is delivered.
        drop(target.update(cx, |_, window, cx| prompts::notify(window, cx, notice)));
    }
}

pub(crate) fn apply_theme(cx: &mut App, appearance: WindowAppearance) -> Vec<Notice> {
    let settings = cx
        .global::<GlobalSettings>()
        .0
        .read(cx)
        .settings()
        .theme
        .clone();
    let mut notices: Vec<_> = themes::catalog::apply_selection(cx, &settings, appearance)
        .into_iter()
        .map(|name| warning(format!("Theme {name} is unavailable; using the default")))
        .collect();
    notices.extend(
        themes::catalog::ThemeCatalog::global(cx)
            .errors()
            .iter()
            .map(|e| warning(format!("{}: {}", e.file.display(), e.message))),
    );
    notices
}

pub(crate) fn show_load_notices(window: &mut gpui_kit::Window, cx: &mut App) {
    let mut notices = std::mem::take(&mut cx.global_mut::<StartupNotices>().0);
    let settings = cx.global::<GlobalSettings>().0.clone();
    let connections = cx
        .global::<catalog::store::GlobalConnectionStore>()
        .0
        .clone();
    for (file, notice) in [
        (
            "settings.json",
            settings.update(cx, |s, _| s.take_load_notice()),
        ),
        (
            "connections.json",
            connections.update(cx, |s, _| s.take_load_notice()),
        ),
    ] {
        if let Some(notice) = notice {
            notices.push(warning(match notice {
                LoadNotice::Corrupt { backup } => {
                    format!("{file} was corrupt; backed up to {}", backup.display())
                }
                LoadNotice::NewerVersion => {
                    format!("{file} was written by a newer Colonnade version and is read-only")
                }
            }));
        }
    }
    for notice in notices {
        prompts::notify(window, cx, notice);
    }
}

/// Confirms all unsaved/running work before closing tabs and quitting.
pub fn request_quit(cx: &mut App) -> Task<QuitOutcome> {
    let store = cx.global::<GlobalTabStore>().0.clone();
    let tabs: Vec<_> = store.read(cx).tabs().iter().map(|t| t.id).collect();
    let reasons = store.read(cx).close_reasons(&tabs, cx);
    let answer = if reasons.is_empty() {
        None
    } else {
        let Some(target) = windows::notice_target(cx) else {
            return Task::ready(QuitOutcome::Cancelled);
        };
        match target.update(cx, |_, w, cx| {
            prompts::confirm(w, cx, discard_request("Quit Colonnade", reasons.len()))
        }) {
            Ok(answer) => Some(answer),
            Err(_) => return Task::ready(QuitOutcome::Cancelled),
        }
    };
    cx.spawn(async move |cx| {
        if let Some(answer) = answer
            && !matches!(answer.await, Ok(ConfirmChoice::Confirm))
        {
            return QuitOutcome::Cancelled;
        }
        cx.update(|cx| {
            store.update(cx, |s, cx| s.close(&tabs, cx));
            cx.quit();
        });
        QuitOutcome::Quit
    })
}

pub(crate) fn discard_request(title: &str, count: usize) -> ConfirmRequest {
    ConfirmRequest {
        title: title.to_owned().into(),
        message: format!("Closing discards unsaved or running work ({count} reasons). Continue?")
            .into(),
        confirm_label: "Discard and close".into(),
        destructive: true,
        alternatives: Vec::new(),
    }
}

/// Isolated dependency handles for headless app-shell tests.
#[cfg(test)]
pub struct TestServices {
    pub factory: db::fake::FakeFactory,
    pub secrets: Arc<secrets::MemorySecretStore>,
    pub prompts: std::rc::Rc<prompts::ScriptedPrompts>,
}

/// Initializes the production wiring without OS/network services.
///
/// # Panics
/// Panics when the supplied isolated config directory cannot be created.
#[cfg(test)]
pub fn init_for_tests(cx: &mut App, root: &std::path::Path) -> TestServices {
    gpui_kit::init(cx);
    cx.set_reduce_motion(true);
    let paths = ConfigPaths::at(root.to_path_buf());
    paths.ensure_dirs().expect("isolated config directories");
    init_settings_and_themes(cx, &paths);
    cx.set_global(db::runtime::DbRuntime::inline());
    let factory = db::fake::FakeFactory::new();
    db::backend::install_factory(cx, Arc::new(factory.clone()));
    let secrets = Arc::new(secrets::MemorySecretStore::default());
    secrets::install(cx, secrets.clone());
    prompts::install_default(cx);
    dialogs::password::install(cx);
    dialogs::confirm::install(cx);
    let prompts = prompts::install_scripted(cx);
    init_workspace(cx, &paths);
    TestServices {
        factory,
        secrets,
        prompts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{
        model::{ConnectionId, ConnectionProfile, Driver, Endpoint, PasswordStorage},
        ops::{DropPosition, NodeRef},
        store::GlobalConnectionStore,
    };
    use crate::db::{
        fake::{FakeEndpoint, FakeServer},
        model::{Cell, ColumnInfo, Dialect, ObjectKind, StatementResult, TlsMode},
    };
    use crate::prompts::{ConfirmChoice, NoticeLevel};
    use crate::query_tab::state::QueryTabs;
    use crate::settings::{GlobalSettings, WindowMode};
    use crate::sidebar::node::{ObjectNodeKey, SidebarNodeKey};
    use crate::windows;
    use crate::workspace::{
        status_bar::status_model,
        tabs::{GlobalTabStore, TabId, WindowKey},
    };
    use gpui_kit::component::ActiveTheme as _;
    use gpui_kit::prelude::*;
    use gpui_kit::test::TestAppContextExt as _;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyWindowHandle, App, Context, IntoElement, Render, TestAppContext, VisualTestContext,
        Window, WindowAppearance, div, point, px,
    };
    use std::sync::Arc;
    use std::time::Duration;

    struct Fx {
        _dir: tempfile::TempDir,
        services: TestServices,
        window: AnyWindowHandle,
    }

    fn setup(cx: &mut TestAppContext) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let services = cx.update(|cx| init_for_tests(cx, dir.path()));
        let window = cx.update(windows::open_main_window).unwrap();
        Fx {
            _dir: dir,
            services,
            window,
        }
    }

    fn ui<R>(cx: &mut TestAppContext, fx: &Fx, f: impl FnOnce(&mut Window, &mut App) -> R) -> R {
        cx.update_window(fx.window, |_, window, cx| {
            window.render_frame(cx);
            let result = f(window, cx);
            window.render_frame(cx);
            result
        })
        .unwrap()
    }

    fn add(cx: &mut TestAppContext) -> ConnectionId {
        cx.update(|cx| {
            let store = cx.global::<GlobalConnectionStore>().0.clone();
            store
                .update(cx, |s, cx| {
                    s.add_connection(
                        cx,
                        ConnectionProfile::new(
                            "Fixture",
                            Driver::Postgres,
                            Endpoint::Server {
                                host: "db.test".into(),
                                port: 5432,
                                user: "ada".into(),
                                database: None,
                                tls: TlsMode::default(),
                            },
                            PasswordStorage::None,
                        ),
                        DropPosition::RootEnd,
                    )
                })
                .unwrap()
        })
    }

    fn dirty(cx: &mut TestAppContext) -> TabId {
        let connection = add(cx);
        let tab = cx.update(|cx| {
            let store = cx.global::<GlobalTabStore>().0.clone();
            store.update(cx, |s, cx| s.open_query(connection, None, cx))
        });
        cx.update(|cx| {
            QueryTabs::state(cx, tab)
                .unwrap()
                .update(cx, |s, _| s.text = "SELECT 1".into());
        });
        tab
    }

    fn tab_count(cx: &mut TestAppContext) -> usize {
        cx.update(|cx| cx.global::<GlobalTabStore>().0.read(cx).tabs().len())
    }

    #[gpui_kit::test]
    fn main_window_shows_workspace(cx: &mut TestAppContext) {
        // Given: initialized isolated services.
        let fx = setup(cx);
        // When: rendering the main window.
        ui(cx, &fx, |window, _| {
            // Then: all three workspace regions are reachable.
            assert!(window.find("sidebar-empty").visible());
            assert!(window.find("tab-strip").visible());
            assert!(window.find("workspace-empty").visible());
        });
    }

    #[gpui_kit::test]
    async fn connection_table_and_query_end_to_end(cx: &mut TestAppContext) {
        // Given: the assembled workspace backed by a scripted PostgreSQL server.
        let fx = setup(cx);
        let server = Arc::new(FakeServer::sample(Dialect::Postgres));
        server.script_query(
            "SELECT 1".into(),
            Ok(vec![StatementResult::Rows {
                columns: vec![ColumnInfo {
                    name: "one".into(),
                    type_name: "int4".into(),
                    numeric: true,
                }],
                rows: vec![vec![Cell::text("1".into())]],
                truncated: false,
            }]),
        );
        fx.services
            .factory
            .register(FakeEndpoint::Host("db.test".into()), server.clone());
        // When: creating a connection, browsing its table, then running SQL through the UI.
        cx.dispatch_action(fx.window, NewConnection);
        crate::dialogs::confirm::harness::dialog_open(cx, fx.window).await;
        for (id, text) in [
            ("connection-name", "Fixture"),
            ("connection-host", "db.test"),
            ("connection-user", "ada"),
            ("connection-password", "fixture"),
        ] {
            ui(cx, &fx, |w, cx| {
                w.click(id, cx);
                w.press("end", cx);
                w.input(text, cx);
            });
        }
        ui(cx, &fx, |w, cx| w.click("dialog-save", cx));
        crate::dialogs::confirm::harness::dialog_closed(cx, fx.window).await;
        let connection = cx.update(|cx| {
            crate::catalog::ops::connections(
                cx.global::<GlobalConnectionStore>().0.read(cx).items(),
            )[0]
            .id
        });
        let objects = [
            (ObjectKind::Database, None, "app"),
            (ObjectKind::Schema, Some("app"), "public"),
        ]
        .map(|(kind, database, name)| SidebarNodeKey::Object {
            connection,
            node: ObjectNodeKey {
                kind,
                database: database.map(str::to_owned),
                schema: None,
                name: name.into(),
            },
        });
        let keys = std::iter::once(SidebarNodeKey::Connection(connection)).chain(objects);
        for key in keys {
            let toggle = format!("sidebar-toggle:{key}");
            cx.wait_for(fx.window, Duration::from_secs(2), |w, _| {
                w.try_find(toggle.clone()).is_some()
            })
            .await;
            ui(cx, &fx, |w, cx| {
                w.click(format!("sidebar-toggle:{key}"), cx);
            });
            cx.run_until_parked();
        }
        let users = SidebarNodeKey::Object {
            connection,
            node: ObjectNodeKey {
                kind: ObjectKind::Table,
                database: Some("app".into()),
                schema: Some("public".into()),
                name: "users".into(),
            },
        };
        cx.wait_for(fx.window, Duration::from_secs(2), |w, _| {
            w.try_find(format!("sidebar-row:{users}")).is_some()
        })
        .await;
        ui(cx, &fx, |w, cx| {
            w.double_click(format!("sidebar-row:{users}"), cx);
        });
        cx.wait_for(fx.window, Duration::from_secs(2), |w, _| {
            w.try_find(("row", 0usize)).is_some_and(|row| row.visible())
        })
        .await;
        ui(cx, &fx, |w, cx| w.press("secondary-t", cx));
        cx.run_until_parked();
        ui(cx, &fx, |w, cx| {
            w.click_at("workspace-content", point(px(40.), px(80.)), cx);
            w.input("SELECT 1", cx);
        });
        ui(cx, &fx, |w, cx| w.click("query-run", cx));
        // Then: the scripted query was executed and its result is shown.
        cx.wait_for(fx.window, Duration::from_secs(2), |w, _| {
            w.try_find("query-result-grid").is_some_and(|e| e.visible())
        })
        .await;
        assert_eq!(server.executed_sql(), vec!["SELECT 1"]);
    }

    #[gpui_kit::test]
    fn status_reports_active_connection(cx: &mut TestAppContext) {
        // Given: an active query tab on a disconnected connection.
        let _fx = setup(cx);
        dirty(cx);
        // When: projecting the status model.
        let model = cx.update(|cx| status_model(WindowKey::Main, cx));
        // Then: it identifies the active connection and its disconnected state.
        let (name, _, status) = model.connection.unwrap();
        assert_eq!(name.as_ref(), "Fixture");
        assert_eq!(status.as_ref(), "Disconnected");
    }

    #[gpui_kit::test]
    fn removing_connection_closes_tabs(cx: &mut TestAppContext) {
        // Given: a connection with an open query.
        let _fx = setup(cx);
        let tab = dirty(cx);
        // When: the catalog removes the connection.
        cx.update(|cx| {
            let id = cx
                .global::<GlobalTabStore>()
                .0
                .read(cx)
                .tab(tab)
                .unwrap()
                .connection;
            let store = cx.global::<GlobalConnectionStore>().0.clone();
            store
                .update(cx, |s, cx| s.remove(cx, NodeRef::Connection(id)))
                .unwrap();
        });
        // Then: its tabs are closed by the application wiring.
        assert_eq!(tab_count(cx), 0);
    }

    #[gpui_kit::test]
    async fn quit_cancel_preserves_dirty_tabs(cx: &mut TestAppContext) {
        // Given: unsaved SQL and a cancelled confirmation.
        let fx = setup(cx);
        dirty(cx);
        fx.services.prompts.queue_confirm(ConfirmChoice::Cancel);
        // When: requesting Quit.
        let outcome = cx.update(request_quit).await;
        // Then: nothing is discarded.
        assert_eq!(outcome, QuitOutcome::Cancelled);
        assert_eq!(tab_count(cx), 1);
        assert_eq!(fx.services.prompts.confirm_requests().len(), 1);
    }

    #[gpui_kit::test]
    async fn quit_confirm_closes_tabs_and_shutdown_is_idempotent(cx: &mut TestAppContext) {
        // Given: unsaved SQL and an accepted confirmation.
        let fx = setup(cx);
        dirty(cx);
        fx.services.prompts.queue_confirm(ConfirmChoice::Confirm);
        // When: requesting Quit.
        let outcome = cx.update(request_quit).await;
        // Then: all tabs close and repeated shutdown remains safe.
        assert_eq!(outcome, QuitOutcome::Quit);
        assert_eq!(tab_count(cx), 0);
        cx.update(|cx| {
            crate::db::runtime::shutdown(cx);
            crate::db::runtime::shutdown(cx);
        });
    }

    #[gpui_kit::test]
    fn system_theme_follows_dark_appearance(cx: &mut TestAppContext) {
        // Given: the default System selection.
        let _fx = setup(cx);
        // When: applying a dark platform appearance.
        cx.update(|cx| {
            apply_theme(cx, WindowAppearance::Dark);
        });
        // Then: Kit uses the dark theme.
        cx.update(|cx| assert!(cx.theme().is_dark()));
    }

    #[gpui_kit::test]
    fn corrupt_catalog_warns_once_after_window_opens(cx: &mut TestAppContext) {
        // Given: an invalid catalog on disk.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("connections.json"), b"{ broken").unwrap();
        let services = cx.update(|cx| init_for_tests(cx, dir.path()));
        assert_eq!(services.prompts.notices().len(), 0);
        // When: opening the main window twice (the second call reuses it).
        cx.update(windows::open_main_window).unwrap();
        cx.update(windows::open_main_window).unwrap();
        // Then: exactly one warning is delivered and the original bytes are backed up.
        assert_eq!(
            services
                .prompts
                .notices()
                .iter()
                .filter(|n| n.level == NoticeLevel::Warning)
                .count(),
            1
        );
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("connections.corrupt-")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(backups[0].path()).unwrap(), b"{ broken");
    }

    #[gpui_kit::test]
    fn window_close_cancel_preserves_window_and_tabs(cx: &mut TestAppContext) {
        // Given: dirty SQL and a cancelled close.
        let fx = setup(cx);
        dirty(cx);
        fx.services.prompts.queue_confirm(ConfirmChoice::Cancel);
        // When: asking to close the workspace.
        ui(cx, &fx, windows::request_window_close);
        cx.run_until_parked();
        // Then: both the window and its work survive.
        assert_eq!(cx.windows().len(), 1);
        assert_eq!(tab_count(cx), 1);
    }

    #[gpui_kit::test]
    fn window_close_confirm_closes_window_and_tabs(cx: &mut TestAppContext) {
        // Given: dirty SQL and an accepted close.
        let fx = setup(cx);
        dirty(cx);
        fx.services.prompts.queue_confirm(ConfirmChoice::Confirm);
        // When: asking to close the workspace.
        cx.update_window(fx.window, |_, w, cx| windows::request_window_close(w, cx))
            .unwrap();
        cx.run_until_parked();
        // Then: the window and its tabs are gone.
        assert_eq!(cx.windows().len(), 0);
        assert_eq!(tab_count(cx), 0);
    }

    #[gpui_kit::test]
    fn platform_close_is_synchronous_and_guarded(cx: &mut TestAppContext) {
        // Given: dirty SQL and a cancelled confirmation.
        let fx = setup(cx);
        dirty(cx);
        // When: invoking the registered platform callback.
        let closed = VisualTestContext::from_window(fx.window, cx).simulate_close();
        cx.run_until_parked();
        // Then: the synchronous answer refused closing while work existed.
        assert!(!closed);
        assert_eq!(cx.windows().len(), 1);
        assert_eq!(fx.services.prompts.confirm_requests().len(), 1);
    }

    #[gpui_kit::test]
    fn repeated_close_starts_only_one_confirmation(cx: &mut TestAppContext) {
        // Given: dirty SQL, before the spawned close task has run.
        let fx = setup(cx);
        dirty(cx);
        // When: two close requests arrive in one update.
        ui(cx, &fx, |w, cx| {
            windows::request_window_close(w, cx);
            windows::request_window_close(w, cx);
        });
        cx.run_until_parked();
        // Then: only one prompt was opened.
        assert_eq!(fx.services.prompts.confirm_requests().len(), 1);
    }

    #[gpui_kit::test]
    fn new_query_without_connection_notifies(cx: &mut TestAppContext) {
        // Given: an empty workspace.
        let fx = setup(cx);
        // When: New Query is dispatched.
        cx.dispatch_action(fx.window, NewQuery);
        // Then: no orphan tab is made and an informational notice explains why.
        assert_eq!(tab_count(cx), 0);
        assert_eq!(fx.services.prompts.notices()[0].level, NoticeLevel::Info);
    }

    #[gpui_kit::test]
    fn settings_mode_updates_tab_routing(cx: &mut TestAppContext) {
        // Given: a workspace with one tab.
        let _fx = setup(cx);
        dirty(cx);
        // When: changing the saved mode.
        cx.update(|cx| {
            let settings = cx.global::<GlobalSettings>().0.clone();
            settings
                .update(cx, |s, cx| {
                    s.update(cx, |s| s.window_mode = WindowMode::PerConnection)
                })
                .unwrap();
        });
        // Then: routing changes and the connection receives its own window.
        cx.update(|cx| {
            assert_eq!(
                cx.global::<GlobalTabStore>().0.read(cx).mode(),
                WindowMode::PerConnection
            );
        });
        assert_eq!(cx.windows().len(), 2);
    }

    #[cfg(target_os = "linux")]
    #[gpui_kit::test]
    fn title_bar_close_uses_confirmation(cx: &mut TestAppContext) {
        // Given: a Linux title bar and dirty SQL.
        let fx = setup(cx);
        dirty(cx);
        // When: clicking the client-side close control.
        ui(cx, &fx, |w, cx| w.click("close", cx));
        cx.run_until_parked();
        // Then: cancelling its confirmation preserves the window.
        assert_eq!(fx.services.prompts.confirm_requests().len(), 1);
        assert_eq!(cx.windows().len(), 1);
    }

    #[gpui_kit::test]
    fn views_are_rebuilt_after_leaving_window_visibility(cx: &mut TestAppContext) {
        // Given: a rendered query view with app-level text.
        let fx = setup(cx);
        let built = std::rc::Rc::new(std::cell::Cell::new(0));
        let count = built.clone();
        cx.update(|cx| {
            crate::workspace::view_factory::register(
                cx,
                crate::workspace::view_factory::TabKindTag::Query,
                move |_, _, cx| {
                    count.set(count.get() + 1);
                    cx.new(|_| Probe).into()
                },
            );
        });
        let tab = dirty(cx);
        ui(cx, &fx, |_, _| {});
        assert_eq!(built.get(), 1);
        // When: routing the tab away and then back into the main window.
        for mode in [WindowMode::PerConnection, WindowMode::Single] {
            cx.update(|cx| {
                let store = cx.global::<GlobalTabStore>().0.clone();
                store.update(cx, |s, cx| s.set_mode(mode, cx));
            });
            ui(cx, &fx, |_, _| {});
        }
        // Then: a fresh window-bound view is built without losing app-level SQL.
        assert_eq!(built.get(), 2);
        cx.update(|cx| assert_eq!(QueryTabs::state(cx, tab).unwrap().read(cx).text, "SELECT 1"));
    }

    struct Probe;
    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }
}
