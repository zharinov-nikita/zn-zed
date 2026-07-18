//! The detail view of a single inbox item: a title editor, a meta line and a
//! read-only render of the item's markdown body as [`Block`]s. Rendered by
//! the panel as a full-panel overlay. Block editing arrives in Task 8, the
//! slash/grip menus in Task 9.

use std::collections::HashMap;

use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontStyle, FontWeight,
    IntoElement, ParentElement, Render, ScrollHandle, StrikethroughStyle, Styled, Subscription,
    TextStyleRefinement, UnderlineStyle, WeakEntity, Window,
};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{Checkbox, Divider, Tab, ToggleState, Tooltip, prelude::*};
use workspace::Workspace;

use crate::block::{Block, BlockDocument, BlockId, BlockType};
use crate::inbox_model::{InboxItem, ItemId, format_age, now_unix, type_color};
use crate::inbox_store::{InboxStore, InboxStoreEvent};
use crate::open_capture_context;

pub enum InboxDetailEvent {
    /// The view wants to be closed (back button, Escape, or its item is gone).
    Closed,
}

impl EventEmitter<InboxDetailEvent> for InboxDetailView {}

pub struct InboxDetailView {
    store: Entity<InboxStore>,
    item_id: ItemId,
    workspace: WeakEntity<Workspace>,
    title_editor: Entity<Editor>,
    /// The block model of the item's markdown body.
    document: BlockDocument,
    /// Lazily-created markdown renderers for the text blocks, keyed by block
    /// id and kept in sync with the block text.
    read_markdown: HashMap<BlockId, Entity<Markdown>>,
    scroll_handle: ScrollHandle,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl InboxDetailView {
    pub fn new(
        store: Entity<InboxStore>,
        item_id: ItemId,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (text, body) = store
            .read(cx)
            .item(&item_id)
            .map(|item| (item.text.clone(), item.body.clone()))
            .unwrap_or_default();

        let title_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 6, window, cx);
            editor.set_placeholder_text("Заголовок записи", window, cx);
            editor.set_text(text, window, cx);
            editor
        });

        let subscriptions = vec![
            cx.subscribe(&title_editor, Self::handle_title_editor_event),
            cx.subscribe_in(&store, window, Self::handle_store_event),
        ];

        Self {
            store,
            item_id,
            workspace,
            title_editor,
            document: BlockDocument::from_markdown(body.as_deref().unwrap_or_default()),
            read_markdown: HashMap::default(),
            scroll_handle: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn handle_title_editor_event(
        &mut self,
        editor: Entity<Editor>,
        event: &EditorEvent,
        cx: &mut Context<Self>,
    ) {
        if let EditorEvent::BufferEdited = event {
            let text = editor.read(cx).text(cx);
            // Only write back real changes; this also keeps the programmatic
            // `set_text` after an external reload from dirtying the store.
            let changed = self
                .store
                .read(cx)
                .item(&self.item_id)
                .is_some_and(|item| item.text != text);
            if changed {
                let item_id = self.item_id.clone();
                self.store
                    .update(cx, |store, cx| store.set_text(&item_id, text, cx));
            }
        }
    }

    fn handle_store_event(
        &mut self,
        _: &Entity<InboxStore>,
        event: &InboxStoreEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InboxStoreEvent::ItemDeleted(id) => {
                if *id == self.item_id {
                    cx.emit(InboxDetailEvent::Closed);
                } else {
                    cx.notify();
                }
            }
            InboxStoreEvent::Changed => cx.notify(),
            InboxStoreEvent::Reloaded => {
                let Some(item) = self.store.read(cx).item(&self.item_id).cloned() else {
                    cx.emit(InboxDetailEvent::Closed);
                    return;
                };
                // The file changed externally: rebuild the document if the
                // body no longer matches ours (compared through the codec so
                // formatting-only differences don't count). No block editing
                // exists yet, so there are no in-progress edits to preserve.
                // TODO(Task 8): skip the rebuild while a block is being
                // edited.
                let new_document =
                    BlockDocument::from_markdown(item.body.as_deref().unwrap_or_default());
                if new_document.to_markdown() != self.document.to_markdown() {
                    self.document = new_document;
                    self.read_markdown.clear();
                }
                if self.title_editor.read(cx).text(cx) != item.text {
                    self.title_editor
                        .update(cx, |editor, cx| editor.set_text(item.text, window, cx));
                }
                cx.notify();
            }
        }
    }

    /// Serializes the document back into the item's body.
    fn save_body(&mut self, cx: &mut Context<Self>) {
        let markdown = self.document.to_markdown();
        let body = (!markdown.is_empty()).then_some(markdown);
        let item_id = self.item_id.clone();
        self.store
            .update(cx, |store, cx| store.set_body(&item_id, body, cx));
    }

    /// Returns the markdown renderer of a text block, creating it on first
    /// use and re-parsing it when the block text has changed.
    fn read_markdown_entity(&mut self, block: &Block, cx: &mut Context<Self>) -> Entity<Markdown> {
        let entity = self
            .read_markdown
            .entry(block.id)
            .or_insert_with(|| {
                let text = SharedString::from(block.text.clone());
                cx.new(|cx| Markdown::new(text, None, None, cx))
            })
            .clone();
        if entity.read(cx).source().as_ref() != block.text {
            let text = SharedString::from(block.text.clone());
            entity.update(cx, |markdown, cx| markdown.replace(text, cx));
        }
        entity
    }

    /// The read-mode markdown style of a text block. The block text is a
    /// single line with the markdown prefix already stripped, so block-level
    /// styling (heading size, quote italics, todo strikethrough) comes from
    /// the base text style rather than from markdown parsing.
    fn markdown_style(block: &Block, window: &Window, cx: &App) -> MarkdownStyle {
        let theme_settings = ThemeSettings::get_global(cx);
        let colors = cx.theme().colors();

        let mut base_text_style = window.text_style();
        base_text_style.refine(&TextStyleRefinement {
            font_family: Some(theme_settings.ui_font.family.clone()),
            font_fallbacks: theme_settings.ui_font.fallbacks.clone(),
            font_features: Some(theme_settings.ui_font.features.clone()),
            color: Some(colors.text),
            ..Default::default()
        });
        let refinement = match block.block_type {
            BlockType::H1 => TextStyleRefinement {
                font_size: Some(rems(1.35).into()),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
            BlockType::H2 => TextStyleRefinement {
                font_size: Some(rems(1.1).into()),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
            BlockType::Quote => TextStyleRefinement {
                font_style: Some(FontStyle::Italic),
                color: Some(Color::Muted.color(cx)),
                ..Default::default()
            },
            BlockType::Todo if block.checked => TextStyleRefinement {
                color: Some(Color::Muted.color(cx)),
                strikethrough: Some(StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(Color::Muted.color(cx)),
                }),
                ..Default::default()
            },
            _ => TextStyleRefinement::default(),
        };
        base_text_style.refine(&refinement);

        MarkdownStyle {
            base_text_style,
            syntax: cx.theme().syntax().clone(),
            selection_background_color: colors.element_selection_background,
            inline_code: TextStyleRefinement {
                font_family: Some(theme_settings.buffer_font.family.clone()),
                font_fallbacks: theme_settings.buffer_font.fallbacks.clone(),
                background_color: Some(colors.editor_background),
                ..Default::default()
            },
            link: TextStyleRefinement {
                color: Some(colors.text_accent),
                underline: Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(colors.text_accent),
                    wavy: false,
                }),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn render_header(&self, cleared: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let (toggle_icon, toggle_label, toggle_color) = if cleared {
            (IconName::Check, "Готово", Color::Created)
        } else {
            (IconName::Circle, "Разобрать", Color::Muted)
        };

        h_flex()
            .flex_none()
            .h(Tab::container_height(cx))
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Button::new("inbox-detail-back", "Инбокс")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .start_icon(
                        Icon::new(IconName::ChevronLeft)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text("Назад к инбоксу"))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(InboxDetailEvent::Closed))),
            )
            .child(div().flex_1())
            .child(
                Button::new("inbox-detail-toggle-cleared", toggle_label)
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .color(toggle_color)
                    .start_icon(
                        Icon::new(toggle_icon)
                            .size(IconSize::Small)
                            .color(toggle_color),
                    )
                    .tooltip(Tooltip::text("Отметить разобранным"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        let item_id = this.item_id.clone();
                        this.store
                            .update(cx, |store, cx| store.toggle_cleared(&item_id, cx));
                    })),
            )
            .child(
                IconButton::new("inbox-detail-delete", IconName::Trash)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Удалить запись"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        let item_id = this.item_id.clone();
                        // The store emits `ItemDeleted`, which closes this
                        // view; no extra confirmation here, as in the design.
                        this.store
                            .update(cx, |store, cx| store.delete_item(&item_id, cx));
                    })),
            )
    }

    fn render_title(&self, item: &InboxItem, cx: &mut Context<Self>) -> impl IntoElement {
        let (type_label, type_square_color) = {
            let store = self.store.read(cx);
            let inbox_type = store.resolve_kind(item);
            (
                SharedString::from(inbox_type.label.clone()),
                type_color(&inbox_type.color, cx),
            )
        };

        let mut meta = h_flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .pl(px(16.))
            .text_xs()
            .font_buffer(cx)
            .text_color(cx.theme().colors().text_placeholder)
            .child(type_label);
        if let Some(created) = item.created {
            let age = format_age(created, now_unix());
            let captured = if age == "сейчас" {
                "захвачено только что".to_string()
            } else {
                format!("захвачено {age} назад")
            };
            meta = meta.child("·").child(captured);
        }
        if let Some(from) = item.from.clone() {
            meta = meta.child("·").child(
                h_flex()
                    .id("inbox-detail-from")
                    .gap_1()
                    .cursor_pointer()
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
        let (done, total) = self.document.subtask_counts();
        let subtasks = if total > 0 {
            format!("{done}/{total} подзадач")
        } else {
            "нет подзадач".to_string()
        };
        meta = meta.child("·").child(subtasks);

        v_flex()
            .flex_none()
            .px_3()
            .pt_3()
            .pb_2()
            .gap_2()
            .child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .child(
                        div()
                            .flex_none()
                            .mt(px(5.))
                            .size(px(8.))
                            .rounded_xs()
                            .bg(type_square_color),
                    )
                    .child(div().flex_1().min_w_0().child(self.title_editor.clone())),
            )
            .child(meta)
    }

    fn render_code_block(&self, block: &Block, cx: &App) -> AnyElement {
        let mut container = v_flex()
            .w_full()
            .my_1()
            .px_2()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().editor_background)
            .font_buffer(cx)
            .text_sm();
        for line in block.text.split('\n') {
            let line = if line.is_empty() {
                SharedString::from("\u{00a0}")
            } else {
                SharedString::from(line.to_string())
            };
            container = container.child(div().child(line));
        }
        container.into_any_element()
    }

    fn render_block(
        &mut self,
        block: &Block,
        is_last: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let block_id = block.id;

        let mut row = h_flex()
            .id(("inbox-detail-block", block_id.0))
            .group("detail-block")
            .items_start()
            .gap_2()
            .px_1()
            .py_0p5()
            .rounded_sm()
            .hover(|style| style.bg(cx.theme().colors().element_hover));

        // Leading adornment.
        row = match block.block_type {
            BlockType::Todo => row.child(
                div().flex_none().mt(px(2.)).child(
                    Checkbox::new(
                        ("inbox-detail-todo", block_id.0),
                        if block.checked {
                            ToggleState::Selected
                        } else {
                            ToggleState::Unselected
                        },
                    )
                    .fill()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.document.toggle_checked(block_id);
                        this.save_body(cx);
                        cx.notify();
                    })),
                ),
            ),
            BlockType::Bullet => row.child(
                div()
                    .flex_none()
                    .mt(px(8.))
                    .size(px(5.))
                    .rounded_full()
                    .bg(Color::Muted.color(cx)),
            ),
            BlockType::Quote => row.child(
                div()
                    .flex_none()
                    .self_stretch()
                    .my_0p5()
                    .w(px(3.))
                    .rounded_full()
                    .bg(cx.theme().colors().text_accent),
            ),
            _ => row,
        };

        let content = match block.block_type {
            BlockType::Divider => div()
                .w_full()
                .py_1()
                .child(Divider::horizontal())
                .into_any_element(),
            BlockType::Code => self.render_code_block(block, cx),
            _ if block.text.is_empty() => Label::new(if is_last {
                "Печатай, или «/» для блока"
            } else {
                "Пустая строка"
            })
            .color(Color::Placeholder)
            .into_any_element(),
            _ => {
                let markdown = self.read_markdown_entity(block, cx);
                let style = Self::markdown_style(block, window, cx);
                MarkdownElement::new(markdown, style).into_any_element()
            }
        };

        row
            // TODO(Task 8): clicking the content starts editing this block.
            .child(div().flex_1().min_w_0().child(content))
            .child(
                div().flex_none().child(
                    // TODO(Task 9): the block actions (grip) menu.
                    IconButton::new(("inbox-detail-grip", block_id.0), IconName::Ellipsis)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .visible_on_hover("detail-block"),
                ),
            )
            .into_any_element()
    }

    fn render_body(&mut self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let blocks = self.document.blocks().to_vec();
        let last_index = blocks.len().saturating_sub(1);

        let mut body = v_flex()
            .id("inbox-detail-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .px_2()
            .py_3();
        for (index, block) in blocks.iter().enumerate() {
            body = body.child(self.render_block(block, index == last_index, window, cx));
        }
        body.child(
            div()
                .id("inbox-detail-trailing")
                .min_h(px(70.))
                .mt_1()
                .px_1()
                .py_2()
                .cursor_text()
                // TODO(Task 8): append a paragraph and start editing on click.
                .child(
                    Label::new("Кликни, чтобы продолжить — печатай или «/» для блока")
                        .size(LabelSize::Small)
                        .color(Color::Placeholder),
                ),
        )
    }
}

impl Focusable for InboxDetailView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for InboxDetailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let item = self.store.read(cx).item(&self.item_id).cloned();

        v_flex()
            .key_context("InboxDetail")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| {
                cx.emit(InboxDetailEvent::Closed);
            }))
            .size_full()
            .bg(cx.theme().colors().panel_background)
            // The item can briefly be gone while the delete event is still in
            // flight; `Closed` is already on its way in that case.
            .when_some(item, |this, item| {
                this.child(self.render_header(item.is_cleared(), cx))
                    .child(self.render_title(&item, cx))
                    .child(self.render_body(window, cx))
            })
    }
}
