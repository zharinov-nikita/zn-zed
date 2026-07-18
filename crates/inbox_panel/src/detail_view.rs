//! The detail view of a single inbox item: a title editor, a meta line and
//! the item's markdown body as editable [`Block`]s. Rendered by the panel as
//! a full-panel overlay. Clicking a block opens the single live editor on
//! it; Enter splits, Backspace at the start merges, Escape commits. The
//! slash/grip menus arrive in Task 9.

use std::collections::HashMap;

use editor::{Editor, EditorElement, EditorEvent, EditorStyle, MultiBufferOffset};
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontStyle, FontWeight,
    IntoElement, ParentElement, Render, ScrollHandle, StrikethroughStyle, Styled, Subscription,
    TextStyle, TextStyleRefinement, UnderlineStyle, WeakEntity, Window,
};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{Checkbox, Divider, Tab, ToggleState, Tooltip, prelude::*};
use workspace::Workspace;

use crate::block::{Block, BlockDocument, BlockId, BlockType, CaretPos, EditTarget};
use crate::inbox_model::{InboxItem, ItemId, format_age, now_unix, type_color};
use crate::inbox_store::{InboxStore, InboxStoreEvent};
use crate::open_capture_context;

pub enum InboxDetailEvent {
    /// The view wants to be closed (back button, Escape, or its item is gone).
    Closed,
}

impl EventEmitter<InboxDetailEvent> for InboxDetailView {}

/// The single live block editor. At most one block is edited at a time;
/// dropping this state drops the editor and its subscriptions.
struct EditingState {
    block_id: BlockId,
    editor: Entity<Editor>,
    _subscriptions: Vec<Subscription>,
}

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
    /// The block currently being edited, if any.
    editing: Option<EditingState>,
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
            editing: None,
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
                // formatting-only differences don't count). While a block is
                // being edited the in-progress edit wins and the resync is
                // skipped; the external change is overwritten by the next
                // save. This is a known compromise — the store-side dirty
                // guard already protects unsaved local edits.
                if self.editing.is_none() {
                    let new_document =
                        BlockDocument::from_markdown(item.body.as_deref().unwrap_or_default());
                    if new_document.to_markdown() != self.document.to_markdown() {
                        self.document = new_document;
                        self.read_markdown.clear();
                    }
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

    /// Starts editing block `id`, placing the caret at `caret`. Creates the
    /// single live editor for the block (committing any previous one first);
    /// when the block is already being edited, only the caret moves.
    /// `Divider` blocks are not editable.
    fn start_editing(
        &mut self,
        id: BlockId,
        caret: CaretPos,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(block) = self.document.block(id) else {
            return;
        };
        if block.block_type == BlockType::Divider {
            return;
        }
        let block_type = block.block_type;
        let text = block.text.clone();

        if let Some(state) = &self.editing
            && state.block_id == id
        {
            // Already editing this block (e.g. Enter on an empty list item
            // converted it to a paragraph in place): just move the caret.
            // The type may have changed, but the editor style is recomputed
            // from the block on every render, so the editor can stay.
            let editor = state.editor.clone();
            Self::place_caret(&editor, caret, window, cx);
            window.focus(&editor.focus_handle(cx), cx);
            cx.notify();
            return;
        }
        self.commit_editing(cx);

        let editor = cx.new(|cx| {
            let mut editor = if block_type == BlockType::Code {
                Editor::auto_height(3, 128, window, cx)
            } else {
                Editor::auto_height(1, 128, window, cx)
            };
            editor.set_placeholder_text("Печатай, или «/» для блока", window, cx);
            editor.set_text(text, window, cx);
            editor
        });
        Self::place_caret(&editor, caret, window, cx);
        let subscriptions = vec![cx.subscribe_in(&editor, window, Self::handle_block_editor_event)];
        window.focus(&editor.focus_handle(cx), cx);
        self.editing = Some(EditingState {
            block_id: id,
            editor,
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    /// Stops editing. The document is kept in sync on every `BufferEdited`,
    /// but a final resync guards against an edit whose event has not been
    /// delivered yet.
    fn commit_editing(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.editing.take() else {
            return;
        };
        let text = state.editor.read(cx).text(cx);
        if self
            .document
            .block(state.block_id)
            .is_some_and(|block| block.text != text)
        {
            self.document.apply_text(state.block_id, &text);
            self.save_body(cx);
        }
        cx.notify();
    }

    /// Places the caret in `editor` according to `caret`. All offsets the
    /// block model produces are char boundaries; `Offset` is additionally
    /// clamped to the text length.
    fn place_caret(editor: &Entity<Editor>, caret: CaretPos, window: &mut Window, cx: &mut App) {
        editor.update(cx, |editor, cx| {
            let len = editor.text(cx).len();
            let offset = match caret {
                CaretPos::Start => 0,
                CaretPos::End => len,
                CaretPos::Offset(offset) => offset.min(len),
            };
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([MultiBufferOffset(offset)..MultiBufferOffset(offset)]);
            });
        });
    }

    fn handle_block_editor_event(
        &mut self,
        editor: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = &self.editing else {
            return;
        };
        // Guard against stale events, e.g. the Blurred of an editor that was
        // already replaced.
        if state.editor.entity_id() != editor.entity_id() {
            return;
        }
        let block_id = state.block_id;
        match event {
            EditorEvent::BufferEdited => {
                let text = editor.read(cx).text(cx);
                // TODO(Task 9): detect a typed "/" here and open the slash
                // menu for the block.
                let target = self.document.apply_text(block_id, &text);
                self.save_body(cx);
                if let Some(target) = target {
                    // Multiline text (paste, shift-enter) was split off into
                    // new blocks. The editor still holds the multiline text,
                    // so drop it without the commit-time resync and continue
                    // editing at the end of the inserted blocks.
                    self.editing = None;
                    self.start_editing(target.block, target.caret, window, cx);
                }
                cx.notify();
            }
            EditorEvent::Blurred => {
                self.commit_editing(cx);
            }
            _ => {}
        }
    }

    /// Enter in the active block: a newline inside `Code` blocks, a block
    /// split everywhere else.
    fn handle_block_confirm(
        &mut self,
        _: &menu::Confirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = &self.editing else {
            cx.propagate();
            return;
        };
        // TODO(Task 9): when the slash menu is open, Enter applies the
        // selected entry instead.
        let block_id = state.block_id;
        let editor = state.editor.clone();
        let Some(block) = self.document.block(block_id) else {
            return;
        };

        if block.block_type == BlockType::Code {
            editor.update(cx, |editor, cx| {
                editor.newline(&editor::actions::Newline, window, cx);
            });
            return;
        }

        let head = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let selection = editor.selections.newest::<MultiBufferOffset>(&snapshot);
            if !selection.is_empty() {
                // Enter with a selection first deletes it, like a newline
                // would in a regular editor.
                editor.insert("", window, cx);
            }
            let snapshot = editor.display_snapshot(cx);
            let selection = editor.selections.newest::<MultiBufferOffset>(&snapshot);
            selection.head().0
        });

        // The document is synced on `BufferEdited`, but sync from the editor
        // text directly so this does not depend on event delivery order.
        let text = editor.read(cx).text(cx);
        if let Some(target) = self.document.apply_text(block_id, &text) {
            // Multiline text slipped in: restructure instead of splitting.
            self.editing = None;
            self.save_body(cx);
            self.start_editing(target.block, target.caret, window, cx);
            return;
        }
        if let Some(target) = self.document.split(block_id, head) {
            if target.block != block_id {
                // Drop the editor without the commit-time resync: the block
                // now holds only the pre-split prefix of the editor text.
                self.editing = None;
            }
            self.save_body(cx);
            self.start_editing(target.block, target.caret, window, cx);
        }
    }

    /// Backspace with an empty selection at offset 0 merges the block into
    /// its predecessor (or converts it to a paragraph); everything else is
    /// the editor's ordinary backspace.
    fn handle_block_backspace(
        &mut self,
        _: &editor::actions::Backspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = &self.editing else {
            cx.propagate();
            return;
        };
        let block_id = state.block_id;
        let editor = state.editor.clone();
        let (selection_empty, head) = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let selection = editor.selections.newest::<MultiBufferOffset>(&snapshot);
            (selection.is_empty(), selection.head().0)
        });
        if !selection_empty || head != 0 {
            cx.propagate();
            return;
        }
        match self.document.backspace_at_start(block_id) {
            Some(target) => {
                if target.block != block_id {
                    // The block was merged away; its text is already part of
                    // the predecessor, so skip the commit-time resync.
                    self.editing = None;
                }
                self.save_body(cx);
                self.start_editing(target.block, target.caret, window, cx);
                cx.stop_propagation();
            }
            // E.g. a paragraph at the very start of the document: let the
            // editor do its own (no-op) backspace.
            None => cx.propagate(),
        }
    }

    /// Escape commits the edit and returns focus to the view.
    fn handle_block_cancel(
        &mut self,
        _: &editor::actions::Cancel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing.is_none() {
            cx.propagate();
            return;
        }
        // TODO(Task 9): Escape first closes the slash menu when it is open.
        self.commit_editing(cx);
        window.focus(&self.focus_handle, cx);
        cx.stop_propagation();
    }

    /// Enter in the title editor moves editing into the first editable
    /// block of the body.
    fn handle_title_confirm(
        &mut self,
        _: &menu::Confirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let first_editable = self
            .document
            .blocks()
            .iter()
            .find(|block| block.block_type != BlockType::Divider)
            .map(|block| block.id);
        if let Some(id) = first_editable {
            self.start_editing(id, CaretPos::Start, window, cx);
        }
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

    /// The block-level text style shared by the read-mode markdown render
    /// and the live editor: UI font as the base, with heading size, quote
    /// italics, todo strikethrough or (for `Code`) the buffer font on top.
    fn block_text_style(block: &Block, window: &Window, cx: &App) -> TextStyle {
        let theme_settings = ThemeSettings::get_global(cx);
        let colors = cx.theme().colors();

        let mut text_style = window.text_style();
        text_style.refine(&TextStyleRefinement {
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
            BlockType::Code => TextStyleRefinement {
                font_family: Some(theme_settings.buffer_font.family.clone()),
                font_fallbacks: theme_settings.buffer_font.fallbacks.clone(),
                font_features: Some(theme_settings.buffer_font.features.clone()),
                font_size: Some(rems(0.875).into()),
                ..Default::default()
            },
            _ => TextStyleRefinement::default(),
        };
        text_style.refine(&refinement);
        text_style
    }

    /// The style of the live block editor, mirroring the block's read-mode
    /// look.
    fn editor_style(block: &Block, window: &Window, cx: &App) -> EditorStyle {
        let colors = cx.theme().colors();
        EditorStyle {
            background: if block.block_type == BlockType::Code {
                colors.editor_background
            } else {
                gpui::transparent_black()
            },
            local_player: cx.theme().players().local(),
            text: Self::block_text_style(block, window, cx),
            syntax: cx.theme().syntax().clone(),
            ..Default::default()
        }
    }

    /// The read-mode markdown style of a text block. The block text is a
    /// single line with the markdown prefix already stripped, so block-level
    /// styling (heading size, quote italics, todo strikethrough) comes from
    /// the base text style rather than from markdown parsing.
    fn markdown_style(block: &Block, window: &Window, cx: &App) -> MarkdownStyle {
        let theme_settings = ThemeSettings::get_global(cx);
        let colors = cx.theme().colors();
        let base_text_style = Self::block_text_style(block, window, cx);

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
            // Enter in the title editor falls through as `menu::Confirm`
            // (plain Enter is unbound in auto-height editors) and moves
            // editing into the first block of the body.
            .on_action(cx.listener(Self::handle_title_confirm))
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

        let editing_editor = self
            .editing
            .as_ref()
            .filter(|state| state.block_id == block_id)
            .map(|state| state.editor.clone());

        let content = if let Some(editor) = editing_editor.as_ref() {
            let element = EditorElement::new(editor, Self::editor_style(block, window, cx));
            if block.block_type == BlockType::Code {
                v_flex()
                    .w_full()
                    .my_1()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .bg(cx.theme().colors().editor_background)
                    .child(element)
                    .into_any_element()
            } else {
                div().w_full().child(element).into_any_element()
            }
        } else {
            match block.block_type {
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
            }
        };

        // The content zone: the live editor with its key handlers while the
        // block is being edited, otherwise the read render, clickable to
        // start editing (dividers are not editable).
        let content_zone = if editing_editor.is_some() {
            div()
                .flex_1()
                .min_w_0()
                // Plain Enter is unbound in auto-height editors and falls
                // through as `menu::Confirm`; Backspace and Escape are
                // intercepted in the capture phase before the editor.
                .on_action(cx.listener(Self::handle_block_confirm))
                .capture_action(cx.listener(Self::handle_block_backspace))
                .capture_action(cx.listener(Self::handle_block_cancel))
                .child(content)
                .into_any_element()
        } else if block.block_type == BlockType::Divider {
            div().flex_1().min_w_0().child(content).into_any_element()
        } else {
            div()
                .id(("inbox-detail-block-content", block_id.0))
                .flex_1()
                .min_w_0()
                .cursor_text()
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.start_editing(block_id, CaretPos::End, window, cx);
                }))
                .child(content)
                .into_any_element()
        };

        row.child(content_zone)
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
                .on_click(cx.listener(|this, _, window, cx| {
                    // Reuse a trailing empty paragraph instead of stacking
                    // new ones.
                    let target = match this.document.blocks().last() {
                        Some(last)
                            if last.block_type == BlockType::Paragraph && last.text.is_empty() =>
                        {
                            EditTarget {
                                block: last.id,
                                caret: CaretPos::Start,
                            }
                        }
                        _ => {
                            let target = this.document.append_paragraph();
                            this.save_body(cx);
                            target
                        }
                    };
                    this.start_editing(target.block, target.caret, window, cx);
                }))
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
