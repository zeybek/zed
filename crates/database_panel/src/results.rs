use std::{collections::BTreeMap, sync::Arc};

use anyhow::Result;
use database_core::{
    ColumnMeta, ConnectionConfig, ConnectionKey, DatabaseEnvironment, DbStore, QueryRun,
    QueryRunEvent, QuerySource, QueryState, ResultRow, TruncationReason, ValueKind,
    export::{ExportColumn, export},
    statement,
};
use editor::Editor;
use gpui::{
    AnyElement, App, ClipboardItem, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    PromptLevel, SharedString, Subscription, Task, WeakEntity, actions,
};
use project::Project;
use tabular_data_preview::{
    TableView, TableViewEvent, TableViewOptions,
    types::{AnyColumn, ColumnKind, DataCellId, DataRow, TableCell, TableLikeContent},
};
use ui::{
    Button, ButtonSize, Color, ContextMenu, Icon, IconButton, IconName, IconSize, Label, LabelSize,
    PopoverMenu, SpinnerLabel, Tooltip, prelude::*, table_row::TableRow,
};
use util::ResultExt as _;
use workspace::{
    Item, Workspace,
    item::{ItemEvent, TabContentParams},
};

use crate::{
    CancelQuery, CopyResults, ExportResults, LoadMoreRows, OpenQueryInEditor, ResultFormat,
    connection_modal::{connect_interactively, detach_and_notify_err},
};

actions!(
    database_panel,
    [
        /// Runs the query of the current result again.
        RerunQuery,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &RerunQuery, window, cx| {
            if let Some(item) = workspace.active_item_as::<QueryResultsItem>(cx) {
                item.update(cx, |item, cx| item.rerun(window, cx));
            }
        });
    })
    .detach();
}

/// What a result tab shows the result of. Running a query with the same origin replaces the
/// result in the existing tab, unless that tab is pinned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResultOrigin {
    Editor(EntityId),
    Relation {
        connection: ConnectionKey,
        schema: SharedString,
        relation: SharedString,
    },
    Other,
}

pub struct QueryRequest {
    pub config: ConnectionConfig,
    pub project: Entity<Project>,
    pub sql: String,
    pub origin: ResultOrigin,
    pub source: QuerySource,
    /// Focus the result tab instead of keeping focus where it is.
    pub focus: bool,
}

/// Runs a query after confirming writes to production, connecting (and asking for a password)
/// as needed, and shows its result in a center tab.
pub fn run_query(
    workspace: &mut Workspace,
    request: QueryRequest,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) {
    if request.project.read(cx).is_via_collab() {
        workspace.show_error("Database queries aren't available to collaborators", cx);
        return;
    }
    let confirmation = confirm_writes(&request.config, &request.sql, window, cx);
    let workspace_handle = cx.weak_entity();
    let task = cx.spawn_in(window, async move |workspace, cx| {
        if !confirmation.await {
            return anyhow::Ok(());
        }
        workspace
            .update_in(cx, |_, window, cx| {
                connect_interactively(
                    workspace_handle,
                    request.config.clone(),
                    request.project.clone(),
                    window,
                    cx,
                )
            })?
            .await?;
        workspace.update_in(cx, |workspace, window, cx| {
            show_result(workspace, request, window, cx);
        })
    });
    detach_and_notify_err(task, cx.weak_entity(), cx);
}

/// Asks before running statements that write to a production database.
pub(crate) fn confirm_writes(
    config: &ConnectionConfig,
    sql: &str,
    window: &mut Window,
    cx: &mut App,
) -> Task<bool> {
    if config.environment != DatabaseEnvironment::Production {
        return Task::ready(true);
    }
    let statements = statement::split_statements(sql)
        .into_iter()
        .map(|range| &sql[range])
        .collect::<Vec<_>>();
    let writes = statements
        .iter()
        .filter(|statement| statement::classify(statement).modifies_data())
        .count();
    if writes == 0 {
        return Task::ready(true);
    }
    let unfiltered = statements
        .iter()
        .any(|statement| statement::is_unfiltered_write(statement));
    let mut detail = format!(
        "`{}` is a production connection. {} statement{} will modify data or the schema.",
        config.key.id,
        writes,
        if writes == 1 { "" } else { "s" }
    );
    if unfiltered {
        detail.push_str(
            "\n\nAn UPDATE or DELETE without a WHERE clause will change every row of its table.",
        );
    }
    let answer = window.prompt(
        if unfiltered {
            PromptLevel::Critical
        } else {
            PromptLevel::Warning
        },
        "Run on production?",
        Some(&detail),
        &["Run", "Cancel"],
        cx,
    );
    cx.background_spawn(async move { answer.await == Ok(0) })
}

fn show_result(
    workspace: &mut Workspace,
    request: QueryRequest,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<QueryResultsItem>(cx)
        .find(|item| {
            let item_ref = item.read(cx);
            if item_ref.origin != request.origin || request.origin == ResultOrigin::Other {
                return false;
            }
            // Pinned results are kept; a new result opens in another tab.
            !workspace.pane_for(item).is_some_and(|pane| {
                let pane = pane.read(cx);
                pane.index_for_item(item)
                    .is_some_and(|index| index < pane.pinned_count())
            })
        });

    let item = match existing {
        Some(item) => {
            workspace.activate_item(&item, true, request.focus, window, cx);
            item
        }
        None => {
            let workspace_handle = cx.weak_entity();
            let item = cx.new(|cx| {
                QueryResultsItem::new(
                    workspace_handle,
                    request.project.clone(),
                    request.config.clone(),
                    request.origin.clone(),
                    window,
                    cx,
                )
            });
            match &request.origin {
                // Keep the editor visible next to its results.
                ResultOrigin::Editor(_) if !request.focus => {
                    let origin_pane = workspace.active_pane().clone();
                    let pane = workspace.adjacent_pane_of(&origin_pane, window, cx);
                    pane.update(cx, |pane, cx| {
                        pane.add_item(Box::new(item.clone()), true, false, None, window, cx)
                    });
                    origin_pane.update(cx, |pane, cx| pane.focus_active_item(window, cx));
                }
                _ => {
                    workspace.add_item_to_center(Box::new(item.clone()), window, cx);
                }
            }
            item
        }
    };
    item.update(cx, |item, cx| {
        item.config = request.config;
        item.run_sql(request.sql, request.source, window, cx)
    });
}

/// Cell edits of a result that shows a table with a primary key.
struct EditState {
    schema: SharedString,
    relation: SharedString,
    /// Result columns that hold the primary key.
    key_columns: Vec<usize>,
    /// Changed cells by (data row, column), with their value before the first change.
    pending: BTreeMap<(usize, usize), PendingEdit>,
    error: Option<SharedString>,
    committing: Option<Task<()>>,
}

struct PendingEdit {
    original: TableCell,
    value: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
enum Content {
    Empty,
    Table,
    /// Statements finished without returning rows.
    Message(SharedString),
}

pub struct QueryResultsItem {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    pub(crate) config: ConnectionConfig,
    origin: ResultOrigin,
    sql: Arc<str>,
    source: QuerySource,
    run: Option<Entity<QueryRun>>,
    table: Entity<TableView>,
    columns: Vec<ColumnMeta>,
    content: Content,
    focus_handle: FocusHandle,
    edits: Option<EditState>,
    _run_subscription: Option<Subscription>,
    _table_subscriptions: [Subscription; 2],
}

impl EventEmitter<ItemEvent> for QueryResultsItem {}

impl QueryResultsItem {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        config: ConnectionConfig,
        origin: ResultOrigin,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let table = cx.new(|cx| {
            TableView::new(window, cx).with_options(TableViewOptions {
                row_identifiers: tabular_data_preview::RowIdentifiers::RowNum,
                ..Default::default()
            })
        });
        let table_subscriptions = [
            cx.observe(&table, |_, _, cx| cx.notify()),
            cx.subscribe_in(&table, window, Self::on_table_event),
        ];
        Self {
            workspace,
            project,
            config,
            origin,
            sql: "".into(),
            source: QuerySource::Editor,
            run: None,
            table,
            columns: Vec::new(),
            content: Content::Empty,
            focus_handle: cx.focus_handle(),
            edits: None,
            _run_subscription: None,
            _table_subscriptions: table_subscriptions,
        }
    }

    pub fn sql(&self) -> &str {
        &self.sql
    }

    pub fn run(&self) -> Option<&Entity<QueryRun>> {
        self.run.as_ref()
    }

    pub fn table(&self) -> &Entity<TableView> {
        &self.table
    }

    pub fn origin(&self) -> &ResultOrigin {
        &self.origin
    }

    /// The text shown instead of a table, such as the number of affected rows.
    pub fn message(&self) -> Option<SharedString> {
        match &self.content {
            Content::Message(message) => Some(message.clone()),
            Content::Empty | Content::Table => None,
        }
    }

    pub(crate) fn run_sql(
        &mut self,
        sql: String,
        source: QuerySource,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if let Some(previous) = self.run.take() {
            previous.update(cx, |run, cx| run.cancel(cx));
        }
        self.sql = sql.clone().into();
        self.source = source;
        self.columns.clear();
        self.content = Content::Empty;
        self.edits = None;
        self.table.update(cx, |table, cx| {
            table.set_marked_cells([], cx);
            table.set_contents(TableLikeContent::default(), cx);
            table.reset_view_state(cx);
            table.set_loading(true, cx);
        });

        let config = self.config.clone();
        let project = self.project.clone();
        let run = DbStore::global(cx).update(cx, |store, cx| {
            store.execute(config, Some(project), sql, source, cx)
        });
        self._run_subscription = Some(cx.subscribe(&run, Self::on_run_event));
        self.run = Some(run);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn on_run_event(
        &mut self,
        run: Entity<QueryRun>,
        event: &QueryRunEvent,
        cx: &mut gpui::Context<Self>,
    ) {
        match event {
            QueryRunEvent::ResultSet(columns) => {
                self.columns = columns.clone();
                self.content = Content::Table;
                let contents = table_contents(columns);
                self.table.update(cx, |table, cx| {
                    table.set_contents(contents, cx);
                    table.reset_view_state(cx);
                });
                self.detect_editability(cx);
            }
            QueryRunEvent::Rows(rows) => {
                let cols = self.columns.len();
                let rows = rows
                    .iter()
                    .filter_map(|row| table_row(row, cols))
                    .collect::<Vec<_>>();
                self.table.update(cx, |table, cx| {
                    if let Err(error) = table.append_rows(rows, cx) {
                        log::error!("failed to show query results: {error:#}");
                    }
                });
            }
            QueryRunEvent::Updated => {
                let (state, rows_affected) = {
                    let run = run.read(cx);
                    (run.state.clone(), run.rows_affected)
                };
                if !state.is_active() {
                    self.table
                        .update(cx, |table, cx| table.set_loading(false, cx));
                    if self.content == Content::Empty && matches!(state, QueryState::Finished) {
                        self.content = Content::Message(match rows_affected {
                            Some(rows) => format!(
                                "Statement executed. {} row{} affected.",
                                rows,
                                if rows == 1 { "" } else { "s" }
                            )
                            .into(),
                            None => "Statement executed.".into(),
                        });
                    }
                }
                cx.emit(ItemEvent::UpdateTab);
            }
        }
        cx.notify();
    }

    /// Results of browsing a table can be edited when they include its primary key.
    fn detect_editability(&mut self, cx: &mut gpui::Context<Self>) {
        let ResultOrigin::Relation {
            connection,
            schema,
            relation,
        } = &self.origin
        else {
            return;
        };
        if self.config.read_only || connection != &self.config.key {
            return;
        }
        let (schema, relation) = (schema.clone(), relation.clone());
        let store = DbStore::global(cx);
        let load = store.update(cx, |store, cx| {
            if store
                .columns(&self.config.key, &schema, &relation)
                .is_some()
            {
                Task::ready(Ok(()))
            } else {
                store.load_columns(
                    self.config.clone(),
                    Some(self.project.clone()),
                    schema.clone(),
                    relation.clone(),
                    cx,
                )
            }
        });
        cx.spawn(async move |this, cx| {
            load.await?;
            this.update(cx, |this, cx| {
                let Some(columns) = DbStore::global(cx)
                    .read(cx)
                    .columns(&this.config.key, &schema, &relation)
                    .map(<[_]>::to_vec)
                else {
                    return;
                };
                let key_columns = columns
                    .iter()
                    .filter(|column| column.primary_key)
                    .map(|column| {
                        this.columns
                            .iter()
                            .position(|result_column| result_column.name == column.name)
                    })
                    .collect::<Option<Vec<_>>>()
                    .unwrap_or_default();
                if !key_columns.is_empty() {
                    this.edits = Some(EditState {
                        schema,
                        relation,
                        key_columns,
                        pending: BTreeMap::new(),
                        error: None,
                        committing: None,
                    });
                    cx.notify();
                }
            })
        })
        .detach_and_log_err(cx);
    }

    pub fn is_editable(&self) -> bool {
        self.edits.is_some()
    }

    pub fn pending_edit_count(&self) -> usize {
        self.edits.as_ref().map_or(0, |edits| edits.pending.len())
    }

    fn on_table_event(
        &mut self,
        _: &Entity<TableView>,
        event: &TableViewEvent,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let TableViewEvent::CellActivated(cell) = event else {
            return;
        };
        if self.edits.is_none() {
            return;
        }
        let Some(column) = self.columns.get(*cell.col).cloned() else {
            return;
        };
        let value = self
            .table
            .read(cx)
            .cell(*cell)
            .and_then(|cell| cell.display_value().cloned());
        if column.kind == ValueKind::Binary {
            return;
        }
        let this = cx.entity().downgrade();
        let cell = *cell;
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, move |window, cx| {
                    crate::edit::EditCellModal::new(
                        column.name.clone(),
                        column.type_name.clone(),
                        value,
                        Box::new(move |value, _, cx| {
                            this.update(cx, |this, cx| this.apply_edit(cell, value, cx))
                                .ok();
                        }),
                        window,
                        cx,
                    )
                })
            })
            .ok();
    }

    pub(crate) fn apply_edit(
        &mut self,
        cell: DataCellId,
        value: Option<String>,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(edits) = self.edits.as_mut() else {
            return;
        };
        let Some(current) = self.table.read(cx).cell(cell).cloned() else {
            return;
        };
        let key = (*cell.row, *cell.col);
        let original = edits
            .pending
            .remove(&key)
            .map_or(current, |pending| pending.original);
        let unchanged = original.display_value().map(|value| value.to_string()) == value;
        let new_cell = match &value {
            Some(value) => TableCell::Generated(value.clone().into()),
            None => TableCell::Null,
        };
        if !unchanged {
            edits.pending.insert(key, PendingEdit { original, value });
        }
        edits.error = None;
        let marked = edits
            .pending
            .keys()
            .map(|(row, col)| DataCellId::new(DataRow(*row), AnyColumn(*col)))
            .collect::<Vec<_>>();
        self.table.update(cx, |table, cx| {
            table.set_cell(cell, new_cell, cx).log_err();
            table.set_marked_cells(marked, cx);
        });
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    pub(crate) fn revert_edits(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(edits) = self.edits.as_mut() else {
            return;
        };
        let pending = std::mem::take(&mut edits.pending);
        edits.error = None;
        self.table.update(cx, |table, cx| {
            for ((row, col), edit) in pending {
                table
                    .set_cell(
                        DataCellId::new(DataRow(row), AnyColumn(col)),
                        edit.original,
                        cx,
                    )
                    .log_err();
            }
            table.set_marked_cells([], cx);
        });
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// The `UPDATE` statements that write the pending edits, one per changed row.
    pub(crate) fn edit_statements(&self, cx: &App) -> Vec<String> {
        let Some(edits) = &self.edits else {
            return Vec::new();
        };
        let table = self.table.read(cx);
        let mut rows = BTreeMap::<usize, Vec<(usize, Option<&str>)>>::new();
        for ((row, col), edit) in &edits.pending {
            rows.entry(*row)
                .or_default()
                .push((*col, edit.value.as_deref()));
        }
        rows.into_iter()
            .filter_map(|(row, assignments)| {
                let assignments = assignments
                    .into_iter()
                    .map(|(col, value)| Some((self.columns.get(col)?.name.as_ref(), value)))
                    .collect::<Option<Vec<_>>>()?;
                // Rows are found by their key as loaded, even if the key itself was edited.
                let key = edits
                    .key_columns
                    .iter()
                    .map(|col| {
                        let value = match edits.pending.get(&(row, *col)) {
                            Some(edit) => edit.original.display_value(),
                            None => table
                                .cell(DataCellId::new(DataRow(row), AnyColumn(*col)))?
                                .display_value(),
                        };
                        Some((
                            self.columns.get(*col)?.name.as_ref(),
                            value.map(|value| value.as_ref()),
                        ))
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some(statement::update_statement(
                    self.config.driver,
                    &edits.schema,
                    &edits.relation,
                    &assignments,
                    &key,
                ))
            })
            .collect()
    }

    /// Asks for confirmation with a summary of the changes, then writes them in one transaction.
    fn commit_edits(
        &mut self,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Task<Result<()>> {
        let statements = self.edit_statements(cx);
        if statements.is_empty()
            || self
                .edits
                .as_ref()
                .is_some_and(|edits| edits.committing.is_some())
        {
            return Task::ready(Ok(()));
        }
        let mut detail = statements
            .iter()
            .take(10)
            .map(|statement| format!("{statement};"))
            .collect::<Vec<_>>()
            .join("\n");
        if statements.len() > 10 {
            detail.push_str(&format!("\n…and {} more", statements.len() - 10));
        }
        if self.config.environment == DatabaseEnvironment::Production {
            detail.insert_str(
                0,
                &format!("`{}` is a production connection.\n\n", self.config.key.id),
            );
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!(
                "Commit {} change{}?",
                statements.len(),
                if statements.len() == 1 { "" } else { "s" }
            ),
            Some(&detail),
            &["Commit", "Cancel"],
            cx,
        );
        let config = self.config.clone();
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                anyhow::bail!("the commit was cancelled");
            }
            let commit = this.update(cx, |_, cx| {
                DbStore::global(cx).update(cx, |store, cx| {
                    store.execute_transaction(config, Some(project), statements, cx)
                })
            })?;
            let result = commit.await;
            this.update(cx, |this, cx| {
                if let Some(edits) = this.edits.as_mut() {
                    edits.committing = None;
                    match &result {
                        Ok(()) => {
                            edits.pending.clear();
                            edits.error = None;
                            this.table
                                .update(cx, |table, cx| table.set_marked_cells([], cx));
                        }
                        Err(error) => edits.error = Some(format!("{error:#}").into()),
                    }
                }
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            })?;
            result
        })
    }

    pub(crate) fn rerun(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.sql.is_empty() {
            return;
        }
        let request = QueryRequest {
            config: self.config.clone(),
            project: self.project.clone(),
            sql: self.sql.to_string(),
            origin: self.origin.clone(),
            source: self.source,
            focus: true,
        };
        self.workspace
            .update(cx, |workspace, cx| {
                run_query(workspace, request, window, cx)
            })
            .ok();
    }

    fn cancel(&mut self, _: &CancelQuery, _window: &mut Window, cx: &mut gpui::Context<Self>) {
        if let Some(run) = &self.run {
            run.update(cx, |run, cx| run.cancel(cx));
        }
    }

    fn load_more(&mut self, _: &LoadMoreRows, _window: &mut Window, cx: &mut gpui::Context<Self>) {
        if let Some(run) = &self.run {
            run.update(cx, |run, cx| run.load_more(cx));
        }
    }

    /// The displayed rows, after sorting and filtering, serialized in `format`.
    fn serialize(&self, format: ResultFormat, cx: &App) -> Option<String> {
        if self.content != Content::Table {
            return None;
        }
        let table = self.table.read(cx);
        let contents = table.contents();
        let columns = self
            .columns
            .iter()
            .map(|column| ExportColumn {
                name: column.name.as_ref(),
                kind: column.kind,
            })
            .collect::<Vec<_>>();
        let rows = table
            .displayed_data_rows()
            .filter_map(|data_row| contents.get_row(data_row))
            .map(|row| {
                row.as_slice()
                    .iter()
                    .map(|cell| cell.display_value().map(|value| value.as_ref()))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        Some(export(format.into(), &columns, rows))
    }

    fn copy_results(
        &mut self,
        action: &CopyResults,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if let Some(text) = self.serialize(action.format, cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn export_results(
        &mut self,
        action: &ExportResults,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(text) = self.serialize(action.format, cx) else {
            return;
        };
        let language = match action.format {
            ResultFormat::Csv => "CSV",
            ResultFormat::Json => "JSON",
            ResultFormat::Markdown => "Markdown",
        };
        telemetry::event!(
            "Database Results Exported",
            format = format!("{:?}", action.format).to_lowercase()
        );
        open_text_in_editor(
            self.workspace.clone(),
            self.project.clone(),
            text,
            language,
            None,
            window,
            cx,
        );
    }

    fn open_in_editor(
        &mut self,
        _: &OpenQueryInEditor,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        open_text_in_editor(
            self.workspace.clone(),
            self.project.clone(),
            self.sql.to_string(),
            "SQL",
            Some(self.config.key.clone()),
            window,
            cx,
        );
    }

    fn render_status(&self, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let run = self.run.as_ref().map(|run| run.read(cx));
        let state = run.map(|run| run.state.clone());
        let row_count = self.table.read(cx).contents().rows.len();
        let mut summary = Vec::<AnyElement>::new();

        if let Some(run) = run {
            match &run.state {
                QueryState::Connecting => {
                    summary.push(SpinnerLabel::new().into_any_element());
                    summary.push(
                        Label::new("Connecting…")
                            .size(LabelSize::Small)
                            .into_any_element(),
                    );
                }
                QueryState::Running => {
                    summary.push(SpinnerLabel::new().into_any_element());
                    summary.push(
                        Label::new(format!("Running… {}", format_rows(row_count)))
                            .size(LabelSize::Small)
                            .into_any_element(),
                    );
                }
                QueryState::Failed(_) => summary.push(
                    Label::new("Failed")
                        .size(LabelSize::Small)
                        .color(Color::Error)
                        .into_any_element(),
                ),
                QueryState::Cancelled => summary.push(
                    Label::new(format!("Cancelled after {}", format_rows(row_count)))
                        .size(LabelSize::Small)
                        .color(Color::Warning)
                        .into_any_element(),
                ),
                QueryState::Finished | QueryState::Paused { .. } => {
                    if self.content == Content::Table {
                        summary.push(
                            Label::new(format_rows(row_count))
                                .size(LabelSize::Small)
                                .into_any_element(),
                        );
                    } else if let Some(rows) = run.rows_affected {
                        summary.push(
                            Label::new(format!("{rows} affected"))
                                .size(LabelSize::Small)
                                .into_any_element(),
                        );
                    }
                }
            }
            summary.push(
                Label::new(format_duration(run.duration()))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
            );
            if let QueryState::Paused { reason } = &run.state {
                summary.push(
                    Label::new(match reason {
                        TruncationReason::RowLimit => "row limit reached",
                        TruncationReason::MaxRows => "maximum rows reached",
                        TruncationReason::MaxBytes => "result size limit reached",
                    })
                    .size(LabelSize::Small)
                    .color(Color::Warning)
                    .into_any_element(),
                );
            }
        }

        let pending_edits = self.pending_edit_count();
        let edit_error = self.edits.as_ref().and_then(|edits| edits.error.clone());
        let is_active = state.as_ref().is_some_and(QueryState::is_active);
        let can_load_more = run.is_some_and(|run| run.can_load_more());
        let has_table = self.content == Content::Table;
        let focus_handle = self.focus_handle.clone();

        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(crate::panel::environment_label(&self.config))
            .child(
                Label::new(self.config.key.id.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .children(summary)
            .when(self.is_editable() && pending_edits == 0, |row| {
                row.child(
                    Label::new("Double-click a cell to edit")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when_some(edit_error, |row, error| {
                row.child(
                    Label::new(error)
                        .size(LabelSize::Small)
                        .color(Color::Error)
                        .truncate(),
                )
            })
            .child(div().flex_1())
            .when(pending_edits > 0, |row| {
                row.child(
                    Label::new(format!(
                        "{pending_edits} pending change{}",
                        if pending_edits == 1 { "" } else { "s" }
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Modified),
                )
                .child(
                    Button::new("revert-edits", "Revert")
                        .size(ButtonSize::Compact)
                        .on_click(cx.listener(|this, _, _, cx| this.revert_edits(cx))),
                )
                .child(
                    Button::new("commit-edits", "Commit…")
                        .size(ButtonSize::Compact)
                        .style(ui::ButtonStyle::Filled)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.commit_edits(window, cx).detach();
                        })),
                )
            })
            .when(can_load_more, |row| {
                row.child(
                    Button::new("load-more", "Load More")
                        .size(ButtonSize::Compact)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.load_more(&LoadMoreRows, window, cx)
                        })),
                )
            })
            .when(is_active, |row| {
                row.child(
                    Button::new("cancel", "Cancel")
                        .size(ButtonSize::Compact)
                        .start_icon(Icon::new(IconName::Stop).size(IconSize::Small))
                        .on_click(
                            cx.listener(|this, _, window, cx| {
                                this.cancel(&CancelQuery, window, cx)
                            }),
                        ),
                )
            })
            .when(!is_active && !self.sql.is_empty(), |row| {
                row.child(
                    IconButton::new("rerun", IconName::Rerun)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Run Again"))
                        .on_click(cx.listener(|this, _, window, cx| this.rerun(window, cx))),
                )
            })
            .child(
                IconButton::new("open-sql", IconName::FileCode)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Open Query in Editor"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_in_editor(&OpenQueryInEditor, window, cx)
                    })),
            )
            .when(has_table, |row| {
                row.child(
                    PopoverMenu::new("export-results")
                        .trigger_with_tooltip(
                            IconButton::new("export", IconName::Download)
                                .icon_size(IconSize::Small),
                            Tooltip::text("Export Results"),
                        )
                        .anchor(gpui::Anchor::TopRight)
                        .menu(move |window, cx| {
                            let focus_handle = focus_handle.clone();
                            Some(ContextMenu::build(window, cx, move |menu, _, _| {
                                let mut menu = menu.context(focus_handle).header("Open as");
                                for format in [
                                    ResultFormat::Csv,
                                    ResultFormat::Json,
                                    ResultFormat::Markdown,
                                ] {
                                    menu = menu.action(
                                        format_name(format),
                                        Box::new(ExportResults { format }),
                                    );
                                }
                                menu = menu.separator().header("Copy as");
                                for format in [
                                    ResultFormat::Csv,
                                    ResultFormat::Json,
                                    ResultFormat::Markdown,
                                ] {
                                    menu = menu.action(
                                        format_name(format),
                                        Box::new(CopyResults { format }),
                                    );
                                }
                                menu
                            }))
                        }),
                )
            })
    }
}

fn format_name(format: ResultFormat) -> &'static str {
    match format {
        ResultFormat::Csv => "CSV",
        ResultFormat::Json => "JSON",
        ResultFormat::Markdown => "Markdown",
    }
}

fn format_rows(count: usize) -> String {
    let digits = count.to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("{grouped} row{}", if count == 1 { "" } else { "s" })
}

fn format_duration(duration: std::time::Duration) -> String {
    if duration.as_millis() < 1000 {
        format!("{} ms", duration.as_millis())
    } else {
        format!("{:.2} s", duration.as_secs_f64())
    }
}

fn table_contents(columns: &[ColumnMeta]) -> TableLikeContent {
    let mut contents = TableLikeContent::default();
    contents.number_of_cols = columns.len();
    contents.headers = TableRow::from_element(TableCell::Virtual, 0);
    if let Ok(headers) = TableRow::try_from_vec(
        columns
            .iter()
            .map(|column| TableCell::Generated(column.name.clone()))
            .collect(),
        columns.len(),
    ) {
        contents.headers = headers;
    }
    contents.column_kinds = columns
        .iter()
        .map(|column| match column.kind {
            ValueKind::Number => ColumnKind::Number,
            ValueKind::Boolean => ColumnKind::Boolean,
            ValueKind::DateTime => ColumnKind::DateTime,
            ValueKind::Text | ValueKind::Binary => ColumnKind::Text,
        })
        .collect();
    contents
}

fn table_row(row: &ResultRow, cols: usize) -> Option<TableRow<TableCell>> {
    let cells = row
        .iter()
        .map(|value| match value {
            Some(value) => TableCell::Generated(value.clone()),
            None => TableCell::Null,
        })
        .collect();
    match TableRow::try_from_vec(cells, cols) {
        Ok(row) => Some(row),
        Err(error) => {
            log::error!("dropping a malformed result row: {error}");
            None
        }
    }
}

/// Opens text in a new, unsaved editor tab with the given language.
pub(crate) fn open_text_in_editor(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    text: String,
    language_name: &'static str,
    connection: Option<ConnectionKey>,
    window: &mut Window,
    cx: &mut App,
) {
    let language = project
        .read(cx)
        .languages()
        .language_for_name(language_name);
    window
        .spawn(cx, async move |cx| {
            let language = language.await.ok();
            let buffer = project
                .update(cx, |project, cx| project.create_buffer(language, true, cx))
                .await?;
            buffer.update(cx, |buffer, cx| buffer.set_text(text, cx));
            workspace.update_in(cx, |workspace, window, cx| {
                let editor =
                    cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
                if let Some(connection) = connection {
                    let root = crate::sql_editor::worktree_root(&project, cx);
                    DbStore::global(cx).update(cx, |store, cx| {
                        store.set_editor_connection(editor.entity_id(), root, connection, cx)
                    });
                }
                workspace.add_item_to_center(Box::new(editor), window, cx);
            })?;
            Result::<()>::Ok(())
        })
        .detach_and_log_err(cx);
}

impl Focusable for QueryResultsItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.content == Content::Table {
            self.table.focus_handle(cx)
        } else {
            self.focus_handle.clone()
        }
    }
}

impl Render for QueryResultsItem {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let error = self.run.as_ref().and_then(|run| match &run.read(cx).state {
            QueryState::Failed(message) => Some(message.clone()),
            _ => None,
        });
        let body = match (&self.content, error) {
            (_, Some(error)) => div()
                .p_4()
                .size_full()
                .child(
                    v_flex()
                        .gap_2()
                        .child(Label::new("The query failed").color(Color::Error))
                        .child(
                            div()
                                .font_buffer(cx)
                                .text_color(cx.theme().status().error)
                                .child(error),
                        ),
                )
                .into_any_element(),
            (Content::Message(message), None) => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(Label::new(message.clone()).color(Color::Muted))
                .into_any_element(),
            (Content::Table | Content::Empty, None) => self.table.clone().into_any_element(),
        };
        v_flex()
            .key_context("QueryResults")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::load_more))
            .on_action(cx.listener(Self::copy_results))
            .on_action(cx.listener(Self::export_results))
            .on_action(cx.listener(Self::open_in_editor))
            .child(self.render_status(cx))
            .child(div().flex_1().min_h_0().child(body))
    }
}

impl Item for QueryResultsItem {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        if self.sql.is_empty() {
            return "Query Results".into();
        }
        statement::summary(&self.sql, 32).into()
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let is_active = self
            .run
            .as_ref()
            .is_some_and(|run| run.read(cx).state.is_active());
        h_flex()
            .gap_1()
            .child(
                Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
                    .color(params.text_color()),
            )
            .when(is_active, |row| row.child(SpinnerLabel::new()))
            .into_any_element()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Table))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        if self.sql.is_empty() {
            return None;
        }
        let mut tooltip: String = self.sql.chars().take(2_000).collect();
        if tooltip.len() < self.sql.len() {
            tooltip.push('…');
        }
        Some(tooltip.into())
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Query Results Opened")
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn is_dirty(&self, _: &App) -> bool {
        self.pending_edit_count() > 0
    }

    fn can_save(&self, _: &App) -> bool {
        self.is_editable()
    }

    fn save(
        &mut self,
        options: workspace::item::SaveOptions,
        _project: Entity<Project>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Task<Result<()>> {
        // Writing to a database must always be a deliberate action.
        if options.autosave {
            return Task::ready(Ok(()));
        }
        self.commit_edits(window, cx)
    }
}
