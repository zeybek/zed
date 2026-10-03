---
title: Database Panel - Zed
description: Browse PostgreSQL, MySQL, MariaDB, and SQLite databases, run SQL, and edit results in Zed's database panel.
---

# Database Panel

The database panel connects to PostgreSQL, MySQL, MariaDB, and SQLite databases.
You can browse schemas, run SQL from any `.sql` file, explore results in a table, and edit rows.

The panel is off by default. Enable it in your settings:

```json [settings]
{
  "database_panel": {
    "enabled": true
  }
}
```

Then open it with {#action database_panel::ToggleFocus} ({#kb database_panel::ToggleFocus}) or the database icon in the status bar.

## Adding Connections {#adding-connections}

Use {#action database_panel::NewConnection} or the `+` button in the panel header to add a connection.
The dialog lets you test the connection and saves it to your user settings.

Connections are stored under `database_connections`, keyed by a name that you choose:

```json [settings]
{
  "database_connections": {
    "app-dev": {
      "driver": "postgres",
      "host": "localhost",
      "port": 5432,
      "database": "app_dev",
      "username": "app"
    },
    "analytics": {
      "driver": "mysql",
      "url": "mysql://reader@analytics.internal:3306/events",
      "ssl_mode": "verify-full",
      "environment": "production"
    },
    "fixtures": {
      "driver": "sqlite",
      "path": "test/fixtures.sqlite3"
    }
  }
}
```

The `driver` is one of `postgres`, `mysql`, `mariadb`, or `sqlite`.
For PostgreSQL and MySQL, you can provide a `url`, individual fields, or both: fields set explicitly take precedence over the URL.

### Passwords {#passwords}

Passwords are never written to settings.
Zed asks for a password when a connection needs one and can save it in the system keychain.

If a connection `url` in your settings contains a password, the panel shows a warning icon next to the connection, since settings files are often shared or committed.
Use the keychain or an environment variable instead.

### Environment Variables {#environment-variables}

`url`, `host`, `database`, `username`, `path`, and the certificate paths can reference environment variables, such as `${DATABASE_URL}`, and `~` for your home directory.

```json [settings]
{
  "database_connections": {
    "app": {
      "driver": "postgres",
      "url": "${DATABASE_URL}"
    }
  }
}
```

For trusted projects, variables are resolved from the project environment, including `direnv`.
Otherwise, they're resolved from the environment Zed was started with.

### SQLite Paths {#sqlite-paths}

Relative SQLite paths and `$ZED_WORKTREE_ROOT` are resolved against the worktree root.
Hover over a connection to see the file it opens.
Zed never creates a database file that doesn't exist.

### TLS {#tls}

`ssl_mode` follows the PostgreSQL (libpq) naming:

- `disable`: no TLS.
- `prefer` (default): use TLS when the server supports it, without verifying its certificate.
- `require`: always use TLS, without verifying the server certificate.
- `verify-ca`: also check that the certificate is signed by a trusted authority.
- `verify-full`: also check that the certificate matches the host name.

Set `ssl_root_cert` to a PEM file to trust a private certificate authority, and `ssl_cert` with `ssl_key` to authenticate with a client certificate.

### SSH Tunnels {#ssh-tunnels}

To reach a database through a bastion host, add an `ssh` section.
Zed runs your system's `ssh` command, so your `~/.ssh/config`, SSH agent, and host aliases work as they do in a terminal.

```json [settings]
{
  "database_connections": {
    "staging": {
      "driver": "postgres",
      "host": "db.internal",
      "database": "app",
      "username": "app",
      "environment": "staging",
      "ssh": {
        "host": "bastion.example.com",
        "username": "deploy"
      }
    }
  }
}
```

Connections are always made from the machine running Zed, including in [remote projects](./remote-development.md).
Use an SSH tunnel to reach a database that's only accessible from the remote host.

### Environments and Read-Only Connections {#environments}

Set `environment` to `local` (default), `staging`, or `production`.
Staging and production connections are labeled in the panel and in SQL editors.
Zed asks for confirmation before running statements that modify data or the schema on a production connection, and warns about `UPDATE` and `DELETE` statements without a `WHERE` clause.

Set `read_only` to `true` to open every session as read-only, so that the database rejects writes.

### Project Connections {#project-connections}

Connections can also be defined in a project's `.zed/settings.json`.
They appear in the panel with a folder icon, after the connections from your user settings.

Project connections are only loaded once you [trust the project](./worktree-trust.md), and they default to `read_only: true`.
Passwords for project connections are stored in the keychain separately for each project.

## Browsing Schemas {#browsing-schemas}

Expand a connection to see its schemas, tables, views, and columns.
Double-click a table or view to open its first 100 rows, or right-click it to copy its name or definition.
Right-click a connection to connect, disconnect, open a new SQL file, or edit and delete it.

Use the filter at the top of the panel to find connections, schemas, tables, and columns by name.
Use {#action database_panel::RefreshSchema} after changing the schema outside of Zed.

## Running SQL {#running-sql}

Any file with a `.sql` extension, or with the SQL language, can run queries.
Use {#action database_panel::NewSqlFile} to create one for the selected connection, or choose the connection of an open SQL file with {#action database_panel::SelectConnection} ({#kb database_panel::SelectConnection}) or the connection picker in its toolbar.

- {#action database_panel::RunQuery} ({#kb database_panel::RunQuery}) runs the statement under the cursor, or the selection.
- {#action database_panel::RunSelection} ({#kb database_panel::RunSelection}) runs the selection, or the whole file.
- {#action database_panel::CancelQuery} ({#kb database_panel::CancelQuery}) cancels the running query on the server.
- {#action database_panel::ExplainQuery} ({#kb database_panel::ExplainQuery}) shows the execution plan of the statement under the cursor.
- {#action database_panel::QueryHistory} ({#kb database_panel::QueryHistory}) shows recently executed queries of the connection.

SQL editors complete table and column names from the schema of the selected connection, alongside completions from language servers.

### Inline Results {#inline-results}

{#action database_panel::RunQueryInline} ({#kb database_panel::RunQueryInline}) shows the first rows of a result directly below the statement.
Inline results disappear when you edit the statement, or with {#action database_panel::ClearInlineResults}.

### SQL in Application Code {#embedded-sql}

In other languages, {#action database_panel::RunEmbeddedQuery} ({#kb database_panel::RunEmbeddedQuery}) runs the SQL in the string literal under the cursor and shows its result inline.
When the SQL is highlighted as an injected language, Zed uses that range.

## Results {#results}

Results open in a tab.
Running another query from the same SQL file replaces its result, unless you pin the tab.

Use the buttons in a column header to sort or filter by it, select cells with the mouse or `shift` and the arrow keys, and copy them with `cmd-c` (`ctrl-c` on Linux and Windows).
{#action database_panel::CopyResults} and {#action database_panel::ExportResults} copy or open the whole result as CSV, JSON, or Markdown, with its sorting and filters applied.

Queries stop after `row_limit` rows; use {#action database_panel::LoadMoreRows} to fetch more.

### Editing Rows {#editing-rows}

The rows of a table opened from the panel can be edited when the table has a primary key and the connection isn't read-only.
Double-click a cell or press `enter` to change its value or set it to `NULL`.

Changed cells are highlighted until you commit them with the **Commit…** button or by saving the tab.
Zed shows the `UPDATE` statements before running them in one transaction.
Use **Revert** to discard the changes.
Autosave never writes to the database.

## AI Agents {#ai-agents}

With `agent_access` enabled, Zed provides a `zed-database` [MCP server](./ai/mcp.md) to agents with three tools:

- `db_list_connections`: lists the connections of the project.
- `db_schema`: lists schemas and tables, or describes a table.
- `db_query`: runs one read-only statement and returns up to 200 rows.

Queries run in a read-only transaction that's always rolled back.
The server also provides a `table` prompt to add a table definition to a conversation.

```json [settings]
{
  "database_panel": {
    "agent_access": true
  }
}
```

## Settings {#settings}

```json [settings]
{
  "database_panel": {
    "enabled": false,
    "button": true,
    "dock": "right",
    "default_width": 320,
    "row_limit": 1000,
    "query_timeout_seconds": 0,
    "history_size": 500,
    "agent_access": false
  }
}
```

- `row_limit`: how many rows a query fetches before offering to load more, up to `100000`.
- `query_timeout_seconds`: cancels queries that run longer; `0` disables the timeout.
- `history_size`: how many queries to remember per connection.

## Limitations {#limitations}

- Collaboration guests don't see the host's connections.
- Results are kept in memory; very large results stop at the row limit and at 64 MB.
