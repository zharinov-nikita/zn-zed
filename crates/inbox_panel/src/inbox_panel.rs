pub mod block;
pub mod detail_view;
pub mod inbox_model;
mod inbox_panel_settings;
pub mod inbox_store;
pub mod markdown_codec;
pub mod slash_menu;
mod type_editor;

pub use detail_view::{InboxDetailEvent, InboxDetailView};
pub use inbox_store::{InboxStore, InboxStoreEvent};

use std::{collections::HashSet, path::Path, sync::Arc, time::Duration};

use editor::Editor;
use fs::Fs;
use gpui::{
    Action, AnyElement, App, AppContext as _, AsyncWindowContext, ClickEvent, Context, ElementId,
    Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, ParentElement, Pixels,
    Point, Render, ScrollHandle, Styled, Subscription, Task, WeakEntity, Window, actions, anchored,
    deferred,
};
use theme_settings::ThemeSettings;
use ui::{
    ButtonLike, Checkbox, ContextMenu, ContextMenuEntry, ContextMenuItem, Disclosure, PopoverMenu,
    ScrollAxes, Scrollbars, Tab, TintColor, ToggleButtonGroup, ToggleButtonSimple, ToggleState,
    Tooltip, WithScrollbar, prelude::*,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::inbox_model::{
    InboxItem, ItemId, format_age, now_unix, parse_context, subtask_counts, type_color,
};
use crate::inbox_panel_settings::{DockSide, InboxPanelSettings, Settings};
use crate::type_editor::TypeEditorState;

actions!(
    inbox_panel,
    [
        /// Toggles focus on the inbox panel.
        ToggleFocus,
        /// Captures the text of the capture editor as a new inbox item.
        Capture,
    ]
);

const INBOX_PANEL_KEY: &str = "InboxPanel";

/// How often the item age labels ("2m"/"15h") are refreshed.
const AGE_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// Whether the open items are shown as a flat list or grouped by type.
#[derive(Clone, Copy, PartialEq)]
enum ViewMode {
    /// A flat list of all open items ("All").
    All,
    /// Open items grouped by their resolved type ("By list").
    Grouped,
}

/// Drag payload for moving an inbox item into another group. Doubles as the
/// ghost view that follows the cursor while dragging.
#[derive(Clone)]
struct DraggedInboxItem {
    id: ItemId,
    text: SharedString,
    click_offset: Point<Pixels>,
}

impl Render for DraggedInboxItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui_font = ThemeSettings::get_global(cx).ui_font.clone();
        h_flex()
            .font(ui_font)
            .pl(self.click_offset.x + px(12.))
            .pt(self.click_offset.y + px(12.))
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .max_w(px(240.))
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().element_selected)
                    .child(
                        Label::new(self.text.clone())
                            .size(LabelSize::Small)
                            .truncate(),
                    ),
            )
    }
}

/// Which section of the panel an item row is rendered in.
#[derive(Clone, Copy, PartialEq)]
enum ItemRow {
    /// A not-yet-cleared item in the main list.
    Open,
    /// A cleared item still in the inbox, shown in the archive section.
    ClearedInbox,
    /// An item that has been moved to the archive.
    Archived,
}

pub struct InboxPanel {
    workspace: WeakEntity<Workspace>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    store: Entity<InboxStore>,
    capture_editor: Entity<Editor>,
    /// Type key preselected for the next capture. `None` means no type.
    capture_kind: Option<String>,
    view_mode: ViewMode,
    /// Type keys of the groups collapsed in the grouped view.
    collapsed_groups: HashSet<String>,
    show_archive: bool,
    /// State of the type editor overlay; `Some` while it is open. Mutually
    /// exclusive with `detail`: opening one closes the other.
    type_editor: Option<TypeEditorState>,
    /// The detail view overlay of a single item; `Some` while it is open.
    detail: Option<(Entity<InboxDetailView>, Subscription)>,
    confirming_delete: Option<(ItemId, Point<Pixels>)>,
    scroll_handle: ScrollHandle,
    _age_refresh: Task<()>,
    _subscriptions: Vec<Subscription>,
}

/// Opens the `"path:row"` capture context of an item in the workspace,
/// putting the cursor on the captured line. Shared by the panel's item rows
/// and the detail view's meta line.
pub(crate) fn open_capture_context(
    workspace: &WeakEntity<Workspace>,
    from: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let Some((path, row)) = parse_context(from) else {
        return;
    };
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let Some(project_path) = workspace
        .read(cx)
        .project()
        .read(cx)
        .find_project_path(Path::new(&path), cx)
    else {
        log::warn!("inbox panel: capture context not found in project: {path}");
        return;
    };
    let open_task = workspace.update(cx, |workspace, cx| {
        workspace.open_path(project_path, None, true, window, cx)
    });
    window
        .spawn(cx, async move |cx| {
            let item = open_task.await?;
            if let Some(editor) = item.downcast::<Editor>()
                && let Some(row) = row
            {
                editor
                    .update_in(cx, |editor, window, cx| {
                        editor.go_to_singleton_buffer_point(
                            text::Point::new(row.saturating_sub(1), 0),
                            window,
                            cx,
                        );
                    })
                    .ok();
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
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
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let fs = workspace.app_state().fs.clone();
        let project = workspace.project().clone();
        let weak_workspace = workspace.weak_handle();

        cx.new(|cx| {
            let store = cx.new(|cx| InboxStore::new(project, fs.clone(), cx));
            let capture_editor = cx.new(|cx| {
                let mut editor = Editor::auto_height(1, 5, window, cx);
                editor.set_placeholder_text(
                    "Dump whatever's on your mind about the project…",
                    window,
                    cx,
                );
                editor
            });

            let focus_handle = cx.focus_handle();
            let editor_focus_handle = capture_editor.focus_handle(cx);
            let subscriptions = vec![
                cx.subscribe(&store, Self::handle_store_event),
                cx.on_focus(&focus_handle, window, Self::focus_in),
                // Re-render so the capture box border reflects the editor's
                // focus state.
                cx.on_focus_in(&editor_focus_handle, window, |_, _, cx| cx.notify()),
                cx.on_focus_out(&editor_focus_handle, window, |_, _, _, cx| cx.notify()),
            ];

            let age_refresh = cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(AGE_REFRESH_INTERVAL).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            });

            Self {
                workspace: weak_workspace,
                fs,
                focus_handle,
                store,
                capture_editor,
                capture_kind: None,
                view_mode: ViewMode::All,
                collapsed_groups: HashSet::default(),
                show_archive: false,
                type_editor: None,
                detail: None,
                confirming_delete: None,
                scroll_handle: ScrollHandle::new(),
                _age_refresh: age_refresh,
                _subscriptions: subscriptions,
            }
        })
    }

    fn handle_store_event(
        &mut self,
        _: Entity<InboxStore>,
        event: &InboxStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            InboxStoreEvent::ItemDeleted(id) => {
                if self
                    .confirming_delete
                    .as_ref()
                    .is_some_and(|(confirming_id, _)| confirming_id == id)
                {
                    self.confirming_delete = None;
                }
                // The detail view subscribes to the store itself and emits
                // `Closed` when its item is deleted; nothing to do here.
                cx.notify();
            }
            InboxStoreEvent::Changed => {
                self.reconcile_capture_kind(cx);
                cx.notify();
            }
            InboxStoreEvent::Reloaded => {
                // The file changed externally: the types (and their keys) may
                // be entirely different now, so drop the rename editors
                // rather than trying to reconcile them. The item pending
                // delete confirmation may be gone too, so drop the stale
                // confirmation as well.
                self.type_editor = None;
                self.confirming_delete = None;
                self.reconcile_capture_kind(cx);
                cx.notify();
            }
        }
    }

    /// Drops the preselected capture type when its key no longer exists in
    /// the store's types (e.g. the type was deleted or the file was edited
    /// externally), falling back to "No list".
    fn reconcile_capture_kind(&mut self, cx: &Context<Self>) {
        if let Some(kind) = &self.capture_kind
            && !self
                .store
                .read(cx)
                .types()
                .iter()
                .any(|inbox_type| &inbox_type.key == kind)
        {
            self.capture_kind = None;
        }
    }

    fn focus_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((detail, _)) = &self.detail {
            detail.focus_handle(cx).focus(window, cx);
        } else if self.store.read(cx).has_worktree() {
            self.capture_editor.focus_handle(cx).focus(window, cx);
        }
    }

    /// Opens the detail view of an item as a full-panel overlay, closing the
    /// type editor if it was open.
    fn open_detail(&mut self, id: ItemId, window: &mut Window, cx: &mut Context<Self>) {
        self.type_editor = None;
        self.confirming_delete = None;
        let detail = cx.new(|cx| {
            InboxDetailView::new(self.store.clone(), id, self.workspace.clone(), window, cx)
        });
        let subscription = cx.subscribe_in(
            &detail,
            window,
            |this, _, event: &InboxDetailEvent, window, cx| match event {
                InboxDetailEvent::Closed => {
                    this.detail = None;
                    this.focus_handle.focus(window, cx);
                    cx.notify();
                }
            },
        );
        detail.focus_handle(cx).focus(window, cx);
        self.detail = Some((detail, subscription));
        cx.notify();
    }

    fn capture(&mut self, _: &Capture, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.capture_editor.read(cx).text(cx).trim().to_string();
        if text.is_empty() {
            return;
        }
        let kind = self.capture_kind.clone();
        let from = self.active_editor_context(window, cx);
        self.store.update(cx, |store, cx| {
            store.capture(text, kind, from, cx);
        });
        self.capture_editor
            .update(cx, |editor, cx| editor.set_text("", window, cx));
    }

    /// Returns a `"path:row"` context string for the workspace's active
    /// editor, or `None` if the active item is not a file-backed editor.
    fn active_editor_context(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<String> {
        let workspace = self.workspace.upgrade()?;
        let editor = workspace.read(cx).active_item_as::<Editor>(cx)?;
        if editor == self.capture_editor {
            return None;
        }
        editor.update(cx, |editor, cx| {
            let buffer = editor.buffer().read(cx).as_singleton()?;
            let path = buffer.read(cx).file()?.path().as_unix_str().to_string();
            let snapshot = editor.snapshot(window, cx);
            let row = editor
                .selections
                .newest::<text::Point>(&snapshot)
                .head()
                .row;
            Some(format!("{path}:{}", row + 1))
        })
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_none()
            .h(Tab::container_height(cx))
            .px_2()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::Envelope)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new("Inbox").weight(FontWeight::MEDIUM)),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new(
                            "toggle-archive",
                            if self.show_archive {
                                IconName::EyeOff
                            } else {
                                IconName::Eye
                            },
                        )
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .toggle_state(self.show_archive)
                        .tooltip(Tooltip::text("Show/hide cleared"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_archive = !this.show_archive;
                            cx.notify();
                        })),
                    )
                    .child(
                        IconButton::new("inbox-type-settings", IconName::Settings)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Configure lists"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_type_editor(window, cx);
                            })),
                    ),
            )
    }

    fn render_error_banner(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let store = self.store.read(cx);
        let message = if store.load_error().is_some() {
            "inbox.json is corrupted — fix the file"
        } else if store.save_error().is_some() {
            "Failed to save inbox.json"
        } else {
            return None;
        };
        Some(
            h_flex()
                .flex_none()
                .px_2()
                .py_1()
                .gap_1p5()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::XSmall)
                        .color(Color::Warning),
                )
                .child(
                    Label::new(message)
                        .size(LabelSize::XSmall)
                        .color(Color::Warning),
                ),
        )
    }

    /// A clickable colored square + label chip describing an inbox type,
    /// suitable as a [`PopoverMenu`] trigger.
    fn type_chip_button(
        id: impl Into<ElementId>,
        label: SharedString,
        color: gpui::Hsla,
    ) -> ButtonLike {
        ButtonLike::new(id).size(ButtonSize::None).child(
            h_flex()
                .px_0p5()
                .gap_1()
                .child(div().flex_none().size(px(7.)).rounded_xs().bg(color))
                .child(
                    Label::new(label)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        )
    }

    /// A context menu item with a colored type square, a label, and a check
    /// mark on the currently selected entry.
    fn type_menu_item(
        label: SharedString,
        color: gpui::Hsla,
        selected: bool,
        handler: impl Fn(&mut Window, &mut App) + 'static,
    ) -> ContextMenuItem {
        ContextMenuItem::custom_entry(
            move |_, _| {
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .child(div().flex_none().size(px(7.)).rounded_xs().bg(color))
                    .child(div().flex_1().child(Label::new(label.clone())))
                    .when(selected, |this| {
                        this.child(
                            Icon::new(IconName::Check)
                                .size(IconSize::Small)
                                .color(Color::Accent),
                        )
                    })
                    .into_any_element()
            },
            handler,
            None,
        )
    }

    /// The capture box chip: picks the type preselected for the next capture.
    fn render_capture_type_menu(
        &self,
        chip_label: SharedString,
        chip_color: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let store = self.store.clone();
        let panel = cx.entity().downgrade();
        PopoverMenu::new("inbox-capture-type-menu")
            .trigger(Self::type_chip_button(
                "inbox-capture-type-chip",
                chip_label,
                chip_color,
            ))
            .menu(move |window, cx| {
                let store = store.clone();
                let panel = panel.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, cx| {
                    let capture_kind = panel
                        .upgrade()
                        .and_then(|panel| panel.read(cx).capture_kind.clone());
                    let types: Vec<(String, SharedString, gpui::Hsla)> = store
                        .read(cx)
                        .types()
                        .iter()
                        .map(|inbox_type| {
                            (
                                inbox_type.key.clone(),
                                SharedString::from(inbox_type.label.clone()),
                                type_color(&inbox_type.color, cx),
                            )
                        })
                        .collect();
                    menu = menu.header("Add to list").item(Self::type_menu_item(
                        SharedString::from("No list"),
                        Color::Muted.color(cx),
                        capture_kind.is_none(),
                        {
                            let panel = panel.clone();
                            move |_, cx| {
                                panel
                                    .update(cx, |this, cx| {
                                        this.capture_kind = None;
                                        cx.notify();
                                    })
                                    .ok();
                            }
                        },
                    ));
                    for (key, label, color) in types {
                        let selected = capture_kind.as_deref() == Some(key.as_str());
                        let panel = panel.clone();
                        menu = menu.item(Self::type_menu_item(
                            label,
                            color,
                            selected,
                            move |_, cx| {
                                let key = key.clone();
                                panel
                                    .update(cx, |this, cx| {
                                        this.capture_kind = Some(key);
                                        cx.notify();
                                    })
                                    .ok();
                            },
                        ));
                    }
                    menu.separator().item(
                        ContextMenuEntry::new("Configure lists…")
                            .icon(IconName::Settings)
                            .icon_color(Color::Muted)
                            .handler(move |window, cx| {
                                panel
                                    .update(cx, |this, cx| this.open_type_editor(window, cx))
                                    .ok();
                            }),
                    )
                }))
            })
    }

    /// The item row chip: moves the item into another list or opens the type
    /// editor.
    fn render_item_type_menu(
        &self,
        item_id: ItemId,
        chip_label: SharedString,
        chip_color: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let store = self.store.clone();
        let panel = cx.entity().downgrade();
        PopoverMenu::new(SharedString::from(format!(
            "inbox-item-type-menu-{item_id}"
        )))
        .trigger(Self::type_chip_button(
            SharedString::from(format!("inbox-item-type-chip-{item_id}")),
            chip_label,
            chip_color,
        ))
        .menu(move |window, cx| {
            let store = store.clone();
            let panel = panel.clone();
            let item_id = item_id.clone();
            Some(ContextMenu::build(window, cx, move |mut menu, _, cx| {
                let (current_key, types) = {
                    let store_ref = store.read(cx);
                    let current_key = store_ref
                        .item(&item_id)
                        .and_then(|item| store_ref.resolve_kind(item))
                        .map(|inbox_type| inbox_type.key.clone());
                    let types: Vec<(String, SharedString, gpui::Hsla)> = store_ref
                        .types()
                        .iter()
                        .map(|inbox_type| {
                            (
                                inbox_type.key.clone(),
                                SharedString::from(inbox_type.label.clone()),
                                type_color(&inbox_type.color, cx),
                            )
                        })
                        .collect();
                    (current_key, types)
                };
                for (key, label, color) in types {
                    let selected = current_key.as_deref() == Some(key.as_str());
                    let store = store.clone();
                    let item_id = item_id.clone();
                    menu = menu.item(Self::type_menu_item(
                        label,
                        color,
                        selected,
                        move |_, cx| {
                            store.update(cx, |store, cx| {
                                store.set_kind(&item_id, Some(key.clone()), cx);
                            });
                        },
                    ));
                }
                menu.separator().item(
                    ContextMenuEntry::new("Configure lists…")
                        .icon(IconName::Settings)
                        .icon_color(Color::Muted)
                        .handler(move |window, cx| {
                            panel
                                .update(cx, |this, cx| this.open_type_editor(window, cx))
                                .ok();
                        }),
                )
            }))
        })
    }

    fn render_capture_box(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor_focused = self
            .capture_editor
            .focus_handle(cx)
            .contains_focused(window, cx);
        let border_color = if editor_focused {
            cx.theme().colors().border_focused
        } else {
            cx.theme().colors().border_variant
        };

        let has_types = !self.store.read(cx).types().is_empty();
        let (chip_label, chip_color) = {
            let store = self.store.read(cx);
            match self.capture_kind.as_deref().and_then(|key| {
                store
                    .types()
                    .iter()
                    .find(|inbox_type| inbox_type.key == key)
            }) {
                Some(inbox_type) => (
                    SharedString::from(inbox_type.label.clone()),
                    type_color(&inbox_type.color, cx),
                ),
                None => (SharedString::from("No list"), Color::Muted.color(cx)),
            }
        };

        div().flex_none().p_2().child(
            v_flex()
                // Scopes the `enter -> inbox_panel::Capture` binding (bound
                // as "InboxCapture > Editor" in the default keymaps) to the
                // capture editor only. Binding it as "InboxPanel > Editor"
                // would also match the detail view's title/block editors,
                // because a `>` (descendant) context predicate matches the
                // parent at any ancestor depth and the whole panel renders
                // under `key_context("InboxPanel")`.
                .key_context("InboxCapture")
                .on_action(cx.listener(Self::capture))
                .bg(cx.theme().colors().editor_background)
                .rounded_md()
                .border_1()
                .border_color(border_color)
                .child(div().px_2().pt_1p5().child(self.capture_editor.clone()))
                .child(
                    h_flex()
                        .px_2()
                        .py_1()
                        .when(has_types, |this| this.justify_between())
                        .when(!has_types, |this| this.justify_end())
                        .when(has_types, |this| {
                            this.child(self.render_capture_type_menu(chip_label, chip_color, cx))
                        })
                        .child(
                            Label::new("↵ add")
                                .size(LabelSize::XSmall)
                                .color(Color::Placeholder),
                        ),
                ),
        )
    }

    fn render_empty_state(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_1()
            .p_4()
            .child(
                Icon::new(IconName::Envelope)
                    .size(IconSize::XLarge)
                    .color(Color::Muted),
            )
            .child(
                Label::new("Inbox is empty")
                    .color(Color::Muted)
                    .weight(FontWeight::MEDIUM),
            )
            .child(
                div().text_center().max_w(px(240.)).child(
                    Label::new(
                        "All clear. When a thought strikes, drop it here without leaving your code.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Placeholder),
                ),
            )
    }

    fn render_no_worktree(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .p_4()
            .child(
                div()
                    .text_center()
                    .max_w(px(240.))
                    .child(Label::new("Open a project to use the inbox").color(Color::Muted)),
            )
    }

    fn render_item(
        &self,
        item: &InboxItem,
        row: ItemRow,
        now: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = item.id.clone();
        let is_archive_row = row != ItemRow::Open;
        // The type chip is hidden entirely for unassigned items (no kind, or
        // an unknown/deleted kind) rather than shown with a blank label.
        let type_chip = {
            let store = self.store.read(cx);
            store.resolve_kind(item).map(|inbox_type| {
                (
                    SharedString::from(inbox_type.label.clone()),
                    type_color(&inbox_type.color, cx),
                )
            })
        };

        let checkbox_id = SharedString::from(format!("inbox-checkbox-{}", item.id));
        let checkbox = Checkbox::new(
            checkbox_id,
            if is_archive_row {
                ToggleState::Selected
            } else {
                ToggleState::Unselected
            },
        )
        .fill()
        .on_click(cx.listener({
            let id = id.clone();
            move |this, _, _, cx| {
                this.store.update(cx, |store, cx| match row {
                    ItemRow::Open | ItemRow::ClearedInbox => store.toggle_cleared(&id, cx),
                    ItemRow::Archived => store.restore(&id, cx),
                });
            }
        }));

        let text_label = if is_archive_row {
            Label::new(item.text.clone())
                .color(Color::Disabled)
                .strikethrough()
        } else {
            Label::new(item.text.clone())
        };

        let mut meta = h_flex().flex_wrap().items_center().gap_2();
        if let Some((type_label, type_chip_color)) = type_chip {
            meta = meta.child(self.render_item_type_menu(
                item.id.clone(),
                type_label,
                type_chip_color,
                cx,
            ));
        }
        if let Some(created) = item.created {
            meta = meta.child(
                div()
                    .text_xs()
                    .font_buffer(cx)
                    .text_color(cx.theme().colors().text_placeholder)
                    .child(format_age(created, now)),
            );
        }
        if let Some((done, total)) = item.body.as_deref().and_then(subtask_counts) {
            meta = meta.child(
                h_flex()
                    .gap_0p5()
                    .child(
                        Icon::new(IconName::TodoComplete)
                            .size(IconSize::XSmall)
                            .color(Color::Placeholder),
                    )
                    .child(
                        Label::new(format!("{done}/{total}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Placeholder),
                    ),
            );
        }
        if let Some(from) = item.from.clone() {
            meta = meta.child(
                h_flex()
                    .id(SharedString::from(format!("inbox-item-from-{}", item.id)))
                    .gap_1()
                    .cursor_pointer()
                    .text_xs()
                    .font_buffer(cx)
                    .text_color(cx.theme().colors().text_placeholder)
                    .hover(|style| style.text_color(cx.theme().colors().text_accent))
                    .on_click(cx.listener({
                        let from = from.clone();
                        move |this, _, window, cx| {
                            open_capture_context(&this.workspace, &from, window, cx);
                        }
                    }))
                    .child(
                        Icon::new(IconName::File)
                            .size(IconSize::XSmall)
                            .color(Color::Placeholder),
                    )
                    .child(from),
            );
        }

        let delete_button_id = SharedString::from(format!("inbox-item-delete-{}", item.id));
        let delete_button = IconButton::new(delete_button_id, IconName::Close)
            .icon_size(IconSize::XSmall)
            .icon_color(Color::Muted)
            .visible_on_hover("inbox-item")
            .tooltip(Tooltip::text("Delete"))
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                this.confirming_delete = Some((id.clone(), event.position()));
                cx.notify();
            }));

        h_flex()
            .id(SharedString::from(format!("inbox-item-{}", item.id)))
            .group("inbox-item")
            .items_start()
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded_md()
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .when(row == ItemRow::Open, |this| {
                this.on_drag(
                    DraggedInboxItem {
                        id: item.id.clone(),
                        text: SharedString::from(item.text.clone()),
                        click_offset: Point::default(),
                    },
                    |drag, click_offset, _window, cx| {
                        cx.new(|_| DraggedInboxItem {
                            click_offset,
                            ..drag.clone()
                        })
                    },
                )
            })
            .child(div().flex_none().child(checkbox))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .id(SharedString::from(format!("inbox-item-text-{}", item.id)))
                            .cursor_pointer()
                            .on_click(cx.listener({
                                let id = item.id.clone();
                                move |this, _, window, cx| {
                                    this.open_detail(id.clone(), window, cx);
                                }
                            }))
                            .child(text_label),
                    )
                    .child(meta),
            )
            .child(div().flex_none().child(delete_button))
            .into_any_element()
    }

    fn render_archive_header(&self, count: usize, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("inbox-archive-header")
            .cursor_pointer()
            .px_2()
            .py_1()
            .gap_1()
            .on_click(cx.listener(|this, _, _, cx| {
                this.show_archive = !this.show_archive;
                cx.notify();
            }))
            .child(Disclosure::new(
                "inbox-archive-disclosure",
                self.show_archive,
            ))
            .child(
                Label::new("ARCHIVE")
                    .size(LabelSize::XSmall)
                    .weight(FontWeight::BOLD)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .px_1()
                    .rounded_sm()
                    .bg(cx.theme().colors().element_background)
                    .child(
                        Label::new(count.to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
    }

    /// The "All" / "By list" segmented switch above the item list.
    fn render_view_mode_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_none()
            .mb_1()
            .p(px(2.))
            .bg(cx.theme().colors().editor_background)
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .rounded_md()
            .child(
                ToggleButtonGroup::single_row(
                    "inbox-view-mode",
                    [
                        ToggleButtonSimple::new(
                            "All",
                            cx.listener(|this, _, _, cx| {
                                this.view_mode = ViewMode::All;
                                cx.notify();
                            }),
                        ),
                        ToggleButtonSimple::new(
                            "By list",
                            cx.listener(|this, _, _, cx| {
                                this.view_mode = ViewMode::Grouped;
                                cx.notify();
                            }),
                        ),
                    ],
                )
                .selected_index(match self.view_mode {
                    ViewMode::All => 0,
                    ViewMode::Grouped => 1,
                })
                .label_size(LabelSize::XSmall),
            )
    }

    fn render_group_header(
        &self,
        key: String,
        label: SharedString,
        color: gpui::Hsla,
        count: usize,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .id(SharedString::from(format!("inbox-group-header-{key}")))
            .cursor_pointer()
            .px_2()
            .py_1()
            .gap_1()
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.collapsed_groups.remove(&key) {
                    this.collapsed_groups.insert(key.clone());
                }
                cx.notify();
            }))
            .child(Disclosure::new("inbox-group-disclosure", !collapsed))
            .child(div().flex_none().size(px(7.)).rounded_xs().bg(color))
            .child(
                Label::new(label.to_uppercase())
                    .size(LabelSize::XSmall)
                    .weight(FontWeight::BOLD)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .px_1()
                    .rounded_sm()
                    .bg(cx.theme().colors().element_background)
                    .child(
                        Label::new(count.to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
    }

    /// The `collapsed_groups` entry for the synthetic "Unassigned" group
    /// (items with no kind, or an unknown/deleted kind). Real type keys are
    /// generated as `k{id}` by [`InboxStore::add_type`](crate::inbox_store::InboxStore::add_type),
    /// so this cannot collide with one produced by the UI; a hand-edited
    /// `inbox.json` could in principle define a type with this exact key,
    /// but that's an accepted, self-inflicted edge case.
    const UNASSIGNED_GROUP_KEY: &'static str = "__unassigned__";

    /// Renders the open items grouped by type, in `store.types()` order,
    /// followed by an "Unassigned" group for items with no resolved kind.
    /// Every group is rendered, even an empty one, so it stays a valid drop
    /// target; this is what lets an item be dragged into an empty list.
    fn render_grouped_items(
        &self,
        open: &[InboxItem],
        now: i64,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let groups: Vec<(String, SharedString, gpui::Hsla)> = {
            let store = self.store.read(cx);
            store
                .types()
                .iter()
                .map(|inbox_type| {
                    (
                        inbox_type.key.clone(),
                        SharedString::from(inbox_type.label.clone()),
                        type_color(&inbox_type.color, cx),
                    )
                })
                .collect()
        };
        // Group by the resolved kind. Items with no kind, or an unknown/deleted
        // kind, resolve to `None` and land in the "Unassigned" group below.
        let item_keys: Vec<Option<String>> = {
            let store = self.store.read(cx);
            open.iter()
                .map(|item| store.resolve_kind(item).map(|t| t.key.clone()))
                .collect()
        };

        let mut elements = Vec::new();
        for (key, label, color) in groups {
            let items: Vec<&InboxItem> = open
                .iter()
                .zip(&item_keys)
                .filter(|(_, item_key)| item_key.as_deref() == Some(key.as_str()))
                .map(|(item, _)| item)
                .collect();
            let collapsed = self.collapsed_groups.contains(&key);
            let mut group = v_flex()
                .id(SharedString::from(format!("inbox-group-{key}")))
                .rounded_md()
                .drag_over::<DraggedInboxItem>(|style, _, _, cx| {
                    style.bg(cx.theme().colors().drop_target_background)
                })
                .on_drop(cx.listener({
                    let key = key.clone();
                    move |this, drag: &DraggedInboxItem, _, cx| {
                        this.store.update(cx, |store, cx| {
                            let already_there = store.item(&drag.id).is_some_and(|item| {
                                store.resolve_kind(item).map(|t| t.key.as_str())
                                    == Some(key.as_str())
                            });
                            if !already_there {
                                store.set_kind(&drag.id, Some(key.clone()), cx);
                            }
                        });
                    }
                }))
                .child(self.render_group_header(
                    key.clone(),
                    label,
                    color,
                    items.len(),
                    collapsed,
                    cx,
                ));
            if !collapsed {
                for item in items {
                    group = group.child(self.render_item(item, ItemRow::Open, now, cx));
                }
            }
            elements.push(group.into_any_element());
        }

        let unassigned: Vec<&InboxItem> = open
            .iter()
            .zip(&item_keys)
            .filter(|(_, item_key)| item_key.is_none())
            .map(|(item, _)| item)
            .collect();
        if !unassigned.is_empty() {
            let collapsed = self.collapsed_groups.contains(Self::UNASSIGNED_GROUP_KEY);
            let mut group = v_flex()
                .id("inbox-group-unassigned")
                .rounded_md()
                .drag_over::<DraggedInboxItem>(|style, _, _, cx| {
                    style.bg(cx.theme().colors().drop_target_background)
                })
                .on_drop(cx.listener(|this, drag: &DraggedInboxItem, _, cx| {
                    this.store.update(cx, |store, cx| {
                        let already_unassigned = store
                            .item(&drag.id)
                            .is_some_and(|item| store.resolve_kind(item).is_none());
                        if !already_unassigned {
                            store.set_kind(&drag.id, None, cx);
                        }
                    });
                }))
                .child(self.render_group_header(
                    Self::UNASSIGNED_GROUP_KEY.to_string(),
                    SharedString::from("Unassigned"),
                    cx.theme().colors().border_variant,
                    unassigned.len(),
                    collapsed,
                    cx,
                ));
            if !collapsed {
                for item in unassigned {
                    group = group.child(self.render_item(item, ItemRow::Open, now, cx));
                }
            }
            elements.push(group.into_any_element());
        }

        elements
    }

    fn render_list(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (open, cleared, archived) = {
            let store = self.store.read(cx);
            let (cleared, open): (Vec<_>, Vec<_>) = store
                .items()
                .iter()
                .cloned()
                .partition(InboxItem::is_cleared);
            (open, cleared, store.archived().to_vec())
        };
        let archive_count = cleared.len() + archived.len();
        let now = now_unix();

        let open_rows = match self.view_mode {
            ViewMode::All => open
                .iter()
                .map(|item| self.render_item(item, ItemRow::Open, now, cx))
                .collect(),
            ViewMode::Grouped => self.render_grouped_items(&open, now, cx),
        };
        let mut archive_rows = Vec::new();
        if self.show_archive {
            for item in &cleared {
                archive_rows.push(self.render_item(item, ItemRow::ClearedInbox, now, cx));
            }
            for item in &archived {
                archive_rows.push(self.render_item(item, ItemRow::Archived, now, cx));
            }
        }

        let show_empty_state = open.is_empty() && (archive_count == 0 || !self.show_archive);

        v_flex()
            .id("inbox-list")
            .flex_1()
            .min_h_0()
            .p_1()
            .when(!open.is_empty(), |this| {
                this.child(self.render_view_mode_toggle(cx))
            })
            .when(show_empty_state, |this| {
                this.child(self.render_empty_state(cx))
            })
            .children(open_rows)
            .when(archive_count > 0, |this| {
                this.child(self.render_archive_header(archive_count, cx))
                    .children(archive_rows)
            })
            // Visible, auto-hiding vertical scrollbar matching Zed's other
            // dock panels (see e.g. project_panel/outline_panel). This also
            // takes over the scroll tracking/overflow that used to be set
            // manually via `.overflow_y_scroll().track_scroll(...)`.
            .custom_scrollbars(
                Scrollbars::new(ScrollAxes::Vertical).tracked_scroll_handle(&self.scroll_handle),
                window,
                cx,
            )
    }

    /// The full-panel detail view overlay, or `None` while it is closed.
    fn render_detail_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (detail, _) = self.detail.as_ref()?;
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .bg(cx.theme().colors().panel_background)
                .child(detail.clone())
                .into_any_element(),
        )
    }

    fn render_delete_confirmation(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (id, position) = self.confirming_delete.clone()?;
        Some(
            deferred(
                anchored()
                    .position(position)
                    .anchor(gpui::Anchor::TopLeft)
                    .child(
                        v_flex()
                            .occlude()
                            .elevation_2(cx)
                            .p_2()
                            .gap_2()
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                this.confirming_delete = None;
                                cx.notify();
                            }))
                            .child(Label::new("Delete item?"))
                            .child(
                                h_flex()
                                    .gap_1()
                                    .justify_end()
                                    .child(
                                        Button::new("inbox-delete-cancel", "Cancel")
                                            .style(ButtonStyle::Subtle)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.confirming_delete = None;
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        Button::new("inbox-delete-confirm", "Delete")
                                            .style(ButtonStyle::Tinted(TintColor::Error))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.confirming_delete = None;
                                                this.store.update(cx, |store, cx| {
                                                    store.delete_item(&id, cx)
                                                });
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    ),
            )
            .with_priority(1),
        )
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_worktree = self.store.read(cx).has_worktree();

        v_flex()
            .key_context("InboxPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::capture))
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .children(self.render_error_banner(cx))
            .map(|this| {
                if has_worktree {
                    this.child(self.render_capture_box(window, cx))
                        .child(self.render_list(window, cx))
                } else {
                    this.child(self.render_no_worktree(cx))
                }
            })
            .children(self.render_type_editor(cx))
            .children(self.render_detail_overlay(cx))
            .children(self.render_delete_confirmation(cx))
    }
}
