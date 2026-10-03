//! Database tools for AI agents, served over MCP.
//!
//! When `database_panel.agent_access` is enabled, a context server named `zed-database` is
//! registered. Each project gets an in-process MCP server on a Unix socket; agents start
//! `zed --database-mcp <socket>`, which bridges stdio to that socket. This makes the tools
//! available to Zed's agent and to external agents alike, and every call goes through the agent's
//! tool permission prompts.

use std::{collections::HashMap, io, path::PathBuf, rc::Rc, sync::Arc};

use anyhow::{Context as _, Result, anyhow};
use context_server::{
    ContextServerCommand,
    listener::{McpServer, McpServerTool, ToolResponse},
    types::{
        Implementation, InitializeParams, InitializeResponse, MessageContent, Prompt,
        PromptArgument, PromptMessage, PromptsCapabilities, PromptsGetParams, PromptsGetResponse,
        PromptsListResponse, ProtocolVersion, Role, ServerCapabilities, ToolAnnotations,
        ToolResponseContent, ToolsCapabilities, requests,
    },
};
use gpui::{App, AsyncApp, BorrowAppContext as _, Entity, EntityId, Global, Task, WeakEntity};
use project::{
    Project,
    context_server_store::registry::{ContextServerDescriptor, ContextServerDescriptorRegistry},
    worktree_store::WorktreeStore,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{Settings as _, SettingsStore};

use crate::{connection::ConnectionConfig, database_settings::DatabaseSettings, store::DbStore};

/// The id of the context server that provides the database tools.
pub const CONTEXT_SERVER_ID: &str = "zed-database";
/// The command line flag that makes Zed bridge stdio to a database MCP socket.
pub const BRIDGE_FLAG: &str = "--database-mcp";

/// Rows and characters returned to agents per query, to keep results within their context.
pub const AGENT_MAX_ROWS: usize = 200;
pub const AGENT_MAX_CHARS: usize = 20_000;

#[derive(Default)]
struct McpServers {
    /// Servers by the entity id of their project's worktree store.
    servers: HashMap<EntityId, Rc<McpServer>>,
    projects: Vec<WeakEntity<Project>>,
}

impl Global for McpServers {}

pub fn init(cx: &mut App) {
    cx.set_global(McpServers::default());
    update_registration(cx);
    cx.observe_global::<SettingsStore>(update_registration)
        .detach();
    // Projects are registered when they're created, because their context server store asks
    // for the server's command before a workspace opens them.
    cx.observe_new(|_: &mut Project, _, cx| {
        let project = cx.entity();
        register_project(&project, cx);
    })
    .detach();
}

/// Makes a project's connections available to the tools.
fn register_project(project: &Entity<Project>, cx: &mut App) {
    if let Some(servers) = cx.try_global::<McpServers>()
        && !servers
            .projects
            .iter()
            .any(|registered| registered == &project.downgrade())
    {
        cx.update_global::<McpServers, _>(|servers, _| {
            servers
                .projects
                .retain(|project| project.upgrade().is_some());
            servers.projects.push(project.downgrade());
        });
    }
}

fn update_registration(cx: &mut App) {
    let enabled = {
        let settings = DatabaseSettings::get_global(cx);
        settings.enabled && settings.agent_access
    };
    let registry = ContextServerDescriptorRegistry::default_global(cx);
    let registered = registry
        .read(cx)
        .context_server_descriptor(CONTEXT_SERVER_ID)
        .is_some();
    match (enabled, registered) {
        (true, false) => registry.update(cx, |registry, cx| {
            registry.register_context_server_descriptor(
                CONTEXT_SERVER_ID.into(),
                Arc::new(DatabaseContextServer),
                cx,
            )
        }),
        (false, true) => {
            registry.update(cx, |registry, cx| {
                registry.unregister_context_server_descriptor_by_id(CONTEXT_SERVER_ID, cx)
            });
            cx.update_global::<McpServers, _>(|servers, _| servers.servers.clear());
        }
        _ => {}
    }
}

struct DatabaseContextServer;

impl ContextServerDescriptor for DatabaseContextServer {
    fn command(
        &self,
        worktree_store: Entity<WorktreeStore>,
        cx: &AsyncApp,
    ) -> Task<Result<ContextServerCommand>> {
        cx.spawn(async move |cx| {
            let socket = socket_for(&worktree_store, cx).await?;
            Ok(ContextServerCommand {
                path: std::env::current_exe().context("locating the Zed executable")?,
                args: vec![BRIDGE_FLAG.into(), socket.to_string_lossy().into_owned()],
                env: None,
                timeout: None,
            })
        })
    }

    fn configuration(
        &self,
        _worktree_store: Entity<WorktreeStore>,
        _cx: &AsyncApp,
    ) -> Task<Result<Option<extension::ContextServerConfiguration>>> {
        Task::ready(Ok(None))
    }
}

/// The socket of the project's MCP server, starting the server if needed.
async fn socket_for(worktree_store: &Entity<WorktreeStore>, cx: &mut AsyncApp) -> Result<PathBuf> {
    let key = worktree_store.entity_id();
    if let Some(server) = cx.update(|cx| cx.global::<McpServers>().servers.get(&key).cloned()) {
        return Ok(server.socket_path().to_path_buf());
    }
    let project = cx
        .update(|cx| {
            cx.global::<McpServers>()
                .projects
                .iter()
                .filter_map(WeakEntity::upgrade)
                .find(|project| project.read(cx).worktree_store() == *worktree_store)
        })
        .context("the project of this agent is not open")?;

    let mut server = McpServer::new(cx).await?;
    server.handle_request::<requests::Initialize>(handle_initialize);
    server.handle_request::<requests::PromptsList>(|_, _| Task::ready(Ok(prompts())));
    server.handle_request::<requests::PromptsGet>({
        let project = project.downgrade();
        move |params, cx| {
            let project = project.clone();
            cx.spawn(async move |cx| table_prompt(project, params, cx).await)
        }
    });
    let tools = DatabaseTools {
        project: project.downgrade(),
    };
    server.add_tool(ListConnectionsTool(tools.clone()));
    server.add_tool(SchemaTool(tools.clone()));
    server.add_tool(QueryTool(tools));

    let socket = server.socket_path().to_path_buf();
    cx.update(|cx| {
        cx.update_global::<McpServers, _>(|servers, _| {
            servers.servers.insert(key, Rc::new(server));
        })
    });
    Ok(socket)
}

fn handle_initialize(_: InitializeParams, _cx: &App) -> Task<Result<InitializeResponse>> {
    let version = env!("CARGO_PKG_VERSION").to_string();
    Task::ready(Ok(InitializeResponse {
        protocol_version: ProtocolVersion("2025-06-18".into()),
        capabilities: ServerCapabilities {
            experimental: None,
            logging: None,
            completions: None,
            prompts: Some(PromptsCapabilities {
                list_changed: Some(false),
            }),
            resources: None,
            tools: Some(ToolsCapabilities {
                list_changed: Some(false),
            }),
        },
        server_info: Implementation {
            name: CONTEXT_SERVER_ID.into(),
            title: Some("Zed Databases".into()),
            version,
            description: Some("The database connections configured in Zed".into()),
        },
        meta: None,
    }))
}

fn prompts() -> PromptsListResponse {
    PromptsListResponse {
        prompts: vec![Prompt {
            name: "table".into(),
            title: Some("Database Table".into()),
            description: Some(
                "Adds the definition of a database table or view to the conversation".into(),
            ),
            arguments: Some(vec![
                PromptArgument {
                    name: "connection".into(),
                    title: None,
                    description: Some("The connection name".into()),
                    required: Some(true),
                },
                PromptArgument {
                    name: "table".into(),
                    title: None,
                    description: Some("The table, optionally qualified as schema.table".into()),
                    required: Some(true),
                },
            ]),
        }],
        next_cursor: None,
        meta: None,
    }
}

async fn table_prompt(
    project: WeakEntity<Project>,
    params: PromptsGetParams,
    cx: &mut AsyncApp,
) -> Result<PromptsGetResponse> {
    anyhow::ensure!(params.name == "table", "unknown prompt `{}`", params.name);
    let arguments = params.arguments.unwrap_or_default();
    let connection = arguments
        .get("connection")
        .context("the `connection` argument is required")?;
    let table = arguments
        .get("table")
        .context("the `table` argument is required")?;
    let tools = DatabaseTools { project };
    let (config, project) = tools.connection(connection, cx)?;
    let (schema, relation) = split_table_name(table);
    let schema = match schema {
        Some(schema) => schema,
        None => tools.default_schema(&config, &project, cx).await?,
    };
    let ddl = cx
        .update(|cx| {
            DbStore::global(cx).update(cx, |store, cx| {
                store.relation_ddl(
                    config.clone(),
                    Some(project),
                    schema.into(),
                    relation.into(),
                    cx,
                )
            })
        })
        .await?;
    Ok(PromptsGetResponse {
        description: Some(format!("The definition of {table} in {connection}")),
        messages: vec![PromptMessage {
            role: Role::User,
            content: MessageContent::Text {
                text: format!(
                    "This is the definition of `{table}` in the {} database `{connection}`:\n\n```sql\n{ddl}\n```",
                    config.driver.display_name()
                ),
                annotations: None,
            },
        }],
        meta: None,
    })
}

fn split_table_name(name: &str) -> (Option<String>, String) {
    match name.split_once('.') {
        Some((schema, table)) => (Some(schema.trim().to_string()), table.trim().to_string()),
        None => (None, name.trim().to_string()),
    }
}

#[derive(Clone)]
struct DatabaseTools {
    project: WeakEntity<Project>,
}

impl DatabaseTools {
    fn connection(
        &self,
        name: &str,
        cx: &mut AsyncApp,
    ) -> Result<(ConnectionConfig, Entity<Project>)> {
        let project = self.project.upgrade().context("the project was closed")?;
        let config = cx
            .update(|cx| DbStore::connections_for_project(&project, cx))
            .into_iter()
            .find(|connection| connection.key.id.as_ref() == name)
            .with_context(|| {
                format!("no connection named `{name}`; use db_list_connections to see them")
            })?;
        Ok((config, project))
    }

    /// Agents can't type passwords, so they use connections that are already open or that have
    /// a stored password.
    async fn ensure_connected(
        &self,
        config: &ConnectionConfig,
        project: &Entity<Project>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let connect = cx.update(|cx| {
            DbStore::global(cx).update(cx, |store, cx| {
                store.ensure_connected(config.clone(), Some(project.clone()), cx)
            })
        });
        connect.await.map(|_| ()).map_err(|error| {
            if error.is::<crate::PasswordRequired>()
                || error.to_string().contains("password is required")
            {
                anyhow!(
                    "`{}` needs a password; connect to it from Zed's database panel first",
                    config.key.id
                )
            } else {
                error
            }
        })
    }

    async fn default_schema(
        &self,
        config: &ConnectionConfig,
        project: &Entity<Project>,
        cx: &mut AsyncApp,
    ) -> Result<String> {
        self.schemas(config, project, cx)
            .await?
            .into_iter()
            .next()
            .context("the database has no schemas")
    }

    async fn schemas(
        &self,
        config: &ConnectionConfig,
        project: &Entity<Project>,
        cx: &mut AsyncApp,
    ) -> Result<Vec<String>> {
        self.ensure_connected(config, project, cx).await?;
        let load = cx.update(|cx| {
            DbStore::global(cx).update(cx, |store, cx| {
                if store.schemas(&config.key).is_some() {
                    Task::ready(Ok(()))
                } else {
                    store.load_schemas(config.clone(), Some(project.clone()), cx)
                }
            })
        });
        load.await?;
        cx.update(|cx| {
            DbStore::global(cx)
                .read(cx)
                .schemas(&config.key)
                .map(|schemas| {
                    schemas
                        .iter()
                        .map(|schema| schema.name.to_string())
                        .collect()
                })
                .context("the schema couldn't be loaded")
        })
    }
}

/// Lists the database connections configured in Zed for this project. Use the names with
/// db_schema and db_query.
#[derive(Deserialize, JsonSchema)]
struct ListConnectionsInput {}

#[derive(Serialize, JsonSchema)]
struct ConnectionDescription {
    name: String,
    database: String,
    environment: String,
    read_only: bool,
    connected: bool,
}

#[derive(Serialize, JsonSchema)]
struct ListConnectionsOutput {
    connections: Vec<ConnectionDescription>,
}

#[derive(Clone)]
struct ListConnectionsTool(DatabaseTools);

impl McpServerTool for ListConnectionsTool {
    type Input = ListConnectionsInput;
    type Output = ListConnectionsOutput;
    const NAME: &'static str = "db_list_connections";

    fn annotations(&self) -> ToolAnnotations {
        read_only_annotations("List Database Connections")
    }

    async fn run(
        &self,
        _input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let project = self.0.project.upgrade().context("the project was closed")?;
        let connections = cx.update(|cx| {
            let store = DbStore::global(cx);
            DbStore::connections_for_project(&project, cx)
                .into_iter()
                .map(|connection| ConnectionDescription {
                    connected: store.read(cx).is_connected(&connection.key),
                    name: connection.key.id.to_string(),
                    database: connection.driver.display_name().to_string(),
                    environment: format!("{:?}", connection.environment).to_lowercase(),
                    read_only: connection.read_only,
                })
                .collect::<Vec<_>>()
        });
        let text = if connections.is_empty() {
            "No database connections are configured.".to_string()
        } else {
            connections
                .iter()
                .map(|connection| {
                    format!(
                        "- {} ({}, {}{})",
                        connection.name,
                        connection.database,
                        connection.environment,
                        if connection.read_only {
                            ", read-only"
                        } else {
                            ""
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text }],
            structured_content: ListConnectionsOutput { connections },
        })
    }
}

/// Describes a database's structure. Without `schema`, lists the schemas. With `schema`, lists
/// its tables and views. With `schema` and `table`, returns the table's columns and definition.
#[derive(Deserialize, JsonSchema)]
struct SchemaInput {
    /// The connection name, from db_list_connections.
    connection: String,
    schema: Option<String>,
    table: Option<String>,
}

#[derive(Clone)]
struct SchemaTool(DatabaseTools);

impl McpServerTool for SchemaTool {
    type Input = SchemaInput;
    type Output = ();
    const NAME: &'static str = "db_schema";

    fn annotations(&self) -> ToolAnnotations {
        read_only_annotations("Read Database Schema")
    }

    async fn run(&self, input: Self::Input, cx: &mut AsyncApp) -> Result<ToolResponse<()>> {
        let (config, project) = self.0.connection(&input.connection, cx)?;
        let schemas = self.0.schemas(&config, &project, cx).await?;
        let text = match (input.schema, input.table) {
            (None, None) => format!("Schemas:\n{}", schemas.join("\n")),
            (schema, Some(table)) => {
                let schema = match schema {
                    Some(schema) => schema,
                    None => schemas
                        .first()
                        .cloned()
                        .context("the database has no schemas")?,
                };
                let ddl = cx
                    .update(|cx| {
                        DbStore::global(cx).update(cx, |store, cx| {
                            store.relation_ddl(
                                config.clone(),
                                Some(project.clone()),
                                schema.into(),
                                table.into(),
                                cx,
                            )
                        })
                    })
                    .await?;
                format!("```sql\n{ddl}\n```")
            }
            (Some(schema), None) => {
                anyhow::ensure!(
                    schemas.contains(&schema),
                    "no schema named `{schema}`; available: {}",
                    schemas.join(", ")
                );
                let load = cx.update(|cx| {
                    DbStore::global(cx).update(cx, |store, cx| {
                        store.load_relations(
                            config.clone(),
                            Some(project.clone()),
                            schema.clone().into(),
                            cx,
                        )
                    })
                });
                load.await?;
                cx.update(|cx| {
                    let store = DbStore::global(cx);
                    let relations = store
                        .read(cx)
                        .relations(&config.key, &schema)
                        .unwrap_or_default();
                    relations
                        .iter()
                        .map(|relation| {
                            format!(
                                "{}{}",
                                relation.name,
                                if relation.kind.is_view() {
                                    " (view)"
                                } else {
                                    ""
                                }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
            }
        };
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text }],
            structured_content: (),
        })
    }
}

/// Runs a read-only SQL query and returns up to 200 rows. The query runs as a single statement
/// in a read-only transaction that is rolled back, so it can't change any data. Prefer
/// selecting only the needed columns and filtering rows in SQL.
#[derive(Deserialize, JsonSchema)]
struct QueryInput {
    /// The connection name, from db_list_connections.
    connection: String,
    /// A single SQL statement.
    sql: String,
}

#[derive(Clone)]
struct QueryTool(DatabaseTools);

impl McpServerTool for QueryTool {
    type Input = QueryInput;
    type Output = crate::store::CollectedResult;
    const NAME: &'static str = "db_query";

    fn annotations(&self) -> ToolAnnotations {
        read_only_annotations("Run a Read-Only Database Query")
    }

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let (config, project) = self.0.connection(&input.connection, cx)?;
        self.0.ensure_connected(&config, &project, cx).await?;
        let result = cx
            .update(|cx| {
                DbStore::global(cx).update(cx, |store, cx| {
                    store.execute_for_agent(
                        config,
                        Some(project),
                        input.sql,
                        AGENT_MAX_ROWS,
                        AGENT_MAX_CHARS,
                        cx,
                    )
                })
            })
            .await?;
        let text = format_result(&result);
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text }],
            structured_content: result,
        })
    }
}

fn read_only_annotations(title: &str) -> ToolAnnotations {
    ToolAnnotations {
        title: Some(title.into()),
        read_only_hint: Some(true),
        destructive_hint: Some(false),
        idempotent_hint: Some(true),
        open_world_hint: Some(false),
    }
}

/// A Markdown table of a result, which agents read well.
fn format_result(result: &crate::store::CollectedResult) -> String {
    if result.columns.is_empty() {
        return match result.rows_affected {
            Some(rows) => format!("The statement returned no rows ({rows} affected)."),
            None => "The statement returned no rows.".to_string(),
        };
    }
    let columns = result
        .columns
        .iter()
        .map(|name| crate::export::ExportColumn {
            name,
            kind: crate::driver::ValueKind::Text,
        })
        .collect::<Vec<_>>();
    let rows = result
        .rows
        .iter()
        .map(|row| row.iter().map(|value| value.as_deref()).collect::<Vec<_>>());
    let mut text = crate::export::export(crate::export::ExportFormat::Markdown, &columns, rows);
    if result.truncated {
        text.push_str(&format!(
            "\nOnly the first {} rows are shown. Narrow the query to see others.",
            result.rows.len()
        ));
    }
    text
}

/// Bridges stdin and stdout to the MCP socket of a running Zed. Runs as `zed --database-mcp`.
pub fn run_stdio_bridge(socket: &str) -> Result<()> {
    #[cfg(not(target_os = "windows"))]
    let (mut socket_reader, mut socket_writer) = {
        let stream =
            net::UnixStream::connect(socket).with_context(|| format!("connecting to {socket}"))?;
        (stream.try_clone()?, stream)
    };
    #[cfg(target_os = "windows")]
    let (mut socket_reader, mut socket_writer) = net::UnixStream::connect(socket)
        .with_context(|| format!("connecting to {socket}"))?
        .into_split();

    let output = std::thread::spawn(move || -> io::Result<u64> {
        let mut stdout = io::stdout();
        io::copy(&mut socket_reader, &mut stdout)
    });
    io::copy(&mut io::stdin(), &mut socket_writer)?;
    // The client closed stdin; closing the socket ends the connection.
    drop(socket_writer);
    output
        .join()
        .map_err(|_| anyhow!("the output thread panicked"))??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_table_name() {
        assert_eq!(split_table_name("users"), (None, "users".into()));
        assert_eq!(
            split_table_name("public.users"),
            (Some("public".into()), "users".into())
        );
    }

    #[test]
    fn test_format_result() {
        let result = crate::store::CollectedResult {
            columns: vec!["id".into(), "name".into()],
            rows: vec![vec![Some("1".into()), None]],
            truncated: true,
            rows_affected: None,
        };
        assert_eq!(
            format_result(&result),
            "| id | name |\n| --- | --- |\n| 1 | *NULL* |\n\nOnly the first 1 rows are shown. Narrow the query to see others."
        );
        assert_eq!(
            format_result(&crate::store::CollectedResult {
                rows_affected: Some(0),
                ..Default::default()
            }),
            "The statement returned no rows (0 affected)."
        );
    }
}
