//! Running SQL from editors, and choosing the connection an editor runs against.

use std::{any::TypeId, ops::Range, path::Path, sync::Arc, time::Duration};

use database_core::{
    ConnectionConfig, ConnectionKey, DbStore, DbStoreEvent, DriverKind, QuerySource, statement,
};
use editor::{Editor, RowHighlightOptions};
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    App, DismissEvent, DispatchPhase, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    Subscription, Task, WeakEntity,
};
use language::{Buffer, BufferSnapshot, Point};
use picker::{Picker, PickerDelegate};
use ui::{
    Button, ButtonCommon, ButtonSize, ButtonStyle, Color, Icon, IconButton, IconName, IconSize,
    Label, LabelSize, ListItem, ListItemSpacing, Tooltip, prelude::*,
};
use workspace::{
    ItemHandle, ModalView, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace,
};

use crate::{
    CancelQuery, ClearInlineResults, ExplainQuery, QueryHistory, RunEmbeddedQuery, RunQuery,
    RunQueryInline, RunSelection, SelectConnection,
    results::{QueryRequest, QueryResultsItem, ResultOrigin, run_query},
};

/// Marks the statement being run, briefly.
enum ExecutedStatement {}

const FLASH_DURATION: Duration = Duration::from_millis(400);

pub fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, window, cx| {
        if window.is_none() || !editor.mode().is_full() || !editor.buffer().read(cx).is_singleton()
        {
            return;
        }
        editor
            .register_action_renderer(|editor, window, cx| {
                if !crate::is_enabled(cx)
                    || editor
                        .project()
                        .is_none_or(|project| project.read(cx).is_via_collab())
                {
                    return;
                }
                let editor_handle = cx.entity().downgrade();
                let register =
                    |window: &mut Window,
                     action: TypeId,
                     handler: fn(WeakEntity<Editor>, &mut Window, &mut App)| {
                        let editor_handle = editor_handle.clone();
                        window.on_action(action, move |_, phase, window, cx| {
                            if phase == DispatchPhase::Bubble {
                                handler(editor_handle.clone(), window, cx);
                            }
                        });
                    };
                register(
                    window,
                    TypeId::of::<ClearInlineResults>(),
                    |editor, _, cx| {
                        editor
                            .update(cx, |editor, cx| crate::inline::clear(editor, cx))
                            .ok();
                    },
                );
                if !is_sql_editor(editor, cx) {
                    // In application code, SQL lives in strings.
                    register(window, TypeId::of::<RunEmbeddedQuery>(), run_embedded);
                    return;
                }
                register(
                    window,
                    TypeId::of::<RunQueryInline>(),
                    |editor, window, cx| run_from_editor(editor, RunScope::Inline, window, cx),
                );
                register(window, TypeId::of::<RunQuery>(), |editor, window, cx| {
                    run_from_editor(editor, RunScope::Statement, window, cx)
                });
                register(
                    window,
                    TypeId::of::<RunSelection>(),
                    |editor, window, cx| {
                        run_from_editor(editor, RunScope::SelectionOrFile, window, cx)
                    },
                );
                register(
                    window,
                    TypeId::of::<ExplainQuery>(),
                    |editor, window, cx| run_from_editor(editor, RunScope::Explain, window, cx),
                );
                register(window, TypeId::of::<CancelQuery>(), cancel_from_editor);
                register(
                    window,
                    TypeId::of::<SelectConnection>(),
                    |editor, window, cx| {
                        if let Some(editor) = editor.upgrade() {
                            ConnectionPicker::toggle(&editor, None, window, cx);
                        }
                    },
                );
                register(
                    window,
                    TypeId::of::<QueryHistory>(),
                    |editor, window, cx| {
                        if let Some(editor) = editor.upgrade()
                            && let Some(config) = connection_for_editor(&editor, cx)
                        {
                            crate::history::HistoryPicker::toggle(&editor, config, window, cx);
                        }
                    },
                );
            })
            .detach();
        crate::sql_completion::register(editor, window, cx);
        let is_collab = editor
            .project()
            .is_some_and(|project| project.read(cx).is_via_collab());
        if !is_collab && let Some(buffer) = editor.buffer().read(cx).as_singleton() {
            editor.register_addon(SqlEditorAddon {
                buffer: buffer.downgrade(),
            });
            if is_sql_editor(editor, cx)
                && let Some(root) = editor_worktree_root(editor, cx)
                && let Some(store) = DbStore::try_global(cx)
            {
                store.update(cx, |store, cx| store.load_worktree_connection(root, cx));
            }
        }

        let editor_id = cx.entity_id();
        cx.on_release(move |_, cx| {
            if let Some(store) = DbStore::try_global(cx) {
                store.update(cx, |store, _| store.forget_editor(editor_id));
            }
            crate::inline::forget_editor(editor_id, cx);
        })
        .detach();
        cx.subscribe_self(|editor, event: &editor::EditorEvent, cx| {
            if let editor::EditorEvent::BufferEdited = event {
                crate::inline::invalidate_edited(editor, cx);
            }
        })
        .detach();
    })
    .detach();
}

/// Whether the editor holds SQL: its language is SQL (as provided by the SQL extension), or its
/// file has a `.sql` extension, so that SQL files can be run without the extension installed.
pub fn is_sql_editor(editor: &Editor, cx: &App) -> bool {
    if editor
        .project()
        .is_some_and(|project| project.read(cx).is_via_collab())
    {
        return false;
    }
    editor
        .buffer()
        .read(cx)
        .as_singleton()
        .is_some_and(|buffer| is_sql_buffer(buffer.read(cx)))
}

fn is_sql_buffer(buffer: &Buffer) -> bool {
    if buffer
        .language()
        .is_some_and(|language| language.name().as_ref().eq_ignore_ascii_case("sql"))
    {
        return true;
    }
    buffer.file().is_some_and(|file| {
        file.path()
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("sql"))
    })
}

/// Adds the `sql_editor` key context, so that SQL keybindings also apply to unsaved buffers
/// whose language is SQL. Holds the buffer rather than the editor, because key contexts are
/// computed while the editor is being updated.
struct SqlEditorAddon {
    buffer: WeakEntity<Buffer>,
}

impl editor::Addon for SqlEditorAddon {
    fn extend_key_context(&self, context: &mut gpui::KeyContext, cx: &App) {
        if crate::is_enabled(cx)
            && self
                .buffer
                .upgrade()
                .is_some_and(|buffer| is_sql_buffer(buffer.read(cx)))
        {
            context.add("sql_editor");
        }
    }

    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// The root of the worktree that an editor's queries resolve against: the worktree of its file,
/// or the project's first one for untitled buffers.
pub(crate) fn editor_worktree_root(editor: &Editor, cx: &App) -> Option<Arc<Path>> {
    let project = editor.project()?.read(cx);
    let file_worktree = editor
        .buffer()
        .read(cx)
        .as_singleton()
        .and_then(|buffer| buffer.read(cx).file().map(|file| file.worktree_id(cx)))
        .and_then(|worktree_id| project.worktree_for_id(worktree_id, cx))
        .filter(|worktree| worktree.read(cx).is_visible());
    file_worktree
        .or_else(|| project.visible_worktrees(cx).next())
        .map(|worktree| worktree.read(cx).abs_path())
}

/// The connection an editor runs queries against: the one chosen for it, the one last chosen
/// in its worktree, or the only one available.
pub(crate) fn connection_for_editor(editor: &Entity<Editor>, cx: &App) -> Option<ConnectionConfig> {
    let project = editor.read(cx).project()?.clone();
    let connections = DbStore::connections_for_project(&project, cx);
    let store = DbStore::global(cx);
    let store = store.read(cx);
    let find = |key: &ConnectionKey| {
        connections
            .iter()
            .find(|connection| &connection.key == key)
            .cloned()
    };
    if let Some(config) = store.editor_connection(editor.entity_id()).and_then(find) {
        return Some(config);
    }
    if let Some(config) = editor_worktree_root(editor.read(cx), cx)
        .and_then(|root| store.worktree_connection(&root).cloned())
        .and_then(|key| find(&key))
    {
        return Some(config);
    }
    (connections.len() == 1).then(|| connections[0].clone())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunScope {
    /// The selection, or the statement under the cursor.
    Statement,
    /// The selection, or the whole file.
    SelectionOrFile,
    /// The execution plan of the selection or the statement under the cursor.
    Explain,
    /// The selection or statement, with its result shown below it.
    Inline,
}

fn run_from_editor(editor: WeakEntity<Editor>, scope: RunScope, window: &mut Window, cx: &mut App) {
    let Some(editor) = editor.upgrade() else {
        return;
    };
    let Some(config) = connection_for_editor(&editor, cx) else {
        // Choose a connection first, then run.
        ConnectionPicker::toggle(
            &editor,
            Some(Box::new(move |editor, window, cx| {
                run_from_editor(editor.downgrade(), scope, window, cx)
            })),
            window,
            cx,
        );
        return;
    };
    let Some((sql, range)) = editor.update(cx, |editor, cx| sql_to_run(editor, scope, cx)) else {
        return;
    };
    let sql = match scope {
        RunScope::Explain => match explain_statement(config.driver, &sql) {
            Some(sql) => sql,
            None => return,
        },
        RunScope::Statement | RunScope::SelectionOrFile | RunScope::Inline => sql,
    };
    flash(&editor, range.clone(), cx);
    if scope == RunScope::Inline {
        if let Some(project) = editor.read(cx).project().cloned() {
            crate::inline::run_inline(editor, config, project, sql, range, window, cx);
        }
        return;
    }

    let Some(workspace) = editor.read(cx).workspace() else {
        return;
    };
    let Some(project) = editor.read(cx).project().cloned() else {
        return;
    };
    let editor_id = editor.entity_id();
    workspace.update(cx, |workspace, cx| {
        if scope == RunScope::Explain {
            crate::explain::show_explain(config, project, sql, window, cx);
            return;
        }
        run_query(
            workspace,
            QueryRequest {
                config,
                project,
                sql,
                origin: ResultOrigin::Editor(editor_id),
                source: QuerySource::Editor,
                focus: false,
            },
            window,
            cx,
        );
    });
}

/// Wraps a statement so the database returns its execution plan without running it.
fn explain_statement(driver: DriverKind, sql: &str) -> Option<String> {
    let statements = statement::split_statements(sql);
    let [range] = statements.as_slice() else {
        return None;
    };
    let statement = &sql[range.clone()];
    Some(match driver {
        DriverKind::Postgres => format!("EXPLAIN (FORMAT JSON) {statement}"),
        DriverKind::Mysql => format!("EXPLAIN FORMAT=JSON {statement}"),
        DriverKind::Sqlite => format!("EXPLAIN QUERY PLAN {statement}"),
    })
}

/// Runs the SQL in the string literal (or SQL injection) under the cursor of application code,
/// or the selection.
fn run_embedded(editor: WeakEntity<Editor>, window: &mut Window, cx: &mut App) {
    let Some(editor) = editor.upgrade() else {
        return;
    };
    let Some(config) = connection_for_editor(&editor, cx) else {
        ConnectionPicker::toggle(
            &editor,
            Some(Box::new(|editor, window, cx| {
                run_embedded(editor.downgrade(), window, cx)
            })),
            window,
            cx,
        );
        return;
    };
    let found = editor.update(cx, |editor, cx| {
        let buffer = editor.buffer().read(cx).as_singleton()?;
        let snapshot = buffer.read(cx).snapshot();
        let selection = editor
            .selections
            .newest_adjusted(&editor.display_snapshot(cx));
        let range = if selection.is_empty() {
            embedded_sql_range(&snapshot, snapshot.point_to_offset(selection.head()))?
        } else {
            snapshot.point_to_offset(selection.start)..snapshot.point_to_offset(selection.end)
        };
        let sql = snapshot.text_for_range(range.clone()).collect::<String>();
        Some((
            sql,
            snapshot.offset_to_point(range.start)..snapshot.offset_to_point(range.end),
        ))
    });
    let Some((sql, range)) = found.filter(|(sql, _)| looks_like_sql(sql)) else {
        if let Some(workspace) = editor.read(cx).workspace() {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    workspace::Toast::new(
                        workspace::notifications::NotificationId::unique::<RunEmbeddedQuery>(),
                        "No SQL found at the cursor. Place it inside a string with SQL, or select the SQL.",
                    )
                    .autohide(),
                    cx,
                );
            });
        }
        return;
    };
    flash(&editor, range, cx);
    let (Some(workspace), Some(project)) = (
        editor.read(cx).workspace(),
        editor.read(cx).project().cloned(),
    ) else {
        return;
    };
    let editor_id = editor.entity_id();
    workspace.update(cx, |workspace, cx| {
        run_query(
            workspace,
            QueryRequest {
                config,
                project,
                sql,
                origin: ResultOrigin::Editor(editor_id),
                source: QuerySource::Editor,
                focus: false,
            },
            window,
            cx,
        )
    });
}

/// Whether text starts with a statement keyword, so that unrelated strings aren't sent to the
/// database.
fn looks_like_sql(text: &str) -> bool {
    !text.trim().is_empty() && statement::classify(text) != statement::StatementKind::Other
        || text
            .trim_start()
            .get(..5)
            .is_some_and(|start| start.eq_ignore_ascii_case("begin"))
}

/// The SQL around `offset` in application code: an injected SQL layer (when the SQL language
/// is installed), or the contents of the string literal under the cursor.
pub(crate) fn embedded_sql_range(snapshot: &BufferSnapshot, offset: usize) -> Option<Range<usize>> {
    if let Some(layer) = snapshot.syntax_layer_at(offset)
        && layer.language.name().as_ref().eq_ignore_ascii_case("sql")
    {
        return Some(layer.node().byte_range());
    }
    let mut node = snapshot.syntax_ancestor(offset..offset)?;
    loop {
        let kind = node.kind();
        if kind.contains("string") && !kind.contains("content") && !kind.contains("fragment") {
            let range = node.byte_range();
            let text = snapshot.text_for_range(range.clone()).collect::<String>();
            let (start, end) = string_contents(&text)?;
            return Some(range.start + start..range.start + end);
        }
        node = node.parent()?;
    }
}

/// The byte range of a string literal's contents, without prefixes and quotes, such as for
/// `"…"`, `'…'`, `"""…"""`, `` `…` ``, `r#"…"#` and `f"…"`.
fn string_contents(literal: &str) -> Option<(usize, usize)> {
    let prefix_len = literal.find(['"', '\'', '`'])?;
    let quote = literal[prefix_len..].chars().next()?;
    let hashes = literal[..prefix_len]
        .chars()
        .rev()
        .take_while(|c| *c == '#')
        .count();
    let opening = if literal[prefix_len..].starts_with(&quote.to_string().repeat(3)) {
        3
    } else {
        1
    };
    let start = prefix_len + opening;
    let end = literal.len().checked_sub(opening + hashes)?;
    (start <= end).then_some((start, end))
}

fn cancel_from_editor(editor: WeakEntity<Editor>, _window: &mut Window, cx: &mut App) {
    let Some(editor) = editor.upgrade() else {
        return;
    };
    let Some(workspace) = editor.read(cx).workspace() else {
        return;
    };
    let origin = ResultOrigin::Editor(editor.entity_id());
    let runs = workspace
        .read(cx)
        .items_of_type::<QueryResultsItem>(cx)
        .filter(|item| item.read(cx).origin() == &origin)
        .filter_map(|item| item.read(cx).run().cloned())
        .collect::<Vec<_>>();
    for run in runs {
        run.update(cx, |run, cx| run.cancel(cx));
    }
}

/// The SQL to run and the buffer range it came from.
fn sql_to_run(
    editor: &mut Editor,
    scope: RunScope,
    cx: &mut gpui::Context<Editor>,
) -> Option<(String, Range<Point>)> {
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let snapshot = buffer.read(cx).snapshot();
    let selection = editor
        .selections
        .newest_adjusted(&editor.display_snapshot(cx));
    if !selection.is_empty() {
        let range = selection.range();
        let text = snapshot.text_for_range(range.clone()).collect::<String>();
        return (!text.trim().is_empty()).then_some((text, range));
    }
    match scope {
        RunScope::SelectionOrFile => {
            let text = snapshot.text();
            (!text.trim().is_empty()).then(|| (text, Point::zero()..snapshot.max_point()))
        }
        RunScope::Statement | RunScope::Explain | RunScope::Inline => {
            let cursor = snapshot.point_to_offset(selection.head());
            let range = statement_range(&snapshot, cursor)?;
            let text = snapshot.text_for_range(range.clone()).collect::<String>();
            let range = snapshot.offset_to_point(range.start)..snapshot.offset_to_point(range.end);
            Some((text, range))
        }
    }
}

/// The statement at `offset`. Uses the SQL syntax tree when a grammar is loaded, and falls back
/// to splitting at semicolons and blank lines otherwise, or when the tree has errors there.
pub(crate) fn statement_range(snapshot: &BufferSnapshot, offset: usize) -> Option<Range<usize>> {
    if let Some(range) = syntax_statement_range(snapshot, offset) {
        return Some(range);
    }
    let text = snapshot.text();
    statement::statement_at(&text, offset)
}

fn syntax_statement_range(snapshot: &BufferSnapshot, offset: usize) -> Option<Range<usize>> {
    let mut node = snapshot.syntax_ancestor(offset..offset)?;
    loop {
        if node.has_error() || node.is_error() {
            return None;
        }
        let parent = node.parent()?;
        // Top-level statements are direct children of the root, which the SQL grammar calls
        // `program`. Statements inside `BEGIN ... END` blocks stay part of their block.
        if parent.parent().is_none() {
            return node.kind().contains("statement").then(|| node.byte_range());
        }
        node = parent;
    }
}

fn flash(editor: &Entity<Editor>, range: Range<Point>, cx: &mut App) {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        // Highlight whole lines, including the last one.
        let end_row = range.end.row + u32::from(range.end.column > 0 || range.start == range.end);
        let end = Point::new(end_row, 0).min(snapshot.max_point());
        let anchors =
            snapshot.anchor_before(Point::new(range.start.row, 0))..snapshot.anchor_after(end);
        editor.highlight_rows::<ExecutedStatement>(
            anchors,
            |cx| cx.theme().colors().editor_highlighted_line_background,
            RowHighlightOptions {
                autoscroll: false,
                ..Default::default()
            },
            cx,
        );
        cx.notify();
        cx.spawn(async move |editor, cx| {
            cx.background_executor().timer(FLASH_DURATION).await;
            editor.update(cx, |editor, cx| {
                editor.clear_row_highlights::<ExecutedStatement>();
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    });
}

type OnSelect = Box<dyn FnOnce(Entity<Editor>, &mut Window, &mut App)>;

/// Chooses the connection an editor runs queries against.
pub struct ConnectionPicker {
    picker: Entity<Picker<ConnectionPickerDelegate>>,
}

impl ConnectionPicker {
    pub fn toggle(
        editor: &Entity<Editor>,
        on_select: Option<OnSelect>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(workspace) = editor.read(cx).workspace() else {
            return;
        };
        let Some(project) = editor.read(cx).project().cloned() else {
            return;
        };
        let connections = DbStore::connections_for_project(&project, cx);
        if connections.is_empty() {
            workspace.update(cx, |workspace, cx| {
                crate::connection_modal::ConnectionModal::toggle(workspace, None, window, cx);
            });
            return;
        }
        let current = connection_for_editor(editor, cx).map(|config| config.key);
        let editor = editor.downgrade();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, move |window, cx| {
                let this = cx.entity().downgrade();
                let delegate = ConnectionPickerDelegate {
                    this,
                    editor,
                    matches: Vec::new(),
                    connections,
                    current,
                    selected_index: 0,
                    on_select,
                };
                Self {
                    picker: cx.new(|cx| Picker::uniform_list(delegate, window, cx)),
                }
            });
        });
    }
}

impl EventEmitter<DismissEvent> for ConnectionPicker {}

impl ModalView for ConnectionPicker {}

impl Focusable for ConnectionPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for ConnectionPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        v_flex().w(rems(30.)).child(self.picker.clone())
    }
}

pub struct ConnectionPickerDelegate {
    this: WeakEntity<ConnectionPicker>,
    editor: WeakEntity<Editor>,
    connections: Vec<ConnectionConfig>,
    matches: Vec<StringMatch>,
    current: Option<ConnectionKey>,
    selected_index: usize,
    on_select: Option<OnSelect>,
}

impl PickerDelegate for ConnectionPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "database connection picker"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        _cx: &mut gpui::Context<Picker<Self>>,
    ) {
        self.selected_index = index;
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Run queries against…".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut gpui::Context<Picker<Self>>,
    ) -> Task<()> {
        let candidates = self
            .connections
            .iter()
            .enumerate()
            .map(|(index, connection)| {
                StringMatchCandidate::new(
                    index,
                    &format!("{} {}", connection.key.id, connection.display_target()),
                )
            })
            .collect::<Vec<_>>();
        let executor = cx.background_executor().clone();
        cx.spawn(async move |picker, cx| {
            let matches = if query.is_empty() {
                candidates
                    .iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        score: 0.,
                        positions: Vec::new(),
                        string: candidate.string.clone(),
                    })
                    .collect()
            } else {
                fuzzy::match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    candidates.len(),
                    &Default::default(),
                    executor,
                )
                .await
            };
            picker
                .update(cx, |picker, cx| {
                    let delegate = &mut picker.delegate;
                    delegate.selected_index = matches
                        .iter()
                        .position(|candidate| {
                            delegate
                                .connections
                                .get(candidate.candidate_id)
                                .map(|c| &c.key)
                                == delegate.current.as_ref()
                        })
                        .unwrap_or(0);
                    delegate.matches = matches;
                    cx.notify();
                })
                .ok();
        })
    }

    fn confirm(
        &mut self,
        _secondary: bool,
        window: &mut Window,
        cx: &mut gpui::Context<Picker<Self>>,
    ) {
        let Some(connection) = self
            .matches
            .get(self.selected_index)
            .and_then(|candidate| self.connections.get(candidate.candidate_id))
        else {
            return;
        };
        let key = connection.key.clone();
        if let Some(editor) = self.editor.upgrade() {
            let root = editor_worktree_root(editor.read(cx), cx);
            let editor_id = editor.entity_id();
            DbStore::global(cx).update(cx, |store, cx| {
                store.set_editor_connection(editor_id, root, key, cx)
            });
            if let Some(on_select) = self.on_select.take() {
                window.defer(cx, move |window, cx| on_select(editor, window, cx));
            }
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut gpui::Context<Picker<Self>>) {
        self.this.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut gpui::Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let candidate = self.matches.get(index)?;
        let connection = self.connections.get(candidate.candidate_id)?;
        let is_current = self.current.as_ref() == Some(&connection.key);
        let status = DbStore::global(cx).read(cx).status(&connection.key);
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(crate::panel::status_indicator(&status))
                .child(
                    h_flex()
                        .gap_2()
                        .child(Label::new(connection.key.id.clone()))
                        .child(
                            Label::new(connection.display_target())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
                .end_slot(
                    h_flex()
                        .gap_1()
                        .child(crate::panel::environment_label(connection))
                        .when(is_current, |row| {
                            row.child(Icon::new(IconName::Check).size(IconSize::Small))
                        }),
                ),
        )
    }
}

/// Shows the connection and run controls for SQL editors.
pub struct SqlEditorToolbar {
    editor: Option<WeakEntity<Editor>>,
    _subscriptions: Vec<Subscription>,
}

impl SqlEditorToolbar {
    pub fn new(cx: &mut gpui::Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(store) = DbStore::try_global(cx) {
            subscriptions.push(cx.subscribe(&store, |_, _, event: &DbStoreEvent, cx| {
                if matches!(
                    event,
                    DbStoreEvent::ConnectionChanged(_)
                        | DbStoreEvent::EditorConnectionChanged(_)
                        | DbStoreEvent::WorktreeConnectionChanged(_)
                ) {
                    cx.notify();
                }
            }));
        }
        Self {
            editor: None,
            _subscriptions: subscriptions,
        }
    }

    fn location(&self, cx: &App) -> ToolbarItemLocation {
        let is_sql = self
            .editor
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .is_some_and(|editor| is_sql_editor(editor.read(cx), cx));
        if is_sql && crate::is_enabled(cx) {
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }
}

impl EventEmitter<ToolbarItemEvent> for SqlEditorToolbar {}

impl ToolbarItemView for SqlEditorToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> ToolbarItemLocation {
        self._subscriptions.truncate(1);
        self.editor = None;
        if let Some(editor) = active_pane_item.and_then(|item| item.downcast::<Editor>()) {
            // The language can change after the editor is activated, e.g. once detected.
            self._subscriptions.push(cx.observe(&editor, |this, _, cx| {
                let location = this.location(cx);
                cx.emit(ToolbarItemEvent::ChangeLocation(location));
                cx.notify();
            }));
            self.editor = Some(editor.downgrade());
        }
        self.location(cx)
    }
}

impl Render for SqlEditorToolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let Some(editor) = self.editor.as_ref().and_then(WeakEntity::upgrade) else {
            return div().into_any_element();
        };
        if !crate::is_enabled(cx) || !is_sql_editor(editor.read(cx), cx) {
            return div().into_any_element();
        }
        let connection = connection_for_editor(&editor, cx);
        let editor_id = editor.entity_id();
        let is_running = editor
            .read(cx)
            .workspace()
            .is_some_and(|workspace| is_editor_query_running(&workspace, editor_id, cx));
        let focus_handle = editor.focus_handle(cx);

        h_flex()
            .gap_1()
            .child(
                Button::new(
                    "sql-connection",
                    connection
                        .as_ref()
                        .map_or("Choose Connection".into(), |connection| {
                            connection.key.id.to_string()
                        }),
                )
                .size(ButtonSize::Compact)
                .style(ButtonStyle::Subtle)
                .when_some(connection.as_ref(), |button, connection| {
                    button.start_icon(crate::panel::environment_icon(connection))
                })
                .tooltip({
                    let focus_handle = focus_handle.clone();
                    move |_, cx| {
                        Tooltip::for_action_in(
                            "Choose Connection",
                            &SelectConnection,
                            &focus_handle,
                            cx,
                        )
                    }
                })
                .on_click(move |_, window, cx| ConnectionPicker::toggle(&editor, None, window, cx)),
            )
            .map(|row| {
                if is_running {
                    row.child(
                        IconButton::new("sql-cancel", IconName::Stop)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Error)
                            .tooltip({
                                let focus_handle = focus_handle.clone();
                                move |_, cx| {
                                    Tooltip::for_action_in(
                                        "Cancel Query",
                                        &CancelQuery,
                                        &focus_handle,
                                        cx,
                                    )
                                }
                            })
                            .on_click({
                                let focus_handle = focus_handle.clone();
                                move |_, window, cx| {
                                    focus_handle.dispatch_action(&CancelQuery, window, cx)
                                }
                            }),
                    )
                } else {
                    row.child(
                        IconButton::new("sql-run", IconName::PlayFilled)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Success)
                            .tooltip({
                                let focus_handle = focus_handle.clone();
                                move |_, cx| {
                                    Tooltip::for_action_in(
                                        "Run Statement",
                                        &RunQuery,
                                        &focus_handle,
                                        cx,
                                    )
                                }
                            })
                            .on_click({
                                let focus_handle = focus_handle.clone();
                                move |_, window, cx| {
                                    focus_handle.dispatch_action(&RunQuery, window, cx)
                                }
                            }),
                    )
                }
            })
            .into_any_element()
    }
}

fn is_editor_query_running(workspace: &Entity<Workspace>, editor_id: EntityId, cx: &App) -> bool {
    let origin = ResultOrigin::Editor(editor_id);
    workspace
        .read(cx)
        .items_of_type::<QueryResultsItem>(cx)
        .any(|item| {
            let item = item.read(cx);
            item.origin() == &origin && item.run().is_some_and(|run| run.read(cx).state.is_active())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_string_contents() {
        fn contents(literal: &str) -> Option<&str> {
            string_contents(literal).map(|(start, end)| &literal[start..end])
        }
        assert_eq!(contents("\"select 1\""), Some("select 1"));
        assert_eq!(contents("'select 1'"), Some("select 1"));
        assert_eq!(contents("`select ${x}`"), Some("select ${x}"));
        assert_eq!(contents("f\"select {x}\""), Some("select {x}"));
        assert_eq!(
            contents("\"\"\"\n  select 1\n\"\"\""),
            Some("\n  select 1\n")
        );
        assert_eq!(contents("r#\"select \"x\"\"#"), Some("select \"x\""));
        assert!(looks_like_sql(" SELECT * FROM t"));
        assert!(looks_like_sql("with x as (select 1) select * from x"));
        assert!(!looks_like_sql("hello world"));
    }

    #[test]
    fn test_explain_statement() {
        assert_eq!(
            explain_statement(DriverKind::Postgres, "select 1;").as_deref(),
            Some("EXPLAIN (FORMAT JSON) select 1")
        );
        assert_eq!(
            explain_statement(DriverKind::Sqlite, "select 1").as_deref(),
            Some("EXPLAIN QUERY PLAN select 1")
        );
        assert_eq!(
            explain_statement(DriverKind::Mysql, "select 1; select 2"),
            None
        );
    }
}
