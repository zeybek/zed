use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use db::kvp::KeyValueStore;
use gpui::{App, AppContext as _, Task};
use serde::{Deserialize, Serialize};

use crate::connection::ConnectionKey;

const KVP_PREFIX: &str = "database_query_history:";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub sql: String,
    /// Seconds since the Unix epoch.
    pub executed_at: i64,
}

/// Recently executed queries per connection, most recent first.
#[derive(Default)]
pub struct QueryHistory {
    entries: HashMap<ConnectionKey, VecDeque<HistoryEntry>>,
    loaded: HashSet<ConnectionKey>,
}

impl QueryHistory {
    pub fn entries(&self, key: &ConnectionKey) -> impl Iterator<Item = &HistoryEntry> {
        self.entries.get(key).into_iter().flatten()
    }

    pub fn is_loaded(&self, key: &ConnectionKey) -> bool {
        self.loaded.contains(key)
    }

    /// Reads the persisted history of a connection.
    pub fn load(key: &ConnectionKey, cx: &App) -> Task<Result<Vec<HistoryEntry>>> {
        let kvp = KeyValueStore::global(cx);
        let storage_key = storage_key(key);
        cx.background_spawn(async move {
            match kvp.read_kvp(&storage_key)? {
                Some(json) => Ok(serde_json::from_str(&json)?),
                None => Ok(Vec::new()),
            }
        })
    }

    /// Merges loaded entries with any recorded before loading finished.
    pub fn set_loaded(&mut self, key: ConnectionKey, loaded: Vec<HistoryEntry>, limit: usize) {
        let entries = self.entries.entry(key.clone()).or_default();
        for entry in loaded {
            if !entries.iter().any(|existing| existing.sql == entry.sql) {
                entries.push_back(entry);
            }
        }
        entries.truncate(limit);
        self.loaded.insert(key);
    }

    /// Records an executed query and returns a task that persists the connection's history.
    pub fn record(
        &mut self,
        key: &ConnectionKey,
        sql: &str,
        executed_at: i64,
        limit: usize,
        cx: &App,
    ) -> Task<Result<()>> {
        let sql = sql.trim();
        let entries = self.entries.entry(key.clone()).or_default();
        if sql.is_empty() || limit == 0 {
            entries.clear();
        } else {
            entries.retain(|entry| entry.sql != sql);
            entries.push_front(HistoryEntry {
                sql: sql.to_string(),
                executed_at,
            });
            entries.truncate(limit);
        }
        self.persist(key, cx)
    }

    pub fn clear(&mut self, key: &ConnectionKey, cx: &App) -> Task<Result<()>> {
        self.entries.remove(key);
        let kvp = KeyValueStore::global(cx);
        let storage_key = storage_key(key);
        cx.background_spawn(async move { kvp.delete_kvp(storage_key).await })
    }

    fn persist(&self, key: &ConnectionKey, cx: &App) -> Task<Result<()>> {
        // Entries recorded before the persisted history was read would overwrite it.
        if !self.loaded.contains(key) {
            return Task::ready(Ok(()));
        }
        let entries = self
            .entries
            .get(key)
            .map(|entries| entries.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let kvp = KeyValueStore::global(cx);
        let storage_key = storage_key(key);
        cx.background_spawn(async move {
            kvp.write_kvp(storage_key, serde_json::to_string(&entries)?)
                .await
        })
    }
}

fn storage_key(key: &ConnectionKey) -> String {
    format!("{KVP_PREFIX}{}", key.persistence_key())
}
