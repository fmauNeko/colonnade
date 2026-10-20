//! Connection sessions: connect, passwords, schema cache, disconnect.
// allow: SIZE_OK — Single-file task ownership requires the session lifecycle and its adjacent headless regression tests here.

use crate::{
    catalog::{
        model::{ConnectionId, ConnectionProfile, PasswordStorage},
        store::{ConnectionStoreEvent, GlobalConnectionStore},
    },
    db::{
        backend::{Backends, DbBackend, QueryConnection},
        model::{
            ColumnInfo, DbError, ObjectNode, ObjectPath, ObjectRef, Page, PageRequest, Secret,
        },
        runtime::spawn_db,
    },
    prompts::{self, Notice, NoticeLevel, PasswordRequest},
    secrets::{Availability, Secrets},
};
use futures::{FutureExt, future::Shared};
use gpui_kit::prelude::*;
use gpui_kit::{App, AsyncWindowContext, Context, Entity, EventEmitter, Global, Task, Window};
use std::{collections::HashMap, sync::Arc};

type ConnectTask = Shared<Task<Result<Arc<dyn DbBackend>, DbError>>>;
type ChildrenTask = Shared<Task<Result<Arc<Vec<ObjectNode>>, DbError>>>;

/// Lifecycle visible to connection views.
#[derive(Clone, Default)]
pub enum SessionStatus {
    #[default]
    Disconnected,
    Connecting,
    Connected(Arc<dyn DbBackend>),
    Failed(DbError),
}

/// Cached catalog lookup state.
#[derive(Clone, Debug)]
pub enum ChildrenState {
    Loading,
    Loaded(Arc<Vec<ObjectNode>>),
    Failed(DbError),
}

#[derive(Default)]
struct Session {
    status: SessionStatus,
    connecting: Option<ConnectTask>,
    children: HashMap<ObjectPath, ChildrenState>,
    loading: HashMap<ObjectPath, (Arc<()>, ChildrenTask)>,
    generation: Arc<()>,
}

/// Session changes consumed by workspace views.
#[derive(Clone, Debug)]
pub enum SessionEvent {
    StatusChanged(ConnectionId),
    ChildrenChanged(ConnectionId, ObjectPath),
    Notice(Notice),
}

/// Lazily connected backends and session-scoped credentials.
#[derive(Default)]
pub struct ConnectionSessions {
    sessions: HashMap<ConnectionId, Session>,
    ask_passwords: HashMap<ConnectionId, Secret>,
    names: HashMap<ConnectionId, String>,
}
impl EventEmitter<SessionEvent> for ConnectionSessions {}

/// Application-wide session entity.
pub struct GlobalSessions(pub Entity<ConnectionSessions>);
impl Global for GlobalSessions {}

impl ConnectionSessions {
    #[must_use]
    pub fn status(&self, id: ConnectionId) -> &SessionStatus {
        self.sessions
            .get(&id)
            .map_or(&SessionStatus::Disconnected, |s| &s.status)
    }

    #[must_use]
    pub fn children(&self, id: ConnectionId, path: &ObjectPath) -> Option<&ChildrenState> {
        self.sessions.get(&id)?.children.get(path)
    }

    /// Resolves credentials and shares one connection attempt between consumers.
    ///
    /// # Errors
    /// The task returns cancellation, a missing-profile error or the driver's connection error.
    pub fn ensure_connected(
        &mut self,
        id: ConnectionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<Arc<dyn DbBackend>, DbError>> {
        let session = self.sessions.entry(id).or_default();
        match (&session.status, session.connecting.clone()) {
            (SessionStatus::Connected(backend), _) => return Task::ready(Ok(backend.clone())),
            (SessionStatus::Connecting, Some(task)) => {
                return cx.spawn(async move |_, _| task.await);
            }
            (SessionStatus::Connecting, None)
            | (SessionStatus::Disconnected | SessionStatus::Failed(_), _) => {}
        }
        let store = cx.global::<GlobalConnectionStore>().0.clone();
        let Some(profile) = store.read(cx).profile(id).cloned() else {
            return Task::ready(Err(DbError::Connect("Connection no longer exists".into())));
        };
        let cached = self.ask_passwords.get(&id).cloned();
        session.status = SessionStatus::Connecting;
        let generation = session.generation.clone();
        cx.emit(SessionEvent::StatusChanged(id));
        let task = cx
            .spawn_in(window, async move |this, cx| {
                let resolved = resolve_password(&profile, cached, cx).await;
                let mut password = None;
                let mut save = false;
                let result = match resolved {
                    Ok((secret, save_secret)) => {
                        password = secret;
                        save = save_secret;
                        let current = this
                            .update(cx, |s, _| s.is_current(id, &generation))
                            .unwrap_or(false);
                        if current {
                            let params = profile.connect_params(password.clone());
                            let task = cx
                                .update(|_, cx| {
                                    spawn_db(cx, cx.global::<Backends>().0.connect(params))
                                })
                                .map_err(|_| DbError::Cancelled)?;
                            task.await
                        } else {
                            Err(DbError::Cancelled)
                        }
                    }
                    Err(error) => Err(error),
                };
                let applied = this
                    .update(cx, |s, cx| {
                        if !s.is_current(id, &generation) {
                            return false;
                        }
                        let session = s.sessions.entry(id).or_default();
                        session.connecting = None;
                        session.status = match &result {
                            Ok(backend) => {
                                if profile.password == PasswordStorage::Ask
                                    && let Some(secret) = &password
                                {
                                    s.ask_passwords.insert(id, secret.clone());
                                }
                                SessionStatus::Connected(backend.clone())
                            }
                            Err(DbError::Cancelled) => SessionStatus::Disconnected,
                            Err(error) => {
                                s.ask_passwords.remove(&id);
                                SessionStatus::Failed(error.clone())
                            }
                        };
                        cx.emit(SessionEvent::StatusChanged(id));
                        true
                    })
                    .unwrap_or(false);
                if !applied {
                    if let Ok(backend) = result {
                        let close = cx
                            .update(|_, cx| spawn_db(cx, backend.close()))
                            .map_err(|_| DbError::Cancelled)?;
                        close.await?;
                    }
                    return Err(DbError::Cancelled);
                }
                if result.is_ok()
                    && save
                    && let Some(password) = password
                {
                    let store = cx
                        .update(|_, cx| cx.global::<Secrets>().0.clone())
                        .map_err(|_| DbError::Cancelled)?;
                    let written = cx
                        .background_spawn(async move { store.set(id, &password) })
                        .await;
                    if written.is_err() {
                        warn(
                            cx,
                            format!("Could not save the password for {}", profile.name),
                        );
                    }
                }
                result
            })
            .shared();
        self.sessions.entry(id).or_default().connecting = Some(task.clone());
        cx.spawn(async move |_, _| task.await)
    }

    fn is_current(&self, id: ConnectionId, generation: &Arc<()>) -> bool {
        self.sessions
            .get(&id)
            .is_some_and(|s| Arc::ptr_eq(&s.generation, generation))
    }

    /// Loads one catalog branch, reusing pending or cached results.
    ///
    /// # Errors
    /// The task returns connection, cancellation or catalog lookup errors.
    pub fn load_children(
        &mut self,
        id: ConnectionId,
        path: ObjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<Arc<Vec<ObjectNode>>, DbError>> {
        if let Some(session) = self.sessions.get(&id) {
            if let Some((_, task)) = session.loading.get(&path) {
                let task = task.clone();
                return cx.spawn(async move |_, _| task.await);
            }
            if let Some(ChildrenState::Loaded(nodes)) = session.children.get(&path) {
                return Task::ready(Ok(nodes.clone()));
            }
        }
        let connect = self.ensure_connected(id, window, cx);
        let generation = self.sessions.entry(id).or_default().generation.clone();
        let request = Arc::new(());
        let token = request.clone();
        self.sessions
            .entry(id)
            .or_default()
            .children
            .insert(path.clone(), ChildrenState::Loading);
        cx.emit(SessionEvent::ChildrenChanged(id, path.clone()));
        let lookup = path.clone();
        let task = cx
            .spawn(async move |this, cx| {
                let result = match connect.await {
                    Ok(backend) => {
                        let task =
                            cx.update(|cx| spawn_db(cx, backend.list_children(lookup.clone())));
                        task.await.map(Arc::new)
                    }
                    Err(error) => Err(error),
                };
                this.update(cx, |s, cx| {
                    if !s.is_current(id, &generation) {
                        return Err(DbError::Cancelled);
                    }
                    let session = s.sessions.entry(id).or_default();
                    if !session
                        .loading
                        .get(&lookup)
                        .is_some_and(|(active, _)| Arc::ptr_eq(active, &token))
                    {
                        return Err(DbError::Cancelled);
                    }
                    session.loading.remove(&lookup);
                    match &result {
                        Ok(nodes) => {
                            session
                                .children
                                .insert(lookup.clone(), ChildrenState::Loaded(nodes.clone()));
                        }
                        Err(DbError::Cancelled) => {
                            session.children.remove(&lookup);
                        }
                        Err(error) => {
                            session
                                .children
                                .insert(lookup.clone(), ChildrenState::Failed(error.clone()));
                        }
                    }
                    cx.emit(SessionEvent::ChildrenChanged(id, lookup));
                    result
                })
                .unwrap_or(Err(DbError::Cancelled))
            })
            .shared();
        self.sessions
            .entry(id)
            .or_default()
            .loading
            .insert(path, (request, task.clone()));
        cx.spawn(async move |_, _| task.await)
    }

    /// Invalidates one branch before loading it again.
    ///
    /// # Errors
    /// The task returns the same errors as [`Self::load_children`].
    pub fn refresh_children(
        &mut self,
        id: ConnectionId,
        path: ObjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<Arc<Vec<ObjectNode>>, DbError>> {
        if let Some(session) = self.sessions.get_mut(&id) {
            session.children.remove(&path);
            session.loading.remove(&path);
        }
        self.load_children(id, path, window, cx)
    }

    /// Opens a dedicated query connection on the database runtime.
    ///
    /// # Errors
    /// The task returns connection or driver errors.
    pub fn open_query_connection(
        &mut self,
        id: ConnectionId,
        database: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<Arc<dyn QueryConnection>, DbError>> {
        let connect = self.ensure_connected(id, window, cx);
        cx.spawn(async move |_, cx| {
            let backend = connect.await?;
            cx.update(|cx| spawn_db(cx, backend.open_query_connection(database)))
                .await
        })
    }

    /// Fetches a table page on the database runtime.
    ///
    /// # Errors
    /// The task returns connection or page query errors.
    pub fn fetch_page(
        &mut self,
        id: ConnectionId,
        request: PageRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<Page, DbError>> {
        let connect = self.ensure_connected(id, window, cx);
        cx.spawn(async move |_, cx| {
            let backend = connect.await?;
            cx.update(|cx| spawn_db(cx, backend.fetch_page(request)))
                .await
        })
    }

    /// Fetches column metadata on the database runtime.
    ///
    /// # Errors
    /// The task returns connection or metadata lookup errors.
    pub fn table_columns(
        &mut self,
        id: ConnectionId,
        object: ObjectRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<ColumnInfo>, DbError>> {
        let connect = self.ensure_connected(id, window, cx);
        cx.spawn(async move |_, cx| {
            let backend = connect.await?;
            cx.update(|cx| spawn_db(cx, backend.table_columns(object)))
                .await
        })
    }

    /// Clears session credentials and catalog caches, closing a connected backend asynchronously.
    pub fn disconnect(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        self.ask_passwords.remove(&id);
        if let Some(session) = self.sessions.remove(&id) {
            for path in session.children.into_keys() {
                cx.emit(SessionEvent::ChildrenChanged(id, path));
            }
            match session.status {
                SessionStatus::Connected(backend) => {
                    let task = spawn_db(cx, backend.close());
                    cx.spawn(async move |this, cx| {
                        if task.await.is_err() {
                            drop(this.update(cx, |_, cx| {
                                cx.emit(SessionEvent::Notice(warning(
                                    "Could not close the connection".into(),
                                )));
                            }));
                        }
                    })
                    .detach();
                }
                SessionStatus::Disconnected
                | SessionStatus::Connecting
                | SessionStatus::Failed(_) => {}
            }
        }
        cx.emit(SessionEvent::StatusChanged(id));
    }
}

fn warning(message: String) -> Notice {
    Notice {
        level: NoticeLevel::Warning,
        message: message.into(),
    }
}

fn warn(cx: &mut AsyncWindowContext, message: String) {
    // A notice remains observable even if its originating window was closed.
    let notice = warning(message);
    let sessions = gpui_kit::AsyncApp::update(cx, |cx| cx.global::<GlobalSessions>().0.clone());
    sessions.update(cx, |_, cx| cx.emit(SessionEvent::Notice(notice.clone())));
    drop(cx.update(|window, cx| prompts::notify(window, cx, notice)));
}

async fn resolve_password(
    profile: &ConnectionProfile,
    cached: Option<Secret>,
    cx: &mut AsyncWindowContext,
) -> Result<(Option<Secret>, bool), DbError> {
    let mut offer_keychain = false;
    match profile.password {
        PasswordStorage::None => return Ok((None, false)),
        PasswordStorage::Ask => {
            if let Some(password) = cached {
                return Ok((Some(password), false));
            }
        }
        PasswordStorage::Keychain => {
            let store = gpui_kit::AsyncApp::update(cx, |cx| cx.global::<Secrets>().0.clone());
            let id = profile.id;
            let (availability, read) = cx
                .background_spawn(async move { (store.availability(), store.get(id)) })
                .await;
            offer_keychain = match availability {
                Availability::Available => true,
                Availability::Unavailable(_) => false,
            };
            match read {
                Ok(Some(password)) => return Ok((Some(password), false)),
                Ok(None) => {}
                Err(_) => warn(
                    cx,
                    format!("Could not read the password for {}", profile.name),
                ),
            }
        }
    }
    let request = PasswordRequest {
        connection: profile.id,
        connection_name: profile.name.clone().into(),
        offer_keychain,
    };
    let answer = cx
        .update(|window, cx| prompts::prompt_password(window, cx, request))
        .map_err(|_| DbError::Cancelled)?
        .await
        .map_err(|_| DbError::Cancelled)?;
    match answer {
        Some(answer) => Ok((
            Some(answer.password),
            offer_keychain && answer.save_to_keychain,
        )),
        None => Err(DbError::Cancelled),
    }
}

/// Installs sessions and observes endpoint changes and catalog removals.
pub fn init(cx: &mut App) {
    let store = cx.global::<GlobalConnectionStore>().0.clone();
    let names = crate::catalog::ops::connections(store.read(cx).items())
        .into_iter()
        .map(|p| (p.id, p.name.clone()))
        .collect();
    let sessions = cx.new(|cx| {
        cx.subscribe(
            &store,
            |sessions: &mut ConnectionSessions, store, event, cx| match event {
                ConnectionStoreEvent::ConnectionRemoved(id) => {
                    let name = sessions
                        .names
                        .remove(id)
                        .unwrap_or_else(|| id.as_uuid().to_string());
                    sessions.disconnect(*id, cx);
                    let secrets = cx.global::<Secrets>().0.clone();
                    let id = *id;
                    let task = cx.background_spawn(async move { secrets.delete(id) });
                    cx.spawn(async move |this, cx| {
                        if task.await.is_err() {
                            drop(this.update(cx, |_, cx| {
                                cx.emit(SessionEvent::Notice(warning(format!(
                                    "Could not delete the password for {name}"
                                ))));
                            }));
                        }
                    })
                    .detach();
                }
                ConnectionStoreEvent::ConnectionUpdated {
                    id,
                    endpoint_changed,
                } => {
                    if *endpoint_changed {
                        sessions.disconnect(*id, cx);
                    }
                    if let Some(profile) = store.read(cx).profile(*id) {
                        sessions.names.insert(*id, profile.name.clone());
                    }
                }
                ConnectionStoreEvent::ConnectionAdded(id) => {
                    if let Some(profile) = store.read(cx).profile(*id) {
                        sessions.names.insert(*id, profile.name.clone());
                    }
                }
                ConnectionStoreEvent::Changed | ConnectionStoreEvent::ColorChanged(_) => {}
            },
        )
        .detach();
        ConnectionSessions {
            names,
            ..ConnectionSessions::default()
        }
    });
    cx.set_global(GlobalSessions(sessions));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        catalog::{
            model::{ConnectionProfile, Driver, Endpoint},
            ops::{DropPosition, NodeRef},
            store::{self, ConnectionStore},
        },
        db::{
            backend::install_factory,
            fake::{FakeEndpoint, FakeFactory, FakeServer},
            model::{Dialect, TableKind, TlsMode},
            runtime::DbRuntime,
        },
        paths::ConfigPaths,
        prompts::{self, PasswordAnswer, ScriptedPrompts},
        secrets::{self, MemorySecretStore, SecretStore},
    };
    use gpui_kit::TestAppContext;
    use std::{cell::RefCell, rc::Rc};

    struct Fixture {
        _dir: tempfile::TempDir,
        id: ConnectionId,
        sessions: Entity<ConnectionSessions>,
        store: Entity<ConnectionStore>,
        factory: Arc<FakeFactory>,
        server: Arc<FakeServer>,
        secrets: Arc<MemorySecretStore>,
        prompts: Rc<ScriptedPrompts>,
        events: Rc<RefCell<Vec<SessionEvent>>>,
    }

    fn setup(cx: &mut TestAppContext, storage: PasswordStorage) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let factory = Arc::new(FakeFactory::new());
        let server = Arc::new(FakeServer::sample(Dialect::Postgres));
        factory.register(FakeEndpoint::Host("local".into()), server.clone());
        let secrets = Arc::new(MemorySecretStore::default());
        let profile = ConnectionProfile::new(
            "Test connection",
            Driver::Postgres,
            Endpoint::Server {
                host: "local".into(),
                port: 5432,
                user: "user".into(),
                database: None,
                tls: TlsMode::Prefer,
            },
            storage,
        );
        let id = profile.id;
        let events = Rc::new(RefCell::new(Vec::new()));
        let (sessions, store, prompts) = cx.update(|cx| {
            cx.set_global(DbRuntime::inline());
            install_factory(cx, factory.clone());
            secrets::install(cx, secrets.clone());
            let prompts = prompts::install_scripted(cx);
            store::init(cx, &ConfigPaths::at(dir.path().to_owned()));
            let store = cx.global::<GlobalConnectionStore>().0.clone();
            store
                .update(cx, |store, cx| {
                    store.add_connection(cx, profile, DropPosition::RootEnd)
                })
                .unwrap();
            init(cx);
            let sessions = cx.global::<GlobalSessions>().0.clone();
            let sink = events.clone();
            cx.subscribe(&sessions, move |_, event: &SessionEvent, _| {
                sink.borrow_mut().push(event.clone());
            })
            .detach();
            (sessions, store, prompts)
        });
        Fixture {
            _dir: dir,
            id,
            sessions,
            store,
            factory,
            server,
            secrets,
            prompts,
            events,
        }
    }

    impl Fixture {
        fn answer(&self, save: bool) {
            self.prompts.queue_password(Some(PasswordAnswer {
                password: Secret::new("password".into()),
                save_to_keychain: save,
            }));
        }
        fn connect(
            &self,
            window: &mut Window,
            cx: &mut App,
        ) -> Task<Result<Arc<dyn DbBackend>, DbError>> {
            self.sessions.update(cx, |sessions, cx| {
                sessions.ensure_connected(self.id, window, cx)
            })
        }
        fn disconnect(&self, cx: &mut App) {
            self.sessions
                .update(cx, |sessions, cx| sessions.disconnect(self.id, cx));
        }
    }

    #[gpui_kit::test]
    async fn keychain_password_reaches_factory_without_prompt(cx: &mut TestAppContext) {
        // Given: a saved credential.
        let f = setup(cx, PasswordStorage::Keychain);
        f.secrets.set(f.id, &Secret::new("stored".into())).unwrap();
        let window = cx.add_empty_window();
        // When: connecting.
        assert!(window.update(|w, cx| f.connect(w, cx)).await.is_ok());
        // Then: the factory receives it without prompting.
        assert_eq!(f.prompts.password_requests().len(), 0);
        assert!(
            matches!(&f.factory.connects()[0], crate::db::model::ConnectParams::Postgres(p) if p.password.as_ref().unwrap().expose() == "stored")
        );
    }

    #[gpui_kit::test]
    async fn missing_keychain_password_can_be_saved(cx: &mut TestAppContext) {
        // Given: no saved credential and an affirmative answer.
        let f = setup(cx, PasswordStorage::Keychain);
        f.answer(true);
        let window = cx.add_empty_window();
        // When: connecting.
        assert!(window.update(|w, cx| f.connect(w, cx)).await.is_ok());
        // Then: the answer is stored only after success.
        assert_eq!(
            f.secrets.get(f.id).unwrap(),
            Some(Secret::new("password".into()))
        );
        assert!(f.prompts.password_requests()[0].offer_keychain);
    }

    #[gpui_kit::test]
    async fn ask_prompts_once_while_connected(cx: &mut TestAppContext) {
        // Given: an established Ask session.
        let f = setup(cx, PasswordStorage::Ask);
        f.answer(false);
        let window = cx.add_empty_window();
        assert!(window.update(|w, cx| f.connect(w, cx)).await.is_ok());
        // When: requesting it again.
        assert!(window.update(|w, cx| f.connect(w, cx)).await.is_ok());
        // Then: neither prompt nor factory runs again.
        assert_eq!(f.prompts.password_requests().len(), 1);
        assert_eq!(f.factory.connects().len(), 1);
    }

    #[gpui_kit::test]
    async fn disconnect_clears_ask_password(cx: &mut TestAppContext) {
        // Given: a previously connected Ask session, now disconnected.
        let f = setup(cx, PasswordStorage::Ask);
        f.answer(false);
        let window = cx.add_empty_window();
        assert!(window.update(|w, cx| f.connect(w, cx)).await.is_ok());
        window.update(|_, cx| f.disconnect(cx));
        f.answer(false);
        // When: connecting again.
        assert!(window.update(|w, cx| f.connect(w, cx)).await.is_ok());
        // Then: a fresh answer was required.
        assert_eq!(f.prompts.password_requests().len(), 2);
        assert_eq!(f.server.lifecycle_counts().0, 1);
    }

    #[gpui_kit::test]
    async fn cancelled_prompt_is_silent(cx: &mut TestAppContext) {
        // Given: an unanswered prompt.
        let f = setup(cx, PasswordStorage::Ask);
        let window = cx.add_empty_window();
        // When: the user cancels.
        let result = window.update(|w, cx| f.connect(w, cx)).await;
        // Then: cancellation leaves no failed status or notice.
        assert!(matches!(result, Err(DbError::Cancelled)));
        window.update(|_, cx| {
            assert!(matches!(
                f.sessions.read(cx).status(f.id),
                SessionStatus::Disconnected
            ));
        });
        assert_eq!(f.prompts.notices().len(), 0);
        assert!(
            !f.events
                .borrow()
                .iter()
                .any(|e| matches!(e, SessionEvent::Notice(_)))
        );
    }

    #[gpui_kit::test]
    async fn failed_connect_does_not_cache_password(cx: &mut TestAppContext) {
        // Given: authentication will fail.
        let f = setup(cx, PasswordStorage::Ask);
        f.answer(false);
        f.factory.fail_connect(DbError::Auth("rejected".into()));
        let window = cx.add_empty_window();
        // When: connecting.
        assert!(matches!(
            window.update(|w, cx| f.connect(w, cx)).await,
            Err(DbError::Auth(_))
        ));
        // Then: failed state retains no session password; retry prompts again.
        window.update(|_, cx| {
            assert!(matches!(
                f.sessions.read(cx).status(f.id),
                SessionStatus::Failed(DbError::Auth(_))
            ));
            assert!(!f.sessions.read(cx).ask_passwords.contains_key(&f.id));
        });
        f.answer(false);
        assert!(window.update(|w, cx| f.connect(w, cx)).await.is_ok());
        assert_eq!(f.prompts.password_requests().len(), 2);
    }

    #[gpui_kit::test]
    async fn concurrent_connects_share_one_factory_call(cx: &mut TestAppContext) {
        // Given: a held connection attempt.
        let f = setup(cx, PasswordStorage::None);
        let gate = f.factory.hold_connects();
        let window = cx.add_empty_window();
        // When: two consumers request it before completion.
        let (first, second) = window.update(|w, cx| (f.connect(w, cx), f.connect(w, cx)));
        gate.release_all();
        let (first, second) = futures::join!(first, second);
        // Then: both receive the same backend from a single connect.
        assert!(Arc::ptr_eq(&first.unwrap(), &second.unwrap()));
        assert_eq!(f.factory.connects().len(), 1);
    }

    #[gpui_kit::test]
    async fn children_transition_from_loading_to_loaded(cx: &mut TestAppContext) {
        // Given: an unloaded catalog.
        let f = setup(cx, PasswordStorage::None);
        let window = cx.add_empty_window();
        // When: loading its root.
        let task = window.update(|w, cx| {
            f.sessions
                .update(cx, |s, cx| s.load_children(f.id, ObjectPath::Root, w, cx))
        });
        window.update(|_, cx| {
            assert!(matches!(
                f.sessions.read(cx).children(f.id, &ObjectPath::Root),
                Some(ChildrenState::Loading)
            ));
        });
        task.await.unwrap();
        // Then: the real fake catalog is cached.
        window.update(|_, cx| assert!(matches!(f.sessions.read(cx).children(f.id, &ObjectPath::Root), Some(ChildrenState::Loaded(nodes)) if nodes.len() == 2)));
    }

    #[gpui_kit::test]
    async fn children_failure_emits_event(cx: &mut TestAppContext) {
        // Given: a failing catalog lookup.
        let f = setup(cx, PasswordStorage::None);
        f.server.fail_children(DbError::Timeout { seconds: 3 });
        let window = cx.add_empty_window();
        // When: loading.
        let result = window
            .update(|w, cx| {
                f.sessions
                    .update(cx, |s, cx| s.load_children(f.id, ObjectPath::Root, w, cx))
            })
            .await;
        // Then: the failure is cached and published.
        assert!(matches!(result, Err(DbError::Timeout { .. })));
        window.update(|_, cx| {
            assert!(matches!(
                f.sessions.read(cx).children(f.id, &ObjectPath::Root),
                Some(ChildrenState::Failed(DbError::Timeout { .. }))
            ));
        });
        assert_eq!(f.events.borrow().iter().filter(|e| matches!(e, SessionEvent::ChildrenChanged(id, ObjectPath::Root) if *id == f.id)).count(), 2);
    }

    #[gpui_kit::test]
    async fn removal_disconnects_and_deletes_secret(cx: &mut TestAppContext) {
        // Given: a connected saved credential.
        let f = setup(cx, PasswordStorage::Keychain);
        f.secrets.set(f.id, &Secret::new("stored".into())).unwrap();
        let window = cx.add_empty_window();
        assert!(window.update(|w, cx| f.connect(w, cx)).await.is_ok());
        // When: removing its catalog entry.
        window.update(|_, cx| {
            f.store
                .update(cx, |s, cx| s.remove(cx, NodeRef::Connection(f.id)))
                .unwrap()
        });
        window.run_until_parked();
        // Then: the session and credential are gone.
        window.update(|_, cx| {
            assert!(matches!(
                f.sessions.read(cx).status(f.id),
                SessionStatus::Disconnected
            ));
        });
        assert_eq!(f.secrets.get(f.id).unwrap(), None);
        assert_eq!(f.server.lifecycle_counts().0, 1);
    }

    #[gpui_kit::test]
    async fn endpoint_change_disconnects(cx: &mut TestAppContext) {
        // Given: a connected profile.
        let f = setup(cx, PasswordStorage::None);
        let window = cx.add_empty_window();
        assert!(window.update(|w, cx| f.connect(w, cx)).await.is_ok());
        // When: changing its authentication policy.
        window.update(|_, cx| {
            let mut profile = f.store.read(cx).profile(f.id).unwrap().clone();
            profile.password = PasswordStorage::Ask;
            f.store
                .update(cx, |s, cx| s.update_connection(cx, profile))
                .unwrap();
        });
        // Then: the previous backend is no longer usable through the session.
        window.update(|_, cx| {
            assert!(matches!(
                f.sessions.read(cx).status(f.id),
                SessionStatus::Disconnected
            ));
        });
    }

    #[gpui_kit::test]
    async fn keychain_failure_warns_then_prompts(cx: &mut TestAppContext) {
        // Given: a locked keychain.
        let f = setup(cx, PasswordStorage::Keychain);
        f.secrets.fail_with("locked");
        f.answer(false);
        let window = cx.add_empty_window();
        // When: connecting with the fallback answer.
        assert!(window.update(|w, cx| f.connect(w, cx)).await.is_ok());
        // Then: fallback succeeds with a warning.
        assert_eq!(f.prompts.password_requests().len(), 1);
        assert_eq!(f.prompts.notices()[0].level, NoticeLevel::Warning);
    }

    #[gpui_kit::test]
    async fn disconnect_during_connect_prevents_resurrection(cx: &mut TestAppContext) {
        // Given: a pending connect.
        let f = setup(cx, PasswordStorage::None);
        let gate = f.factory.hold_connects();
        let window = cx.add_empty_window();
        let task = window.update(|w, cx| f.connect(w, cx));
        // When: disconnecting before completion.
        window.update(|_, cx| f.disconnect(cx));
        gate.release_all();
        // Then: no stale result revives the session.
        assert!(matches!(task.await, Err(DbError::Cancelled)));
        window.update(|_, cx| {
            assert!(matches!(
                f.sessions.read(cx).status(f.id),
                SessionStatus::Disconnected
            ));
        });
    }

    #[gpui_kit::test]
    async fn adapters_route_database_pages_and_columns(cx: &mut TestAppContext) {
        // Given: an unconnected profile and a real fake table request.
        let f = setup(cx, PasswordStorage::None);
        let object = ObjectRef {
            database: Some("app".into()),
            schema: Some("public".into()),
            name: "users".into(),
            kind: TableKind::Table,
        };
        let window = cx.add_empty_window();
        // When: using the public adapters.
        let query = window
            .update(|w, cx| {
                f.sessions.update(cx, |s, cx| {
                    s.open_query_connection(f.id, Some("analytics".into()), w, cx)
                })
            })
            .await
            .unwrap();
        let columns = window
            .update(|w, cx| {
                f.sessions
                    .update(cx, |s, cx| s.table_columns(f.id, object.clone(), w, cx))
            })
            .await
            .unwrap();
        let page = window
            .update(|w, cx| {
                f.sessions.update(cx, |s, cx| {
                    s.fetch_page(
                        f.id,
                        PageRequest {
                            object,
                            order_by: None,
                            offset: 400,
                            limit: 200,
                        },
                        w,
                        cx,
                    )
                })
            })
            .await
            .unwrap();
        // Then: all adapters reached the backend using one shared connection.
        assert_eq!(f.server.query_databases(), [Some("analytics".into())]);
        assert_eq!(columns.len(), 3);
        assert_eq!(page.rows.len(), 50);
        assert_eq!(f.factory.connects().len(), 1);
        assert!(query.close().await.is_ok());
    }
}
