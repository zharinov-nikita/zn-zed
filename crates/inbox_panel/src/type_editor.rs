//! The "Списки записей" overlay: renaming, recoloring, adding and deleting
//! inbox types. Rendered on top of the whole panel while it is open.

use std::collections::HashMap;

use editor::{Editor, EditorEvent};
use gpui::{AnyElement, Context, Entity, FontWeight, Subscription, Window};
use ui::{Tab, Tooltip, prelude::*};

use crate::InboxPanel;
use crate::inbox_model::type_color;
use crate::inbox_store::InboxStore;

/// Editor state of the type editor overlay: one single-line rename editor per
/// type, keyed by the type key. Created when the overlay opens and dropped
/// when it closes.
pub(crate) struct TypeEditorState {
    rename_editors: HashMap<String, (Entity<Editor>, Subscription)>,
}

impl TypeEditorState {
    pub(crate) fn new(
        store: &Entity<InboxStore>,
        window: &mut Window,
        cx: &mut Context<InboxPanel>,
    ) -> Self {
        let mut this = Self {
            rename_editors: HashMap::default(),
        };
        this.sync(store, window, cx);
        this
    }

    /// Creates rename editors for types that don't have one yet and drops the
    /// editors of deleted types. Editors of surviving types keep their text,
    /// so in-progress edits are not clobbered.
    pub(crate) fn sync(
        &mut self,
        store: &Entity<InboxStore>,
        window: &mut Window,
        cx: &mut Context<InboxPanel>,
    ) {
        let types: Vec<(String, String)> = store
            .read(cx)
            .types()
            .iter()
            .map(|inbox_type| (inbox_type.key.clone(), inbox_type.label.clone()))
            .collect();
        self.rename_editors
            .retain(|key, _| types.iter().any(|(type_key, _)| type_key == key));
        for (key, label) in types {
            if self.rename_editors.contains_key(&key) {
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
                            .update(cx, |store, cx| store.rename_type(&key, label, cx));
                    }
                }
            });
            self.rename_editors.insert(key, (editor, subscription));
        }
    }

    /// Drops all rename editors and recreates them from the store. Used after
    /// "Сброс", when the labels change out from under the editors.
    pub(crate) fn rebuild(
        &mut self,
        store: &Entity<InboxStore>,
        window: &mut Window,
        cx: &mut Context<InboxPanel>,
    ) {
        self.rename_editors.clear();
        self.sync(store, window, cx);
    }

    fn editor(&self, key: &str) -> Option<&Entity<Editor>> {
        self.rename_editors.get(key).map(|(editor, _)| editor)
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

    fn render_type_row(
        &self,
        key: &str,
        color: gpui::Hsla,
        editor: Entity<Editor>,
        can_delete: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .gap_2()
            .px_1()
            .py_0p5()
            .child(
                div()
                    .id(SharedString::from(format!("inbox-type-swatch-{key}")))
                    .flex_none()
                    .size(px(16.))
                    .rounded_sm()
                    .bg(color)
                    .cursor_pointer()
                    .on_click(cx.listener({
                        let key = key.to_string();
                        move |this, _, _, cx| {
                            this.store
                                .update(cx, |store, cx| store.cycle_type_color(&key, cx));
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
            .child(
                IconButton::new(
                    SharedString::from(format!("inbox-type-delete-{key}")),
                    IconName::Close,
                )
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .disabled(!can_delete)
                .tooltip(Tooltip::text("Удалить список"))
                .on_click(cx.listener({
                    let key = key.to_string();
                    move |this, _, window, cx| {
                        let store = this.store.clone();
                        store.update(cx, |store, cx| store.delete_type(&key, cx));
                        if let Some(state) = this.type_editor.as_mut() {
                            state.sync(&store, window, cx);
                        }
                        cx.notify();
                    }
                })),
            )
    }

    /// The full-panel type editor overlay, or `None` while it is closed.
    pub(crate) fn render_type_editor(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.type_editor.as_ref()?;
        let types: Vec<(String, gpui::Hsla)> = {
            let store = self.store.read(cx);
            store
                .types()
                .iter()
                .map(|inbox_type| (inbox_type.key.clone(), type_color(&inbox_type.color, cx)))
                .collect()
        };
        let can_delete = types.len() > 1;

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
                    .tooltip(Tooltip::text("Назад"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.type_editor = None;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .child(Label::new("Списки записей").weight(FontWeight::MEDIUM)),
            )
            .child(
                Button::new("inbox-types-reset", "Сброс")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::XSmall)
                    .on_click(cx.listener(|this, _, window, cx| {
                        let store = this.store.clone();
                        store.update(cx, |store, cx| store.reset_types(cx));
                        if let Some(state) = this.type_editor.as_mut() {
                            state.rebuild(&store, window, cx);
                        }
                        cx.notify();
                    })),
            );

        let type_rows: Vec<AnyElement> = types
            .iter()
            .filter_map(|(key, color)| {
                let editor = state.editor(key)?.clone();
                Some(
                    self.render_type_row(key, *color, editor, can_delete, cx)
                        .into_any_element(),
                )
            })
            .collect();

        let body = v_flex()
            .id("inbox-type-editor-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_2()
            .gap_1()
            .child(
                div()
                    .px_1()
                    .child(section_label("СИСТЕМНЫЕ · НЕЛЬЗЯ ИЗМЕНИТЬ")),
            )
            .child(self.render_system_type_row("Все", Color::Accent.color(cx), cx))
            .child(self.render_system_type_row("Архив", Color::Muted.color(cx), cx))
            .child(
                div()
                    .my_1()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant),
            )
            .child(div().px_1().child(section_label("СВОИ СПИСКИ")))
            .children(type_rows)
            .child(
                h_flex()
                    .id("inbox-add-type")
                    .gap_2()
                    .px_1()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let store = this.store.clone();
                        store.update(cx, |store, cx| {
                            store.add_type(cx);
                        });
                        if let Some(state) = this.type_editor.as_mut() {
                            state.sync(&store, window, cx);
                        }
                        cx.notify();
                    }))
                    .child(
                        Icon::new(IconName::Plus)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new("Добавить тип")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            );

        let footer = div()
            .flex_none()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .text_xs()
            .font_buffer(cx)
            .text_color(cx.theme().colors().text_placeholder)
            .child("хранится в .zed/inbox.json → \"types\"");

        Some(
            v_flex()
                .id("inbox-type-editor")
                .occlude()
                .absolute()
                .inset_0()
                .bg(cx.theme().colors().panel_background)
                .child(header)
                .child(body)
                .child(footer)
                .into_any_element(),
        )
    }
}
