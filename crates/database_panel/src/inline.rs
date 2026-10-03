//! Small query results shown in the editor, below the statement that produced them.

use std::{collections::HashMap, ops::Range, sync::Arc};

use database_core::{
    ColumnMeta, ConnectionConfig, DbStore, QueryRun, QueryRunEvent, QuerySource, QueryState,
    ResultRow,
};
use editor::{
    Anchor, Editor, MultiBufferSnapshot, ToOffset as _,
    display_map::{
        BlockContext, BlockId, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId,
        RenderBlock,
    },
};
use gpui::{
    App, AppContext as _, BorrowAppContext as _, Context, Entity, EntityId, Global, Subscription,
    Task, WeakEntity,
};
use language::Point;
use project::Project;
use ui::{
    Button, ButtonSize, Color, IconButton, IconButtonShape, IconName, IconSize, Label, LabelSize,
    Tooltip, prelude::*,
};
use util::ResultExt as _;

use crate::{
    connection_modal::{connect_interactively, detach_and_notify_err},
    results::{QueryRequest, ResultOrigin, confirm_writes, run_query},
};

/// Rows shown in an inline result. Larger results can be opened in a tab.
pub const INLINE_ROW_LIMIT: usize = 10;
const MAX_COLUMN_CHARS: usize = 32;

/// Inline results of each editor, by editor.
#[derive(Default)]
struct InlineResults(HashMap<EntityId, Vec<InlineBlock>>);

impl Global for InlineResults {}

struct InlineBlock {
    block_id: CustomBlockId,
    range: Range<Anchor>,
    text: String,
    _view: Entity<InlineResultView>,
}

/// Runs `sql` from `range` of the editor and shows its result below the range.
pub fn run_inline(
    editor: Entity<Editor>,
    config: ConnectionConfig,
    project: Entity<Project>,
    sql: String,
    range: Range<Point>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = editor.read(cx).workspace() else {
        return;
    };
    let confirmation = confirm_writes(&config, &sql, window, cx);
    let workspace = workspace.downgrade();
    let task = window.spawn(cx, {
        let workspace = workspace.clone();
        async move |cx| {
            if !confirmation.await {
                return anyhow::Ok(());
            }
            cx.update(|window, cx| {
                connect_interactively(
                    workspace.clone(),
                    config.clone(),
                    project.clone(),
                    window,
                    cx,
                )
            })?
            .await?;
            cx.update(|_, cx| show_inline(editor, workspace, config, project, sql, range, cx))
        }
    });
    detach_and_notify_err(task, workspace, cx);
}

fn show_inline(
    editor: Entity<Editor>,
    workspace: WeakEntity<workspace::Workspace>,
    config: ConnectionConfig,
    project: Entity<Project>,
    sql: String,
    range: Range<Point>,
    cx: &mut App,
) {
    let run = DbStore::global(cx).update(cx, |store, cx| {
        store.execute(
            config.clone(),
            Some(project.clone()),
            sql.clone(),
            QuerySource::Editor,
            cx,
        )
    });
    let editor_id = editor.entity_id();
    let view = cx.new(|cx| {
        InlineResultView::new(run, editor.downgrade(), workspace, config, project, sql, cx)
    });

    editor.update(cx, |editor, cx| {
        let buffer = editor.buffer().clone();
        let snapshot = buffer.read(cx).snapshot(cx);
        // The block goes below the statement's last line, so that line must end with a newline.
        let next_row_start = Point::new(range.end.row + 1, 0);
        if next_row_start > snapshot.max_point() {
            let end = snapshot.max_point();
            buffer.update(cx, |buffer, cx| buffer.edit([(end..end, "\n")], None, cx));
        }
        let snapshot = buffer.read(cx).snapshot(cx);
        let anchors = snapshot.anchor_before(range.start)..snapshot.anchor_after(range.end);
        let text = snapshot.text_for_range(anchors.clone()).collect::<String>();

        // Running a statement again replaces its previous result.
        let previous = cx.update_global::<InlineResults, _>(|results, _| {
            let blocks = results.0.entry(editor_id).or_default();
            let (replaced, kept): (Vec<_>, Vec<_>) = std::mem::take(blocks)
                .into_iter()
                .partition(|block| ranges_overlap(&block.range, &anchors, &snapshot));
            *blocks = kept;
            replaced
        });
        if !previous.is_empty() {
            editor.remove_blocks(
                previous.iter().map(|block| block.block_id).collect(),
                None,
                cx,
            );
        }

        let block = BlockProperties {
            placement: BlockPlacement::Below(snapshot.anchor_before(range.end)),
            height: Some(1),
            style: BlockStyle::Sticky,
            render: renderer(view.clone(), editor_id),
            priority: 0,
        };
        let block_id = editor.insert_blocks([block], None, cx)[0];
        cx.update_global::<InlineResults, _>(|results, _| {
            results.0.entry(editor_id).or_default().push(InlineBlock {
                block_id,
                range: anchors,
                text,
                _view: view,
            });
        });
    });
}

fn ranges_overlap(a: &Range<Anchor>, b: &Range<Anchor>, snapshot: &MultiBufferSnapshot) -> bool {
    let a = a.start.to_offset(snapshot)..a.end.to_offset(snapshot);
    let b = b.start.to_offset(snapshot)..b.end.to_offset(snapshot);
    a.start <= b.end && b.start <= a.end
}

/// Removes inline results whose statement was edited since it ran.
pub fn invalidate_edited(editor: &mut Editor, cx: &mut Context<Editor>) {
    let editor_id = cx.entity_id();
    if !cx
        .try_global::<InlineResults>()
        .is_some_and(|results| results.0.contains_key(&editor_id))
    {
        return;
    }
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let stale = cx.update_global::<InlineResults, _>(|results, _| {
        let Some(blocks) = results.0.get_mut(&editor_id) else {
            return Vec::new();
        };
        let (stale, kept): (Vec<_>, Vec<_>) =
            std::mem::take(blocks).into_iter().partition(|block| {
                snapshot
                    .text_for_range(block.range.clone())
                    .collect::<String>()
                    != block.text
            });
        *blocks = kept;
        stale
    });
    if !stale.is_empty() {
        editor.remove_blocks(stale.iter().map(|block| block.block_id).collect(), None, cx);
    }
}

/// Removes every inline result of the editor.
pub fn clear(editor: &mut Editor, cx: &mut Context<Editor>) {
    let editor_id = cx.entity_id();
    let blocks = cx.update_global::<InlineResults, _>(|results, _| {
        results.0.remove(&editor_id).unwrap_or_default()
    });
    if !blocks.is_empty() {
        editor.remove_blocks(
            blocks.iter().map(|block| block.block_id).collect(),
            None,
            cx,
        );
    }
}

pub fn init(cx: &mut App) {
    cx.set_global(InlineResults::default());
}

pub fn forget_editor(editor_id: EntityId, cx: &mut App) {
    if cx.has_global::<InlineResults>() {
        cx.update_global::<InlineResults, _>(|results, _| results.0.remove(&editor_id));
    }
}

fn close_block(
    editor_id: EntityId,
    block_id: CustomBlockId,
    editor: WeakEntity<Editor>,
    cx: &mut App,
) {
    cx.update_global::<InlineResults, _>(|results, _| {
        if let Some(blocks) = results.0.get_mut(&editor_id) {
            blocks.retain(|block| block.block_id != block_id);
        }
    });
    editor
        .update(cx, |editor, cx| {
            editor.remove_blocks([block_id].into_iter().collect(), None, cx)
        })
        .log_err();
}

fn renderer(view: Entity<InlineResultView>, editor_id: EntityId) -> RenderBlock {
    Arc::new(move |cx: &mut BlockContext| {
        let gutter = cx.margins.gutter;
        let line_height = cx.window.line_height();
        let block_id = cx.block_id;
        let editor = view.read(cx.app).editor.clone();
        div()
            .id(cx.block_id)
            .block_mouse_except_scroll()
            .flex()
            .items_start()
            .w_full()
            .border_y_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().background)
            .child(
                div()
                    .w(gutter.full_width())
                    .flex()
                    .justify_center()
                    .pt_1()
                    .child(
                        IconButton::new("close-inline-result", IconName::Close)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .size(ButtonSize::Compact)
                            .shape(IconButtonShape::Square)
                            .tooltip(Tooltip::text("Close Result"))
                            .on_click(move |_, _, cx| {
                                if let BlockId::Custom(block_id) = block_id {
                                    close_block(editor_id, block_id, editor.clone(), cx);
                                }
                            }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .py(line_height / 4.)
                    .mr(cx.margins.right)
                    .overflow_x_hidden()
                    .child(view.clone()),
            )
            .into_any_element()
    })
}

pub struct InlineResultView {
    run: Entity<QueryRun>,
    editor: WeakEntity<Editor>,
    workspace: WeakEntity<workspace::Workspace>,
    config: ConnectionConfig,
    project: Entity<Project>,
    sql: String,
    columns: Vec<ColumnMeta>,
    rows: Vec<ResultRow>,
    /// More rows were available than shown.
    truncated: bool,
    _subscription: Subscription,
    _cancel: Option<Task<()>>,
}

impl InlineResultView {
    fn new(
        run: Entity<QueryRun>,
        editor: WeakEntity<Editor>,
        workspace: WeakEntity<workspace::Workspace>,
        config: ConnectionConfig,
        project: Entity<Project>,
        sql: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe(&run, Self::on_run_event);
        Self {
            run,
            editor,
            workspace,
            config,
            project,
            sql,
            columns: Vec::new(),
            rows: Vec::new(),
            truncated: false,
            _subscription: subscription,
            _cancel: None,
        }
    }

    fn on_run_event(
        &mut self,
        run: Entity<QueryRun>,
        event: &QueryRunEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            QueryRunEvent::ResultSet(columns) => {
                self.columns = columns.clone();
                self.rows.clear();
                self.truncated = false;
            }
            QueryRunEvent::Rows(rows) => {
                let room = INLINE_ROW_LIMIT.saturating_sub(self.rows.len());
                self.rows.extend(rows.iter().take(room).cloned());
                if rows.len() > room && !self.truncated {
                    // The rest isn't shown; stop the query instead of fetching it.
                    self.truncated = true;
                    run.update(cx, |run, cx| run.cancel(cx));
                }
            }
            QueryRunEvent::Updated => {}
        }
        cx.notify();
    }

    fn open_in_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        let request = QueryRequest {
            config: self.config.clone(),
            project: self.project.clone(),
            sql: self.sql.clone(),
            origin: ResultOrigin::Editor(editor.entity_id()),
            source: QuerySource::Editor,
            focus: true,
        };
        self.workspace
            .update(cx, |workspace, cx| {
                run_query(workspace, request, window, cx)
            })
            .log_err();
    }

    fn column_widths(&self) -> Vec<usize> {
        self.columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                self.rows
                    .iter()
                    .map(|row| {
                        row.get(index)
                            .and_then(|value| value.as_ref())
                            .map_or(4, |value| value.chars().count())
                    })
                    .chain([column.name.chars().count()])
                    .max()
                    .unwrap_or(4)
                    .clamp(4, MAX_COLUMN_CHARS)
            })
            .collect()
    }
}

impl Render for InlineResultView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let run = self.run.read(cx);
        let em_width = window.text_style().font_size.to_pixels(window.rem_size()) * 0.62;
        let status: SharedString = match &run.state {
            QueryState::Connecting => "Connecting…".into(),
            QueryState::Running => "Running…".into(),
            QueryState::Failed(error) => error.clone(),
            QueryState::Cancelled if self.truncated => {
                format!("First {} rows", self.rows.len()).into()
            }
            QueryState::Cancelled => "Cancelled".into(),
            QueryState::Finished | QueryState::Paused { .. } => {
                if self.columns.is_empty() {
                    match run.rows_affected {
                        Some(rows) => format!("{rows} rows affected").into(),
                        None => "Done".into(),
                    }
                } else {
                    format!(
                        "{} row{}",
                        self.rows.len(),
                        if self.rows.len() == 1 { "" } else { "s" }
                    )
                    .into()
                }
            }
        };
        let status_color = match &run.state {
            QueryState::Failed(_) => Color::Error,
            _ => Color::Muted,
        };
        let duration = format!("{} ms", run.duration().as_millis());
        let widths = self.column_widths();
        let cell = |text: SharedString, chars: usize, color: Color| {
            div()
                .w(em_width * chars as f32)
                .flex_none()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(
                    Label::new(text)
                        .size(LabelSize::Small)
                        .color(color)
                        .buffer_font(cx),
                )
        };

        v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(status)
                            .size(LabelSize::Small)
                            .color(status_color),
                    )
                    .child(
                        Label::new(duration)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .when(self.truncated, |row| {
                        row.child(
                            Button::new("open-in-tab", "Open in Tab")
                                .size(ButtonSize::Compact)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.open_in_tab(window, cx)),
                                ),
                        )
                    }),
            )
            .when(!self.columns.is_empty(), |table| {
                table
                    .child(h_flex().gap_3().children(
                        self.columns.iter().zip(&widths).map(|(column, width)| {
                            cell(column.name.clone(), *width, Color::Default)
                        }),
                    ))
                    .children(self.rows.iter().map(|row| {
                        h_flex()
                            .gap_3()
                            .children(row.iter().zip(&widths).map(|(value, width)| match value {
                                Some(value) => cell(value.clone(), *width, Color::Muted),
                                None => cell("NULL".into(), *width, Color::Disabled),
                            }))
                    }))
            })
    }
}

#[cfg(test)]
pub(crate) fn inline_block_count(editor: EntityId, cx: &App) -> usize {
    cx.try_global::<InlineResults>()
        .and_then(|results| results.0.get(&editor))
        .map_or(0, Vec::len)
}

#[cfg(test)]
pub(crate) fn inline_rows(editor: EntityId, cx: &App) -> Vec<Vec<Option<String>>> {
    cx.try_global::<InlineResults>()
        .and_then(|results| results.0.get(&editor))
        .and_then(|blocks| blocks.last())
        .map(|block| {
            block
                ._view
                .read(cx)
                .rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|value| value.as_ref().map(|value| value.to_string()))
                        .collect()
                })
                .collect()
        })
        .unwrap_or_default()
}
