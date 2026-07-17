pub mod inbox_model;
mod inbox_panel_settings;
pub mod inbox_store;

pub use inbox_store::{InboxStore, InboxStoreEvent};

use std::{sync::Arc, time::Duration};

use editor::Editor;
use fs::Fs;
use gpui::{
    Action, AnyElement, App, AppContext as _, AsyncWindowContext, ClickEvent, Context, Entity,
    EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, ParentElement, Pixels, Point,
    Render, ScrollHandle, Styled, Subscription, Task, WeakEntity, Window, actions, anchored,
    deferred,
};
use ui::{Checkbox, Disclosure, Tab, TintColor, ToggleState, Tooltip, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::inbox_model::{
    InboxItem, ItemId, classify, format_age, now_unix, subtask_counts, type_color,
};
use crate::inbox_panel_settings::{DockSide, InboxPanelSettings, Settings};

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

/// How often the item age labels ("2м"/"15ч") are refreshed.
const AGE_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

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
    /// Type key preselected for the next capture. `None` means "Авто"
    /// (classify the text on capture).
    capture_kind: Option<String>,
    show_archive: bool,
    confirming_delete: Option<(ItemId, Point<Pixels>)>,
    scroll_handle: ScrollHandle,
    _age_refresh: Task<()>,
    _subscriptions: Vec<Subscription>,
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
                    "Выгрузи из головы — что угодно про проект…",
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
                show_archive: false,
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
                // TODO(Task 7): close the detail view when its item is deleted.
                cx.notify();
            }
            InboxStoreEvent::Changed | InboxStoreEvent::Reloaded => cx.notify(),
        }
    }

    fn focus_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.store.read(cx).has_worktree() {
            self.capture_editor.focus_handle(cx).focus(window, cx);
        }
    }

    fn capture(&mut self, _: &Capture, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.capture_editor.read(cx).text(cx).trim().to_string();
        if text.is_empty() {
            return;
        }
        // "Авто" resolves to the classified kind at capture time, so the file
        // is self-contained.
        let kind = self
            .capture_kind
            .clone()
            .or_else(|| Some(classify(&text).to_string()));
        let from = self.active_editor_context(window, cx);
        self.store.update(cx, |store, cx| {
            store.capture(text, kind, from, cx);
        });
        self.capture_editor
            .update(cx, |editor, cx| editor.set_text("", window, cx));
    }

    /// Returns a `"path:row"` context string for the workspace's active
    /// editor, or `None` if the active item is not a file-backed editor.
    fn active_editor_context(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let workspace = self.workspace.upgrade()?;
        let editor = workspace.read(cx).active_item_as::<Editor>(cx)?;
        if editor == self.capture_editor {
            return None;
        }
        editor.update(cx, |editor, cx| {
            let buffer = editor.buffer().read(cx).as_singleton()?;
            let path = buffer.read(cx).file()?.path().as_unix_str().to_string();
            let snapshot = editor.snapshot(window, cx);
            let row = editor.selections.newest::<text::Point>(&snapshot).head().row;
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
                        .tooltip(Tooltip::text("Показать/скрыть разобранные"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_archive = !this.show_archive;
                            cx.notify();
                        })),
                    )
                    .child(
                        IconButton::new("archive-cleared", IconName::Trash)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Убрать разобранные в архив"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.store.update(cx, |store, cx| store.archive_cleared(cx));
                            })),
                    ),
                // TODO(Task 5): add the type settings button here.
            )
    }

    fn render_error_banner(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let store = self.store.read(cx);
        let message = if store.load_error().is_some() {
            "inbox.json повреждён — исправьте файл"
        } else if store.save_error().is_some() {
            "не удалось сохранить inbox.json"
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

    /// A colored square + label chip describing an inbox type.
    fn render_type_chip(
        &self,
        label: SharedString,
        color: gpui::Hsla,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .gap_1()
            .child(div().flex_none().size(px(7.)).rounded_xs().bg(color))
            .child(Label::new(label).size(LabelSize::XSmall).color(Color::Muted))
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

        let (chip_label, chip_color) = {
            let store = self.store.read(cx);
            match self
                .capture_kind
                .as_deref()
                .and_then(|key| store.types().iter().find(|inbox_type| inbox_type.key == key))
            {
                Some(inbox_type) => (
                    SharedString::from(inbox_type.label.clone()),
                    type_color(&inbox_type.color, cx),
                ),
                None => (SharedString::from("Авто"), Color::Muted.color(cx)),
            }
        };

        div().flex_none().p_2().child(
            v_flex()
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
                        .justify_between()
                        // TODO(Task 5): replace the static chip with a
                        // PopoverMenu type picker.
                        .child(self.render_type_chip(chip_label, chip_color, cx))
                        .child(
                            Label::new("↵ добавить")
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
                Label::new("Инбокс пуст")
                    .color(Color::Muted)
                    .weight(FontWeight::MEDIUM),
            )
            .child(
                div().text_center().max_w(px(240.)).child(
                    Label::new(
                        "Всё разобрано. Появится мысль — бросай сюда, не отвлекаясь от кода.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Placeholder),
                ),
            )
    }

    fn render_no_worktree(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().flex_1().items_center().justify_center().p_4().child(
            div().text_center().max_w(px(240.)).child(
                Label::new("Откройте проект, чтобы вести инбокс").color(Color::Muted),
            ),
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
        let (type_label, type_chip_color) = {
            let store = self.store.read(cx);
            let inbox_type = store.resolve_kind(item);
            (
                SharedString::from(inbox_type.label.clone()),
                type_color(&inbox_type.color, cx),
            )
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

        let mut meta = h_flex().flex_wrap().items_center().gap_2().child(
            // TODO(Task 5): open a type picker menu from this chip.
            self.render_type_chip(type_label, type_chip_color, cx),
        );
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
                // TODO(Task 5): open the file at the captured location on click.
                h_flex()
                    .gap_1()
                    .cursor_pointer()
                    .text_xs()
                    .font_buffer(cx)
                    .text_color(cx.theme().colors().text_placeholder)
                    .hover(|style| style.text_color(cx.theme().colors().text_accent))
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
            .tooltip(Tooltip::text("Удалить"))
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
            .child(div().flex_none().child(checkbox))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    // TODO(Task 7): open the detail view when the text is clicked.
                    .child(text_label)
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
            .child(Disclosure::new("inbox-archive-disclosure", self.show_archive))
            .child(
                Label::new("АРХИВ")
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

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
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

        let mut open_rows = Vec::new();
        for item in &open {
            open_rows.push(self.render_item(item, ItemRow::Open, now, cx));
        }
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
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .p_1()
            .when(show_empty_state, |this| {
                this.child(self.render_empty_state(cx))
            })
            .children(open_rows)
            .when(archive_count > 0, |this| {
                this.child(self.render_archive_header(archive_count, cx))
                    .children(archive_rows)
            })
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
                            .child(Label::new("Удалить запись?"))
                            .child(
                                h_flex()
                                    .gap_1()
                                    .justify_end()
                                    .child(
                                        Button::new("inbox-delete-cancel", "Отмена")
                                            .style(ButtonStyle::Subtle)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.confirming_delete = None;
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        Button::new("inbox-delete-confirm", "Удалить")
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
                        .child(self.render_list(cx))
                } else {
                    this.child(self.render_no_worktree(cx))
                }
            })
            .children(self.render_delete_confirmation(cx))
    }
}
