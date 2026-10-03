use std::{
    collections::HashMap,
    fmt,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
use gpui::SharedString;
use settings::{
    DatabaseConnectionContent, DatabaseEnvironment, DatabaseSshTunnelContent, DatabaseSslMode,
    redact_url_password,
};
use url::Url;

/// The kind of database a connection talks to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DriverKind {
    Postgres,
    Mysql,
    Sqlite,
}

impl DriverKind {
    pub fn display_name(self) -> &'static str {
        match self {
            DriverKind::Postgres => "PostgreSQL",
            DriverKind::Mysql => "MySQL",
            DriverKind::Sqlite => "SQLite",
        }
    }

    /// A stable identifier, used in telemetry and settings.
    pub fn id(self) -> &'static str {
        match self {
            DriverKind::Postgres => "postgres",
            DriverKind::Mysql => "mysql",
            DriverKind::Sqlite => "sqlite",
        }
    }

    pub fn default_port(self) -> Option<u16> {
        match self {
            DriverKind::Postgres => Some(5432),
            DriverKind::Mysql => Some(3306),
            DriverKind::Sqlite => None,
        }
    }

    pub fn uses_network(self) -> bool {
        !matches!(self, DriverKind::Sqlite)
    }
}

/// Identifies a connection across windows.
///
/// Connections from project settings are scoped to their worktree root, so that two projects
/// can each define a connection named `local` without sharing sessions or stored passwords.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionKey {
    pub id: Arc<str>,
    pub project_root: Option<Arc<Path>>,
}

impl ConnectionKey {
    pub fn user(id: impl Into<Arc<str>>) -> Self {
        Self {
            id: id.into(),
            project_root: None,
        }
    }

    pub fn project(id: impl Into<Arc<str>>, root: Arc<Path>) -> Self {
        Self {
            id: id.into(),
            project_root: Some(root),
        }
    }

    pub fn is_from_project(&self) -> bool {
        self.project_root.is_some()
    }

    /// The key under which the connection's password is stored in the keychain.
    ///
    /// Project connections include a hash of the worktree root, so that a project can never
    /// make Zed send a password the user saved for an unrelated connection with the same id.
    pub fn credentials_key(&self) -> String {
        match &self.project_root {
            None => format!("zed-database:{}", self.id),
            Some(root) => {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                root.hash(&mut hasher);
                format!("zed-database:{}@{:016x}", self.id, hasher.finish())
            }
        }
    }

    /// A string that identifies the connection in local persistence such as query history.
    pub fn persistence_key(&self) -> String {
        self.credentials_key()
    }
}

impl fmt::Display for ConnectionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshTunnelConfig {
    pub host: String,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub identity_file: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TlsConfig {
    pub mode: DatabaseSslMode,
    pub root_cert: Option<PathBuf>,
    pub client_cert: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
}

/// A connection as configured in settings, before environment variables are resolved.
#[derive(Clone, PartialEq)]
pub struct ConnectionConfig {
    pub key: ConnectionKey,
    pub driver: DriverKind,
    pub url: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub username: Option<String>,
    pub ssl_mode: DatabaseSslMode,
    pub ssl_root_cert: Option<String>,
    pub ssl_cert: Option<String>,
    pub ssl_key: Option<String>,
    pub ssh: Option<SshTunnelConfig>,
    /// SQLite database file.
    pub path: Option<String>,
    pub environment: DatabaseEnvironment,
    pub read_only: bool,
}

impl fmt::Debug for ConnectionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionConfig")
            .field("key", &self.key)
            .field("driver", &self.driver)
            .field("url", &self.url.as_deref().map(redact_url_password))
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("username", &self.username)
            .field("ssl_mode", &self.ssl_mode)
            .field("ssh", &self.ssh)
            .field("path", &self.path)
            .field("environment", &self.environment)
            .field("read_only", &self.read_only)
            .finish()
    }
}

impl ConnectionConfig {
    pub fn from_content(key: ConnectionKey, content: &DatabaseConnectionContent) -> Self {
        // Project connections default to read-only, because their settings are written by
        // whoever wrote the repository rather than by the user.
        let default_read_only = key.is_from_project();
        match content {
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
            } => Self {
                driver: if matches!(content, DatabaseConnectionContent::Postgres { .. }) {
                    DriverKind::Postgres
                } else {
                    DriverKind::Mysql
                },
                key,
                url: url.clone(),
                host: host.clone(),
                port: *port,
                database: database.clone(),
                username: username.clone(),
                ssl_mode: ssl_mode.unwrap_or_default(),
                ssl_root_cert: ssl_root_cert.clone(),
                ssl_cert: ssl_cert.clone(),
                ssl_key: ssl_key.clone(),
                ssh: ssh.as_ref().and_then(ssh_config_from_content),
                path: None,
                environment: environment.unwrap_or_default(),
                read_only: read_only.unwrap_or(default_read_only),
            },
            DatabaseConnectionContent::Sqlite {
                path,
                environment,
                read_only,
            } => Self {
                key,
                driver: DriverKind::Sqlite,
                url: None,
                host: None,
                port: None,
                database: None,
                username: None,
                ssl_mode: DatabaseSslMode::Disable,
                ssl_root_cert: None,
                ssl_cert: None,
                ssl_key: None,
                ssh: None,
                path: path.clone(),
                environment: environment.unwrap_or_default(),
                read_only: read_only.unwrap_or(default_read_only),
            },
        }
    }

    /// The settings representation of this connection, for writing it back to user settings.
    pub fn to_content(&self) -> DatabaseConnectionContent {
        let ssh = self.ssh.as_ref().map(|ssh| DatabaseSshTunnelContent {
            host: Some(ssh.host.clone()),
            port: ssh.port,
            username: ssh.username.clone(),
            identity_file: ssh.identity_file.clone(),
        });
        let environment = Some(self.environment);
        let read_only = Some(self.read_only);
        match self.driver {
            DriverKind::Postgres => DatabaseConnectionContent::Postgres {
                url: self.url.clone(),
                host: self.host.clone(),
                port: self.port,
                database: self.database.clone(),
                username: self.username.clone(),
                ssl_mode: Some(self.ssl_mode),
                ssl_root_cert: self.ssl_root_cert.clone(),
                ssl_cert: self.ssl_cert.clone(),
                ssl_key: self.ssl_key.clone(),
                ssh,
                environment,
                read_only,
            },
            DriverKind::Mysql => DatabaseConnectionContent::Mysql {
                url: self.url.clone(),
                host: self.host.clone(),
                port: self.port,
                database: self.database.clone(),
                username: self.username.clone(),
                ssl_mode: Some(self.ssl_mode),
                ssl_root_cert: self.ssl_root_cert.clone(),
                ssl_cert: self.ssl_cert.clone(),
                ssl_key: self.ssl_key.clone(),
                ssh,
                environment,
                read_only,
            },
            DriverKind::Sqlite => DatabaseConnectionContent::Sqlite {
                path: self.path.clone(),
                environment,
                read_only,
            },
        }
    }

    /// A short description of where the connection points, without any secret.
    pub fn display_target(&self) -> SharedString {
        if self.driver == DriverKind::Sqlite {
            return self.path.clone().unwrap_or_default().into();
        }
        if let Some(url) = &self.url
            && self.host.is_none()
        {
            return redact_url_password(url).into();
        }
        let mut target = self.host.clone().unwrap_or_else(|| "localhost".into());
        if let Some(port) = self.port {
            target.push_str(&format!(":{port}"));
        }
        if let Some(database) = &self.database {
            target.push('/');
            target.push_str(database);
        }
        target.into()
    }

    /// Whether the settings contain a literal password, which is shared with collaborators and
    /// remote hosts along with the settings file.
    pub fn has_password_in_settings(&self) -> bool {
        self.url.as_deref().is_some_and(|url| {
            !contains_variable(url)
                && Url::parse(url).is_ok_and(|url| url.password().is_some_and(|p| !p.is_empty()))
        })
    }

    /// Whether resolving this connection reads environment variables.
    pub fn uses_variables(&self) -> bool {
        [
            &self.url,
            &self.host,
            &self.database,
            &self.username,
            &self.path,
            &self.ssl_root_cert,
            &self.ssl_cert,
            &self.ssl_key,
        ]
        .iter()
        .any(|value| value.as_deref().is_some_and(contains_variable))
    }

    /// Resolves variables and the URL into concrete connection parameters.
    ///
    /// `environment` is used for `${VAR}` references. `worktree_root` anchors relative SQLite
    /// paths and `$ZED_WORKTREE_ROOT`.
    pub fn resolve(
        &self,
        environment: &HashMap<String, String>,
        worktree_root: Option<&Path>,
        password: Option<String>,
        options: &SessionOptions,
    ) -> Result<ResolvedConnection> {
        let expand = |value: &Option<String>| -> Result<Option<String>> {
            value
                .as_deref()
                .map(|value| expand_variables(value, environment, worktree_root))
                .transpose()
        };
        let expand_path = |value: &Option<String>| -> Result<Option<PathBuf>> {
            Ok(expand(value)?.map(|path| absolute_path(&path, worktree_root)))
        };

        let tls = TlsConfig {
            mode: self.ssl_mode,
            root_cert: expand_path(&self.ssl_root_cert)?,
            client_cert: expand_path(&self.ssl_cert)?,
            client_key: expand_path(&self.ssl_key)?,
        };

        if self.driver == DriverKind::Sqlite {
            let path = expand(&self.path)?
                .filter(|path| !path.is_empty())
                .context("SQLite connections need a `path`")?;
            return Ok(ResolvedConnection {
                driver: self.driver,
                host: String::new(),
                port: 0,
                database: None,
                username: None,
                password: None,
                tls,
                ssh: None,
                path: Some(absolute_path(&path, worktree_root)),
                read_only: self.read_only,
                statement_timeout: options.statement_timeout,
            });
        }

        let mut host = None;
        let mut port = None;
        let mut database = None;
        let mut username = None;
        let mut url_password = None;
        let mut tls = tls;
        if let Some(url) = expand(&self.url)?.filter(|url| !url.is_empty()) {
            let parsed = parse_connection_url(self.driver, &url)?;
            host = parsed.host;
            port = parsed.port;
            database = parsed.database;
            username = parsed.username;
            url_password = parsed.password;
            if let Some(mode) = parsed.ssl_mode {
                tls.mode = mode;
            }
        }
        if let Some(value) = expand(&self.host)?.filter(|value| !value.is_empty()) {
            host = Some(value);
        }
        if let Some(value) = self.port {
            port = Some(value);
        }
        if let Some(value) = expand(&self.database)?.filter(|value| !value.is_empty()) {
            database = Some(value);
        }
        if let Some(value) = expand(&self.username)?.filter(|value| !value.is_empty()) {
            username = Some(value);
        }

        Ok(ResolvedConnection {
            driver: self.driver,
            host: host.unwrap_or_else(|| "localhost".into()),
            port: port.or(self.driver.default_port()).unwrap_or_default(),
            database,
            username,
            password: password.or(url_password),
            tls,
            ssh: self.ssh.clone(),
            path: None,
            read_only: self.read_only,
            statement_timeout: options.statement_timeout,
        })
    }
}

fn ssh_config_from_content(content: &DatabaseSshTunnelContent) -> Option<SshTunnelConfig> {
    let host = content.host.clone().filter(|host| !host.is_empty())?;
    Some(SshTunnelConfig {
        host,
        port: content.port,
        username: content.username.clone(),
        identity_file: content.identity_file.clone(),
    })
}

/// Settings that apply to every session, independent of the connection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionOptions {
    pub statement_timeout: Option<Duration>,
}

/// Concrete connection parameters. Contains the password, so it must never be logged or stored.
#[derive(Clone)]
pub struct ResolvedConnection {
    pub driver: DriverKind,
    pub host: String,
    pub port: u16,
    pub database: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub tls: TlsConfig,
    pub ssh: Option<SshTunnelConfig>,
    pub path: Option<PathBuf>,
    pub read_only: bool,
    pub statement_timeout: Option<Duration>,
}

impl fmt::Debug for ResolvedConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedConnection")
            .field("driver", &self.driver)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .field("tls", &self.tls)
            .field("ssh", &self.ssh)
            .field("path", &self.path)
            .field("read_only", &self.read_only)
            .finish()
    }
}

impl ResolvedConnection {
    /// Removes secrets that a driver error message may echo back, such as the password or a
    /// connection URL.
    pub fn sanitize_message(&self, message: &str) -> String {
        let mut message = redact_urls(message);
        if let Some(password) = self
            .password
            .as_deref()
            .filter(|password| password.len() >= 3)
        {
            message = message.replace(password, "***");
        }
        message
    }
}

fn redact_urls(text: &str) -> String {
    text.split_inclusive(char::is_whitespace)
        .map(|word| {
            if word.contains("://") {
                redact_url_password(word)
            } else {
                word.to_string()
            }
        })
        .collect()
}

struct ParsedUrl {
    host: Option<String>,
    port: Option<u16>,
    database: Option<String>,
    username: Option<String>,
    password: Option<String>,
    ssl_mode: Option<DatabaseSslMode>,
}

fn parse_connection_url(driver: DriverKind, url: &str) -> Result<ParsedUrl> {
    let parsed = Url::parse(url).map_err(|error| {
        anyhow!(
            "invalid connection URL {}: {error}",
            redact_url_password(url)
        )
    })?;
    let expected_schemes: &[&str] = match driver {
        DriverKind::Postgres => &["postgres", "postgresql"],
        DriverKind::Mysql => &["mysql", "mariadb"],
        DriverKind::Sqlite => &["sqlite"],
    };
    if !expected_schemes.contains(&parsed.scheme()) {
        bail!(
            "expected a {} URL, got a `{}://` URL",
            driver.display_name(),
            parsed.scheme()
        );
    }
    let decode = |value: &str| -> String {
        percent_encoding::percent_decode_str(value)
            .decode_utf8_lossy()
            .into_owned()
    };
    let database = parsed
        .path()
        .trim_start_matches('/')
        .split('/')
        .next()
        .filter(|database| !database.is_empty())
        .map(decode);
    let ssl_mode = parsed
        .query_pairs()
        .find(|(key, _)| key == "sslmode" || key == "ssl-mode" || key == "ssl_mode")
        .map(
            |(_, value)| match value.to_ascii_lowercase().replace('_', "-").as_str() {
                "disable" | "disabled" => Ok(DatabaseSslMode::Disable),
                "allow" | "prefer" | "preferred" => Ok(DatabaseSslMode::Prefer),
                "require" | "required" => Ok(DatabaseSslMode::Require),
                "verify-ca" => Ok(DatabaseSslMode::VerifyCa),
                "verify-full" | "verify-identity" => Ok(DatabaseSslMode::VerifyFull),
                other => Err(anyhow!("unsupported sslmode `{other}`")),
            },
        )
        .transpose()?;
    Ok(ParsedUrl {
        host: parsed
            .host_str()
            .filter(|host| !host.is_empty())
            .map(|host| {
                host.trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_string()
            }),
        port: parsed.port(),
        database,
        username: Some(parsed.username())
            .filter(|username| !username.is_empty())
            .map(decode),
        password: parsed.password().map(decode),
        ssl_mode,
    })
}

fn contains_variable(value: &str) -> bool {
    value.contains('$') || value.starts_with('~')
}

/// Expands `$VAR`, `${VAR}`, `$ZED_WORKTREE_ROOT` and a leading `~`.
pub fn expand_variables(
    value: &str,
    environment: &HashMap<String, String>,
    worktree_root: Option<&Path>,
) -> Result<String> {
    let home_dir = || {
        std::env::var("HOME")
            .ok()
            .or_else(|| environment.get("HOME").cloned())
    };
    shellexpand::full_with_context(
        value,
        home_dir,
        |name: &str| -> Result<Option<String>, anyhow::Error> {
            if name == "ZED_WORKTREE_ROOT" {
                return worktree_root
                    .map(|root| Some(root.to_string_lossy().into_owned()))
                    .context("`$ZED_WORKTREE_ROOT` is only available in a project");
            }
            match environment.get(name) {
                Some(value) => Ok(Some(value.clone())),
                None => Err(anyhow!("environment variable `{name}` is not set")),
            }
        },
    )
    .map(|expanded| expanded.into_owned())
    .map_err(|error| error.cause)
}

fn absolute_path(path: &str, worktree_root: Option<&Path>) -> PathBuf {
    let path = PathBuf::from(path);
    match worktree_root {
        Some(root) if path.is_relative() => root.join(path),
        _ => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn postgres(content: &str) -> ConnectionConfig {
        let content: DatabaseConnectionContent = serde_json::from_str(content).unwrap();
        ConnectionConfig::from_content(ConnectionKey::user("test"), &content)
    }

    #[test]
    fn test_resolve_url_and_overrides() {
        let config = postgres(
            r#"{ "driver": "postgres", "url": "postgres://app:s%40cret@db.example.com:6543/app_dev?sslmode=verify-full", "database": "override" }"#,
        );
        let resolved = config
            .resolve(&HashMap::default(), None, None, &SessionOptions::default())
            .unwrap();
        assert_eq!(resolved.host, "db.example.com");
        assert_eq!(resolved.port, 6543);
        assert_eq!(resolved.database.as_deref(), Some("override"));
        assert_eq!(resolved.username.as_deref(), Some("app"));
        assert_eq!(resolved.password.as_deref(), Some("s@cret"));
        assert_eq!(resolved.tls.mode, DatabaseSslMode::VerifyFull);
        assert!(config.has_password_in_settings());
        assert!(!format!("{config:?}{resolved:?}").contains("cret"));
    }

    #[test]
    fn test_resolve_variables() {
        let config = postgres(r#"{ "driver": "postgres", "url": "${DATABASE_URL}" }"#);
        assert!(config.uses_variables());
        assert!(!config.has_password_in_settings());

        let error = config
            .resolve(&HashMap::default(), None, None, &SessionOptions::default())
            .unwrap_err();
        assert!(error.to_string().contains("DATABASE_URL"), "{error}");

        let environment = HashMap::from_iter([(
            "DATABASE_URL".to_string(),
            "postgresql://localhost/app".to_string(),
        )]);
        let resolved = config
            .resolve(
                &environment,
                None,
                Some("from keychain".into()),
                &SessionOptions::default(),
            )
            .unwrap();
        assert_eq!(resolved.port, 5432);
        assert_eq!(resolved.database.as_deref(), Some("app"));
        assert_eq!(resolved.password.as_deref(), Some("from keychain"));
    }

    #[test]
    fn test_wrong_scheme_is_rejected() {
        let config = postgres(r#"{ "driver": "postgres", "url": "mysql://localhost/app" }"#);
        assert!(
            config
                .resolve(&HashMap::default(), None, None, &SessionOptions::default())
                .is_err()
        );
    }

    #[test]
    fn test_sqlite_paths() {
        let content: DatabaseConnectionContent = serde_json::from_str(
            r#"{ "driver": "sqlite", "path": "$ZED_WORKTREE_ROOT/db/dev.sqlite3" }"#,
        )
        .unwrap();
        let root = Path::new("/projects/app");
        let config = ConnectionConfig::from_content(
            ConnectionKey::project("fixtures", root.into()),
            &content,
        );
        assert!(config.read_only, "project connections default to read-only");
        let resolved = config
            .resolve(
                &HashMap::default(),
                Some(root),
                None,
                &SessionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            resolved.path.as_deref(),
            Some(Path::new("/projects/app/db/dev.sqlite3"))
        );

        let content: DatabaseConnectionContent =
            serde_json::from_str(r#"{ "driver": "sqlite", "path": "db/dev.sqlite3" }"#).unwrap();
        let config = ConnectionConfig::from_content(ConnectionKey::user("fixtures"), &content);
        assert!(!config.read_only);
        let resolved = config
            .resolve(
                &HashMap::default(),
                Some(root),
                None,
                &SessionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            resolved.path.as_deref(),
            Some(Path::new("/projects/app/db/dev.sqlite3"))
        );
    }

    #[test]
    fn test_credentials_keys_are_scoped() {
        let user = ConnectionKey::user("local");
        let first = ConnectionKey::project("local", Path::new("/a").into());
        let second = ConnectionKey::project("local", Path::new("/b").into());
        assert_eq!(user.credentials_key(), "zed-database:local");
        assert_ne!(first.credentials_key(), user.credentials_key());
        assert_ne!(first.credentials_key(), second.credentials_key());
    }

    #[test]
    fn test_sanitize_message() {
        let resolved = postgres(r#"{ "driver": "postgres", "host": "h" }"#)
            .resolve(
                &HashMap::default(),
                None,
                Some("hunter2".into()),
                &SessionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            resolved.sanitize_message(
                "failed to connect to postgres://app:hunter2@h/db: bad password hunter2"
            ),
            "failed to connect to postgres://app:***@h/db: bad password ***"
        );
    }
}
