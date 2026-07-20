pub mod attachment;
pub mod block;
pub mod detail_view;
pub mod inbox_model;
mod inbox_panel_settings;
pub mod inbox_store;
pub mod markdown_codec;
pub mod slash_menu;
mod type_editor;

pub use detail_view::{InboxDetailEvent, InboxDetailView};
pub use inbox_panel_settings::InboxPanelSettings;
pub use inbox_store::{InboxStore, InboxStoreEvent};

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use agent_ui::AgentPanel;
use collections::{HashMap, HashSet};
use editor::Editor;
use fs::Fs;
use gpui::{
    Action, AnyElement, App, AppContext as _, AsyncWindowContext, ClickEvent, ClipboardItem,
    Context, DismissEvent, Div, ElementId, Entity, EventEmitter, ExternalPaths, FocusHandle,
    Focusable, FontWeight, IntoElement, ParentElement, Pixels, Point, Render, ScrollHandle,
    Stateful, Styled, Subscription, Task, WeakEntity, Window, actions, anchored, deferred,
};
use project::DirectoryLister;
use theme_settings::ThemeSettings;
use ui::{
    ButtonLike, Checkbox, ContextMenu, ContextMenuEntry, ContextMenuItem, Disclosure, IconPosition,
    PopoverMenu, ScrollAxes, Scrollbars, Tab, TintColor, ToggleState, Tooltip, WithScrollbar,
    prelude::*,
};
use workspace::{
    DraggedText, Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};

use util::paths::PathWithPosition;

use crate::attachment::{
    AttachmentCompletionProvider, AttachmentSet, OnPick, attach_external_paths, pick_and_attach,
};
use crate::inbox_model::{
    AttachmentRef, InboxFile, InboxItem, ItemId, MetaField, SortMode, catalog_color, format_age,
    item_to_markdown, now_unix, subtask_counts,
};
use crate::inbox_panel_settings::{DockSide, Settings};
use crate::type_editor::TypeEditorState;

actions!(
    inbox_panel,
    [
        /// Toggles focus on the inbox panel.
        ToggleFocus,
        /// Captures the text of the capture editor as a new inbox item.
        Capture,
        /// Exports the inbox to a JSON file.
        ExportInbox,
        /// Imports items from an exported inbox JSON file, merging them into
        /// the current inbox.
        ImportInbox,
    ]
);

const INBOX_PANEL_KEY: &str = "InboxPanel";

/// How often the item age labels ("2m"/"15h") are refreshed.
const AGE_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// Line height (in rems) of an item row's title. Chosen to comfortably fit the
/// 20px checkbox so it centers on the first line without over-spacing the text;
/// independent of the editor's `buffer_line_height`, which governs body text,
/// not this UI label.
const ITEM_LINE_HEIGHT: f32 = 1.3;

/// Whether the open items are shown as a flat list or grouped by type.
#[derive(Clone, Copy, PartialEq)]
enum ViewMode {
    /// A flat list of all open items ("All").
    All,
    /// Open items grouped by their resolved type ("By list").
    Grouped,
}

/// Drag payload for moving an inbox item into another group. Doubles as the
/// ghost view that follows the cursor while dragging. The drag payload itself
/// is a shared [`DraggedText`] (so the agent panel can accept it); this struct
/// only renders the floating preview.
#[derive(Clone)]
struct InboxItemGhost {
    text: SharedString,
    click_offset: Point<Pixels>,
}

impl Render for InboxItemGhost {
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
    /// Attachments staged for the next capture (added via `@` or the file
    /// picker). Drained into the item when it is captured.
    capture_attachments: Entity<AttachmentSet>,
    /// Type key preselected for the next capture. `None` means no type.
    capture_kind: Option<String>,
    /// Tag keys staged for the next capture, drained into the item when it is
    /// captured (like attachments, unlike the sticky `capture_kind`).
    capture_tags: HashSet<String>,
    /// The header tag filter. Ephemeral view state like `view_mode` — never
    /// persisted to the inbox file.
    tag_filter: TagFilter,
    view_mode: ViewMode,
    /// Type keys of the groups collapsed in the grouped view.
    collapsed_groups: HashSet<String>,
    show_archive: bool,
    /// Whether the panel is the visible member of its dock (`Panel::set_active`).
    /// The window-activation refresh is gated on it: a hidden panel renders
    /// nothing, so reloading for it on every alt-tab would be wasted I/O.
    panel_active: bool,
    /// State of the type editor overlay; `Some` while it is open. Mutually
    /// exclusive with `detail`: opening one closes the other.
    type_editor: Option<TypeEditorState>,
    /// The detail view overlay of a single item; `Some` while it is open.
    detail: Option<(Entity<InboxDetailView>, Subscription)>,
    confirming_delete: Option<(ItemId, Point<Pixels>)>,
    /// Pending confirmation before removing a staged capture attachment.
    confirming_attachment_removal: Option<(AttachmentRef, Point<Pixels>)>,
    scroll_handle: ScrollHandle,
    /// Per-item Markdown cache for the drag payload, so the eager `on_drag`
    /// value isn't rebuilt on every panel render. Cleared on any store change.
    markdown_cache: RefCell<HashMap<ItemId, SharedString>>,
    /// The partitioned and sorted rows of the list, cached across frames so
    /// the per-frame render doesn't re-clone and re-sort the whole inbox.
    /// Cleared on any store change.
    list_cache: RefCell<Option<Rc<ListRows>>>,
    _age_refresh: Task<()>,
    _subscriptions: Vec<Subscription>,
}

/// Open (sorted), cleared and archived rows in display order; the cached
/// value behind [`InboxPanel::list_rows`].
struct ListRows {
    open: Vec<InboxItem>,
    cleared: Vec<InboxItem>,
    archived: Vec<InboxItem>,
}

/// Which items the header tag filter shows while any tags are selected.
#[derive(Clone, Copy, PartialEq, Default)]
enum TagFilterMode {
    /// Items carrying at least one selected tag.
    #[default]
    Any,
    /// Items carrying every selected tag.
    All,
}

/// The header tag filter: the selected tag keys plus the match mode. An
/// empty selection means "no filter" — everything matches.
#[derive(Default)]
struct TagFilter {
    keys: HashSet<String>,
    mode: TagFilterMode,
}

impl TagFilter {
    fn is_active(&self) -> bool {
        !self.keys.is_empty()
    }

    fn matches(&self, item_tags: &[String]) -> bool {
        if self.keys.is_empty() {
            return true;
        }
        match self.mode {
            TagFilterMode::Any => item_tags.iter().any(|key| self.keys.contains(key)),
            TagFilterMode::All => self.keys.iter().all(|key| item_tags.contains(key)),
        }
    }
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
    if from.trim().is_empty() {
        return;
    }
    let PathWithPosition { path, row, .. } = PathWithPosition::parse_str(from);
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let Some(project_path) = workspace
        .read(cx)
        .project()
        .read(cx)
        .find_project_path(&path, cx)
    else {
        log::warn!(
            "inbox panel: capture context not found in project: {}",
            path.display()
        );
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

/// Opens a file attachment in the workspace: a project-relative path through
/// the project, an external absolute path directly. Shared by the capture box,
/// item rows and the detail view.
pub(crate) fn open_attachment(
    workspace: &WeakEntity<Workspace>,
    attachment: &AttachmentRef,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    match attachment {
        AttachmentRef::Project { path } => {
            let Some(project_path) = workspace
                .read(cx)
                .project()
                .read(cx)
                .find_project_path(Path::new(path), cx)
            else {
                log::warn!("inbox panel: attachment not found in project: {path}");
                return;
            };
            workspace
                .update(cx, |workspace, cx| {
                    workspace.open_path(project_path, None, true, window, cx)
                })
                .detach_and_log_err(cx);
        }
        AttachmentRef::External { path } => {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.open_abs_path(
                        PathBuf::from(path),
                        workspace::OpenOptions::default(),
                        window,
                        cx,
                    )
                })
                .detach_and_log_err(cx);
        }
    }
}

/// Renders an item as Markdown, resolving its list label through the store.
/// Shared by the copy-to-clipboard and send-to-chat actions in both the list
/// rows and the detail view. Public so out-of-crate consumers (the MCP
/// server) produce identical markdown.
pub fn item_markdown(store: &Entity<InboxStore>, item: &InboxItem, cx: &App) -> String {
    let store = store.read(cx);
    let label = store.resolve_kind(item).map(|kind| kind.label.clone());
    let tags = store
        .resolve_tags(item)
        .map(|tag| tag.label.clone())
        .collect::<Vec<_>>();
    item_to_markdown(item, label.as_deref(), &tags)
}

/// Copies the item's Markdown to the clipboard and shows a confirmation toast.
pub(crate) fn copy_item_as_markdown(
    workspace: &WeakEntity<Workspace>,
    store: &Entity<InboxStore>,
    item: &InboxItem,
    cx: &mut App,
) {
    let markdown = item_markdown(store, item, cx);
    cx.write_to_clipboard(ClipboardItem::new_string(markdown));
    show_inbox_toast(
        workspace,
        "inbox-copy-markdown",
        "Copied task as Markdown",
        cx,
    );
}

/// Opens the Zed agent panel and drops the item's Markdown into the message
/// editor as a draft. The item itself is left untouched.
pub(crate) fn send_item_to_chat(
    workspace: &WeakEntity<Workspace>,
    store: &Entity<InboxStore>,
    item: &InboxItem,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace_entity) = workspace.upgrade() else {
        return;
    };
    let markdown = item_markdown(store, item, cx);
    let Some(panel) = workspace_entity.read(cx).panel::<AgentPanel>(cx) else {
        show_inbox_toast(
            workspace,
            "inbox-agent-unavailable",
            "The Zed agent panel is unavailable",
            cx,
        );
        return;
    };
    workspace_entity.update(cx, |workspace, cx| {
        workspace.focus_panel::<AgentPanel>(window, cx);
    });
    panel.update(cx, |panel, cx| {
        panel.insert_prompt_text(markdown, window, cx);
    });
}

/// Shows a short auto-hiding toast in the workspace.
fn show_inbox_toast(
    workspace: &WeakEntity<Workspace>,
    id: &'static str,
    message: impl Into<std::borrow::Cow<'static, str>>,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    workspace.update(cx, |workspace, cx| {
        workspace.show_toast(
            Toast::new(NotificationId::named(id.into()), message).autohide(),
            cx,
        );
    });
}

/// The shared visual for a file-attachment chip: a file icon and the display
/// name. Callers add trailing controls and a click handler.
fn attachment_chip(
    id: impl Into<ElementId>,
    attachment: &AttachmentRef,
    cx: &App,
) -> Stateful<Div> {
    let name = attachment.display_name();
    let name = if name.trim().is_empty() {
        "(file)".to_string()
    } else {
        name.to_string()
    };
    // The full path is recoverable on hover, since the label truncates.
    let full_path = SharedString::from(attachment.path().to_string());
    h_flex()
        .id(id)
        .flex_none()
        .h(px(22.))
        // Cap the width so a long file name truncates instead of stretching
        // the tray; `overflow_hidden` + the label's `truncate()` clip it.
        .max_w(px(180.))
        .gap_1()
        .pl_1p5()
        .pr_1()
        .rounded_sm()
        .bg(cx.theme().colors().element_background)
        .overflow_hidden()
        .tooltip(Tooltip::text(full_path))
        .child(
            Icon::new(IconName::File)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        )
        // The `min_w_0` wrapper lets the label shrink and truncate so a
        // trailing remove button (added by callers) always stays visible.
        .child(
            div().min_w_0().overflow_hidden().child(
                Label::new(name)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            ),
        )
}

/// The colored square marking a catalog entry (list or tag). The one owner of
/// the swatch's size/shape, shared by chips, menu rows and drag ghosts.
pub(crate) fn catalog_swatch(color: gpui::Hsla) -> Div {
    div().flex_none().size(px(7.)).rounded_xs().bg(color)
}

/// The shared read-only visual for a tag chip: a colored dot and the tag
/// label. Used on item rows and in the detail view.
pub(crate) fn tag_chip(label: SharedString, color: gpui::Hsla) -> impl IntoElement {
    h_flex()
        .flex_none()
        .px_0p5()
        .gap_1()
        .child(catalog_swatch(color))
        .child(
            Label::new(label)
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
}

/// Resolves an item's tags into displayable chip data, in catalog order.
/// Shared by the list rows and the detail view so the two surfaces can't
/// drift in how they render the same item's tags.
pub(crate) fn resolved_tag_chips(
    store: &InboxStore,
    item: &InboxItem,
    cx: &App,
) -> Vec<(SharedString, gpui::Hsla)> {
    store
        .resolve_tags(item)
        .map(|tag| {
            (
                SharedString::from(tag.label.clone()),
                catalog_color(&tag.color, cx),
            )
        })
        .collect()
}

/// An [`attachment_chip`] with a trailing remove button. `on_open` fires when
/// the chip body is clicked; `on_remove` fires from the trailing button (which
/// already stops propagation). Shared by the capture box and the detail view.
fn removable_attachment_chip(
    chip_id: impl Into<ElementId>,
    remove_id: impl Into<ElementId>,
    attachment: &AttachmentRef,
    cx: &App,
    on_open: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_remove: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    attachment_chip(chip_id, attachment, cx)
        .child(
            IconButton::new(remove_id, IconName::Close)
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text("Remove attachment"))
                .on_click(on_remove),
        )
        .on_click(on_open)
}

/// A small "Title? [Cancel] [Confirm]" popover anchored at `position`. The
/// `dismiss` and `confirm` callbacks own whatever state reset the caller needs
/// (they capture a `WeakEntity`), which keeps this free of the caller's view
/// type. Shared by every delete/remove confirmation in the panel and detail.
pub(crate) fn confirmation_popover(
    id: &'static str,
    position: Point<Pixels>,
    anchor: gpui::Anchor,
    title: impl Into<SharedString>,
    confirm_label: impl Into<SharedString>,
    dismiss: Rc<dyn Fn(&mut Window, &mut App)>,
    confirm: Rc<dyn Fn(&mut Window, &mut App)>,
    cx: &App,
) -> AnyElement {
    let on_dismiss = dismiss.clone();
    deferred(
        anchored().position(position).anchor(anchor).child(
            v_flex()
                .occlude()
                .elevation_2(cx)
                .p_2()
                .gap_2()
                .on_mouse_down_out(move |_, window, cx| dismiss(window, cx))
                .child(Label::new(title.into()))
                .child(
                    h_flex()
                        .gap_1()
                        .justify_end()
                        .child(
                            Button::new(SharedString::from(format!("{id}-cancel")), "Cancel")
                                .style(ButtonStyle::Subtle)
                                .on_click(move |_, window, cx| on_dismiss(window, cx)),
                        )
                        .child(
                            Button::new(SharedString::from(format!("{id}-confirm")), confirm_label)
                                .style(ButtonStyle::Tinted(TintColor::Error))
                                .on_click(move |_, window, cx| confirm(window, cx)),
                        ),
                ),
        ),
    )
    .with_priority(1)
    .into_any_element()
}

/// A [`confirmation_popover`] wired to a view entity, absorbing the
/// `WeakEntity` + `Rc<dyn Fn>` plumbing every call site would otherwise
/// repeat: both buttons run `reset` (clearing the caller's
/// pending-confirmation state) and notify; the confirm button additionally
/// runs `on_confirm`.
pub(crate) fn entity_confirmation_popover<V: 'static>(
    entity: WeakEntity<V>,
    id: &'static str,
    position: Point<Pixels>,
    anchor: gpui::Anchor,
    title: impl Into<SharedString>,
    confirm_label: impl Into<SharedString>,
    reset: impl Fn(&mut V, &mut Context<V>) + 'static,
    on_confirm: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    cx: &App,
) -> AnyElement {
    let reset = Rc::new(reset);
    let dismiss = {
        let entity = entity.clone();
        let reset = reset.clone();
        Rc::new(move |_: &mut Window, cx: &mut App| {
            entity
                .update(cx, |this, cx| {
                    reset(this, cx);
                    cx.notify();
                })
                .ok();
        }) as Rc<dyn Fn(&mut Window, &mut App)>
    };
    let confirm = Rc::new(move |window: &mut Window, cx: &mut App| {
        entity
            .update(cx, |this, cx| {
                reset(this, cx);
                on_confirm(this, window, cx);
                cx.notify();
            })
            .ok();
    }) as Rc<dyn Fn(&mut Window, &mut App)>;
    confirmation_popover(
        id,
        position,
        anchor,
        title,
        confirm_label,
        dismiss,
        confirm,
        cx,
    )
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

            // Staging list for `@`/picker attachments added before the item
            // exists; the `@` completion provider pushes into it.
            let capture_attachments = cx.new(|_| AttachmentSet::default());
            capture_editor.update(cx, |editor, _| {
                let on_pick: OnPick = Arc::new(Self::capture_sink(&capture_attachments));
                editor.set_completion_provider(Some(Rc::new(AttachmentCompletionProvider::new(
                    weak_workspace.clone(),
                    on_pick,
                ))));
            });

            let focus_handle = cx.focus_handle();
            let editor_focus_handle = capture_editor.focus_handle(cx);
            let subscriptions = vec![
                cx.subscribe_in(&store, window, Self::handle_store_event),
                cx.on_focus(&focus_handle, window, Self::focus_in),
                // Re-render so the capture box border reflects the editor's
                // focus state.
                cx.on_focus_in(&editor_focus_handle, window, |_, _, cx| cx.notify()),
                cx.on_focus_out(&editor_focus_handle, window, |_, _, _, cx| cx.notify()),
                // Re-render the staged attachment chips when the set changes.
                cx.observe(&capture_attachments, |_, _, cx| cx.notify()),
                // Another window on the same repo may have saved under our
                // key; pick its edits up when this window regains focus.
                // Only while the panel is visible — a hidden panel refreshes
                // when it opens (`set_active`) instead.
                cx.observe_window_activation(window, |this, window, cx| {
                    if window.is_window_active() && this.panel_active {
                        this.store.update(cx, |store, cx| store.refresh(cx));
                    }
                }),
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
                capture_attachments,
                capture_kind: None,
                capture_tags: HashSet::default(),
                tag_filter: TagFilter::default(),
                view_mode: ViewMode::All,
                collapsed_groups: HashSet::default(),
                show_archive: false,
                panel_active: false,
                type_editor: None,
                detail: None,
                confirming_delete: None,
                confirming_attachment_removal: None,
                scroll_handle: ScrollHandle::new(),
                markdown_cache: RefCell::new(HashMap::default()),
                list_cache: RefCell::new(None),
                _age_refresh: age_refresh,
                _subscriptions: subscriptions,
            }
        })
    }

    /// Sink appending a picked attachment to the staged capture set. Shared
    /// by the `@` completion, the OS file dialog and drag & drop.
    fn capture_sink(
        set: &Entity<AttachmentSet>,
    ) -> impl Fn(AttachmentRef, &mut App) + Send + Sync + 'static {
        let set = set.downgrade();
        move |attachment, cx| {
            set.update(cx, |set, cx| {
                if set.add(attachment) {
                    cx.notify();
                }
            })
            .ok();
        }
    }

    fn handle_store_event(
        &mut self,
        _: &Entity<InboxStore>,
        event: &InboxStoreEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Any store change can affect an item's rendered Markdown (body, kind,
        // type labels) and the cached row order, so drop both caches
        // wholesale.
        self.markdown_cache.borrow_mut().clear();
        self.list_cache.borrow_mut().take();
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
                self.reconcile_catalog_refs(cx);
                // Reconcile the catalog editor's rename editors here, next to
                // the panel's own catalog-derived state, so no catalog
                // mutation site has to remember to sync them by hand.
                let store = self.store.clone();
                if let Some(state) = self.type_editor.as_mut() {
                    state.sync(&store, window, cx);
                }
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
                self.reconcile_catalog_refs(cx);
                cx.notify();
            }
        }
    }

    /// Drops ephemeral catalog references — the preselected capture list, the
    /// staged capture tags and the filter tags — whose keys no longer exist
    /// in the store (e.g. the entry was deleted, or a restore/rebind replaced
    /// the document). Covers both uniformly, since both emit store events.
    fn reconcile_catalog_refs(&mut self, cx: &Context<Self>) {
        let store = self.store.read(cx);
        if let Some(kind) = &self.capture_kind
            && store.type_by_key(kind).is_none()
        {
            self.capture_kind = None;
        }
        self.capture_tags
            .retain(|key| store.tag_by_key(key).is_some());
        self.tag_filter
            .keys
            .retain(|key| store.tag_by_key(key).is_some());
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
                InboxDetailEvent::OpenTagEditor => {
                    this.detail = None;
                    this.open_type_editor(window, cx);
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
        let attachments = self.capture_attachments.update(cx, |set, _| set.take());
        let staged_tags = std::mem::take(&mut self.capture_tags);
        self.confirming_attachment_removal = None;
        self.store.update(cx, |store, cx| {
            let id = store.capture(text, kind, from, cx);
            if !attachments.is_empty() {
                store.set_attachments(&id, attachments, cx);
            }
            if !staged_tags.is_empty() {
                let tags = store.catalog_ordered_tag_keys(&staged_tags);
                store.set_tags(&id, tags, cx);
            }
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

    /// Opens the OS file dialog and stages the chosen files as attachments for
    /// the next capture.
    fn pick_capture_attachment(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();
        pick_and_attach(project, cx, Self::capture_sink(&self.capture_attachments));
    }

    /// Stages dropped OS files as attachments for the next capture.
    fn stage_external_attachments(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();
        attach_external_paths(
            paths,
            &project,
            cx,
            Self::capture_sink(&self.capture_attachments),
        );
    }

    /// The staged-attachment chips, laid out as the flexible middle of the
    /// capture box's control row (so files never add a second row). Also acts
    /// as the spacer that pushes the trailing controls to the right; empty when
    /// nothing is staged.
    fn render_capture_attachments(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let attachments = self.capture_attachments.read(cx).list().to_vec();
        h_flex()
            .id("inbox-capture-attachments")
            .flex_1()
            .min_w_0()
            .gap_1()
            // One scrolling row rather than wrapping, so adding files never
            // grows the capture box taller.
            .overflow_x_scroll()
            .children(
                attachments
                    .into_iter()
                    .enumerate()
                    .map(|(index, attachment)| {
                        let open = attachment.clone();
                        let remove = attachment.clone();
                        let on_open = cx.listener(move |this, _, window, cx| {
                            open_attachment(&this.workspace, &open, window, cx);
                        });
                        let on_remove = cx.listener(move |this, event: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            this.confirming_attachment_removal =
                                Some((remove.clone(), event.position()));
                            cx.notify();
                        });
                        removable_attachment_chip(
                            ("capture-attachment", index),
                            ("capture-attachment-remove", index),
                            &attachment,
                            cx,
                            on_open,
                            on_remove,
                        )
                    }),
            )
    }

    /// Confirmation popover for removing a staged capture attachment.
    fn render_attachment_removal_confirmation(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (attachment, position) = self.confirming_attachment_removal.clone()?;
        Some(entity_confirmation_popover(
            cx.entity().downgrade(),
            "inbox-capture-attachment-remove",
            position,
            gpui::Anchor::TopLeft,
            "Remove attachment?",
            "Remove",
            |this, _| this.confirming_attachment_removal = None,
            move |this, _, cx| {
                this.capture_attachments.update(cx, |set, cx| {
                    set.remove(&attachment);
                    cx.notify();
                });
            },
            cx,
        ))
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_worktree = self.store.read(cx).has_worktree();
        let has_tags = !self.store.read(cx).tags().is_empty();
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
                    .child(Label::new("Inbox").weight(FontWeight::MEDIUM))
                    // The list/grouped view selector lives here in the header
                    // next to the title, not inside the scrolled list body.
                    .when(has_worktree, |this| {
                        this.child(self.render_view_mode_toggle(cx))
                    }),
            )
            .child(
                h_flex()
                    .gap_1()
                    .when(has_tags, |this| this.child(self.render_tag_filter_menu(cx)))
                    .child(self.render_fields_menu(cx))
                    .child(self.render_sort_menu(cx))
                    // Collapse/expand every list group at once. Only meaningful
                    // in the "By list" view, where groups exist.
                    .when(self.view_mode == ViewMode::Grouped, |this| {
                        let any_collapsed = !self.collapsed_groups.is_empty();
                        this.child(
                            IconButton::new(
                                "inbox-toggle-groups",
                                if any_collapsed {
                                    IconName::ListTree
                                } else {
                                    IconName::ListCollapse
                                },
                            )
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text(if any_collapsed {
                                "Expand all lists"
                            } else {
                                "Collapse all lists"
                            }))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.collapsed_groups.is_empty() {
                                    let keys: Vec<String> = this
                                        .store
                                        .read(cx)
                                        .types()
                                        .iter()
                                        .map(|inbox_type| inbox_type.key.clone())
                                        .collect();
                                    this.collapsed_groups.extend(keys);
                                    this.collapsed_groups
                                        .insert(Self::UNASSIGNED_GROUP_KEY.to_string());
                                } else {
                                    this.collapsed_groups.clear();
                                }
                                cx.notify();
                            })),
                        )
                    })
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
                    )
                    // Export/import need a bound project to be meaningful.
                    .when(has_worktree, |this| {
                        this.child(self.render_more_menu(cx))
                    }),
            )
    }

    /// The overflow menu: exporting the inbox to a JSON file and importing
    /// one back — the manual bridge for moved/renamed projects, whose stored
    /// entry is keyed by the worktree path.
    fn render_more_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = cx.entity().downgrade();
        PopoverMenu::new("inbox-more-menu")
            .trigger(
                IconButton::new("inbox-more", IconName::Ellipsis)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("More")),
            )
            .menu(move |window, cx| {
                let export_panel = panel.clone();
                let import_panel = panel.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.entry("Export Inbox…", None, move |window, cx| {
                        export_panel
                            .update(cx, |this, cx| this.export_inbox(&ExportInbox, window, cx))
                            .ok();
                    })
                    .entry("Import Inbox…", None, move |window, cx| {
                        import_panel
                            .update(cx, |this, cx| this.import_inbox(&ImportInbox, window, cx))
                            .ok();
                    })
                }))
            })
    }

    /// Prompts for a destination and writes the current inbox there as
    /// pretty JSON. Goes through the workspace prompt so remote projects and
    /// the `use_system_path_prompts` setting are handled like every other
    /// save dialog (which also picks the default directory).
    fn export_inbox(&mut self, _: &ExportInbox, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let fs = self.fs.clone();
        let workspace = self.workspace.clone();
        if !store.read(cx).has_worktree() {
            show_inbox_toast(
                &workspace,
                "inbox-export",
                "Open a project to export its inbox",
                cx,
            );
            return;
        }
        let Some(workspace_entity) = workspace.upgrade() else {
            return;
        };
        let path_prompt = workspace_entity.update(cx, |workspace, cx| {
            workspace.prompt_for_new_path(
                DirectoryLister::Project(workspace.project().clone()),
                Some("inbox.json".to_string()),
                window,
                cx,
            )
        });
        cx.spawn(async move |_, cx| {
            // A receiver error means the dialog was torn down (the workspace
            // prompt surfaces portal errors itself) — treat it like a cancel.
            let Ok(Some(paths)) = path_prompt.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            // Snapshot after the dialog closes, so edits made while it was
            // open are included.
            let file = store.read_with(cx, |store, _| store.export_snapshot());
            let write_result = cx
                .background_executor()
                .spawn(async move {
                    let mut content = serde_json::to_string_pretty(&file)?;
                    content.push('\n');
                    fs.atomic_write(path, content).await
                })
                .await;
            let message = match write_result {
                Ok(()) => "Inbox exported".to_string(),
                Err(error) => format!("Failed to export inbox: {error:#}"),
            };
            cx.update(|cx| show_inbox_toast(&workspace, "inbox-export", message, cx));
        })
        .detach();
    }

    /// Prompts for an exported inbox JSON file and merges its items into the
    /// current inbox (duplicate ids are skipped, current data wins).
    fn import_inbox(&mut self, _: &ImportInbox, window: &mut Window, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        let workspace = self.workspace.clone();
        // Without a bound project there is nowhere to persist: the imported
        // items would show until the next rebind and then silently vanish.
        if !self.store.read(cx).has_worktree() {
            show_inbox_toast(
                &workspace,
                "inbox-import",
                "Open a project to import an inbox",
                cx,
            );
            return;
        }
        let Some(workspace_entity) = workspace.upgrade() else {
            return;
        };
        let key_when_prompted = self
            .store
            .read(cx)
            .bound_project_key()
            .map(str::to_owned);
        let paths_prompt = workspace_entity.update(cx, |workspace, cx| {
            workspace.prompt_for_open_path(
                gpui::PathPromptOptions {
                    files: true,
                    directories: false,
                    multiple: false,
                    prompt: None,
                },
                DirectoryLister::Project(workspace.project().clone()),
                window,
                cx,
            )
        });
        cx.spawn(async move |this, cx| {
            let Ok(Some(paths)) = paths_prompt.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let text = fs.load(&path).await?;
                    anyhow::Ok(serde_json::from_str::<InboxFile>(&text)?)
                })
                .await;
            let import_result = match loaded {
                Ok(snapshot) => {
                    let Ok(imported) = this.update(cx, |this, cx| {
                        this.store.update(cx, |store, cx| {
                            // The project may have been switched while the
                            // dialog was open; merging the file into a
                            // different project than the one the user was
                            // looking at would cross-contaminate inboxes.
                            anyhow::ensure!(
                                store.bound_project_key() == key_when_prompted.as_deref(),
                                "the project changed while the dialog was open"
                            );
                            store.import_snapshot(snapshot, cx)
                        })
                    }) else {
                        return;
                    };
                    imported
                }
                Err(error) => Err(error),
            };
            let message = match import_result {
                Ok(0) => "No new items to import".to_string(),
                Ok(1) => "Imported 1 item".to_string(),
                Ok(count) => format!("Imported {count} items"),
                Err(error) => format!("Failed to import inbox: {error:#}"),
            };
            cx.update(|cx| show_inbox_toast(&workspace, "inbox-import", message, cx));
        })
        .detach();
    }

    /// The "Fields" dropdown: toggles which meta fields show on item rows.
    /// Iterating [`MetaField::ALL`] keeps it generic — a new field appears here
    /// automatically and is hideable without touching this menu.
    fn render_fields_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let any_hidden = {
            let store = self.store.read(cx);
            MetaField::ALL
                .iter()
                .any(|field| store.is_field_hidden(field.key()))
        };
        let panel = cx.entity().downgrade();
        PopoverMenu::new("inbox-fields-menu")
            .trigger(
                IconButton::new("inbox-fields", IconName::Filter)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .toggle_state(any_hidden)
                    .tooltip(Tooltip::text("Fields")),
            )
            .menu(move |window, cx| {
                let panel = panel.clone();
                // A persistent menu stays open on click and rebuilds itself, so
                // several fields can be toggled without reopening it. Each row's
                // eye icon reflects visibility.
                Some(ContextMenu::build_persistent(
                    window,
                    cx,
                    move |mut menu, _, cx| {
                        menu = menu.header("Fields");
                        for field in MetaField::ALL {
                            let hidden = panel
                                .upgrade()
                                .is_some_and(|panel| panel.read(cx).is_field_hidden(field, cx));
                            let panel = panel.clone();
                            menu = menu.item(
                                ContextMenuEntry::new(field.label())
                                    .icon(if hidden {
                                        IconName::EyeOff
                                    } else {
                                        IconName::Eye
                                    })
                                    .icon_color(if hidden { Color::Muted } else { Color::Default })
                                    .handler(move |_, cx| {
                                        panel
                                            .update(cx, |this, cx| {
                                                this.store.update(cx, |store, cx| {
                                                    store.toggle_field(field.key(), cx)
                                                });
                                            })
                                            .ok();
                                    }),
                            );
                        }
                        menu
                    },
                ))
            })
    }

    /// Whether a meta field is hidden. Small helper so menus can read it
    /// without reaching through `store`.
    fn is_field_hidden(&self, field: MetaField, cx: &App) -> bool {
        self.store.read(cx).is_field_hidden(field.key())
    }

    /// The "Sort by" dropdown: picks how open items are ordered.
    fn render_sort_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.store.read(cx).sort_mode();
        let panel = cx.entity().downgrade();
        PopoverMenu::new("inbox-sort-menu")
            .trigger(
                ButtonLike::new("inbox-sort")
                    .size(ButtonSize::None)
                    .tooltip(Tooltip::text("Sort"))
                    .child(
                        h_flex()
                            .px_1()
                            .py_0p5()
                            .gap_0p5()
                            .child(
                                Icon::new(IconName::ArrowDown10)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(current.label())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    ),
            )
            .menu(move |window, cx| {
                let panel = panel.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    menu = menu.header("Sort by");
                    for mode in SortMode::ALL {
                        let panel = panel.clone();
                        menu = menu.toggleable_entry(
                            mode.label(),
                            current == mode,
                            IconPosition::End,
                            None,
                            move |_, cx| {
                                panel
                                    .update(cx, |this, cx| {
                                        this.store.update(cx, |store, cx| store.set_sort(mode, cx));
                                    })
                                    .ok();
                            },
                        );
                    }
                    menu
                }))
            })
    }

    fn render_error_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let store = self.store.read(cx);
        // The recovery offer takes priority: it means the stored data is gone
        // or corrupt but a backup can bring it back.
        if store.can_restore() {
            return Some(self.render_restore_banner(cx));
        }
        // Surface the specific load error: "written by a newer version of
        // Zed" must reach the user as-is, or they can't know that editing
        // (and thus overwriting the entry) is the wrong move.
        let message = if let Some(error) = store.load_error() {
            format!("Can't load inbox data: {error}")
        } else if store.save_error().is_some() {
            "Failed to save inbox data".to_string()
        } else {
            return None;
        };
        Some(Self::warning_banner(cx, message).into_any_element())
    }

    /// The shared warning-banner scaffold: a bottom-bordered row with a warning
    /// icon and message. Callers append their own trailing controls.
    fn warning_banner(cx: &App, message: impl Into<SharedString>) -> Div {
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
                Label::new(message.into())
                    .size(LabelSize::XSmall)
                    .color(Color::Warning),
            )
    }

    /// Banner shown when the stored inbox data is missing or corrupt but a
    /// backup holds recoverable data. Offers to restore it or accept the
    /// empty state.
    fn render_restore_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        Self::warning_banner(cx, "Inbox data is missing — restore from backup?")
            .child(div().flex_1())
            .child(
                Button::new("inbox-restore", "Restore")
                    .label_size(LabelSize::XSmall)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.store
                            .update(cx, |store, cx| store.restore_from_backup(cx));
                    })),
            )
            .child(
                Button::new("inbox-restore-dismiss", "Keep empty")
                    .label_size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.store.update(cx, |store, cx| store.dismiss_restore(cx));
                    })),
            )
            .into_any_element()
    }

    /// A clickable colored square + label chip describing a catalog entry,
    /// suitable as a [`PopoverMenu`] trigger. Same visual as [`tag_chip`],
    /// wrapped in a button.
    fn type_chip_button(
        id: impl Into<ElementId>,
        label: SharedString,
        color: gpui::Hsla,
    ) -> ButtonLike {
        ButtonLike::new(id)
            .size(ButtonSize::None)
            .child(tag_chip(label, color))
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
                    .child(catalog_swatch(color))
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

    /// Appends one row per store tag (colored square + label + a check on
    /// each selected tag) to `menu`. Unlike the radio-style type menu this is
    /// a checkbox: rows toggle membership, so callers wrap it in a
    /// *persistent* menu that stays open across toggles. Shared by the
    /// capture box, the item rows, the header filter and the detail view,
    /// which differ only in where the selection lives and what toggling does.
    pub(crate) fn extend_tag_menu(
        mut menu: ContextMenu,
        store: &InboxStore,
        is_selected: impl Fn(&str) -> bool,
        on_toggle: impl Fn(String, &mut Window, &mut App) + Clone + 'static,
        cx: &App,
    ) -> ContextMenu {
        for tag in store.tags() {
            let selected = is_selected(&tag.key);
            let key = tag.key.clone();
            let on_toggle = on_toggle.clone();
            menu = menu.item(Self::type_menu_item(
                SharedString::from(tag.label.clone()),
                catalog_color(&tag.color, cx),
                selected,
                move |window, cx| on_toggle(key.clone(), window, cx),
            ));
        }
        menu
    }

    /// Snapshot of a panel-owned tag-key set plus a toggle handler over it,
    /// shared by the capture-tags and filter menus (both stage selections in
    /// a `HashSet` on the panel rather than in the store).
    fn tag_set_menu_hooks(
        panel: WeakEntity<Self>,
        cx: &mut App,
        field: impl Fn(&mut Self) -> &mut HashSet<String> + Clone + 'static,
    ) -> (
        HashSet<String>,
        impl Fn(String, &mut Window, &mut App) + Clone + 'static,
    ) {
        let snapshot = panel
            .update(cx, |this, _| field(this).clone())
            .unwrap_or_default();
        let on_toggle = move |key: String, _: &mut Window, cx: &mut App| {
            panel
                .update(cx, |this, cx| {
                    let set = field(this);
                    if !set.remove(&key) {
                        set.insert(key);
                    }
                    cx.notify();
                })
                .ok();
        };
        (snapshot, on_toggle)
    }

    /// The trailing "Configure …" entry of a catalog menu. Dismisses `menu`
    /// before running `on_configure`: persistent menus rebuild instead of
    /// closing on entry click, and without the explicit dismiss the popover
    /// would linger on top of the just-opened catalog editor overlay.
    fn configure_entry(
        label: &'static str,
        menu: WeakEntity<ContextMenu>,
        on_configure: impl Fn(&mut Window, &mut App) + 'static,
    ) -> ContextMenuEntry {
        ContextMenuEntry::new(label)
            .icon(IconName::Settings)
            .icon_color(Color::Muted)
            .handler(move |window, cx| {
                menu.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
                on_configure(window, cx);
            })
    }

    /// [`Self::configure_entry`] wired to open the panel's catalog editor —
    /// the trailing entry of every panel-owned catalog menu.
    fn configure_editor_entry(
        label: &'static str,
        menu: WeakEntity<ContextMenu>,
        panel: WeakEntity<Self>,
    ) -> ContextMenuEntry {
        Self::configure_entry(label, menu, move |window, cx| {
            panel
                .update(cx, |this, cx| this.open_type_editor(window, cx))
                .ok();
        })
    }

    /// Appends one row per store type (colored square + label + a check on
    /// `current_key`) to `menu`. Shared by the capture box chip and the item
    /// row chip, which differ only in where the current key comes from and
    /// what selecting does; callers append their own trailing entries.
    fn extend_type_menu(
        mut menu: ContextMenu,
        store: &InboxStore,
        current_key: Option<String>,
        on_select: impl Fn(String, &mut Window, &mut App) + Clone + 'static,
        cx: &App,
    ) -> ContextMenu {
        for inbox_type in store.types() {
            let selected = current_key.as_deref() == Some(inbox_type.key.as_str());
            let key = inbox_type.key.clone();
            let on_select = on_select.clone();
            menu = menu.item(Self::type_menu_item(
                SharedString::from(inbox_type.label.clone()),
                catalog_color(&inbox_type.color, cx),
                selected,
                move |window, cx| on_select(key.clone(), window, cx),
            ));
        }
        menu
    }

    /// The item-scoped tag menu: a persistent checkbox menu toggling the
    /// item's tags in the store. Shared verbatim by the panel rows and the
    /// detail view, which differ only in how "Configure tags…" opens the
    /// catalog editor.
    pub(crate) fn build_item_tags_menu(
        window: &mut Window,
        cx: &mut App,
        store: Entity<InboxStore>,
        item_id: ItemId,
        on_configure: impl Fn(&mut Window, &mut App) + Clone + 'static,
    ) -> Entity<ContextMenu> {
        ContextMenu::build_persistent(window, cx, move |mut menu, _, cx| {
            let current = store
                .read(cx)
                .item(&item_id)
                .map(|item| item.tags.clone())
                .unwrap_or_default();
            menu = menu.header("Tags");
            let on_toggle = {
                let store = store.clone();
                let item_id = item_id.clone();
                move |key: String, _: &mut Window, cx: &mut App| {
                    store.update(cx, |store, cx| {
                        store.toggle_item_tag(&item_id, &key, cx);
                    });
                }
            };
            menu = Self::extend_tag_menu(
                menu,
                store.read(cx),
                move |key| current.iter().any(|current_key| current_key == key),
                on_toggle,
                cx,
            );
            menu.separator().item(Self::configure_entry(
                "Configure tags…",
                cx.weak_entity(),
                on_configure.clone(),
            ))
        })
    }

    /// The capture box tags chip: stages tags for the next capture.
    fn render_capture_tags_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.clone();
        let panel = cx.entity().downgrade();
        let count = self.capture_tags.len();
        let (label, color) = if count == 0 {
            (SharedString::from("Tags"), Color::Muted.color(cx))
        } else {
            (
                SharedString::from(format!("{count} tag{}", if count == 1 { "" } else { "s" })),
                Color::Accent.color(cx),
            )
        };
        PopoverMenu::new("inbox-capture-tags-menu")
            .trigger(Self::type_chip_button(
                "inbox-capture-tags-chip",
                label,
                color,
            ))
            .menu(move |window, cx| {
                let store = store.clone();
                let panel = panel.clone();
                Some(ContextMenu::build_persistent(
                    window,
                    cx,
                    move |mut menu, _, cx| {
                        let (staged, on_toggle) =
                            Self::tag_set_menu_hooks(panel.clone(), cx, |this| {
                                &mut this.capture_tags
                            });
                        menu = menu.header("Tags for next capture");
                        menu = Self::extend_tag_menu(
                            menu,
                            store.read(cx),
                            move |key| staged.contains(key),
                            on_toggle,
                            cx,
                        );
                        let menu_handle = cx.weak_entity();
                        menu.separator().item(Self::configure_editor_entry(
                            "Configure tags…",
                            menu_handle,
                            panel.clone(),
                        ))
                    },
                ))
            })
    }

    /// The item row tags trigger: a hover-revealed button opening a
    /// checkbox menu that toggles the item's tags in the store.
    fn render_item_tags_menu(&self, item_id: ItemId, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.clone();
        let panel = cx.entity().downgrade();
        PopoverMenu::new((ElementId::from(item_id.clone()), "tags-menu"))
            .trigger(
                IconButton::new(
                    (ElementId::from(item_id.clone()), "tags-trigger"),
                    IconName::Hash,
                )
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .visible_on_hover("inbox-item")
                .tooltip(Tooltip::text("Tags")),
            )
            .menu(move |window, cx| {
                let panel = panel.clone();
                Some(Self::build_item_tags_menu(
                    window,
                    cx,
                    store.clone(),
                    item_id.clone(),
                    move |window, cx| {
                        panel
                            .update(cx, |this, cx| this.open_type_editor(window, cx))
                            .ok();
                    },
                ))
            })
    }

    /// The header tag-filter dropdown: a persistent checkbox menu over the
    /// tag catalog plus the Any/All match mode and a "Clear filter" entry.
    fn render_tag_filter_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.clone();
        let panel = cx.entity().downgrade();
        PopoverMenu::new("inbox-tag-filter-menu")
            .trigger(
                IconButton::new("inbox-tag-filter", IconName::Hash)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .toggle_state(self.tag_filter.is_active())
                    .tooltip(Tooltip::text("Filter by tags")),
            )
            .menu(move |window, cx| {
                let store = store.clone();
                let panel = panel.clone();
                Some(ContextMenu::build_persistent(
                    window,
                    cx,
                    move |mut menu, _, cx| {
                        let Some(panel_entity) = panel.upgrade() else {
                            return menu;
                        };
                        let mode = panel_entity.read(cx).tag_filter.mode;
                        let (selected, on_toggle) =
                            Self::tag_set_menu_hooks(panel.clone(), cx, |this| {
                                &mut this.tag_filter.keys
                            });
                        menu = menu.header("Filter by tags");
                        for (label, menu_mode) in [
                            ("Match any", TagFilterMode::Any),
                            ("Match all", TagFilterMode::All),
                        ] {
                            let panel = panel.clone();
                            menu = menu.toggleable_entry(
                                label,
                                mode == menu_mode,
                                IconPosition::End,
                                None,
                                move |_, cx| {
                                    panel
                                        .update(cx, |this, cx| {
                                            this.tag_filter.mode = menu_mode;
                                            cx.notify();
                                        })
                                        .ok();
                                },
                            );
                        }
                        menu = menu.separator();
                        let filter_active = !selected.is_empty();
                        menu = Self::extend_tag_menu(
                            menu,
                            store.read(cx),
                            move |key| selected.contains(key),
                            on_toggle,
                            cx,
                        );
                        if filter_active {
                            let panel = panel.clone();
                            menu = menu.separator().entry("Clear filter", None, move |_, cx| {
                                panel
                                    .update(cx, |this, cx| {
                                        this.tag_filter.keys.clear();
                                        cx.notify();
                                    })
                                    .ok();
                            });
                        }
                        menu
                    },
                ))
            })
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
                    let on_select = {
                        let panel = panel.clone();
                        move |key: String, _: &mut Window, cx: &mut App| {
                            panel
                                .update(cx, |this, cx| {
                                    this.capture_kind = Some(key);
                                    cx.notify();
                                })
                                .ok();
                        }
                    };
                    menu =
                        Self::extend_type_menu(menu, store.read(cx), capture_kind, on_select, cx);
                    let menu_handle = cx.weak_entity();
                    menu.separator().item(Self::configure_editor_entry(
                        "Configure lists…",
                        menu_handle,
                        panel,
                    ))
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
        PopoverMenu::new((ElementId::from(item_id.clone()), "type-menu"))
            .trigger(Self::type_chip_button(
                (ElementId::from(item_id.clone()), "type-chip"),
                chip_label,
                chip_color,
            ))
            .menu(move |window, cx| {
                let store = store.clone();
                let panel = panel.clone();
                let item_id = item_id.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, cx| {
                    let store_ref = store.read(cx);
                    let current_key = store_ref
                        .item(&item_id)
                        .and_then(|item| store_ref.resolve_kind(item))
                        .map(|inbox_type| inbox_type.key.clone());
                    let on_select = {
                        let store = store.clone();
                        let item_id = item_id.clone();
                        move |key: String, _: &mut Window, cx: &mut App| {
                            store.update(cx, |store, cx| {
                                store.set_kind(&item_id, Some(key), cx);
                            });
                        }
                    };
                    menu = Self::extend_type_menu(menu, store_ref, current_key, on_select, cx);
                    let menu_handle = cx.weak_entity();
                    menu.separator().item(Self::configure_editor_entry(
                        "Configure lists…",
                        menu_handle,
                        panel.clone(),
                    ))
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
        let has_tags = !self.store.read(cx).tags().is_empty();
        let (chip_label, chip_color) = {
            let store = self.store.read(cx);
            match self
                .capture_kind
                .as_deref()
                .and_then(|key| store.type_by_key(key))
            {
                Some(inbox_type) => (
                    SharedString::from(inbox_type.label.clone()),
                    catalog_color(&inbox_type.color, cx),
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
                .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                    this.stage_external_attachments(paths.paths(), cx);
                }))
                .bg(cx.theme().colors().editor_background)
                .rounded_md()
                .border_1()
                .border_color(border_color)
                .child(div().px_2().pt_1p5().child(self.capture_editor.clone()))
                .child(
                    // One control row: [type] · [attachment chips, scrolling] ·
                    // [attach] [add]. Chips live here so they never add height.
                    h_flex()
                        .px_2()
                        .py_1()
                        .gap_2()
                        .items_center()
                        .when(has_types, |this| {
                            this.child(self.render_capture_type_menu(chip_label, chip_color, cx))
                        })
                        .when(has_tags, |this| {
                            this.child(self.render_capture_tags_menu(cx))
                        })
                        .child(self.render_capture_attachments(cx))
                        .child(
                            h_flex()
                                .flex_none()
                                .gap_1()
                                .child(
                                    IconButton::new("inbox-attach", IconName::Paperclip)
                                        .icon_size(IconSize::XSmall)
                                        .icon_color(Color::Muted)
                                        .tooltip(Tooltip::text("Attach file"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.pick_capture_attachment(window, cx);
                                        })),
                                )
                                .child(
                                    Label::new("↵ add")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Placeholder),
                                ),
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

    /// Empty state shown when the tag filter hid every row (as opposed to
    /// the inbox genuinely being empty).
    fn render_no_matches_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_2()
            .p_4()
            .child(
                Icon::new(IconName::Hash)
                    .size(IconSize::XLarge)
                    .color(Color::Muted),
            )
            .child(Label::new("No items match the tag filter").color(Color::Muted))
            .child(
                Button::new("inbox-clear-tag-filter", "Clear filter")
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.tag_filter.keys.clear();
                        cx.notify();
                    })),
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
                    catalog_color(&inbox_type.color, cx),
                )
            })
        };

        let checkbox = Checkbox::new(
            (ElementId::from(item.id.clone()), "checkbox"),
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
            // `Muted` (not `Disabled`) so cleared/archived text stays legible in
            // both themes, including under the hover background on dark themes.
            Label::new(item.text.clone())
                .size(LabelSize::Default)
                .color(Color::Muted)
                .strikethrough()
        } else {
            Label::new(item.text.clone()).size(LabelSize::Default)
        };

        // Per-field visibility, toggled from the header "Fields" menu.
        let (hide_list, hide_tags, hide_age, hide_subtasks, hide_context, hide_attachments) = {
            let store = self.store.read(cx);
            (
                store.is_field_hidden(MetaField::List.key()),
                store.is_field_hidden(MetaField::Tags.key()),
                store.is_field_hidden(MetaField::Age.key()),
                store.is_field_hidden(MetaField::Subtasks.key()),
                store.is_field_hidden(MetaField::Context.key()),
                store.is_field_hidden(MetaField::Attachments.key()),
            )
        };
        // Resolved in catalog order; dangling keys are silently skipped.
        let tag_chips = if hide_tags {
            Vec::new()
        } else {
            resolved_tag_chips(self.store.read(cx), item, cx)
        };

        let mut meta = h_flex().flex_wrap().items_center().gap_2();
        if let Some((type_label, type_chip_color)) = type_chip.filter(|_| !hide_list) {
            meta = meta.child(self.render_item_type_menu(
                item.id.clone(),
                type_label,
                type_chip_color,
                cx,
            ));
        }
        for (label, color) in tag_chips {
            meta = meta.child(tag_chip(label, color));
        }
        if let Some(created) = item.created.filter(|_| !hide_age) {
            meta = meta.child(
                div()
                    .text_xs()
                    .font_buffer(cx)
                    .text_color(cx.theme().colors().text_placeholder)
                    .child(format_age(created, now)),
            );
        }
        if let Some((done, total)) = item
            .body
            .as_deref()
            .and_then(subtask_counts)
            .filter(|_| !hide_subtasks)
        {
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
        if let Some(from) = item.from.clone().filter(|_| !hide_context) {
            meta = meta.child(
                h_flex()
                    .id((ElementId::from(item.id.clone()), "from"))
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
        if !hide_attachments {
            for (index, attachment) in item.attachments.iter().enumerate() {
                let open = attachment.clone();
                let chip_id =
                    SharedString::from(format!("inbox-item-attachment-{}-{index}", item.id));
                meta = meta.child(
                    attachment_chip(chip_id, attachment, cx)
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            open_attachment(&this.workspace, &open, window, cx);
                        })),
                );
            }
        }

        let delete_button = IconButton::new(
            (ElementId::from(item.id.clone()), "delete"),
            IconName::Close,
        )
        .icon_size(IconSize::XSmall)
        .icon_color(Color::Muted)
        .visible_on_hover("inbox-item")
        .tooltip(Tooltip::text("Delete"))
        .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
            this.confirming_delete = Some((id.clone(), event.position()));
            cx.notify();
        }));

        // Item-to-item drag reorder only makes sense in manual order (other
        // sort modes compute the order, so a manual drop would not stick) and
        // with no tag filter active — reordering against a partial view would
        // silently reposition items relative to hidden neighbors.
        let reorderable = row == ItemRow::Open
            && !self.tag_filter.is_active()
            && self.store.read(cx).sort_mode().is_manual();
        h_flex()
            .id((ElementId::from(item.id.clone()), "row"))
            .group("inbox-item")
            .items_start()
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded_md()
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .when(row == ItemRow::Open, |this| {
                // The drag payload is a shared `DraggedText` carrying the item's
                // Markdown, so it can be dropped onto the agent panel (send to
                // chat) as well as onto other lists (reorder / move). The ghost
                // preview shows just the title.
                let title = SharedString::from(item.text.clone());
                let payload = DraggedText {
                    id: SharedString::from(item.id.to_string()),
                    text: self.drag_markdown(item, cx),
                };
                this.on_drag(payload, move |_payload, click_offset, _window, cx| {
                    cx.new(|_| InboxItemGhost {
                        text: title.clone(),
                        click_offset,
                    })
                })
            })
            .when(reorderable, |this| {
                this.drag_over::<DraggedText>(|style, _, _, cx| {
                    style.bg(cx.theme().colors().drop_target_background)
                })
                .on_drop(cx.listener({
                    let target_id = item.id.clone();
                    move |this, drag: &DraggedText, _, cx| {
                        let drag_id: ItemId = drag.id.as_ref().into();
                        if drag_id == target_id {
                            return;
                        }
                        this.store.update(cx, |store, cx| {
                            // Dropping onto an item in another list also moves
                            // the dragged item into that list.
                            let target_kind = store
                                .item(&target_id)
                                .and_then(|item| store.resolve_kind(item).map(|t| t.key.clone()));
                            let drag_kind = store
                                .item(&drag_id)
                                .and_then(|item| store.resolve_kind(item).map(|t| t.key.clone()));
                            if target_kind != drag_kind {
                                store.set_kind(&drag_id, target_kind, cx);
                            }
                            store.move_item_before(&drag_id, &target_id, cx);
                        });
                    }
                }))
            })
            .child(
                h_flex()
                    .flex_none()
                    .h(rems(ITEM_LINE_HEIGHT))
                    .child(checkbox),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .id((ElementId::from(item.id.clone()), "text"))
                            .line_height(rems(ITEM_LINE_HEIGHT))
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
            .child(
                h_flex()
                    .flex_none()
                    .child(self.render_item_tags_menu(item.id.clone(), cx))
                    .child(self.render_item_actions_menu(item, cx))
                    .child(delete_button),
            )
            .into_any_element()
    }

    /// The item's Markdown for the drag payload, memoized so the eager
    /// `on_drag` value isn't rebuilt for every open row on every render. The
    /// cache is invalidated on any store change in [`Self::handle_store_event`].
    fn drag_markdown(&self, item: &InboxItem, cx: &App) -> SharedString {
        if let Some(cached) = self.markdown_cache.borrow().get(&item.id) {
            return cached.clone();
        }
        let markdown = SharedString::from(item_markdown(&self.store, item, cx));
        self.markdown_cache
            .borrow_mut()
            .insert(item.id.clone(), markdown.clone());
        markdown
    }

    /// The per-row overflow menu (`…`): Copy as Markdown / Send to AI Chat.
    fn render_item_actions_menu(
        &self,
        item: &InboxItem,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let panel = cx.entity().downgrade();
        let item = item.clone();
        PopoverMenu::new((ElementId::from(item.id.clone()), "actions"))
            .trigger(
                IconButton::new(
                    (ElementId::from(item.id.clone()), "actions-trigger"),
                    IconName::Ellipsis,
                )
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .visible_on_hover("inbox-item")
                .tooltip(Tooltip::text("More actions")),
            )
            .menu(move |window, cx| {
                let panel = panel.clone();
                let item = item.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    let (copy_item, copy_panel) = (item.clone(), panel.clone());
                    let (chat_item, chat_panel) = (item.clone(), panel);
                    menu.entry("Copy as Markdown", None, move |_window, cx| {
                        copy_panel
                            .update(cx, |this, cx| {
                                copy_item_as_markdown(&this.workspace, &this.store, &copy_item, cx);
                            })
                            .ok();
                    })
                    .entry("Send to AI Chat", None, move |window, cx| {
                        chat_panel
                            .update(cx, |this, cx| {
                                send_item_to_chat(
                                    &this.workspace,
                                    &this.store,
                                    &chat_item,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    })
                }))
            })
    }

    /// Shared chrome of the collapsible section headers (archive and "By
    /// list" groups): disclosure, catalog swatch, bold uppercase label and a
    /// count badge.
    fn render_section_header(
        &self,
        id: SharedString,
        expanded: bool,
        swatch_color: gpui::Hsla,
        label: SharedString,
        count: usize,
        on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .id(id)
            .cursor_pointer()
            .px_2()
            .py_1()
            .gap_1()
            .on_click(on_toggle)
            .child(Disclosure::new("inbox-section-disclosure", expanded))
            .child(catalog_swatch(swatch_color))
            .child(
                Label::new(label)
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

    fn render_archive_header(&self, count: usize, cx: &mut Context<Self>) -> impl IntoElement {
        // Muted swatch so the archive section lines up visually with the
        // "By list" group headers.
        let swatch_color = Color::Muted.color(cx);
        let on_toggle = cx.listener(|this: &mut Self, _, _, cx| {
            this.show_archive = !this.show_archive;
            cx.notify();
        });
        self.render_section_header(
            "inbox-archive-header".into(),
            self.show_archive,
            swatch_color,
            "ARCHIVE".into(),
            count,
            on_toggle,
            cx,
        )
    }

    /// A minimalist dropdown that switches between "All" and "By list": a
    /// compact muted label + chevron that opens a small select menu.
    fn render_view_mode_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current_label = match self.view_mode {
            ViewMode::All => "All",
            ViewMode::Grouped => "By list",
        };
        let panel = cx.entity().downgrade();
        h_flex().flex_none().child(
            PopoverMenu::new("inbox-view-mode-menu")
                .trigger(
                    ButtonLike::new("inbox-view-mode-trigger")
                        .size(ButtonSize::None)
                        .child(
                            h_flex()
                                .px_1()
                                .py_0p5()
                                .gap_0p5()
                                .child(
                                    Label::new(current_label)
                                        .size(LabelSize::Default)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Icon::new(IconName::ChevronDown)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                ),
                        ),
                )
                .menu(move |window, cx| {
                    let panel = panel.clone();
                    Some(ContextMenu::build(window, cx, move |menu, _, cx| {
                        let current = panel.upgrade().map(|panel| panel.read(cx).view_mode);
                        menu.toggleable_entry(
                            "All",
                            current == Some(ViewMode::All),
                            IconPosition::End,
                            None,
                            {
                                let panel = panel.clone();
                                move |_, cx| {
                                    panel
                                        .update(cx, |this, cx| {
                                            this.view_mode = ViewMode::All;
                                            cx.notify();
                                        })
                                        .ok();
                                }
                            },
                        )
                        .toggleable_entry(
                            "By list",
                            current == Some(ViewMode::Grouped),
                            IconPosition::End,
                            None,
                            move |_, cx| {
                                panel
                                    .update(cx, |this, cx| {
                                        this.view_mode = ViewMode::Grouped;
                                        cx.notify();
                                    })
                                    .ok();
                            },
                        )
                    }))
                }),
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
        let id = SharedString::from(format!("inbox-group-header-{key}"));
        let on_toggle = cx.listener(move |this: &mut Self, _, _, cx| {
            if !this.collapsed_groups.remove(&key) {
                this.collapsed_groups.insert(key.clone());
            }
            cx.notify();
        });
        self.render_section_header(
            id,
            !collapsed,
            color,
            SharedString::from(label.to_uppercase()),
            count,
            on_toggle,
            cx,
        )
    }

    /// The `collapsed_groups` entry for the synthetic "Unassigned" group
    /// (items with no kind, or an unknown/deleted kind). Real type keys are
    /// generated as `k{id}` by [`InboxStore::add_entry`](crate::inbox_store::InboxStore::add_entry),
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
        open: &[&InboxItem],
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
                        catalog_color(&inbox_type.color, cx),
                    )
                })
                .collect()
        };
        // Bucket by the resolved kind in one pass. Items with no kind, or an
        // unknown/deleted kind, resolve to `None` and land in the
        // "Unassigned" group below.
        let mut buckets: HashMap<Option<String>, Vec<&InboxItem>> = HashMap::default();
        {
            let store = self.store.read(cx);
            for &item in open {
                buckets
                    .entry(store.resolve_kind(item).map(|t| t.key.clone()))
                    .or_default()
                    .push(item);
            }
        }

        let mut elements = Vec::new();
        for (key, label, color) in groups {
            let items = buckets.remove(&Some(key.clone())).unwrap_or_default();
            elements.push(self.render_type_group(Some(key), label, color, items, now, cx));
        }
        let unassigned = buckets.remove(&None).unwrap_or_default();
        if !unassigned.is_empty() {
            elements.push(self.render_type_group(
                None,
                SharedString::from("Unassigned"),
                cx.theme().colors().border_variant,
                unassigned,
                now,
                cx,
            ));
        }
        elements
    }

    /// One droppable "By list" group: header plus, when expanded, the
    /// group's item rows. `key` is `None` for the synthetic "Unassigned"
    /// group (items whose kind resolves to no existing type); dropping an
    /// item there clears its kind.
    fn render_type_group(
        &self,
        key: Option<String>,
        label: SharedString,
        color: gpui::Hsla,
        items: Vec<&InboxItem>,
        now: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let group_key = key
            .clone()
            .unwrap_or_else(|| Self::UNASSIGNED_GROUP_KEY.to_string());
        let collapsed = self.collapsed_groups.contains(&group_key);
        let mut group = v_flex()
            .id(SharedString::from(format!("inbox-group-{group_key}")))
            .rounded_md()
            .drag_over::<DraggedText>(|style, _, _, cx| {
                style.bg(cx.theme().colors().drop_target_background)
            })
            .on_drop(cx.listener({
                let target = key;
                move |this, drag: &DraggedText, _, cx| {
                    let drag_id: ItemId = drag.id.as_ref().into();
                    this.store.update(cx, |store, cx| {
                        let already_there = store.item(&drag_id).is_some_and(|item| {
                            store.resolve_kind(item).map(|t| t.key.clone()) == target
                        });
                        if !already_there {
                            store.set_kind(&drag_id, target.clone(), cx);
                        }
                    });
                }
            }))
            .child(self.render_group_header(group_key, label, color, items.len(), collapsed, cx));
        if !collapsed {
            for item in items {
                group = group.child(self.render_item(item, ItemRow::Open, now, cx));
            }
        }
        group.into_any_element()
    }

    /// The partitioned and sorted rows of the list, cached across frames and
    /// invalidated on any store event in [`Self::handle_store_event`].
    fn list_rows(&self, cx: &App) -> Rc<ListRows> {
        if let Some(rows) = self.list_cache.borrow().as_ref() {
            return rows.clone();
        }
        let store = self.store.read(cx);
        let (cleared, mut open): (Vec<_>, Vec<_>) = store
            .items()
            .iter()
            .cloned()
            .partition(InboxItem::is_cleared);
        // Sorting the flat list also orders items within each "By list" group,
        // since the groups keep the flat order.
        store.sort_mode().apply(&mut open);
        let rows = Rc::new(ListRows {
            open,
            cleared,
            archived: store.archived().to_vec(),
        });
        *self.list_cache.borrow_mut() = Some(rows.clone());
        rows
    }

    /// Applies the tag filter to one section of the cached rows.
    fn filter_rows<'a>(&self, items: &'a [InboxItem]) -> Vec<&'a InboxItem> {
        items
            .iter()
            .filter(|item| self.tag_filter.matches(&item.tags))
            .collect()
    }

    fn render_list(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.list_rows(cx);
        // The tag filter is a cheap render-time pass over the cached rows, so
        // toggling it never has to invalidate `list_cache`. It applies
        // uniformly to open, cleared and archived rows.
        let open = self.filter_rows(&rows.open);
        let cleared = self.filter_rows(&rows.cleared);
        let archived = self.filter_rows(&rows.archived);
        // The badge shows how many archive rows are currently visible, but
        // the header itself is gated on the *unfiltered* count so the archive
        // section can't silently vanish while a filter hides all its rows.
        let archive_count = cleared.len() + archived.len();
        let archive_exists = !rows.cleared.is_empty() || !rows.archived.is_empty();
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
        // Distinguish "the filter hid rows that would otherwise be visible"
        // from "the inbox is genuinely empty", so the empty state offers to
        // clear the filter only when clearing it would actually reveal rows.
        // (Under `show_empty_state` every filtered section is empty, so "the
        // filter hid something" reduces to "the raw section is non-empty".)
        let filtered_out = show_empty_state
            && self.tag_filter.is_active()
            && (!rows.open.is_empty() || (self.show_archive && archive_exists));

        // Two layers: the outer container hosts the scrollbar overlay and does
        // NOT scroll, while the inner container carries `overflow_y_scroll` +
        // `track_scroll` and holds the rows. The scrollbar must live outside the
        // scrolled content, otherwise its thumb rides along with the content and
        // appears frozen. Both share `scroll_handle`; `tracked_scroll_handle`
        // keeps the thumb tracking it and `tracked_entity` re-renders the panel
        // while dragging the thumb. Matches memory_view / breakpoint_list.
        v_flex()
            .id("inbox-list-container")
            .flex_1()
            .min_h_0()
            .child(
                v_flex()
                    .id("inbox-list")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .p_1()
                    .when(show_empty_state, |this| {
                        if filtered_out {
                            this.child(self.render_no_matches_state(cx))
                        } else {
                            this.child(self.render_empty_state(cx))
                        }
                    })
                    .children(open_rows)
                    .when(archive_exists, |this| {
                        this.child(self.render_archive_header(archive_count, cx))
                            .children(archive_rows)
                    }),
            )
            .custom_scrollbars(
                Scrollbars::new(ScrollAxes::Vertical)
                    .tracked_scroll_handle(&self.scroll_handle)
                    .tracked_entity(cx.entity_id()),
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

    fn render_delete_confirmation(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (id, position) = self.confirming_delete.clone()?;
        Some(entity_confirmation_popover(
            cx.entity().downgrade(),
            "inbox-item-delete",
            position,
            gpui::Anchor::TopLeft,
            "Delete item?",
            "Delete",
            |this, _| this.confirming_delete = None,
            move |this, _, cx| {
                this.store
                    .update(cx, |store, cx| store.delete_item(&id, cx));
            },
            cx,
        ))
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

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.panel_active = active;
        // Opening the panel (e.g. after working in another window on the
        // same repo) must show the latest stored document.
        if active {
            self.store.update(cx, |store, cx| store.refresh(cx));
        }
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
            .on_action(cx.listener(Self::export_inbox))
            .on_action(cx.listener(Self::import_inbox))
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
            .children(self.render_type_editor(window, cx))
            .children(self.render_detail_overlay(cx))
            .children(self.render_delete_confirmation(cx))
            .children(self.render_attachment_removal_confirmation(cx))
    }
}
