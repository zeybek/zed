use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context as _, Result, anyhow};
use async_trait::async_trait;
use rusqlite::{Connection, InterruptHandle, OpenFlags, types::ValueRef};

use crate::{
    connection::{DriverKind, ResolvedConnection},
    driver::{
        CancelHandle, ColumnInfo, ColumnMeta, DatabaseSession, ExecOptions, RelationInfo,
        RelationKind, ResultEvent, ResultSender, ResultStream, RowBatcher, SchemaInfo, ValueKind,
        blob_value, display_value, quote_identifier,
    },
    statement::split_statements,
};

pub struct SqliteSession {
    connection: Arc<Mutex<Connection>>,
    cancel: Arc<CancelState>,
}

/// Executions are numbered, so that cancelling one that hasn't started running yet still takes
/// effect: `sqlite3_interrupt` alone only stops a statement that is already running.
struct CancelState {
    interrupt: InterruptHandle,
    /// The number given to the most recent execution.
    assigned: AtomicU64,
    /// The number of the execution that holds the connection.
    running: AtomicU64,
    /// Executions up to this number are cancelled.
    cancelled_up_to: AtomicU64,
}

/// Cancels whatever runs on the session.
struct SqliteCancel(Arc<CancelState>);

#[async_trait]
impl CancelHandle for SqliteCancel {
    async fn cancel(&self) -> Result<()> {
        let state = &self.0;
        state
            .cancelled_up_to
            .fetch_max(state.running.load(Ordering::SeqCst), Ordering::SeqCst);
        state.interrupt.interrupt();
        Ok(())
    }
}

/// Cancels one execution, whether it is running or still waiting for the connection.
struct ExecutionCancel {
    state: Arc<CancelState>,
    execution: u64,
}

#[async_trait]
impl CancelHandle for ExecutionCancel {
    async fn cancel(&self) -> Result<()> {
        self.state
            .cancelled_up_to
            .fetch_max(self.execution, Ordering::SeqCst);
        if self.state.running.load(Ordering::SeqCst) == self.execution {
            self.state.interrupt.interrupt();
        }
        Ok(())
    }
}

/// Opens the database file. The file must exist: a mistyped path never creates a new database.
///
/// Must be called on the Tokio runtime.
pub async fn connect(connection: &ResolvedConnection) -> Result<SqliteSession> {
    let path = connection
        .path
        .clone()
        .context("SQLite connections need a `path`")?;
    let read_only = connection.read_only;
    let connection = tokio::task::spawn_blocking(move || open(&path, read_only)).await??;
    let cancel = Arc::new(CancelState {
        interrupt: connection.get_interrupt_handle(),
        assigned: AtomicU64::new(0),
        running: AtomicU64::new(0),
        cancelled_up_to: AtomicU64::new(0),
    });
    // Checked periodically while a statement runs.
    let state = cancel.clone();
    connection.progress_handler(
        1_000,
        Some(move || {
            state.cancelled_up_to.load(Ordering::SeqCst) >= state.running.load(Ordering::SeqCst)
        }),
    );
    Ok(SqliteSession {
        connection: Arc::new(Mutex::new(connection)),
        cancel,
    })
}

fn open(path: &Path, read_only: bool) -> Result<Connection> {
    anyhow::ensure!(
        path.is_file(),
        "SQLite database {} doesn't exist",
        path.display()
    );
    let access = if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let connection = Connection::open_with_flags(path, access | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("opening {}", path.display()))?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(connection)
}

impl SqliteSession {
    async fn with_connection<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            let connection = connection
                .lock()
                .map_err(|_| anyhow!("the SQLite connection is unusable after a panic"))?;
            f(&connection)
        })
        .await?
    }
}

fn query_strings(connection: &Connection, sql: &str, columns: usize) -> Result<Vec<Vec<String>>> {
    let mut statement = connection.prepare(sql)?;
    let mut rows = statement.query([])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        result.push(
            (0..columns)
                .map(|index| {
                    value_string(row.get_ref(index)?).map(|value| value.unwrap_or_default())
                })
                .collect::<rusqlite::Result<_>>()?,
        );
    }
    Ok(result)
}

#[async_trait]
impl DatabaseSession for SqliteSession {
    fn driver(&self) -> DriverKind {
        DriverKind::Sqlite
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        self.with_connection(|connection| {
            Ok(query_strings(connection, "PRAGMA database_list", 2)?
                .into_iter()
                .filter_map(|row| row.into_iter().nth(1))
                .filter(|name| name != "temp")
                .map(|name| SchemaInfo { name: name.into() })
                .collect())
        })
        .await
    }

    async fn list_relations(&self, schema: &str) -> Result<Vec<RelationInfo>> {
        let sql = format!(
            "SELECT name, type FROM {}.sqlite_master \
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
             ORDER BY name",
            quote_identifier(DriverKind::Sqlite, schema)
        );
        self.with_connection(move |connection| {
            Ok(query_strings(connection, &sql, 2)?
                .into_iter()
                .map(|row| {
                    let mut row = row.into_iter();
                    let name = row.next().unwrap_or_default();
                    let kind = match row.next().as_deref() {
                        Some("view") => RelationKind::View,
                        _ => RelationKind::Table,
                    };
                    RelationInfo {
                        name: name.into(),
                        kind,
                    }
                })
                .collect())
        })
        .await
    }

    async fn list_columns(&self, schema: &str, relation: &str) -> Result<Vec<ColumnInfo>> {
        let sql = format!(
            "SELECT name, type, \"notnull\", pk FROM pragma_table_info({}, {})",
            sql_string(relation),
            sql_string(schema)
        );
        self.with_connection(move |connection| {
            Ok(query_strings(connection, &sql, 4)?
                .into_iter()
                .map(|row| ColumnInfo {
                    name: row[0].clone().into(),
                    data_type: row[1].clone().into(),
                    nullable: row[2] == "0",
                    primary_key: row[3] != "0",
                })
                .collect())
        })
        .await
    }

    async fn relation_ddl(&self, schema: &str, relation: &str) -> Result<String> {
        let sql = format!(
            "SELECT sql FROM {}.sqlite_master WHERE name = {} AND sql IS NOT NULL ORDER BY type = 'index'",
            quote_identifier(DriverKind::Sqlite, schema),
            sql_string(relation)
        );
        let name = relation.to_string();
        self.with_connection(move |connection| {
            let statements = query_strings(connection, &sql, 1)?;
            anyhow::ensure!(!statements.is_empty(), "no definition found for {name}");
            Ok(statements
                .into_iter()
                .filter_map(|row| row.into_iter().next())
                .map(|statement| format!("{statement};"))
                .collect::<Vec<_>>()
                .join("\n"))
        })
        .await
    }

    fn execute(&self, sql: String, options: ExecOptions) -> ResultStream {
        let connection = self.connection.clone();
        let state = self.cancel.clone();
        let execution = state.assigned.fetch_add(1, Ordering::SeqCst) + 1;
        let cancel = Arc::new(ExecutionCancel {
            state: state.clone(),
            execution,
        });
        ResultStream::spawn(Some(cancel), move |mut sender| async move {
            let result = tokio::task::spawn_blocking(move || {
                let connection = match connection.lock() {
                    Ok(connection) => connection,
                    Err(_) => {
                        sender.send_blocking(Err(anyhow!(
                            "the SQLite connection is unusable after a panic"
                        )));
                        return;
                    }
                };
                state.running.store(execution, Ordering::SeqCst);
                if state.cancelled_up_to.load(Ordering::SeqCst) >= execution {
                    return;
                }
                if let Err(error) = run_script(&connection, &sql, options, &mut sender) {
                    sender.send_blocking(Err(error));
                }
            })
            .await;
            if let Err(error) = result {
                log::error!("SQLite statement task failed: {error}");
            }
        })
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        Arc::new(SqliteCancel(self.cancel.clone()))
    }

    fn is_closed(&self) -> bool {
        false
    }
}

fn run_script(
    connection: &Connection,
    sql: &str,
    options: ExecOptions,
    sender: &mut ResultSender,
) -> Result<()> {
    if options.read_only_transaction {
        // `prepare` only compiles the first statement and ignores the rest.
        anyhow::ensure!(
            split_statements(sql).len() <= 1,
            "agent queries must be a single statement"
        );
        let statement = connection
            .prepare(sql)
            .context("agent queries must be a single statement")?;
        anyhow::ensure!(
            statement.readonly(),
            "agent queries must not modify the database"
        );
        drop(statement);
        run_statement(connection, sql, sender)?;
        return Ok(());
    }
    for range in split_statements(sql) {
        if !run_statement(connection, &sql[range], sender)? {
            break;
        }
    }
    Ok(())
}

/// Runs one statement, streaming its rows. Returns `false` once the consumer is gone.
fn run_statement(connection: &Connection, sql: &str, sender: &mut ResultSender) -> Result<bool> {
    let mut statement = connection.prepare(sql)?;
    let column_count = statement.column_count();
    if column_count == 0 {
        let rows_affected = statement.execute([])?;
        return Ok(sender.send_blocking(Ok(ResultEvent::StatementComplete {
            rows_affected: Some(rows_affected as u64),
        })));
    }

    let metas = statement
        .columns()
        .iter()
        .map(|column| {
            let type_name = column.decl_type().unwrap_or_default().to_string();
            ColumnMeta {
                name: column.name().to_string().into(),
                kind: value_kind(&type_name),
                type_name: type_name.into(),
            }
        })
        .collect::<Vec<_>>();
    if !sender.send_blocking(Ok(ResultEvent::Columns(metas))) {
        return Ok(false);
    }

    let mut batcher = RowBatcher::default();
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let values = (0..column_count)
            .map(|index| {
                Ok(match row.get_ref(index)? {
                    ValueRef::Blob(blob) => Some(blob_value(blob.len())),
                    value => value_string(value)?.map(|value| display_value(&value)),
                })
            })
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if let Some(batch) = batcher.push(values)
            && !sender.send_blocking(Ok(batch))
        {
            return Ok(false);
        }
    }
    if let Some(batch) = batcher.take()
        && !sender.send_blocking(Ok(batch))
    {
        return Ok(false);
    }
    Ok(sender.send_blocking(Ok(ResultEvent::StatementComplete {
        rows_affected: None,
    })))
}

fn value_string(value: ValueRef<'_>) -> rusqlite::Result<Option<String>> {
    Ok(match value {
        ValueRef::Null => None,
        ValueRef::Integer(value) => Some(value.to_string()),
        ValueRef::Real(value) => Some(value.to_string()),
        ValueRef::Text(text) => Some(String::from_utf8_lossy(text).into_owned()),
        ValueRef::Blob(blob) => Some(String::from_utf8_lossy(blob).into_owned()),
    })
}

/// Maps a declared column type to a value kind, following SQLite's type affinity rules.
fn value_kind(declared_type: &str) -> ValueKind {
    let declared_type = declared_type.to_ascii_uppercase();
    if declared_type.contains("BOOL") {
        ValueKind::Boolean
    } else if declared_type.contains("DATE") || declared_type.contains("TIME") {
        ValueKind::DateTime
    } else if declared_type.contains("INT")
        || declared_type.contains("REAL")
        || declared_type.contains("FLOA")
        || declared_type.contains("DOUB")
        || declared_type.contains("NUM")
        || declared_type.contains("DEC")
    {
        ValueKind::Number
    } else if declared_type.contains("BLOB") {
        ValueKind::Binary
    } else {
        ValueKind::Text
    }
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;

    async fn collect(stream: ResultStream) -> Vec<ResultEvent> {
        stream.map(|event| event.unwrap()).collect::<Vec<_>>().await
    }

    fn create_database(path: &Path) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL, avatar BLOB);
                 CREATE VIEW names AS SELECT name FROM users;
                 INSERT INTO users (name, score, avatar) VALUES ('ada', 9.5, x'0102'), ('bob', NULL, NULL);",
            )
            .unwrap();
    }

    fn resolved(path: &Path, read_only: bool) -> ResolvedConnection {
        ResolvedConnection {
            driver: DriverKind::Sqlite,
            host: String::new(),
            port: 0,
            database: None,
            username: None,
            password: None,
            tls: Default::default(),
            ssh: None,
            path: Some(path.to_path_buf()),
            read_only,
            statement_timeout: None,
        }
    }

    #[tokio::test]
    async fn test_sqlite_session() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("app.sqlite3");

        let missing = connect(&resolved(&path, false)).await;
        assert!(missing.is_err());
        assert!(!path.exists(), "connecting must not create the file");

        create_database(&path);
        let session = connect(&resolved(&path, false)).await.unwrap();

        assert_eq!(
            session.list_schemas().await.unwrap(),
            vec![SchemaInfo {
                name: "main".into()
            }]
        );
        let relations = session.list_relations("main").await.unwrap();
        assert_eq!(
            relations
                .iter()
                .map(|relation| (relation.name.as_ref(), relation.kind))
                .collect::<Vec<_>>(),
            vec![
                ("names", RelationKind::View),
                ("users", RelationKind::Table)
            ]
        );
        let columns = session.list_columns("main", "users").await.unwrap();
        assert_eq!(columns.len(), 4);
        assert!(columns[0].primary_key);
        assert!(!columns[1].nullable);
        assert!(
            session
                .relation_ddl("main", "users")
                .await
                .unwrap()
                .starts_with("CREATE TABLE users")
        );

        let events = collect(session.execute(
            "SELECT id, name, score, avatar FROM users ORDER BY id; UPDATE users SET score = 1 WHERE id = 2"
                .into(),
            ExecOptions::default(),
        ))
        .await;
        let ResultEvent::Columns(columns) = &events[0] else {
            panic!("expected columns, got {events:?}");
        };
        assert_eq!(
            columns.iter().map(|column| column.kind).collect::<Vec<_>>(),
            vec![
                ValueKind::Number,
                ValueKind::Text,
                ValueKind::Number,
                ValueKind::Binary
            ]
        );
        assert_eq!(
            events[1],
            ResultEvent::Rows(vec![
                vec![
                    Some("1".into()),
                    Some("ada".into()),
                    Some("9.5".into()),
                    Some("<blob 2 bytes>".into())
                ],
                vec![Some("2".into()), Some("bob".into()), None, None],
            ])
        );
        assert_eq!(
            events.last(),
            Some(&ResultEvent::StatementComplete {
                rows_affected: Some(1)
            })
        );

        let agent_write = collect_errors(session.execute(
            "DELETE FROM users".into(),
            ExecOptions {
                read_only_transaction: true,
            },
        ))
        .await;
        assert!(agent_write.contains("must not modify"), "{agent_write}");
        let agent_script = collect_errors(session.execute(
            "SELECT 1; DELETE FROM users".into(),
            ExecOptions {
                read_only_transaction: true,
            },
        ))
        .await;
        assert!(agent_script.contains("single statement"), "{agent_script}");
    }

    #[tokio::test]
    async fn test_read_only_sqlite_session_rejects_writes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("app.sqlite3");
        create_database(&path);
        let session = connect(&resolved(&path, true)).await.unwrap();
        let error =
            collect_errors(session.execute("DELETE FROM users".into(), ExecOptions::default()))
                .await;
        assert!(error.contains("readonly"), "{error}");
    }

    async fn collect_errors(stream: ResultStream) -> String {
        stream
            .filter_map(|event| async move { event.err().map(|error| format!("{error:#}")) })
            .collect::<Vec<_>>()
            .await
            .join("\n")
    }
}
