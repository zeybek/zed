use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{
    StreamExt as _,
    channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
};
use gpui::{Context, EventEmitter, SharedString, Task};

use crate::{
    connection::{ConnectionKey, DriverKind},
    database_settings::MAX_RESULT_ROWS,
    driver::{CancelHandle, ColumnMeta, DatabaseSession, ExecOptions, ResultEvent, ResultRow},
};

/// The most cell bytes a result keeps in memory. Fetching stops once it's exceeded.
pub const MAX_RESULT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryState {
    Connecting,
    Running,
    /// Fetching stopped at a limit. More rows may be available.
    Paused {
        reason: TruncationReason,
    },
    Finished,
    Failed(SharedString),
    Cancelled,
}

impl QueryState {
    pub fn is_active(&self) -> bool {
        matches!(self, QueryState::Connecting | QueryState::Running)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TruncationReason {
    /// The configured row limit was reached. Loading more is possible.
    RowLimit,
    /// The result reached [`MAX_RESULT_ROWS`].
    MaxRows,
    /// The result reached [`MAX_RESULT_BYTES`].
    MaxBytes,
}

impl TruncationReason {
    pub fn can_load_more(self) -> bool {
        self == TruncationReason::RowLimit
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum QueryRunEvent {
    /// A new result set started; previously received rows belong to an earlier statement.
    ResultSet(Vec<ColumnMeta>),
    Rows(Vec<ResultRow>),
    /// The state or statistics changed.
    Updated,
}

enum RunControl {
    LoadMore,
}

/// Messages from the Tokio pump to the foreground.
pub(crate) enum PumpMessage {
    Event(ResultEvent),
    Paused(TruncationReason),
    Resumed,
    Failed(String),
    Finished,
}

/// One execution of SQL against a connection, with its streaming state.
pub struct QueryRun {
    pub connection: ConnectionKey,
    pub driver: DriverKind,
    pub sql: Arc<str>,
    pub state: QueryState,
    /// Columns of the current result set.
    pub columns: Vec<ColumnMeta>,
    /// Rows received for the current result set.
    pub row_count: usize,
    pub rows_affected: Option<u64>,
    pub statements_completed: usize,
    pub started_at: Instant,
    pub elapsed: Option<Duration>,
    control: Option<UnboundedSender<RunControl>>,
    cancel_handle: Option<Arc<dyn CancelHandle>>,
    task: Option<Task<()>>,
}

impl EventEmitter<QueryRunEvent> for QueryRun {}

impl QueryRun {
    pub(crate) fn new(connection: ConnectionKey, driver: DriverKind, sql: Arc<str>) -> Self {
        Self {
            connection,
            driver,
            sql,
            state: QueryState::Connecting,
            columns: Vec::new(),
            row_count: 0,
            rows_affected: None,
            statements_completed: 0,
            started_at: Instant::now(),
            elapsed: None,
            control: None,
            cancel_handle: None,
            task: None,
        }
    }

    pub(crate) fn set_task(&mut self, task: Task<()>) {
        self.task = Some(task);
    }

    pub(crate) fn fail(&mut self, message: String, cx: &mut Context<Self>) {
        self.finish(QueryState::Failed(message.into()), cx);
    }

    /// Time spent so far, or in total once finished.
    pub fn duration(&self) -> Duration {
        self.elapsed.unwrap_or_else(|| self.started_at.elapsed())
    }

    pub fn can_load_more(&self) -> bool {
        self.control.is_some()
            && matches!(&self.state, QueryState::Paused { reason } if reason.can_load_more())
    }

    pub fn load_more(&mut self, cx: &mut Context<Self>) {
        if !self.can_load_more() {
            return;
        }
        if let Some(control) = &self.control
            && control.unbounded_send(RunControl::LoadMore).is_ok()
        {
            self.state = QueryState::Running;
            cx.emit(QueryRunEvent::Updated);
            cx.notify();
        }
    }

    /// Stops the query on the client and asks the server to stop it. A paused query keeps the
    /// rows it already received.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, QueryState::Paused { .. }) {
            self.release_session(cx);
            return;
        }
        if !self.state.is_active() {
            return;
        }
        // Dropping the task drops the result stream, which sends the cancel request. Sending it
        // explicitly as well makes the server stop even if the stream was already done reading.
        self.task.take();
        self.control.take();
        if let Some(cancel_handle) = self.cancel_handle.take() {
            gpui_tokio::Tokio::spawn(cx, async move {
                if let Err(error) = cancel_handle.cancel().await {
                    log::warn!("failed to cancel database statement: {error:#}");
                }
            })
            .detach();
        }
        self.finish(QueryState::Cancelled, cx);
    }

    /// Releases a paused result so its session can run other statements.
    pub(crate) fn release_session(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, QueryState::Paused { .. }) {
            self.task.take();
            self.control.take();
            self.cancel_handle.take();
            cx.emit(QueryRunEvent::Updated);
            cx.notify();
        }
    }

    pub(crate) fn start_streaming(
        &mut self,
        session: Arc<dyn DatabaseSession>,
        options: ExecOptions,
        row_limit: usize,
        sanitize: Arc<dyn Fn(&str) -> String + Send + Sync>,
        cx: &mut Context<Self>,
    ) {
        let (control_sender, control_receiver) = unbounded();
        let (message_sender, mut message_receiver) = unbounded();
        self.control = Some(control_sender);
        self.cancel_handle = Some(session.cancel_handle());
        self.state = QueryState::Running;
        cx.emit(QueryRunEvent::Updated);
        cx.notify();

        let sql = self.sql.to_string();
        let pump = gpui_tokio::Tokio::spawn(
            cx,
            pump(
                session,
                sql,
                options,
                row_limit,
                control_receiver,
                message_sender,
                sanitize,
            ),
        );
        self.task = Some(cx.spawn(async move |this, cx| {
            let _pump = pump;
            while let Some(message) = message_receiver.next().await {
                if this.update(cx, |this, cx| this.apply(message, cx)).is_err() {
                    break;
                }
            }
        }));
    }

    fn apply(&mut self, message: PumpMessage, cx: &mut Context<Self>) {
        match message {
            PumpMessage::Event(ResultEvent::Columns(columns)) => {
                self.columns = columns.clone();
                self.row_count = 0;
                cx.emit(QueryRunEvent::ResultSet(columns));
            }
            PumpMessage::Event(ResultEvent::Rows(rows)) => {
                self.row_count += rows.len();
                cx.emit(QueryRunEvent::Rows(rows));
            }
            PumpMessage::Event(ResultEvent::StatementComplete { rows_affected }) => {
                self.statements_completed += 1;
                if rows_affected.is_some() {
                    self.rows_affected = rows_affected;
                }
                cx.emit(QueryRunEvent::Updated);
            }
            PumpMessage::Paused(reason) => {
                self.state = QueryState::Paused { reason };
                self.elapsed = Some(self.started_at.elapsed());
                cx.emit(QueryRunEvent::Updated);
            }
            PumpMessage::Resumed => {
                self.state = QueryState::Running;
                cx.emit(QueryRunEvent::Updated);
            }
            PumpMessage::Failed(message) => self.fail(message, cx),
            PumpMessage::Finished => {
                if !matches!(self.state, QueryState::Paused { .. }) {
                    self.finish(QueryState::Finished, cx);
                }
            }
        }
        cx.notify();
    }

    fn finish(&mut self, state: QueryState, cx: &mut Context<Self>) {
        let outcome = match &state {
            QueryState::Finished | QueryState::Paused { .. } => "ok",
            QueryState::Failed(_) => "error",
            QueryState::Cancelled => "cancelled",
            QueryState::Connecting | QueryState::Running => return,
        };
        if !matches!(self.state, QueryState::Paused { .. }) {
            self.elapsed = Some(self.started_at.elapsed());
        }
        self.state = state;
        self.control.take();
        self.cancel_handle.take();
        telemetry::event!(
            "Database Query Executed",
            driver = self.driver.id(),
            outcome
        );
        cx.emit(QueryRunEvent::Updated);
        cx.notify();
    }
}

/// Reads a result stream on the Tokio runtime, enforcing row and byte limits.
///
/// When a limit is reached it waits for [`RunControl::LoadMore`]. While it waits, the result
/// stream isn't polled, so the database stops sending rows. Returning drops the stream, which
/// cancels the statement if it is still running.
async fn pump(
    session: Arc<dyn DatabaseSession>,
    sql: String,
    options: ExecOptions,
    row_limit: usize,
    mut control: UnboundedReceiver<RunControl>,
    output: UnboundedSender<PumpMessage>,
    sanitize: Arc<dyn Fn(&str) -> String + Send + Sync>,
) {
    let send = |message| output.unbounded_send(message).is_ok();
    let row_limit = row_limit.clamp(1, MAX_RESULT_ROWS);
    let mut stream = session.execute(sql, options);
    let mut limit = row_limit;
    let mut rows_in_set = 0usize;
    let mut bytes = 0usize;
    let mut pending: VecDeque<ResultRow> = VecDeque::new();
    let mut stream_done = false;

    loop {
        if !pending.is_empty() && rows_in_set < limit {
            let take = (limit - rows_in_set).min(pending.len());
            let rows = pending.drain(..take).collect::<Vec<_>>();
            rows_in_set += rows.len();
            bytes += rows
                .iter()
                .flatten()
                .flatten()
                .map(|value| value.len())
                .sum::<usize>();
            if bytes >= MAX_RESULT_BYTES {
                limit = rows_in_set;
            }
            if !send(PumpMessage::Event(ResultEvent::Rows(rows))) {
                return;
            }
            continue;
        }

        if rows_in_set >= limit && pending.is_empty() && !stream_done {
            // Look one batch ahead, so that a result that ends exactly at the limit isn't
            // reported as truncated.
            match stream.next().await {
                Some(Ok(ResultEvent::Rows(rows))) => pending.extend(rows),
                Some(Ok(event)) => {
                    if !handle_event(event, &mut rows_in_set, &mut limit, row_limit, &send) {
                        return;
                    }
                    continue;
                }
                Some(Err(error)) => {
                    send(PumpMessage::Failed(sanitize(&format!("{error:#}"))));
                    return;
                }
                None => stream_done = true,
            }
            continue;
        }

        if rows_in_set >= limit && !pending.is_empty() {
            let reason = if bytes >= MAX_RESULT_BYTES {
                TruncationReason::MaxBytes
            } else if rows_in_set >= MAX_RESULT_ROWS {
                TruncationReason::MaxRows
            } else {
                TruncationReason::RowLimit
            };
            if !send(PumpMessage::Paused(reason)) {
                return;
            }
            if !reason.can_load_more() {
                send(PumpMessage::Finished);
                return;
            }
            match control.next().await {
                Some(RunControl::LoadMore) => {
                    limit = (limit + row_limit).min(MAX_RESULT_ROWS);
                    if !send(PumpMessage::Resumed) {
                        return;
                    }
                    continue;
                }
                None => return,
            }
        }

        if stream_done {
            send(PumpMessage::Finished);
            return;
        }

        match stream.next().await {
            Some(Ok(ResultEvent::Rows(rows))) => pending.extend(rows),
            Some(Ok(event)) => {
                if !handle_event(event, &mut rows_in_set, &mut limit, row_limit, &send) {
                    return;
                }
            }
            Some(Err(error)) => {
                send(PumpMessage::Failed(sanitize(&format!("{error:#}"))));
                return;
            }
            None => stream_done = true,
        }
    }
}

fn handle_event(
    event: ResultEvent,
    rows_in_set: &mut usize,
    limit: &mut usize,
    row_limit: usize,
    send: &impl Fn(PumpMessage) -> bool,
) -> bool {
    if let ResultEvent::Columns(_) = &event {
        // Each result set gets its own row limit.
        *rows_in_set = 0;
        *limit = row_limit;
    }
    send(PumpMessage::Event(event))
}
