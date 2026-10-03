use std::cmp::Ordering;

use ui::table_row::TableRow;

use crate::types::{AnyColumn, ColumnKind, DataRow, TableCell};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SortDirection {
    Asc,
    Desc,
}

/// Config or currently active sorting
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedSorting {
    /// 0-based column index
    pub col_idx: AnyColumn,
    /// Direction of sorting (asc/desc)
    pub direction: SortDirection,
}

/// A cell value prepared once per sort, so comparisons don't re-parse it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SortKey<'a> {
    Integer(i128),
    Float(f64),
    Boolean(bool),
    Text(&'a str),
}

impl<'a> SortKey<'a> {
    fn new(value: &'a str, kind: ColumnKind) -> Self {
        match kind {
            ColumnKind::Text | ColumnKind::DateTime => SortKey::Text(value),
            ColumnKind::Number => {
                let trimmed = value.trim();
                if let Ok(integer) = trimmed.parse::<i128>() {
                    SortKey::Integer(integer)
                } else if let Ok(float) = trimmed.parse::<f64>() {
                    SortKey::Float(float)
                } else {
                    SortKey::Text(value)
                }
            }
            ColumnKind::Boolean => match value.trim().to_ascii_lowercase().as_str() {
                "true" | "t" | "1" | "yes" | "y" => SortKey::Boolean(true),
                "false" | "f" | "0" | "no" | "n" => SortKey::Boolean(false),
                _ => SortKey::Text(value),
            },
        }
    }

    /// Values of different shapes (e.g. a number column containing `NaN` text) are ordered by
    /// shape so that the comparison stays a total order.
    fn shape_rank(&self) -> u8 {
        match self {
            SortKey::Integer(_) | SortKey::Float(_) => 0,
            SortKey::Boolean(_) => 1,
            SortKey::Text(_) => 2,
        }
    }

    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (SortKey::Integer(a), SortKey::Integer(b)) => a.cmp(b),
            (SortKey::Float(a), SortKey::Float(b)) => a.total_cmp(b),
            (SortKey::Integer(a), SortKey::Float(b)) => (*a as f64).total_cmp(b),
            (SortKey::Float(a), SortKey::Integer(b)) => a.total_cmp(&(*b as f64)),
            (SortKey::Boolean(a), SortKey::Boolean(b)) => a.cmp(b),
            (SortKey::Text(a), SortKey::Text(b)) => a.cmp(b),
            (a, b) => a.shape_rank().cmp(&b.shape_rank()),
        }
    }
}

/// Sorts `data_row_ids` by the values in `sorting.col_idx`, interpreting them according to `kind`.
///
/// Null and missing values always sort last, regardless of direction, so they don't crowd the top
/// of a descending sort. The sort is stable.
pub fn sort_data_rows(
    content_rows: &[TableRow<TableCell>],
    data_row_ids: Vec<DataRow>,
    sorting: AppliedSorting,
    kind: ColumnKind,
) -> Vec<DataRow> {
    let mut keyed: Vec<(Option<SortKey<'_>>, DataRow)> = data_row_ids
        .into_iter()
        .map(|data_row| {
            let key = content_rows
                .get(*data_row)
                .and_then(|row| row.get(sorting.col_idx))
                .and_then(TableCell::display_value)
                .map(|value| SortKey::new(value.as_str(), kind));
            (key, data_row)
        })
        .collect();

    keyed.sort_by(|(a, _), (b, _)| match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => {
            let cmp = a.cmp(b);
            match sorting.direction {
                SortDirection::Asc => cmp,
                SortDirection::Desc => cmp.reverse(),
            }
        }
    });

    keyed.into_iter().map(|(_, data_row)| data_row).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(values: &[Option<&str>]) -> Vec<TableRow<TableCell>> {
        values
            .iter()
            .map(|value| {
                let cell = match value {
                    Some(value) => TableCell::Generated((*value).to_string().into()),
                    None => TableCell::Null,
                };
                TableRow::from_vec(vec![cell], 1)
            })
            .collect()
    }

    fn sorted(values: &[Option<&str>], kind: ColumnKind, direction: SortDirection) -> Vec<usize> {
        let rows = rows(values);
        let ids = (0..rows.len()).map(DataRow).collect();
        sort_data_rows(
            &rows,
            ids,
            AppliedSorting {
                col_idx: AnyColumn(0),
                direction,
            },
            kind,
        )
        .into_iter()
        .map(|row| row.0)
        .collect()
    }

    #[test]
    fn test_numeric_sort_is_not_lexicographic() {
        let values = [Some("10"), Some("9"), Some("-1.5"), Some("100")];
        assert_eq!(
            sorted(&values, ColumnKind::Number, SortDirection::Asc),
            vec![2, 1, 0, 3]
        );
        assert_eq!(
            sorted(&values, ColumnKind::Text, SortDirection::Asc),
            vec![2, 0, 3, 1]
        );
    }

    #[test]
    fn test_nulls_sort_last_in_both_directions() {
        let values = [None, Some("2"), Some("1"), None, Some("3")];
        assert_eq!(
            sorted(&values, ColumnKind::Number, SortDirection::Asc),
            vec![2, 1, 4, 0, 3]
        );
        assert_eq!(
            sorted(&values, ColumnKind::Number, SortDirection::Desc),
            vec![4, 1, 2, 0, 3]
        );
    }

    #[test]
    fn test_boolean_and_mixed_values() {
        let values = [Some("t"), Some("false"), Some("TRUE"), Some("0")];
        assert_eq!(
            sorted(&values, ColumnKind::Boolean, SortDirection::Asc),
            vec![1, 3, 0, 2]
        );

        let mixed = [Some("abc"), Some("2"), Some("1e3"), Some("NaN-ish")];
        assert_eq!(
            sorted(&mixed, ColumnKind::Number, SortDirection::Asc),
            vec![1, 2, 3, 0]
        );
    }
}
