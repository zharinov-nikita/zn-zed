//! The detail view of a single inbox item: a title editor, a meta line and
//! the item's markdown body as editable [`Block`]s. Rendered by the panel as
//! a full-panel overlay. Clicking a block opens the single live editor on
//! it; Enter splits, Backspace at the start merges, Escape commits. Typing
//! "/" in an empty block opens the slash menu; the grip button on each row
//! opens the block actions menu.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use editor::{Editor, EditorElement, EditorEvent, EditorStyle, MultiBufferOffset};
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, FontStyle, FontWeight, IntoElement, MouseButton, ParentElement, Pixels, Point,
    Render, ScrollHandle, StrikethroughStyle, Styled, Subscription, TextStyle, TextStyleRefinement,
    UnderlineStyle, WeakEntity, Window, anchored, canvas, deferred, point,
};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{Checkbox, ContextMenu, ContextMenuEntry, Divider, Tab, ToggleState, Tooltip, prelude::*};
use workspace::Workspace;

use crate::block::{Block, BlockDocument, BlockId, BlockType, CaretPos, EditTarget};
use crate::inbox_model::{InboxItem, ItemId, format_age, now_unix, type_color};
use crate::inbox_store::{InboxStore, InboxStoreEvent};
use crate::open_capture_context;
use crate::slash_menu::{self, SlashEntry, SlashMenuState};

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
    /// The open slash menu, if any. Invariant: only ever open for the block
    /// currently being edited; closed whenever editing moves or stops.
    slash_menu: Option<SlashMenuState>,
    /// Window bounds of the rendered block rows, written during paint and
    /// read one frame later to anchor the slash menu popup.
    block_bounds: Rc<RefCell<HashMap<BlockId, Bounds<Pixels>>>>,
    /// The open grip (block actions) context menu.
    grip_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
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
            editor.set_placeholder_text("Item title", window, cx);
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
            slash_menu: None,
            block_bounds: Rc::default(),
            grip_menu: None,
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

        // The slash menu follows the edited block; moving to another block
        // closes it.
        if self
            .slash_menu
            .as_ref()
            .is_some_and(|state| state.block_id != id)
        {
            self.slash_menu = None;
        }

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

        // Parity with the read-mode placeholders: only the last block
        // advertises the slash menu.
        let is_last = self
            .document
            .blocks()
            .last()
            .is_some_and(|last| last.id == id);
        let editor = cx.new(|cx| {
            let mut editor = if block_type == BlockType::Code {
                Editor::auto_height(3, 128, window, cx)
            } else {
                Editor::auto_height(1, 128, window, cx)
            };
            editor.set_placeholder_text(
                if is_last {
                    "Type, or «/» for a block"
                } else {
                    "Empty line"
                },
                window,
                cx,
            );
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
        if self
            .slash_menu
            .as_ref()
            .is_some_and(|menu| menu.block_id == state.block_id)
        {
            self.slash_menu = None;
        }
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
                // A freshly-created editor reports its initial `set_text`
                // too (as does clearing an already-empty editor); skip no-op
                // syncs so they don't dirty the store.
                if self
                    .document
                    .block(block_id)
                    .is_some_and(|block| block.text == text)
                {
                    return;
                }
                let target = self.document.apply_text(block_id, &text);
                self.save_body(cx);
                if let Some(target) = target {
                    // Multiline text (paste, shift-enter) was split off into
                    // new blocks. The editor still holds the multiline text,
                    // so drop it without the commit-time resync and continue
                    // editing at the end of the inserted blocks.
                    self.slash_menu = None;
                    self.editing = None;
                    self.start_editing(target.block, target.caret, window, cx);
                } else {
                    self.update_slash_menu(block_id, &text);
                }
                cx.notify();
            }
            EditorEvent::Blurred => {
                // A mousedown inside the slash menu can momentarily steal
                // focus; committing here would tear down the editor under
                // the menu interaction. The menu's apply/close paths finish
                // (or resume) the edit instead.
                if self
                    .slash_menu
                    .as_ref()
                    .is_some_and(|menu| menu.block_id == block_id)
                {
                    return;
                }
                self.commit_editing(cx);
            }
            _ => {}
        }
    }

    /// Opens, retargets or closes the slash menu from the freshly-synced
    /// text of the edited block: the menu is open exactly while the whole
    /// text matches `/\S*` (a "/" followed by no whitespace) and at least
    /// one entry matches the query after the "/".
    fn update_slash_menu(&mut self, block_id: BlockId, text: &str) {
        let len = text
            .strip_prefix('/')
            .filter(|rest| !rest.contains(char::is_whitespace))
            .map_or(0, |query| slash_menu::filtered(query).len());
        if len == 0 {
            self.slash_menu = None;
            return;
        }
        match &mut self.slash_menu {
            Some(state) if state.block_id == block_id => {
                // The list may have shrunk; keep the selection valid.
                state.selected = state.selected.min(len - 1);
            }
            _ => {
                self.slash_menu = Some(SlashMenuState {
                    block_id,
                    selected: 0,
                });
            }
        }
    }

    /// The currently selected entry of the open slash menu, if the menu is
    /// open for the block being edited and has any matches.
    fn selected_slash_entry(&self) -> Option<&'static SlashEntry> {
        let state = self.slash_menu.as_ref()?;
        if self.editing.as_ref().map(|editing| editing.block_id) != Some(state.block_id) {
            return None;
        }
        let block = self.document.block(state.block_id)?;
        let query = block.text.strip_prefix('/').unwrap_or("");
        let entries = slash_menu::filtered(query);
        let last = entries.len().checked_sub(1)?;
        entries.get(state.selected.min(last)).copied()
    }

    /// Moves the slash menu selection by `delta`, clamped to the filtered
    /// list. Returns `false` (untouched) when the menu is not open.
    fn step_slash_selection(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let Some(state) = self.slash_menu.as_ref() else {
            return false;
        };
        let Some(block) = self.document.block(state.block_id) else {
            return false;
        };
        let query = block.text.strip_prefix('/').unwrap_or("");
        let len = slash_menu::filtered(query).len();
        if len == 0 {
            return false;
        }
        if let Some(state) = self.slash_menu.as_mut() {
            let current = state.selected.min(len - 1) as isize;
            state.selected = (current + delta).clamp(0, len as isize - 1) as usize;
        }
        cx.notify();
        true
    }

    /// Up/Down in the active block editor move the slash menu selection
    /// while the menu is open, and are the editor's ordinary cursor motion
    /// otherwise.
    fn handle_block_move_up(
        &mut self,
        _: &zed_actions::editor::MoveUp,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.step_slash_selection(-1, cx) {
            cx.propagate();
        }
    }

    fn handle_block_move_down(
        &mut self,
        _: &zed_actions::editor::MoveDown,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.step_slash_selection(1, cx) {
            cx.propagate();
        }
    }

    /// Applies a slash menu entry: converts the block to the entry's type
    /// and clears the "/query" text.
    fn apply_slash(
        &mut self,
        entry: &'static SlashEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.slash_menu.take() else {
            return;
        };
        let block_id = state.block_id;
        cx.notify();
        if self.editing.as_ref().map(|editing| editing.block_id) != Some(block_id) {
            return;
        }

        if entry.block_type == BlockType::Divider {
            // `convert` restructures the document (clears the block, inserts
            // a paragraph after it), so drop the editor — which still holds
            // the "/query" text — without the commit-time resync, then start
            // editing the inserted paragraph.
            self.editing = None;
            let target = self.document.convert(block_id, BlockType::Divider);
            self.save_body(cx);
            if let Some(target) = target {
                self.start_editing(target.block, target.caret, window, cx);
            }
            return;
        }

        // Update the document first, then the editor: clearing the editor
        // fires `BufferEdited`, which must see editor text == block text
        // ("" == "") so it doesn't write the stale "/query" back.
        self.document.set_text(block_id, String::new());
        self.document.convert(block_id, entry.block_type);

        if entry.block_type == BlockType::Code {
            // A `Code` editor differs (min_lines, style), so recreate it.
            // Drop the old editor without resync — it still holds "/query".
            self.editing = None;
            self.save_body(cx);
            self.start_editing(block_id, CaretPos::Start, window, cx);
            return;
        }

        if let Some(state) = &self.editing {
            let editor = state.editor.clone();
            editor.update(cx, |editor, cx| editor.set_text("", window, cx));
            window.focus(&editor.focus_handle(cx), cx);
        }
        self.save_body(cx);
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
        let block_id = state.block_id;
        let editor = state.editor.clone();
        if let Some(entry) = self.selected_slash_entry() {
            // Enter with the slash menu open applies the selected entry
            // instead of splitting the block.
            self.apply_slash(entry, window, cx);
            return;
        }
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
        // Sync the editor text into the document before mutating it (as in
        // confirm), so the merge below doesn't depend on `BufferEdited`
        // delivery order.
        let text = editor.read(cx).text(cx);
        if self
            .document
            .block(block_id)
            .is_some_and(|block| block.text != text)
            && let Some(target) = self.document.apply_text(block_id, &text)
        {
            // Multiline text slipped in: restructure instead of merging.
            self.slash_menu = None;
            self.editing = None;
            self.save_body(cx);
            self.start_editing(target.block, target.caret, window, cx);
            cx.stop_propagation();
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
        if self.slash_menu.take().is_some() {
            // Escape only closes the slash menu; the "/query" text stays in
            // the block and editing continues.
            if let Some(state) = &self.editing {
                window.focus(&state.editor.focus_handle(cx), cx);
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        self.commit_editing(cx);
        window.focus(&self.focus_handle, cx);
        cx.stop_propagation();
    }

    /// Opens the grip (block actions) context menu for block `id` at
    /// `position`.
    fn deploy_grip_menu(
        &mut self,
        id: BlockId,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Close the slash menu first: it would otherwise keep suppressing
        // the editor's blur commit while the grip menu holds focus.
        self.slash_menu = None;
        let view = cx.weak_entity();
        let context_menu = ContextMenu::build(window, cx, |menu, _, _| {
            menu.item(
                ContextMenuEntry::new("Add below")
                    .icon(IconName::Plus)
                    .icon_color(Color::Muted)
                    .handler({
                        let view = view.clone();
                        move |window, cx| {
                            view.update(cx, |this, cx| {
                                let target = this.document.insert_after(id);
                                this.save_body(cx);
                                this.start_editing(target.block, target.caret, window, cx);
                            })
                            .ok();
                        }
                    }),
            )
            .item(
                ContextMenuEntry::new("Duplicate")
                    .icon(IconName::Copy)
                    .icon_color(Color::Muted)
                    .handler({
                        let view = view.clone();
                        move |_, cx| {
                            view.update(cx, |this, cx| {
                                if this.document.duplicate(id).is_some() {
                                    this.save_body(cx);
                                    cx.notify();
                                }
                            })
                            .ok();
                        }
                    }),
            )
            .item(
                ContextMenuEntry::new("Move up")
                    .icon(IconName::ArrowUp)
                    .icon_color(Color::Muted)
                    .handler({
                        let view = view.clone();
                        move |_, cx| {
                            view.update(cx, |this, cx| {
                                if this.document.move_block(id, -1) {
                                    this.save_body(cx);
                                    cx.notify();
                                }
                            })
                            .ok();
                        }
                    }),
            )
            .item(
                ContextMenuEntry::new("Move down")
                    .icon(IconName::ArrowDown)
                    .icon_color(Color::Muted)
                    .handler({
                        let view = view.clone();
                        move |_, cx| {
                            view.update(cx, |this, cx| {
                                if this.document.move_block(id, 1) {
                                    this.save_body(cx);
                                    cx.notify();
                                }
                            })
                            .ok();
                        }
                    }),
            )
            .separator()
            .item(
                ContextMenuEntry::new("Delete block")
                    .icon(IconName::Trash)
                    .icon_color(Color::Error)
                    .handler({
                        let view = view.clone();
                        move |_, cx| {
                            view.update(cx, |this, cx| {
                                if this
                                    .editing
                                    .as_ref()
                                    .is_some_and(|state| state.block_id == id)
                                {
                                    // The edited block is going away: drop
                                    // the editor without the commit-time
                                    // resync. The remove target is only a
                                    // focus hint — don't start editing it.
                                    this.editing = None;
                                }
                                if this
                                    .slash_menu
                                    .as_ref()
                                    .is_some_and(|menu| menu.block_id == id)
                                {
                                    this.slash_menu = None;
                                }
                                if this.document.remove(id).is_some() {
                                    this.save_body(cx);
                                }
                                cx.notify();
                            })
                            .ok();
                        }
                    }),
            )
        });

        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&context_menu, |this, _, _: &DismissEvent, cx| {
            this.grip_menu.take();
            cx.notify();
        });
        self.grip_menu = Some((context_menu, position, subscription));
        cx.notify();
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
        let (toggle_icon, toggle_label, toggle_color, toggle_tooltip) = if cleared {
            (IconName::Check, "Done", Color::Created, "Return to inbox")
        } else {
            (IconName::Circle, "Clear", Color::Muted, "Mark as cleared")
        };

        h_flex()
            .flex_none()
            .h(Tab::container_height(cx))
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Button::new("inbox-detail-back", "Inbox")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .start_icon(
                        Icon::new(IconName::ChevronLeft)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text("Back to inbox"))
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
                    .tooltip(Tooltip::text(toggle_tooltip))
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
                    .tooltip(Tooltip::text("Delete item"))
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
        let type_info: Option<(SharedString, gpui::Hsla)> = {
            let store = self.store.read(cx);
            store.resolve_kind(item).map(|inbox_type| {
                (
                    SharedString::from(inbox_type.label.clone()),
                    type_color(&inbox_type.color, cx),
                )
            })
        };

        let mut meta = h_flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .pl(px(16.))
            .text_xs()
            .font_buffer(cx)
            .text_color(cx.theme().colors().text_placeholder);
        if let Some((type_label, _)) = type_info.clone() {
            meta = meta.child(type_label);
        }
        if let Some(created) = item.created {
            let age = format_age(created, now_unix());
            let captured = if age == "now" {
                "captured just now".to_string()
            } else {
                format!("captured {age} ago")
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
            format!("{done}/{total} subtasks")
        } else {
            "no subtasks".to_string()
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
            .child({
                let mut title_row = h_flex().items_start().gap_2();
                if let Some((_, type_square_color)) = type_info {
                    title_row = title_row.child(
                        div()
                            .flex_none()
                            .mt(px(5.))
                            .size(px(8.))
                            .rounded_xs()
                            .bg(type_square_color),
                    );
                }
                title_row.child(div().flex_1().min_w_0().child(self.title_editor.clone()))
            })
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
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            // Records the row's window bounds during paint; the slash menu
            // popup is anchored to them a frame later.
            .child(
                canvas(
                    {
                        let block_bounds = self.block_bounds.clone();
                        move |bounds, _, _| {
                            block_bounds.borrow_mut().insert(block_id, bounds);
                        }
                    },
                    |_, _, _, _| {},
                )
                .size_full()
                .absolute()
                .top_0()
                .left_0(),
            );

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
                    "Type, or «/» for a block"
                } else {
                    "Empty line"
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
                // through as `menu::Confirm`; Backspace, Escape and (for the
                // slash menu) Up/Down are intercepted in the capture phase
                // before the editor.
                .on_action(cx.listener(Self::handle_block_confirm))
                .capture_action(cx.listener(Self::handle_block_backspace))
                .capture_action(cx.listener(Self::handle_block_cancel))
                .capture_action(cx.listener(Self::handle_block_move_up))
                .capture_action(cx.listener(Self::handle_block_move_down))
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
                    IconButton::new(("inbox-detail-grip", block_id.0), IconName::Ellipsis)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .visible_on_hover("detail-block")
                        .tooltip(Tooltip::text("Block actions"))
                        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                            this.deploy_grip_menu(block_id, event.position(), window, cx);
                        })),
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
                    Label::new("Click to continue — type or «/» for a block")
                        .size(LabelSize::Small)
                        .color(Color::Placeholder),
                ),
        )
    }

    /// The slash menu popup, anchored to the bottom-left of the edited
    /// block's row. `None` while the menu is closed (or the block's bounds
    /// have not been recorded yet).
    fn render_slash_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.slash_menu.as_ref()?;
        let block_id = state.block_id;
        let query = self
            .document
            .block(block_id)?
            .text
            .strip_prefix('/')
            .unwrap_or("")
            .to_string();
        let entries = slash_menu::filtered(&query);
        if entries.is_empty() {
            return None;
        }
        let selected = state.selected.min(entries.len() - 1);
        let bounds = *self.block_bounds.borrow().get(&block_id)?;
        let position = bounds.origin + point(px(24.), bounds.size.height);

        let selected_bg = cx.theme().colors().element_selected;
        let badge_bg = cx.theme().colors().editor_background;
        let mut list = v_flex()
            .id("inbox-slash-menu")
            .occlude()
            .elevation_2(cx)
            .min_w(px(230.))
            .max_h(px(290.))
            .overflow_y_scroll()
            .py_1()
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.slash_menu = None;
                // If the click also moved focus away, the skipped-while-open
                // Blurred already went by: finish the edit here.
                if let Some(state) = &this.editing
                    && !state.editor.focus_handle(cx).is_focused(window)
                {
                    this.commit_editing(cx);
                }
                cx.notify();
            }))
            .child(
                div().px_2().py_0p5().child(
                    Label::new("BLOCK TYPE")
                        .size(LabelSize::XSmall)
                        .weight(FontWeight::BOLD)
                        .color(Color::Placeholder),
                ),
            );
        for (index, entry) in entries.into_iter().enumerate() {
            list = list.child(
                h_flex()
                    .id(("inbox-slash-entry", index))
                    .mx_1()
                    .px_1p5()
                    .py_1()
                    .gap_2()
                    .rounded_sm()
                    .when(index == selected, |this| this.bg(selected_bg))
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered
                            && let Some(state) = &mut this.slash_menu
                            && state.selected != index
                        {
                            state.selected = index;
                            cx.notify();
                        }
                    }))
                    // Mousedown, not click: it must win over the editor's
                    // blur (see the `Blurred` guard).
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            window.prevent_default();
                            this.apply_slash(entry, window, cx);
                        }),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .size(px(26.))
                            .items_center()
                            .justify_center()
                            .rounded_sm()
                            .bg(badge_bg)
                            .font_buffer(cx)
                            .text_xs()
                            .font_weight(FontWeight::BOLD)
                            .text_color(Color::Muted.color(cx))
                            .child(entry.glyph),
                    )
                    .child(
                        v_flex()
                            .child(Label::new(entry.label).size(LabelSize::Small))
                            .child(
                                Label::new(entry.hint)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Placeholder),
                            ),
                    ),
            );
        }

        Some(
            deferred(
                anchored()
                    .position(position)
                    .anchor(gpui::Anchor::TopLeft)
                    .child(list),
            )
            .with_priority(1)
            .into_any_element(),
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
            .children(self.render_slash_menu(cx))
            .children(self.grip_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::Path;
    use std::rc::Rc;

    use fs::FakeFs;
    use gpui::{AppContext as _, KeyBinding, TestAppContext, VisualTestContext};
    use pretty_assertions::assert_eq;
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    use super::*;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            editor::init(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    /// The subset of the default keymaps (`assets/keymaps/default-*.json`)
    /// the block editor's keyboard handling relies on, bound with the same
    /// contexts: Enter and Escape resolve to the *global*
    /// `menu::Confirm`/`menu::Cancel` (plain Enter is deliberately unbound
    /// in `Editor && mode == auto_height`, so it falls through to the
    /// global binding), while Backspace/Escape/Up/Down are the
    /// `Editor`-context bindings the view intercepts in the capture phase.
    fn bind_default_keys(cx: &mut App) {
        cx.bind_keys([
            KeyBinding::new("enter", menu::Confirm, None),
            KeyBinding::new("escape", menu::Cancel, None),
            KeyBinding::new("backspace", editor::actions::Backspace, Some("Editor")),
            KeyBinding::new("escape", editor::actions::Cancel, Some("Editor")),
            KeyBinding::new("up", zed_actions::editor::MoveUp, Some("Editor")),
            KeyBinding::new("down", zed_actions::editor::MoveDown, Some("Editor")),
        ]);
    }

    /// Builds an `InboxStore` on a `FakeFs` project with a single item
    /// whose body is `body`, opens an `InboxDetailView` on it as the root
    /// of a test window and returns the pieces plus the window's
    /// `VisualTestContext` for keystroke simulation.
    async fn build_detail_view<'a>(
        body: Option<&str>,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<InboxStore>,
        ItemId,
        Entity<InboxDetailView>,
        &'a mut VisualTestContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let project = Project::test(fs.clone(), [path!("/root").as_ref() as &Path], cx).await;
        let store = cx.new(|cx| InboxStore::new(project, fs, cx));
        cx.run_until_parked();
        let item_id = store.update(cx, |store, cx| {
            let id = store.capture("Entry".to_string(), None, None, cx);
            store.set_body(&id, body.map(str::to_string), cx);
            id
        });
        cx.update(bind_default_keys);
        let window = cx.add_window(|window, cx| {
            InboxDetailView::new(
                store.clone(),
                item_id.clone(),
                WeakEntity::new_invalid(),
                window,
                cx,
            )
        });
        let view = window.root(cx).unwrap();
        let cx = VisualTestContext::from_window(*window, cx).into_mut();
        cx.run_until_parked();
        (store, item_id, view, cx)
    }

    /// Wraps an [`InboxDetailView`] in a node carrying
    /// `key_context("InboxPanel")` and a `Capture` handler, mirroring how the
    /// production panel renders the detail overlay inside its root element
    /// (which has both). Used to test keymap-context interactions between
    /// the panel and the detail view's editors.
    struct PanelContextWrapper {
        detail: Entity<InboxDetailView>,
        captures: Rc<Cell<usize>>,
    }

    impl Render for PanelContextWrapper {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let captures = self.captures.clone();
            div()
                .key_context("InboxPanel")
                .on_action(move |_: &crate::Capture, _, _| captures.set(captures.get() + 1))
                .size_full()
                .child(self.detail.clone())
        }
    }

    /// Like [`build_detail_view`], but mounts the detail view under a parent
    /// carrying `key_context("InboxPanel")` (as the real panel does) and
    /// additionally binds `enter -> inbox_panel::Capture` in
    /// `capture_context` after the default bindings, matching how the
    /// default keymaps define the capture binding after the global
    /// `enter -> menu::Confirm`. Returns a counter of how many times the
    /// wrapper handled `Capture`.
    async fn build_detail_view_under_panel<'a>(
        body: Option<&str>,
        capture_context: &str,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<InboxDetailView>,
        Rc<Cell<usize>>,
        &'a mut VisualTestContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let project = Project::test(fs.clone(), [path!("/root").as_ref() as &Path], cx).await;
        let store = cx.new(|cx| InboxStore::new(project, fs, cx));
        cx.run_until_parked();
        let item_id = store.update(cx, |store, cx| {
            let id = store.capture("Entry".to_string(), None, None, cx);
            store.set_body(&id, body.map(str::to_string), cx);
            id
        });
        cx.update(bind_default_keys);
        cx.update(|cx| {
            cx.bind_keys([KeyBinding::new(
                "enter",
                crate::Capture,
                Some(capture_context),
            )]);
        });
        let captures = Rc::new(Cell::new(0));
        let window = cx.add_window({
            let captures = captures.clone();
            move |window, cx| {
                let detail = cx.new(|cx| {
                    InboxDetailView::new(
                        store.clone(),
                        item_id.clone(),
                        WeakEntity::new_invalid(),
                        window,
                        cx,
                    )
                });
                PanelContextWrapper { detail, captures }
            }
        });
        let wrapper = window.root(cx).unwrap();
        let detail = wrapper.read_with(cx, |wrapper, _| wrapper.detail.clone());
        let cx = VisualTestContext::from_window(*window, cx).into_mut();
        cx.run_until_parked();
        (detail, captures, cx)
    }

    /// Starts editing the `block_index`-th block through the view's own
    /// entry point (the same path a click on the block takes) and lets the
    /// window redraw so the live editor is in the dispatch tree.
    fn begin_editing(
        view: &Entity<InboxDetailView>,
        cx: &mut VisualTestContext,
        block_index: usize,
        caret: CaretPos,
    ) {
        view.update_in(cx, |view, window, cx| {
            let id = view.document.blocks()[block_index].id;
            view.start_editing(id, caret, window, cx);
        });
        cx.run_until_parked();
    }

    fn blocks(
        view: &Entity<InboxDetailView>,
        cx: &mut VisualTestContext,
    ) -> Vec<(BlockType, String)> {
        view.read_with(cx, |view, _| {
            view.document
                .blocks()
                .iter()
                .map(|block| (block.block_type, block.text.clone()))
                .collect()
        })
    }

    fn body_in_store(
        store: &Entity<InboxStore>,
        item_id: &ItemId,
        cx: &mut VisualTestContext,
    ) -> Option<String> {
        store.read_with(cx, |store, _| store.item(item_id).unwrap().body.clone())
    }

    /// The currently edited block and the caret's byte offset in its live
    /// editor.
    fn editing_caret(
        view: &Entity<InboxDetailView>,
        cx: &mut VisualTestContext,
    ) -> (BlockId, usize) {
        let (block_id, editor) = view.read_with(cx, |view, _| {
            let state = view.editing.as_ref().expect("a block should be edited");
            (state.block_id, state.editor.clone())
        });
        let offset = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor
                .selections
                .newest::<MultiBufferOffset>(&snapshot)
                .head()
                .0
        });
        (block_id, offset)
    }

    #[gpui::test]
    async fn test_enter_splits_paragraph_at_caret(cx: &mut TestAppContext) {
        let (store, item_id, view, cx) = build_detail_view(Some("abcdef"), cx).await;
        begin_editing(&view, cx, 0, CaretPos::Offset(3));

        cx.simulate_keystrokes("enter");

        assert_eq!(
            blocks(&view, cx),
            vec![
                (BlockType::Paragraph, "abc".to_string()),
                (BlockType::Paragraph, "def".to_string()),
            ]
        );
        // Editing moved into the new block, caret at its start.
        let second_id = view.read_with(cx, |view, _| view.document.blocks()[1].id);
        assert_eq!(editing_caret(&view, cx), (second_id, 0));
        assert_eq!(
            body_in_store(&store, &item_id, cx),
            Some("abc\ndef".to_string())
        );
    }

    #[gpui::test]
    async fn test_enter_continues_todo_and_exits_on_empty(cx: &mut TestAppContext) {
        let (store, item_id, view, cx) = build_detail_view(Some("- [ ] task"), cx).await;
        begin_editing(&view, cx, 0, CaretPos::End);

        // Enter at the end of a todo continues the list with a new todo.
        cx.simulate_keystrokes("enter");
        assert_eq!(
            blocks(&view, cx),
            vec![
                (BlockType::Todo, "task".to_string()),
                (BlockType::Todo, String::new()),
            ]
        );
        let second_id = view.read_with(cx, |view, _| view.document.blocks()[1].id);
        assert_eq!(editing_caret(&view, cx), (second_id, 0));

        // Enter on the empty continuation exits the list: the same block
        // converts to a paragraph in place.
        cx.simulate_keystrokes("enter");
        assert_eq!(
            blocks(&view, cx),
            vec![
                (BlockType::Todo, "task".to_string()),
                (BlockType::Paragraph, String::new()),
            ]
        );
        assert_eq!(editing_caret(&view, cx), (second_id, 0));
        assert_eq!(
            body_in_store(&store, &item_id, cx),
            Some("- [ ] task\n".to_string())
        );
    }

    #[gpui::test]
    async fn test_backspace_at_start_merges_into_previous(cx: &mut TestAppContext) {
        let (store, item_id, view, cx) = build_detail_view(Some("hello\nworld"), cx).await;
        begin_editing(&view, cx, 1, CaretPos::Start);

        cx.simulate_keystrokes("backspace");

        assert_eq!(
            blocks(&view, cx),
            vec![(BlockType::Paragraph, "helloworld".to_string())]
        );
        // Editing moved into the merged block, caret at the former boundary.
        let first_id = view.read_with(cx, |view, _| view.document.blocks()[0].id);
        assert_eq!(editing_caret(&view, cx), (first_id, "hello".len()));
        assert_eq!(
            body_in_store(&store, &item_id, cx),
            Some("helloworld".to_string())
        );
    }

    #[gpui::test]
    async fn test_escape_commits_edit_then_closes_view(cx: &mut TestAppContext) {
        let (_store, _item_id, view, cx) = build_detail_view(Some("abcdef"), cx).await;
        let closed = Rc::new(Cell::new(0));
        cx.update(|_, cx| {
            cx.subscribe(&view, {
                let closed = closed.clone();
                move |_, event, _| match event {
                    InboxDetailEvent::Closed => closed.set(closed.get() + 1),
                }
            })
            .detach();
        });
        begin_editing(&view, cx, 0, CaretPos::End);
        view.read_with(cx, |view, _| assert!(view.editing.is_some()));

        // The first Escape commits the edit and focuses the view.
        cx.simulate_keystrokes("escape");
        view.read_with(cx, |view, _| assert!(view.editing.is_none()));
        let focus_handle = view.read_with(cx, |view, _| view.focus_handle.clone());
        assert!(cx.update(|window, _| focus_handle.is_focused(window)));
        assert_eq!(closed.get(), 0);

        // The second Escape closes the detail view.
        cx.simulate_keystrokes("escape");
        assert_eq!(closed.get(), 1);
    }

    #[gpui::test]
    async fn test_slash_menu_filters_and_applies_heading(cx: &mut TestAppContext) {
        let (store, item_id, view, cx) = build_detail_view(None, cx).await;
        begin_editing(&view, cx, 0, CaretPos::Start);

        // Typing "/" into the empty paragraph opens the slash menu.
        cx.simulate_keystrokes("/");
        view.read_with(cx, |view, _| {
            let state = view.slash_menu.as_ref().expect("slash menu should open");
            assert_eq!(state.selected, 0);
        });

        // "head" narrows the list down to the two headings.
        cx.simulate_input("head");
        view.read_with(cx, |view, _| {
            let state = view.slash_menu.as_ref().expect("menu should stay open");
            let block = view.document.block(state.block_id).unwrap();
            assert_eq!(block.text, "/head");
            let entries = slash_menu::filtered("head");
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].block_type, BlockType::H1);
            assert_eq!(entries[1].block_type, BlockType::H2);
        });

        // Down moves the selection to the second filtered entry (H2).
        cx.simulate_keystrokes("down");
        view.read_with(cx, |view, _| {
            assert_eq!(view.slash_menu.as_ref().unwrap().selected, 1);
        });

        // Enter applies it: the block becomes an empty H2, the menu closes
        // and the "/head" query never reaches the body.
        cx.simulate_keystrokes("enter");
        assert_eq!(blocks(&view, cx), vec![(BlockType::H2, String::new())]);
        view.read_with(cx, |view, _| {
            assert!(view.slash_menu.is_none());
            assert!(view.editing.is_some(), "editing continues in the block");
        });
        let body = body_in_store(&store, &item_id, cx).expect("body should exist");
        assert!(!body.contains('/'), "query leaked into the body: {body:?}");
        assert_eq!(body, "## ");
    }

    #[gpui::test]
    async fn test_enter_splits_cyrillic_text_on_char_boundary(cx: &mut TestAppContext) {
        let (store, item_id, view, cx) = build_detail_view(Some("привет мир"), cx).await;
        // Byte offset 6 is after "при" (Cyrillic characters are two bytes
        // each in UTF-8). The editor only ever produces char-boundary
        // offsets; mid-character offsets are covered by the block model's
        // own clamping tests.
        begin_editing(&view, cx, 0, CaretPos::Offset(6));

        cx.simulate_keystrokes("enter");

        assert_eq!(
            blocks(&view, cx),
            vec![
                (BlockType::Paragraph, "при".to_string()),
                (BlockType::Paragraph, "вет мир".to_string()),
            ]
        );
        let second_id = view.read_with(cx, |view, _| view.document.blocks()[1].id);
        assert_eq!(editing_caret(&view, cx), (second_id, 0));
        assert_eq!(
            body_in_store(&store, &item_id, cx),
            Some("при\nвет мир".to_string())
        );
    }

    /// Regression test: the production `enter -> inbox_panel::Capture`
    /// binding is scoped as `"InboxCapture > Editor"`, so it must NOT match
    /// the detail view's block editors even though they render under an
    /// ancestor with `key_context("InboxPanel")` (a `>` context predicate
    /// matches the parent at any ancestor depth). Enter in a block editor
    /// must fall through to the global `menu::Confirm` and split the block,
    /// not trigger a capture.
    #[gpui::test]
    async fn test_capture_binding_does_not_hijack_enter_in_detail_blocks(cx: &mut TestAppContext) {
        let (view, captures, cx) =
            build_detail_view_under_panel(Some("abcdef"), "InboxCapture > Editor", cx).await;
        begin_editing(&view, cx, 0, CaretPos::Offset(3));

        cx.simulate_keystrokes("enter");

        assert_eq!(
            blocks(&view, cx),
            vec![
                (BlockType::Paragraph, "abc".to_string()),
                (BlockType::Paragraph, "def".to_string()),
            ],
            "enter in a block editor must split the block (menu::Confirm)"
        );
        assert_eq!(
            captures.get(),
            0,
            "enter in a block editor must not trigger inbox_panel::Capture"
        );
    }

    /// Pins why the capture binding must not use `"InboxPanel > Editor"`:
    /// bound that way it also matches the detail view's block editors
    /// (stack: … InboxPanel … InboxDetail … Editor), wins over the global
    /// `enter -> menu::Confirm`, and hijacks Enter into a spurious capture
    /// instead of a block split.
    #[gpui::test]
    async fn test_panel_scoped_capture_binding_would_hijack_enter(cx: &mut TestAppContext) {
        let (view, captures, cx) =
            build_detail_view_under_panel(Some("abcdef"), "InboxPanel > Editor", cx).await;
        begin_editing(&view, cx, 0, CaretPos::Offset(3));

        cx.simulate_keystrokes("enter");

        assert_eq!(
            blocks(&view, cx),
            vec![(BlockType::Paragraph, "abcdef".to_string())],
            "the hijacked enter must not have split the block"
        );
        assert_eq!(
            captures.get(),
            1,
            "an InboxPanel-scoped binding hijacks enter into Capture"
        );
    }
}
