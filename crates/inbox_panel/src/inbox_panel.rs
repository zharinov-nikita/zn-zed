mod inbox_panel_settings;

use std::sync::Arc;

use anyhow::Context as _;
use db::kvp::KeyValueStore;
use fs::Fs;
use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, ParentElement, Pixels, Render, Styled, Task, WeakEntity, Window,
    actions,
};
use serde::{Deserialize, Serialize};
use ui::prelude::*;
use util::{ResultExt, TryFutureExt};
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
    workspace: WeakEntity<Workspace>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    width: Option<Pixels>,
    pending_serialization: Task<Option<()>>,
}

#[derive(Serialize, Deserialize)]
struct SerializedInboxPanel {
    width: Option<Pixels>,
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
        let serialized_panel = match workspace
            .read_with(&cx, |workspace, _| Self::serialization_key(workspace))
            .ok()
            .flatten()
        {
            Some(serialization_key) => {
                let kvp = cx.update(|_, cx| KeyValueStore::global(cx))?;
                cx.background_spawn(async move { kvp.read_kvp(&serialization_key) })
                    .await
                    .context("loading inbox panel")
                    .log_err()
                    .flatten()
                    .map(|panel| serde_json::from_str::<SerializedInboxPanel>(&panel))
                    .transpose()
                    .log_err()
                    .flatten()
            }
            None => None,
        };

        workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = Self::new(workspace, serialized_panel.as_ref(), window, cx);
            panel.update(cx, |_, cx| cx.notify());
            panel
        })
    }

    fn new(
        workspace: &mut Workspace,
        serialized: Option<&SerializedInboxPanel>,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let fs = workspace.app_state().fs.clone();
        let workspace_handle = cx.entity().downgrade();

        cx.new(|cx| Self {
            workspace: workspace_handle,
            fs,
            focus_handle: cx.focus_handle(),
            width: serialized.and_then(|serialized| serialized.width),
            pending_serialization: Task::ready(None),
        })
    }

    fn serialization_key(workspace: &Workspace) -> Option<String> {
        workspace
            .database_id()
            .map(|id| i64::from(id).to_string())
            .or(workspace.session_id())
            .map(|id| format!("{}-{:?}", INBOX_PANEL_KEY, id))
    }

    fn serialize(&mut self, cx: &mut Context<Self>) {
        let Some(serialization_key) = self
            .workspace
            .read_with(cx, |workspace, _| Self::serialization_key(workspace))
            .ok()
            .flatten()
        else {
            return;
        };
        let width = self.width;
        let kvp = KeyValueStore::global(cx);
        self.pending_serialization = cx.background_spawn(
            async move {
                kvp.write_kvp(
                    serialization_key,
                    serde_json::to_string(&SerializedInboxPanel { width })?,
                )
                .await?;
                anyhow::Ok(())
            }
            .log_err(),
        );
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

    fn set_active(&mut self, _active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        self.serialize(cx);
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
