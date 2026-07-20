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
    /// Whether to show the GitHub issues section (read-only mirror of the
    /// repository's open issues).
    pub github_issues_enabled: bool,
    /// Background refresh interval of the issues list, in minutes.
    pub github_issues_poll_minutes: u64,
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
            github_issues_enabled: panel
                .and_then(|panel| panel.github_issues.as_ref())
                .and_then(|github| github.enabled)
                .unwrap_or(true),
            // Clamped: 0 would busy-loop, and an absurd value would overflow
            // the seconds conversion (see `issues_poll_interval`).
            github_issues_poll_minutes: panel
                .and_then(|panel| panel.github_issues.as_ref())
                .and_then(|github| github.poll_minutes)
                .unwrap_or(5)
                .clamp(1, 24 * 60),
        }
    }
}
