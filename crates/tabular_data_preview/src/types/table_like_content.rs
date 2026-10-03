use ui::table_row::TableRow;

use crate::types::{AnyColumn, DataRow, LineNumber, TableCell};

/// How the values of a column should be compared when sorting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColumnKind {
    /// Byte-wise lexicographic comparison.
    #[default]
    Text,
    /// Integers and decimals, compared numerically.
    Number,
    /// `true`/`false`, `t`/`f`, `1`/`0`, compared with `false` first.
    Boolean,
    /// Dates and timestamps. Values are expected in ISO-8601 form, which sorts correctly as text.
    DateTime,
}

/// Generic container struct of table-like data (CSV, TSV, etc)
#[derive(Clone)]
pub struct TableLikeContent {
    /// Number of data columns.
    /// Defines table width used to validate `TableRow` on creation
    pub number_of_cols: usize,
    pub headers: TableRow<TableCell>,
    pub rows: Vec<TableRow<TableCell>>,
    /// Follows the same indices as `rows`
    pub line_numbers: Vec<LineNumber>,
    /// Per-column comparison kind. Columns without an entry are treated as [`ColumnKind::Text`].
    pub column_kinds: Vec<ColumnKind>,
}

impl Default for TableLikeContent {
    fn default() -> Self {
        Self {
            number_of_cols: 0,
            headers: TableRow::<TableCell>::from_vec(vec![], 0),
            rows: vec![],
            line_numbers: vec![],
            column_kinds: vec![],
        }
    }
}

impl TableLikeContent {
    pub fn get_row(&self, data_row: DataRow) -> Option<&TableRow<TableCell>> {
        self.rows.get(*data_row)
    }

    pub fn column_kind(&self, column: AnyColumn) -> ColumnKind {
        self.column_kinds.get(*column).copied().unwrap_or_default()
    }
}
