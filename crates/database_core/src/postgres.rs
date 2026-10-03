use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use async_trait::async_trait;
use futures::{StreamExt as _, pin_mut};
use settings::DatabaseSslMode;
use tokio_postgres::{
    CancelToken, Client, SimpleQueryMessage,
    types::{Kind, Type},
};

use crate::{
    connection::{DriverKind, ResolvedConnection},
    driver::{
        CancelHandle, ColumnInfo, ColumnMeta, DatabaseSession, ExecOptions, RelationInfo,
        RelationKind, ResultEvent, ResultStream, RowBatcher, SchemaInfo, ValueKind, blob_value,
        display_value, qualified_name, quote_identifier,
    },
    tls::{MakeRustlsConnect, client_config},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

pub struct PostgresSession {
    client: Arc<Client>,
    cancel: Arc<PostgresCancel>,
    connection_task: tokio::task::JoinHandle<()>,
}

impl Drop for PostgresSession {
    fn drop(&mut self) {
        self.connection_task.abort();
    }
}

struct PostgresCancel {
    token: CancelToken,
    tls: MakeRustlsConnect,
}

#[async_trait]
impl CancelHandle for PostgresCancel {
    async fn cancel(&self) -> Result<()> {
        self.token
            .cancel_query(self.tls.clone())
            .await
            .context("sending the cancel request")
    }
}

/// Connects to `host:port`, which may be the local end of an SSH tunnel.
///
/// Must be called on the Tokio runtime.
pub async fn connect(
    connection: &ResolvedConnection,
    host: &str,
    port: u16,
) -> Result<PostgresSession> {
    let mut config = tokio_postgres::Config::new();
    config
        .host(host)
        .port(port)
        .application_name("Zed")
        .connect_timeout(CONNECT_TIMEOUT)
        .ssl_mode(match connection.tls.mode {
            DatabaseSslMode::Disable => tokio_postgres::config::SslMode::Disable,
            DatabaseSslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
            DatabaseSslMode::Require | DatabaseSslMode::VerifyCa | DatabaseSslMode::VerifyFull => {
                tokio_postgres::config::SslMode::Require
            }
        });
    if let Some(username) = &connection.username {
        config.user(username);
    } else if let Ok(username) = std::env::var("USER") {
        config.user(username);
    }
    if let Some(password) = &connection.password {
        config.password(password);
    }
    if let Some(database) = &connection.database {
        config.dbname(database);
    }
    let mut options = Vec::new();
    if connection.read_only {
        options.push("-c default_transaction_read_only=on".to_string());
    }
    if let Some(timeout) = connection.statement_timeout {
        options.push(format!("-c statement_timeout={}", timeout.as_millis()));
    }
    if !options.is_empty() {
        config.options(options.join(" "));
    }

    let tls = MakeRustlsConnect::new(client_config(&connection.tls).await?);
    let (client, connection_future) = config.connect(tls.clone()).await?;
    let connection_task = tokio::spawn(async move {
        if let Err(error) = connection_future.await {
            log::info!("PostgreSQL connection closed: {error}");
        }
    });

    let cancel = Arc::new(PostgresCancel {
        token: client.cancel_token(),
        tls,
    });
    Ok(PostgresSession {
        client: Arc::new(client),
        cancel,
        connection_task,
    })
}

/// Whether an error means the server rejected the credentials, so asking for a password may help.
pub fn is_authentication_error(error: &anyhow::Error) -> bool {
    let Some(postgres_error) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<tokio_postgres::Error>())
    else {
        return false;
    };
    if let Some(db_error) = postgres_error.as_db_error() {
        // invalid_password, invalid_authorization_specification
        return matches!(db_error.code().code(), "28P01" | "28000");
    }
    // The client reports a missing password as a configuration error, with the reason in the
    // error's source.
    format!("{error:#}").contains("password missing")
}

impl PostgresSession {
    async fn query_strings(
        &self,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<Vec<tokio_postgres::Row>> {
        Ok(self.client.query(sql, params).await?)
    }
}

#[async_trait]
impl DatabaseSession for PostgresSession {
    fn driver(&self) -> DriverKind {
        DriverKind::Postgres
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        let rows = self
            .query_strings(
                "SELECT nspname::text FROM pg_catalog.pg_namespace \
                 WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema' \
                 ORDER BY nspname = 'public' DESC, nspname",
                &[],
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(SchemaInfo {
                    name: row.try_get::<_, String>(0)?.into(),
                })
            })
            .collect()
    }

    async fn list_relations(&self, schema: &str) -> Result<Vec<RelationInfo>> {
        let rows = self
            .query_strings(
                "SELECT c.relname::text, c.relkind::text FROM pg_catalog.pg_class c \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm', 'f') \
                 ORDER BY c.relname",
                &[&schema],
            )
            .await?;
        rows.iter()
            .map(|row| {
                let kind = match row.try_get::<_, String>(1)?.as_str() {
                    "v" => RelationKind::View,
                    "m" => RelationKind::MaterializedView,
                    "f" => RelationKind::ForeignTable,
                    _ => RelationKind::Table,
                };
                Ok(RelationInfo {
                    name: row.try_get::<_, String>(0)?.into(),
                    kind,
                })
            })
            .collect()
    }

    async fn list_columns(&self, schema: &str, relation: &str) -> Result<Vec<ColumnInfo>> {
        let rows = self
            .query_strings(
                "SELECT a.attname::text, pg_catalog.format_type(a.atttypid, a.atttypmod), \
                 NOT a.attnotnull, \
                 EXISTS (SELECT 1 FROM pg_catalog.pg_index i \
                         WHERE i.indrelid = c.oid AND i.indisprimary \
                         AND a.attnum = ANY(i.indkey::smallint[])) \
                 FROM pg_catalog.pg_attribute a \
                 JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                &[&schema, &relation],
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(ColumnInfo {
                    name: row.try_get::<_, String>(0)?.into(),
                    data_type: row.try_get::<_, String>(1)?.into(),
                    nullable: row.try_get(2)?,
                    primary_key: row.try_get(3)?,
                })
            })
            .collect()
    }

    async fn relation_ddl(&self, schema: &str, relation: &str) -> Result<String> {
        let rows = self
            .query_strings(
                "SELECT c.oid::bigint, c.relkind::text, \
                 CASE WHEN c.relkind IN ('v', 'm') THEN pg_catalog.pg_get_viewdef(c.oid, true) END \
                 FROM pg_catalog.pg_class c \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2",
                &[&schema, &relation],
            )
            .await?;
        let row = rows
            .first()
            .with_context(|| format!("relation {schema}.{relation} not found"))?;
        let oid: i64 = row.try_get(0)?;
        let kind: String = row.try_get(1)?;
        let name = qualified_name(DriverKind::Postgres, schema, relation);
        if let Some(definition) = row.try_get::<_, Option<String>>(2)? {
            let keyword = if kind == "m" {
                "MATERIALIZED VIEW"
            } else {
                "VIEW"
            };
            return Ok(format!(
                "CREATE {keyword} {name} AS\n{};",
                definition.trim_end().trim_end_matches(';')
            ));
        }

        let columns = self
            .query_strings(
                "SELECT a.attname::text, pg_catalog.format_type(a.atttypid, a.atttypmod), \
                 a.attnotnull, pg_catalog.pg_get_expr(d.adbin, d.adrelid) \
                 FROM pg_catalog.pg_attribute a \
                 LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 WHERE a.attrelid = $1::bigint::oid AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                &[&oid],
            )
            .await?;
        let constraints = self
            .query_strings(
                "SELECT conname::text, pg_catalog.pg_get_constraintdef(oid, true) \
                 FROM pg_catalog.pg_constraint WHERE conrelid = $1::bigint::oid \
                 ORDER BY contype = 'p' DESC, conname",
                &[&oid],
            )
            .await?;
        let indexes = self
            .query_strings(
                "SELECT pg_catalog.pg_get_indexdef(i.indexrelid) FROM pg_catalog.pg_index i \
                 WHERE i.indrelid = $1::bigint::oid AND NOT EXISTS ( \
                     SELECT 1 FROM pg_catalog.pg_constraint con WHERE con.conindid = i.indexrelid) \
                 ORDER BY i.indexrelid",
                &[&oid],
            )
            .await?;

        let mut lines = Vec::new();
        for column in &columns {
            let mut line = format!(
                "    {} {}",
                quote_identifier(DriverKind::Postgres, &column.try_get::<_, String>(0)?),
                column.try_get::<_, String>(1)?
            );
            if let Some(default) = column.try_get::<_, Option<String>>(3)? {
                line.push_str(&format!(" DEFAULT {default}"));
            }
            if column.try_get::<_, bool>(2)? {
                line.push_str(" NOT NULL");
            }
            lines.push(line);
        }
        for constraint in &constraints {
            lines.push(format!(
                "    CONSTRAINT {} {}",
                quote_identifier(DriverKind::Postgres, &constraint.try_get::<_, String>(0)?),
                constraint.try_get::<_, String>(1)?
            ));
        }
        let keyword = if kind == "f" {
            "FOREIGN TABLE"
        } else {
            "TABLE"
        };
        let mut ddl = format!("CREATE {keyword} {name} (\n{}\n);", lines.join(",\n"));
        for index in &indexes {
            ddl.push_str(&format!("\n{};", index.try_get::<_, String>(0)?));
        }
        Ok(ddl)
    }

    fn execute(&self, sql: String, options: ExecOptions) -> ResultStream {
        let client = self.client.clone();
        ResultStream::spawn(Some(self.cancel.clone()), move |mut sender| async move {
            if let Err(error) = run_statement(&client, &sql, options, &mut sender).await {
                sender.send(Err(error)).await;
            }
        })
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        self.cancel.clone()
    }

    fn is_closed(&self) -> bool {
        self.client.is_closed()
    }
}

async fn run_statement(
    client: &Client,
    sql: &str,
    options: ExecOptions,
    sender: &mut crate::driver::ResultSender,
) -> Result<()> {
    // Preparing parses the input without running it. It reports column types, which the simple
    // query protocol doesn't, and fails for input that contains several statements.
    let column_types = match client.prepare(sql).await {
        Ok(statement) => Some(
            statement
                .columns()
                .iter()
                .map(|column| column.type_().clone())
                .collect::<Vec<_>>(),
        ),
        Err(error) if options.read_only_transaction => {
            return Err(anyhow!(error).context("agent queries must be a single statement"));
        }
        Err(_) => None,
    };

    if options.read_only_transaction {
        client.batch_execute("BEGIN READ ONLY").await?;
    }
    let result = stream_simple_query(client, sql, column_types.as_deref(), sender).await;
    if options.read_only_transaction {
        client.batch_execute("ROLLBACK").await?;
    }
    result
}

async fn stream_simple_query(
    client: &Client,
    sql: &str,
    column_types: Option<&[Type]>,
    sender: &mut crate::driver::ResultSender,
) -> Result<()> {
    let stream = client.simple_query_raw(sql).await?;
    pin_mut!(stream);

    let mut batcher = RowBatcher::default();
    let mut current_kinds: Vec<ValueKind> = Vec::new();
    let mut last_row_count = None;
    while let Some(message) = stream.next().await {
        match message? {
            SimpleQueryMessage::RowDescription(columns) => {
                if let Some(rows) = batcher.take()
                    && !sender.send(Ok(rows)).await
                {
                    return Ok(());
                }
                // The prepared types only describe the input when it is a single statement.
                let types = column_types.filter(|types| types.len() == columns.len());
                let metas = columns
                    .iter()
                    .enumerate()
                    .map(|(index, column)| {
                        let type_ = types.and_then(|types| types.get(index));
                        ColumnMeta {
                            name: column.name().to_string().into(),
                            type_name: type_
                                .map(|type_| type_.name().to_string())
                                .unwrap_or_default()
                                .into(),
                            kind: type_.map(value_kind).unwrap_or_default(),
                        }
                    })
                    .collect::<Vec<_>>();
                current_kinds = metas.iter().map(|meta| meta.kind).collect();
                if !sender.send(Ok(ResultEvent::Columns(metas))).await {
                    return Ok(());
                }
            }
            SimpleQueryMessage::Row(row) => {
                let values = (0..row.len())
                    .map(|index| {
                        row.get(index).map(|value| {
                            if current_kinds.get(index) == Some(&ValueKind::Binary) {
                                // bytea values arrive hex-encoded as `\x0a1b...`.
                                blob_value(value.len().saturating_sub(2) / 2)
                            } else {
                                display_value(value)
                            }
                        })
                    })
                    .collect();
                if let Some(rows) = batcher.push(values)
                    && !sender.send(Ok(rows)).await
                {
                    return Ok(());
                }
            }
            SimpleQueryMessage::CommandComplete(rows) => {
                if let Some(batch) = batcher.take()
                    && !sender.send(Ok(batch)).await
                {
                    return Ok(());
                }
                last_row_count = Some(rows);
                if !sender
                    .send(Ok(ResultEvent::StatementComplete {
                        rows_affected: Some(rows),
                    }))
                    .await
                {
                    return Ok(());
                }
            }
            _ => {}
        }
    }
    if let Some(rows) = batcher.take() {
        sender.send(Ok(rows)).await;
    }
    log::trace!("PostgreSQL statement finished, last row count {last_row_count:?}");
    Ok(())
}

fn value_kind(type_: &Type) -> ValueKind {
    match *type_ {
        Type::INT2
        | Type::INT4
        | Type::INT8
        | Type::FLOAT4
        | Type::FLOAT8
        | Type::NUMERIC
        | Type::OID
        | Type::MONEY => ValueKind::Number,
        Type::BOOL => ValueKind::Boolean,
        Type::DATE | Type::TIME | Type::TIMETZ | Type::TIMESTAMP | Type::TIMESTAMPTZ => {
            ValueKind::DateTime
        }
        Type::BYTEA => ValueKind::Binary,
        _ => match type_.kind() {
            Kind::Domain(inner) => value_kind(inner),
            _ => ValueKind::Text,
        },
    }
}
