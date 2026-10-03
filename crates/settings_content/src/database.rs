use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

use crate::{DockSide, PixelSetting};

#[with_fallible_options]
#[derive(Clone, Default, Serialize, Deserialize, JsonSchema, MergeFrom, Debug, PartialEq)]
pub struct DatabasePanelSettingsContent {
    /// Whether the database panel is available.
    ///
    /// Default: false
    pub enabled: Option<bool>,
    /// Whether to show the database panel button in the status bar.
    ///
    /// Default: true
    pub button: Option<bool>,
    /// Where to dock the database panel.
    ///
    /// Default: right
    pub dock: Option<DockSide>,
    /// Default width of the database panel in pixels.
    ///
    /// Default: 320
    pub default_width: Option<PixelSetting>,
    /// How many rows a query fetches before offering to load more. At most 100000.
    ///
    /// Default: 1000
    pub row_limit: Option<u32>,
    /// How long a statement may run on the server before it is cancelled, in seconds.
    /// 0 disables the timeout.
    ///
    /// Default: 0
    pub query_timeout_seconds: Option<u64>,
    /// How many queries to remember per connection in the query history.
    ///
    /// Default: 500
    pub history_size: Option<u32>,
    /// Whether AI agents can list connections, read schemas and run read-only queries through
    /// the built-in database MCP server. Every tool call still asks for permission.
    ///
    /// Default: false
    pub agent_access: Option<bool>,
}

/// A database connection. Passwords are never stored in settings: they are requested when
/// connecting and kept in the system keychain.
#[with_fallible_options]
#[derive(Clone, Serialize, Deserialize, JsonSchema, MergeFrom, PartialEq)]
#[serde(tag = "driver", rename_all = "snake_case")]
pub enum DatabaseConnectionContent {
    Postgres {
        /// A connection URL such as `postgres://user@host:5432/database`. Shell-style variables
        /// like `${DATABASE_URL}` are resolved from the project environment when connecting.
        /// Fields set explicitly take precedence over the URL.
        url: Option<String>,
        host: Option<String>,
        /// Default: 5432
        port: Option<u16>,
        database: Option<String>,
        username: Option<String>,
        /// Default: prefer
        ssl_mode: Option<DatabaseSslMode>,
        /// Path to a PEM file with the certificate authorities to trust instead of the
        /// system ones.
        ssl_root_cert: Option<String>,
        /// Path to a PEM client certificate, used together with `ssl_key`.
        ssl_cert: Option<String>,
        /// Path to the PEM private key of `ssl_cert`.
        ssl_key: Option<String>,
        /// Connect through an SSH tunnel.
        ssh: Option<DatabaseSshTunnelContent>,
        /// Default: local
        environment: Option<DatabaseEnvironment>,
        /// Open every session as read-only, so that the server rejects writes.
        ///
        /// Default: false, or true for connections defined in project settings.
        read_only: Option<bool>,
    },
    #[serde(alias = "mariadb")]
    Mysql {
        /// A connection URL such as `mysql://user@host:3306/database`. Shell-style variables
        /// like `${DATABASE_URL}` are resolved from the project environment when connecting.
        /// Fields set explicitly take precedence over the URL.
        url: Option<String>,
        host: Option<String>,
        /// Default: 3306
        port: Option<u16>,
        database: Option<String>,
        username: Option<String>,
        /// Default: prefer
        ssl_mode: Option<DatabaseSslMode>,
        /// Path to a PEM file with the certificate authorities to trust in addition to the
        /// built-in ones.
        ssl_root_cert: Option<String>,
        /// Path to a PEM client certificate, used together with `ssl_key`.
        ssl_cert: Option<String>,
        /// Path to the PEM private key of `ssl_cert`.
        ssl_key: Option<String>,
        /// Connect through an SSH tunnel.
        ssh: Option<DatabaseSshTunnelContent>,
        /// Default: local
        environment: Option<DatabaseEnvironment>,
        /// Open every session as read-only, so that the server rejects writes.
        ///
        /// Default: false, or true for connections defined in project settings.
        read_only: Option<bool>,
    },
    Sqlite {
        /// Path to the database file. Relative paths and `$ZED_WORKTREE_ROOT` are resolved
        /// against the worktree root. The file is never created if it doesn't exist.
        path: Option<String>,
        /// Default: local
        environment: Option<DatabaseEnvironment>,
        /// Open the file as read-only.
        ///
        /// Default: false, or true for connections defined in project settings.
        read_only: Option<bool>,
    },
}

impl fmt::Debug for DatabaseConnectionContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DatabaseConnectionContent::Postgres {
                url,
                host,
                port,
                database,
                username,
                ssl_mode,
                ssl_root_cert,
                ssl_cert,
                ssl_key,
                ssh,
                environment,
                read_only,
            }
            | DatabaseConnectionContent::Mysql {
                url,
                host,
                port,
                database,
                username,
                ssl_mode,
                ssl_root_cert,
                ssl_cert,
                ssl_key,
                ssh,
                environment,
                read_only,
            } => f
                .debug_struct(match self {
                    DatabaseConnectionContent::Postgres { .. } => "Postgres",
                    _ => "Mysql",
                })
                .field("url", &url.as_deref().map(redact_url_password))
                .field("host", host)
                .field("port", port)
                .field("database", database)
                .field("username", username)
                .field("ssl_mode", ssl_mode)
                .field("ssl_root_cert", ssl_root_cert)
                .field("ssl_cert", ssl_cert)
                .field("ssl_key", ssl_key)
                .field("ssh", ssh)
                .field("environment", environment)
                .field("read_only", read_only)
                .finish(),
            DatabaseConnectionContent::Sqlite {
                path,
                environment,
                read_only,
            } => f
                .debug_struct("Sqlite")
                .field("path", path)
                .field("environment", environment)
                .field("read_only", read_only)
                .finish(),
        }
    }
}

/// Replaces the password in a URL's user info (`scheme://user:password@host`) with `***`, so
/// that connection strings can be logged.
pub fn redact_url_password(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_end + 3;
    let authority_end = url[authority_start..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |end| authority_start + end);
    let authority = &url[authority_start..authority_end];
    let Some(at) = authority.rfind('@') else {
        return url.to_string();
    };
    let user_info = &authority[..at];
    let Some(colon) = user_info.find(':') else {
        return url.to_string();
    };
    format!(
        "{}{}:***{}",
        &url[..authority_start],
        &user_info[..colon],
        &url[authority_start + at..]
    )
}

/// How TLS is used for a connection. The values follow libpq: `prefer` and `require` encrypt
/// the connection without verifying the server certificate, `verify-ca` also checks that the
/// certificate is signed by a trusted authority, and `verify-full` additionally checks that it
/// matches the host name.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "kebab-case")]
pub enum DatabaseSslMode {
    Disable,
    #[default]
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

/// The environment a connection points to. Shown as a colored label; queries that write to
/// production ask for confirmation.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseEnvironment {
    #[default]
    Local,
    Staging,
    Production,
}

/// An SSH host to tunnel the connection through, using the system `ssh` binary and its
/// configuration.
#[with_fallible_options]
#[derive(Clone, Default, Serialize, Deserialize, JsonSchema, MergeFrom, Debug, PartialEq)]
pub struct DatabaseSshTunnelContent {
    /// The SSH host or an alias from `~/.ssh/config`.
    pub host: Option<String>,
    /// Default: 22
    pub port: Option<u16>,
    pub username: Option<String>,
    /// Path to a private key to authenticate with.
    pub identity_file: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redact_url_password() {
        assert_eq!(
            redact_url_password("postgres://app:hunter2@db.example.com:5432/app?sslmode=require"),
            "postgres://app:***@db.example.com:5432/app?sslmode=require"
        );
        assert_eq!(
            redact_url_password("postgres://app@localhost/app"),
            "postgres://app@localhost/app"
        );
        assert_eq!(
            redact_url_password("postgres://a:b:c@host/db"),
            "postgres://a:***@host/db"
        );
        assert_eq!(redact_url_password("${DATABASE_URL}"), "${DATABASE_URL}");
    }

    #[test]
    fn test_connection_deserialization() {
        let content: DatabaseConnectionContent = serde_json::from_str(
            r#"{ "driver": "mariadb", "host": "localhost", "ssl_mode": "verify-full" }"#,
        )
        .unwrap();
        let DatabaseConnectionContent::Mysql { host, ssl_mode, .. } = &content else {
            panic!("expected a MySQL connection, got {content:?}");
        };
        assert_eq!(host.as_deref(), Some("localhost"));
        assert_eq!(*ssl_mode, Some(DatabaseSslMode::VerifyFull));

        let debug = format!(
            "{:?}",
            DatabaseConnectionContent::Postgres {
                url: Some("postgres://u:secret@h/d".into()),
                host: None,
                port: None,
                database: None,
                username: None,
                ssl_mode: None,
                ssl_root_cert: None,
                ssl_cert: None,
                ssl_key: None,
                ssh: None,
                environment: None,
                read_only: None,
            }
        );
        assert!(!debug.contains("secret"), "{debug}");
    }
}
