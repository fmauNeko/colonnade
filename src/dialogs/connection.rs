// allow: SIZE_OK — the plan fixes the whole connection form, its commit flows and their tests in this single file.
//! Connection dialog: create and edit connections.

use std::cell::Cell;
use std::fmt::Write as _;
use std::path::PathBuf;

use futures::FutureExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::searchable_list::SearchableListItem;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{ActiveTheme as _, IndexPath, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, Context, Entity, IntoElement, PathPromptOptions, Render, SharedString,
    Subscription, TestSupportExt as _, Window, div, px,
};

use crate::catalog::model::{
    ConnectionId, ConnectionProfile, Driver, Endpoint, Field, HexColor, PasswordStorage,
    ValidationIssue,
};
use crate::catalog::ops::{DropPosition, NodeRef};
use crate::catalog::store::{ConnectionStore, GlobalConnectionStore};
use crate::catalog::url::parse_connection_url;
use crate::color::{Rgb, connection_presets};
use crate::db::backend::Backends;
use crate::db::factory::test_connection;
use crate::db::model::{Secret, TlsMode};
use crate::db::runtime::spawn_db;
use crate::prompts::{ConfirmChoice, ConfirmRequest, Notice, NoticeLevel, confirm, notify};
use crate::secrets::{Availability, Secrets};
use crate::workspace::tabs::{CloseReason, GlobalTabStore, TabId};

fn store(cx: &App) -> Entity<ConnectionStore> {
    cx.global::<GlobalConnectionStore>().0.clone()
}

fn tell(level: NoticeLevel, message: impl Into<SharedString>, window: &mut Window, cx: &mut App) {
    notify(
        window,
        cx,
        Notice {
            level,
            message: message.into(),
        },
    );
}

/// Whether closing the connection's tabs would cancel a running query, and
/// how many of its tabs hold unsaved text.
fn tab_reasons(id: ConnectionId, cx: &App) -> (usize, usize) {
    let Some(tabs) = cx.try_global::<GlobalTabStore>() else {
        return (0, 0);
    };
    let tabs = tabs.0.read(cx);
    let ids: Vec<TabId> = tabs
        .tabs()
        .iter()
        .filter(|tab| tab.connection == id)
        .map(|tab| tab.id)
        .collect();
    let reasons = tabs.close_reasons(&ids, cx);
    let count =
        |wanted: fn(&CloseReason) -> bool| reasons.iter().filter(|(_, r)| wanted(r)).count();
    (
        count(|r| matches!(r, CloseReason::UnsavedQueryText)),
        count(|r| matches!(r, CloseReason::RunningQuery)),
    )
}

/// Background keychain work for one connection. Every operation runs off the
/// UI thread and reports failures as notices.
#[derive(Clone, Copy)]
struct Keychain(ConnectionId);

impl Keychain {
    fn store(self, secret: Secret, window: &mut Window, cx: &mut App) {
        let secrets = cx.global::<Secrets>().0.clone();
        let task = cx.background_spawn(async move { secrets.set(self.0, &secret) });
        window
            .spawn(cx, async move |cx| {
                if let Err(error) = task.await {
                    let message = format!("Could not save the password to the keychain: {error}");
                    drop(cx.update(|window, cx| self.fall_back_to_ask(&message, window, cx)));
                }
            })
            .detach();
    }

    fn delete(self, window: &mut Window, cx: &mut App) {
        let secrets = cx.global::<Secrets>().0.clone();
        let task = cx.background_spawn(async move { secrets.delete(self.0) });
        window
            .spawn(cx, async move |cx| {
                if let Err(error) = task.await {
                    let message = format!("Could not remove the keychain entry: {error}");
                    drop(cx.update(|window, cx| tell(NoticeLevel::Warning, message, window, cx)));
                }
            })
            .detach();
    }

    /// Copies the stored secret to `copy`; the copy asks for its password when that fails.
    fn copy_to(self, copy: ConnectionId, window: &mut Window, cx: &mut App) {
        let secrets = cx.global::<Secrets>().0.clone();
        let task = cx.background_spawn(async move {
            match secrets.get(self.0)? {
                Some(secret) => secrets.set(copy, &secret),
                None => Ok(()),
            }
        });
        window
            .spawn(cx, async move |cx| {
                if let Err(error) = task.await {
                    let message = format!("Could not copy the keychain password: {error}");
                    drop(cx.update(|window, cx| {
                        Keychain(copy).fall_back_to_ask(&message, window, cx);
                    }));
                }
            })
            .detach();
    }

    /// Switches the profile to "Ask every time" and warns the user.
    fn fall_back_to_ask(self, message: &str, window: &mut Window, cx: &mut App) {
        let Some(mut profile) = store(cx).read(cx).profile(self.0).cloned() else {
            return;
        };
        profile.password = PasswordStorage::Ask;
        drop(store(cx).update(cx, |store, cx| store.update_connection(cx, profile)));
        tell(
            NoticeLevel::Warning,
            format!("{message}. The connection will ask for its password."),
            window,
            cx,
        );
    }
}

/// An option of a [`Select`], optionally greyed out.
#[derive(Clone)]
struct Choice<T> {
    label: &'static str,
    value: T,
    disabled: bool,
}

impl<T: Clone + PartialEq> SearchableListItem for Choice<T> {
    type Value = T;

    fn title(&self) -> SharedString {
        self.label.into()
    }

    fn value(&self) -> &T {
        &self.value
    }

    fn disabled(&self) -> bool {
        self.disabled
    }
}

type ChoiceSelect<T> = Entity<SelectState<Vec<Choice<T>>>>;

fn choice<T>(label: &'static str, value: T) -> Choice<T> {
    Choice {
        label,
        value,
        disabled: false,
    }
}

fn driver_choices() -> Vec<Choice<Driver>> {
    [Driver::Postgres, Driver::MySql, Driver::Sqlite]
        .map(|driver| choice(driver.display_name(), driver))
        .to_vec()
}

fn tls_choices() -> Vec<Choice<TlsMode>> {
    vec![
        choice("Disable", TlsMode::Disable),
        choice("Prefer", TlsMode::Prefer),
        choice("Require", TlsMode::Require),
        choice("Verify CA", TlsMode::VerifyCa),
        choice("Verify full", TlsMode::VerifyFull),
    ]
}

/// The storage options; the keychain is greyed out when it is unavailable.
fn storage_choices(keychain: &Availability) -> Vec<Choice<PasswordStorage>> {
    vec![
        Choice {
            label: "Keychain",
            value: PasswordStorage::Keychain,
            disabled: matches!(keychain, Availability::Unavailable(_)),
        },
        choice("Ask every time", PasswordStorage::Ask),
        choice("None", PasswordStorage::None),
    ]
}

fn select_of<T: Clone + PartialEq + 'static>(
    choices: Vec<Choice<T>>,
    current: &T,
    window: &mut Window,
    cx: &mut App,
) -> ChoiceSelect<T> {
    let row = choices.iter().position(|c| c.value == *current);
    cx.new(|cx| SelectState::new(choices, row.map(IndexPath::new), window, cx))
}

/// Result of "Test connection".
#[derive(Clone, Debug, PartialEq, Eq)]
enum TestState {
    Idle,
    Testing,
    Ok(String),
    Err(String),
}

enum Mode {
    New(DropPosition),
    Edit(Box<ConnectionProfile>),
}

/// How a validated form is committed.
enum Target {
    New(DropPosition),
    Edit { previous: PasswordStorage },
}

struct Submission {
    profile: ConnectionProfile,
    password: Option<Secret>,
    target: Target,
}

/// Writes the profile to the store, then the password to the keychain in the background.
fn commit(submission: Submission, window: &mut Window, cx: &mut App) {
    let Submission {
        profile,
        password,
        target,
    } = submission;
    let id = profile.id;
    let storage = profile.password;
    let saved = store(cx).update(cx, |store, cx| match &target {
        Target::New(position) => store.add_connection(cx, profile, *position).map(drop),
        Target::Edit { .. } => store.update_connection(cx, profile),
    });
    if let Err(error) = saved {
        tell(NoticeLevel::Error, error.to_string(), window, cx);
        return;
    }
    let previous = match target {
        Target::New(_) => PasswordStorage::None,
        Target::Edit { previous } => previous,
    };
    match (storage, password) {
        (PasswordStorage::Keychain, Some(secret)) => Keychain(id).store(secret, window, cx),
        (PasswordStorage::Keychain, None) => {}
        (PasswordStorage::Ask | PasswordStorage::None, _) => {
            if previous == PasswordStorage::Keychain {
                Keychain(id).delete(window, cx);
            }
        }
    }
}

/// Asks before saving a change that disconnects a connection with running queries.
fn confirm_disconnect(submission: Submission, window: &mut Window, cx: &mut App) {
    let answer = confirm(
        window,
        cx,
        ConfirmRequest {
            title: "Save connection".into(),
            message: "Saving disconnects this connection and cancels its running queries.".into(),
            confirm_label: "Save and disconnect".into(),
            destructive: true,
            alternatives: Vec::new(),
        },
    );
    window
        .spawn(cx, async move |cx| {
            if matches!(answer.await, Ok(ConfirmChoice::Confirm)) {
                drop(cx.update(|window, cx| {
                    commit(submission, window, cx);
                    window.close_dialog(cx);
                }));
            }
        })
        .detach();
}

fn set_text(input: &Entity<InputState>, text: String, window: &mut Window, cx: &mut App) {
    input.update(cx, |input, cx| input.set_value(text, window, cx));
}

fn trimmed(input: &Entity<InputState>, cx: &App) -> String {
    input.read(cx).value().trim().to_owned()
}

fn field_id(field: Field) -> &'static str {
    match field {
        Field::Name => "connection-error-name",
        Field::Host => "connection-error-host",
        Field::Port => "connection-error-port",
        Field::User => "connection-error-user",
        Field::Path => "connection-error-path",
        Field::Driver => "connection-error-driver",
    }
}

struct ConnectionForm {
    mode: Mode,
    url: Entity<InputState>,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    database: Entity<InputState>,
    file: Entity<InputState>,
    driver: ChoiceSelect<Driver>,
    tls: ChoiceSelect<TlsMode>,
    storage: ChoiceSelect<PasswordStorage>,
    picker: Entity<ColorPickerState>,
    current_driver: Driver,
    custom_color: Option<HexColor>,
    issues: Vec<ValidationIssue>,
    url_error: Option<String>,
    test: TestState,
    test_generation: u32,
    keychain: Availability,
    _subscriptions: Vec<Subscription>,
}

/// The typed text of every field when the form opens.
struct Initial {
    host: String,
    port: String,
    user: String,
    database: String,
    tls: TlsMode,
    file: String,
}

impl Initial {
    fn of(endpoint: &Endpoint) -> Self {
        match endpoint {
            Endpoint::Server {
                host,
                port,
                user,
                database,
                tls,
            } => Self {
                host: host.clone(),
                port: port.to_string(),
                user: user.clone(),
                database: database.clone().unwrap_or_default(),
                tls: *tls,
                file: String::new(),
            },
            Endpoint::File { path } => Self {
                host: String::new(),
                port: Driver::Postgres
                    .default_port()
                    .unwrap_or_default()
                    .to_string(),
                user: String::new(),
                database: String::new(),
                tls: TlsMode::default(),
                file: path.to_string_lossy().into_owned(),
            },
        }
    }
}

/// The profile the form starts from: the edited one, or a blank PostgreSQL one.
fn starting_profile(mode: &Mode, keychain: &Availability) -> ConnectionProfile {
    match mode {
        Mode::Edit(profile) => (**profile).clone(),
        Mode::New(_) => ConnectionProfile::new(
            "",
            Driver::Postgres,
            Endpoint::Server {
                host: String::new(),
                port: 5432,
                user: String::new(),
                database: None,
                tls: TlsMode::default(),
            },
            if matches!(keychain, Availability::Available) {
                PasswordStorage::Keychain
            } else {
                PasswordStorage::Ask
            },
        ),
    }
}

fn text_input(
    placeholder: &'static str,
    value: String,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .default_value(value)
    })
}

impl ConnectionForm {
    fn new(mode: Mode, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let keychain = cx.global::<Secrets>().0.availability();
        let profile = starting_profile(&mode, &keychain);
        let editing = matches!(mode, Mode::Edit(_));
        let initial = Initial::of(&profile.endpoint);
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(if editing {
                    "Leave empty to keep the stored password"
                } else {
                    "Password"
                })
        });
        let driver = select_of(driver_choices(), &profile.driver, window, cx);
        let picker = cx.new(|cx| {
            ColorPickerState::new(window, cx).default_value(profile.color.effective().to_hsla())
        });
        let subscriptions = vec![
            cx.subscribe_in(
                &driver,
                window,
                |this: &mut Self, _, event: &SelectEvent<Vec<Choice<Driver>>>, window, cx| {
                    let SelectEvent::Confirm(selected) = event;
                    if let Some(driver) = selected {
                        this.driver_changed(*driver, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &picker,
                window,
                |this: &mut Self, _, event: &ColorPickerEvent, _, cx| {
                    let ColorPickerEvent::Change(color) = event;
                    this.custom_color = color.map(|c| HexColor::new(Rgb::from_hsla(c)));
                    cx.notify();
                },
            ),
        ];
        Self {
            url: text_input(
                "postgres://user@host:5432/database",
                String::new(),
                window,
                cx,
            ),
            name: text_input("Name", profile.name.clone(), window, cx),
            host: text_input("Host", initial.host, window, cx),
            port: text_input("Port", initial.port, window, cx),
            user: text_input("User", initial.user, window, cx),
            password,
            database: text_input("Database (optional)", initial.database, window, cx),
            file: text_input("Path to the database file", initial.file, window, cx),
            tls: select_of(tls_choices(), &initial.tls, window, cx),
            storage: select_of(storage_choices(&keychain), &profile.password, window, cx),
            driver,
            picker,
            current_driver: profile.driver,
            custom_color: profile.color.custom,
            issues: Vec::new(),
            url_error: None,
            test: TestState::Idle,
            test_generation: 0,
            keychain,
            _subscriptions: subscriptions,
            mode,
        }
    }

    /// The typed password, untrimmed; `None` when empty.
    fn typed_password(&self, cx: &App) -> Option<Secret> {
        let typed = self.password.read(cx).value();
        (!typed.is_empty()).then(|| Secret::new(typed.to_string()))
    }

    /// The profile the typed values describe, plus the problems parsing them.
    ///
    /// A relative SQLite path is joined to the working directory here.
    fn draft(&self, cx: &App) -> (ConnectionProfile, Vec<ValidationIssue>) {
        let mut issues = Vec::new();
        let endpoint = match self.current_driver {
            Driver::Sqlite => {
                let typed = PathBuf::from(trimmed(&self.file, cx));
                let path = if typed.as_os_str().is_empty() || typed.is_absolute() {
                    typed
                } else {
                    std::env::current_dir().map_or_else(|_| typed.clone(), |cwd| cwd.join(&typed))
                };
                Endpoint::File { path }
            }
            Driver::Postgres | Driver::MySql => {
                let port = trimmed(&self.port, cx).parse::<u16>().unwrap_or_else(|_| {
                    issues.push(ValidationIssue {
                        field: Field::Port,
                        message: "the port must be a number from 1 to 65535".to_owned(),
                    });
                    0
                });
                let database = trimmed(&self.database, cx);
                Endpoint::Server {
                    host: trimmed(&self.host, cx),
                    port,
                    user: trimmed(&self.user, cx),
                    database: (!database.is_empty()).then_some(database),
                    tls: self
                        .tls
                        .read(cx)
                        .selected_value()
                        .copied()
                        .unwrap_or_default(),
                }
            }
        };
        let storage = match self.current_driver {
            Driver::Sqlite => PasswordStorage::None,
            Driver::Postgres | Driver::MySql => self
                .storage
                .read(cx)
                .selected_value()
                .copied()
                .unwrap_or(PasswordStorage::Ask),
        };
        let mut profile = ConnectionProfile::new(
            trimmed(&self.name, cx),
            self.current_driver,
            endpoint,
            storage,
        );
        if let Mode::Edit(original) = &self.mode {
            profile.id = original.id;
            profile.color.generated = original.color.generated;
        }
        profile.color.custom = self.custom_color;
        (profile, issues)
    }

    /// The profile ready to save; every problem otherwise.
    fn build(&self, cx: &App) -> Result<ConnectionProfile, Vec<ValidationIssue>> {
        let (profile, mut issues) = self.draft(cx);
        let port_unparsed = !issues.is_empty();
        if let Err(found) = profile.validate() {
            issues.extend(
                found
                    .into_iter()
                    .filter(|issue| !(port_unparsed && issue.field == Field::Port)),
            );
        }
        if issues.is_empty() {
            Ok(profile)
        } else {
            Err(issues)
        }
    }

    fn set_driver(&mut self, driver: Driver, window: &mut Window, cx: &mut Context<Self>) {
        self.driver.update(cx, |select, cx| {
            select.set_selected_value(&driver, window, cx);
        });
        self.driver_changed(driver, window, cx);
    }

    /// Swaps the field set and moves the port to the new default when it still holds the old one.
    fn driver_changed(&mut self, new: Driver, window: &mut Window, cx: &mut Context<Self>) {
        let old = std::mem::replace(&mut self.current_driver, new);
        let port = trimmed(&self.port, cx);
        let untouched =
            port.is_empty() || old.default_port().is_some_and(|p| port == p.to_string());
        if let (true, Some(default)) = (untouched, new.default_port()) {
            set_text(&self.port, default.to_string(), window, cx);
        }
        cx.notify();
    }

    fn apply_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = std::env::current_dir().unwrap_or_default();
        let typed = self.url.read(cx).value();
        match parse_connection_url(&typed, &cwd) {
            Err(error) => self.url_error = Some(error.to_string()),
            Ok(import) => {
                self.url_error = None;
                self.set_driver(import.driver, window, cx);
                match import.endpoint {
                    Endpoint::Server {
                        host,
                        port,
                        user,
                        database,
                        tls,
                    } => {
                        set_text(&self.host, host, window, cx);
                        set_text(&self.port, port.to_string(), window, cx);
                        set_text(&self.user, user, window, cx);
                        set_text(&self.database, database.unwrap_or_default(), window, cx);
                        self.tls
                            .update(cx, |select, cx| select.set_selected_value(&tls, window, cx));
                    }
                    Endpoint::File { path } => {
                        set_text(&self.file, path.to_string_lossy().into_owned(), window, cx);
                    }
                }
                if trimmed(&self.name, cx).is_empty() {
                    set_text(&self.name, import.suggested_name, window, cx);
                }
                if let Some(secret) = import.password {
                    set_text(&self.password, secret.expose().to_owned(), window, cx);
                }
                set_text(&self.url, String::new(), window, cx);
            }
        }
        cx.notify();
    }

    fn browse(window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await
                && let Some(path) = paths.into_iter().next()
            {
                drop(this.update_in(cx, |this, window, cx| {
                    set_text(&this.file, path.to_string_lossy().into_owned(), window, cx);
                }));
            }
        })
        .detach();
    }

    fn reset_color(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.custom_color = None;
        let generated = self.draft(cx).0.color.generated.rgb().to_hsla();
        self.picker
            .update(cx, |picker, cx| picker.set_value(generated, window, cx));
        cx.notify();
    }

    fn run_test(&mut self, cx: &mut Context<Self>) {
        let profile = match self.build(cx) {
            Ok(profile) => profile,
            Err(issues) => {
                self.issues = issues;
                cx.notify();
                return;
            }
        };
        self.issues.clear();
        self.test_generation += 1;
        let generation = self.test_generation;
        self.test = TestState::Testing;
        cx.notify();
        let typed = self.typed_password(cx);
        let stored = match (&typed, &self.mode, profile.password) {
            (None, Mode::Edit(original), PasswordStorage::Keychain) => {
                let (secrets, id) = (cx.global::<Secrets>().0.clone(), original.id);
                Some(cx.background_spawn(async move { secrets.get(id).ok().flatten() }))
            }
            _ => None,
        };
        let factory = cx.global::<Backends>().0.clone();
        let attempt = async move {
            let password = match (typed, stored) {
                (Some(typed), _) => Some(typed),
                (None, Some(stored)) => stored.await,
                (None, None) => None,
            };
            test_connection(factory, profile.connect_params(password)).await
        };
        let task = spawn_db(cx, attempt.boxed());
        cx.spawn(async move |this, cx| {
            let result = task.await;
            drop(this.update(cx, |this, cx| {
                if this.test_generation == generation {
                    this.test = match result {
                        Ok(version) => TestState::Ok(version),
                        Err(error) => TestState::Err(error.to_string()),
                    };
                    cx.notify();
                }
            }));
        })
        .detach();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let profile = match self.build(cx) {
            Ok(profile) => profile,
            Err(issues) => {
                self.issues = issues;
                cx.notify();
                return;
            }
        };
        self.issues.clear();
        let password = self.typed_password(cx);
        let target = match &self.mode {
            Mode::New(position) => Target::New(*position),
            Mode::Edit(original) => {
                let current = store(cx).read(cx).profile(original.id).cloned();
                let Some(current) = current else {
                    window.close_dialog(cx);
                    return;
                };
                let endpoint_changed = current.driver != profile.driver
                    || current.endpoint != profile.endpoint
                    || current.password != profile.password;
                let (_, running) = tab_reasons(current.id, cx);
                if endpoint_changed && running > 0 {
                    confirm_disconnect(
                        Submission {
                            profile,
                            password,
                            target: Target::Edit {
                                previous: current.password,
                            },
                        },
                        window,
                        cx,
                    );
                    return;
                }
                Target::Edit {
                    previous: current.password,
                }
            }
        };
        commit(
            Submission {
                profile,
                password,
                target,
            },
            window,
            cx,
        );
        window.close_dialog(cx);
    }
}

/// A labelled form row with an optional error line under the control.
fn row(
    label: &'static str,
    control: impl IntoElement,
    error: Option<AnyElement>,
    cx: &App,
) -> AnyElement {
    h_flex()
        .gap_3()
        .items_start()
        .child(
            div()
                .w(px(120.))
                .pt_1()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(v_flex().flex_1().gap_1().child(control).children(error))
        .into_any_element()
}

impl ConnectionForm {
    fn problem(&self, field: Field, cx: &App) -> Option<AnyElement> {
        let issue = self.issues.iter().find(|issue| issue.field == field)?;
        Some(
            div()
                .id(field_id(field))
                .test_support()
                .text_xs()
                .text_color(cx.theme().danger)
                .child(issue.message.clone())
                .into_any_element(),
        )
    }

    fn field(
        &self,
        label: &'static str,
        control: impl IntoElement,
        field: Field,
        cx: &App,
    ) -> AnyElement {
        row(label, control, self.problem(field, cx), cx)
    }

    fn url_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let error = self.url_error.as_ref().map(|message| {
            div()
                .id("connection-url-error")
                .test_support()
                .text_xs()
                .text_color(cx.theme().danger)
                .child(message.clone())
                .into_any_element()
        });
        let control = h_flex()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .child(Input::new(&self.url).id("connection-url")),
            )
            .child(
                Button::new("connection-url-apply")
                    .label("Apply")
                    .on_click(cx.listener(|this, _, window, cx| this.apply_url(window, cx))),
            );
        row("URL", control, error, cx)
    }

    fn storage_row(&self, cx: &App) -> AnyElement {
        let note = matches!(self.keychain, Availability::Unavailable(_)).then(|| {
            div()
                .id("connection-password-storage-note")
                .test_support()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("The keychain is unavailable on this system.")
                .into_any_element()
        });
        row(
            "Password storage",
            Select::new(&self.storage).id("connection-password-storage"),
            note,
            cx,
        )
    }

    fn color_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let swatch = self.draft(cx).0.color.effective().to_hsla();
        let presets = connection_presets().map(Rgb::to_hsla).to_vec();
        let control = h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .id("connection-color-swatch")
                    .test_support()
                    .size_6()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(swatch),
            )
            .child(
                div()
                    .id("connection-color-customize")
                    .test_support()
                    .child(ColorPicker::new(&self.picker).featured_colors(presets)),
            )
            .child(
                Button::new("connection-color-reset")
                    .label("Reset")
                    .on_click(cx.listener(|this, _, window, cx| this.reset_color(window, cx))),
            );
        row("Color", control, None, cx)
    }

    fn test_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let result = match &self.test {
            TestState::Idle => None,
            TestState::Testing => Some((cx.theme().muted_foreground, "Testing…".to_owned())),
            TestState::Ok(version) => Some((cx.theme().success, format!("Connected: {version}"))),
            TestState::Err(message) => Some((cx.theme().danger, message.clone())),
        };
        let control = h_flex()
            .gap_3()
            .items_center()
            .child(
                Button::new("connection-test")
                    .label("Test connection")
                    .on_click(cx.listener(|this, _, _, cx| this.run_test(cx))),
            )
            .children(result.map(|(color, text)| {
                div()
                    .id("connection-test-result")
                    .test_support()
                    .text_sm()
                    .text_color(color)
                    .child(text)
            }));
        row("", control, None, cx)
    }

    fn file_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let control = h_flex()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .child(Input::new(&self.file).id("connection-file")),
            )
            .child(
                Button::new("connection-file-browse")
                    .label("Browse…")
                    .on_click(cx.listener(|_, _, window, cx| Self::browse(window, cx))),
            );
        self.field("File", control, Field::Path, cx)
    }
}

impl Render for ConnectionForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rows = vec![
            self.url_row(cx),
            self.field(
                "Name",
                Input::new(&self.name).id("connection-name"),
                Field::Name,
                cx,
            ),
            self.field(
                "Driver",
                Select::new(&self.driver).id("connection-driver"),
                Field::Driver,
                cx,
            ),
        ];
        match self.current_driver {
            Driver::Sqlite => rows.push(self.file_row(cx)),
            Driver::Postgres | Driver::MySql => {
                rows.extend([
                    self.field(
                        "Host",
                        Input::new(&self.host).id("connection-host"),
                        Field::Host,
                        cx,
                    ),
                    self.field(
                        "Port",
                        div()
                            .w(px(120.))
                            .child(Input::new(&self.port).id("connection-port")),
                        Field::Port,
                        cx,
                    ),
                    self.field(
                        "User",
                        Input::new(&self.user).id("connection-user"),
                        Field::User,
                        cx,
                    ),
                    row(
                        "Password",
                        Input::new(&self.password).id("connection-password"),
                        None,
                        cx,
                    ),
                    self.storage_row(cx),
                    row(
                        "Database",
                        Input::new(&self.database).id("connection-database"),
                        None,
                        cx,
                    ),
                    row("TLS", Select::new(&self.tls).id("connection-tls"), None, cx),
                ]);
            }
        }
        rows.push(self.color_row(cx));
        rows.push(self.test_row(cx));
        v_flex().gap_2().children(rows)
    }
}

/// Opens the form dialog and returns its state (tests drive it directly).
fn open_form(mode: Mode, window: &mut Window, cx: &mut App) -> Entity<ConnectionForm> {
    let title = match mode {
        Mode::New(_) => "New connection",
        Mode::Edit(_) => "Edit connection",
    };
    let form = cx.new(|cx| ConnectionForm::new(mode, window, cx));
    let name = form.read(cx).name.clone();
    let initial_focus = Cell::new(true);
    let body = form.clone();
    window.open_dialog(cx, move |dialog, window, cx| {
        if initial_focus.replace(false) {
            name.update(cx, |input, cx| input.focus(window, cx));
        }
        let save = {
            let form = body.clone();
            Button::new("dialog-save")
                .label("Save")
                .primary()
                .on_click(move |_, window, cx| form.update(cx, |form, cx| form.save(window, cx)))
        };
        let cancel = Button::new("dialog-cancel")
            .label("Cancel")
            .on_click(|_, window, cx| window.close_dialog(cx));
        let confirmed = body.clone();
        dialog
            .title(title)
            .w(px(640.))
            .child(body.clone())
            .footer(DialogFooter::new().child(cancel).child(save))
            .on_ok(move |_, window, cx| {
                confirmed.update(cx, |form, cx| form.save(window, cx));
                false
            })
    });
    form
}

/// Opens the dialog creating a connection at `target`.
pub fn open_new(target: DropPosition, window: &mut Window, cx: &mut App) {
    drop(open_form(Mode::New(target), window, cx));
}

/// Opens the dialog editing connection `id`; does nothing for an unknown id.
pub fn open_edit(id: ConnectionId, window: &mut Window, cx: &mut App) {
    if let Some(profile) = store(cx).read(cx).profile(id).cloned() {
        drop(open_form(Mode::Edit(Box::new(profile)), window, cx));
    }
}

/// Duplicates connection `id` and copies its keychain secret in the background.
pub fn duplicate(id: ConnectionId, window: &mut Window, cx: &mut App) {
    let Some(storage) = store(cx).read(cx).profile(id).map(|p| p.password) else {
        return;
    };
    match store(cx).update(cx, |store, cx| store.duplicate_connection(cx, id)) {
        Ok(copy) => {
            if storage == PasswordStorage::Keychain {
                Keychain(id).copy_to(copy, window, cx);
            }
        }
        Err(error) => tell(NoticeLevel::Error, error.to_string(), window, cx),
    }
}

/// Deletes connection `id` after a confirmation that counts its unsaved and running tabs.
pub fn delete(id: ConnectionId, window: &mut Window, cx: &mut App) {
    let Some(name) = store(cx).read(cx).profile(id).map(|p| p.name.clone()) else {
        return;
    };
    let (unsaved, running) = tab_reasons(id, cx);
    let mut message = format!("Delete connection \"{name}\"?");
    if unsaved > 0 {
        let _ = write!(message, " {unsaved} tab(s) have unsaved query text.");
    }
    if running > 0 {
        let _ = write!(message, " {running} tab(s) have a running query.");
    }
    let answer = confirm(
        window,
        cx,
        ConfirmRequest {
            title: "Delete connection".into(),
            message: message.into(),
            confirm_label: "Delete".into(),
            destructive: true,
            alternatives: Vec::new(),
        },
    );
    window
        .spawn(cx, async move |cx| {
            if matches!(answer.await, Ok(ConfirmChoice::Confirm)) {
                drop(cx.update(|window, cx| {
                    let removed =
                        store(cx).update(cx, |store, cx| store.remove(cx, NodeRef::Connection(id)));
                    if let Err(error) = removed {
                        tell(NoticeLevel::Error, error.to_string(), window, cx);
                    }
                }));
            }
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::sync::Arc;

    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

    use super::super::confirm::harness::{dialog_closed, dialog_open, open_window};
    use super::{ConnectionForm, Mode, TestState, delete, duplicate, open_form, storage_choices};
    use crate::catalog::model::{
        CatalogItem, ConnectionId, ConnectionProfile, Driver, Endpoint, HexColor, PasswordStorage,
    };
    use crate::catalog::ops::DropPosition;
    use crate::catalog::store::{GlobalConnectionStore, init};
    use crate::color::generated_color;
    use crate::db::backend::install_factory;
    use crate::db::fake::{FakeEndpoint, FakeFactory, FakeServer};
    use crate::db::model::{DbError, Dialect, Secret, TlsMode};
    use crate::db::runtime::DbRuntime;
    use crate::paths::ConfigPaths;
    use crate::prompts::{ConfirmChoice, NoticeLevel, ScriptedPrompts, install_scripted};
    use crate::secrets::{self, Availability, MemorySecretStore};
    use crate::settings::WindowMode;
    use crate::workspace::tabs::{self, CloseReason, GlobalTabStore};
    use crate::workspace::view_factory::TabKindTag;

    struct Env {
        handle: AnyWindowHandle,
        secrets: Arc<MemorySecretStore>,
        factory: FakeFactory,
        prompts: Rc<ScriptedPrompts>,
        dir: tempfile::TempDir,
    }

    fn setup(cx: &mut TestAppContext) -> Env {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = open_window(cx);
        let secret_store = Arc::new(MemorySecretStore::default());
        let factory = FakeFactory::new();
        factory.register(
            FakeEndpoint::Host("db.test".into()),
            Arc::new(FakeServer::sample(Dialect::Postgres)),
        );
        let paths = ConfigPaths::at(dir.path().to_path_buf());
        let prompts = cx.update(|cx| {
            init(cx, &paths);
            secrets::install(cx, secret_store.clone());
            cx.set_global(DbRuntime::inline());
            install_factory(cx, Arc::new(factory.clone()));
            tabs::init(cx, WindowMode::Single);
            install_scripted(cx)
        });
        Env {
            handle,
            secrets: secret_store,
            factory,
            prompts,
            dir,
        }
    }

    fn profile(name: &str, storage: PasswordStorage) -> ConnectionProfile {
        ConnectionProfile::new(
            name,
            Driver::Postgres,
            Endpoint::Server {
                host: "db.test".into(),
                port: 5432,
                user: "ada".into(),
                database: None,
                tls: TlsMode::default(),
            },
            storage,
        )
    }

    fn add(cx: &mut TestAppContext, profile: ConnectionProfile) -> ConnectionId {
        let id = profile.id;
        cx.update(|cx| {
            super::store(cx).update(cx, |store, cx| {
                store.add_connection(cx, profile, DropPosition::RootEnd)
            })
        })
        .expect("add");
        id
    }

    fn profiles(cx: &mut TestAppContext) -> Vec<ConnectionProfile> {
        cx.update(|cx| {
            let store = cx.global::<GlobalConnectionStore>().0.read(cx);
            store
                .items()
                .iter()
                .filter_map(|item| match item {
                    CatalogItem::Connection(profile) => Some(profile.clone()),
                    CatalogItem::Folder(_) => None,
                })
                .collect()
        })
    }

    async fn open_form_of(
        cx: &mut TestAppContext,
        env: &Env,
        mode: Mode,
    ) -> Entity<ConnectionForm> {
        let form = cx
            .update_window(env.handle, |_, window, cx| open_form(mode, window, cx))
            .expect("window");
        dialog_open(cx, env.handle).await;
        form
    }

    async fn open_blank(cx: &mut TestAppContext, env: &Env) -> Entity<ConnectionForm> {
        open_form_of(cx, env, Mode::New(DropPosition::RootEnd)).await
    }

    fn type_in(cx: &mut TestAppContext, env: &Env, id: &'static str, text: &str) {
        cx.update_window(env.handle, |_, window, cx| {
            window.within("dialog").click(id, cx);
            window.press("end", cx);
            window.within("dialog").input(text, cx);
        })
        .expect("window");
    }

    fn click(cx: &mut TestAppContext, env: &Env, id: &'static str) {
        cx.update_window(env.handle, |_, window, cx| {
            window.within("dialog").click(id, cx);
        })
        .expect("window");
        cx.run_until_parked();
    }

    fn present(cx: &mut TestAppContext, env: &Env, id: &'static str) -> bool {
        cx.update_window(env.handle, |_, window, cx| {
            window.render_frame(cx);
            window.within("dialog").try_find(id).is_some()
        })
        .expect("window")
    }

    fn value_of(cx: &mut TestAppContext, env: &Env, id: &'static str) -> Option<String> {
        cx.update_window(env.handle, |_, window, cx| {
            window.render_frame(cx);
            window
                .within("dialog")
                .try_find(id)
                .and_then(|element| element.value().map(str::to_owned))
        })
        .expect("window")
    }

    fn fill_basics(cx: &mut TestAppContext, env: &Env) {
        type_in(cx, env, "connection-name", "Prod");
        type_in(cx, env, "connection-host", "db.test");
        type_in(cx, env, "connection-user", "ada");
    }

    fn stored_password(env: &Env, id: ConnectionId) -> Option<Secret> {
        use crate::secrets::SecretStore as _;
        env.secrets.get(id).expect("memory store")
    }

    #[gpui_kit::test]
    async fn form_creates_a_postgres_connection_with_the_generated_color(cx: &mut TestAppContext) {
        // Given: the new-connection form with the keychain available.
        let env = setup(cx);
        open_blank(cx, &env).await;
        // When: typing a name, host and user, then saving.
        fill_basics(cx, &env);
        click(cx, &env, "dialog-save");
        // Then: the connection is stored with the seed-generated color, and the dialog closes.
        let saved = profiles(cx);
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].name, "Prod");
        assert_eq!(saved[0].driver, Driver::Postgres);
        assert_eq!(saved[0].password, PasswordStorage::Keychain);
        assert!(matches!(
            &saved[0].endpoint,
            Endpoint::Server { host, port: 5432, user, .. } if host == "db.test" && user == "ada"
        ));
        assert_eq!(
            saved[0].color.generated,
            HexColor::new(generated_color("postgres://db.test:5432"))
        );
        assert_eq!(saved[0].color.custom, None);
        dialog_closed(cx, env.handle).await;
    }

    #[gpui_kit::test]
    async fn password_never_reaches_connections_json(cx: &mut TestAppContext) {
        // Given: the form filled in with a password and Keychain storage.
        let env = setup(cx);
        open_blank(cx, &env).await;
        fill_basics(cx, &env);
        type_in(cx, &env, "connection-password", "s3cret-pass");
        // When: saving.
        click(cx, &env, "dialog-save");
        // Then: the secret store holds the password and connections.json does not.
        let saved = profiles(cx);
        assert_eq!(saved[0].password, PasswordStorage::Keychain);
        assert_eq!(
            stored_password(&env, saved[0].id),
            Some(Secret::new("s3cret-pass".into()))
        );
        let file = std::fs::read_to_string(env.dir.path().join("connections.json")).expect("saved");
        assert!(!file.contains("s3cret-pass"));
    }

    #[gpui_kit::test]
    async fn keychain_write_failure_switches_to_ask_with_a_warning(cx: &mut TestAppContext) {
        // Given: a failing secret store and a filled-in form.
        let env = setup(cx);
        env.secrets.fail_with("locked");
        open_blank(cx, &env).await;
        fill_basics(cx, &env);
        type_in(cx, &env, "connection-password", "pw");
        // When: saving.
        click(cx, &env, "dialog-save");
        // Then: the profile asks for its password and a warning was shown.
        assert_eq!(profiles(cx)[0].password, PasswordStorage::Ask);
        assert!(
            env.prompts
                .notices()
                .iter()
                .any(|notice| notice.level == NoticeLevel::Warning)
        );
    }

    #[gpui_kit::test]
    async fn url_apply_fills_fields_and_a_bad_url_keeps_them(cx: &mut TestAppContext) {
        // Given: the empty new-connection form.
        let env = setup(cx);
        let form = open_blank(cx, &env).await;
        // When: applying a valid URL.
        type_in(
            cx,
            &env,
            "connection-url",
            "postgres://grace:pw@db.example:5433/app?sslmode=require",
        );
        click(cx, &env, "connection-url-apply");
        // Then: every field is filled and no error is shown.
        assert_eq!(
            value_of(cx, &env, "connection-host").as_deref(),
            Some("db.example")
        );
        assert_eq!(
            value_of(cx, &env, "connection-port").as_deref(),
            Some("5433")
        );
        assert_eq!(
            value_of(cx, &env, "connection-user").as_deref(),
            Some("grace")
        );
        assert_eq!(
            value_of(cx, &env, "connection-database").as_deref(),
            Some("app")
        );
        assert_eq!(
            value_of(cx, &env, "connection-name").as_deref(),
            Some("db.example")
        );
        cx.update(|cx| {
            let form = form.read(cx);
            assert_eq!(form.password.read(cx).value().as_str(), "pw");
            assert_eq!(form.tls.read(cx).selected_value(), Some(&TlsMode::Require));
        });
        assert!(!present(cx, &env, "connection-url-error"));
        // When: applying an invalid URL.
        type_in(cx, &env, "connection-url", "nope://x");
        click(cx, &env, "connection-url-apply");
        // Then: the error shows and the fields are untouched.
        assert!(present(cx, &env, "connection-url-error"));
        assert_eq!(
            value_of(cx, &env, "connection-host").as_deref(),
            Some("db.example")
        );
    }

    #[gpui_kit::test]
    async fn validation_keeps_typed_values(cx: &mut TestAppContext) {
        // Given: a form with a name and user but no host.
        let env = setup(cx);
        open_blank(cx, &env).await;
        type_in(cx, &env, "connection-name", "Keep me");
        type_in(cx, &env, "connection-user", "ada");
        // When: saving.
        click(cx, &env, "dialog-save");
        // Then: the dialog stays with the host error and every typed value intact.
        assert!(present(cx, &env, "connection-error-host"));
        assert_eq!(
            value_of(cx, &env, "connection-name").as_deref(),
            Some("Keep me")
        );
        assert_eq!(
            value_of(cx, &env, "connection-user").as_deref(),
            Some("ada")
        );
        assert_eq!(profiles(cx).len(), 0);
    }

    #[gpui_kit::test]
    async fn unavailable_keychain_is_disabled_and_explained(cx: &mut TestAppContext) {
        // Given: an unavailable keychain.
        let env = setup(cx);
        env.secrets
            .set_availability(Availability::Unavailable("no service".into()));
        // When: opening the form.
        let form = open_blank(cx, &env).await;
        // Then: the option is disabled, explained, and not preselected.
        assert!(present(cx, &env, "connection-password-storage-note"));
        let unavailable = storage_choices(&Availability::Unavailable("no service".into()));
        assert!(unavailable[0].disabled);
        assert!(!storage_choices(&Availability::Available)[0].disabled);
        cx.update(|cx| {
            let selected = form.read(cx).storage.read(cx).selected_value().copied();
            assert_eq!(selected, Some(PasswordStorage::Ask));
        });
    }

    #[gpui_kit::test]
    async fn test_connection_shows_ok_then_the_scripted_failure(cx: &mut TestAppContext) {
        // Given: a filled-in form with a typed password and a fake PostgreSQL server.
        let env = setup(cx);
        let form = open_blank(cx, &env).await;
        fill_basics(cx, &env);
        type_in(cx, &env, "connection-password", "tp");
        // When: testing the connection.
        click(cx, &env, "connection-test");
        // Then: the fake server's version is reported.
        assert_eq!(
            cx.update(|cx| form.read(cx).test.clone()),
            TestState::Ok("Fake Postgres 1.0".into())
        );
        assert!(present(cx, &env, "connection-test-result"));
        // When: the next connection attempt fails.
        env.factory.fail_connect(DbError::Auth("rejected".into()));
        click(cx, &env, "connection-test");
        // Then: the failure is reported and both attempts used the typed password.
        assert!(matches!(
            cx.update(|cx| form.read(cx).test.clone()),
            TestState::Err(message) if message.contains("rejected")
        ));
        let connects = env.factory.connects();
        assert_eq!(connects.len(), 2);
        assert!(matches!(
            &connects[0],
            crate::db::model::ConnectParams::Postgres(server)
                if server.password == Some(Secret::new("tp".into()))
        ));
    }

    fn pick(cx: &mut TestAppContext, env: &Env, select: &'static str, downs: usize) {
        cx.update_window(env.handle, |_, window, cx| {
            window.within(select).click("input", cx);
            for _ in 0..downs {
                window.press("down", cx);
            }
            window.press("enter", cx);
        })
        .expect("window");
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    async fn switching_driver_swaps_fields_and_resets_the_default_port(cx: &mut TestAppContext) {
        // Given: the form on PostgreSQL with its default port.
        let env = setup(cx);
        open_blank(cx, &env).await;
        assert!(!present(cx, &env, "connection-file"));
        // When: choosing MySQL, then SQLite.
        pick(cx, &env, "connection-driver", 1);
        // Then: the port moved to the MySQL default.
        assert_eq!(
            value_of(cx, &env, "connection-port").as_deref(),
            Some("3306")
        );
        pick(cx, &env, "connection-driver", 1);
        // Then: the file field replaces the server fields.
        assert!(present(cx, &env, "connection-file"));
        assert!(!present(cx, &env, "connection-host"));
    }

    #[gpui_kit::test]
    async fn sqlite_relative_path_is_joined_to_the_working_directory(cx: &mut TestAppContext) {
        // Given: the form switched to SQLite with a relative file typed.
        let env = setup(cx);
        let form = open_blank(cx, &env).await;
        cx.update_window(env.handle, |_, window, cx| {
            form.update(cx, |form, cx| form.set_driver(Driver::Sqlite, window, cx));
        })
        .expect("window");
        type_in(cx, &env, "connection-name", "Local");
        type_in(cx, &env, "connection-file", "relative.db");
        // When: saving.
        click(cx, &env, "dialog-save");
        // Then: the stored path is absolute, below the working directory.
        let expected = std::env::current_dir().expect("cwd").join("relative.db");
        let saved = profiles(cx);
        assert_eq!(saved[0].endpoint, Endpoint::File { path: expected });
        assert_eq!(saved[0].password, PasswordStorage::None);
    }

    fn register_running_query(cx: &mut TestAppContext, id: ConnectionId) {
        cx.update(|cx| {
            let tabs = cx.global::<GlobalTabStore>().0.clone();
            tabs.update(cx, |tabs, cx| {
                tabs.register_guard(TabKindTag::Query, |_, _| vec![CloseReason::RunningQuery]);
                tabs.open_query(id, None, cx);
            });
        });
    }

    #[gpui_kit::test]
    async fn edit_with_a_running_query_asks_first(cx: &mut TestAppContext) {
        // Given: a connection with a running query, opened for editing.
        let env = setup(cx);
        let original = profile("Existing", PasswordStorage::Ask);
        let id = add(cx, original.clone());
        register_running_query(cx, id);
        open_form_of(cx, &env, Mode::Edit(Box::new(original))).await;
        type_in(cx, &env, "connection-host", "x");
        // When: saving and the confirmation is cancelled.
        click(cx, &env, "dialog-save");
        // Then: the user was asked, the store is unchanged and the form stays open.
        assert_eq!(env.prompts.confirm_requests().len(), 1);
        assert!(matches!(
            &profiles(cx)[0].endpoint,
            Endpoint::Server { host, .. } if host == "db.test"
        ));
        assert!(present(cx, &env, "dialog-save"));
        // When: saving again and confirming.
        env.prompts.queue_confirm(ConfirmChoice::Confirm);
        click(cx, &env, "dialog-save");
        // Then: the edit is stored and the form closes.
        assert!(matches!(
            &profiles(cx)[0].endpoint,
            Endpoint::Server { host, .. } if host == "db.testx"
        ));
        dialog_closed(cx, env.handle).await;
    }

    #[gpui_kit::test]
    async fn edit_keeps_replaces_or_drops_the_stored_password(cx: &mut TestAppContext) {
        // Given: a Keychain connection whose secret is "old".
        use crate::secrets::SecretStore as _;
        let env = setup(cx);
        let original = profile("Keyed", PasswordStorage::Keychain);
        let id = add(cx, original.clone());
        env.secrets
            .set(id, &Secret::new("old".into()))
            .expect("set");
        // When: editing the name with an empty password field.
        open_form_of(cx, &env, Mode::Edit(Box::new(original.clone()))).await;
        type_in(cx, &env, "connection-name", "2");
        click(cx, &env, "dialog-save");
        // Then: the stored password is kept.
        assert_eq!(stored_password(&env, id), Some(Secret::new("old".into())));
        // When: editing again with a new password.
        let current = profiles(cx).remove(0);
        open_form_of(cx, &env, Mode::Edit(Box::new(current.clone()))).await;
        type_in(cx, &env, "connection-password", "new");
        click(cx, &env, "dialog-save");
        // Then: the entry is replaced.
        assert_eq!(stored_password(&env, id), Some(Secret::new("new".into())));
        // When: switching the storage to Ask.
        let current = profiles(cx).remove(0);
        let form = open_form_of(cx, &env, Mode::Edit(Box::new(current))).await;
        cx.update_window(env.handle, |_, window, cx| {
            form.update(cx, |form, cx| {
                form.storage.update(cx, |select, cx| {
                    select.set_selected_value(&PasswordStorage::Ask, window, cx);
                });
            });
        })
        .expect("window");
        click(cx, &env, "dialog-save");
        // Then: the keychain entry is deleted.
        assert_eq!(stored_password(&env, id), None);
    }

    #[gpui_kit::test]
    fn duplicate_copies_the_secret_or_falls_back_to_ask(cx: &mut TestAppContext) {
        // Given: a Keychain connection with a stored secret.
        use crate::secrets::SecretStore as _;
        let env = setup(cx);
        let id = add(cx, profile("Source", PasswordStorage::Keychain));
        env.secrets
            .set(id, &Secret::new("dup".into()))
            .expect("set");
        // When: duplicating it.
        cx.update_window(env.handle, |_, window, cx| duplicate(id, window, cx))
            .expect("window");
        cx.run_until_parked();
        // Then: the copy holds the same secret and keeps Keychain storage.
        let copy = profiles(cx).into_iter().find(|p| p.id != id).expect("copy");
        assert_eq!(copy.password, PasswordStorage::Keychain);
        assert_eq!(
            stored_password(&env, copy.id),
            Some(Secret::new("dup".into()))
        );
        // When: duplicating while the secret store fails.
        env.secrets.fail_with("locked");
        cx.update_window(env.handle, |_, window, cx| duplicate(id, window, cx))
            .expect("window");
        cx.run_until_parked();
        // Then: the new copy asks for its password and a warning was shown.
        let asking = profiles(cx)
            .into_iter()
            .filter(|p| p.id != id && p.id != copy.id)
            .collect::<Vec<_>>();
        assert_eq!(asking.len(), 1);
        assert_eq!(asking[0].password, PasswordStorage::Ask);
        assert!(
            env.prompts
                .notices()
                .iter()
                .any(|notice| notice.level == NoticeLevel::Warning)
        );
    }

    #[gpui_kit::test]
    fn delete_asks_and_removes_only_when_confirmed(cx: &mut TestAppContext) {
        // Given: a connection.
        let env = setup(cx);
        let id = add(cx, profile("Doomed", PasswordStorage::Ask));
        // When: deleting with the default answer (cancel).
        cx.update_window(env.handle, |_, window, cx| delete(id, window, cx))
            .expect("window");
        cx.run_until_parked();
        // Then: a destructive confirmation was asked and nothing was removed.
        let requests = env.prompts.confirm_requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].destructive);
        assert_eq!(profiles(cx).len(), 1);
        // When: deleting and confirming.
        env.prompts.queue_confirm(ConfirmChoice::Confirm);
        cx.update_window(env.handle, |_, window, cx| delete(id, window, cx))
            .expect("window");
        cx.run_until_parked();
        // Then: the connection is gone.
        assert_eq!(profiles(cx).len(), 0);
    }
}
