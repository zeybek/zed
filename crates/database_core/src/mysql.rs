use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use async_trait::async_trait;
use mysql_async::{
    ClientIdentity, Conn, DriverError, Opts, OptsBuilder, Row, SslOpts, Value,
    consts::ColumnType,
    prelude::{Protocol, Queryable as _},
};
use settings::DatabaseSslMode;
use tokio::sync::Mutex;

use crate::{
    connection::{DriverKind, ResolvedConnection},
    driver::{
        CancelHandle, ColumnInfo, ColumnMeta, DatabaseSession, ExecOptions, ForeignKeyInfo,
        IndexInfo, KeyInfo, ObjectRef, RelationDetails, RelationInfo, RelationKind, ResultEvent,
        ResultSender, ResultStream, RoutineInfo, RoutineKind, RowBatcher, SchemaInfo,
        SchemaObjects, TriggerInfo, ValueKind, blob_value, display_value, qualified_name,
        quote_identifier,
    },
    statement::split_statements,
};

/// MySQL's character set number for binary strings.
const BINARY_CHARSET: u16 = 63;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

pub struct MysqlSession {
    conn: Arc<Mutex<Option<Conn>>>,
    cancel: Arc<MysqlCancel>,
    read_only: bool,
}

impl Drop for MysqlSession {
    fn drop(&mut self) {
        let conn = self.conn.clone();
        // Dropping a connection without saying goodbye leaves it open on the server until it
        // times out.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Some(conn) = conn.lock().await.take() {
                    conn.disconnect().await.ok();
                }
            });
        }
    }
}

struct MysqlCancel {
    opts: Opts,
    connection_id: u32,
}

#[async_trait]
impl CancelHandle for MysqlCancel {
    async fn cancel(&self) -> Result<()> {
        // The session is busy with the statement, so the kill has to come from another one.
        let mut control = Conn::new(self.opts.clone())
            .await
            .context("opening a control connection")?;
        let result = control
            .query_drop(format!("KILL QUERY {}", self.connection_id))
            .await;
        control.disconnect().await.ok();
        result.context("killing the query")
    }
}

/// Connects to `host:port`, which may be the local end of an SSH tunnel.
///
/// Must be called on the Tokio runtime.
pub async fn connect(
    connection: &ResolvedConnection,
    host: &str,
    port: u16,
) -> Result<MysqlSession> {
    let base = OptsBuilder::default()
        .ip_or_hostname(host)
        .tcp_port(port)
        .user(connection.username.clone())
        .pass(connection.password.clone())
        .db_name(connection.database.clone())
        .prefer_socket(false)
        .stmt_cache_size(0);
    let ssl_opts = ssl_opts(connection)?;

    let (conn, opts) = match ssl_opts {
        None => {
            let opts = Opts::from(base);
            (connect_with_timeout(opts.clone()).await?, opts)
        }
        Some(ssl_opts) => {
            // Install the default crypto provider that mysql_async's rustls config relies on.
            http_client_tls::tls_config();
            let opts = Opts::from(base.clone().ssl_opts(ssl_opts));
            match connect_with_timeout(opts.clone()).await {
                Ok(conn) => (conn, opts),
                Err(error)
                    if connection.tls.mode == DatabaseSslMode::Prefer
                        && is_tls_unsupported(&error) =>
                {
                    let opts = Opts::from(base);
                    (connect_with_timeout(opts.clone()).await?, opts)
                }
                Err(error) => return Err(error),
            }
        }
    };

    let mut conn = conn;
    if connection.read_only {
        conn.query_drop("SET SESSION TRANSACTION READ ONLY")
            .await
            .context("making the session read-only")?;
    }
    if let Some(timeout) = connection.statement_timeout {
        let mysql = conn
            .query_drop(format!(
                "SET SESSION MAX_EXECUTION_TIME = {}",
                timeout.as_millis()
            ))
            .await;
        if mysql.is_err() {
            // MariaDB names the setting differently and measures it in seconds.
            conn.query_drop(format!(
                "SET SESSION max_statement_time = {}",
                timeout.as_secs_f64()
            ))
            .await
            .context("setting the statement timeout")?;
        }
    }

    let cancel = Arc::new(MysqlCancel {
        opts,
        connection_id: conn.id(),
    });
    Ok(MysqlSession {
        conn: Arc::new(Mutex::new(Some(conn))),
        cancel,
        read_only: connection.read_only,
    })
}

async fn connect_with_timeout(opts: Opts) -> Result<Conn> {
    tokio::time::timeout(CONNECT_TIMEOUT, Conn::new(opts))
        .await
        .map_err(|_| anyhow!("timed out connecting to MySQL"))?
        .map_err(Into::into)
}

fn ssl_opts(connection: &ResolvedConnection) -> Result<Option<SslOpts>> {
    let tls = &connection.tls;
    if tls.mode == DatabaseSslMode::Disable {
        return Ok(None);
    }
    let mut ssl_opts = SslOpts::default();
    if let Some(root_cert) = &tls.root_cert {
        ssl_opts = ssl_opts.with_root_certs(vec![root_cert.clone().into()]);
    }
    match (&tls.client_cert, &tls.client_key) {
        (Some(cert), Some(key)) => {
            ssl_opts = ssl_opts.with_client_identity(Some(ClientIdentity::new(
                cert.clone().into(),
                key.clone().into(),
            )));
        }
        (None, None) => {}
        _ => anyhow::bail!("`ssl_cert` and `ssl_key` must be set together"),
    }
    ssl_opts = match tls.mode {
        DatabaseSslMode::Disable | DatabaseSslMode::Prefer | DatabaseSslMode::Require => {
            ssl_opts.with_danger_accept_invalid_certs(true)
        }
        DatabaseSslMode::VerifyCa => ssl_opts.with_danger_skip_domain_validation(true),
        DatabaseSslMode::VerifyFull => ssl_opts,
    };
    Ok(Some(ssl_opts))
}

fn is_tls_unsupported(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<mysql_async::Error>(),
            Some(mysql_async::Error::Driver(
                DriverError::NoClientSslFlagFromServer
            ))
        )
    })
}

/// Whether an error means the server rejected the credentials, so asking for a password may help.
pub fn is_authentication_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<mysql_async::Error>(),
            Some(mysql_async::Error::Server(server_error))
                if matches!(server_error.code, 1045 | 1698)
        )
    })
}

impl MysqlSession {
    async fn query_rows(&self, sql: &str, params: Vec<Value>) -> Result<Vec<Row>> {
        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().context("the connection is closed")?;
        Ok(conn.exec(sql, params).await?)
    }
}

#[async_trait]
impl DatabaseSession for MysqlSession {
    fn driver(&self) -> DriverKind {
        DriverKind::Mysql
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        let rows = self
            .query_rows(
                "SELECT schema_name FROM information_schema.schemata \
                 WHERE schema_name NOT IN ('information_schema', 'mysql', 'performance_schema', 'sys') \
                 ORDER BY schema_name = DATABASE() DESC, schema_name",
                Vec::new(),
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|row| value_string(row.as_ref(0)?))
            .map(|name| SchemaInfo { name: name.into() })
            .collect())
    }

    async fn list_relations(&self, schema: &str) -> Result<Vec<RelationInfo>> {
        let rows = self
            .query_rows(
                "SELECT table_name, table_type FROM information_schema.tables \
                 WHERE table_schema = ? AND table_type <> 'SEQUENCE' ORDER BY table_name",
                vec![Value::from(schema)],
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let name = value_string(row.as_ref(0)?)?;
                let kind = match value_string(row.as_ref(1)?)?.as_str() {
                    "VIEW" | "SYSTEM VIEW" => RelationKind::View,
                    _ => RelationKind::Table,
                };
                Some(RelationInfo {
                    name: name.into(),
                    kind,
                })
            })
            .collect())
    }

    async fn list_columns(&self, schema: &str, relation: &str) -> Result<Vec<ColumnInfo>> {
        let rows = self
            .query_rows(
                "SELECT column_name, column_type, is_nullable, column_key \
                 FROM information_schema.columns \
                 WHERE table_schema = ? AND table_name = ? ORDER BY ordinal_position",
                vec![Value::from(schema), Value::from(relation)],
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                Some(ColumnInfo {
                    name: value_string(row.as_ref(0)?)?.into(),
                    data_type: value_string(row.as_ref(1)?)?.into(),
                    nullable: value_string(row.as_ref(2)?)? == "YES",
                    primary_key: row
                        .as_ref(3)
                        .and_then(value_string)
                        .is_some_and(|key| key == "PRI"),
                })
            })
            .collect())
    }

    async fn relation_ddl(&self, schema: &str, relation: &str) -> Result<String> {
        let name = qualified_name(DriverKind::Mysql, schema, relation);
        let rows = self
            .query_rows(&format!("SHOW CREATE TABLE {name}"), Vec::new())
            .await?;
        let ddl = rows
            .first()
            .and_then(|row| row.as_ref(1))
            .and_then(value_string)
            .with_context(|| format!("no definition returned for {name}"))?;
        Ok(format!("{ddl};"))
    }

    async fn list_schema_objects(&self, schema: &str) -> Result<SchemaObjects> {
        let routines = self
            .query_rows(
                "SELECT r.routine_name, r.routine_type, \
                 (SELECT GROUP_CONCAT(CONCAT_WS(' ', \
                     CASE WHEN r.routine_type = 'PROCEDURE' THEN p.parameter_mode END, \
                     p.parameter_name, p.dtd_identifier) \
                     ORDER BY p.ordinal_position SEPARATOR ', ') \
                  FROM information_schema.parameters p \
                  WHERE p.specific_schema = r.routine_schema \
                  AND p.specific_name = r.specific_name AND p.ordinal_position > 0) \
                 FROM information_schema.routines r \
                 WHERE r.routine_schema = ? ORDER BY r.routine_name",
                vec![Value::from(schema)],
            )
            .await?
            .iter()
            .filter_map(|row| {
                Some(RoutineInfo {
                    name: value_string(row.as_ref(0)?)?.into(),
                    kind: if value_string(row.as_ref(1)?)? == "PROCEDURE" {
                        RoutineKind::Procedure
                    } else {
                        RoutineKind::Function
                    },
                    arguments: row
                        .as_ref(2)
                        .and_then(value_string)
                        .unwrap_or_default()
                        .into(),
                })
            })
            .collect();
        // Only MariaDB has sequences.
        let sequences = self
            .query_rows(
                "SELECT table_name FROM information_schema.tables \
                 WHERE table_schema = ? AND table_type = 'SEQUENCE' ORDER BY table_name",
                vec![Value::from(schema)],
            )
            .await?
            .iter()
            .filter_map(|row| Some(value_string(row.as_ref(0)?)?.into()))
            .collect();
        Ok(SchemaObjects {
            routines,
            sequences,
        })
    }

    async fn list_relation_details(&self, schema: &str, relation: &str) -> Result<RelationDetails> {
        let params = || vec![Value::from(schema), Value::from(relation)];
        let strings = |row: &Row, count: usize| -> Vec<Option<String>> {
            (0..count)
                .map(|index| row.as_ref(index).and_then(value_string))
                .collect()
        };

        let key_rows = self
            .query_rows(
                "SELECT tc.constraint_name, tc.constraint_type, k.column_name \
                 FROM information_schema.table_constraints tc \
                 JOIN information_schema.key_column_usage k \
                 ON k.constraint_schema = tc.constraint_schema \
                 AND k.constraint_name = tc.constraint_name AND k.table_name = tc.table_name \
                 WHERE tc.table_schema = ? AND tc.table_name = ? \
                 AND tc.constraint_type IN ('PRIMARY KEY', 'UNIQUE') \
                 ORDER BY tc.constraint_type = 'PRIMARY KEY' DESC, tc.constraint_name, \
                 k.ordinal_position",
                params(),
            )
            .await?;
        let mut keys: Vec<KeyInfo> = Vec::new();
        for row in &key_rows {
            let [Some(name), Some(kind), Some(column)] =
                <[_; 3]>::try_from(strings(row, 3)).unwrap_or_default()
            else {
                continue;
            };
            match keys.last_mut() {
                Some(key) if key.name.as_ref() == name => key.columns.push(column.into()),
                _ => keys.push(KeyInfo {
                    name: name.into(),
                    primary: kind == "PRIMARY KEY",
                    columns: vec![column.into()],
                }),
            }
        }

        let foreign_key_rows = self
            .query_rows(
                "SELECT constraint_name, column_name, referenced_table_schema, \
                 referenced_table_name, referenced_column_name \
                 FROM information_schema.key_column_usage \
                 WHERE table_schema = ? AND table_name = ? AND referenced_table_name IS NOT NULL \
                 ORDER BY constraint_name, ordinal_position",
                params(),
            )
            .await?;
        let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
        for row in &foreign_key_rows {
            let [
                Some(name),
                Some(column),
                Some(referenced_schema),
                Some(referenced_relation),
                Some(referenced_column),
            ] = <[_; 5]>::try_from(strings(row, 5)).unwrap_or_default()
            else {
                continue;
            };
            match foreign_keys.last_mut() {
                Some(foreign_key) if foreign_key.name.as_ref() == name => {
                    foreign_key.columns.push(column.into());
                    foreign_key
                        .referenced_columns
                        .push(referenced_column.into());
                }
                _ => foreign_keys.push(ForeignKeyInfo {
                    name: name.into(),
                    columns: vec![column.into()],
                    referenced_schema: referenced_schema.into(),
                    referenced_relation: referenced_relation.into(),
                    referenced_columns: vec![referenced_column.into()],
                }),
            }
        }

        let index_rows = self
            .query_rows(
                "SELECT index_name, non_unique, column_name FROM information_schema.statistics \
                 WHERE table_schema = ? AND table_name = ? \
                 ORDER BY index_name = 'PRIMARY' DESC, index_name, seq_in_index",
                params(),
            )
            .await?;
        let mut indexes: Vec<IndexInfo> = Vec::new();
        for row in &index_rows {
            let [Some(name), Some(non_unique), column] =
                <[_; 3]>::try_from(strings(row, 3)).unwrap_or_default()
            else {
                continue;
            };
            // Functional key parts have no column name.
            let column = column.unwrap_or_else(|| "<expression>".to_string());
            match indexes.last_mut() {
                Some(index) if index.name.as_ref() == name => index.columns.push(column.into()),
                _ => indexes.push(IndexInfo {
                    name: name.into(),
                    unique: non_unique == "0",
                    columns: vec![column.into()],
                }),
            }
        }

        let triggers = self
            .query_rows(
                "SELECT trigger_name, action_timing, event_manipulation \
                 FROM information_schema.triggers \
                 WHERE event_object_schema = ? AND event_object_table = ? ORDER BY trigger_name",
                params(),
            )
            .await?
            .iter()
            .filter_map(|row| {
                let [Some(name), Some(timing), Some(event)] =
                    <[_; 3]>::try_from(strings(row, 3)).unwrap_or_default()
                else {
                    return None;
                };
                Some(TriggerInfo {
                    name: name.into(),
                    description: format!("{timing} {event}").into(),
                })
            })
            .collect();

        Ok(RelationDetails {
            keys,
            foreign_keys,
            indexes,
            triggers,
        })
    }

    async fn object_definition(&self, schema: &str, object: &ObjectRef) -> Result<String> {
        let (statement, column) = match object {
            ObjectRef::Routine(routine) => {
                let keyword = match routine.kind {
                    RoutineKind::Function => "FUNCTION",
                    RoutineKind::Procedure => "PROCEDURE",
                };
                (
                    format!(
                        "SHOW CREATE {keyword} {}",
                        qualified_name(DriverKind::Mysql, schema, &routine.name)
                    ),
                    2,
                )
            }
            ObjectRef::Trigger { name, .. } => (
                format!(
                    "SHOW CREATE TRIGGER {}",
                    qualified_name(DriverKind::Mysql, schema, name)
                ),
                2,
            ),
            ObjectRef::Sequence(name) => (
                format!(
                    "SHOW CREATE SEQUENCE {}",
                    qualified_name(DriverKind::Mysql, schema, name)
                ),
                1,
            ),
            ObjectRef::Index { relation, name } => {
                let details = self.list_relation_details(schema, relation).await?;
                let index = details
                    .indexes
                    .iter()
                    .find(|index| &index.name == name)
                    .with_context(|| format!("index {name} not found"))?;
                let columns = index
                    .columns
                    .iter()
                    .map(|column| quote_identifier(DriverKind::Mysql, column))
                    .collect::<Vec<_>>()
                    .join(", ");
                let table = qualified_name(DriverKind::Mysql, schema, relation);
                return Ok(if name.as_ref() == "PRIMARY" {
                    format!("ALTER TABLE {table} ADD PRIMARY KEY ({columns});")
                } else {
                    format!(
                        "CREATE {}INDEX {} ON {table} ({columns});",
                        if index.unique { "UNIQUE " } else { "" },
                        quote_identifier(DriverKind::Mysql, name),
                    )
                });
            }
        };
        let rows = self.query_rows(&statement, Vec::new()).await?;
        let definition = rows
            .first()
            .and_then(|row| row.as_ref(column))
            .and_then(value_string)
            .with_context(|| {
                format!(
                    "the definition of {} isn't available; it may require more privileges",
                    object.name()
                )
            })?;
        Ok(format!("{};", definition.trim_end().trim_end_matches(';')))
    }

    fn execute(&self, sql: String, options: ExecOptions) -> ResultStream {
        let conn = self.conn.clone();
        let session_read_only = self.read_only;
        ResultStream::spawn(Some(self.cancel.clone()), move |mut sender| async move {
            let mut guard = conn.lock_owned().await;
            let Some(conn) = guard.as_mut() else {
                sender.send(Err(anyhow!("the connection is closed"))).await;
                return;
            };
            if let Err(error) =
                run_statement(conn, &sql, options, session_read_only, &mut sender).await
            {
                sender.send(Err(error)).await;
            }
        })
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        self.cancel.clone()
    }

    fn is_closed(&self) -> bool {
        self.conn.try_lock().is_ok_and(|conn| conn.is_none())
    }
}

async fn run_statement(
    conn: &mut Conn,
    sql: &str,
    options: ExecOptions,
    session_read_only: bool,
    sender: &mut ResultSender,
) -> Result<()> {
    if options.read_only_transaction {
        anyhow::ensure!(
            split_statements(sql).len() <= 1,
            "agent queries must be a single statement"
        );
        // DDL implicitly commits the current transaction and then runs outside of it, so only a
        // read-only session keeps it from changing the database.
        conn.query_drop("SET SESSION TRANSACTION READ ONLY").await?;
        conn.query_drop("START TRANSACTION READ ONLY").await?;
        // Prepared statements can't contain more than one statement either, so the transaction
        // can't be ended from within the input.
        let result = match conn.exec_iter(sql, ()).await {
            Ok(result) => stream_results(result, sender).await,
            Err(error) => Err(error.into()),
        };
        conn.query_drop("ROLLBACK").await?;
        if !session_read_only {
            conn.query_drop("SET SESSION TRANSACTION READ WRITE")
                .await?;
        }
        return result;
    }
    let result = conn.query_iter(sql).await?;
    stream_results(result, sender).await
}

async fn stream_results<P: Protocol>(
    mut result: mysql_async::QueryResult<'_, '_, P>,
    sender: &mut ResultSender,
) -> Result<()> {
    let mut batcher = RowBatcher::default();
    loop {
        let columns = result.columns();
        let column_kinds = match &columns {
            Some(columns) if !columns.is_empty() => {
                let metas = columns
                    .iter()
                    .map(|column| ColumnMeta {
                        name: column.name_str().into_owned().into(),
                        type_name: type_name(column.column_type(), column.character_set()).into(),
                        kind: value_kind(column.column_type(), column.character_set()),
                    })
                    .collect::<Vec<_>>();
                let kinds = metas.iter().map(|meta| meta.kind).collect::<Vec<_>>();
                if !sender.send(Ok(ResultEvent::Columns(metas))).await {
                    return Ok(());
                }
                Some(kinds)
            }
            _ => None,
        };
        // Read before `next` moves on to the following result set.
        let rows_affected = column_kinds.is_none().then(|| result.affected_rows());

        // Returns `None` at the end of the current result set, after moving to the next one.
        while let Some(row) = result.next().await? {
            let kinds = column_kinds.as_deref().unwrap_or_default();
            let values = (0..row.len())
                .map(|index| {
                    let value = row.as_ref(index)?;
                    if kinds.get(index) == Some(&ValueKind::Binary)
                        && let Value::Bytes(bytes) = value
                    {
                        return Some(blob_value(bytes.len()));
                    }
                    value_string(value).map(|value| display_value(&value))
                })
                .collect();
            if let Some(batch) = batcher.push(values)
                && !sender.send(Ok(batch)).await
            {
                return Ok(());
            }
        }
        if let Some(batch) = batcher.take()
            && !sender.send(Ok(batch)).await
        {
            return Ok(());
        }
        if !sender
            .send(Ok(ResultEvent::StatementComplete { rows_affected }))
            .await
        {
            return Ok(());
        }
        if result.is_empty() {
            return Ok(());
        }
    }
}

fn value_string(value: &Value) -> Option<String> {
    Some(match value {
        Value::NULL => return None,
        Value::Bytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        Value::Int(value) => value.to_string(),
        Value::UInt(value) => value.to_string(),
        Value::Float(value) => value.to_string(),
        Value::Double(value) => value.to_string(),
        Value::Date(year, month, day, hour, minute, second, micros) => {
            if (*hour, *minute, *second, *micros) == (0, 0, 0, 0) {
                format!("{year:04}-{month:02}-{day:02}")
            } else if *micros == 0 {
                format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
            } else {
                format!(
                    "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{micros:06}"
                )
            }
        }
        Value::Time(negative, days, hours, minutes, seconds, micros) => {
            let sign = if *negative { "-" } else { "" };
            let hours = *days * 24 + u32::from(*hours);
            if *micros == 0 {
                format!("{sign}{hours:02}:{minutes:02}:{seconds:02}")
            } else {
                format!("{sign}{hours:02}:{minutes:02}:{seconds:02}.{micros:06}")
            }
        }
    })
}

fn value_kind(column_type: ColumnType, charset: u16) -> ValueKind {
    use ColumnType::*;
    match column_type {
        MYSQL_TYPE_DECIMAL
        | MYSQL_TYPE_NEWDECIMAL
        | MYSQL_TYPE_TINY
        | MYSQL_TYPE_SHORT
        | MYSQL_TYPE_LONG
        | MYSQL_TYPE_LONGLONG
        | MYSQL_TYPE_INT24
        | MYSQL_TYPE_FLOAT
        | MYSQL_TYPE_DOUBLE
        | MYSQL_TYPE_YEAR => ValueKind::Number,
        MYSQL_TYPE_DATE
        | MYSQL_TYPE_NEWDATE
        | MYSQL_TYPE_DATETIME
        | MYSQL_TYPE_DATETIME2
        | MYSQL_TYPE_TIMESTAMP
        | MYSQL_TYPE_TIMESTAMP2
        | MYSQL_TYPE_TIME
        | MYSQL_TYPE_TIME2 => ValueKind::DateTime,
        MYSQL_TYPE_TINY_BLOB
        | MYSQL_TYPE_MEDIUM_BLOB
        | MYSQL_TYPE_LONG_BLOB
        | MYSQL_TYPE_BLOB
        | MYSQL_TYPE_STRING
        | MYSQL_TYPE_VAR_STRING
        | MYSQL_TYPE_VARCHAR
            if charset == BINARY_CHARSET =>
        {
            ValueKind::Binary
        }
        MYSQL_TYPE_GEOMETRY => ValueKind::Binary,
        _ => ValueKind::Text,
    }
}

fn type_name(column_type: ColumnType, charset: u16) -> &'static str {
    use ColumnType::*;
    let binary = charset == BINARY_CHARSET;
    match column_type {
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "decimal",
        MYSQL_TYPE_TINY => "tinyint",
        MYSQL_TYPE_SHORT => "smallint",
        MYSQL_TYPE_INT24 => "mediumint",
        MYSQL_TYPE_LONG => "int",
        MYSQL_TYPE_LONGLONG => "bigint",
        MYSQL_TYPE_FLOAT => "float",
        MYSQL_TYPE_DOUBLE => "double",
        MYSQL_TYPE_YEAR => "year",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "datetime",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "timestamp",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "time",
        MYSQL_TYPE_BIT => "bit",
        MYSQL_TYPE_JSON => "json",
        MYSQL_TYPE_ENUM => "enum",
        MYSQL_TYPE_SET => "set",
        MYSQL_TYPE_GEOMETRY => "geometry",
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
            if binary {
                "blob"
            } else {
                "text"
            }
        }
        MYSQL_TYPE_STRING => {
            if binary {
                "binary"
            } else {
                "char"
            }
        }
        MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR => {
            if binary {
                "varbinary"
            } else {
                "varchar"
            }
        }
        MYSQL_TYPE_NULL => "null",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_value_string() {
        assert_eq!(value_string(&Value::NULL), None);
        assert_eq!(
            value_string(&Value::Date(2026, 10, 3, 0, 0, 0, 0)).as_deref(),
            Some("2026-10-03")
        );
        assert_eq!(
            value_string(&Value::Date(2026, 10, 3, 7, 5, 9, 120)).as_deref(),
            Some("2026-10-03 07:05:09.000120")
        );
        assert_eq!(
            value_string(&Value::Time(true, 1, 2, 3, 4, 0)).as_deref(),
            Some("-26:03:04")
        );
        assert_eq!(
            value_string(&Value::Bytes(b"text".to_vec())).as_deref(),
            Some("text")
        );
    }

    #[test]
    fn test_binary_columns() {
        assert_eq!(
            value_kind(ColumnType::MYSQL_TYPE_BLOB, BINARY_CHARSET),
            ValueKind::Binary
        );
        assert_eq!(
            value_kind(ColumnType::MYSQL_TYPE_BLOB, 255),
            ValueKind::Text
        );
        assert_eq!(
            value_kind(ColumnType::MYSQL_TYPE_LONGLONG, BINARY_CHARSET),
            ValueKind::Number
        );
    }
}
