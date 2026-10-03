//! This module defines core operations and config of the tabular data view
//! It operates in 2 coordinate systems:
//! - `DataCellId` - indices of src data cells
//! - `DisplayCellId` - indices of data after applied transformations like sorting/filtering, which is used to render cell on the screen
//!
//! It's designed to contain core logic of operations without relying on `TabularDataPreviewPane`, context or window handles.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use ui::table_row::TableRow;

use crate::{
    table_data_engine::{
        filtering_by_column::{FilterEntry, FilterStack, retain_rows},
        sorting_by_column::{AppliedSorting, sort_data_rows},
    },
    types::{AnyColumn, DataCellId, DataRow, DisplayRow, TableCell, TableLikeContent},
};

pub mod filtering_by_column;
pub mod sorting_by_column;

#[derive(Default)]
pub(crate) struct TableDataEngine {
    pub filter_stack: FilterStack,
    /// Unique values per column, used to populate filter menus. Computed lazily per column the
    /// first time its filter menu is opened, so loading large contents never scans every cell on
    /// the main thread.
    all_filters: HashMap<AnyColumn, Vec<FilterEntry>>,
    pub applied_sorting: Option<AppliedSorting>,
    d2d_mapping: DisplayToDataMapping,
    pub contents: Arc<TableLikeContent>,
}

impl TableDataEngine {
    pub(crate) fn set_contents(&mut self, contents: TableLikeContent) {
        if self
            .contents
            .headers
            .as_slice()
            .iter()
            .map(TableCell::display_value)
            .ne(contents
                .headers
                .as_slice()
                .iter()
                .map(TableCell::display_value))
        {
            // Filters and sorting use column indexes, which may now name different fields.
            self.filter_stack = FilterStack::default();
            self.applied_sorting = None;
        }
        self.contents = Arc::new(contents);
        // The previous mapping can reference rows removed by an edit. Keep it empty
        // until the background filter/sort task builds a mapping for the new contents.
        self.d2d_mapping = DisplayToDataMapping::default();
        self.all_filters.clear();
    }

    /// Clears sorting and filters regardless of whether the headers changed.
    pub(crate) fn reset_view_state(&mut self) {
        self.filter_stack = FilterStack::default();
        self.applied_sorting = None;
    }

    /// Appends rows to the current contents. Returns the range of the new data rows.
    ///
    /// Callers must ensure every row has `number_of_cols` cells.
    pub(crate) fn append_rows(&mut self, rows: Vec<TableRow<TableCell>>) -> std::ops::Range<usize> {
        let contents = Arc::make_mut(&mut self.contents);
        let start = contents.rows.len();
        contents.rows.extend(rows);
        let end = contents.rows.len();
        self.all_filters.clear();
        start..end
    }

    pub(crate) fn set_cell(&mut self, cell: DataCellId, value: TableCell) -> anyhow::Result<()> {
        let contents = Arc::make_mut(&mut self.contents);
        let row = contents
            .rows
            .get_mut(*cell.row)
            .ok_or_else(|| anyhow::anyhow!("row {:?} doesn't exist", cell.row))?;
        let target = row
            .as_mut_slice()
            .get_mut(*cell.col)
            .ok_or_else(|| anyhow::anyhow!("column {:?} doesn't exist", cell.col))?;
        *target = value;
        // Filter menus list the values of a column.
        self.all_filters.remove(&cell.col);
        Ok(())
    }

    pub(crate) fn has_any_filter_or_sort(&self) -> bool {
        self.applied_sorting.is_some() || !self.filter_stack.is_empty()
    }

    pub(crate) fn d2d_mapping(&self) -> &DisplayToDataMapping {
        &self.d2d_mapping
    }

    pub(crate) fn set_d2d_mapping(&mut self, mapping: DisplayToDataMapping) {
        self.d2d_mapping = mapping;
    }

    /// Extends the identity mapping with newly appended rows. Only valid when no sorting or
    /// filtering is applied and the current mapping is up to date.
    pub(crate) fn extend_identity_mapping(&mut self, new_rows: std::ops::Range<usize>) {
        self.d2d_mapping.extend_identity(new_rows);
    }

    pub(crate) fn cached_filters_for_column(&mut self, column: AnyColumn) -> &Vec<FilterEntry> {
        let contents = &self.contents;
        self.all_filters.entry(column).or_insert_with(|| {
            filtering_by_column::calculate_filter_entries(&contents.rows, column)
        })
    }
}

/// Relation of Display (rendered) rows to Data (src) rows with applied transformations
/// Transformations applied:
/// - sorting by column
/// - filtering by column values
#[derive(Debug, Default)]
pub struct DisplayToDataMapping {
    /// All rows sorted, regardless of applied filtering. Recomputed every time sorting changes
    pub sorted_rows: Vec<DataRow>,
    /// Rows that survive the active filters. Recomputed every time filters change
    pub retained_rows: HashSet<DataRow>,
    /// Merged result: sorted rows intersected with retained rows, indexed by display row.
    pub mapping: Arc<Vec<DataRow>>,
}

impl DisplayToDataMapping {
    /// Computes the full display-to-data mapping from owned inputs.
    /// Intended to be called from a background thread.
    pub(crate) fn compute(
        contents: &Arc<TableLikeContent>,
        filter_stack: &FilterStack,
        sorting: Option<AppliedSorting>,
    ) -> Self {
        let mut mapping = Self::default();
        mapping.apply_sorting(sorting, contents);
        mapping.apply_filtering(filter_stack, &contents.rows);
        mapping.merge_mappings();
        mapping
    }

    /// Get the data row for a given display row
    pub fn get_data_row(&self, display_row: DisplayRow) -> Option<DataRow> {
        self.mapping.get(*display_row).copied()
    }

    /// Get the display row for a given data row
    pub fn get_display_row(&self, data_row: DataRow) -> Option<DisplayRow> {
        // Without sorting and filtering, the mapping is the identity.
        if self.mapping.get(*data_row) == Some(&data_row) {
            return Some(DisplayRow(*data_row));
        }
        self.mapping
            .iter()
            .position(|&mapped_data_row| mapped_data_row == data_row)
            .map(DisplayRow)
    }

    /// Get the number of filtered rows
    pub fn visible_row_count(&self) -> usize {
        self.mapping.len()
    }

    fn extend_identity(&mut self, new_rows: std::ops::Range<usize>) {
        let mapping = Arc::make_mut(&mut self.mapping);
        for row in new_rows {
            self.sorted_rows.push(DataRow(row));
            self.retained_rows.insert(DataRow(row));
            mapping.push(DataRow(row));
        }
    }

    /// Computes sorting
    fn apply_sorting(&mut self, sorting: Option<AppliedSorting>, contents: &TableLikeContent) {
        let data_rows: Vec<DataRow> = (0..contents.rows.len()).map(DataRow).collect();

        let sorted_rows = if let Some(sorting) = sorting {
            sort_data_rows(
                &contents.rows,
                data_rows,
                sorting,
                contents.column_kind(sorting.col_idx),
            )
        } else {
            data_rows
        };

        self.sorted_rows = sorted_rows;
    }

    fn apply_filtering(&mut self, filter_stack: &FilterStack, rows: &[TableRow<TableCell>]) {
        self.retained_rows = retain_rows(rows, filter_stack, None);
    }

    /// Merges pre-computed sorting and filtering into the final display mapping
    fn merge_mappings(&mut self) {
        self.mapping = Arc::new(
            self.sorted_rows
                .iter()
                .filter(|data_row| self.retained_rows.contains(data_row))
                .copied()
                .collect(),
        );
    }
}
