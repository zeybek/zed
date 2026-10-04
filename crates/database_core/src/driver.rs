use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use anyhow::Result;
use async_trait::async_trait;
use futures::{Stream, channel::mpsc};
use gpui::SharedString;

use crate::connection::DriverKind;

/// Longest cell value kept in a result, in bytes. Longer values are cut off with an ellipsis.
pub const MAX_CELL_BYTES: usize = 4 * 1024;
/// Number of rows a driver sends at once.
pub const ROW_BATCH_SIZE: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaInfo {
    pub name: SharedString,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RelationKind {
    Table,
    View,
    MaterializedView,
    ForeignTable,
}

impl RelationKind {
    pub fn is_view(self) -> bool {
        matches!(self, RelationKind::View | RelationKind::MaterializedView)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelationInfo {
    pub name: SharedString,
    pub kind: RelationKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: SharedString,
    pub data_type: SharedString,
    pub nullable: bool,
    pub primary_key: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RoutineKind {
    Function,
    Procedure,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutineInfo {
    pub name: SharedString,
    pub kind: RoutineKind,
    /// The argument list, such as `a integer, b text`, which tells overloads apart.
    pub arguments: SharedString,
}

/// Objects of a schema other than its tables and views.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchemaObjects {
    pub routines: Vec<RoutineInfo>,
    pub sequences: Vec<SharedString>,
}

/// A primary key or unique constraint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInfo {
    pub name: SharedString,
    pub primary: bool,
    pub columns: Vec<SharedString>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKeyInfo {
    pub name: SharedString,
    pub columns: Vec<SharedString>,
    pub referenced_schema: SharedString,
    pub referenced_relation: SharedString,
    pub referenced_columns: Vec<SharedString>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexInfo {
    pub name: SharedString,
    /// Column names, or expressions for indexes on expressions.
    pub columns: Vec<SharedString>,
    pub unique: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TriggerInfo {
    pub name: SharedString,
    /// When the trigger fires and on which events, such as `BEFORE UPDATE`.
    pub description: SharedString,
}

/// What belongs to a table or view besides its columns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelationDetails {
    pub keys: Vec<KeyInfo>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
    pub indexes: Vec<IndexInfo>,
    pub triggers: Vec<TriggerInfo>,
}

/// A schema object whose definition can be shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObjectRef {
    Routine(RoutineInfo),
    Sequence(SharedString),
    Index {
        relation: SharedString,
        name: SharedString,
    },
    Trigger {
        relation: SharedString,
        name: SharedString,
    },
}

impl ObjectRef {
    pub fn name(&self) -> &SharedString {
        match self {
            ObjectRef::Routine(routine) => &routine.name,
            ObjectRef::Sequence(name)
            | ObjectRef::Index { name, .. }
            | ObjectRef::Trigger { name, .. } => name,
        }
    }
}

/// How the values of a result column compare, derived from its database type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ValueKind {
    #[default]
    Text,
    Number,
    Boolean,
    DateTime,
    /// Binary data, shown as `<blob N bytes>`.
    Binary,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnMeta {
    pub name: SharedString,
    pub type_name: SharedString,
    pub kind: ValueKind,
}

/// A row of display values. `None` is SQL `NULL`.
pub type ResultRow = Vec<Option<SharedString>>;

#[derive(Clone, Debug, PartialEq)]
pub enum ResultEvent {
    /// A new result set starts. Rows that follow belong to it.
    Columns(Vec<ColumnMeta>),
    Rows(Vec<ResultRow>),
    /// A statement finished. `rows_affected` is reported for statements that change data.
    StatementComplete {
        rows_affected: Option<u64>,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExecOptions {
    /// Run the statement in a read-only transaction that is rolled back afterwards, and refuse
    /// input that contains more than one statement. Used for agent queries, where the server
    /// must enforce that nothing is written.
    pub read_only_transaction: bool,
}

/// Cancels the statement that is currently running on a session.
#[async_trait]
pub trait CancelHandle: Send + Sync {
    async fn cancel(&self) -> Result<()>;
}

/// An open session with a database.
#[async_trait]
pub trait DatabaseSession: Send + Sync {
    fn driver(&self) -> DriverKind;
    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>>;
    async fn list_relations(&self, schema: &str) -> Result<Vec<RelationInfo>>;
    async fn list_columns(&self, schema: &str, relation: &str) -> Result<Vec<ColumnInfo>>;
    /// A `CREATE` statement that recreates the relation.
    async fn relation_ddl(&self, schema: &str, relation: &str) -> Result<String>;
    async fn list_schema_objects(&self, schema: &str) -> Result<SchemaObjects>;
    async fn list_relation_details(&self, schema: &str, relation: &str) -> Result<RelationDetails>;
    /// The statement that defines the object, as the database reports it.
    async fn object_definition(&self, schema: &str, object: &ObjectRef) -> Result<String>;
    /// Executes SQL, streaming results as they arrive.
    ///
    /// The stream applies backpressure: when it isn't polled, the driver stops reading from the
    /// server. Dropping it before it finishes cancels the statement on the server.
    fn execute(&self, sql: String, options: ExecOptions) -> ResultStream;
    fn cancel_handle(&self) -> Arc<dyn CancelHandle>;
    /// Whether the connection to the server was lost.
    fn is_closed(&self) -> bool;
}

/// A stream of [`ResultEvent`]s fed by a producer task on the Tokio runtime.
pub struct ResultStream {
    receiver: mpsc::Receiver<Result<ResultEvent>>,
    producer: Option<tokio::task::JoinHandle<()>>,
    cancel: Option<Arc<dyn CancelHandle>>,
    finished: bool,
}

impl ResultStream {
    /// Spawns `produce` on the current Tokio runtime. It sends events into the given sender and
    /// stops when sending fails, which happens once the stream is dropped.
    pub fn spawn<F, Fut>(cancel: Option<Arc<dyn CancelHandle>>, produce: F) -> Self
    where
        F: FnOnce(ResultSender) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        // A single slot keeps memory bounded: the producer waits until the previous batch was
        // taken before reading more rows from the server.
        let (sender, receiver) = mpsc::channel(1);
        let producer = tokio::spawn(produce(ResultSender(sender)));
        Self {
            receiver,
            producer: Some(producer),
            cancel,
            finished: false,
        }
    }

    /// A stream that fails immediately.
    pub fn error(error: anyhow::Error) -> Self {
        let (mut sender, receiver) = mpsc::channel(1);
        sender.try_send(Err(error)).ok();
        Self {
            receiver,
            producer: None,
            cancel: None,
            finished: false,
        }
    }
}

impl Stream for ResultStream {
    type Item = Result<ResultEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let poll = Pin::new(&mut self.receiver).poll_next(cx);
        // A failed statement has already ended on the server. Cancelling it when the stream is
        // dropped could hit the next statement of the session instead.
        if let Poll::Ready(None | Some(Err(_))) = poll {
            self.finished = true;
        }
        poll
    }
}

impl Drop for ResultStream {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Some(producer) = self.producer.take() {
            if producer.is_finished() {
                return;
            }
            producer.abort();
        }
        // Dropping the client side stops reading, but the server keeps running the statement
        // until it is told to stop.
        if let Some(cancel) = self.cancel.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                if let Err(error) = cancel.cancel().await {
                    log::warn!("failed to cancel database statement: {error:#}");
                }
            });
        }
    }
}

pub struct ResultSender(mpsc::Sender<Result<ResultEvent>>);

impl ResultSender {
    /// Sends an event, waiting while the consumer hasn't taken the previous one. Returns `false`
    /// once the consumer is gone.
    pub async fn send(&mut self, event: Result<ResultEvent>) -> bool {
        use futures::SinkExt as _;
        self.0.send(event).await.is_ok()
    }

    /// Like [`Self::send`], for producers running on a blocking thread.
    pub fn send_blocking(&mut self, event: Result<ResultEvent>) -> bool {
        futures::executor::block_on(self.send(event))
    }
}

/// Collects rows into batches of [`ROW_BATCH_SIZE`].
#[derive(Default)]
pub struct RowBatcher {
    rows: Vec<ResultRow>,
}

impl RowBatcher {
    pub fn push(&mut self, row: ResultRow) -> Option<ResultEvent> {
        self.rows.push(row);
        (self.rows.len() >= ROW_BATCH_SIZE)
            .then(|| self.take())
            .flatten()
    }

    pub fn take(&mut self) -> Option<ResultEvent> {
        (!self.rows.is_empty()).then(|| ResultEvent::Rows(std::mem::take(&mut self.rows)))
    }
}

/// Shortens a value to [`MAX_CELL_BYTES`] on a character boundary.
pub fn display_value(value: &str) -> SharedString {
    if value.len() <= MAX_CELL_BYTES {
        return SharedString::from(value.to_string());
    }
    let end = value.floor_char_boundary(MAX_CELL_BYTES);
    format!("{}…", &value[..end]).into()
}

pub fn blob_value(len: usize) -> SharedString {
    format!("<blob {len} bytes>").into()
}

/// Quotes an identifier for the given database when it isn't a plain lowercase word.
pub fn quote_identifier(driver: DriverKind, name: &str) -> String {
    let is_plain = name
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !RESERVED_WORDS.contains(&name);
    if is_plain {
        return name.to_string();
    }
    match driver {
        DriverKind::Mysql => format!("`{}`", name.replace('`', "``")),
        DriverKind::Postgres | DriverKind::Sqlite => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

/// Words that can't be used as unquoted identifiers in at least one supported database.
const RESERVED_WORDS: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "both",
    "case",
    "cast",
    "check",
    "collate",
    "column",
    "constraint",
    "create",
    "current_catalog",
    "current_date",
    "current_role",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "false",
    "fetch",
    "for",
    "foreign",
    "from",
    "grant",
    "group",
    "having",
    "in",
    "index",
    "initially",
    "insert",
    "intersect",
    "into",
    "key",
    "lateral",
    "leading",
    "limit",
    "localtime",
    "localtimestamp",
    "not",
    "null",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "placing",
    "primary",
    "references",
    "returning",
    "select",
    "session_user",
    "some",
    "symmetric",
    "table",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "update",
    "user",
    "using",
    "values",
    "variadic",
    "when",
    "where",
    "window",
    "with",
];

/// A qualified relation name, such as `public.users`. SQLite's `main` schema is omitted.
pub fn qualified_name(driver: DriverKind, schema: &str, relation: &str) -> String {
    if driver == DriverKind::Sqlite && schema == "main" {
        return quote_identifier(driver, relation);
    }
    format!(
        "{}.{}",
        quote_identifier(driver, schema),
        quote_identifier(driver, relation)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingCancel(std::sync::atomic::AtomicUsize);

    #[async_trait]
    impl CancelHandle for RecordingCancel {
        async fn cancel(&self) -> Result<()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_dropping_a_stream_cancels_only_running_statements() {
        use futures::StreamExt as _;

        // The producer is still running after sending its error, as when the stream is
        // dropped right after the error arrives.
        let cancel = Arc::new(RecordingCancel::default());
        let mut stream = ResultStream::spawn(Some(cancel.clone()), |mut sender| async move {
            sender.send(Err(anyhow::anyhow!("syntax error"))).await;
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        });
        assert!(stream.next().await.is_some_and(|event| event.is_err()));
        drop(stream);
        tokio::task::yield_now().await;
        assert_eq!(
            cancel.0.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a failed statement must not cancel the next one"
        );

        let cancel = Arc::new(RecordingCancel::default());
        let mut stream = ResultStream::spawn(Some(cancel.clone()), |mut sender| async move {
            sender.send(Ok(ResultEvent::Rows(Vec::new()))).await;
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        });
        assert!(stream.next().await.is_some_and(|event| event.is_ok()));
        drop(stream);
        for _ in 0..100 {
            if cancel.0.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(cancel.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn test_quote_identifier() {
        assert_eq!(quote_identifier(DriverKind::Postgres, "users"), "users");
        assert_eq!(quote_identifier(DriverKind::Postgres, "user"), "\"user\"");
        assert_eq!(quote_identifier(DriverKind::Postgres, "Users"), "\"Users\"");
        assert_eq!(
            quote_identifier(DriverKind::Postgres, "we\"ird"),
            "\"we\"\"ird\""
        );
        assert_eq!(
            quote_identifier(DriverKind::Mysql, "my table"),
            "`my table`"
        );
        assert_eq!(
            qualified_name(DriverKind::Sqlite, "main", "order"),
            "\"order\""
        );
        assert_eq!(
            qualified_name(DriverKind::Postgres, "public", "orders"),
            "public.orders"
        );
    }

    #[test]
    fn test_display_value_truncates_on_char_boundary() {
        let long = "é".repeat(MAX_CELL_BYTES);
        let value = display_value(&long);
        assert!(value.len() <= MAX_CELL_BYTES + "…".len());
        assert!(value.ends_with('…'));
        assert_eq!(display_value("short").as_ref(), "short");
    }
}
