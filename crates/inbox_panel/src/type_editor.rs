//! The "Lists" overlay: renaming, recoloring, adding and deleting inbox
//! types. Rendered on top of the whole panel while it is open.

use std::collections::HashMap;

use editor::{Editor, EditorEvent};
use gpui::{AnyElement, Context, Entity, FontWeight, Subscription, Window};
use ui::{Tab, TintColor, Tooltip, prelude::*};

use crate::InboxPanel;
use crate::inbox_model::type_color;
use crate::inbox_store::InboxStore;

/// Editor state of the type editor overlay: one single-line rename editor per
/// type, keyed by the type key. Created when the overlay opens and dropped
/// when it closes.
pub(crate) struct TypeEditorState {
    rename_editors: HashMap<String, (Entity<Editor>, Subscription)>,
    /// The key of the custom list whose delete action is pending
    /// confirmation, if any. Only one row can be confirming at a time.
    confirming_delete: Option<String>,
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
        if let Some(pending) = &self.confirming_delete {
            if !types.iter().any(|(type_key, _)| type_key == pending) {
                self.confirming_delete = None;
            }
        }
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
        // The type editor and the detail view overlays are mutually
        // exclusive: opening one closes the other.
        self.detail = None;
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
        confirming_delete: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let trailing: AnyElement = if confirming_delete {
            h_flex()
                .flex_none()
                .gap_1()
                .child(
                    Label::new("Delete list?")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    Button::new(
                        SharedString::from(format!("inbox-type-delete-cancel-{key}")),
                        "Cancel",
                    )
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .on_click(cx.listener({
                        let key = key.to_string();
                        move |this, _, _, cx| {
                            if let Some(state) = this.type_editor.as_mut() {
                                if state.confirming_delete.as_deref() == Some(key.as_str()) {
                                    state.confirming_delete = None;
                                }
                            }
                            cx.notify();
                        }
                    })),
                )
                .child(
                    Button::new(
                        SharedString::from(format!("inbox-type-delete-confirm-{key}")),
                        "Delete",
                    )
                    .style(ButtonStyle::Tinted(TintColor::Error))
                    .size(ButtonSize::Compact)
                    .on_click(cx.listener({
                        let key = key.to_string();
                        move |this, _, window, cx| {
                            let store = this.store.clone();
                            store.update(cx, |store, cx| store.delete_type(&key, cx));
                            if let Some(state) = this.type_editor.as_mut() {
                                state.confirming_delete = None;
                                state.sync(&store, window, cx);
                            }
                            cx.notify();
                        }
                    })),
                )
                .into_any_element()
        } else {
            IconButton::new(
                SharedString::from(format!("inbox-type-delete-{key}")),
                IconName::Close,
            )
            .icon_size(IconSize::XSmall)
            .icon_color(Color::Muted)
            .tooltip(Tooltip::text("Delete list"))
            .on_click(cx.listener({
                let key = key.to_string();
                move |this, _, _, cx| {
                    if let Some(state) = this.type_editor.as_mut() {
                        state.confirming_delete = Some(key.clone());
                    }
                    cx.notify();
                }
            }))
            .into_any_element()
        };

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
            .child(trailing)
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
                    .child(Label::new("Lists").weight(FontWeight::MEDIUM)),
            );

        let type_rows: Vec<AnyElement> = types
            .iter()
            .filter_map(|(key, color)| {
                let editor = state.editor(key)?.clone();
                let confirming_delete = state.confirming_delete.as_deref() == Some(key.as_str());
                Some(
                    self.render_type_row(key, *color, editor, confirming_delete, cx)
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
            .child(div().px_1().child(section_label("SYSTEM · READ-ONLY")))
            .child(self.render_system_type_row("All", Color::Accent.color(cx), cx))
            .child(self.render_system_type_row("Archive", Color::Muted.color(cx), cx))
            .child(
                div()
                    .my_1()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant),
            )
            .child(div().px_1().child(section_label("YOUR LISTS")))
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
                        Label::new("Add list")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            );

        Some(
            v_flex()
                .id("inbox-type-editor")
                .occlude()
                .absolute()
                .inset_0()
                .bg(cx.theme().colors().panel_background)
                .child(header)
                .child(body)
                .into_any_element(),
        )
    }
}
