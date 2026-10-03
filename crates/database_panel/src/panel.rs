use std::{
    collections::HashSet,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context as _;
use database_core::{
    ColumnInfo, ConnectionConfig, ConnectionKey, ConnectionStatus, DatabaseEnvironment,
    DatabaseSettings, DbStore, DbStoreEvent, QuerySource, RelationInfo, qualified_name,
};
use db::kvp::KeyValueStore;
use editor::{Editor, EditorEvent};
use fs::Fs;
use gpui::{
    Action, AnyElement, App, AsyncWindowContext, ClickEvent, ClipboardItem, Context, DismissEvent,
    Entity, EventEmitter, FocusHandle, Focusable, MouseDownEvent, Pixels, Point, PromptLevel,
    SharedString, Subscription, Task, UniformListScrollHandle, WeakEntity, actions, anchored,
    deferred, uniform_list,
};
use project::Project;
use serde::{Deserialize, Serialize};
use settings::{DockSide, Settings as _, SettingsStore};
use ui::{
    Button, ButtonStyle, Color, ContextMenu, Icon, IconButton, IconName, IconSize, Indicator,
    Label, LabelSize, ListItem, ListItemSpacing, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{
    NewConnection, NewSqlFile, QueryHistory, RefreshSchema, ToggleFocus,
    connection_modal::{ConnectionModal, connect_interactively, detach_and_notify_err},
    results::{QueryRequest, ResultOrigin, open_text_in_editor, run_query},
};

const DATABASE_PANEL_KEY: &str = "database_panel";
const SERIALIZATION_KEY_PREFIX: &str = "DatabasePanel";
const ROW_PREVIEW_LIMIT: usize = 100;

actions!(
    database_panel,
    [
        /// Connects the selected connection.
        Connect,
        /// Disconnects the selected connection.
        Disconnect,
        /// Shows the first rows of the selected table or view.
        ShowRows,
        /// Copies the definition of the selected table or view.
        CopyDdl,
        /// Copies the name of the selected item.
        CopyName,
        /// Edits the selected connection.
        EditConnection,
        /// Deletes the selected connection from settings.
        DeleteConnection,
    ]
);

/// A node of the tree, identified across sessions.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
enum NodeId {
    Connection(StoredKey),
    Schema(StoredKey, String),
    Relation(StoredKey, String, String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct StoredKey {
    id: String,
    project_root: Option<PathBuf>,
}

impl From<&ConnectionKey> for StoredKey {
    fn from(key: &ConnectionKey) -> Self {
        Self {
            id: key.id.to_string(),
            project_root: key.project_root.as_deref().map(Path::to_path_buf),
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
struct SerializedDatabasePanel {
    active: Option<bool>,
    #[serde(default)]
    expanded: Vec<NodeId>,
}

#[derive(Clone, Debug)]
enum EntryKind {
    Connection,
    Schema(SharedString),
    Relation(SharedString, RelationInfo),
    Column(ColumnInfo),
    Message(SharedString, Color),
}

#[derive(Clone, Debug)]
struct Entry {
    depth: usize,
    connection: usize,
    kind: EntryKind,
}

impl Entry {
    fn node_id(&self, connections: &[ConnectionConfig]) -> Option<NodeId> {
        let key = StoredKey::from(&connections.get(self.connection)?.key);
        Some(match &self.kind {
            EntryKind::Connection => NodeId::Connection(key),
            EntryKind::Schema(schema) => NodeId::Schema(key, schema.to_string()),
            EntryKind::Relation(schema, relation) => {
                NodeId::Relation(key, schema.to_string(), relation.name.to_string())
            }
            EntryKind::Column(_) | EntryKind::Message(..) => return None,
        })
    }
}

pub struct DatabasePanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    filter_editor: Entity<Editor>,
    connections: Vec<ConnectionConfig>,
    entries: Vec<Entry>,
    expanded: HashSet<NodeId>,
    selected: Option<usize>,
    active: bool,
    scroll_handle: UniformListScrollHandle,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    pending_serialization: Task<Option<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PanelEvent> for DatabasePanel {}

impl DatabasePanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        let serialized = match workspace
            .read_with(&cx, |workspace, _| Self::serialization_key(workspace))
            .ok()
            .flatten()
        {
            Some(key) => {
                let kvp = cx.update(|_, cx| KeyValueStore::global(cx))?;
                cx.background_spawn(async move { kvp.read_kvp(&key) })
                    .await
                    .context("loading the database panel")
                    .log_err()
                    .flatten()
                    .map(|panel| serde_json::from_str::<SerializedDatabasePanel>(&panel))
                    .transpose()
                    .log_err()
                    .flatten()
            }
            None => None,
        };
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = Self::new(workspace, serialized.unwrap_or_default(), window, cx);
            panel.update(cx, |_, cx| cx.notify());
            panel
        })
    }

    fn new(
        workspace: &mut Workspace,
        serialized: SerializedDatabasePanel,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let fs = workspace.app_state().fs.clone();
        let workspace_handle = cx.entity().downgrade();
        cx.new(|cx| {
            let filter_editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Filter…", window, cx);
                editor
            });
            let store = DbStore::global(cx);
            let subscriptions = vec![
                cx.subscribe(
                    &filter_editor,
                    |this: &mut Self, _, event: &EditorEvent, cx| {
                        if let EditorEvent::BufferEdited = event {
                            this.update_entries(cx);
                        }
                    },
                ),
                cx.subscribe_in(&store, window, Self::on_store_event),
                cx.observe_global_in::<SettingsStore>(window, |this, _, cx| {
                    this.update_entries(cx);
                }),
                cx.observe(&project, |this, _, cx| this.update_entries(cx)),
            ];
            let mut this = Self {
                workspace: workspace_handle,
                project,
                fs,
                focus_handle: cx.focus_handle(),
                filter_editor,
                connections: Vec::new(),
                entries: Vec::new(),
                expanded: serialized.expanded.into_iter().collect(),
                selected: None,
                active: serialized.active.unwrap_or(false),
                scroll_handle: UniformListScrollHandle::new(),
                context_menu: None,
                pending_serialization: Task::ready(None),
                _subscriptions: subscriptions,
            };
            this.update_entries(cx);
            this
        })
    }

    fn serialization_key(workspace: &Workspace) -> Option<String> {
        workspace
            .database_id()
            .map(|id| i64::from(id).to_string())
            .or(workspace.session_id())
            .map(|id| format!("{SERIALIZATION_KEY_PREFIX}-{id}"))
    }

    fn serialize(&mut self, cx: &mut Context<Self>) {
        let serialized = SerializedDatabasePanel {
            active: self.active.then_some(true),
            expanded: self.expanded.iter().cloned().collect(),
        };
        let workspace = self.workspace.clone();
        let kvp = KeyValueStore::global(cx);
        // The workspace is read later because the panel is also serialized while the workspace
        // is being updated, such as when the dock activates the panel.
        self.pending_serialization = cx.spawn(async move |_, cx| {
            async move {
                let Some(key) =
                    workspace.read_with(cx, |workspace, _| Self::serialization_key(workspace))?
                else {
                    return Ok(());
                };
                kvp.write_kvp(key, serde_json::to_string(&serialized)?)
                    .await?;
                anyhow::Ok(())
            }
            .await
            .log_err()
        });
    }

    fn on_store_event(
        &mut self,
        _: &Entity<DbStore>,
        event: &DbStoreEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            DbStoreEvent::ConnectionChanged(key) => {
                // Restore the expanded nodes of a connection once it is connected.
                if DbStore::global(cx).read(cx).is_connected(key)
                    && self
                        .expanded
                        .contains(&NodeId::Connection(StoredKey::from(key)))
                    && let Some(config) = self.config(key)
                {
                    self.load_expanded(config, cx);
                }
                self.update_entries(cx);
            }
            DbStoreEvent::SchemaChanged(key) => {
                if let Some(config) = self.config(key) {
                    self.load_expanded(config, cx);
                }
                self.update_entries(cx);
            }
            DbStoreEvent::HistoryChanged(_)
            | DbStoreEvent::EditorConnectionChanged(_)
            | DbStoreEvent::WorktreeConnectionChanged(_) => {}
        }
    }

    fn config(&self, key: &ConnectionKey) -> Option<ConnectionConfig> {
        self.connections
            .iter()
            .find(|connection| &connection.key == key)
            .cloned()
    }

    /// Loads whatever expanded nodes of a connected connection haven't been loaded yet.
    fn load_expanded(&self, config: ConnectionConfig, cx: &mut Context<Self>) {
        let store = DbStore::global(cx);
        if !store.read(cx).is_connected(&config.key) {
            return;
        }
        let key = StoredKey::from(&config.key);
        let project = Some(self.project.clone());
        let mut tasks = Vec::new();
        store.update(cx, |store, cx| {
            if store.schemas(&config.key).is_none() && !store.is_loading_schemas(&config.key) {
                tasks.push(store.load_schemas(config.clone(), project.clone(), cx));
                return;
            }
            for node in &self.expanded {
                match node {
                    NodeId::Schema(node_key, schema) if *node_key == key => {
                        if store.relations(&config.key, schema).is_none()
                            && !store.is_loading_relations(&config.key, schema)
                        {
                            tasks.push(store.load_relations(
                                config.clone(),
                                project.clone(),
                                schema.clone().into(),
                                cx,
                            ));
                        }
                    }
                    NodeId::Relation(node_key, schema, relation) if *node_key == key => {
                        if store.relations(&config.key, schema).is_some()
                            && store.columns(&config.key, schema, relation).is_none()
                            && !store.is_loading_columns(&config.key, schema, relation)
                        {
                            tasks.push(store.load_columns(
                                config.clone(),
                                project.clone(),
                                schema.clone().into(),
                                relation.clone().into(),
                                cx,
                            ));
                        }
                    }
                    _ => {}
                }
            }
        });
        for task in tasks {
            task.detach_and_log_err(cx);
        }
    }

    fn update_entries(&mut self, cx: &mut Context<Self>) {
        let selected_node = self
            .selected
            .and_then(|index| self.entries.get(index))
            .and_then(|entry| entry.node_id(&self.connections));
        self.connections = DbStore::connections_for_project(&self.project, cx);
        let query = self.filter_editor.read(cx).text(cx).trim().to_lowercase();
        let store = DbStore::global(cx);
        let store = store.read(cx);
        let mut entries = Vec::new();

        for (connection_index, config) in self.connections.iter().enumerate() {
            let key = StoredKey::from(&config.key);
            let expanded = self.expanded.contains(&NodeId::Connection(key.clone()));
            let mut children = Vec::new();
            let mut child_matches = false;
            let connection_matches = matches_filter(&config.key.id, &query);

            if expanded || !query.is_empty() {
                match store.status(&config.key) {
                    ConnectionStatus::Connecting => {
                        children.push((1, EntryKind::Message("Connecting…".into(), Color::Muted)));
                    }
                    ConnectionStatus::Failed(message) => {
                        children.push((1, EntryKind::Message(message, Color::Error)));
                    }
                    ConnectionStatus::PasswordRequired => {
                        children.push((
                            1,
                            EntryKind::Message(
                                "A password is required. Connect to enter it.".into(),
                                Color::Warning,
                            ),
                        ));
                    }
                    ConnectionStatus::Disconnected => {}
                    ConnectionStatus::Connected => match store.schemas(&config.key) {
                        None => {
                            children.push((1, EntryKind::Message("Loading…".into(), Color::Muted)))
                        }
                        Some(schemas) => {
                            for schema in schemas {
                                let schema_node =
                                    NodeId::Schema(key.clone(), schema.name.to_string());
                                let schema_expanded = self.expanded.contains(&schema_node);
                                let mut schema_children = Vec::new();
                                let mut schema_child_matches = false;
                                if schema_expanded || !query.is_empty() {
                                    match store.relations(&config.key, &schema.name) {
                                        None if schema_expanded => schema_children.push((
                                            2,
                                            EntryKind::Message("Loading…".into(), Color::Muted),
                                        )),
                                        None => {}
                                        Some(relations) => {
                                            for relation in relations {
                                                let relation_node = NodeId::Relation(
                                                    key.clone(),
                                                    schema.name.to_string(),
                                                    relation.name.to_string(),
                                                );
                                                let relation_expanded =
                                                    self.expanded.contains(&relation_node);
                                                let columns = store.columns(
                                                    &config.key,
                                                    &schema.name,
                                                    &relation.name,
                                                );
                                                let matching_columns = columns
                                                    .into_iter()
                                                    .flatten()
                                                    .filter(|column| {
                                                        !query.is_empty()
                                                            && matches_filter(&column.name, &query)
                                                    })
                                                    .count();
                                                let relation_matches =
                                                    matches_filter(&relation.name, &query);
                                                if !query.is_empty()
                                                    && !relation_matches
                                                    && matching_columns == 0
                                                    && !connection_matches
                                                {
                                                    continue;
                                                }
                                                schema_child_matches = true;
                                                schema_children.push((
                                                    2,
                                                    EntryKind::Relation(
                                                        schema.name.clone(),
                                                        relation.clone(),
                                                    ),
                                                ));
                                                if relation_expanded || matching_columns > 0 {
                                                    match columns {
                                                        None => schema_children.push((
                                                            3,
                                                            EntryKind::Message(
                                                                "Loading…".into(),
                                                                Color::Muted,
                                                            ),
                                                        )),
                                                        Some(columns) => {
                                                            let ancestor_matches = relation_matches
                                                                || matches_filter(
                                                                    &schema.name,
                                                                    &query,
                                                                )
                                                                || connection_matches;
                                                            for column in columns {
                                                                if !query.is_empty()
                                                                    && !ancestor_matches
                                                                    && !matches_filter(
                                                                        &column.name,
                                                                        &query,
                                                                    )
                                                                {
                                                                    continue;
                                                                }
                                                                schema_children.push((
                                                                    3,
                                                                    EntryKind::Column(
                                                                        column.clone(),
                                                                    ),
                                                                ));
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                let schema_matches = matches_filter(&schema.name, &query);
                                if !query.is_empty()
                                    && !schema_matches
                                    && !schema_child_matches
                                    && !connection_matches
                                {
                                    continue;
                                }
                                child_matches = true;
                                children.push((1, EntryKind::Schema(schema.name.clone())));
                                if schema_expanded || schema_child_matches {
                                    children.extend(schema_children);
                                }
                            }
                        }
                    },
                }
            }

            if !query.is_empty() && !connection_matches && !child_matches {
                continue;
            }
            entries.push(Entry {
                depth: 0,
                connection: connection_index,
                kind: EntryKind::Connection,
            });
            if expanded || child_matches {
                entries.extend(children.into_iter().map(|(depth, kind)| Entry {
                    depth,
                    connection: connection_index,
                    kind,
                }));
            }
        }

        self.entries = entries;
        self.selected = selected_node.and_then(|node| {
            self.entries
                .iter()
                .position(|entry| entry.node_id(&self.connections).as_ref() == Some(&node))
        });
        cx.notify();
    }

    fn toggle_expanded(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(index).cloned() else {
            return;
        };
        let Some(node) = entry.node_id(&self.connections) else {
            return;
        };
        if self.expanded.remove(&node) {
            self.serialize(cx);
            self.update_entries(cx);
            return;
        }
        self.expand(&entry, window, cx);
    }

    fn expand(&mut self, entry: &Entry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = entry.node_id(&self.connections) else {
            return;
        };
        let Some(config) = self.connections.get(entry.connection).cloned() else {
            return;
        };
        self.expanded.insert(node);
        self.serialize(cx);
        let store = DbStore::global(cx);
        let project = Some(self.project.clone());
        match &entry.kind {
            EntryKind::Connection => {
                if store.read(cx).is_connected(&config.key) {
                    self.load_expanded(config, cx);
                } else {
                    let connect = connect_interactively(
                        self.workspace.clone(),
                        config,
                        self.project.clone(),
                        window,
                        cx,
                    );
                    cx.spawn(async move |_, _| connect.await.map(|_| ()))
                        .detach_and_log_err(cx);
                }
            }
            EntryKind::Schema(schema) => {
                if store.read(cx).relations(&config.key, schema).is_none() {
                    store
                        .update(cx, |store, cx| {
                            store.load_relations(config, project, schema.clone(), cx)
                        })
                        .detach_and_log_err(cx);
                }
            }
            EntryKind::Relation(schema, relation) => {
                if store
                    .read(cx)
                    .columns(&config.key, schema, &relation.name)
                    .is_none()
                {
                    store
                        .update(cx, |store, cx| {
                            store.load_columns(
                                config,
                                project,
                                schema.clone(),
                                relation.name.clone(),
                                cx,
                            )
                        })
                        .detach_and_log_err(cx);
                }
            }
            EntryKind::Column(_) | EntryKind::Message(..) => {}
        }
        self.update_entries(cx);
    }

    fn connection_tooltip(&self, config: &ConnectionConfig, cx: &App) -> SharedString {
        let worktree_root = config.key.project_root.clone().or_else(|| {
            self.project
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).abs_path())
        });
        let target = config
            .sqlite_file_path(worktree_root.as_deref())
            .map(SharedString::from)
            .unwrap_or_else(|| config.display_target());
        let mut tooltip = format!("{} · {}", config.driver.display_name(), target);
        if config.key.is_from_project() {
            tooltip.push_str(" · from project settings");
        }
        if config.has_password_in_settings() {
            tooltip.push_str(
                "\nThe connection URL contains a password. Settings files may be shared, \
                 so prefer the keychain or an environment variable.",
            );
        }
        tooltip.into()
    }

    fn is_expandable(entry: &Entry) -> bool {
        matches!(
            entry.kind,
            EntryKind::Connection | EntryKind::Schema(_) | EntryKind::Relation(..)
        )
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if self.entries.is_empty() {
            return;
        }
        let next = self
            .selected
            .map_or(0, |index| (index + 1).min(self.entries.len() - 1));
        self.select(next, cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.entries.is_empty() {
            return;
        }
        let previous = self.selected.map_or(0, |index| index.saturating_sub(1));
        self.select(previous, cx);
    }

    fn select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        if !self.entries.is_empty() {
            self.select(0, cx);
        }
    }

    fn select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        if !self.entries.is_empty() {
            self.select(self.entries.len() - 1, cx);
        }
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected = Some(index);
        self.scroll_handle
            .scroll_to_item(index, gpui::ScrollStrategy::Center);
        cx.notify();
    }

    pub(crate) fn expand_selected(
        &mut self,
        _: &menu::SelectChild,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .selected
            .and_then(|index| self.entries.get(index))
            .cloned()
        else {
            return;
        };
        let is_expanded = entry
            .node_id(&self.connections)
            .is_some_and(|node| self.expanded.contains(&node));
        if Self::is_expandable(&entry) && !is_expanded {
            self.expand(&entry, window, cx);
        } else if let Some(index) = self.selected
            && self
                .entries
                .get(index + 1)
                .is_some_and(|next| next.depth > entry.depth)
        {
            self.select(index + 1, cx);
        }
    }

    fn collapse_selected(
        &mut self,
        _: &menu::SelectParent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.selected else {
            return;
        };
        let Some(entry) = self.entries.get(index).cloned() else {
            return;
        };
        if let Some(node) = entry.node_id(&self.connections)
            && self.expanded.remove(&node)
        {
            self.serialize(cx);
            self.update_entries(cx);
            return;
        }
        // Move to the parent.
        if let Some(parent) = self.entries[..index]
            .iter()
            .rposition(|candidate| candidate.depth < entry.depth)
        {
            self.select(parent, cx);
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected else {
            return;
        };
        match self.entries.get(index).map(|entry| entry.kind.clone()) {
            Some(EntryKind::Relation(..)) => self.show_rows(&ShowRows, window, cx),
            Some(_) => self.toggle_expanded(index, window, cx),
            None => {}
        }
    }

    fn selected_config(&self) -> Option<ConnectionConfig> {
        let entry = self.entries.get(self.selected?)?;
        self.connections.get(entry.connection).cloned()
    }

    fn selected_relation(&self) -> Option<(ConnectionConfig, SharedString, RelationInfo)> {
        let entry = self.entries.get(self.selected?)?;
        let EntryKind::Relation(schema, relation) = &entry.kind else {
            return None;
        };
        Some((
            self.connections.get(entry.connection)?.clone(),
            schema.clone(),
            relation.clone(),
        ))
    }

    fn connect(&mut self, _: &Connect, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.selected_config() else {
            return;
        };
        let node = NodeId::Connection(StoredKey::from(&config.key));
        self.expanded.insert(node);
        self.serialize(cx);
        connect_interactively(
            self.workspace.clone(),
            config,
            self.project.clone(),
            window,
            cx,
        )
        .detach_and_log_err(cx);
        self.update_entries(cx);
    }

    fn disconnect(&mut self, _: &Disconnect, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(config) = self.selected_config() {
            DbStore::global(cx).update(cx, |store, cx| store.disconnect(&config.key, cx));
        }
    }

    fn refresh(&mut self, _: &RefreshSchema, _: &mut Window, cx: &mut Context<Self>) {
        let configs = match self.selected_config() {
            Some(config) => vec![config],
            None => self.connections.clone(),
        };
        let store = DbStore::global(cx);
        for config in configs {
            store.update(cx, |store, cx| store.refresh_schema(&config.key, cx));
            self.load_expanded(config, cx);
        }
    }

    fn new_connection(&mut self, _: &NewConnection, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                ConnectionModal::toggle(workspace, None, window, cx)
            })
            .log_err();
    }

    fn edit_connection(&mut self, _: &EditConnection, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.selected_config() else {
            return;
        };
        if config.key.is_from_project() {
            self.open_project_settings(window, cx);
            return;
        }
        self.workspace
            .update(cx, |workspace, cx| {
                ConnectionModal::toggle(workspace, Some(config), window, cx)
            })
            .log_err();
    }

    fn open_project_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.dispatch_action(zed_actions::OpenProjectSettings.boxed_clone(), cx);
    }

    fn delete_connection(
        &mut self,
        _: &DeleteConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(config) = self.selected_config() else {
            return;
        };
        if config.key.is_from_project() {
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete the connection `{}`?", config.key.id),
            Some("It is removed from your settings, along with its saved password and query history."),
            &["Delete", "Cancel"],
            cx,
        );
        let fs = self.fs.clone();
        cx.spawn(async move |_, cx| {
            if answer.await != Ok(0) {
                return anyhow::Ok(());
            }
            let id = config.key.id.clone();
            cx.update(|cx| {
                settings::update_settings_file(fs, cx, move |settings, _| {
                    if let Some(connections) = settings.project.database_connections.as_mut() {
                        connections.remove(&id);
                    }
                });
                DbStore::global(cx).update(cx, |store, cx| store.forget(&config.key, cx))
            })
            .await?;
            telemetry::event!("Database Connection Deleted", driver = config.driver.id());
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn new_sql_file(&mut self, _: &NewSqlFile, window: &mut Window, cx: &mut Context<Self>) {
        let connection = self.selected_config().map(|config| config.key);
        open_text_in_editor(
            self.workspace.clone(),
            self.project.clone(),
            String::new(),
            "SQL",
            connection,
            window,
            cx,
        );
    }

    fn query_history(&mut self, _: &QueryHistory, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.selected_config() else {
            return;
        };
        let project = self.project.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                crate::history::HistoryPicker::toggle_in_workspace(
                    workspace, config, project, None, window, cx,
                )
            })
            .log_err();
    }

    pub(crate) fn show_rows(&mut self, _: &ShowRows, window: &mut Window, cx: &mut Context<Self>) {
        let Some((config, schema, relation)) = self.selected_relation() else {
            return;
        };
        let sql = format!(
            "SELECT * FROM {} LIMIT {ROW_PREVIEW_LIMIT}",
            qualified_name(config.driver, &schema, &relation.name)
        );
        let request = QueryRequest {
            origin: ResultOrigin::Relation {
                connection: config.key.clone(),
                schema,
                relation: relation.name,
            },
            config,
            project: self.project.clone(),
            sql,
            source: QuerySource::Panel,
            focus: true,
            statement: None,
        };
        self.workspace
            .update(cx, |workspace, cx| {
                run_query(workspace, request, window, cx)
            })
            .log_err();
    }

    fn copy_ddl(&mut self, _: &CopyDdl, window: &mut Window, cx: &mut Context<Self>) {
        let Some((config, schema, relation)) = self.selected_relation() else {
            return;
        };
        let project = self.project.clone();
        let connect = connect_interactively(
            self.workspace.clone(),
            config.clone(),
            project.clone(),
            window,
            cx,
        );
        let task = cx.spawn(async move |_, cx| {
            connect.await?;
            let ddl = cx
                .update(|cx| {
                    DbStore::global(cx).update(cx, |store, cx| {
                        store.relation_ddl(config, Some(project), schema, relation.name, cx)
                    })
                })
                .await?;
            cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string(ddl)));
            anyhow::Ok(())
        });
        detach_and_notify_err(task, self.workspace.clone(), cx);
    }

    pub(crate) fn copy_name(&mut self, _: &CopyName, _: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected.and_then(|index| self.entries.get(index)) else {
            return;
        };
        let Some(config) = self.connections.get(entry.connection) else {
            return;
        };
        let name = match &entry.kind {
            EntryKind::Connection => config.key.id.to_string(),
            EntryKind::Schema(schema) => schema.to_string(),
            EntryKind::Relation(schema, relation) => {
                qualified_name(config.driver, schema, &relation.name)
            }
            EntryKind::Column(column) => column.name.to_string(),
            EntryKind::Message(..) => return,
        };
        cx.write_to_clipboard(ClipboardItem::new_string(name));
    }

    fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entries.get(index).cloned() else {
            return;
        };
        let Some(config) = self.connections.get(entry.connection).cloned() else {
            return;
        };
        self.selected = Some(index);
        let is_connected = DbStore::global(cx).read(cx).is_connected(&config.key);
        let focus_handle = self.focus_handle.clone();
        let context_menu = ContextMenu::build(window, cx, |menu, _, _| {
            let menu = menu.context(focus_handle);
            match &entry.kind {
                EntryKind::Connection => {
                    let menu = if is_connected {
                        menu.action("Disconnect", Disconnect.boxed_clone())
                    } else {
                        menu.action("Connect", Connect.boxed_clone())
                    };
                    let menu = menu
                        .action("New SQL File", NewSqlFile.boxed_clone())
                        .action("Query History", QueryHistory.boxed_clone())
                        .action_disabled_when(!is_connected, "Refresh", RefreshSchema.boxed_clone())
                        .separator()
                        .action("Copy Name", CopyName.boxed_clone());
                    if config.key.is_from_project() {
                        menu.action("Open Project Settings", EditConnection.boxed_clone())
                    } else {
                        menu.action("Edit…", EditConnection.boxed_clone())
                            .action("Delete…", DeleteConnection.boxed_clone())
                    }
                }
                EntryKind::Schema(_) => menu
                    .action("New SQL File", NewSqlFile.boxed_clone())
                    .action("Refresh", RefreshSchema.boxed_clone())
                    .action("Copy Name", CopyName.boxed_clone()),
                EntryKind::Relation(..) => menu
                    .action(
                        format!("Show First {ROW_PREVIEW_LIMIT} Rows"),
                        ShowRows.boxed_clone(),
                    )
                    .action("Copy DDL", CopyDdl.boxed_clone())
                    .action("Copy Name", CopyName.boxed_clone()),
                EntryKind::Column(_) => menu.action("Copy Name", CopyName.boxed_clone()),
                EntryKind::Message(..) => menu,
            }
        });
        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription =
            cx.subscribe_in(
                &context_menu,
                window,
                |this, _, _: &DismissEvent, window, cx| {
                    if this.context_menu.as_ref().is_some_and(|(menu, _, _)| {
                        menu.focus_handle(cx).contains_focused(window, cx)
                    }) {
                        this.focus_handle.focus(window, cx);
                    }
                    this.context_menu.take();
                    cx.notify();
                },
            );
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    fn render_entry(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(entry) = self.entries.get(index) else {
            return div().into_any_element();
        };
        let Some(config) = self.connections.get(entry.connection) else {
            return div().into_any_element();
        };
        let is_expanded = entry
            .node_id(&self.connections)
            .is_some_and(|node| self.expanded.contains(&node));
        let store = DbStore::global(cx);
        let store = store.read(cx);

        let (icon, label, end_slot): (Option<Icon>, SharedString, Option<AnyElement>) =
            match &entry.kind {
                EntryKind::Connection => (
                    Some(
                        Icon::new(IconName::Database)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    ),
                    config.key.id.clone().into(),
                    Some(
                        h_flex()
                            .gap_1()
                            .when(config.key.is_from_project(), |row| {
                                row.child(
                                    Icon::new(IconName::Folder)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                            })
                            .when(config.read_only, |row| {
                                row.child(
                                    Icon::new(IconName::Lock)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                            })
                            .when(config.has_password_in_settings(), |row| {
                                row.child(
                                    Icon::new(IconName::Warning)
                                        .size(IconSize::XSmall)
                                        .color(Color::Warning),
                                )
                            })
                            .child(environment_label(config))
                            .child(status_indicator(&store.status(&config.key)))
                            .into_any_element(),
                    ),
                ),
                EntryKind::Schema(schema) => (
                    Some(
                        Icon::new(IconName::Folder)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    ),
                    schema.clone(),
                    None,
                ),
                EntryKind::Relation(_, relation) => (
                    Some(
                        Icon::new(if relation.kind.is_view() {
                            IconName::Eye
                        } else {
                            IconName::Table
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    ),
                    relation.name.clone(),
                    None,
                ),
                EntryKind::Column(column) => (
                    None,
                    column.name.clone(),
                    Some(
                        h_flex()
                            .gap_1()
                            .when(column.primary_key, |row| {
                                row.child(
                                    Label::new("PK")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Accent),
                                )
                            })
                            .child(
                                Label::new(if column.nullable {
                                    format!("{}?", column.data_type)
                                } else {
                                    column.data_type.to_string()
                                })
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            )
                            .into_any_element(),
                    ),
                ),
                EntryKind::Message(message, _) => (None, message.clone(), None),
            };
        let message_color = match &entry.kind {
            EntryKind::Message(_, color) => Some(*color),
            _ => None,
        };
        let tooltip = match &entry.kind {
            EntryKind::Connection => Some(self.connection_tooltip(config, cx)),
            EntryKind::Message(message, _) => Some(message.clone()),
            _ => None,
        };

        ListItem::new(index)
            .spacing(ListItemSpacing::Sparse)
            .indent_level(entry.depth)
            .indent_step_size(px(12.))
            .toggle_state(self.selected == Some(index))
            .when(Self::is_expandable(entry), |item| {
                item.toggle(Some(is_expanded)).on_toggle(
                    cx.listener(move |this, _, window, cx| this.toggle_expanded(index, window, cx)),
                )
            })
            .when_some(icon, |item, icon| item.start_slot(icon))
            .child(
                Label::new(label)
                    .size(LabelSize::Small)
                    .when_some(message_color, |label, color| label.color(color))
                    .truncate(),
            )
            .when_some(end_slot, |item, end_slot| item.end_slot(end_slot))
            .when_some(tooltip, |item, tooltip| {
                item.tooltip(Tooltip::text(tooltip))
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.selected = Some(index);
                let is_relation = matches!(
                    this.entries.get(index).map(|entry| &entry.kind),
                    Some(EntryKind::Relation(..))
                );
                if is_relation && event.click_count() >= 2 {
                    this.show_rows(&ShowRows, window, cx);
                } else if event.click_count() < 2 {
                    this.toggle_expanded(index, window, cx);
                }
                this.focus_handle.focus(window, cx);
                cx.notify();
            }))
            .on_secondary_mouse_down(cx.listener(
                move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.deploy_context_menu(event.position, index, window, cx);
                },
            ))
            .into_any_element()
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.project.read(cx).is_via_collab() {
            return v_flex()
                .p_4()
                .gap_2()
                .child(
                    Label::new("Databases aren't available in shared projects.")
                        .color(Color::Muted),
                )
                .into_any_element();
        }
        v_flex()
            .p_4()
            .gap_2()
            .items_center()
            .child(Label::new("No database connections").color(Color::Muted))
            .child(
                Button::new("add-connection", "Add Connection")
                    .style(ButtonStyle::Filled)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.new_connection(&NewConnection, window, cx)
                    })),
            )
            .child(
                Label::new("Or add `database_connections` to your settings.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_sql_extension_hint(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let has_sql = self
            .project
            .read(cx)
            .languages()
            .language_names()
            .iter()
            .any(|name| name.as_ref().eq_ignore_ascii_case("sql"));
        if has_sql {
            return None;
        }
        Some(
            h_flex()
                .p_2()
                .gap_2()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new("Install the SQL extension for highlighting and completions.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
                .child(
                    Button::new("install-sql", "Install")
                        .style(ButtonStyle::Subtle)
                        .on_click(|_, window, cx| {
                            window.dispatch_action(
                                zed_actions::Extensions {
                                    category_filter: None,
                                    id: Some("sql".to_string()),
                                }
                                .boxed_clone(),
                                cx,
                            )
                        }),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
impl DatabasePanel {
    /// The visible tree as text, one entry per line, for tests.
    pub(crate) fn entries_text(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| {
                let indent = "  ".repeat(entry.depth);
                let expanded = entry
                    .node_id(&self.connections)
                    .map(|node| {
                        if self.expanded.contains(&node) {
                            "v "
                        } else {
                            "> "
                        }
                    })
                    .unwrap_or("");
                let label = match &entry.kind {
                    EntryKind::Connection => self.connections[entry.connection].key.id.to_string(),
                    EntryKind::Schema(schema) => schema.to_string(),
                    EntryKind::Relation(_, relation) => relation.name.to_string(),
                    EntryKind::Column(column) => format!("{} {}", column.name, column.data_type),
                    EntryKind::Message(message, _) => format!("({message})"),
                };
                format!("{indent}{expanded}{label}")
            })
            .collect()
    }

    pub(crate) fn select_entry(&mut self, label: &str, cx: &mut Context<Self>) {
        let index = self
            .entries_text()
            .iter()
            .position(|text| text.trim_start().trim_start_matches(['v', '>']).trim() == label)
            .unwrap_or_else(|| panic!("no entry named {label}"));
        self.select(index, cx);
    }

    pub(crate) fn set_filter(&mut self, filter: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_editor
            .update(cx, |editor, cx| editor.set_text(filter, window, cx));
    }
}

fn matches_filter(name: &str, query: &str) -> bool {
    query.is_empty() || name.to_lowercase().contains(query)
}

pub(crate) fn status_indicator(status: &ConnectionStatus) -> AnyElement {
    let (color, tooltip) = match status {
        ConnectionStatus::Connected => (Color::Success, "Connected"),
        ConnectionStatus::Connecting => (Color::Info, "Connecting"),
        ConnectionStatus::PasswordRequired => (Color::Warning, "Password required"),
        ConnectionStatus::Failed(_) => (Color::Error, "Connection failed"),
        ConnectionStatus::Disconnected => return div().w(px(6.)).into_any_element(),
    };
    div()
        .id(tooltip)
        .child(Indicator::dot().color(color))
        .tooltip(Tooltip::text(tooltip))
        .into_any_element()
}

fn environment_color(environment: DatabaseEnvironment) -> Color {
    match environment {
        DatabaseEnvironment::Local => Color::Muted,
        DatabaseEnvironment::Staging => Color::Warning,
        DatabaseEnvironment::Production => Color::Error,
    }
}

/// The colored environment label of a connection, such as `production`.
pub(crate) fn environment_label(config: &ConnectionConfig) -> AnyElement {
    let name = match config.environment {
        DatabaseEnvironment::Local => return div().into_any_element(),
        DatabaseEnvironment::Staging => "staging",
        DatabaseEnvironment::Production => "production",
    };
    Label::new(name)
        .size(LabelSize::XSmall)
        .color(environment_color(config.environment))
        .into_any_element()
}

pub(crate) fn environment_icon(config: &ConnectionConfig) -> Icon {
    Icon::new(IconName::Database)
        .size(IconSize::Small)
        .color(match config.environment {
            DatabaseEnvironment::Local => Color::Muted,
            environment => environment_color(environment),
        })
}

impl Focusable for DatabasePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DatabasePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let item_count = self.entries.len();
        let mut key_context = gpui::KeyContext::new_with_defaults();
        key_context.add("DatabasePanel");
        if !self.filter_editor.focus_handle(cx).is_focused(window) {
            key_context.add("not_editing");
        }
        v_flex()
            .key_context(key_context)
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::expand_selected))
            .on_action(cx.listener(Self::collapse_selected))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::connect))
            .on_action(cx.listener(Self::disconnect))
            .on_action(cx.listener(Self::refresh))
            .on_action(cx.listener(Self::new_connection))
            .on_action(cx.listener(Self::edit_connection))
            .on_action(cx.listener(Self::delete_connection))
            .on_action(cx.listener(Self::new_sql_file))
            .on_action(cx.listener(Self::query_history))
            .on_action(cx.listener(Self::show_rows))
            .on_action(cx.listener(Self::copy_ddl))
            .on_action(cx.listener(Self::copy_name))
            .child(
                h_flex()
                    .p_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(div().flex_1().px_1().child(self.filter_editor.clone()))
                    .child(
                        IconButton::new("refresh-schema", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Refresh"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.refresh(&RefreshSchema, window, cx)
                            })),
                    )
                    .child(
                        IconButton::new("new-connection", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("New Connection"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.new_connection(&NewConnection, window, cx)
                            })),
                    ),
            )
            .map(|panel| {
                if self.connections.is_empty() {
                    panel.child(self.render_empty_state(cx))
                } else {
                    panel.child(
                        uniform_list(
                            "database-panel-entries",
                            item_count,
                            cx.processor(|this, range: Range<usize>, _window, cx| {
                                range.map(|index| this.render_entry(index, cx)).collect()
                            }),
                        )
                        .size_full()
                        .track_scroll(&self.scroll_handle),
                    )
                }
            })
            .children(self.render_sql_extension_hint(cx))
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

impl Panel for DatabasePanel {
    fn persistent_name() -> &'static str {
        "DatabasePanel"
    }

    fn panel_key() -> &'static str {
        DATABASE_PANEL_KEY
    }

    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        match DatabaseSettings::get_global(cx).dock {
            DockSide::Left => DockPosition::Left,
            DockSide::Right => DockPosition::Right,
        }
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            let dock = match position {
                DockPosition::Left | DockPosition::Bottom => DockSide::Left,
                DockPosition::Right => DockSide::Right,
            };
            settings.database_panel.get_or_insert_default().dock = Some(dock);
        });
    }

    fn default_size(&self, _: &Window, cx: &App) -> Pixels {
        DatabaseSettings::get_global(cx).default_width
    }

    fn icon(&self, _: &Window, cx: &App) -> Option<IconName> {
        let settings = DatabaseSettings::get_global(cx);
        (settings.enabled && settings.button && !self.project.read(cx).is_via_collab())
            .then_some(IconName::Database)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Database Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        self.active
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        if self.active != active {
            self.active = active;
            self.serialize(cx);
        }
    }

    fn activation_priority(&self) -> u32 {
        8
    }

    fn enabled(&self, cx: &App) -> bool {
        DatabaseSettings::get_global(cx).enabled && !self.project.read(cx).is_via_collab()
    }

    fn hide_button_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        Some(workspace::HideStatusItem::new(|settings| {
            settings.database_panel.get_or_insert_default().button = Some(false);
        }))
    }
}
