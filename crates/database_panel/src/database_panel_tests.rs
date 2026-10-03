use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use database_core::{ConnectionKey, DbStore, DriverKind, QueryState};
use editor::{Editor, test::editor_test_context::EditorTestContext};
use gpui::{
    BorrowAppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext, WindowHandle,
};
use language::{Language, LanguageConfig, LanguageMatcher, Point};
use project::{FakeFs, Project};
use serde_json::json;
use settings::SettingsStore;
use util::path;
use workspace::{MultiWorkspace, Workspace};

use crate::{
    DatabasePanel, QueryResultsItem, RunQuery, RunSelection,
    connection_modal::ConnectionModal,
    panel::{CopyName, ShowRows},
    results::ResultOrigin,
};

fn init_test(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    cx.update(|cx| {
        workspace::AppState::test(cx);
        gpui_tokio::init(cx);
        editor::init(cx);
        crate::init(cx);
    });
}

fn enable_panel(cx: &mut TestAppContext, database: &Path) {
    let database = database.to_string_lossy().into_owned();
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.database_panel.get_or_insert_default().enabled = Some(true);
                let content = serde_json::from_value(json!({
                    "driver": "sqlite",
                    "path": database,
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
}

fn create_database(directory: &Path) -> PathBuf {
    let path = directory.join("app.sqlite3");
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE numbers (n INTEGER PRIMARY KEY, label TEXT);
             WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 1500)
             INSERT INTO numbers SELECT n, 'n' || n FROM s;
             CREATE VIEW names AS SELECT label FROM numbers;",
        )
        .unwrap();
    path
}

/// Waits for work on the Tokio runtime, which the test executor can't see.
#[track_caller]
fn wait_until(
    cx: &mut VisualTestContext,
    mut condition: impl FnMut(&mut VisualTestContext) -> bool,
) {
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

async fn open_workspace(
    cx: &mut TestAppContext,
) -> (
    Entity<Project>,
    WindowHandle<MultiWorkspace>,
    Entity<Workspace>,
) {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/project"), json!({ "query.sql": "" }))
        .await;
    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    (project, window, workspace)
}

async fn add_panel(
    window: WindowHandle<MultiWorkspace>,
    workspace: &Entity<Workspace>,
    cx: &mut TestAppContext,
) -> Entity<DatabasePanel> {
    let workspace_weak = workspace.downgrade();
    let panel = window
        .update(cx, |_, window, cx| {
            cx.spawn_in(window, async move |_, cx| {
                DatabasePanel::load(workspace_weak, cx.clone()).await
            })
        })
        .unwrap()
        .await
        .unwrap();
    window
        .update(cx, |multi_workspace, window, cx| {
            multi_workspace.workspace().update(cx, |workspace, cx| {
                workspace.add_panel(panel.clone(), window, cx);
            });
        })
        .unwrap();
    panel
}

fn sql_language() -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: "SQL".into(),
            matcher: LanguageMatcher {
                path_suffixes: vec!["sql".to_string()],
                ..Default::default()
            }
            .into(),
            line_comments: vec!["-- ".into()],
            ..Default::default()
        },
        None,
    ))
}

fn results(workspace: &Entity<Workspace>, cx: &VisualTestContext) -> Vec<Entity<QueryResultsItem>> {
    workspace.read_with(cx, |workspace, cx| {
        workspace.items_of_type::<QueryResultsItem>(cx).collect()
    })
}

#[gpui::test]
async fn test_panel_tree_and_row_preview(cx: &mut TestAppContext) {
    init_test(cx);
    let directory = tempfile::tempdir().unwrap();
    let database = create_database(directory.path());
    enable_panel(cx, &database);
    let (_project, window, workspace) = open_workspace(cx).await;
    let panel = add_panel(window, &workspace, cx).await;
    let cx = &mut VisualTestContext::from_window(window.into(), cx);

    assert_eq!(
        panel.read_with(cx, |panel, _| panel.entries_text()),
        ["> fixtures"]
    );

    // Expanding the connection connects and loads schemas.
    panel.update_in(cx, |panel, window, cx| {
        panel.select_entry("fixtures", cx);
        panel.expand_selected(&menu::SelectChild, window, cx);
    });
    wait_until(cx, |cx| {
        panel.read_with(cx, |panel, _| {
            panel.entries_text() == ["v fixtures", "  > main"]
        })
    });
    panel.update_in(cx, |panel, window, cx| {
        panel.select_entry("main", cx);
        panel.expand_selected(&menu::SelectChild, window, cx);
    });
    wait_until(cx, |cx| {
        panel.read_with(cx, |panel, _| {
            panel.entries_text() == ["v fixtures", "  v main", "    > names", "    > numbers"]
        })
    });
    panel.update_in(cx, |panel, window, cx| {
        panel.select_entry("numbers", cx);
        panel.expand_selected(&menu::SelectChild, window, cx);
    });
    wait_until(cx, |cx| {
        panel.read_with(cx, |panel, _| {
            panel.entries_text()
                == [
                    "v fixtures",
                    "  v main",
                    "    > names",
                    "    v numbers",
                    "      n INTEGER",
                    "      label TEXT",
                ]
        })
    });

    // Filtering keeps the matching entries and their ancestors.
    panel.update_in(cx, |panel, window, cx| panel.set_filter("lab", window, cx));
    cx.run_until_parked();
    assert_eq!(
        panel.read_with(cx, |panel, _| panel.entries_text()),
        [
            "v fixtures",
            "  v main",
            "    v numbers",
            "      label TEXT"
        ]
    );
    panel.update_in(cx, |panel, window, cx| panel.set_filter("", window, cx));
    cx.run_until_parked();

    panel.update_in(cx, |panel, window, cx| {
        panel.select_entry("numbers", cx);
        panel.copy_name(&CopyName, window, cx);
    });
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some("numbers".to_string())
    );

    // Showing rows opens a result tab with the first 100 rows.
    panel.update_in(cx, |panel, window, cx| {
        panel.show_rows(&ShowRows, window, cx)
    });
    wait_until(cx, |cx| {
        results(&workspace, cx).first().is_some_and(|item| {
            item.read_with(cx, |item, cx| {
                item.run()
                    .is_some_and(|run| run.read(cx).state == QueryState::Finished)
            })
        })
    });
    let item = results(&workspace, cx).remove(0);
    item.read_with(cx, |item, cx| {
        assert_eq!(
            item.sql(),
            "SELECT * FROM main.numbers LIMIT 100".replace("main.", "")
        );
        assert_eq!(item.table().read(cx).contents().rows.len(), 100);
        assert_eq!(item.config.driver, DriverKind::Sqlite);
    });

    // Showing the same table again reuses the tab.
    panel.update_in(cx, |panel, window, cx| {
        panel.show_rows(&ShowRows, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(results(&workspace, cx).len(), 1);
}

#[gpui::test]
async fn test_run_statements_from_sql_editor(cx: &mut TestAppContext) {
    init_test(cx);
    let directory = tempfile::tempdir().unwrap();
    let database = create_database(directory.path());
    enable_panel(cx, &database);
    let (project, window, workspace) = open_workspace(cx).await;
    let cx = &mut VisualTestContext::from_window(window.into(), cx);

    // `.sql` files run without the SQL extension installed.
    let worktree_id = project.read_with(cx, |project, cx| {
        project.worktrees(cx).next().unwrap().read(cx).id()
    });
    let editor = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(
                (worktree_id, util::rel_path::rel_path("query.sql")),
                None,
                true,
                window,
                cx,
            )
        })
        .await
        .unwrap()
        .downcast::<Editor>()
        .unwrap();
    let set_text = |cx: &mut VisualTestContext, text: &str, cursor: Option<Point>| {
        editor.update_in(cx, |editor, window, cx| {
            editor.set_text(text, window, cx);
            let selection = match cursor {
                Some(cursor) => cursor..cursor,
                None => Point::zero()..editor.buffer().read(cx).snapshot(cx).max_point(),
            };
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([selection])
            });
            window.focus(&editor.focus_handle(cx), cx);
        });
    };

    set_text(
        cx,
        "select count(*) from numbers;\n\nselect label from numbers where n <= 3;",
        Some(Point::new(2, 7)),
    );
    // The only connection is used without asking.
    cx.dispatch_action(RunQuery);
    wait_until(cx, |cx| {
        results(&workspace, cx).first().is_some_and(|item| {
            item.read_with(cx, |item, cx| {
                item.run()
                    .is_some_and(|run| run.read(cx).state == QueryState::Finished)
            })
        })
    });
    let item = results(&workspace, cx).remove(0);
    item.read_with(cx, |item, cx| {
        assert_eq!(item.sql(), "select label from numbers where n <= 3");
        assert_eq!(item.origin(), &ResultOrigin::Editor(editor.entity_id()));
        assert_eq!(item.table().read(cx).contents().rows.len(), 3);
    });
    // The editor keeps focus while results show next to it.
    editor.update_in(cx, |editor, window, cx| {
        assert!(editor.focus_handle(cx).is_focused(window));
    });

    // Running another statement from the same editor replaces the result.
    set_text(
        cx,
        "select count(*) from numbers;\n\nselect label from numbers;",
        Some(Point::new(0, 0)),
    );
    cx.dispatch_action(RunQuery);
    wait_until(cx, |cx| {
        item.read_with(cx, |item, cx| {
            item.sql() == "select count(*) from numbers"
                && item
                    .run()
                    .is_some_and(|run| run.read(cx).state == QueryState::Finished)
        })
    });
    assert_eq!(results(&workspace, cx).len(), 1);
    item.read_with(cx, |item, cx| {
        let contents = item.table().read(cx).contents().clone();
        assert_eq!(
            contents.rows[0].as_slice()[0]
                .display_value()
                .map(|value| value.as_ref()),
            Some("1500")
        );
    });

    // A pinned result is kept, and the next run opens a new tab.
    cx.update(|_, cx| {
        let pane = workspace.read(cx).pane_for(&item).unwrap();
        pane.update(cx, |pane, _| {
            assert_eq!(pane.index_for_item(&item), Some(0));
            pane.set_pinned_count(1);
        });
    });
    cx.dispatch_action(RunSelection);
    wait_until(cx, |cx| results(&workspace, cx).len() == 2);

    // Statements without rows report the affected rows.
    set_text(cx, "update numbers set label = 'x' where n < 10", None);
    cx.dispatch_action(RunQuery);
    wait_until(cx, |cx| {
        results(&workspace, cx)
            .iter()
            .any(|item| item.read_with(cx, |item, _| item.message().is_some()))
    });
    let message = results(&workspace, cx)
        .iter()
        .find_map(|item| item.read_with(cx, |item, _| item.message()));
    assert_eq!(
        message.as_deref(),
        Some("Statement executed. 9 rows affected.")
    );
}

#[gpui::test]
async fn test_disabling_removes_the_panel(cx: &mut TestAppContext) {
    init_test(cx);
    let directory = tempfile::tempdir().unwrap();
    let database = create_database(directory.path());
    enable_panel(cx, &database);
    let (_project, window, workspace) = open_workspace(cx).await;
    let workspace_weak = workspace.downgrade();
    window
        .update(cx, |_, window, cx| {
            cx.spawn_in(window, async move |_, cx| {
                crate::initialize(workspace_weak, cx.clone()).await
            })
        })
        .unwrap()
        .await
        .unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    cx.run_until_parked();
    assert!(workspace.read_with(cx, |workspace, cx| {
        workspace.panel::<DatabasePanel>(cx).is_some()
    }));

    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.database_panel.get_or_insert_default().enabled = Some(false);
            });
        });
    });
    cx.run_until_parked();
    assert!(workspace.read_with(cx, |workspace, cx| {
        workspace.panel::<DatabasePanel>(cx).is_none()
    }));
}

#[gpui::test]
async fn test_connection_modal_saves_to_settings(cx: &mut TestAppContext) {
    init_test(cx);
    let directory = tempfile::tempdir().unwrap();
    let database = create_database(directory.path());
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.database_panel.get_or_insert_default().enabled = Some(true);
            });
        });
    });
    let (project, window, workspace) = open_workspace(cx).await;
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    workspace.update_in(cx, |workspace, window, cx| {
        ConnectionModal::toggle(workspace, None, window, cx);
    });
    let modal = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<ConnectionModal>(cx)
        })
        .unwrap();

    // Saving without a name is refused.
    modal.update_in(cx, |modal, window, cx| {
        modal.save(&menu::Confirm, window, cx)
    });
    assert_eq!(
        modal.read_with(cx, |modal, _| modal.status()).as_deref(),
        Some("Give the connection a name")
    );

    modal.update_in(cx, |modal, window, cx| {
        modal.set_driver(DriverKind::Sqlite);
        modal.set_field("name", "local", window, cx);
        modal.set_field("path", &database.to_string_lossy(), window, cx);
        modal.test(window, cx);
    });
    wait_until(cx, |cx| {
        modal.read_with(cx, |modal, _| {
            modal.status().as_deref() == Some("Connected successfully")
        })
    });
    modal.update_in(cx, |modal, window, cx| {
        modal.save(&menu::Confirm, window, cx)
    });
    cx.run_until_parked();
    assert!(workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<ConnectionModal>(cx).is_none()
    }));

    // The connection is written to the user settings file, without any password.
    let fs = workspace.read_with(cx, |workspace, _| workspace.app_state().fs.clone());
    let settings = fs.load(paths::settings_file()).await.unwrap();
    let settings: serde_json::Value = serde_json_lenient::from_str(&settings).unwrap();
    assert_eq!(
        settings["database_connections"]["local"],
        json!({
            "driver": "sqlite",
            "path": database.to_string_lossy(),
            "environment": "local",
            "read_only": false,
        })
    );
    // Connections from settings are listed once the settings file is loaded.
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                let content = serde_json::from_value(json!({
                    "driver": "sqlite",
                    "path": database.to_string_lossy(),
                }))
                .unwrap();
                settings
                    .project
                    .database_connections
                    .get_or_insert_default()
                    .insert("local".into(), content);
            });
        });
    });
    let connections = cx.update(|_, cx| DbStore::connections_for_project(&project, cx));
    assert_eq!(
        connections
            .iter()
            .map(|connection| connection.key.clone())
            .collect::<Vec<_>>(),
        vec![ConnectionKey::user("local")]
    );
}

#[gpui::test]
async fn test_statement_detection_without_a_grammar(cx: &mut TestAppContext) {
    init_test(cx);
    let mut editor_cx = EditorTestContext::new(cx).await;
    editor_cx.update_buffer(|buffer, cx| buffer.set_language(Some(sql_language()), cx));
    editor_cx.set_state("select 1;\nselect ˇ2\n  from t;\n\nselect 3");
    let range = editor_cx.update_editor(|editor: &mut Editor, _, cx| {
        let buffer = editor.buffer().read(cx).as_singleton().unwrap();
        let snapshot = buffer.read(cx).snapshot();
        let offset = snapshot.text().find('2').unwrap();
        crate::sql_editor::statement_range(&snapshot, offset)
            .map(|range| snapshot.text_for_range(range).collect::<String>())
    });
    assert_eq!(range.as_deref(), Some("select 2\n  from t"));
}
