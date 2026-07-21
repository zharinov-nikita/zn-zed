//! Bridge between the inbox panel and the agent panel: everything the chat
//! needs in order to treat an inbox task as an attachment lives here, so the
//! inbox-specific code isn't scattered across the message editor, the
//! completion provider and the thread view.
//!
//! A task mention only stores identity (`project_key` + item id), never a copy
//! of the task: the title is re-read on every render and the body on every
//! send, so the attachment keeps tracking the live task.

use anyhow::{Context as _, Result};
use gpui::Entity;
use inbox_panel::inbox_model::ItemId;
use inbox_panel::inbox_store::InboxStoreRegistry;
use inbox_panel::{InboxPanel, InboxStore, item_markdown};
use ui::prelude::*;
use workspace::Workspace;

/// The live store of the project the mention points at, if that project is
/// still open in this process.
pub fn inbox_store_for_project(project_key: &str, cx: &mut App) -> Option<Entity<InboxStore>> {
    InboxStoreRegistry::live_stores(cx)
        .into_iter()
        .find(|store| store.read(cx).bound_project_key() == Some(project_key))
}

/// What a task mention shows on its chip. Resolved on every render, so a task
/// renamed, processed or deleted in the panel is reflected in the chat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxItemState {
    Open,
    /// Processed (checked off) but still in the inbox document.
    Cleared,
    /// The project is open, but it has no item with this id — deleted, or
    /// archived out of the document.
    Missing,
    /// The owning project isn't open, so nothing can be said about the task.
    Unresolved,
}

/// The chip label and state for a task mention. `None` for the label means the
/// caller should fall back to the name stored in the URI.
pub fn inbox_item_summary(
    project_key: &str,
    id: &str,
    cx: &mut App,
) -> (Option<String>, InboxItemState) {
    let Some(store) = inbox_store_for_project(project_key, cx) else {
        return (None, InboxItemState::Unresolved);
    };
    let item_id: ItemId = id.into();
    let store = store.read(cx);
    let Some(item) = store.item(&item_id) else {
        return (None, InboxItemState::Missing);
    };
    let state = if item.is_cleared() {
        InboxItemState::Cleared
    } else {
        InboxItemState::Open
    };
    // The same collapsed display string the panel's rows show — markdown links
    // in the title render as their label there too.
    (
        Some(inbox_panel::parse_title_links(&item.text).0),
        state,
    )
}

/// One open task of this window's inbox, as offered by the `@` menu.
#[derive(Clone, Debug)]
pub struct InboxTask {
    pub project_key: SharedString,
    pub id: SharedString,
    pub title: SharedString,
}

/// The open (unprocessed) tasks of the inbox panel in `workspace`, in the
/// panel's stored order. Empty when the window has no inbox panel or no
/// project bound to it.
pub fn open_inbox_tasks(workspace: &Entity<Workspace>, cx: &mut App) -> Vec<InboxTask> {
    let Some(panel) = workspace.read(cx).panel::<InboxPanel>(cx) else {
        return Vec::new();
    };
    let store = panel.read(cx).store().clone();
    let store = store.read(cx);
    let Some(project_key) = store.bound_project_key().map(SharedString::from) else {
        return Vec::new();
    };
    store
        .items()
        .iter()
        .filter(|item| !item.is_cleared())
        .map(|item| InboxTask {
            project_key: project_key.clone(),
            id: item.id.to_string().into(),
            title: inbox_panel::parse_title_links(&item.text).0.into(),
        })
        .collect()
}

/// The attachment card for a task mention: title over the task's own meta
/// row. Rendered inside a block crease, so its height is fixed in editor
/// lines — long titles are truncated; clicking the card opens the task in the
/// inbox panel's detail view, the one place tasks are edited.
pub fn render_inbox_card(
    project_key: &str,
    id: &str,
    fallback: &SharedString,
    is_selected: bool,
    workspace: Option<gpui::WeakEntity<Workspace>>,
    cx: &mut App,
) -> AnyElement {
    // Title and state come from the same resolver the dedup and pruning
    // paths use, so the card can't disagree with them about a task's fate.
    let (title, state) = inbox_item_summary(project_key, id, cx);
    let title = Some(title.unwrap_or_else(|| fallback.to_string()))
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| "(untitled)".to_string());
    let store = inbox_store_for_project(project_key, cx);
    let item = store.as_ref().and_then(|store| {
        let item_id: ItemId = id.into();
        store.read(cx).item(&item_id).cloned()
    });

    let colors = cx.theme().colors();
    let border_color = if is_selected {
        colors.border_focused
    } else {
        colors.border_variant
    };

    // Fills its block completely (`size_full`): the block's height is fixed in
    // editor lines, and any unfilled remainder would look like a dead margin
    // that still hit-tests as the card.
    let mut card = v_flex()
        .id(SharedString::from(format!("inbox-card-{id}")))
        .size_full()
        .justify_center()
        .px_2()
        .py_1()
        .gap_1()
        // The block's height is fixed; when the meta row's chips are taller
        // than the leftover space they must clip at the card's edge, not
        // spill past its border.
        .overflow_hidden()
        .rounded_sm()
        .border_1()
        .border_color(border_color)
        .bg(colors.element_background)
        .when(state == InboxItemState::Cleared, |this| this.opacity(0.6))
        .tooltip(ui::Tooltip::text(title.clone()))
        .when_some(workspace, |this, workspace| {
            let id = id.to_string();
            this.cursor_pointer()
                .hover(|style| style.bg(colors.element_hover))
                .on_click(move |_, window, cx| {
                    let Some(workspace) = workspace.upgrade() else {
                        return;
                    };
                    workspace.update(cx, |workspace, cx| {
                        open_inbox_item(workspace, &id, window, cx);
                    });
                })
        })
        .child(
            h_flex()
                .w_full()
                .gap_1p5()
                .items_center()
                .child(
                    Icon::new(IconName::InboxTray)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    div().min_w_0().flex_1().child(
                        Label::new(title)
                            .size(LabelSize::Small)
                            .truncate()
                            .when(state == InboxItemState::Missing, |this| {
                                this.strikethrough().color(Color::Muted)
                            }),
                    ),
                )
                .when(state != InboxItemState::Open, |this| {
                    this.child(
                        Label::new(match state {
                            InboxItemState::Cleared => "done",
                            InboxItemState::Missing => "deleted",
                            _ => "inbox not open",
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
                }),
        );

    if let (Some(store), Some(item)) = (&store, &item) {
        card = card.child(div().pl(px(16.)).child(inbox_panel::item_meta_row(
            store, item, cx,
        )));
    }

    card.into_any_element()
}

/// Focuses the inbox panel on the task a mention points at — the one place
/// tasks are edited. Does nothing when this window has no inbox panel or the
/// task is gone.
pub fn open_inbox_item(
    workspace: &mut Workspace,
    id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<InboxPanel>(cx) else {
        return;
    };
    workspace.focus_panel::<InboxPanel>(window, cx);
    let item_id: ItemId = id.into();
    panel.update(cx, |panel, cx| panel.open_item(item_id, window, cx));
}

/// The task's Markdown as it goes to the model, followed by the pointer that
/// lets the agent read and edit the live task through the inbox MCP server.
pub fn inbox_item_content(project_key: &str, id: &str, cx: &mut App) -> Result<String> {
    let store = inbox_store_for_project(project_key, cx).context(
        "The inbox this task belongs to is not open. Open its project to attach the task.",
    )?;
    let item_id: ItemId = id.into();
    let item = store
        .read(cx)
        .item(&item_id)
        .cloned()
        .context("This task is no longer in the inbox.")?;
    let markdown = item_markdown(&store, &item, cx);
    let project = store
        .read(cx)
        .worktree_root(cx)
        .map(|root| root.to_string_lossy().into_owned())
        .unwrap_or_else(|| project_key.to_string());
    Ok(format!(
        "{markdown}\n\nInbox item id: {id} (project {project}).\n\
         Read the current version with the `inbox_get_item` tool and edit it with \
         `inbox_update_item`; the user sees those edits in the inbox panel immediately.",
    ))
}
