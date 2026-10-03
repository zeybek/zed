//! Serializing result sets for export.

use crate::driver::ValueKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Json,
    Markdown,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Json => "json",
            ExportFormat::Markdown => "md",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            ExportFormat::Csv => "CSV",
            ExportFormat::Json => "JSON",
            ExportFormat::Markdown => "Markdown",
        }
    }
}

pub struct ExportColumn<'a> {
    pub name: &'a str,
    pub kind: ValueKind,
}

/// Serializes rows in the given format. `None` values are SQL `NULL`.
pub fn export<'a>(
    format: ExportFormat,
    columns: &[ExportColumn<'_>],
    rows: impl IntoIterator<Item = Vec<Option<&'a str>>>,
) -> String {
    match format {
        ExportFormat::Csv => to_csv(columns, rows),
        ExportFormat::Json => to_json(columns, rows),
        ExportFormat::Markdown => to_markdown(columns, rows),
    }
}

fn to_csv<'a>(
    columns: &[ExportColumn<'_>],
    rows: impl IntoIterator<Item = Vec<Option<&'a str>>>,
) -> String {
    let mut output = String::new();
    let mut write_line = |values: &mut dyn Iterator<Item = &str>| {
        for (index, value) in values.enumerate() {
            if index > 0 {
                output.push(',');
            }
            if value.contains([',', '"', '\n', '\r']) || value.starts_with(' ') {
                output.push('"');
                output.push_str(&value.replace('"', "\"\""));
                output.push('"');
            } else {
                output.push_str(value);
            }
        }
        output.push_str("\r\n");
    };
    write_line(&mut columns.iter().map(|column| column.name));
    for row in rows {
        write_line(&mut row.iter().map(|value| value.unwrap_or("")));
    }
    output
}

fn to_json<'a>(
    columns: &[ExportColumn<'_>],
    rows: impl IntoIterator<Item = Vec<Option<&'a str>>>,
) -> String {
    let rows = rows
        .into_iter()
        .map(|row| {
            let object = columns
                .iter()
                .zip(row)
                .map(|(column, value)| (column.name.to_string(), json_value(column.kind, value)))
                .collect::<serde_json::Map<_, _>>();
            serde_json::Value::Object(object)
        })
        .collect::<Vec<_>>();
    serde_json::to_string_pretty(&rows).unwrap_or_default()
}

fn json_value(kind: ValueKind, value: Option<&str>) -> serde_json::Value {
    let Some(value) = value else {
        return serde_json::Value::Null;
    };
    match kind {
        ValueKind::Number => {
            // Values that a JSON number can't represent exactly, such as high-precision decimals,
            // stay strings.
            if let Ok(number) = serde_json::from_str::<serde_json::Number>(value.trim())
                && number.to_string() == value.trim()
            {
                return serde_json::Value::Number(number);
            }
        }
        ValueKind::Boolean => match value.trim().to_ascii_lowercase().as_str() {
            "t" | "true" | "1" => return serde_json::Value::Bool(true),
            "f" | "false" | "0" => return serde_json::Value::Bool(false),
            _ => {}
        },
        ValueKind::Text | ValueKind::DateTime | ValueKind::Binary => {}
    }
    serde_json::Value::String(value.to_string())
}

fn to_markdown<'a>(
    columns: &[ExportColumn<'_>],
    rows: impl IntoIterator<Item = Vec<Option<&'a str>>>,
) -> String {
    let escape = |value: &str| {
        value
            .replace('\\', "\\\\")
            .replace('|', "\\|")
            .replace("\r\n", "<br>")
            .replace('\n', "<br>")
    };
    let mut output = String::new();
    output.push('|');
    for column in columns {
        output.push_str(&format!(" {} |", escape(column.name)));
    }
    output.push_str("\n|");
    for column in columns {
        output.push_str(if column.kind == ValueKind::Number {
            " ---: |"
        } else {
            " --- |"
        });
    }
    output.push('\n');
    for row in rows {
        output.push('|');
        for value in row {
            match value {
                Some(value) => output.push_str(&format!(" {} |", escape(value))),
                None => output.push_str(" *NULL* |"),
            }
        }
        output.push('\n');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns() -> Vec<ExportColumn<'static>> {
        vec![
            ExportColumn {
                name: "id",
                kind: ValueKind::Number,
            },
            ExportColumn {
                name: "name",
                kind: ValueKind::Text,
            },
            ExportColumn {
                name: "active",
                kind: ValueKind::Boolean,
            },
        ]
    }

    fn rows() -> Vec<Vec<Option<&'static str>>> {
        vec![
            vec![Some("1"), Some("Ada, \"the first\""), Some("t")],
            vec![Some("12345678901234567890"), None, Some("f")],
        ]
    }

    #[test]
    fn test_csv() {
        assert_eq!(
            export(ExportFormat::Csv, &columns(), rows()),
            "id,name,active\r\n1,\"Ada, \"\"the first\"\"\",t\r\n12345678901234567890,,f\r\n"
        );
    }

    #[test]
    fn test_json() {
        let json: serde_json::Value =
            serde_json::from_str(&export(ExportFormat::Json, &columns(), rows())).unwrap();
        assert_eq!(
            json,
            serde_json::json!([
                { "id": 1, "name": "Ada, \"the first\"", "active": true },
                { "id": 12345678901234567890u64, "name": null, "active": false },
            ])
        );
    }

    #[test]
    fn test_markdown() {
        assert_eq!(
            export(
                ExportFormat::Markdown,
                &columns()[..2],
                vec![vec![Some("1"), Some("a|b\nc")], vec![Some("2"), None]]
            ),
            "| id | name |\n| ---: | --- |\n| 1 | a\\|b<br>c |\n| 2 | *NULL* |\n"
        );
    }
}
