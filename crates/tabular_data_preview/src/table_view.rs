//! The reusable tabular-data viewer component.
//!
//! `TableView` owns the [`TableDataEngine`] (client-side filter/sort + display-to-data mapping)
//! and all of the grid rendering over `ui::Table`.
//!
//! It is deliberately source-agnostic: it renders whatever [`TableLikeContent`] it is handed
//! via [`TableView::set_contents`], whether that content comes from the CSV parser or from some
//! other producer (e.g. a database result set).

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use std::{ops::RangeInclusive, sync::Arc};

use gpui::{
    App, AppContext, ClipboardItem, Entity, EventEmitter, FocusHandle, Focusable, ListAlignment,
    ListState, Point, Task, Window,
};
use ui::{
    AbsoluteLength, ResizableColumnsState, SharedString, TableInteractionState,
    TableResizeBehavior, prelude::*, table_row::TableRow,
};

use crate::{
    ActivateFocusedCell, CopySelection, ExtendSelection, MoveFocusedCell, NavigationDirection,
    settings::{RowIdentifiers, TableViewSettings, VerticalAlignment},
    table_data_engine::{DisplayToDataMapping, TableDataEngine, sorting_by_column::AppliedSorting},
    types::{AnyColumn, DataCellId, DataRow, DisplayRow, TableCell, TableLikeContent},
};

/// The keyboard cursor: which cell is currently focused for navigation.
/// Both fields are stored in data-space so the selection survives sort/filter changes.
/// In single-cell navigation, `anchor == focus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellSelection {
    pub anchor: DataCellId,
    pub focus: DataCellId,
}

impl CellSelection {
    pub fn new(anchor: DataCellId, focus: DataCellId) -> Self {
        Self { anchor, focus }
    }

    pub fn single_cell(cell: DataCellId) -> Self {
        Self {
            anchor: cell,
            focus: cell,
        }
    }

    pub fn is_single_cell(&self) -> bool {
        self.anchor == self.focus
    }
}

/// Events emitted by [`TableView`] so that embedding views can react to user interaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableViewEvent {
    SelectionChanged(Option<CellSelection>),
    /// A cell was double-clicked or activated with the keyboard.
    CellActivated(DataCellId),
}

/// Options for embedding a [`TableView`] in a context other than the file preview.
#[derive(Clone, Copy, Debug, Default)]
pub struct TableViewOptions {
    pub row_identifiers: RowIdentifiers,
    pub vertical_alignment: VerticalAlignment,
    pub multiline_cells: bool,
}

#[derive(Debug, Default)]
pub struct PerformanceMetrics {
    /// Map of timing metrics with their duration and measurement time.
    pub timings: HashMap<&'static str, (Duration, Instant)>,
    /// List of display indices that were rendered in the current frame.
    pub rendered_indices: Vec<usize>,
}

impl PerformanceMetrics {
    pub fn record<F, R>(&mut self, name: &'static str, mut f: F) -> R
    where
        F: FnMut() -> R,
    {
        let start_time = Instant::now();
        let ret = f();
        let duration = start_time.elapsed();
        self.timings.insert(name, (duration, Instant::now()));
        ret
    }

    /// Displays all metrics sorted A-Z in format: `{name}: {took}ms {ago}s ago`
    pub fn display(&self) -> String {
        let mut metrics = self.timings.iter().collect::<Vec<_>>();
        metrics.sort_by_key(|&(name, _)| *name);
        metrics
            .iter()
            .map(|(name, (duration, time))| {
                let took = duration.as_secs_f32() * 1000.;
                let ago = time.elapsed().as_secs();
                format!("{name}: {took:.3}ms {ago}s ago")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Get timing for a specific metric
    pub fn get_timing(&self, name: &str) -> Option<Duration> {
        self.timings.get(name).map(|(duration, _)| *duration)
    }
}

pub struct TableView {
    pub(crate) engine: TableDataEngine,
    pub(crate) focus_handle: FocusHandle,
    pub(crate) table_interaction_state: Entity<TableInteractionState>,
    pub(crate) column_widths: Entity<ResizableColumnsState>,
    /// Background task computing the display-to-data mapping after a filter/sort change.
    /// Stored here so that a new change cancels the previous in-flight computation.
    pub(crate) filter_sort_task: Option<Task<()>>,
    pub(crate) settings: TableViewSettings,
    /// Performance metrics for debugging and monitoring grid operations.
    pub(crate) performance_metrics: PerformanceMetrics,
    pub(crate) list_state: ListState,
    /// Cached row height, refreshed from the actual text line height on every render.
    /// Used to size not-yet-rendered rows for the scrollbar without a full `.measure_all()`
    /// pass, so it tracks the real row height instead of a hardcoded guess.
    pub(crate) row_height: Pixels,
    /// Whether the producer feeding this view is currently computing content. While set, the grid
    /// shows a loading indicator instead of the (stale or empty) table.
    pub(crate) is_loading: bool,
    /// The keyboard navigation cursor. `None` until the user first navigates.
    pub(crate) selection: Option<CellSelection>,
    /// Whether `filter_sort_task` is still computing a mapping for the current contents.
    pub(crate) mapping_pending: bool,
    /// Cells drawn as modified, such as edits that weren't saved yet.
    pub(crate) marked_cells: std::collections::HashSet<DataCellId>,
}

impl EventEmitter<TableViewEvent> for TableView {}

impl TableView {
    pub fn new(window: &Window, cx: &mut Context<Self>) -> Self {
        let contents = TableLikeContent::default();
        let table_interaction_state = cx.new(|cx| {
            TableInteractionState::new(cx).with_custom_scrollbar(ui::Scrollbars::for_settings::<
                editor::EditorSettingsScrollbarProxy,
            >())
        });
        let row_height = window.pixel_snap(window.line_height());

        Self {
            engine: TableDataEngine::default(),
            focus_handle: cx.focus_handle(),
            table_interaction_state,
            column_widths: cx.new(|_cx| {
                ResizableColumnsState::new(
                    1,
                    vec![AbsoluteLength::Pixels(px(150.))],
                    vec![TableResizeBehavior::Resizable],
                )
            }),
            filter_sort_task: None,
            settings: TableViewSettings::default(),
            performance_metrics: PerformanceMetrics::default(),
            list_state: gpui::ListState::new(contents.rows.len(), ListAlignment::Top, px(1.))
                .with_uniform_item_height(row_height + px(1.0)),
            row_height,
            is_loading: false,
            selection: None,
            mapping_pending: false,
            marked_cells: Default::default(),
        }
    }

    pub fn with_options(mut self, options: TableViewOptions) -> Self {
        self.settings.numbering_type = options.row_identifiers;
        self.settings.vertical_alignment = options.vertical_alignment;
        self.settings.multiline_cells_enabled = options.multiline_cells;
        self
    }

    pub fn contents(&self) -> &Arc<TableLikeContent> {
        &self.engine.contents
    }

    pub fn cell(&self, cell: DataCellId) -> Option<&TableCell> {
        self.engine.contents.get_row(cell.row)?.get(cell.col)
    }

    /// Replaces the value of one cell, keeping sorting, filters and the scroll position.
    pub fn set_cell(
        &mut self,
        cell: DataCellId,
        value: TableCell,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        self.engine.set_cell(cell, value)?;
        cx.notify();
        Ok(())
    }

    /// Draws the given cells as modified.
    pub fn set_marked_cells(
        &mut self,
        cells: impl IntoIterator<Item = DataCellId>,
        cx: &mut Context<Self>,
    ) {
        self.marked_cells = cells.into_iter().collect();
        cx.notify();
    }

    pub fn is_loading(&self) -> bool {
        self.is_loading
    }

    pub fn sorting(&self) -> Option<AppliedSorting> {
        self.engine.applied_sorting
    }

    pub fn set_sorting(&mut self, sorting: Option<AppliedSorting>, cx: &mut Context<Self>) {
        self.engine.applied_sorting = sorting;
        self.apply_filter_sort(cx);
        cx.notify();
    }

    /// Clears sorting and filters. Useful when the same columns now hold unrelated data, such as
    /// a re-executed query.
    pub fn reset_view_state(&mut self, cx: &mut Context<Self>) {
        self.engine.reset_view_state();
        self.apply_filter_sort(cx);
        cx.notify();
    }

    pub fn selection(&self) -> Option<CellSelection> {
        self.selection
    }

    pub fn set_selection(&mut self, selection: Option<CellSelection>, cx: &mut Context<Self>) {
        if self.selection != selection {
            self.selection = selection;
            cx.emit(TableViewEvent::SelectionChanged(selection));
            cx.notify();
        }
    }

    pub fn data_row(&self, display_row: DisplayRow) -> Option<DataRow> {
        self.engine.d2d_mapping().get_data_row(display_row)
    }

    pub fn display_row(&self, data_row: DataRow) -> Option<DisplayRow> {
        self.engine.d2d_mapping().get_display_row(data_row)
    }

    /// Number of rows currently shown, after filtering.
    pub fn visible_row_count(&self) -> usize {
        self.engine.d2d_mapping().visible_row_count()
    }

    /// Data rows in display order (after sorting and filtering).
    pub fn displayed_data_rows(&self) -> impl Iterator<Item = DataRow> + '_ {
        (0..self.visible_row_count()).filter_map(|row| self.data_row(DisplayRow(row)))
    }

    /// Appends rows to the end of the current contents, keeping the scroll position.
    ///
    /// Unlike [`Self::set_contents`], this doesn't reset filters, the selection or the scroll
    /// position, which makes it suitable for streaming results in batches.
    pub fn append_rows(
        &mut self,
        rows: Vec<TableRow<TableCell>>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let number_of_cols = self.engine.contents.number_of_cols;
        if let Some(row) = rows.iter().find(|row| row.cols() != number_of_cols) {
            anyhow::bail!(
                "Expected appended rows to have {number_of_cols} columns, got {}",
                row.cols()
            );
        }
        if rows.is_empty() {
            return Ok(());
        }

        let previous_row_digits = digit_count(self.engine.contents.rows.len());
        let new_rows = self.engine.append_rows(rows);

        if self.mapping_pending || self.engine.has_any_filter_or_sort() {
            self.apply_filter_sort_preserving_scroll(cx);
        } else {
            let old_count = self.list_state.item_count();
            self.engine.extend_identity_mapping(new_rows.clone());
            self.list_state
                .splice(old_count..old_count, new_rows.end - new_rows.start);
            // Freshly spliced items have no size hint. Re-applying the uniform height gives the
            // scrollbar a correct total height without measuring every row.
            self.list_state = self
                .list_state
                .clone()
                .with_uniform_item_height(self.row_height + px(1.0));
        }

        if digit_count(self.engine.contents.rows.len()) != previous_row_digits {
            self.sync_column_widths(cx);
        }
        cx.notify();
        Ok(())
    }

    /// Replace the data shown by the grid. Recomputes filter menus and column widths, kicks off the
    /// display-to-data recomputation, and clears the loading state.
    pub fn set_contents(&mut self, contents: TableLikeContent, cx: &mut Context<Self>) {
        let selection_out_of_bounds = self.selection.is_some_and(|selection| {
            [selection.anchor, selection.focus].iter().any(|cell| {
                *cell.row >= contents.rows.len() || *cell.col >= contents.number_of_cols
            })
        });
        if selection_out_of_bounds {
            self.set_selection(None, cx);
        }
        self.engine.set_contents(contents);
        // The old mapping may reference rows removed by this change. Clear it immediately
        // rather than leaving the list showing stale rows until the background task below
        // recomputes the mapping.
        self.list_state
            .reset_with_uniform_height(0, self.row_height + px(1.0));
        self.sync_column_widths(cx);
        self.is_loading = false;
        self.apply_filter_sort(cx);
    }

    /// Toggle the loading indicator (shown while a producer computes new content).
    pub fn set_loading(&mut self, is_loading: bool, cx: &mut Context<Self>) {
        self.is_loading = is_loading;
        cx.notify();
    }

    pub(crate) fn sync_column_widths(&self, cx: &mut Context<Self>) {
        // plus 1 for the row identifier column
        let cols = self.engine.contents.headers.cols() + 1;
        let line_number_width = self.calculate_row_identifier_column_width();

        let mut widths: Vec<AbsoluteLength> = vec![AbsoluteLength::Pixels(px(150.)); cols];
        widths[0] = AbsoluteLength::Pixels(px(line_number_width));

        let mut resize_behaviors = vec![TableResizeBehavior::Resizable; cols];
        resize_behaviors[0] = TableResizeBehavior::None;

        self.column_widths.update(cx, |state, _cx| {
            if state.cols() != cols {
                *state = ResizableColumnsState::new(cols, widths, resize_behaviors);
            } else {
                state.set_column_configuration(
                    0,
                    AbsoluteLength::Pixels(px(line_number_width)),
                    TableResizeBehavior::None,
                );
            }
        });
    }

    pub fn clear_filters(&mut self, col: AnyColumn, cx: &mut Context<Self>) {
        self.engine.clear_filters_for_col(col);
        self.apply_filter_sort(cx);
    }

    pub fn toggle_filter(
        &mut self,
        col: AnyColumn,
        value: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) = self.engine.toggle_filter(col, value) {
            log::error!("Failed to toggle filter: {err}");
            return;
        }
        self.apply_filter_sort(cx);
    }

    /// Spawns a background task to recompute the display-to-data mapping after a filter or sort
    /// change. Storing the task cancels any previous in-flight computation automatically.
    pub(crate) fn apply_filter_sort(&mut self, cx: &mut Context<Self>) {
        self.recompute_mapping(false, cx);
    }

    fn apply_filter_sort_preserving_scroll(&mut self, cx: &mut Context<Self>) {
        self.recompute_mapping(true, cx);
    }

    fn recompute_mapping(&mut self, preserve_scroll: bool, cx: &mut Context<Self>) {
        let contents = self.engine.contents.clone();
        let filter_stack = self.engine.filter_stack.clone();
        let sorting = self.engine.applied_sorting;
        self.mapping_pending = true;

        self.filter_sort_task = Some(cx.spawn(async move |this, cx| {
            let mapping = cx
                .background_spawn(async move {
                    DisplayToDataMapping::compute(&contents, &filter_stack, sorting)
                })
                .await;

            this.update(cx, |view, cx| {
                view.engine.set_d2d_mapping(mapping);
                view.mapping_pending = false;
                let visible_rows = view.engine.d2d_mapping().visible_row_count();
                let scroll_top = view.list_state.logical_scroll_top();
                // Uses the row height measured on the last render. Cheaper than a full
                // `.measure_all()` pass; exact row heights are re-measured on scrolling.
                view.list_state
                    .reset_with_uniform_height(visible_rows, view.row_height + px(1.0));
                if preserve_scroll && scroll_top.item_ix < visible_rows {
                    view.list_state.scroll_to(scroll_top);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(crate) fn move_focused_cell(
        &mut self,
        action: &MoveFocusedCell,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let row_count = self.engine.d2d_mapping().visible_row_count();
        let column_count = self.engine.contents.number_of_cols;

        if row_count == 0 || column_count == 0 {
            return;
        }

        let current_pos = self.selection.as_ref().and_then(|selection| {
            self.engine
                .d2d_mapping()
                .get_display_row(selection.focus.row)
                .map(|r| (r.0, selection.focus.col.0))
        });

        let (new_row, new_column) = match current_pos {
            Some(pos) => self.compute_move(pos, action.direction),
            None => (0, 0),
        };

        let Some(new_data_row) = self.engine.d2d_mapping().get_data_row(DisplayRow(new_row)) else {
            return;
        };
        let new_cell = DataCellId::new(new_data_row, AnyColumn(new_column));
        self.set_selection(Some(CellSelection::single_cell(new_cell)), cx);

        self.scroll_to_reveal_row(new_row, action.direction);
        self.scroll_to_reveal_column(new_column, window, cx);
        cx.notify();
    }

    /// Moves the selection focus while keeping its anchor, selecting a rectangle of cells.
    pub(crate) fn extend_selection(
        &mut self,
        action: &ExtendSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let row_count = self.engine.d2d_mapping().visible_row_count();
        let column_count = self.engine.contents.number_of_cols;
        if row_count == 0 || column_count == 0 {
            return;
        }

        let Some(selection) = self.selection else {
            self.move_focused_cell(
                &MoveFocusedCell {
                    direction: action.direction,
                },
                window,
                cx,
            );
            return;
        };
        let Some(focus_row) = self
            .engine
            .d2d_mapping()
            .get_display_row(selection.focus.row)
        else {
            return;
        };

        let (new_row, new_column) =
            self.compute_move((*focus_row, *selection.focus.col), action.direction);
        let Some(new_data_row) = self.engine.d2d_mapping().get_data_row(DisplayRow(new_row)) else {
            return;
        };
        let focus = DataCellId::new(new_data_row, AnyColumn(new_column));
        self.set_selection(Some(CellSelection::new(selection.anchor, focus)), cx);

        self.scroll_to_reveal_row(new_row, action.direction);
        self.scroll_to_reveal_column(new_column, window, cx);
    }

    pub(crate) fn activate_focused_cell(
        &mut self,
        _: &ActivateFocusedCell,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(selection) = self.selection {
            cx.emit(TableViewEvent::CellActivated(selection.focus));
        }
    }

    pub(crate) fn select_cell_with_mouse(
        &mut self,
        cell: DataCellId,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        let selection = match self.selection {
            Some(selection) if extend => CellSelection::new(selection.anchor, cell),
            _ => CellSelection::single_cell(cell),
        };
        self.set_selection(Some(selection), cx);
    }

    /// Display rows and columns covered by the selection, or `None` if its anchor or focus is
    /// hidden by the current filters.
    pub fn selection_display_range(
        &self,
    ) -> Option<(RangeInclusive<usize>, RangeInclusive<usize>)> {
        let selection = self.selection?;
        let mapping = self.engine.d2d_mapping();
        let focus_row = *mapping.get_display_row(selection.focus.row)?;
        let anchor_row = mapping
            .get_display_row(selection.anchor.row)
            .map_or(focus_row, |row| *row);
        let rows = anchor_row.min(focus_row)..=anchor_row.max(focus_row);
        let columns = (*selection.anchor.col).min(*selection.focus.col)
            ..=(*selection.anchor.col).max(*selection.focus.col);
        Some((rows, columns))
    }

    pub(crate) fn is_cell_selected(
        &self,
        display_row: DisplayRow,
        column: AnyColumn,
        selection_range: Option<&(RangeInclusive<usize>, RangeInclusive<usize>)>,
    ) -> bool {
        selection_range.is_some_and(|(rows, columns)| {
            rows.contains(&display_row.0) && columns.contains(&column.0)
        })
    }

    /// The selected cells as tab-separated values, with one line per row. Null cells become empty
    /// fields; values containing tabs, newlines or quotes are quoted so that spreadsheets read
    /// them back as a single field.
    pub fn selection_as_tsv(&self) -> Option<String> {
        let (rows, columns) = self.selection_display_range()?;
        let mut output = String::new();
        for display_row in rows {
            let Some(data_row) = self.data_row(DisplayRow(display_row)) else {
                continue;
            };
            let Some(row) = self.engine.contents.get_row(data_row) else {
                continue;
            };
            if !output.is_empty() {
                output.push('\n');
            }
            for (index, column) in columns.clone().enumerate() {
                if index > 0 {
                    output.push('\t');
                }
                if let Some(value) = row
                    .get(AnyColumn(column))
                    .and_then(TableCell::display_value)
                {
                    output.push_str(&quote_tsv_field(value));
                }
            }
        }
        Some(output)
    }

    pub(crate) fn copy_selection(
        &mut self,
        _: &CopySelection,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(text) = self.selection_as_tsv() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    pub(crate) fn scroll_to_reveal_row(&self, row: usize, direction: NavigationDirection) {
        let row_count = self.engine.d2d_mapping().visible_row_count();
        if row_count == 0 {
            return;
        }

        match direction {
            NavigationDirection::Down => {
                // When moving down, reveal row + 1 (if not already at the end) so that the focused
                // cell isn't obscured by the bottom border or horizontal scrollbar overlay.
                let reveal_row = (row + 1).min(row_count.saturating_sub(1));
                self.list_state.scroll_to_reveal_item(reveal_row);
            }
            NavigationDirection::Up => {
                // When moving up, reveal row - 1 (if not at top) for top peek cushion.
                let reveal_row = row.saturating_sub(1);
                self.list_state.scroll_to_reveal_item(reveal_row);
            }
            NavigationDirection::Left | NavigationDirection::Right => {
                self.list_state.scroll_to_reveal_item(row);
            }
        }
    }

    pub(crate) fn compute_move(
        &self,
        current: (usize, usize),
        direction: NavigationDirection,
    ) -> (usize, usize) {
        let row_count = self.engine.d2d_mapping().visible_row_count();
        let column_count = self.engine.contents.number_of_cols;
        let (row, column) = current;

        match direction {
            NavigationDirection::Up => (row.saturating_sub(1), column),
            NavigationDirection::Down => ((row + 1).min(row_count.saturating_sub(1)), column),
            NavigationDirection::Left => (row, column.saturating_sub(1)),
            NavigationDirection::Right => (row, (column + 1).min(column_count.saturating_sub(1))),
        }
    }

    pub(crate) fn scroll_to_reveal_column(&self, column_index: usize, window: &Window, cx: &App) {
        let handle = self
            .table_interaction_state
            .read(cx)
            .horizontal_scroll_handle
            .clone();

        let widths_state = self.column_widths.read(cx);
        let total_cols = widths_state.cols();
        if total_cols <= 1 || column_index + 2 > total_cols {
            return;
        }

        let rem_size = window.rem_size();
        let peek_padding = px(20.0);

        let left_px = widths_state.pinned_width(column_index + 1, rem_size)
            - widths_state.pinned_width(1, rem_size);
        let right_px = widths_state.pinned_width(column_index + 2, rem_size)
            - widths_state.pinned_width(1, rem_size);

        let scroll_x = -handle.offset().x;
        let viewport_width = handle.bounds().size.width;

        if left_px < scroll_x {
            handle.set_offset(Point::new(-left_px, px(0.)));
        } else if right_px + peek_padding > scroll_x + viewport_width {
            let target = (right_px + peek_padding) - viewport_width;
            handle.set_offset(Point::new(-target, px(0.)));
        }
    }
}

fn digit_count(value: usize) -> usize {
    value
        .checked_ilog10()
        .map_or(1, |digits| digits as usize + 1)
}

fn quote_tsv_field(value: &str) -> std::borrow::Cow<'_, str> {
    if value.contains(['\t', '\n', '\r', '"']) {
        format!("\"{}\"", value.replace('"', "\"\"")).into()
    } else {
        value.into()
    }
}

impl Focusable for TableView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{Action, TestAppContext, VisualTestContext};

    use super::*;
    use crate::types::{DataRow, TableCell, TableLikeContent};
    use ui::table_row::TableRow;

    fn setup_test_view(
        cx: &mut TestAppContext,
        rows: usize,
        cols: usize,
    ) -> (Entity<TableView>, &mut VisualTestContext) {
        cx.update(|cx| {
            workspace::AppState::test(cx);
            editor::init(cx);
        });

        cx.add_window_view(|window, cx| {
            let mut view = TableView::new(window, cx);
            let mut contents = TableLikeContent::default();
            contents.number_of_cols = cols;
            contents.headers = TableRow::from_vec(
                (0..cols)
                    .map(|i| TableCell::Generated(format!("col_{i}").into()))
                    .collect(),
                cols,
            );
            for _ in 0..rows {
                let cells: Vec<TableCell> = (0..cols)
                    .map(|_| TableCell::Generated("val".into()))
                    .collect();
                contents.rows.push(TableRow::from_vec(cells, cols));
            }
            view.set_contents(contents, cx);
            view.engine.set_d2d_mapping(DisplayToDataMapping::compute(
                &view.engine.contents,
                &view.engine.filter_stack,
                view.engine.applied_sorting,
            ));
            view
        })
    }

    fn dispatch<A: Action>(view: &Entity<TableView>, action: &A, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            let focus_handle = view.read(cx).focus_handle(cx);
            focus_handle.focus(window, cx);
            focus_handle.dispatch_action(action, window, cx);
        });
    }

    fn selection_cells(
        view: &Entity<TableView>,
        cx: &VisualTestContext,
    ) -> Option<(DataCellId, DataCellId)> {
        view.read_with(cx, |this, _| {
            let selection = this.selection.as_ref()?;
            Some((selection.anchor, selection.focus))
        })
    }

    #[gpui::test]
    fn test_compute_move_directions(cx: &mut TestAppContext) {
        let (view, cx) = setup_test_view(cx, 10, 5);

        use NavigationDirection as Nav;
        view.read_with(cx, |this, _| {
            assert_eq!(this.compute_move((0, 0), Nav::Down), (1, 0));
            assert_eq!(this.compute_move((0, 0), Nav::Right), (0, 1));
            assert_eq!(this.compute_move((0, 0), Nav::Up), (0, 0));
            assert_eq!(this.compute_move((0, 0), Nav::Left), (0, 0));
            assert_eq!(this.compute_move((9, 4), Nav::Down), (9, 4));
            assert_eq!(this.compute_move((9, 4), Nav::Right), (9, 4));
        });
    }

    #[gpui::test]
    fn test_move_focused_cell_action(cx: &mut TestAppContext) {
        let (view, cx) = setup_test_view(cx, 10, 5);
        let cell = |row, column| DataCellId::new(DataRow(row), AnyColumn(column));

        // 1. First move initializes focus at (0, 0)
        dispatch(
            &view,
            &MoveFocusedCell {
                direction: NavigationDirection::Down,
            },
            cx,
        );
        assert_eq!(selection_cells(&view, cx), Some((cell(0, 0), cell(0, 0))));

        // 2. Step Down and Right -> single cell focus at (1, 1)
        dispatch(
            &view,
            &MoveFocusedCell {
                direction: NavigationDirection::Down,
            },
            cx,
        );
        dispatch(
            &view,
            &MoveFocusedCell {
                direction: NavigationDirection::Right,
            },
            cx,
        );
        assert_eq!(selection_cells(&view, cx), Some((cell(1, 1), cell(1, 1))));
    }

    fn generated_row(values: &[Option<&str>]) -> TableRow<TableCell> {
        let cells = values
            .iter()
            .map(|value| match value {
                Some(value) => TableCell::Generated((*value).to_string().into()),
                None => TableCell::Null,
            })
            .collect::<Vec<_>>();
        let cols = cells.len();
        TableRow::from_vec(cells, cols)
    }

    #[gpui::test]
    fn test_append_rows_extends_mapping_without_reset(cx: &mut TestAppContext) {
        let (view, cx) = setup_test_view(cx, 3, 2);
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            assert_eq!(view.visible_row_count(), 3);
            assert_eq!(view.list_state.item_count(), 3);
            view.append_rows(
                vec![
                    generated_row(&[Some("a"), None]),
                    generated_row(&[Some("b"), Some("c")]),
                ],
                cx,
            )
            .unwrap();
            // No filter or sort is active, so the mapping is extended synchronously.
            assert_eq!(view.visible_row_count(), 5);
            assert_eq!(view.list_state.item_count(), 5);
            assert!(
                view.cell(DataCellId::new(DataRow(3), AnyColumn(1)))
                    .unwrap()
                    .is_null()
            );

            assert!(
                view.append_rows(vec![generated_row(&[Some("only one column")])], cx)
                    .is_err()
            );
            assert_eq!(view.contents().rows.len(), 5);
        });
    }

    #[gpui::test]
    fn test_append_rows_with_sorting_recomputes_mapping(cx: &mut TestAppContext) {
        let (view, cx) = setup_test_view(cx, 0, 1);
        view.update(cx, |view, cx| {
            let mut contents = TableLikeContent::default();
            contents.number_of_cols = 1;
            contents.headers = generated_row(&[Some("n")]);
            contents.column_kinds = vec![crate::types::ColumnKind::Number];
            contents.rows = vec![generated_row(&[Some("10")]), generated_row(&[Some("9")])];
            view.set_contents(contents, cx);
            view.set_sorting(
                Some(AppliedSorting {
                    col_idx: AnyColumn(0),
                    direction: crate::SortDirection::Asc,
                }),
                cx,
            );
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.append_rows(
                vec![generated_row(&[Some("1")]), generated_row(&[None])],
                cx,
            )
            .unwrap();
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let order = view
                .displayed_data_rows()
                .map(|row| row.0)
                .collect::<Vec<_>>();
            assert_eq!(order, vec![2, 1, 0, 3]);
            assert_eq!(view.list_state.item_count(), 4);
        });
    }

    #[gpui::test]
    fn test_extend_selection_and_copy_as_tsv(cx: &mut TestAppContext) {
        let (view, cx) = setup_test_view(cx, 0, 3);
        view.update(cx, |view, cx| {
            let mut contents = TableLikeContent::default();
            contents.number_of_cols = 3;
            contents.headers = generated_row(&[Some("a"), Some("b"), Some("c")]);
            contents.rows = vec![
                generated_row(&[Some("1"), Some("x\ty"), Some("z")]),
                generated_row(&[Some("2"), None, Some("say \"hi\"")]),
            ];
            view.set_contents(contents, cx);
        });
        cx.run_until_parked();

        let events = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|_, cx| {
            let events = events.clone();
            cx.subscribe(&view, move |_, event: &TableViewEvent, _| {
                events.borrow_mut().push(event.clone());
            })
        });

        // The first move places the cursor at the top-left cell, the second moves it right.
        for _ in 0..2 {
            dispatch(
                &view,
                &MoveFocusedCell {
                    direction: NavigationDirection::Right,
                },
                cx,
            );
        }
        dispatch(
            &view,
            &ExtendSelection {
                direction: NavigationDirection::Down,
            },
            cx,
        );
        dispatch(
            &view,
            &ExtendSelection {
                direction: NavigationDirection::Right,
            },
            cx,
        );
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.selection_as_tsv().unwrap(),
                "\"x\ty\"\tz\n\t\"say \"\"hi\"\"\""
            );
        });
        dispatch(&view, &ActivateFocusedCell, cx);
        cx.run_until_parked();

        let events = events.borrow();
        assert_eq!(events.len(), 5);
        assert_eq!(
            events.last(),
            Some(&TableViewEvent::CellActivated(DataCellId::new(
                DataRow(1),
                AnyColumn(2)
            )))
        );
    }

    #[gpui::test]
    fn test_set_contents_clears_out_of_bounds_selection(cx: &mut TestAppContext) {
        let (view, cx) = setup_test_view(cx, 5, 2);
        view.update(cx, |view, cx| {
            view.set_selection(
                Some(CellSelection::single_cell(DataCellId::new(
                    DataRow(4),
                    AnyColumn(1),
                ))),
                cx,
            );
            let mut contents = TableLikeContent::default();
            contents.number_of_cols = 2;
            contents.headers = generated_row(&[Some("col_0"), Some("col_1")]);
            contents.rows = vec![generated_row(&[Some("1"), Some("2")])];
            view.set_contents(contents, cx);
            assert_eq!(view.selection(), None);
        });
    }

    #[gpui::test]
    fn test_set_cell_keeps_mapping(cx: &mut TestAppContext) {
        let (view, cx) = setup_test_view(cx, 3, 2);
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            let cell = DataCellId::new(DataRow(1), AnyColumn(1));
            view.set_cell(cell, TableCell::Null, cx).unwrap();
            view.set_marked_cells([cell], cx);
            assert!(view.cell(cell).unwrap().is_null());
            assert_eq!(view.visible_row_count(), 3);
            assert!(
                view.set_cell(
                    DataCellId::new(DataRow(9), AnyColumn(0)),
                    TableCell::Null,
                    cx
                )
                .is_err()
            );
        });
    }
}
