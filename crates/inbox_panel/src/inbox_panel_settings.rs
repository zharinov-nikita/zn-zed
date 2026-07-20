use gpui::Pixels;
use settings::RegisterSetting;
pub use settings::{DockSide, Settings};

#[derive(Debug, Clone, Copy, PartialEq, RegisterSetting)]
pub struct InboxPanelSettings {
    pub button: bool,
    pub default_width: Pixels,
    pub dock: DockSide,
    /// Whether to serve the inbox over the embedded localhost MCP server.
    /// Read once at startup — toggling it requires a restart.
    pub mcp_server: bool,
}

impl Settings for InboxPanelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        // Fall back to the documented defaults (matching
        // `assets/settings/default.json`) instead of unwrapping, so a missing
        // section can never panic.
        let panel = content.inbox_panel.as_ref();
        Self {
            button: panel.and_then(|panel| panel.button).unwrap_or(true),
            default_width: panel
                .and_then(|panel| panel.default_width)
                .map_or(gpui::px(300.), gpui::px),
            dock: panel.and_then(|panel| panel.dock).unwrap_or(DockSide::Left),
            mcp_server: panel.and_then(|panel| panel.mcp_server).unwrap_or(true),
        }
    }
}
