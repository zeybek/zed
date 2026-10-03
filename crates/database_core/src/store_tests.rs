use std::{
    collections::HashMap,
    future::Future,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;
use credentials_provider::CredentialsProvider;
use gpui::{AsyncApp, Entity, TestAppContext};
use settings::{DatabaseConnectionContent, SettingsStore};

use crate::{
    ConnectionConfig, ConnectionKey, ConnectionStatus, DbStore, PasswordInput, PasswordRequired,
    QueryRun, QueryRunEvent, QuerySource, QueryState, TruncationReason,
};

#[derive(Default)]
struct FakeCredentials(Mutex<HashMap<String, (String, Vec<u8>)>>);

impl CredentialsProvider for FakeCredentials {
    fn read_credentials<'a>(
        &'a self,
        url: &'a str,
        _: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>>> + 'a>> {
        let value = self.0.lock().unwrap().get(url).cloned();
        Box::pin(async move { Ok(value) })
    }

    fn write_credentials<'a>(
        &'a self,
        url: &'a str,
        username: &'a str,
        password: &'a [u8],
        _: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        self.0
            .lock()
            .unwrap()
            .insert(url.to_string(), (username.to_string(), password.to_vec()));
        Box::pin(async { Ok(()) })
    }

    fn delete_credentials<'a>(
        &'a self,
        url: &'a str,
        _: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        self.0.lock().unwrap().remove(url);
        Box::pin(async { Ok(()) })
    }
}

fn init_test(cx: &mut TestAppContext) -> Arc<FakeCredentials> {
    cx.executor().allow_parking();
    let credentials = Arc::new(FakeCredentials::default());
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
        gpui_tokio::init(cx);
        crate::init_with_credentials_provider(credentials.clone(), cx);
    });
    credentials
}

/// Waits for work on the Tokio runtime, which the test executor can't see.
#[track_caller]
fn wait_until(cx: &mut TestAppContext, mut condition: impl FnMut(&mut TestAppContext) -> bool) {
    for _ in 0..2000 {
        cx.run_until_parked();
        if condition(cx) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!(
        "timed out waiting for a condition at {}",
        std::panic::Location::caller()
    );
}

#[track_caller]
fn wait_for<T: 'static>(cx: &mut TestAppContext, task: gpui::Task<T>) -> T {
    let output = Arc::new(Mutex::new(None));
    cx.update(|cx| {
        let output = output.clone();
        cx.spawn(async move |_| {
            *output.lock().unwrap() = Some(task.await);
        })
        .detach();
    });
    wait_until(cx, |_| output.lock().unwrap().is_some());
    output.lock().unwrap().take().unwrap()
}

fn sqlite_config(id: &str, path: &Path) -> ConnectionConfig {
    let content: DatabaseConnectionContent = serde_json::from_value(serde_json::json!({
        "driver": "sqlite",
        "path": path.to_string_lossy(),
    }))
    .unwrap();
    ConnectionConfig::from_content(ConnectionKey::user(id), &content)
}

fn create_database(path: &Path, rows: usize) {
    let connection = rusqlite::Connection::open(path).unwrap();
    connection
        .execute_batch(&format!(
            "CREATE TABLE numbers (n INTEGER);
             WITH RECURSIVE series(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM series WHERE n < {rows})
             INSERT INTO numbers SELECT n FROM series;"
        ))
        .unwrap();
}

fn received_rows(run: &Entity<QueryRun>, cx: &mut TestAppContext) -> Arc<Mutex<usize>> {
    let received = Arc::new(Mutex::new(0));
    cx.update(|cx| {
        let received = received.clone();
        cx.subscribe(run, move |_, event: &QueryRunEvent, _| {
            if let QueryRunEvent::Rows(rows) = event {
                *received.lock().unwrap() += rows.len();
            }
        })
        .detach();
    });
    received
}

#[gpui::test]
fn test_query_pauses_at_the_row_limit(cx: &mut TestAppContext) {
    init_test(cx);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("numbers.sqlite3");
    create_database(&path, 2_500);
    let config = sqlite_config("row_limit", &path);

    let store = cx.update(|cx| DbStore::global(cx));
    let run = store.update(cx, |store, cx| {
        store.execute(
            config.clone(),
            None,
            "SELECT n FROM numbers ORDER BY n".into(),
            QuerySource::Editor,
            cx,
        )
    });
    let received = received_rows(&run, cx);

    wait_until(cx, |cx| {
        matches!(
            run.read_with(cx, |run, _| run.state.clone()),
            QueryState::Paused { .. }
        )
    });
    run.read_with(cx, |run, _| {
        assert_eq!(
            run.state,
            QueryState::Paused {
                reason: TruncationReason::RowLimit
            }
        );
        assert_eq!(run.row_count, 1_000);
        assert!(run.can_load_more());
    });
    assert_eq!(*received.lock().unwrap(), 1_000);
    assert!(store.read_with(cx, |store, _| store.is_connected(&config.key)));

    run.update(cx, |run, cx| run.load_more(cx));
    wait_until(cx, |cx| {
        run.read_with(cx, |run, _| {
            run.row_count == 2_000 && !run.state.is_active()
        })
    });
    run.update(cx, |run, cx| run.load_more(cx));
    wait_until(cx, |cx| {
        run.read_with(cx, |run, _| run.state == QueryState::Finished)
    });
    run.read_with(cx, |run, _| {
        assert_eq!(run.row_count, 2_500);
        assert!(!run.can_load_more());
        assert!(run.elapsed.is_some());
    });
    assert_eq!(*received.lock().unwrap(), 2_500);

    // A result that ends exactly at the limit isn't reported as truncated.
    let exact = store.update(cx, |store, cx| {
        store.execute(
            config.clone(),
            None,
            "SELECT n FROM numbers WHERE n <= 1000".into(),
            QuerySource::Editor,
            cx,
        )
    });
    wait_until(cx, |cx| {
        exact.read_with(cx, |run, _| !run.state.is_active())
    });
    exact.read_with(cx, |run, _| {
        assert_eq!(run.state, QueryState::Finished);
        assert_eq!(run.row_count, 1_000);
    });

    let history = store.read_with(cx, |store, _| {
        store
            .history(&config.key)
            .map(|entry| entry.sql.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        history,
        vec![
            "SELECT n FROM numbers WHERE n <= 1000".to_string(),
            "SELECT n FROM numbers ORDER BY n".to_string()
        ]
    );
}

#[gpui::test]
fn test_new_query_releases_a_paused_result(cx: &mut TestAppContext) {
    init_test(cx);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("numbers.sqlite3");
    create_database(&path, 1_500);
    let config = sqlite_config("release", &path);
    let store = cx.update(|cx| DbStore::global(cx));

    let paused = store.update(cx, |store, cx| {
        store.execute(
            config.clone(),
            None,
            "SELECT n FROM numbers".into(),
            QuerySource::Editor,
            cx,
        )
    });
    wait_until(cx, |cx| {
        matches!(
            paused.read_with(cx, |run, _| run.state.clone()),
            QueryState::Paused { .. }
        )
    });

    // The paused statement holds the session; the next query must not wait behind it.
    let next = store.update(cx, |store, cx| {
        store.execute(
            config.clone(),
            None,
            "SELECT count(*) FROM numbers".into(),
            QuerySource::Editor,
            cx,
        )
    });
    wait_until(cx, |cx| {
        next.read_with(cx, |run, _| run.state == QueryState::Finished)
    });
    paused.read_with(cx, |run, _| {
        assert!(matches!(run.state, QueryState::Paused { .. }));
        assert!(!run.can_load_more(), "the released result can't load more");
        assert_eq!(run.row_count, 1_000);
    });
}

#[gpui::test]
fn test_cancel_and_errors(cx: &mut TestAppContext) {
    init_test(cx);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("numbers.sqlite3");
    create_database(&path, 10);
    let config = sqlite_config("cancel", &path);
    let store = cx.update(|cx| DbStore::global(cx));

    let endless = store.update(cx, |store, cx| {
        store.execute(
            config.clone(),
            None,
            "WITH RECURSIVE forever(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM forever) SELECT count(*) FROM forever".into(),
            QuerySource::Editor,
            cx,
        )
    });
    wait_until(cx, |cx| {
        endless.read_with(cx, |run, _| run.state == QueryState::Running)
    });
    endless.update(cx, |run, cx| run.cancel(cx));
    endless.read_with(cx, |run, _| assert_eq!(run.state, QueryState::Cancelled));

    // The interrupted session keeps working.
    let next = store.update(cx, |store, cx| {
        store.execute(
            config.clone(),
            None,
            "SELECT 1".into(),
            QuerySource::Editor,
            cx,
        )
    });
    wait_until(cx, |cx| next.read_with(cx, |run, _| !run.state.is_active()));
    next.read_with(cx, |run, _| assert_eq!(run.state, QueryState::Finished));

    let failing = store.update(cx, |store, cx| {
        store.execute(
            config.clone(),
            None,
            "SELECT * FROM missing".into(),
            QuerySource::Editor,
            cx,
        )
    });
    wait_until(cx, |cx| {
        failing.read_with(cx, |run, _| !run.state.is_active())
    });
    failing.read_with(cx, |run, _| {
        let QueryState::Failed(message) = &run.state else {
            panic!("expected a failure, got {:?}", run.state);
        };
        assert!(message.contains("no such table"), "{message}");
    });

    let missing_file = sqlite_config("missing", &directory.path().join("missing.sqlite3"));
    let task = store.update(cx, |store, cx| {
        store.connect(missing_file.clone(), None, None, cx)
    });
    wait_until(cx, |cx| {
        store.read_with(cx, |store, _| {
            matches!(store.status(&missing_file.key), ConnectionStatus::Failed(_))
        })
    });
    drop(task);
    assert!(!directory.path().join("missing.sqlite3").exists());
}

#[gpui::test]
fn test_postgres_password_flow(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("ZED_DATABASE_TEST_POSTGRES_URL") else {
        return;
    };
    let credentials = init_test(cx);
    let parsed = url::Url::parse(&url).unwrap();
    let password = parsed.password().unwrap().to_string();
    let mut without_password = parsed;
    without_password.set_password(None).unwrap();
    let content: DatabaseConnectionContent = serde_json::from_value(serde_json::json!({
        "driver": "postgres",
        "url": without_password.to_string(),
    }))
    .unwrap();
    let config = ConnectionConfig::from_content(ConnectionKey::user("password_flow"), &content);
    let store = cx.update(|cx| DbStore::global(cx));

    let connect = store.update(cx, |store, cx| {
        store.connect(config.clone(), None, None, cx)
    });
    wait_until(cx, |cx| {
        store.read_with(cx, |store, _| {
            store.status(&config.key) == ConnectionStatus::PasswordRequired
        })
    });
    drop(connect);

    let connect = store.update(cx, |store, cx| {
        store.connect(
            config.clone(),
            None,
            Some(PasswordInput {
                password: password.clone(),
                remember: true,
            }),
            cx,
        )
    });
    wait_until(cx, |cx| {
        store.read_with(cx, |store, _| store.is_connected(&config.key))
    });
    drop(connect);
    assert_eq!(
        credentials
            .0
            .lock()
            .unwrap()
            .get(&config.key.credentials_key())
            .map(|(_, stored)| stored.clone()),
        Some(password.into_bytes())
    );

    // After disconnecting, the stored password is used without asking again.
    store.update(cx, |store, cx| store.disconnect(&config.key, cx));
    let connect = store.update(cx, |store, cx| {
        store.connect(config.clone(), None, None, cx)
    });
    wait_until(cx, |cx| {
        store.read_with(cx, |store, _| store.is_connected(&config.key))
    });
    drop(connect);

    let load = store.update(cx, |store, cx| store.load_schemas(config.clone(), None, cx));
    wait_until(cx, |cx| {
        store.read_with(cx, |store, _| store.schemas(&config.key).is_some())
    });
    drop(load);

    let result = store.update(cx, |store, cx| {
        store.execute_for_agent(
            config.clone(),
            None,
            "SELECT generate_series(1, 500) AS n".into(),
            200,
            20_000,
            cx,
        )
    });
    let result = wait_for(cx, result).unwrap();
    assert_eq!(result.columns, vec!["n".to_string()]);
    assert_eq!(result.rows.len(), 200);
    assert!(result.truncated);

    let forget = store.update(cx, |store, cx| store.forget(&config.key, cx));
    wait_for(cx, forget).unwrap();
    assert!(credentials.0.lock().unwrap().is_empty());
    let _ = PasswordRequired { message: None };
}
