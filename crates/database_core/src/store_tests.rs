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
use gpui::{AsyncApp, BorrowAppContext as _, Entity, TestAppContext};
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

#[gpui::test]
async fn test_mcp_tools_for_agents(cx: &mut TestAppContext) {
    use std::io::{BufRead as _, BufReader, Write as _};

    use project::context_server_store::registry::ContextServerDescriptorRegistry;
    use project::{FakeFs, Project};

    init_test(cx);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("numbers.sqlite3");
    create_database(&path, 300);
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                let panel = settings.database_panel.get_or_insert_default();
                panel.enabled = Some(true);
                panel.agent_access = Some(true);
                let content = serde_json::from_value(serde_json::json!({
                    "driver": "sqlite",
                    "path": path.to_string_lossy(),
                }))
                .unwrap();
                settings
                    .project
                    .database_connections
                    .get_or_insert_default()
                    .insert("fixtures".into(), content);
            });
        });
    });
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project", serde_json::json!({})).await;
    let project = Project::test(fs, ["/project".as_ref()], cx).await;

    let descriptor = cx.update(|cx| {
        ContextServerDescriptorRegistry::default_global(cx)
            .read(cx)
            .context_server_descriptor(crate::mcp::CONTEXT_SERVER_ID)
    });
    let descriptor = descriptor.expect("the database context server is registered");
    let worktree_store = project.read_with(cx, |project, _| project.worktree_store());
    let command = wait_for(
        cx,
        cx.update(|cx| descriptor.command(worktree_store, &cx.to_async())),
    )
    .unwrap();
    assert_eq!(command.args[0], crate::mcp::BRIDGE_FLAG);
    let socket = command.args[1].clone();

    // Talk to the server the way the bridge does, from another thread, while the test pumps the
    // foreground executor that serves the requests.
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let mut request = |id: u32, method: &str, params: serde_json::Value| {
            let message = serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
            writeln!(writer, "{message}").unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            serde_json::from_str::<serde_json::Value>(&line).unwrap()
        };
        let responses = vec![
            request(
                1,
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1" }
                }),
            ),
            request(2, "tools/list", serde_json::json!({})),
            request(
                3,
                "tools/call",
                serde_json::json!({
                    "name": "db_list_connections", "arguments": {}
                }),
            ),
            request(
                4,
                "tools/call",
                serde_json::json!({
                    "name": "db_query",
                    "arguments": { "connection": "fixtures", "sql": "SELECT n FROM numbers ORDER BY n" }
                }),
            ),
            request(
                5,
                "tools/call",
                serde_json::json!({
                    "name": "db_query",
                    "arguments": { "connection": "fixtures", "sql": "DELETE FROM numbers" }
                }),
            ),
            request(
                6,
                "tools/call",
                serde_json::json!({
                    "name": "db_schema",
                    "arguments": { "connection": "fixtures", "schema": "main" }
                }),
            ),
            request(
                7,
                "prompts/get",
                serde_json::json!({
                    "name": "table",
                    "arguments": { "connection": "fixtures", "table": "numbers" }
                }),
            ),
        ];
        sender.send(responses).unwrap();
    });
    let mut responses = None;
    for _ in 0..2000 {
        cx.run_until_parked();
        if let Ok(received) = receiver.try_recv() {
            responses = Some(received);
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let responses = responses.expect("the MCP server answered");

    assert_eq!(responses[0]["result"]["serverInfo"]["name"], "zed-database");
    let tools = responses[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| {
            assert_eq!(tool["annotations"]["readOnlyHint"], true);
            tool["name"].as_str().unwrap().to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(tools.len(), 3);
    for name in ["db_list_connections", "db_query", "db_schema"] {
        assert!(tools.iter().any(|tool| tool == name), "missing {name}");
    }
    let listing = responses[2]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(listing.contains("fixtures (SQLite, local)"), "{listing}");

    let query = &responses[3]["result"];
    assert_eq!(
        query["structuredContent"]["rows"].as_array().unwrap().len(),
        200
    );
    assert_eq!(query["structuredContent"]["truncated"], true);

    let write = responses[4].to_string();
    assert!(write.contains("must not modify"), "{write}");
    let schema = responses[5]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert_eq!(schema, "numbers");
    let prompt = responses[6]["result"]["messages"][0]["content"]["text"]
        .as_str()
        .unwrap();
    assert!(prompt.contains("CREATE TABLE numbers"), "{prompt}");

    // Disabling agent access unregisters the server.
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.database_panel.get_or_insert_default().agent_access = Some(false);
            });
        });
    });
    cx.run_until_parked();
    assert!(cx.update(|cx| {
        ContextServerDescriptorRegistry::default_global(cx)
            .read(cx)
            .context_server_descriptor(crate::mcp::CONTEXT_SERVER_ID)
            .is_none()
    }));
}

#[gpui::test]
fn test_edit_transactions(cx: &mut TestAppContext) {
    init_test(cx);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("numbers.sqlite3");
    create_database(&path, 5);
    let config = sqlite_config("edits", &path);
    let store = cx.update(|cx| DbStore::global(cx));
    let update = |n: &str, value: &str| {
        crate::statement::update_statement(
            crate::DriverKind::Sqlite,
            "main",
            "numbers",
            &[("n", Some(value))],
            &[("n", Some(n))],
        )
    };

    let task = store.update(cx, |store, cx| {
        store.execute_transaction(
            config.clone(),
            None,
            vec![update("1", "10"), update("2", "20")],
            cx,
        )
    });
    wait_for(cx, task).unwrap();

    // The second statement matches no row, so the first one is rolled back too.
    let task = store.update(cx, |store, cx| {
        store.execute_transaction(
            config.clone(),
            None,
            vec![update("3", "30"), update("99", "1")],
            cx,
        )
    });
    let error = wait_for(cx, task).unwrap_err();
    assert!(error.to_string().contains("changed 0"), "{error}");

    let run = store.update(cx, |store, cx| {
        store.execute(
            config.clone(),
            None,
            "SELECT n FROM numbers ORDER BY n".into(),
            QuerySource::Editor,
            cx,
        )
    });
    let rows = received_rows(&run, cx);
    wait_until(cx, |cx| {
        run.read_with(cx, |run, _| run.state == QueryState::Finished)
    });
    assert_eq!(*rows.lock().unwrap(), 5);
    let values = run.read_with(cx, |run, _| run.row_count);
    assert_eq!(values, 5);
    let check = store.update(cx, |store, cx| {
        store.execute_for_agent(
            config.clone(),
            None,
            "SELECT group_concat(n, ',') FROM (SELECT n FROM numbers ORDER BY n)".into(),
            10,
            1000,
            cx,
        )
    });
    let result = wait_for(cx, check).unwrap();
    assert_eq!(result.rows[0][0].as_deref(), Some("3,4,5,10,20"));
}
