use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, anyhow};
use credentials_provider::CredentialsProvider;
use db::kvp::KeyValueStore;
use futures::{FutureExt as _, StreamExt as _, future::Shared};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EntityId, EventEmitter, Global, SharedString,
    Task, TaskExt as _, WeakEntity,
};
use project::{Project, trusted_worktrees::TrustedWorktrees};
use remote::CommandTemplate;
use serde::{Deserialize, Serialize};
use settings::{Settings as _, SettingsStore};
use util::ResultExt as _;

use crate::{
    connection::{ConnectionConfig, ConnectionKey, DriverKind, ResolvedConnection, SessionOptions},
    database_settings::DatabaseSettings,
    driver::{
        ColumnInfo, ColumnMeta, DatabaseSession, ExecOptions, ObjectRef, RelationDetails,
        RelationInfo, ResultEvent, ResultRow, SchemaInfo, SchemaObjects,
    },
    history::{HistoryEntry, QueryHistory},
    mysql, postgres,
    query::QueryRun,
    sqlite,
    ssh_tunnel::{self, SshTunnel},
};

const WORKTREE_CONNECTION_KVP_PREFIX: &str = "database_worktree_connection:";
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(30);

/// The server rejected the connection's credentials, or none are stored. Ask for a password and
/// connect again with [`DbStore::connect`].
#[derive(Debug)]
pub struct PasswordRequired {
    pub message: Option<String>,
}

impl std::fmt::Display for PasswordRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.message {
            Some(message) => write!(f, "authentication failed: {message}"),
            None => f.write_str("a password is required"),
        }
    }
}

impl std::error::Error for PasswordRequired {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    PasswordRequired,
    Failed(SharedString),
}

#[derive(Clone)]
pub struct PasswordInput {
    pub password: String,
    /// Save the password in the keychain once the connection succeeds.
    pub remember: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DbStoreEvent {
    ConnectionChanged(ConnectionKey),
    SchemaChanged(ConnectionKey),
    HistoryChanged(ConnectionKey),
    EditorConnectionChanged(EntityId),
    /// The connection remembered for a worktree root was loaded or changed.
    WorktreeConnectionChanged(Arc<Path>),
}

/// Open sessions of a connection. Metadata queries use their own session, so that browsing the
/// schema never waits behind a long query or a result that is paused at its row limit.
pub struct Sessions {
    pub main: Arc<dyn DatabaseSession>,
    pub metadata: Arc<dyn DatabaseSession>,
    sanitize: Arc<dyn Fn(&str) -> String + Send + Sync>,
    _tunnel: Option<SshTunnel>,
}

impl Sessions {
    fn is_closed(&self) -> bool {
        self.main.is_closed() || self.metadata.is_closed()
    }
}

type SharedConnectTask = Shared<Task<Result<Arc<Sessions>, Arc<anyhow::Error>>>>;

struct ConnectionState {
    status: ConnectionStatus,
    sessions: Option<Arc<Sessions>>,
    connect_task: Option<SharedConnectTask>,
    failed_attempts: u32,
    last_failure: Option<Instant>,
    schemas: Option<Vec<SchemaInfo>>,
    relations: HashMap<SharedString, Vec<RelationInfo>>,
    columns: HashMap<(SharedString, SharedString), Vec<ColumnInfo>>,
    schema_objects: HashMap<SharedString, SchemaObjects>,
    relation_details: HashMap<(SharedString, SharedString), RelationDetails>,
    loading: HashSet<SchemaRequest>,
    /// Requests that failed. They aren't retried until asked for explicitly, so that a failing
    /// query isn't repeated whenever the schema changes.
    failed: HashMap<SchemaRequest, SharedString>,
    active_runs: Vec<WeakEntity<QueryRun>>,
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self {
            status: ConnectionStatus::Disconnected,
            sessions: None,
            connect_task: None,
            failed_attempts: 0,
            last_failure: None,
            schemas: None,
            relations: HashMap::default(),
            columns: HashMap::default(),
            schema_objects: HashMap::default(),
            relation_details: HashMap::default(),
            loading: HashSet::default(),
            failed: HashMap::default(),
            active_runs: Vec::new(),
        }
    }
}

/// A piece of schema information that is loaded on demand.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SchemaRequest {
    Schemas,
    Relations(SharedString),
    Columns(SharedString, SharedString),
    SchemaObjects(SharedString),
    RelationDetails(SharedString, SharedString),
}

/// Where a query was started from, for telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuerySource {
    Editor,
    Panel,
}

/// The result of a query run on behalf of an AI agent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct CollectedResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    pub truncated: bool,
    pub rows_affected: Option<u64>,
}

pub struct DbStore {
    states: BTreeMap<ConnectionKey, ConnectionState>,
    editor_connections: HashMap<EntityId, ConnectionKey>,
    worktree_connections: HashMap<Arc<Path>, ConnectionKey>,
    history: QueryHistory,
    credentials_provider: Arc<dyn CredentialsProvider>,
}

impl EventEmitter<DbStoreEvent> for DbStore {}

struct GlobalDbStore(Entity<DbStore>);

impl Global for GlobalDbStore {}

impl DbStore {
    pub fn init_global(credentials_provider: Arc<dyn CredentialsProvider>, cx: &mut App) {
        let store = cx.new(|_| Self::new(credentials_provider));
        cx.set_global(GlobalDbStore(store));
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalDbStore>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalDbStore>()
            .map(|store| store.0.clone())
    }

    pub fn new(credentials_provider: Arc<dyn CredentialsProvider>) -> Self {
        Self {
            states: BTreeMap::default(),
            editor_connections: HashMap::default(),
            worktree_connections: HashMap::default(),
            history: QueryHistory::default(),
            credentials_provider,
        }
    }

    /// Connections available to a project: those from user settings, followed by those from
    /// the project's `.zed/settings.json` files. Project files only contribute once the project
    /// is trusted. Collaboration guests get none, since connecting would use the host's
    /// configuration from the guest's machine.
    pub fn connections_for_project(project: &Entity<Project>, cx: &App) -> Vec<ConnectionConfig> {
        let project = project.read(cx);
        if project.is_via_collab() {
            return Vec::new();
        }
        let store = cx.global::<SettingsStore>();
        let mut connections = store
            .merged_settings()
            .project
            .database_connections
            .iter()
            .flatten()
            .map(|(id, content)| {
                ConnectionConfig::from_content(ConnectionKey::user(id.clone()), content)
            })
            .collect::<Vec<_>>();
        connections.sort_by(|a, b| a.key.id.cmp(&b.key.id));

        for worktree in project.visible_worktrees(cx) {
            let worktree = worktree.read(cx);
            let root: Arc<Path> = worktree.abs_path();
            // Settings in nested directories override those closer to the root.
            let mut project_connections = BTreeMap::new();
            for (_, content) in store.local_settings(worktree.id()) {
                for (id, connection) in content.database_connections.iter().flatten() {
                    project_connections.insert(id.clone(), connection.clone());
                }
            }
            connections.extend(project_connections.into_iter().map(|(id, content)| {
                ConnectionConfig::from_content(ConnectionKey::project(id, root.clone()), &content)
            }));
        }
        connections
    }

    pub fn status(&self, key: &ConnectionKey) -> ConnectionStatus {
        self.states
            .get(key)
            .map_or(ConnectionStatus::Disconnected, |state| state.status.clone())
    }

    pub fn is_connected(&self, key: &ConnectionKey) -> bool {
        self.status(key) == ConnectionStatus::Connected
    }

    /// Connects, or returns the open sessions if the connection is already up.
    ///
    /// Without `password`, the password is read from the keychain. If the server requires one
    /// that isn't stored, this fails with [`PasswordRequired`].
    pub fn connect(
        &mut self,
        config: ConnectionConfig,
        project: Option<Entity<Project>>,
        password: Option<PasswordInput>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Arc<Sessions>>> {
        let key = config.key.clone();
        let state = self.states.entry(key.clone()).or_default();
        if password.is_none() {
            if let Some(sessions) = &state.sessions
                && !sessions.is_closed()
            {
                return Task::ready(Ok(sessions.clone()));
            }
            if let Some(task) = &state.connect_task {
                let task = task.clone();
                return cx.background_spawn(async move {
                    task.await.map_err(|error| anyhow!("{error:#}"))
                });
            }
        }

        if let Some(sessions) = state.sessions.take() {
            drop_on_runtime(sessions, cx);
        }
        state.status = ConnectionStatus::Connecting;
        cx.emit(DbStoreEvent::ConnectionChanged(key));
        cx.notify();

        let credentials_provider = self.credentials_provider.clone();
        let task = cx
            .spawn(async move |this, cx| {
                let result =
                    open_sessions(&config, project, password, credentials_provider, cx).await;
                this.update(cx, |this, cx| {
                    let state = this.states.entry(config.key.clone()).or_default();
                    state.connect_task = None;
                    match &result {
                        Ok(sessions) => {
                            state.status = ConnectionStatus::Connected;
                            state.sessions = Some(sessions.clone());
                            state.failed_attempts = 0;
                            state.last_failure = None;
                        }
                        Err(error) => {
                            state.failed_attempts += 1;
                            state.last_failure = Some(Instant::now());
                            state.status = if error.is::<PasswordRequired>() {
                                ConnectionStatus::PasswordRequired
                            } else {
                                ConnectionStatus::Failed(format!("{error:#}").into())
                            };
                        }
                    }
                    telemetry::event!(
                        "Database Connected",
                        driver = config.driver.id(),
                        outcome = if result.is_ok() { "ok" } else { "error" }
                    );
                    cx.emit(DbStoreEvent::ConnectionChanged(config.key.clone()));
                    cx.notify();
                })
                .ok();
                result.map_err(Arc::new)
            })
            .shared();
        state.connect_task = Some(task.clone());
        cx.background_spawn(async move { task.await.map_err(|error| anyhow!("{error:#}")) })
    }

    /// Like [`Self::connect`], but refuses to retry a connection that failed moments ago, with
    /// a delay that doubles after each failure. Used when running queries, so that a server that
    /// is down isn't hammered by repeated attempts.
    pub fn ensure_connected(
        &mut self,
        config: ConnectionConfig,
        project: Option<Entity<Project>>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Arc<Sessions>>> {
        if let Some(state) = self.states.get(&config.key)
            && state.sessions.is_none()
            && state.connect_task.is_none()
        {
            if state.status == ConnectionStatus::PasswordRequired {
                return Task::ready(Err(anyhow!(PasswordRequired { message: None })));
            }
            if let Some(last_failure) = state.last_failure {
                let backoff = Duration::from_secs(1 << state.failed_attempts.min(5))
                    .min(MAX_RECONNECT_BACKOFF);
                if last_failure.elapsed() < backoff {
                    let message = match &state.status {
                        ConnectionStatus::Failed(message) => message.to_string(),
                        _ => "the connection failed".to_string(),
                    };
                    return Task::ready(Err(anyhow!(
                        "{message} (retrying in {}s)",
                        (backoff - last_failure.elapsed()).as_secs().max(1)
                    )));
                }
            }
        }
        self.connect(config, project, None, cx)
    }

    pub fn disconnect(&mut self, key: &ConnectionKey, cx: &mut Context<Self>) {
        let Some(state) = self.states.get_mut(key) else {
            return;
        };
        for run in state.active_runs.drain(..) {
            run.update(cx, |run, cx| run.cancel(cx)).ok();
        }
        state.connect_task = None;
        if let Some(sessions) = state.sessions.take() {
            drop_on_runtime(sessions, cx);
        }
        state.status = ConnectionStatus::Disconnected;
        state.failed_attempts = 0;
        state.last_failure = None;
        state.schemas = None;
        state.relations.clear();
        state.columns.clear();
        state.schema_objects.clear();
        state.relation_details.clear();
        state.failed.clear();
        cx.emit(DbStoreEvent::ConnectionChanged(key.clone()));
        cx.emit(DbStoreEvent::SchemaChanged(key.clone()));
        cx.notify();
    }

    /// Forgets a connection that was removed from settings, along with its stored password.
    pub fn forget(&mut self, key: &ConnectionKey, cx: &mut Context<Self>) -> Task<Result<()>> {
        self.disconnect(key, cx);
        self.states.remove(key);
        self.editor_connections
            .retain(|_, connection| connection != key);
        let worktrees = self
            .worktree_connections
            .iter()
            .filter(|(_, connection)| *connection == key)
            .map(|(root, _)| root.clone())
            .collect::<Vec<_>>();
        for root in worktrees {
            self.set_worktree_connection(root, None, cx);
        }
        let history = self.history.clear(key, cx);
        let credentials_provider = self.credentials_provider.clone();
        let credentials_key = key.credentials_key();
        cx.spawn(async move |_, cx| {
            history.await.log_err();
            credentials_provider
                .delete_credentials(&credentials_key, cx)
                .await
        })
    }

    /// Opens and closes a connection without keeping it, to check its settings. Doesn't store
    /// the password.
    pub fn test_connection(
        &self,
        config: ConnectionConfig,
        project: Option<Entity<Project>>,
        password: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let credentials_provider = self.credentials_provider.clone();
        let password = password.map(|password| PasswordInput {
            password,
            remember: false,
        });
        cx.spawn(async move |_, cx| {
            let sessions =
                open_sessions(&config, project, password, credentials_provider, cx).await?;
            cx.update(|cx| drop_on_runtime(sessions, cx));
            Ok(())
        })
    }

    /// Saves a connection's password in the keychain.
    pub fn store_password(
        &self,
        key: &ConnectionKey,
        username: String,
        password: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let credentials_provider = self.credentials_provider.clone();
        let credentials_key = key.credentials_key();
        cx.spawn(async move |_, cx| {
            credentials_provider
                .write_credentials(&credentials_key, &username, password.as_bytes(), cx)
                .await
        })
    }

    /// Moves a stored password to a connection's new key after it was renamed.
    pub fn move_password(
        &self,
        from: &ConnectionKey,
        to: &ConnectionKey,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let credentials_provider = self.credentials_provider.clone();
        let from = from.credentials_key();
        let to = to.credentials_key();
        cx.spawn(async move |_, cx| {
            if let Some((username, password)) =
                credentials_provider.read_credentials(&from, cx).await?
            {
                credentials_provider
                    .write_credentials(&to, &username, &password, cx)
                    .await?;
                credentials_provider.delete_credentials(&from, cx).await?;
            }
            Ok(())
        })
    }

    pub fn has_stored_password(&self, key: &ConnectionKey, cx: &Context<Self>) -> Task<bool> {
        let credentials_provider = self.credentials_provider.clone();
        let credentials_key = key.credentials_key();
        cx.spawn(async move |_, cx| {
            credentials_provider
                .read_credentials(&credentials_key, cx)
                .await
                .is_ok_and(|credentials| credentials.is_some())
        })
    }

    pub fn schemas(&self, key: &ConnectionKey) -> Option<&[SchemaInfo]> {
        self.states.get(key)?.schemas.as_deref()
    }

    pub fn relations(&self, key: &ConnectionKey, schema: &str) -> Option<&[RelationInfo]> {
        self.states
            .get(key)?
            .relations
            .get(schema)
            .map(Vec::as_slice)
    }

    pub fn columns(
        &self,
        key: &ConnectionKey,
        schema: &str,
        relation: &str,
    ) -> Option<&[ColumnInfo]> {
        self.states
            .get(key)?
            .columns
            .get(&(
                SharedString::from(schema.to_string()),
                SharedString::from(relation.to_string()),
            ))
            .map(Vec::as_slice)
    }

    pub fn schema_objects(&self, key: &ConnectionKey, schema: &str) -> Option<&SchemaObjects> {
        self.states.get(key)?.schema_objects.get(schema)
    }

    pub fn relation_details(
        &self,
        key: &ConnectionKey,
        schema: &str,
        relation: &str,
    ) -> Option<&RelationDetails> {
        self.states.get(key)?.relation_details.get(&(
            SharedString::from(schema.to_string()),
            SharedString::from(relation.to_string()),
        ))
    }

    pub fn is_loading_schema_objects(&self, key: &ConnectionKey, schema: &str) -> bool {
        self.states.get(key).is_some_and(|state| {
            state
                .loading
                .contains(&SchemaRequest::SchemaObjects(schema.to_string().into()))
        })
    }

    pub fn is_loading_relation_details(
        &self,
        key: &ConnectionKey,
        schema: &str,
        relation: &str,
    ) -> bool {
        self.states.get(key).is_some_and(|state| {
            state.loading.contains(&SchemaRequest::RelationDetails(
                schema.to_string().into(),
                relation.to_string().into(),
            ))
        })
    }

    pub fn is_loading(&self, key: &ConnectionKey, request: &SchemaRequest) -> bool {
        self.states
            .get(key)
            .is_some_and(|state| state.loading.contains(request))
    }

    /// Why loading the requested information failed, if it did.
    pub fn load_error(
        &self,
        key: &ConnectionKey,
        request: &SchemaRequest,
    ) -> Option<&SharedString> {
        self.states.get(key)?.failed.get(request)
    }

    pub fn is_loading_schemas(&self, key: &ConnectionKey) -> bool {
        self.states
            .get(key)
            .is_some_and(|state| state.loading.contains(&SchemaRequest::Schemas))
    }

    pub fn is_loading_relations(&self, key: &ConnectionKey, schema: &str) -> bool {
        self.states.get(key).is_some_and(|state| {
            state
                .loading
                .contains(&SchemaRequest::Relations(schema.to_string().into()))
        })
    }

    pub fn is_loading_columns(&self, key: &ConnectionKey, schema: &str, relation: &str) -> bool {
        self.states.get(key).is_some_and(|state| {
            state.loading.contains(&SchemaRequest::Columns(
                schema.to_string().into(),
                relation.to_string().into(),
            ))
        })
    }

    pub fn load_schemas(
        &mut self,
        config: ConnectionConfig,
        project: Option<Entity<Project>>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.load(config, project, SchemaRequest::Schemas, cx)
    }

    pub fn load_relations(
        &mut self,
        config: ConnectionConfig,
        project: Option<Entity<Project>>,
        schema: SharedString,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.load(config, project, SchemaRequest::Relations(schema), cx)
    }

    pub fn load_columns(
        &mut self,
        config: ConnectionConfig,
        project: Option<Entity<Project>>,
        schema: SharedString,
        relation: SharedString,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.load(
            config,
            project,
            SchemaRequest::Columns(schema, relation),
            cx,
        )
    }

    pub fn load_schema_objects(
        &mut self,
        config: ConnectionConfig,
        project: Option<Entity<Project>>,
        schema: SharedString,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.load(config, project, SchemaRequest::SchemaObjects(schema), cx)
    }

    pub fn load_relation_details(
        &mut self,
        config: ConnectionConfig,
        project: Option<Entity<Project>>,
        schema: SharedString,
        relation: SharedString,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.load(
            config,
            project,
            SchemaRequest::RelationDetails(schema, relation),
            cx,
        )
    }

    /// Drops cached schema information, so that it is fetched again when needed.
    pub fn refresh_schema(&mut self, key: &ConnectionKey, cx: &mut Context<Self>) {
        if let Some(state) = self.states.get_mut(key) {
            state.schemas = None;
            state.relations.clear();
            state.columns.clear();
            state.schema_objects.clear();
            state.relation_details.clear();
            state.failed.clear();
            cx.emit(DbStoreEvent::SchemaChanged(key.clone()));
            cx.notify();
        }
    }

    fn load(
        &mut self,
        config: ConnectionConfig,
        project: Option<Entity<Project>>,
        request: SchemaRequest,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let key = config.key.clone();
        let state = self.states.entry(key.clone()).or_default();
        if !state.loading.insert(request.clone()) {
            return Task::ready(Ok(()));
        }
        state.failed.remove(&request);
        cx.emit(DbStoreEvent::SchemaChanged(key.clone()));
        let connect = self.ensure_connected(config, project, cx);
        cx.spawn(async move |this, cx| {
            let result = async {
                let sessions = connect.await?;
                let session = sessions.metadata.clone();
                let sanitize = sessions.sanitize.clone();
                let request = request.clone();
                gpui_tokio::Tokio::spawn_result(cx, async move {
                    let result = match request {
                        SchemaRequest::Schemas => session.list_schemas().await.map(Loaded::Schemas),
                        SchemaRequest::Relations(schema) => session
                            .list_relations(&schema)
                            .await
                            .map(|relations| Loaded::Relations(schema, relations)),
                        SchemaRequest::Columns(schema, relation) => session
                            .list_columns(&schema, &relation)
                            .await
                            .map(|columns| Loaded::Columns(schema, relation, columns)),
                        SchemaRequest::SchemaObjects(schema) => session
                            .list_schema_objects(&schema)
                            .await
                            .map(|objects| Loaded::SchemaObjects(schema, objects)),
                        SchemaRequest::RelationDetails(schema, relation) => session
                            .list_relation_details(&schema, &relation)
                            .await
                            .map(|details| Loaded::RelationDetails(schema, relation, details)),
                    };
                    result.map_err(|error| anyhow!(sanitize(&format!("{error:#}"))))
                })
                .await
            }
            .await;
            this.update(cx, |this, cx| {
                let state = this.states.entry(key.clone()).or_default();
                state.loading.remove(&request);
                match &result {
                    Ok(Loaded::Schemas(schemas)) => state.schemas = Some(schemas.clone()),
                    Ok(Loaded::Relations(schema, relations)) => {
                        state.relations.insert(schema.clone(), relations.clone());
                    }
                    Ok(Loaded::Columns(schema, relation, columns)) => {
                        state
                            .columns
                            .insert((schema.clone(), relation.clone()), columns.clone());
                    }
                    Ok(Loaded::SchemaObjects(schema, objects)) => {
                        state.schema_objects.insert(schema.clone(), objects.clone());
                    }
                    Ok(Loaded::RelationDetails(schema, relation, details)) => {
                        state
                            .relation_details
                            .insert((schema.clone(), relation.clone()), details.clone());
                    }
                    Err(error) => {
                        state
                            .failed
                            .insert(request.clone(), format!("{error:#}").into());
                    }
                }
                this.mark_disconnected_if_closed(&key, cx);
                cx.emit(DbStoreEvent::SchemaChanged(key.clone()));
                cx.notify();
            })?;
            result.map(|_| ())
        })
    }

    /// Notices sessions whose server connection dropped, so the next use reconnects.
    fn mark_disconnected_if_closed(&mut self, key: &ConnectionKey, cx: &mut Context<Self>) {
        let Some(state) = self.states.get_mut(key) else {
            return;
        };
        if state
            .sessions
            .as_ref()
            .is_some_and(|sessions| sessions.is_closed())
        {
            if let Some(sessions) = state.sessions.take() {
                drop_on_runtime(sessions, cx);
            }
            state.status = ConnectionStatus::Failed("the connection was lost".into());
            cx.emit(DbStoreEvent::ConnectionChanged(key.clone()));
        }
    }

    /// The statement that defines a routine, sequence, index or trigger.
    pub fn object_definition(
        &mut self,
        config: ConnectionConfig,
        project: Option<Entity<Project>>,
        schema: SharedString,
        object: ObjectRef,
        cx: &mut Context<Self>,
    ) -> Task<Result<String>> {
        let connect = self.ensure_connected(config, project, cx);
        cx.spawn(async move |_, cx| {
            let sessions = connect.await?;
            let session = sessions.metadata.clone();
            let sanitize = sessions.sanitize.clone();
            gpui_tokio::Tokio::spawn_result(cx, async move {
                session
                    .object_definition(&schema, &object)
                    .await
                    .map_err(|error| anyhow!(sanitize(&format!("{error:#}"))))
            })
            .await
        })
    }

    pub fn relation_ddl(
        &mut self,
        config: ConnectionConfig,
        project: Option<Entity<Project>>,
        schema: SharedString,
        relation: SharedString,
        cx: &mut Context<Self>,
    ) -> Task<Result<String>> {
        let connect = self.ensure_connected(config, project, cx);
        cx.spawn(async move |_, cx| {
            let sessions = connect.await?;
            let session = sessions.metadata.clone();
            let sanitize = sessions.sanitize.clone();
            gpui_tokio::Tokio::spawn_result(cx, async move {
                session
                    .relation_ddl(&schema, &relation)
                    .await
                    .map_err(|error| anyhow!(sanitize(&format!("{error:#}"))))
            })
            .await
        })
    }

    /// Runs SQL on the connection's main session, streaming results into the returned
    /// [`QueryRun`]. Dropping the run cancels the query.
    pub fn execute(
        &mut self,
        config: ConnectionConfig,
        project: Option<Entity<Project>>,
        sql: String,
        source: QuerySource,
        cx: &mut Context<Self>,
    ) -> Entity<QueryRun> {
        let key = config.key.clone();
        let settings = DatabaseSettings::get_global(cx).clone();
        let run = cx.new(|_| QueryRun::new(key.clone(), config.driver, sql.clone().into()));

        // A result paused at its row limit holds the session. Release it, so the new query
        // doesn't wait behind it.
        let state = self.states.entry(key.clone()).or_default();
        state.active_runs.retain(|run| run.upgrade().is_some());
        for previous in &state.active_runs {
            previous
                .update(cx, |previous, cx| previous.release_session(cx))
                .ok();
        }
        state.active_runs.push(run.downgrade());

        self.record_history(&key, &sql, settings.history_size, cx);
        log::debug!("running a {:?} query from {source:?}", config.driver);

        let connect = self.ensure_connected(config, project, cx);
        let weak_run = run.downgrade();
        let task = cx.spawn(async move |this, cx| {
            let sessions = match connect.await {
                Ok(sessions) => sessions,
                Err(error) => {
                    weak_run
                        .update(cx, |run, cx| run.fail(format!("{error:#}"), cx))
                        .ok();
                    return;
                }
            };
            weak_run
                .update(cx, |run, cx| {
                    run.start_streaming(
                        sessions.main.clone(),
                        ExecOptions::default(),
                        settings.row_limit,
                        sessions.sanitize.clone(),
                        cx,
                    )
                })
                .ok();
            this.update(cx, |this, cx| this.mark_disconnected_if_closed(&key, cx))
                .ok();
        });
        run.update(cx, |run, _| run.set_task(task));
        run
    }

    /// Runs a read-only query for an AI agent and collects at most `max_rows` rows and
    /// `max_chars` characters of values.
    ///
    /// The statement runs in a read-only transaction that is rolled back, and input with more
    /// than one statement is refused, so the server guarantees that nothing is written.
    pub fn execute_for_agent(
        &mut self,
        config: ConnectionConfig,
        project: Option<Entity<Project>>,
        sql: String,
        max_rows: usize,
        max_chars: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<CollectedResult>> {
        let connect = self.ensure_connected(config.clone(), project, cx);
        let driver = config.driver;
        cx.spawn(async move |_, cx| {
            let sessions = connect.await?;
            let session = sessions.metadata.clone();
            let sanitize = sessions.sanitize.clone();
            let result = gpui_tokio::Tokio::spawn_result(cx, async move {
                collect_result(session, sql, max_rows, max_chars)
                    .await
                    .map_err(|error| anyhow!(sanitize(&format!("{error:#}"))))
            })
            .await;
            telemetry::event!(
                "Database Query Executed",
                driver = driver.id(),
                outcome = if result.is_ok() { "ok" } else { "error" },
                source = "agent"
            );
            result
        })
    }

    /// Runs statements in one transaction on the connection's main session. Each statement
    /// must change exactly one row; otherwise everything is rolled back, so that edits based on
    /// stale results never apply partially.
    pub fn execute_transaction(
        &mut self,
        config: ConnectionConfig,
        project: Option<Entity<Project>>,
        statements: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let connect = self.ensure_connected(config.clone(), project, cx);
        let key = config.key.clone();
        // A result paused at its row limit holds the session.
        if let Some(state) = self.states.get(&key) {
            for run in &state.active_runs {
                run.update(cx, |run, cx| run.release_session(cx)).ok();
            }
        }
        let driver = config.driver;
        cx.spawn(async move |_, cx| {
            let sessions = connect.await?;
            let session = sessions.main.clone();
            let sanitize = sessions.sanitize.clone();
            let result = gpui_tokio::Tokio::spawn_result(cx, async move {
                run_transaction(session, driver, statements)
                    .await
                    .map_err(|error| anyhow!(sanitize(&format!("{error:#}"))))
            })
            .await;
            telemetry::event!(
                "Database Rows Edited",
                driver = driver.id(),
                outcome = if result.is_ok() { "ok" } else { "error" }
            );
            result
        })
    }

    pub fn history(&self, key: &ConnectionKey) -> impl Iterator<Item = &HistoryEntry> {
        self.history.entries(key)
    }

    /// Loads the persisted history of a connection if it isn't loaded yet.
    pub fn load_history(&mut self, key: &ConnectionKey, cx: &mut Context<Self>) {
        if self.history.is_loaded(key) {
            return;
        }
        let key = key.clone();
        let load = QueryHistory::load(&key, cx);
        cx.spawn(async move |this, cx| {
            let entries = load.await.log_err().unwrap_or_default();
            this.update(cx, |this, cx| {
                let limit = DatabaseSettings::get_global(cx).history_size;
                this.history.set_loaded(key.clone(), entries, limit);
                cx.emit(DbStoreEvent::HistoryChanged(key));
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    pub fn clear_history(&mut self, key: &ConnectionKey, cx: &mut Context<Self>) {
        self.history.clear(key, cx).detach_and_log_err(cx);
        cx.emit(DbStoreEvent::HistoryChanged(key.clone()));
        cx.notify();
    }

    fn record_history(
        &mut self,
        key: &ConnectionKey,
        sql: &str,
        limit: usize,
        cx: &mut Context<Self>,
    ) {
        self.load_history(key, cx);
        let executed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs() as i64);
        self.history
            .record(key, sql, executed_at, limit, cx)
            .detach_and_log_err(cx);
        cx.emit(DbStoreEvent::HistoryChanged(key.clone()));
    }

    /// The connection chosen for an editor, if any.
    pub fn editor_connection(&self, editor: EntityId) -> Option<&ConnectionKey> {
        self.editor_connections.get(&editor)
    }

    pub fn set_editor_connection(
        &mut self,
        editor: EntityId,
        worktree_root: Option<Arc<Path>>,
        connection: ConnectionKey,
        cx: &mut Context<Self>,
    ) {
        self.editor_connections.insert(editor, connection.clone());
        if let Some(root) = worktree_root {
            self.set_worktree_connection(root, Some(connection), cx);
        }
        cx.emit(DbStoreEvent::EditorConnectionChanged(editor));
        cx.notify();
    }

    pub fn forget_editor(&mut self, editor: EntityId) {
        self.editor_connections.remove(&editor);
    }

    /// The connection last chosen for an editor in this worktree.
    pub fn worktree_connection(&self, root: &Path) -> Option<&ConnectionKey> {
        self.worktree_connections.get(root)
    }

    /// Reads the remembered connection of a worktree from the previous session.
    pub fn load_worktree_connection(&mut self, root: Arc<Path>, cx: &mut Context<Self>) {
        if self.worktree_connections.contains_key(&root) {
            return;
        }
        let kvp = KeyValueStore::global(cx);
        let storage_key = format!("{WORKTREE_CONNECTION_KVP_PREFIX}{}", root.display());
        cx.spawn(async move |this, cx| {
            let stored = cx
                .background_spawn(async move { kvp.read_kvp(&storage_key) })
                .await?;
            let Some(stored) = stored else {
                return Ok(());
            };
            let stored: StoredConnectionKey = serde_json::from_str(&stored)?;
            this.update(cx, |this, cx| {
                if !this.worktree_connections.contains_key(&root) {
                    this.worktree_connections
                        .insert(root.clone(), stored.into());
                    cx.emit(DbStoreEvent::WorktreeConnectionChanged(root));
                    cx.notify();
                }
            })
        })
        .detach_and_log_err(cx);
    }

    fn set_worktree_connection(
        &mut self,
        root: Arc<Path>,
        connection: Option<ConnectionKey>,
        cx: &mut Context<Self>,
    ) {
        let kvp = KeyValueStore::global(cx);
        let storage_key = format!("{WORKTREE_CONNECTION_KVP_PREFIX}{}", root.display());
        match connection {
            Some(connection) => {
                let stored = StoredConnectionKey::from(&connection);
                self.worktree_connections.insert(root.clone(), connection);
                cx.emit(DbStoreEvent::WorktreeConnectionChanged(root));
                cx.background_spawn(async move {
                    kvp.write_kvp(storage_key, serde_json::to_string(&stored)?)
                        .await
                })
                .detach_and_log_err(cx);
            }
            None => {
                self.worktree_connections.remove(&root);
                cx.emit(DbStoreEvent::WorktreeConnectionChanged(root.clone()));
                cx.background_spawn(async move { kvp.delete_kvp(storage_key).await })
                    .detach_and_log_err(cx);
            }
        }
    }
}

enum Loaded {
    Schemas(Vec<SchemaInfo>),
    Relations(SharedString, Vec<RelationInfo>),
    Columns(SharedString, SharedString, Vec<ColumnInfo>),
    SchemaObjects(SharedString, SchemaObjects),
    RelationDetails(SharedString, SharedString, RelationDetails),
}

#[derive(Serialize, Deserialize)]
struct StoredConnectionKey {
    id: String,
    project_root: Option<std::path::PathBuf>,
}

impl From<&ConnectionKey> for StoredConnectionKey {
    fn from(key: &ConnectionKey) -> Self {
        Self {
            id: key.id.to_string(),
            project_root: key.project_root.as_deref().map(Path::to_path_buf),
        }
    }
}

impl From<StoredConnectionKey> for ConnectionKey {
    fn from(key: StoredConnectionKey) -> Self {
        Self {
            id: key.id.into(),
            project_root: key.project_root.map(Into::into),
        }
    }
}

/// Sessions must be dropped on the Tokio runtime, where drivers can close them gracefully.
fn drop_on_runtime(sessions: Arc<Sessions>, cx: &App) {
    gpui_tokio::Tokio::spawn(cx, async move { drop(sessions) }).detach();
}

/// How the client reaches the database server.
enum Route {
    Direct,
    /// Through an SSH tunnel; holds candidate commands for different local ports.
    Tunnel(Vec<(u16, CommandTemplate)>),
}

async fn open_sessions(
    config: &ConnectionConfig,
    project: Option<Entity<Project>>,
    password: Option<PasswordInput>,
    credentials_provider: Arc<dyn CredentialsProvider>,
    cx: &mut AsyncApp,
) -> Result<Arc<Sessions>> {
    let credentials_key = config.key.credentials_key();
    let stored_password = if password.is_none() && config.driver.uses_network() {
        credentials_provider
            .read_credentials(&credentials_key, cx)
            .await
            .log_err()
            .flatten()
            .map(|(_, password)| String::from_utf8_lossy(&password).into_owned())
    } else {
        None
    };
    let explicit_password = password.as_ref().map(|input| input.password.clone());

    let worktree_root = project_worktree_root(config, project.as_ref(), cx);
    let environment =
        resolve_environment(config, project.as_ref(), worktree_root.as_deref(), cx).await;
    let session_options = cx.update(|cx| SessionOptions {
        statement_timeout: DatabaseSettings::get_global(cx).query_timeout,
    });
    let resolved = config.resolve(
        &environment,
        worktree_root.as_deref(),
        explicit_password.clone().or(stored_password.clone()),
        &session_options,
    )?;
    let route = route(config, &resolved, project.as_ref(), cx)?;

    let resolved_for_runtime = resolved.clone();
    let result = gpui_tokio::Tokio::spawn_result(cx, async move {
        connect_sessions(resolved_for_runtime, route).await
    })
    .await;

    let had_password = resolved.password.is_some();
    match result {
        Ok(sessions) => {
            if let Some(input) = password
                && input.remember
            {
                credentials_provider
                    .write_credentials(
                        &credentials_key,
                        resolved.username.as_deref().unwrap_or_default(),
                        input.password.as_bytes(),
                        cx,
                    )
                    .await
                    .context("saving the password in the keychain")?;
            }
            Ok(sessions)
        }
        Err(error) => {
            let is_auth_error = match config.driver {
                DriverKind::Postgres => postgres::is_authentication_error(&error),
                DriverKind::Mysql => mysql::is_authentication_error(&error),
                DriverKind::Sqlite => false,
            };
            let message = resolved.sanitize_message(&format!("{error:#}"));
            if is_auth_error {
                Err(anyhow!(PasswordRequired {
                    message: had_password.then_some(message),
                }))
            } else {
                Err(anyhow!(message))
            }
        }
    }
}

/// The worktree that anchors a connection: the one that defines it, or the project's first.
fn project_worktree_root(
    config: &ConnectionConfig,
    project: Option<&Entity<Project>>,
    cx: &AsyncApp,
) -> Option<Arc<Path>> {
    if let Some(root) = &config.key.project_root {
        return Some(root.clone());
    }
    let project = project?;
    cx.update(|cx| {
        project
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path())
    })
}

/// The variables available to `${VAR}` references.
///
/// The project environment (including direnv) is only loaded for trusted worktrees, because
/// loading it runs the user's shell and `.envrc` in the worktree. Otherwise, Zed's own process
/// environment is used.
async fn resolve_environment(
    config: &ConnectionConfig,
    project: Option<&Entity<Project>>,
    worktree_root: Option<&Path>,
    cx: &mut AsyncApp,
) -> HashMap<String, String> {
    let process_environment = || std::env::vars().collect::<HashMap<_, _>>();
    if !config.uses_variables() {
        return HashMap::default();
    }
    let (Some(project), Some(root)) = (project, worktree_root) else {
        return process_environment();
    };
    let environment = cx.update(|cx| {
        let is_local = project.read(cx).is_local();
        let worktree_store = project.read(cx).worktree_store();
        let worktree_id = project
            .read(cx)
            .visible_worktrees(cx)
            .find(|worktree| worktree.read(cx).abs_path().as_ref() == root)
            .map(|worktree| worktree.read(cx).id())?;
        let trusted = TrustedWorktrees::try_get_global(cx).is_none_or(|trusted| {
            trusted.update(cx, |trusted, cx| {
                trusted.can_trust(&worktree_store, worktree_id, cx)
            })
        });
        if !trusted || !is_local {
            // Remote environments describe the remote machine, while the connection is made
            // from this one.
            return None;
        }
        Some(project.update(cx, |project, cx| {
            project.environment().update(cx, |environment, cx| {
                environment.directory_environment(root.into(), cx)
            })
        }))
    });
    match environment {
        Some(environment) => environment
            .await
            .map(|environment| environment.into_iter().collect())
            .unwrap_or_else(process_environment),
        None => process_environment(),
    }
}

fn route(
    config: &ConnectionConfig,
    resolved: &ResolvedConnection,
    project: Option<&Entity<Project>>,
    cx: &AsyncApp,
) -> Result<Route> {
    if !config.driver.uses_network() {
        if config.key.is_from_project()
            && project.is_some_and(|project| cx.update(|cx| !project.read(cx).is_local()))
        {
            anyhow::bail!(
                "SQLite files of remote projects can't be opened yet; copy the file locally"
            );
        }
        return Ok(Route::Direct);
    }
    let host = resolved.host.clone();
    let port = resolved.port;
    if let Some(ssh) = &resolved.ssh {
        let candidates = candidate_ports()?
            .into_iter()
            .map(|local_port| {
                (
                    local_port,
                    ssh_tunnel::ssh_command(ssh, local_port, &host, port),
                )
            })
            .collect();
        return Ok(Route::Tunnel(candidates));
    }
    // Connections defined in a remote project's settings point at the remote machine's network.
    if config.key.is_from_project()
        && let Some(project) = project
        && let Some(remote_client) = cx.update(|cx| project.read(cx).remote_client())
    {
        return cx.update(|cx| {
            let remote_client = remote_client.read(cx);
            if remote_client.shares_network_interface() {
                return Ok(Route::Direct);
            }
            let candidates = candidate_ports()?
                .into_iter()
                .map(|local_port| {
                    remote_client
                        .build_forward_ports_command(vec![(local_port, host.clone(), port)])
                        .map(|command| (local_port, command))
                        .context("forwarding a port through the remote connection")
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(Route::Tunnel(candidates))
        });
    }
    Ok(Route::Direct)
}

fn candidate_ports() -> Result<Vec<u16>> {
    (0..3)
        .map(|_| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            Ok(listener.local_addr()?.port())
        })
        .collect()
}

/// Opens the tunnel (if any) and both sessions. Runs on the Tokio runtime.
async fn connect_sessions(resolved: ResolvedConnection, route: Route) -> Result<Arc<Sessions>> {
    let (tunnel, host, port) = match route {
        Route::Direct => (None, resolved.host.clone(), resolved.port),
        Route::Tunnel(candidates) => {
            let tunnel = ssh_tunnel::open(candidates).await?;
            let port = tunnel.local_port;
            (Some(tunnel), "127.0.0.1".to_string(), port)
        }
    };

    let (main, metadata): (Arc<dyn DatabaseSession>, Arc<dyn DatabaseSession>) = match resolved
        .driver
    {
        DriverKind::Postgres => {
            let (main, metadata) = futures::future::try_join(
                postgres::connect(&resolved, &host, port),
                postgres::connect(&resolved, &host, port),
            )
            .await?;
            (Arc::new(main), Arc::new(metadata))
        }
        DriverKind::Mysql => {
            let (main, metadata) = futures::future::try_join(
                mysql::connect(&resolved, &host, port),
                mysql::connect(&resolved, &host, port),
            )
            .await?;
            (Arc::new(main), Arc::new(metadata))
        }
        DriverKind::Sqlite => {
            let (main, metadata) =
                futures::future::try_join(sqlite::connect(&resolved), sqlite::connect(&resolved))
                    .await?;
            (Arc::new(main), Arc::new(metadata))
        }
    };

    let sanitizer = resolved.clone();
    Ok(Arc::new(Sessions {
        main,
        metadata,
        sanitize: Arc::new(move |message| sanitizer.sanitize_message(message)),
        _tunnel: tunnel,
    }))
}

/// Runs a statement in a read-only transaction and collects a bounded result. Runs on the Tokio
/// runtime.
async fn collect_result(
    session: Arc<dyn DatabaseSession>,
    sql: String,
    max_rows: usize,
    max_chars: usize,
) -> Result<CollectedResult> {
    let mut stream = session.execute(
        sql,
        ExecOptions {
            read_only_transaction: true,
        },
    );
    let mut result = CollectedResult::default();
    let mut chars = 0;
    while let Some(event) = stream.next().await {
        match event? {
            ResultEvent::Columns(columns) => {
                result.columns = columns
                    .iter()
                    .map(|ColumnMeta { name, .. }| name.to_string())
                    .collect();
                result.rows.clear();
                chars = 0;
            }
            ResultEvent::Rows(rows) => {
                for row in rows {
                    let row_chars = row_chars(&row);
                    if result.rows.len() >= max_rows || chars + row_chars > max_chars {
                        // Dropping the stream cancels the rest of the query.
                        result.truncated = true;
                        return Ok(result);
                    }
                    chars += row_chars;
                    result.rows.push(
                        row.into_iter()
                            .map(|value| value.map(|value| value.to_string()))
                            .collect(),
                    );
                }
            }
            ResultEvent::StatementComplete { rows_affected } => {
                result.rows_affected = rows_affected;
            }
        }
    }
    Ok(result)
}

/// Runs a statement to completion and returns the number of rows it changed.
async fn run_to_completion(session: &Arc<dyn DatabaseSession>, sql: String) -> Result<Option<u64>> {
    let mut stream = session.execute(sql, ExecOptions::default());
    let mut rows_affected = None;
    while let Some(event) = stream.next().await {
        if let ResultEvent::StatementComplete {
            rows_affected: Some(rows),
        } = event?
        {
            rows_affected = Some(rows);
        }
    }
    Ok(rows_affected)
}

async fn run_transaction(
    session: Arc<dyn DatabaseSession>,
    driver: DriverKind,
    statements: Vec<String>,
) -> Result<()> {
    let begin = match driver {
        DriverKind::Mysql => "START TRANSACTION",
        DriverKind::Postgres | DriverKind::Sqlite => "BEGIN",
    };
    run_to_completion(&session, begin.into()).await?;
    let result = async {
        for statement in statements {
            let rows = run_to_completion(&session, statement.clone()).await?;
            if rows != Some(1) {
                anyhow::bail!(
                    "expected `{}` to change one row, but it changed {}; the row may have been modified or deleted since it was loaded",
                    crate::statement::summary(&statement, 80),
                    rows.unwrap_or(0)
                );
            }
        }
        run_to_completion(&session, "COMMIT".into()).await?;
        anyhow::Ok(())
    }
    .await;
    if result.is_err() {
        run_to_completion(&session, "ROLLBACK".into())
            .await
            .log_err();
    }
    result
}

fn row_chars(row: &ResultRow) -> usize {
    row.iter()
        .map(|value| value.as_ref().map_or(4, |value| value.chars().count()))
        .sum()
}
