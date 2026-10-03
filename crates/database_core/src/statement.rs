//! Splitting SQL text into statements and classifying them, without a full parser.

use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenKind {
    Word,
    Semicolon,
    /// A line containing only whitespace.
    BlankLine,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Token {
    kind: TokenKind,
    range: Range<usize>,
}

/// Scans SQL into words and separators, skipping over strings, quoted identifiers, comments and
/// dollar-quoted bodies so that semicolons inside them don't end a statement.
fn tokenize(sql: &str) -> Vec<Token> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    let mut line_has_content = false;
    let mut line_start = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b'\n' => {
                if !line_has_content && index > line_start {
                    tokens.push(Token {
                        kind: TokenKind::BlankLine,
                        range: line_start..index,
                    });
                } else if !line_has_content
                    && index == line_start
                    && index > 0
                    && tokens
                        .last()
                        .is_none_or(|token| token.kind != TokenKind::BlankLine)
                {
                    tokens.push(Token {
                        kind: TokenKind::BlankLine,
                        range: index..index,
                    });
                }
                index += 1;
                line_start = index;
                line_has_content = false;
                continue;
            }
            b' ' | b'\t' | b'\r' => {
                index += 1;
                continue;
            }
            _ => {}
        }
        line_has_content = true;
        let start = index;
        match byte {
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index = find_byte(bytes, index, b'\n');
                // The comment is part of the line; the newline is handled by the loop.
                continue;
            }
            b'#' => {
                // MySQL line comment. Harmless elsewhere, where `#` is rarely valid.
                index = find_byte(bytes, index, b'\n');
                continue;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = find_sequence(bytes, index + 2, b"*/");
                line_has_content = true;
                continue;
            }
            b'\'' | b'"' | b'`' => {
                index = skip_quoted(bytes, index, byte);
                tokens.push(Token {
                    kind: TokenKind::Other,
                    range: start..index,
                });
            }
            b'$' => {
                if let Some(end) = skip_dollar_quoted(sql, index) {
                    index = end;
                } else {
                    index += 1;
                }
                tokens.push(Token {
                    kind: TokenKind::Other,
                    range: start..index,
                });
            }
            b';' => {
                index += 1;
                tokens.push(Token {
                    kind: TokenKind::Semicolon,
                    range: start..index,
                });
            }
            byte if byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80 => {
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric()
                        || bytes[index] == b'_'
                        || bytes[index] >= 0x80)
                {
                    index += 1;
                }
                tokens.push(Token {
                    kind: TokenKind::Word,
                    range: start..index,
                });
            }
            _ => {
                index += 1;
                tokens.push(Token {
                    kind: TokenKind::Other,
                    range: start..index,
                });
            }
        }
    }
    tokens
}

fn find_byte(bytes: &[u8], from: usize, target: u8) -> usize {
    bytes[from..]
        .iter()
        .position(|byte| *byte == target)
        .map_or(bytes.len(), |position| from + position)
}

fn find_sequence(bytes: &[u8], from: usize, target: &[u8]) -> usize {
    bytes[from.min(bytes.len())..]
        .windows(target.len())
        .position(|window| window == target)
        .map_or(bytes.len(), |position| from + position + target.len())
}

/// Skips a quoted string starting at `start`, where a doubled quote or a backslash escapes it.
fn skip_quoted(bytes: &[u8], start: usize, quote: u8) -> usize {
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if quote == b'\'' => index += 2,
            byte if byte == quote => {
                if bytes.get(index + 1) == Some(&quote) {
                    index += 2;
                } else {
                    return index + 1;
                }
            }
            _ => index += 1,
        }
    }
    bytes.len()
}

/// Skips a PostgreSQL dollar-quoted body like `$$ ... $$` or `$tag$ ... $tag$`.
fn skip_dollar_quoted(sql: &str, start: usize) -> Option<usize> {
    let rest = &sql[start + 1..];
    let tag_len = rest.find('$')?;
    let tag = &rest[..tag_len];
    if !tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        || tag.starts_with(|c: char| c.is_ascii_digit())
    {
        return None;
    }
    let delimiter = &sql[start..start + tag_len + 2];
    let body_start = start + tag_len + 2;
    Some(
        sql[body_start..]
            .find(delimiter)
            .map_or(sql.len(), |end| body_start + end + delimiter.len()),
    )
}

/// Splits SQL text into statements. Statements end at a semicolon or, for scripts written
/// without semicolons, at a blank line. Returned ranges are trimmed and exclude the semicolon.
pub fn split_statements(sql: &str) -> Vec<Range<usize>> {
    let tokens = tokenize(sql);
    let mut statements = Vec::new();
    let mut current: Option<Range<usize>> = None;
    let mut block_depth = 0usize;
    let mut words_in_statement: Vec<String> = Vec::new();

    let finish = |current: &mut Option<Range<usize>>, statements: &mut Vec<Range<usize>>| {
        if let Some(range) = current.take() {
            statements.push(range);
        }
    };

    for token in tokens {
        match token.kind {
            TokenKind::Semicolon if block_depth == 0 => {
                finish(&mut current, &mut statements);
                words_in_statement.clear();
            }
            TokenKind::BlankLine if block_depth == 0 => {
                finish(&mut current, &mut statements);
                words_in_statement.clear();
            }
            TokenKind::BlankLine => {}
            TokenKind::Word => {
                let word = sql[token.range.clone()].to_ascii_lowercase();
                // Trigger bodies (and the CASE expressions inside them) contain semicolons that
                // don't end the statement.
                let in_trigger = words_in_statement.first().is_some_and(|w| w == "create")
                    && words_in_statement.iter().any(|w| w == "trigger");
                if in_trigger {
                    match word.as_str() {
                        "begin" | "case" => block_depth += 1,
                        "end" => block_depth = block_depth.saturating_sub(1),
                        _ => {}
                    }
                }
                if words_in_statement.len() < 8 {
                    words_in_statement.push(word);
                }
                extend(&mut current, token.range);
            }
            TokenKind::Semicolon | TokenKind::Other => extend(&mut current, token.range),
        }
    }
    finish(&mut current, &mut statements);
    statements
}

fn extend(current: &mut Option<Range<usize>>, range: Range<usize>) {
    match current {
        Some(current) => current.end = range.end,
        None => *current = Some(range),
    }
}

/// The statement at `offset`. When the offset is between statements, the statement before it
/// on the same line wins, then the next one.
pub fn statement_at(sql: &str, offset: usize) -> Option<Range<usize>> {
    let statements = split_statements(sql);
    if let Some(statement) = statements
        .iter()
        .find(|statement| statement.start <= offset && offset <= statement.end)
    {
        return Some(statement.clone());
    }
    let line_start = sql[..offset.min(sql.len())]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    statements
        .iter()
        .rev()
        .find(|statement| statement.end <= offset && statement.end >= line_start)
        .or_else(|| {
            statements
                .iter()
                .find(|statement| statement.start >= offset)
        })
        .cloned()
}

/// What a statement does, for confirmation prompts and read-only checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatementKind {
    /// `SELECT`, `SHOW`, `EXPLAIN` without `ANALYZE`, and similar.
    Read,
    /// `INSERT`, `UPDATE`, `DELETE`, `MERGE` and similar.
    Write,
    /// Schema changes such as `CREATE`, `ALTER`, `DROP` and `TRUNCATE`.
    Ddl,
    /// `BEGIN`, `COMMIT`, `SET` and other session statements.
    Other,
    /// Statements whose effect depends on what they run, such as `EXECUTE`, and statements that
    /// aren't recognized.
    Unknown,
}

impl StatementKind {
    /// Whether the statement modifies data or the schema, or might.
    pub fn may_modify_data(self) -> bool {
        matches!(
            self,
            StatementKind::Write | StatementKind::Ddl | StatementKind::Unknown
        )
    }
}

fn words(sql: &str) -> impl Iterator<Item = String> + '_ {
    tokenize(sql)
        .into_iter()
        .filter(|token| token.kind == TokenKind::Word)
        .map(move |token| sql[token.range].to_ascii_lowercase())
}

pub fn classify(statement: &str) -> StatementKind {
    let words = words(statement).collect::<Vec<_>>();
    let Some(first) = words.first() else {
        return StatementKind::Other;
    };
    let contains_write_keyword = || {
        words
            .iter()
            .any(|word| matches!(word.as_str(), "insert" | "update" | "delete" | "merge"))
    };
    match first.as_str() {
        "select" | "table" | "values" => {
            // `SELECT ... INTO new_table` creates a table in PostgreSQL.
            if words.iter().any(|word| word == "into") {
                StatementKind::Write
            } else {
                StatementKind::Read
            }
        }
        "with" => {
            if contains_write_keyword() {
                StatementKind::Write
            } else {
                StatementKind::Read
            }
        }
        "show" | "describe" | "desc" | "pragma" => StatementKind::Read,
        "explain" => {
            if words
                .get(1)
                .is_some_and(|word| word == "analyze" || word == "analyse")
                && contains_write_keyword()
            {
                StatementKind::Write
            } else {
                StatementKind::Read
            }
        }
        "insert" | "update" | "delete" | "merge" | "replace" | "upsert" | "copy" | "call"
        | "do" | "load" => StatementKind::Write,
        "create" | "alter" | "drop" | "truncate" | "rename" | "comment" | "grant" | "revoke"
        | "reindex" | "vacuum" | "cluster" | "refresh" | "attach" | "detach" => StatementKind::Ddl,
        "begin" | "start" | "commit" | "end" | "rollback" | "abort" | "savepoint" | "release"
        | "set" | "reset" | "use" | "prepare" | "deallocate" | "declare" | "fetch" | "move"
        | "close" | "lock" | "unlock" | "listen" | "unlisten" | "analyze" | "analyse"
        | "checkpoint" | "discard" => StatementKind::Other,
        _ => StatementKind::Unknown,
    }
}

/// Whether the statement is an `UPDATE` or `DELETE` that affects every row of its table.
pub fn is_unfiltered_write(statement: &str) -> bool {
    let words = words(statement).collect::<Vec<_>>();
    let Some(first) = words.first() else {
        return false;
    };
    let target = if first == "with" {
        words
            .iter()
            .position(|word| word == "update" || word == "delete")
    } else if first == "update" || first == "delete" {
        Some(0)
    } else {
        None
    };
    target.is_some_and(|target| !words[target..].iter().any(|word| word == "where"))
}

/// The first few words of a statement, for tab titles. Comments are dropped and whitespace is
/// collapsed.
pub fn summary(statement: &str, max_chars: usize) -> String {
    let mut summary = String::new();
    let mut previous_end = None;
    for token in tokenize(statement) {
        if token.kind == TokenKind::BlankLine {
            continue;
        }
        let text = &statement[token.range.clone()];
        // Keep tokens that touch in the source together, like `count(*)`, and collapse any
        // whitespace or comments between the others into a single space.
        let needs_space = previous_end.is_some_and(|end| end != token.range.start);
        previous_end = Some(token.range.end);
        let added_chars = text.chars().count() + usize::from(needs_space);
        if summary.chars().count() + added_chars > max_chars {
            if summary.is_empty() {
                summary = text.chars().take(max_chars).collect();
            }
            summary.push('…');
            return summary;
        }
        if needs_space {
            summary.push(' ');
        }
        summary.push_str(text);
    }
    summary
}

/// A value as an SQL literal: a quoted string, or `NULL`. Strings are compared and assigned
/// through the database's implicit casts, which accept the text form of every value shown in
/// results.
pub fn literal(driver: crate::DriverKind, value: Option<&str>) -> String {
    let Some(value) = value else {
        return "NULL".to_string();
    };
    let mut escaped = value.replace('\'', "''");
    if driver == crate::DriverKind::Mysql {
        // MySQL treats backslashes in strings as escapes by default.
        escaped = escaped.replace('\\', "\\\\");
    }
    format!("'{escaped}'")
}

/// An `UPDATE` of one row, identified by its primary key values.
pub fn update_statement(
    driver: crate::DriverKind,
    schema: &str,
    relation: &str,
    assignments: &[(&str, Option<&str>)],
    key: &[(&str, Option<&str>)],
) -> String {
    let set = assignments
        .iter()
        .map(|(column, value)| {
            format!(
                "{} = {}",
                crate::driver::quote_identifier(driver, column),
                literal(driver, *value)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let condition = key
        .iter()
        .map(|(column, value)| {
            let column = crate::driver::quote_identifier(driver, column);
            match value {
                Some(value) => format!("{column} = {}", literal(driver, Some(value))),
                None => format!("{column} IS NULL"),
            }
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    format!(
        "UPDATE {} SET {set} WHERE {condition}",
        crate::driver::qualified_name(driver, schema, relation)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statements(sql: &str) -> Vec<&str> {
        split_statements(sql)
            .into_iter()
            .map(|range| &sql[range])
            .collect()
    }

    #[test]
    fn test_split_on_semicolons_and_blank_lines() {
        assert_eq!(
            statements("select 1; select 2;\nselect 3\n\nselect\n  4"),
            vec!["select 1", "select 2", "select 3", "select\n  4"]
        );
        assert_eq!(statements("  \n\n  "), Vec::<&str>::new());
        assert_eq!(
            statements("select 1\n   \nselect 2"),
            vec!["select 1", "select 2"]
        );
    }

    #[test]
    fn test_split_ignores_quoted_and_commented_semicolons() {
        let sql =
            "select ';', \"a;b\", `c;d` -- x;y\nfrom t; /* ; */ select $$ ; $$, $tag$ ;\n\n $tag$";
        assert_eq!(
            statements(sql),
            vec![
                "select ';', \"a;b\", `c;d` -- x;y\nfrom t",
                "select $$ ; $$, $tag$ ;\n\n $tag$"
            ]
        );
        assert_eq!(
            statements("select 'it''s; fine'; select 2"),
            vec!["select 'it''s; fine'", "select 2"]
        );
        assert_eq!(
            statements("select $1, $2; select 3"),
            vec!["select $1, $2", "select 3"]
        );
    }

    #[test]
    fn test_split_keeps_trigger_bodies_together() {
        let sql = "CREATE TRIGGER t AFTER INSERT ON a BEGIN\n  UPDATE b SET x = CASE WHEN 1 THEN 2 END;\n  DELETE FROM c;\nEND; SELECT 1;";
        assert_eq!(
            statements(sql),
            vec![
                "CREATE TRIGGER t AFTER INSERT ON a BEGIN\n  UPDATE b SET x = CASE WHEN 1 THEN 2 END;\n  DELETE FROM c;\nEND",
                "SELECT 1"
            ]
        );
    }

    #[test]
    fn test_statement_at_cursor() {
        let sql = "select 1;\nselect 2; select 3;\n\nselect 4";
        let at = |offset| statement_at(sql, offset).map(|range| &sql[range]);
        assert_eq!(at(0), Some("select 1"));
        assert_eq!(at(9), Some("select 1"), "right after the semicolon");
        assert_eq!(at(12), Some("select 2"));
        assert_eq!(at(sql.find("3").unwrap()), Some("select 3"));
        assert_eq!(at(sql.find("\n\n").unwrap() + 1), Some("select 4"));
        assert_eq!(at(sql.len()), Some("select 4"));
        assert_eq!(statement_at("", 0), None);
    }

    #[test]
    fn test_classify() {
        assert_eq!(classify("SELECT * FROM t"), StatementKind::Read);
        assert_eq!(classify("-- c\n  select 1"), StatementKind::Read);
        assert_eq!(
            classify("WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x"),
            StatementKind::Write
        );
        assert_eq!(classify("select * into t2 from t"), StatementKind::Write);
        assert_eq!(classify("update t set a = 1"), StatementKind::Write);
        assert_eq!(classify("DROP TABLE t"), StatementKind::Ddl);
        assert_eq!(
            classify("explain analyze delete from t"),
            StatementKind::Write
        );
        assert_eq!(classify("explain select 1"), StatementKind::Read);
        assert_eq!(classify("begin"), StatementKind::Other);
        assert_eq!(classify("SET search_path = app"), StatementKind::Other);
        assert_eq!(classify("EXECUTE purge_users"), StatementKind::Unknown);
        assert!(StatementKind::Unknown.may_modify_data());
        assert!(!StatementKind::Other.may_modify_data());
        assert_eq!(classify("ATTACH 'other.db' AS other"), StatementKind::Ddl);
        assert_eq!(classify("select 'delete'"), StatementKind::Read);
    }

    #[test]
    fn test_unfiltered_writes() {
        assert!(is_unfiltered_write("DELETE FROM users"));
        assert!(is_unfiltered_write("update users set active = false"));
        assert!(!is_unfiltered_write("update users set a = 1 where id = 2"));
        assert!(!is_unfiltered_write("select * from users"));
        assert!(!is_unfiltered_write("delete from t where x = 'where'"));
        assert!(is_unfiltered_write("delete from t -- where id = 1"));
    }

    #[test]
    fn test_update_statement() {
        use crate::DriverKind;
        assert_eq!(
            update_statement(
                DriverKind::Postgres,
                "public",
                "users",
                &[("name", Some("O'Brien")), ("note", None)],
                &[("id", Some("7"))]
            ),
            "UPDATE public.users SET name = 'O''Brien', note = NULL WHERE id = '7'"
        );
        assert_eq!(
            update_statement(
                DriverKind::Mysql,
                "app",
                "order",
                &[("path", Some("C:\\tmp"))],
                &[("a", Some("1")), ("b", None)]
            ),
            "UPDATE app.`order` SET path = 'C:\\\\tmp' WHERE a = '1' AND b IS NULL"
        );
        assert_eq!(
            update_statement(
                DriverKind::Sqlite,
                "main",
                "t",
                &[("x", Some("1"))],
                &[("id", Some("2"))]
            ),
            "UPDATE t SET x = '1' WHERE id = '2'"
        );
    }

    #[test]
    fn test_summary() {
        assert_eq!(
            summary("select  id,\n name -- comment\nfrom users where id = 1", 22),
            "select id, name from…"
        );
        assert_eq!(
            summary("select count(*) from t", 100),
            "select count(*) from t"
        );
    }
}
