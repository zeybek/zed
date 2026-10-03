//! Tests against real database servers. They run only when the server's URL is set:
//!
//! - `ZED_DATABASE_TEST_POSTGRES_URL`, e.g. `postgres://zed:zedtest@127.0.0.1:55432/zed_test`
//! - `ZED_DATABASE_TEST_MYSQL_URL`, e.g. `mysql://zed:zedtest@127.0.0.1:53306/zed_test`

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::StreamExt as _;
use settings::{DatabaseConnectionContent, DatabaseSslMode};

use crate::{
    connection::{ConnectionConfig, ConnectionKey, ResolvedConnection, SessionOptions},
    driver::{DatabaseSession, ExecOptions, RelationKind, ResultEvent, ResultStream, ValueKind},
    mysql, postgres,
};

fn resolved(
    variable: &str,
    driver: &str,
    configure: impl FnOnce(&mut ConnectionConfig),
) -> Option<ResolvedConnection> {
    let url = std::env::var(variable).ok()?;
    let content: DatabaseConnectionContent =
        serde_json::from_value(serde_json::json!({ "driver": driver, "url": url })).ok()?;
    let mut config = ConnectionConfig::from_content(ConnectionKey::user("test"), &content);
    configure(&mut config);
    Some(
        config
            .resolve(&HashMap::default(), None, None, &SessionOptions::default())
            .expect("valid test URL"),
    )
}

async fn connect(resolved: &ResolvedConnection) -> Arc<dyn DatabaseSession> {
    match resolved.driver {
        crate::DriverKind::Postgres => Arc::new(
            postgres::connect(resolved, &resolved.host, resolved.port)
                .await
                .expect("connecting to PostgreSQL"),
        ),
        crate::DriverKind::Mysql => Arc::new(
            mysql::connect(resolved, &resolved.host, resolved.port)
                .await
                .expect("connecting to MySQL"),
        ),
        crate::DriverKind::Sqlite => unreachable!(),
    }
}

async fn run(session: &dyn DatabaseSession, sql: &str) -> Vec<ResultEvent> {
    collect(session.execute(sql.to_string(), ExecOptions::default()))
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error:#}"))
}

async fn collect(stream: ResultStream) -> anyhow::Result<Vec<ResultEvent>> {
    let mut events = Vec::new();
    futures::pin_mut!(stream);
    while let Some(event) = stream.next().await {
        events.push(event?);
    }
    Ok(events)
}

async fn error_of(session: &dyn DatabaseSession, sql: &str, options: ExecOptions) -> String {
    match collect(session.execute(sql.to_string(), options)).await {
        Ok(events) => panic!("expected {sql} to fail, got {events:?}"),
        Err(error) => format!("{error:#}"),
    }
}

fn row_count(events: &[ResultEvent]) -> usize {
    events
        .iter()
        .map(|event| match event {
            ResultEvent::Rows(rows) => rows.len(),
            _ => 0,
        })
        .sum()
}

fn unique_name(prefix: &str) -> String {
    format!(
        "{prefix}_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn test_postgres_session() {
    let Some(connection) = resolved("ZED_DATABASE_TEST_POSTGRES_URL", "postgres", |_| {}) else {
        eprintln!("ZED_DATABASE_TEST_POSTGRES_URL is not set, skipping");
        return;
    };
    let session = connect(&connection).await;
    let schema = unique_name("zed_test");
    run(
        session.as_ref(),
        &format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.orders (
                 id bigint PRIMARY KEY,
                 total numeric(10, 2) NOT NULL DEFAULT 0,
                 paid boolean,
                 note text,
                 created_at timestamptz DEFAULT now(),
                 payload bytea
             );
             CREATE VIEW {schema}.paid_orders AS SELECT id FROM {schema}.orders WHERE paid;
             INSERT INTO {schema}.orders (id, total, paid, note, payload)
                 SELECT n, n * 1.5, n % 2 = 0, CASE WHEN n % 3 = 0 THEN NULL ELSE 'note ' || n END, '\\x0102'
                 FROM generate_series(1, 1200) AS n;"
        ),
    )
    .await;

    let schemas = session.list_schemas().await.unwrap();
    assert!(schemas.iter().any(|info| info.name.as_ref() == schema));
    assert_eq!(schemas[0].name.as_ref(), "public", "public is listed first");

    let relations = session.list_relations(&schema).await.unwrap();
    assert_eq!(
        relations
            .iter()
            .map(|relation| (relation.name.as_ref(), relation.kind))
            .collect::<Vec<_>>(),
        vec![
            ("orders", RelationKind::Table),
            ("paid_orders", RelationKind::View)
        ]
    );
    let columns = session.list_columns(&schema, "orders").await.unwrap();
    assert_eq!(columns.len(), 6);
    assert!(columns[0].primary_key && !columns[0].nullable);
    assert_eq!(columns[1].data_type.as_ref(), "numeric(10,2)");

    let ddl = session.relation_ddl(&schema, "orders").await.unwrap();
    assert!(
        ddl.contains("total numeric(10,2) DEFAULT 0 NOT NULL"),
        "{ddl}"
    );
    assert!(ddl.contains("PRIMARY KEY (id)"), "{ddl}");
    let view_ddl = session.relation_ddl(&schema, "paid_orders").await.unwrap();
    assert!(
        view_ddl.starts_with(&format!("CREATE VIEW {schema}.paid_orders AS")),
        "{view_ddl}"
    );

    let events = run(
        session.as_ref(),
        &format!(
            "SELECT id, total, paid, note, created_at, payload FROM {schema}.orders ORDER BY id"
        ),
    )
    .await;
    let ResultEvent::Columns(metas) = &events[0] else {
        panic!("expected columns first, got {:?}", events.first());
    };
    assert_eq!(
        metas.iter().map(|meta| meta.kind).collect::<Vec<_>>(),
        vec![
            ValueKind::Number,
            ValueKind::Number,
            ValueKind::Boolean,
            ValueKind::Text,
            ValueKind::DateTime,
            ValueKind::Binary
        ]
    );
    assert_eq!(row_count(&events), 1200);
    let ResultEvent::Rows(first_batch) = &events[1] else {
        panic!("expected rows, got {:?}", events[1]);
    };
    assert_eq!(first_batch.len(), crate::driver::ROW_BATCH_SIZE);
    assert_eq!(
        first_batch[2][3], None,
        "NULL stays distinct from empty text"
    );
    assert_eq!(first_batch[0][5].as_deref(), Some("<blob 2 bytes>"));

    // Read-only agent queries can't write, even when the statement tries to end the transaction.
    let agent = ExecOptions {
        read_only_transaction: true,
    };
    let error = error_of(
        session.as_ref(),
        &format!("DELETE FROM {schema}.orders"),
        agent,
    )
    .await;
    assert!(error.contains("read-only transaction"), "{error}");
    let error = error_of(
        session.as_ref(),
        &format!("COMMIT; DELETE FROM {schema}.orders"),
        agent,
    )
    .await;
    assert!(error.contains("single statement"), "{error}");
    let error = error_of(session.as_ref(), &format!("DROP TABLE {schema}.orders"), agent).await;
    assert!(error.contains("read-only transaction"), "{error}");
    let events = collect(session.execute(format!("SELECT count(*) FROM {schema}.orders"), agent))
        .await
        .unwrap();
    assert!(matches!(&events[1], ResultEvent::Rows(rows) if rows[0][0].as_deref() == Some("1200")));

    // Read-only connections are enforced by the server.
    let read_only = resolved("ZED_DATABASE_TEST_POSTGRES_URL", "postgres", |config| {
        config.read_only = true
    })
    .unwrap();
    let read_only_session = connect(&read_only).await;
    let error = error_of(
        read_only_session.as_ref(),
        &format!("UPDATE {schema}.orders SET paid = true"),
        ExecOptions::default(),
    )
    .await;
    assert!(error.contains("read-only transaction"), "{error}");

    // Dropping the stream cancels the statement on the server within a second.
    let stream = session.execute("SELECT pg_sleep(30)".into(), ExecOptions::default());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let cancelled_at = Instant::now();
    drop(stream);
    loop {
        let events = run(
            read_only_session.as_ref(),
            "SELECT count(*) FROM pg_stat_activity WHERE query = 'SELECT pg_sleep(30)' AND state = 'active'",
        )
        .await;
        if matches!(&events[1], ResultEvent::Rows(rows) if rows[0][0].as_deref() == Some("0")) {
            break;
        }
        assert!(
            cancelled_at.elapsed() < Duration::from_secs(1),
            "the statement kept running after the stream was dropped"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The session is usable again afterwards.
    assert_eq!(row_count(&run(session.as_ref(), "SELECT 1").await), 1);

    run(session.as_ref(), &format!("DROP SCHEMA {schema} CASCADE")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_postgres_statement_timeout_and_errors() {
    let Some(mut connection) = resolved("ZED_DATABASE_TEST_POSTGRES_URL", "postgres", |_| {})
    else {
        return;
    };
    connection.statement_timeout = Some(Duration::from_millis(200));
    let session = connect(&connection).await;
    let error = error_of(
        session.as_ref(),
        "SELECT pg_sleep(5)",
        ExecOptions::default(),
    )
    .await;
    assert!(error.contains("statement timeout"), "{error}");

    let mut wrong_password = connection.clone();
    wrong_password.password = Some("definitely wrong".into());
    let error =
        match postgres::connect(&wrong_password, &wrong_password.host, wrong_password.port).await {
            Ok(_) => panic!("connected with a wrong password"),
            Err(error) => error,
        };
    assert!(postgres::is_authentication_error(&error), "{error:#}");
    assert!(
        !wrong_password
            .sanitize_message(&format!("{error:#}"))
            .contains("definitely wrong")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_mysql_session() {
    let Some(connection) = resolved("ZED_DATABASE_TEST_MYSQL_URL", "mysql", |_| {}) else {
        eprintln!("ZED_DATABASE_TEST_MYSQL_URL is not set, skipping");
        return;
    };
    let session = connect(&connection).await;
    let table = unique_name("orders");
    let database = connection.database.clone().unwrap_or_default();
    run(
        session.as_ref(),
        &format!(
            "CREATE TABLE {table} (
                 id BIGINT PRIMARY KEY,
                 total DECIMAL(10, 2) NOT NULL,
                 note VARCHAR(100),
                 created_at DATETIME,
                 payload VARBINARY(10)
             )"
        ),
    )
    .await;
    let mut values = Vec::new();
    for id in 1..=1200 {
        let note = if id % 3 == 0 {
            "NULL".to_string()
        } else {
            format!("'note {id}'")
        };
        values.push(format!(
            "({id}, {}, {note}, '2026-10-03 12:00:00', x'0102')",
            id as f64 * 1.5
        ));
    }
    let events = run(
        session.as_ref(),
        &format!("INSERT INTO {table} VALUES {}", values.join(", ")),
    )
    .await;
    assert_eq!(
        events.last(),
        Some(&ResultEvent::StatementComplete {
            rows_affected: Some(1200)
        })
    );

    let schemas = session.list_schemas().await.unwrap();
    assert_eq!(
        schemas[0].name.as_ref(),
        database,
        "the current database is listed first"
    );
    let relations = session.list_relations(&database).await.unwrap();
    assert!(
        relations
            .iter()
            .any(|relation| relation.name.as_ref() == table)
    );
    let columns = session.list_columns(&database, &table).await.unwrap();
    assert!(columns[0].primary_key);
    assert_eq!(columns[1].data_type.as_ref(), "decimal(10,2)");
    let ddl = session.relation_ddl(&database, &table).await.unwrap();
    assert!(ddl.starts_with("CREATE TABLE"), "{ddl}");

    let events = run(
        session.as_ref(),
        &format!("SELECT id, total, note, created_at, payload FROM {table} ORDER BY id"),
    )
    .await;
    let ResultEvent::Columns(metas) = &events[0] else {
        panic!("expected columns first, got {:?}", events.first());
    };
    assert_eq!(
        metas.iter().map(|meta| meta.kind).collect::<Vec<_>>(),
        vec![
            ValueKind::Number,
            ValueKind::Number,
            ValueKind::Text,
            ValueKind::DateTime,
            ValueKind::Binary
        ]
    );
    assert_eq!(row_count(&events), 1200);
    let ResultEvent::Rows(first_batch) = &events[1] else {
        panic!("expected rows, got {:?}", events[1]);
    };
    assert_eq!(first_batch[2][2], None);
    assert_eq!(first_batch[0][3].as_deref(), Some("2026-10-03 12:00:00"));
    assert_eq!(first_batch[0][4].as_deref(), Some("<blob 2 bytes>"));

    // Multiple statements produce several result sets.
    let events = run(session.as_ref(), "SELECT 1 AS a; SELECT 2 AS b, 3 AS c").await;
    let result_sets = events
        .iter()
        .filter(|event| matches!(event, ResultEvent::Columns(_)))
        .count();
    assert_eq!(result_sets, 2);

    let agent = ExecOptions {
        read_only_transaction: true,
    };
    let error = error_of(session.as_ref(), &format!("DELETE FROM {table}"), agent).await;
    assert!(error.contains("READ ONLY"), "{error}");
    let error = error_of(
        session.as_ref(),
        &format!("SELECT 1; DELETE FROM {table}"),
        agent,
    )
    .await;
    assert!(error.contains("single statement"), "{error}");
    // DDL commits the read-only transaction implicitly, so it must be stopped by the session.
    let probe = format!("{table}_agent_probe");
    let error = error_of(
        session.as_ref(),
        &format!("CREATE TABLE {probe} (id INT)"),
        agent,
    )
    .await;
    assert!(error.contains("READ ONLY"), "{error}");
    let error = error_of(session.as_ref(), &format!("DROP TABLE {table}"), agent).await;
    assert!(error.contains("READ ONLY"), "{error}");
    let events = run(
        session.as_ref(),
        &format!("SELECT COUNT(*) FROM information_schema.tables WHERE table_name = '{probe}'"),
    )
    .await;
    let ResultEvent::Rows(rows) = &events[1] else {
        panic!("expected rows, got {:?}", events[1]);
    };
    assert_eq!(rows[0][0].as_deref(), Some("0"));
    // Afterwards, the session accepts writes again.
    run(
        session.as_ref(),
        &format!("CREATE TABLE {probe} (id INT); DROP TABLE {probe}"),
    )
    .await;

    let read_only = resolved("ZED_DATABASE_TEST_MYSQL_URL", "mysql", |config| {
        config.read_only = true
    })
    .unwrap();
    let read_only_session = connect(&read_only).await;
    let error = error_of(
        read_only_session.as_ref(),
        &format!("DELETE FROM {table}"),
        ExecOptions::default(),
    )
    .await;
    assert!(error.contains("READ ONLY"), "{error}");

    // Dropping the stream kills the statement from a control connection.
    let stream = session.execute("SELECT SLEEP(30)".into(), ExecOptions::default());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let cancelled_at = Instant::now();
    drop(stream);
    loop {
        let events = run(
            read_only_session.as_ref(),
            "SELECT COUNT(*) FROM information_schema.processlist WHERE info = 'SELECT SLEEP(30)'",
        )
        .await;
        if matches!(&events[1], ResultEvent::Rows(rows) if rows[0][0].as_deref() == Some("0")) {
            break;
        }
        assert!(
            cancelled_at.elapsed() < Duration::from_secs(1),
            "the statement kept running after the stream was dropped"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(row_count(&run(session.as_ref(), "SELECT 1").await), 1);

    run(session.as_ref(), &format!("DROP TABLE {table}")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_mysql_tls_modes() {
    let Some(required) = resolved("ZED_DATABASE_TEST_MYSQL_URL", "mysql", |config| {
        config.ssl_mode = DatabaseSslMode::Require
    }) else {
        return;
    };
    // MySQL 8 generates a self-signed certificate: encryption works, verification doesn't.
    let session = connect(&required).await;
    let events = run(session.as_ref(), "SHOW SESSION STATUS LIKE 'Ssl_cipher'").await;
    assert!(
        matches!(&events[1], ResultEvent::Rows(rows) if rows[0][1].as_deref().is_some_and(|cipher| !cipher.is_empty())),
        "the session is not encrypted: {events:?}"
    );

    let verified = resolved("ZED_DATABASE_TEST_MYSQL_URL", "mysql", |config| {
        config.ssl_mode = DatabaseSslMode::VerifyFull
    })
    .unwrap();
    assert!(
        mysql::connect(&verified, &verified.host, verified.port)
            .await
            .is_err(),
        "a self-signed certificate must not pass verify-full"
    );
}

/// `ZED_DATABASE_TEST_POSTGRES_TLS_URL` points at a server with a self-signed certificate, and
/// `ZED_DATABASE_TEST_POSTGRES_TLS_CA` at that certificate.
#[tokio::test(flavor = "multi_thread")]
async fn test_postgres_tls_modes() {
    let Ok(certificate) = std::env::var("ZED_DATABASE_TEST_POSTGRES_TLS_CA") else {
        return;
    };
    let with_mode = |mode: DatabaseSslMode, root_cert: Option<&str>| {
        resolved("ZED_DATABASE_TEST_POSTGRES_TLS_URL", "postgres", |config| {
            config.ssl_mode = mode;
            config.ssl_root_cert = root_cert.map(str::to_string);
        })
    };
    let Some(required) = with_mode(DatabaseSslMode::Require, None) else {
        return;
    };
    let session = connect(&required).await;
    let events = run(
        session.as_ref(),
        "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
    )
    .await;
    assert!(
        matches!(&events[1], ResultEvent::Rows(rows) if rows[0][0].as_deref() == Some("t")),
        "the session is not encrypted: {events:?}"
    );

    let full = with_mode(DatabaseSslMode::VerifyFull, None).unwrap();
    assert!(
        postgres::connect(&full, &full.host, full.port)
            .await
            .is_err(),
        "a self-signed certificate must not pass verify-full with the system roots"
    );

    // The certificate is issued for the container's host name, not 127.0.0.1.
    let ca = with_mode(DatabaseSslMode::VerifyCa, Some(&certificate)).unwrap();
    postgres::connect(&ca, &ca.host, ca.port)
        .await
        .expect("verify-ca with the server's CA ignores the host name");
    let full_with_ca = with_mode(DatabaseSslMode::VerifyFull, Some(&certificate)).unwrap();
    assert!(
        postgres::connect(&full_with_ca, &full_with_ca.host, full_with_ca.port)
            .await
            .is_err(),
        "verify-full checks the host name"
    );
}
