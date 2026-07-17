mod inbox_panel_settings;

use std::sync::Arc;

use fs::Fs;
use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, ParentElement, Pixels, Render, Styled, WeakEntity, Window, actions,
};
use ui::prelude::*;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::inbox_panel_settings::{DockSide, InboxPanelSettings, Settings};

actions!(
    inbox_panel,
    [
        /// Toggles focus on the inbox panel.
        ToggleFocus,
    ]
);

const INBOX_PANEL_KEY: &str = "InboxPanel";

pub struct InboxPanel {
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<InboxPanel>(window, cx);
        });
    })
    .detach();
}

impl InboxPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = Self::new(workspace, window, cx);
            panel.update(cx, |_, cx| cx.notify());
            panel
        })
    }

    fn new(
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let fs = workspace.app_state().fs.clone();

        cx.new(|cx| Self {
            fs,
            focus_handle: cx.focus_handle(),
        })
    }
}

impl Panel for InboxPanel {
    fn persistent_name() -> &'static str {
        "Inbox Panel"
    }

    fn panel_key() -> &'static str {
        INBOX_PANEL_KEY
    }

    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        match InboxPanelSettings::get_global(cx).dock {
            DockSide::Left => DockPosition::Left,
            DockSide::Right => DockPosition::Right,
        }
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            let dock = match position {
                DockPosition::Left | DockPosition::Bottom => DockSide::Left,
                DockPosition::Right => DockSide::Right,
            };
            settings.inbox_panel.get_or_insert_default().dock = Some(dock);
        });
    }

    fn default_size(&self, _: &Window, cx: &App) -> Pixels {
        InboxPanelSettings::get_global(cx).default_width
    }

    fn icon(&self, _: &Window, cx: &App) -> Option<IconName> {
        InboxPanelSettings::get_global(cx)
            .button
            .then_some(IconName::Envelope)
    }

    fn icon_tooltip(&self, _window: &Window, _: &App) -> Option<&'static str> {
        Some("Inbox Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        4
    }

    fn hide_button_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        Some(workspace::HideStatusItem::new(|settings| {
            settings.inbox_panel.get_or_insert_default().button = Some(false);
        }))
    }
}

impl Focusable for InboxPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for InboxPanel {}

impl Render for InboxPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("InboxPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .items_center()
            .justify_center()
            .child(Label::new("Inbox"))
    }
}
