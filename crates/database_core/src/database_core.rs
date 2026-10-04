//! Database connections, drivers and query execution for the database panel.
//!
//! Nothing here depends on UI code, so that the panel, editor actions and the agent tools can all
//! share connections through [`DbStore`].

mod connection;
mod database_settings;
mod driver;
pub mod export;
mod history;
pub mod mcp;
mod mysql;
mod postgres;
mod query;
mod sqlite;
mod ssh_tunnel;
pub mod statement;
mod store;
mod tls;

use std::sync::Arc;

use credentials_provider::CredentialsProvider;
use gpui::App;

pub use connection::{
    ConnectionConfig, ConnectionKey, DriverKind, SshTunnelConfig, expand_variables,
};
pub use database_settings::{DatabaseSettings, MAX_RESULT_ROWS};
pub use driver::{
    ColumnInfo, ColumnMeta, ForeignKeyInfo, IndexInfo, KeyInfo, ObjectRef, RelationDetails,
    RelationInfo, RelationKind, ResultRow, RoutineInfo, RoutineKind, SchemaInfo, SchemaObjects,
    TriggerInfo, ValueKind, qualified_name, quote_identifier,
};
pub use history::HistoryEntry;
pub use query::{MAX_RESULT_BYTES, QueryRun, QueryRunEvent, QueryState, TruncationReason};
pub use settings::{DatabaseEnvironment, DatabaseSslMode};
pub use store::{
    CollectedResult, ConnectionStatus, DbStore, DbStoreEvent, PasswordInput, PasswordRequired,
    QuerySource, SchemaRequest, Sessions,
};

pub fn init(cx: &mut App) {
    let credentials_provider = zed_credentials_provider::global(cx);
    init_with_credentials_provider(credentials_provider, cx);
}

pub fn init_with_credentials_provider(
    credentials_provider: Arc<dyn CredentialsProvider>,
    cx: &mut App,
) {
    DbStore::init_global(credentials_provider, cx);
    mcp::init(cx);
}

#[cfg(test)]
mod driver_tests;
#[cfg(test)]
mod store_tests;
