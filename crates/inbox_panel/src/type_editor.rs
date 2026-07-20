//! The "Lists & Tags" overlay: renaming, recoloring, adding and deleting
//! inbox lists and tags. Both catalogs share the same row/section mechanics
//! ([`CatalogKind`] picks which store methods a row dispatches to). Rendered
//! on top of the whole panel while it is open.

use collections::HashMap;
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, ClickEvent, Context, Entity, FontWeight, Hsla, Pixels, Point, Render, ScrollHandle,
    Subscription, Window,
};
use ui::{ScrollAxes, Scrollbars, Tab, Tooltip, WithScrollbar, prelude::*};

use crate::inbox_model::{CatalogKind, catalog_color};
use crate::inbox_store::InboxStore;
use crate::{InboxPanel, catalog_swatch, entity_confirmation_popover};

/// The overlay's per-kind UI vocabulary: element-id prefixes, section labels
/// and confirmation strings. An inherent impl in this file (the enum's home
/// is `inbox_model`) because it is presentation-only; kept together so
/// adding a catalog kind updates every string in one place.
impl CatalogKind {
    /// Prefix for per-row element ids, so list and tag rows never collide.
    fn id_prefix(self) -> &'static str {
        match self {
            CatalogKind::List => "inbox-list",
            CatalogKind::Tag => "inbox-tag",
        }
    }

    fn section_title(self) -> &'static str {
        match self {
            CatalogKind::List => "YOUR LISTS",
            CatalogKind::Tag => "TAGS",
        }
    }

    fn add_label(self) -> &'static str {
        match self {
            CatalogKind::List => "Add list",
            CatalogKind::Tag => "Add tag",
        }
    }

    fn delete_tooltip(self) -> &'static str {
        match self {
            CatalogKind::List => "Delete list",
            CatalogKind::Tag => "Delete tag",
        }
    }

    fn confirm_title(self) -> &'static str {
        match self {
            CatalogKind::List => "Delete list?",
            CatalogKind::Tag => "Delete tag?",
        }
    }
}

/// Drag payload and ghost view for reordering catalog rows in the editor.
/// Carries its [`CatalogKind`] so a tag row dropped onto a list row (or vice
/// versa) is inert instead of corrupting the other catalog's order.
#[derive(Clone)]
struct DraggedCatalogEntry {
    kind: CatalogKind,
    key: String,
    label: SharedString,
    color: Hsla,
}

impl Render for DraggedCatalogEntry {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_1()
            .px_2()
            .py_0p5()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().element_selected)
            .child(catalog_swatch(self.color))
            .child(Label::new(self.label.clone()).size(LabelSize::Small))
    }
}

/// Editor state of the catalog editor overlay: one single-line rename editor
/// per list and per tag, keyed by catalog kind + entry key. Created when the
/// overlay opens and dropped when it closes.
pub(crate) struct TypeEditorState {
    rename_editors: HashMap<(CatalogKind, String), (Entity<Editor>, Subscription)>,
    /// The catalog entry whose delete action is pending confirmation, plus
    /// the window position of the confirm popover, if any. Only one row (of
    /// either catalog) can be confirming at a time.
    confirming_delete: Option<(CatalogKind, String, Point<Pixels>)>,
    /// Scroll position of the overlay body, shared with its scrollbar.
    scroll_handle: ScrollHandle,
}

impl TypeEditorState {
    pub(crate) fn new(
        store: &Entity<InboxStore>,
        window: &mut Window,
        cx: &mut Context<InboxPanel>,
    ) -> Self {
        let mut this = Self {
            rename_editors: HashMap::default(),
            confirming_delete: None,
            scroll_handle: ScrollHandle::new(),
        };
        this.sync(store, window, cx);
        this
    }

    /// Creates rename editors for entries that don't have one yet and drops
    /// the editors of deleted entries, in both catalogs. Editors of surviving
    /// entries keep their text, so in-progress edits are not clobbered.
    pub(crate) fn sync(
        &mut self,
        store: &Entity<InboxStore>,
        window: &mut Window,
        cx: &mut Context<InboxPanel>,
    ) {
        let entries: Vec<(CatalogKind, String, String)> = {
            let store = store.read(cx);
            [CatalogKind::List, CatalogKind::Tag]
                .into_iter()
                .flat_map(|kind| {
                    store
                        .catalog(kind)
                        .iter()
                        .map(move |entry| (kind, entry.key.clone(), entry.label.clone()))
                })
                .collect()
        };
        if let Some((kind, pending, _)) = &self.confirming_delete
            && !entries
                .iter()
                .any(|(entry_kind, key, _)| entry_kind == kind && key == pending)
        {
            self.confirming_delete = None;
        }
        self.rename_editors.retain(|(kind, key), _| {
            entries
                .iter()
                .any(|(entry_kind, entry_key, _)| entry_kind == kind && entry_key == key)
        });
        for (kind, key, label) in entries {
            if self.rename_editors.contains_key(&(kind, key.clone())) {
                continue;
            }
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_text(label, window, cx);
                editor
            });
            let subscription = cx.subscribe(&editor, {
                let key = key.clone();
                move |this: &mut InboxPanel, editor, event: &EditorEvent, cx| {
                    if let EditorEvent::BufferEdited = event {
                        let label = editor.read(cx).text(cx);
                        this.store
                            .update(cx, |store, cx| store.rename_entry(kind, &key, label, cx));
                    }
                }
            });
            self.rename_editors
                .insert((kind, key), (editor, subscription));
        }
    }

    fn editor(&self, kind: CatalogKind, key: &str) -> Option<&Entity<Editor>> {
        self.rename_editors
            .get(&(kind, key.to_string()))
            .map(|(editor, _)| editor)
    }
}

fn section_label(text: &'static str) -> Label {
    Label::new(text)
        .size(LabelSize::XSmall)
        .weight(FontWeight::BOLD)
        .color(Color::Placeholder)
}

impl InboxPanel {
    pub(crate) fn open_type_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The type editor and the detail view overlays are mutually
        // exclusive: opening one closes the others.
        self.detail = None;
        self.issue_detail = None;
        if self.type_editor.is_none() {
            let store = self.store.clone();
            self.type_editor = Some(TypeEditorState::new(&store, window, cx));
        }
        cx.notify();
    }

    fn render_system_type_row(
        &self,
        label: &'static str,
        color: gpui::Hsla,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .gap_2()
            .px_1()
            .py_0p5()
            .child(div().flex_none().size(px(16.)).rounded_sm().bg(color))
            .child(
                div()
                    .flex_1()
                    .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
            )
            .child(
                Icon::new(IconName::Lock)
                    .size(IconSize::XSmall)
                    .color(Color::Placeholder),
            )
    }

    fn render_catalog_row(
        &self,
        kind: CatalogKind,
        key: &str,
        color: gpui::Hsla,
        editor: Entity<Editor>,
        reorderable: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let prefix = kind.id_prefix();
        // The confirm popover is anchored at the click and rendered at the
        // overlay root (see `render_catalog_delete_confirmation`), matching
        // how the list rows confirm a task deletion.
        let trailing = IconButton::new(
            SharedString::from(format!("{prefix}-delete-{key}")),
            IconName::Close,
        )
        .icon_size(IconSize::XSmall)
        .icon_color(Color::Muted)
        .tooltip(Tooltip::text(kind.delete_tooltip()))
        .on_click(cx.listener({
            let key = key.to_string();
            move |this, event: &ClickEvent, _, cx| {
                if let Some(state) = this.type_editor.as_mut() {
                    state.confirming_delete = Some((kind, key.clone(), event.position()));
                }
                cx.notify();
            }
        }));

        let label = SharedString::from(editor.read(cx).text(cx));
        h_flex()
            .id(SharedString::from(format!("{prefix}-row-{key}")))
            .gap_2()
            .px_1()
            .py_0p5()
            .when(reorderable, |this| {
                // Both the highlight and the drop are gated on the drag
                // coming from this row's own catalog, so a tag dragged over
                // a list row (or vice versa) shows no false affordance.
                this.drag_over::<DraggedCatalogEntry>(move |style, drag, _, cx| {
                    if drag.kind == kind {
                        style.bg(cx.theme().colors().drop_target_background)
                    } else {
                        style
                    }
                })
                .on_drop(cx.listener({
                    let target_key = key.to_string();
                    move |this, drag: &DraggedCatalogEntry, _, cx| {
                        if drag.kind != kind {
                            return;
                        }
                        this.store.update(cx, |store, cx| {
                            store.move_entry_before(kind, &drag.key, &target_key, cx)
                        });
                    }
                }))
            })
            .when(reorderable, |this| {
                this.child(
                    div()
                        .id(SharedString::from(format!("{prefix}-grip-{key}")))
                        .flex_none()
                        .cursor_pointer()
                        .on_drag(
                            DraggedCatalogEntry {
                                kind,
                                key: key.to_string(),
                                label: label.clone(),
                                color,
                            },
                            |drag, _offset, _window, cx| cx.new(|_| drag.clone()),
                        )
                        .child(
                            Icon::new(IconName::Menu)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
            })
            .child(
                div()
                    .id(SharedString::from(format!("{prefix}-swatch-{key}")))
                    .flex_none()
                    .size(px(16.))
                    .rounded_sm()
                    .bg(color)
                    .cursor_pointer()
                    .on_click(cx.listener({
                        let key = key.to_string();
                        move |this, _, _, cx| {
                            this.store
                                .update(cx, |store, cx| store.cycle_entry_color(kind, &key, cx));
                        }
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(editor),
            )
            .child(trailing)
    }

    /// One catalog section of the overlay: a header row (title + optional
    /// "Sort A–Z"), the entry rows, and a trailing "Add …" row. Shared by the
    /// lists and tags sections, which differ only in which store mutations
    /// the controls dispatch to.
    fn render_catalog_section(
        &self,
        kind: CatalogKind,
        show_sort: bool,
        rows: Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let prefix = kind.id_prefix();
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .px_1()
                    .items_center()
                    .justify_between()
                    .child(section_label(kind.section_title()))
                    .when(show_sort, |this| {
                        this.child(
                            Button::new(SharedString::from(format!("{prefix}-sort")), "Sort A–Z")
                                .style(ButtonStyle::Subtle)
                                .size(ButtonSize::Compact)
                                .label_size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.store
                                        .update(cx, |store, cx| store.sort_entries_alpha(kind, cx));
                                })),
                        )
                    }),
            )
            .children(rows)
            .child(
                h_flex()
                    .id(SharedString::from(format!("{prefix}-add")))
                    .gap_2()
                    .px_1()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.store.update(cx, |store, cx| {
                            store.add_entry(kind, cx);
                        });
                    }))
                    .child(
                        Icon::new(IconName::Plus)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(kind.add_label())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
    }

    /// One catalog's entry rows. Reordering by drag only makes sense with
    /// more than one entry.
    fn catalog_rows(
        &self,
        state: &TypeEditorState,
        kind: CatalogKind,
        entries: &[(String, gpui::Hsla)],
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let reorderable = entries.len() >= 2;
        entries
            .iter()
            .filter_map(|(key, color)| {
                let editor = state.editor(kind, key)?.clone();
                Some(
                    self.render_catalog_row(kind, key, *color, editor, reorderable, cx)
                        .into_any_element(),
                )
            })
            .collect()
    }

    /// The full-panel catalog editor overlay, or `None` while it is closed.
    pub(crate) fn render_type_editor(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = self.type_editor.as_ref()?;
        let scroll_handle = state.scroll_handle.clone();
        let entries = |entries: &[crate::inbox_model::CatalogEntry]| -> Vec<(String, gpui::Hsla)> {
            entries
                .iter()
                .map(|entry| (entry.key.clone(), catalog_color(&entry.color, cx)))
                .collect()
        };
        let (lists, tags) = {
            let store = self.store.read(cx);
            (entries(store.types()), entries(store.tags()))
        };

        let header = h_flex()
            .flex_none()
            .h(Tab::container_height(cx))
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                IconButton::new("inbox-types-back", IconName::ArrowLeft)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Back"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.type_editor = None;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .child(Label::new("Lists & Tags").weight(FontWeight::MEDIUM)),
            );

        let list_rows = self.catalog_rows(state, CatalogKind::List, &lists, cx);
        let tag_rows = self.catalog_rows(state, CatalogKind::Tag, &tags, cx);

        let border_variant = cx.theme().colors().border_variant;
        let divider = move || div().my_1().border_t_1().border_color(border_variant);
        let body = v_flex()
            .id("inbox-type-editor-body")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&scroll_handle)
            .p_2()
            .gap_1()
            .child(div().px_1().child(section_label("SYSTEM · READ-ONLY")))
            .child(self.render_system_type_row("All", Color::Accent.color(cx), cx))
            .child(self.render_system_type_row("Archive", Color::Muted.color(cx), cx))
            .child(divider())
            .child(self.render_catalog_section(CatalogKind::List, lists.len() >= 2, list_rows, cx))
            .child(divider())
            .child(self.render_catalog_section(CatalogKind::Tag, tags.len() >= 2, tag_rows, cx));

        Some(
            v_flex()
                .id("inbox-type-editor")
                .occlude()
                .absolute()
                .inset_0()
                .bg(cx.theme().colors().panel_background)
                .child(header)
                // Two layers, as in the panel's list: the outer container
                // hosts the scrollbar overlay and does not scroll itself.
                .child(
                    v_flex()
                        .id("inbox-type-editor-scroll")
                        .flex_1()
                        .min_h_0()
                        .child(body)
                        .custom_scrollbars(
                            Scrollbars::new(ScrollAxes::Vertical)
                                .tracked_scroll_handle(&scroll_handle)
                                .tracked_entity(cx.entity_id()),
                            window,
                            cx,
                        ),
                )
                .children(self.render_catalog_delete_confirmation(cx))
                .into_any_element(),
        )
    }

    /// The delete-confirmation popover for a catalog entry, anchored where
    /// the row's delete button was clicked. Mirrors the item rows'
    /// confirmation in the main list.
    fn render_catalog_delete_confirmation(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (kind, key, position) = self.type_editor.as_ref()?.confirming_delete.clone()?;
        Some(entity_confirmation_popover(
            cx.entity().downgrade(),
            "inbox-catalog-delete",
            position,
            gpui::Anchor::TopLeft,
            kind.confirm_title(),
            "Delete",
            |this, _| {
                if let Some(state) = this.type_editor.as_mut() {
                    state.confirming_delete = None;
                }
            },
            move |this, _, cx| {
                this.store
                    .update(cx, |store, cx| store.delete_entry(kind, &key, cx));
            },
            cx,
        ))
    }
}
