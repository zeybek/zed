use std::time::Duration;

use gpui::Pixels;
use settings::{DockSide, IntoGpui as _, RegisterSetting, Settings};

/// The most rows a single query result can hold, however often more rows are loaded.
pub const MAX_RESULT_ROWS: usize = 100_000;

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct DatabaseSettings {
    pub enabled: bool,
    pub button: bool,
    pub dock: DockSide,
    pub default_width: Pixels,
    pub row_limit: usize,
    pub query_timeout: Option<Duration>,
    pub history_size: usize,
    pub agent_access: bool,
}

impl Settings for DatabaseSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let panel = content.database_panel.as_ref().unwrap();
        Self {
            enabled: panel.enabled.unwrap(),
            button: panel.button.unwrap(),
            dock: panel.dock.unwrap(),
            default_width: panel.default_width.unwrap().into_gpui(),
            row_limit: (panel.row_limit.unwrap() as usize).clamp(1, MAX_RESULT_ROWS),
            query_timeout: Some(panel.query_timeout_seconds.unwrap())
                .filter(|seconds| *seconds > 0)
                .map(Duration::from_secs),
            history_size: panel.history_size.unwrap() as usize,
            agent_access: panel.agent_access.unwrap(),
        }
    }
}
