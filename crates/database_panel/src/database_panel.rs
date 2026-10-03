//! The database panel: browsing connections and schemas in a dock panel, running SQL from
//! editors, and showing results in center-pane tabs.

mod connection_modal;
mod explain;
mod history;
mod panel;
mod results;
mod sql_completion;
mod sql_editor;

use anyhow::Result;
use command_palette_hooks::CommandPaletteFilter;
use database_core::DatabaseSettings;
use gpui::{Action, App, AsyncWindowContext, Context, Task, TaskExt as _, WeakEntity, Window};
use schemars::JsonSchema;
use serde::Deserialize;
use settings::{Settings as _, SettingsStore};
use workspace::Workspace;

pub use panel::DatabasePanel;
pub use results::QueryResultsItem;
pub use sql_editor::SqlEditorToolbar;

gpui::actions!(
    database_panel,
    [
        /// Toggles focus on the database panel.
        ToggleFocus,
        /// Toggles the database panel.
        Toggle,
        /// Adds a database connection.
        NewConnection,
        /// Reloads the schema of the selected connection.
        RefreshSchema,
        /// Runs the SQL statement under the cursor, or the selection.
        RunQuery,
        /// Runs the selected SQL, or the whole file if nothing is selected.
        RunSelection,
        /// Cancels the running query.
        CancelQuery,
        /// Opens a new SQL file using the selected connection.
        NewSqlFile,
        /// Chooses the connection that the current SQL editor runs queries against.
        SelectConnection,
        /// Shows recently executed queries of the current connection.
        QueryHistory,
        /// Loads more rows of a result that stopped at the row limit.
        LoadMoreRows,
        /// Opens the SQL of the current result in an editor.
        OpenQueryInEditor,
        /// Shows the execution plan of the statement under the cursor.
        ExplainQuery,
    ]
);

/// The format of an exported result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResultFormat {
    #[default]
    Csv,
    Json,
    Markdown,
}

impl From<ResultFormat> for database_core::export::ExportFormat {
    fn from(format: ResultFormat) -> Self {
        match format {
            ResultFormat::Csv => Self::Csv,
            ResultFormat::Json => Self::Json,
            ResultFormat::Markdown => Self::Markdown,
        }
    }
}

/// Opens the current result, with its sorting and filters applied, in a new editor.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = database_panel)]
#[serde(deny_unknown_fields)]
pub struct ExportResults {
    #[serde(default)]
    pub format: ResultFormat,
}

/// Copies the current result, with its sorting and filters applied, to the clipboard.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = database_panel)]
#[serde(deny_unknown_fields)]
pub struct CopyResults {
    #[serde(default)]
    pub format: ResultFormat,
}

const ACTION_NAMESPACE: &str = "database_panel";

pub fn init(cx: &mut App) {
    database_core::init(cx);

    cx.observe_new(|workspace: &mut Workspace, _, cx| {
        database_core::mcp::register_project(workspace.project(), cx);
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            if !workspace.toggle_panel_focus::<DatabasePanel>(window, cx) {
                workspace.close_panel::<DatabasePanel>(window, cx);
            }
        });
        workspace.register_action(|workspace, _: &NewConnection, window, cx| {
            connection_modal::ConnectionModal::toggle(workspace, None, window, cx);
        });
    })
    .detach();

    sql_editor::init(cx);
    history::init(cx);
    results::init(cx);

    update_command_palette_filter(cx);
    cx.observe_global::<SettingsStore>(update_command_palette_filter)
        .detach();
}

/// Hides the panel's actions from the command palette while it is disabled.
fn update_command_palette_filter(cx: &mut App) {
    let enabled = DatabaseSettings::get_global(cx).enabled;
    CommandPaletteFilter::update_global(cx, |filter, _| {
        if enabled {
            filter.show_namespace(ACTION_NAMESPACE);
        } else {
            filter.hide_namespace(ACTION_NAMESPACE);
        }
    });
}

pub fn is_enabled(cx: &App) -> bool {
    DatabaseSettings::get_global(cx).enabled
}

/// Adds the panel to a new workspace when it is enabled, and adds or removes it whenever the
/// `database_panel.enabled` setting changes.
///
/// Call this while the workspace restores its docks, so that an open database panel is restored
/// too.
pub async fn initialize(
    workspace: WeakEntity<Workspace>,
    mut cx: AsyncWindowContext,
) -> Result<()> {
    workspace
        .update_in(&mut cx, |workspace, window, cx| {
            setup_or_teardown(workspace, window, cx)
        })?
        .await?;
    workspace.update_in(&mut cx, |_, window, cx| {
        cx.observe_global_in::<SettingsStore>(window, |workspace, window, cx| {
            setup_or_teardown(workspace, window, cx).detach_and_log_err(cx);
        })
        .detach();
    })?;
    Ok(())
}

fn setup_or_teardown(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<()>> {
    let should_exist = |workspace: &Workspace, cx: &App| {
        is_enabled(cx) && !workspace.project().read(cx).is_via_collab()
    };
    match (
        should_exist(workspace, cx),
        workspace.panel::<DatabasePanel>(cx),
    ) {
        (true, None) => cx.spawn_in(window, async move |workspace, cx| {
            let panel = DatabasePanel::load(workspace.clone(), cx.clone()).await?;
            workspace.update_in(cx, |workspace, window, cx| {
                if should_exist(workspace, cx) && workspace.panel::<DatabasePanel>(cx).is_none() {
                    workspace.add_panel(panel, window, cx);
                }
            })
        }),
        (false, Some(panel)) => {
            workspace.remove_panel(&panel, window, cx);
            Task::ready(Ok(()))
        }
        _ => Task::ready(Ok(())),
    }
}

#[cfg(test)]
mod database_panel_tests;
