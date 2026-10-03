use std::sync::Arc;

use anyhow::Result;
use database_core::{
    ConnectionConfig, ConnectionKey, ConnectionStatus, DatabaseEnvironment, DatabaseSslMode,
    DbStore, DriverKind, PasswordInput, Sessions, SshTunnelConfig,
};
use fs::Fs;
use futures::channel::oneshot;
use gpui::{
    App, AsyncWindowContext, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    ScrollHandle, Task, WeakEntity,
};
use project::Project;
use ui::{
    Button, ButtonSize, ButtonStyle, Color, Label, LabelSize, Modal, ModalFooter, ModalHeader,
    Section, Switch, ToggleState, prelude::*,
};
use ui_input::InputField;
use workspace::{ModalView, Workspace};

/// Adds or edits a connection in user settings.
pub struct ConnectionModal {
    fs: Arc<dyn Fs>,
    project: Entity<Project>,
    /// The connection being edited, if any.
    original: Option<ConnectionConfig>,
    driver: DriverKind,
    environment: DatabaseEnvironment,
    ssl_mode: DatabaseSslMode,
    read_only: bool,
    show_advanced: bool,
    name: Entity<InputField>,
    url: Entity<InputField>,
    host: Entity<InputField>,
    port: Entity<InputField>,
    database: Entity<InputField>,
    username: Entity<InputField>,
    password: Entity<InputField>,
    path: Entity<InputField>,
    ssh_host: Entity<InputField>,
    ssh_username: Entity<InputField>,
    ssl_root_cert: Entity<InputField>,
    ssl_cert: Entity<InputField>,
    ssl_key: Entity<InputField>,
    status: Option<(Color, SharedString)>,
    test_task: Option<Task<()>>,
    scroll_handle: ScrollHandle,
}

impl EventEmitter<DismissEvent> for ConnectionModal {}

impl ModalView for ConnectionModal {}

impl Focusable for ConnectionModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

fn input(
    window: &mut Window,
    cx: &mut Context<ConnectionModal>,
    label: &'static str,
    placeholder: &str,
    value: Option<&str>,
) -> Entity<InputField> {
    let field = cx.new(|cx| {
        InputField::new(window, cx, placeholder)
            .label(label)
            .label_size(LabelSize::Small)
            .tab_stop(true)
    });
    if let Some(value) = value {
        field.update(cx, |field, cx| field.set_text(value, window, cx));
    }
    field
}

impl ConnectionModal {
    pub fn toggle(
        workspace: &mut Workspace,
        original: Option<ConnectionConfig>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let fs = workspace.app_state().fs.clone();
        let project = workspace.project().clone();
        workspace.toggle_modal(window, cx, move |window, cx| {
            Self::new(fs, project, original, window, cx)
        });
    }

    fn new(
        fs: Arc<dyn Fs>,
        project: Entity<Project>,
        original: Option<ConnectionConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let config = original.as_ref();
        fn text(value: Option<&String>) -> Option<&str> {
            value.map(String::as_str)
        }
        let port = config
            .and_then(|config| config.port)
            .map(|port| port.to_string());
        let ssh = config.and_then(|config| config.ssh.as_ref());
        Self {
            name: input(
                window,
                cx,
                "Name",
                "local",
                config.map(|config| config.key.id.as_ref()),
            ),
            url: input(
                window,
                cx,
                "URL",
                "postgres://user@localhost:5432/app or ${DATABASE_URL}",
                text(config.and_then(|config| config.url.as_ref())),
            ),
            host: input(
                window,
                cx,
                "Host",
                "localhost",
                text(config.and_then(|config| config.host.as_ref())),
            ),
            port: input(window, cx, "Port", "5432", port.as_deref()),
            database: input(
                window,
                cx,
                "Database",
                "app_dev",
                text(config.and_then(|config| config.database.as_ref())),
            ),
            username: input(
                window,
                cx,
                "User",
                "app",
                text(config.and_then(|config| config.username.as_ref())),
            ),
            password: cx.new(|cx| {
                InputField::new(window, cx, "Saved in the keychain, not in settings")
                    .label("Password")
                    .label_size(LabelSize::Small)
                    .masked(true)
                    .tab_stop(true)
            }),
            path: input(
                window,
                cx,
                "Database File",
                "db/dev.sqlite3 (relative to the project)",
                text(config.and_then(|config| config.path.as_ref())),
            ),
            ssh_host: input(
                window,
                cx,
                "SSH Host",
                "bastion.example.com (optional)",
                ssh.map(|ssh| ssh.host.as_str()),
            ),
            ssh_username: input(
                window,
                cx,
                "SSH User",
                "deploy (optional)",
                ssh.and_then(|ssh| ssh.username.as_deref()),
            ),
            ssl_root_cert: input(
                window,
                cx,
                "CA Certificate",
                "/path/to/ca.pem (optional)",
                text(config.and_then(|config| config.ssl_root_cert.as_ref())),
            ),
            ssl_cert: input(
                window,
                cx,
                "Client Certificate",
                "/path/to/client.pem (optional)",
                text(config.and_then(|config| config.ssl_cert.as_ref())),
            ),
            ssl_key: input(
                window,
                cx,
                "Client Key",
                "/path/to/client.key (optional)",
                text(config.and_then(|config| config.ssl_key.as_ref())),
            ),
            driver: config.map_or(DriverKind::Postgres, |config| config.driver),
            environment: config.map_or(DatabaseEnvironment::Local, |config| config.environment),
            ssl_mode: config.map_or(DatabaseSslMode::Prefer, |config| config.ssl_mode),
            read_only: config.is_some_and(|config| config.read_only),
            show_advanced: config.is_some_and(|config| {
                config.ssl_root_cert.is_some() || config.ssl_cert.is_some() || config.ssh.is_some()
            }),
            original,
            fs,
            project,
            status: None,
            test_task: None,
            scroll_handle: ScrollHandle::new(),
        }
    }

    fn field_text(field: &Entity<InputField>, cx: &App) -> Option<String> {
        let text = field.read(cx).text(cx).trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    /// The connection described by the form, or a message explaining what's missing.
    fn config(&self, cx: &App) -> Result<ConnectionConfig, SharedString> {
        let name = Self::field_text(&self.name, cx).ok_or("Give the connection a name")?;
        let port = match Self::field_text(&self.port, cx) {
            Some(port) => Some(
                port.parse::<u16>()
                    .map_err(|_| SharedString::from(format!("`{port}` is not a valid port")))?,
            ),
            None => None,
        };
        let url = Self::field_text(&self.url, cx);
        let host = Self::field_text(&self.host, cx);
        let path = Self::field_text(&self.path, cx);
        match self.driver {
            DriverKind::Sqlite if path.is_none() => {
                return Err("Choose the database file".into());
            }
            DriverKind::Postgres | DriverKind::Mysql if url.is_none() && host.is_none() => {
                return Err("Enter a host or a URL".into());
            }
            _ => {}
        }
        let ssh = Self::field_text(&self.ssh_host, cx).map(|host| SshTunnelConfig {
            host,
            port: None,
            username: Self::field_text(&self.ssh_username, cx),
            identity_file: self
                .original
                .as_ref()
                .and_then(|original| original.ssh.as_ref())
                .and_then(|ssh| ssh.identity_file.clone()),
        });
        let network = self.driver.uses_network();
        Ok(ConnectionConfig {
            key: ConnectionKey::user(name),
            driver: self.driver,
            url: url.filter(|_| network),
            host: host.filter(|_| network),
            port: port.filter(|_| network),
            database: Self::field_text(&self.database, cx).filter(|_| network),
            username: Self::field_text(&self.username, cx).filter(|_| network),
            ssl_mode: if network {
                self.ssl_mode
            } else {
                DatabaseSslMode::Disable
            },
            ssl_root_cert: Self::field_text(&self.ssl_root_cert, cx).filter(|_| network),
            ssl_cert: Self::field_text(&self.ssl_cert, cx).filter(|_| network),
            ssl_key: Self::field_text(&self.ssl_key, cx).filter(|_| network),
            ssh: ssh.filter(|_| network),
            path: path.filter(|_| !network),
            environment: self.environment,
            read_only: self.read_only,
        })
    }

    fn password(&self, cx: &App) -> Option<String> {
        let password = self.password.read(cx).text(cx);
        (!password.is_empty()).then_some(password)
    }

    pub(crate) fn test(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let config = match self.config(cx) {
            Ok(config) => config,
            Err(message) => {
                self.status = Some((Color::Error, message));
                cx.notify();
                return;
            }
        };
        if config.has_password_in_settings() {
            self.status = Some((
                Color::Warning,
                "The URL contains a password. Settings are shared with collaborators; use the password field instead.".into(),
            ));
        }
        let password = self.password(cx);
        let test = DbStore::global(cx).update(cx, |store, cx| {
            store.test_connection(config, Some(self.project.clone()), password, cx)
        });
        self.status = Some((Color::Muted, "Connecting…".into()));
        self.test_task = Some(cx.spawn(async move |this, cx| {
            let result = test.await;
            this.update(cx, |this, cx| {
                this.status = Some(match result {
                    Ok(()) => (Color::Success, "Connected successfully".into()),
                    Err(error) if error.is::<database_core::PasswordRequired>() => {
                        (Color::Error, "The server requires a password".into())
                    }
                    Err(error) => (Color::Error, format!("{error:#}").into()),
                });
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub(crate) fn save(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let config = match self.config(cx) {
            Ok(config) => config,
            Err(message) => {
                self.status = Some((Color::Error, message));
                cx.notify();
                return;
            }
        };
        let original_id = self
            .original
            .as_ref()
            .map(|original| original.key.id.clone());
        let renamed = original_id
            .as_ref()
            .is_some_and(|original_id| *original_id != config.key.id);
        let exists = cx
            .global::<settings::SettingsStore>()
            .raw_user_settings()
            .and_then(|settings| settings.content.project.database_connections.as_ref())
            .is_some_and(|connections| connections.contains_key(&config.key.id));
        if exists && (original_id.is_none() || renamed) {
            self.status = Some((
                Color::Error,
                format!("A connection named `{}` already exists", config.key.id).into(),
            ));
            cx.notify();
            return;
        }

        let id = config.key.id.clone();
        let content = config.to_content();
        let remove = original_id.filter(|_| renamed);
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            let connections = settings
                .project
                .database_connections
                .get_or_insert_default();
            if let Some(original_id) = remove {
                connections.remove(&original_id);
            }
            connections.insert(id, content);
        });

        let store = DbStore::global(cx);
        store.update(cx, |store, cx| {
            if let Some(original) = &self.original {
                // Sessions use the old settings until they reconnect.
                store.disconnect(&original.key, cx);
                if renamed && self.password(cx).is_none() {
                    store
                        .move_password(&original.key, &config.key, cx)
                        .detach_and_log_err(cx);
                }
            }
            if let Some(password) = self.password(cx) {
                store
                    .store_password(
                        &config.key,
                        config.username.clone().unwrap_or_default(),
                        password,
                        cx,
                    )
                    .detach_and_log_err(cx);
            }
        });
        telemetry::event!(
            "Database Connection Saved",
            driver = config.driver.id(),
            edited = self.original.is_some()
        );
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn focus_next(&mut self, _: &menu::SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_next(cx);
    }

    fn focus_previous(
        &mut self,
        _: &menu::SelectPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus_prev(cx);
    }

    fn render_choice<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        label: &'static str,
        options: &[(T, &'static str)],
        current: T,
        on_select: fn(&mut Self, T),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(Label::new(label).size(LabelSize::Small))
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .children(options.iter().enumerate().map(|(index, (value, name))| {
                        let value = *value;
                        Button::new((id, index), *name)
                            .size(ButtonSize::Compact)
                            .style(ButtonStyle::Subtle)
                            .toggle_state(value == current)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                on_select(this, value);
                                cx.notify();
                            }))
                    })),
            )
    }
}

impl Render for ConnectionModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let network = self.driver.uses_network();
        let headline = if self.original.is_some() {
            "Edit Connection"
        } else {
            "New Connection"
        };
        let form = v_flex()
            .gap_3()
            .child(self.name.clone())
            .child(self.render_choice(
                "driver",
                "Database",
                &[
                    (DriverKind::Postgres, "PostgreSQL"),
                    (DriverKind::Mysql, "MySQL / MariaDB"),
                    (DriverKind::Sqlite, "SQLite"),
                ],
                self.driver,
                |this, driver| this.driver = driver,
                cx,
            ))
            .when(network, |form| {
                form.child(self.url.clone())
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(self.host.clone()))
                            .child(div().w_24().child(self.port.clone())),
                    )
                    .child(self.database.clone())
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(self.username.clone()))
                            .child(div().flex_1().child(self.password.clone())),
                    )
                    .child(self.render_choice(
                        "ssl-mode",
                        "TLS",
                        &[
                            (DatabaseSslMode::Disable, "Disable"),
                            (DatabaseSslMode::Prefer, "Prefer"),
                            (DatabaseSslMode::Require, "Require"),
                            (DatabaseSslMode::VerifyCa, "Verify CA"),
                            (DatabaseSslMode::VerifyFull, "Verify Full"),
                        ],
                        self.ssl_mode,
                        |this, mode| this.ssl_mode = mode,
                        cx,
                    ))
            })
            .when(!network, |form| form.child(self.path.clone()))
            .child(self.render_choice(
                "environment",
                "Environment",
                &[
                    (DatabaseEnvironment::Local, "Local"),
                    (DatabaseEnvironment::Staging, "Staging"),
                    (DatabaseEnvironment::Production, "Production"),
                ],
                self.environment,
                |this, environment| this.environment = environment,
                cx,
            ))
            .child(
                Switch::new("read-only", ToggleState::from(self.read_only))
                    .label("Read-only: the server rejects writes")
                    .on_click(cx.listener(|this, state: &ToggleState, _, cx| {
                        this.read_only = state.selected();
                        cx.notify();
                    })),
            )
            .when(network, |form| {
                form.child(
                    Button::new(
                        "advanced",
                        if self.show_advanced {
                            "Hide SSH and certificates"
                        } else {
                            "SSH tunnel and certificates…"
                        },
                    )
                    .style(ButtonStyle::Transparent)
                    .size(ButtonSize::Compact)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_advanced = !this.show_advanced;
                        cx.notify();
                    })),
                )
                .when(self.show_advanced, |form| {
                    form.child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(self.ssh_host.clone()))
                            .child(div().flex_1().child(self.ssh_username.clone())),
                    )
                    .child(self.ssl_root_cert.clone())
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(self.ssl_cert.clone()))
                            .child(div().flex_1().child(self.ssl_key.clone())),
                    )
                })
            });

        v_flex()
            .key_context("DatabaseConnectionModal")
            .elevation_3(cx)
            .w(rems(36.))
            .tab_group()
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::focus_next))
            .on_action(cx.listener(Self::focus_previous))
            .child(
                Modal::new("database-connection", Some(self.scroll_handle.clone()))
                    .header(
                        ModalHeader::new()
                            .headline(headline)
                            .description("Saved in your settings. Passwords go to the keychain."),
                    )
                    .section(Section::new().child(form))
                    .footer(
                        ModalFooter::new()
                            .start_slot::<Label>(self.status.clone().map(|(color, message)| {
                                Label::new(message).size(LabelSize::Small).color(color)
                            }))
                            .end_slot(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Button::new("test", "Test")
                                            .style(ButtonStyle::Subtle)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.test(window, cx)
                                            })),
                                    )
                                    .child(Button::new("cancel", "Cancel").on_click(cx.listener(
                                        |this, _, window, cx| {
                                            this.cancel(&menu::Cancel, window, cx)
                                        },
                                    )))
                                    .child(
                                        Button::new("save", "Save")
                                            .style(ButtonStyle::Filled)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.save(&menu::Confirm, window, cx)
                                            })),
                                    ),
                            ),
                    ),
            )
    }
}

/// Asks for a connection's password and connects with it.
pub struct PasswordModal {
    config: ConnectionConfig,
    project: Entity<Project>,
    password: Entity<InputField>,
    remember: bool,
    error: Option<SharedString>,
    connecting: Option<Task<()>>,
    done: Option<oneshot::Sender<Result<Arc<Sessions>>>>,
}

impl EventEmitter<DismissEvent> for PasswordModal {}

impl ModalView for PasswordModal {}

impl Focusable for PasswordModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.password.focus_handle(cx)
    }
}

impl PasswordModal {
    fn new(
        config: ConnectionConfig,
        project: Entity<Project>,
        error: Option<String>,
        done: oneshot::Sender<Result<Arc<Sessions>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let password = cx.new(|cx| {
            InputField::new(window, cx, "Password")
                .masked(true)
                .tab_stop(true)
        });
        Self {
            config,
            project,
            password,
            remember: true,
            error: error.map(Into::into),
            connecting: None,
            done: Some(done),
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        if self.connecting.is_some() {
            return;
        }
        let password = self.password.read(cx).text(cx);
        let connect = DbStore::global(cx).update(cx, |store, cx| {
            store.connect(
                self.config.clone(),
                Some(self.project.clone()),
                Some(PasswordInput {
                    password,
                    remember: self.remember,
                }),
                cx,
            )
        });
        let key = self.config.key.clone();
        self.error = None;
        self.connecting = Some(cx.spawn(async move |this, cx| {
            let result = connect.await;
            this.update(cx, |this, cx| {
                this.connecting = None;
                match result {
                    Ok(sessions) => {
                        if let Some(done) = this.done.take() {
                            done.send(Ok(sessions)).ok();
                        }
                        cx.emit(DismissEvent);
                    }
                    Err(error) => {
                        let password_rejected = DbStore::global(cx).read(cx).status(&key)
                            == ConnectionStatus::PasswordRequired;
                        this.error = Some(if password_rejected {
                            "The password was rejected".into()
                        } else {
                            format!("{error:#}").into()
                        });
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(done) = self.done.take() {
            done.send(Err(anyhow::anyhow!("no password was entered")))
                .ok();
        }
        cx.emit(DismissEvent);
    }
}

impl Render for PasswordModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let target = match &self.config.username {
            Some(username) => format!("{username} @ {}", self.config.display_target()),
            None => self.config.display_target().to_string(),
        };
        v_flex()
            .key_context("DatabasePasswordModal")
            .elevation_3(cx)
            .w(rems(28.))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .child(
                Modal::new("database-password", None)
                    .header(
                        ModalHeader::new()
                            .headline(format!("Password for {}", self.config.key.id))
                            .description(target),
                    )
                    .section(
                        Section::new().child(
                            v_flex().gap_2().child(self.password.clone()).child(
                                Switch::new("remember", ToggleState::from(self.remember))
                                    .label("Save in the keychain")
                                    .on_click(cx.listener(|this, state: &ToggleState, _, cx| {
                                        this.remember = state.selected();
                                        cx.notify();
                                    })),
                            ),
                        ),
                    )
                    .footer(
                        ModalFooter::new()
                            .start_slot::<Label>(self.error.clone().map(|error| {
                                Label::new(error).size(LabelSize::Small).color(Color::Error)
                            }))
                            .end_slot(
                                Button::new(
                                    "connect",
                                    if self.connecting.is_some() {
                                        "Connecting…"
                                    } else {
                                        "Connect"
                                    },
                                )
                                .style(ButtonStyle::Filled)
                                .disabled(self.connecting.is_some())
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.confirm(&menu::Confirm, window, cx),
                                )),
                            ),
                    ),
            )
    }
}

impl Drop for PasswordModal {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            done.send(Err(anyhow::anyhow!("no password was entered")))
                .ok();
        }
    }
}

/// Connects, asking for a password if the server needs one that isn't stored.
pub fn connect_interactively(
    workspace: WeakEntity<Workspace>,
    config: ConnectionConfig,
    project: Entity<Project>,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<Arc<Sessions>>> {
    let store = DbStore::global(cx);
    let connect = store.update(cx, |store, cx| {
        store.ensure_connected(config.clone(), Some(project.clone()), cx)
    });
    window.spawn(cx, async move |cx: &mut AsyncWindowContext| {
        let error = match connect.await {
            Ok(sessions) => return Ok(sessions),
            Err(error) => error,
        };
        let password_required = store.read_with(cx, |store, _| {
            store.status(&config.key) == ConnectionStatus::PasswordRequired
        });
        if !password_required {
            return Err(error);
        }
        let message = error
            .to_string()
            .strip_prefix("authentication failed: ")
            .map(str::to_string);
        let (done, result) = oneshot::channel();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_modal(window, cx, move |window, cx| {
                PasswordModal::new(config, project, message, done, window, cx)
            });
        })?;
        result
            .await
            .map_err(|_| anyhow::anyhow!("the password prompt was closed"))?
    })
}

impl ConnectionModal {
    #[cfg(test)]
    pub(crate) fn set_field(&self, field: &str, value: &str, window: &mut Window, cx: &mut App) {
        let input = match field {
            "name" => &self.name,
            "host" => &self.host,
            "port" => &self.port,
            "database" => &self.database,
            "username" => &self.username,
            "password" => &self.password,
            "path" => &self.path,
            "url" => &self.url,
            _ => panic!("unknown field {field}"),
        };
        input.update(cx, |input, cx| input.set_text(value, window, cx));
    }

    #[cfg(test)]
    pub(crate) fn set_driver(&mut self, driver: DriverKind) {
        self.driver = driver;
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> Option<SharedString> {
        self.status.as_ref().map(|(_, message)| message.clone())
    }
}
