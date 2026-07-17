use gpui::Pixels;
use settings::RegisterSetting;
pub use settings::{DockSide, Settings};

#[derive(Debug, Clone, Copy, PartialEq, RegisterSetting)]
pub struct InboxPanelSettings {
    pub button: bool,
    pub default_width: Pixels,
    pub dock: DockSide,
}

impl Settings for InboxPanelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let panel = content.inbox_panel.as_ref().unwrap();
        Self {
            button: panel.button.unwrap(),
            default_width: panel.default_width.map(gpui::px).unwrap(),
            dock: panel.dock.unwrap(),
        }
    }
}
