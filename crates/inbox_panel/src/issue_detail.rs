//! Read-only detail overlay for one GitHub issue.
//!
//! Deliberately not [`InboxDetailView`](crate::detail_view::InboxDetailView):
//! that is a block *editor* bound to an [`ItemId`](crate::inbox_model::ItemId)
//! and store mutations, while this view renders a snapshot of remote content
//! nothing can edit. Comments are fetched lazily when the view opens.

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla, ScrollHandle, SharedString,
    TextStyleRefinement, Window,
};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use theme_settings::ThemeSettings;
use ui::{ScrollAxes, Scrollbars, Tab, Tooltip, WithScrollbar, prelude::*};

use crate::github_issues::{GithubComment, GithubIssue, fetch_issue_comments};
use crate::inbox_model::{format_age, now_unix};
use crate::inbox_panel_settings::Settings as _;
use crate::inbox_store::InboxStore;

pub enum GithubIssueDetailEvent {
    Closed,
}

/// The lazily fetched comments of the open issue.
enum CommentsState {
    Loading,
    Loaded(Vec<CommentEntry>),
    Failed(String),
}

struct CommentEntry {
    comment: GithubComment,
    markdown: Entity<Markdown>,
}

pub struct GithubIssueDetailView {
    /// A snapshot taken at open time; a background refresh replacing the
    /// list does not touch an open detail view.
    issue: GithubIssue,
    body_markdown: Entity<Markdown>,
    comments: CommentsState,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
}

impl EventEmitter<GithubIssueDetailEvent> for GithubIssueDetailView {}

impl Focusable for GithubIssueDetailView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl GithubIssueDetailView {
    pub fn new(store: &Entity<InboxStore>, issue: GithubIssue, cx: &mut Context<Self>) -> Self {
        let body = SharedString::from(issue.body.clone().unwrap_or_default());
        let body_markdown = cx.new(|cx| Markdown::new(body, None, None, cx));
        let comments = if issue.comments == 0 {
            CommentsState::Loaded(Vec::new())
        } else {
            Self::spawn_comments_fetch(store, issue.number, cx)
        };
        Self {
            issue,
            body_markdown,
            comments,
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
        }
    }

    /// Kicks off the lazy comments fetch and returns the state to start in.
    fn spawn_comments_fetch(
        store: &Entity<InboxStore>,
        number: u64,
        cx: &mut Context<Self>,
    ) -> CommentsState {
        let state = store.read(cx).github_issues();
        let Some((owner, repo)) = state
            .owner_repo()
            .map(|(owner, repo)| (owner.to_string(), repo.to_string()))
        else {
            // Opened from a cached list whose binding is gone; the count is
            // still shown, just without the bodies.
            return CommentsState::Failed("no GitHub binding".to_string());
        };
        let cached_token = state.cached_token();
        let http = cx.http_client();
        let store = store.downgrade();
        cx.spawn(async move |this, cx| {
            let token = match cached_token {
                Some(token) => token,
                None => {
                    let token = crate::github_issues::resolve_github_token().await;
                    // Write the resolution back so the store's own next list
                    // fetch doesn't spawn `gh` again.
                    store
                        .update(cx, |store, _| store.memoize_github_token(token.clone()))
                        .ok();
                    token
                }
            };
            let result = fetch_issue_comments(http, token.as_deref(), &owner, &repo, number).await;
            this.update(cx, |this, cx| {
                this.comments = match result {
                    Ok(comments) => CommentsState::Loaded(
                        comments
                            .into_iter()
                            .map(|comment| {
                                let body =
                                    SharedString::from(comment.body.clone().unwrap_or_default());
                                CommentEntry {
                                    comment,
                                    markdown: cx.new(|cx| Markdown::new(body, None, None, cx)),
                                }
                            })
                            .collect(),
                    ),
                    Err(error) => CommentsState::Failed(error.to_string()),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
        CommentsState::Loading
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let url = self.issue.html_url.clone();
        h_flex()
            .flex_none()
            .h(Tab::container_height(cx))
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Button::new("inbox-issue-back", "Inbox")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .start_icon(
                        Icon::new(IconName::ChevronLeft)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text("Back to inbox"))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(GithubIssueDetailEvent::Closed))),
            )
            .child(
                Label::new(format!("#{}", self.issue.number))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .px_1()
                    .rounded_sm()
                    .bg(cx.theme().colors().element_background)
                    .child(
                        // The fetch filters `state=open`, so the chip is
                        // effectively a constant "open" badge; `state` is
                        // kept as data for the MCP output.
                        Label::new(self.issue.state.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Created),
                    ),
            )
            .child(div().flex_1())
            .child(
                IconButton::new("inbox-issue-open-browser", IconName::ArrowUpRight)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Open on GitHub"))
                    .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url))),
            )
    }

    fn render_meta(&self, now: i64, cx: &mut Context<Self>) -> impl IntoElement {
        let mut meta = h_flex().flex_wrap().items_center().gap_2().px_3();
        if let Some(user) = &self.issue.user {
            meta = meta.child(
                Label::new(format!("@{}", user.login))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        }
        if let Some(created) = self.issue.created_unix() {
            meta = meta.child(
                Label::new(format!("opened {} ago", format_age(created, now)))
                    .size(LabelSize::XSmall)
                    .color(Color::Placeholder),
            );
        }
        if let Some(updated) = self.issue.updated_unix() {
            meta = meta.child(
                Label::new(format!("updated {} ago", format_age(updated, now)))
                    .size(LabelSize::XSmall)
                    .color(Color::Placeholder),
            );
        }
        for label in &self.issue.labels {
            meta = meta.child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(github_label_dot(github_label_color(&label.color, cx)))
                    .child(
                        Label::new(label.name.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            );
        }
        meta
    }

    fn render_comments(&self, now: i64, window: &Window, cx: &mut Context<Self>) -> Div {
        let mut section = v_flex().gap_2().px_3().pb_3().child(
            Label::new(format!("COMMENTS ({})", self.issue.comments))
                .size(LabelSize::XSmall)
                .weight(gpui::FontWeight::BOLD)
                .color(Color::Muted),
        );
        match &self.comments {
            CommentsState::Loading => {
                section = section.child(
                    Label::new("Loading comments…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                );
            }
            CommentsState::Failed(error) => {
                section = section.child(
                    Label::new(format!("Failed to load comments: {error}"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                );
            }
            CommentsState::Loaded(comments) => {
                for (index, entry) in comments.iter().enumerate() {
                    let mut header = h_flex().gap_2().items_center();
                    if let Some(user) = &entry.comment.user {
                        header = header.child(
                            Label::new(format!("@{}", user.login))
                                .size(LabelSize::XSmall)
                                .weight(gpui::FontWeight::BOLD)
                                .color(Color::Muted),
                        );
                    }
                    if let Some(created) = entry.comment.created_unix() {
                        header = header.child(
                            Label::new(format!("{} ago", format_age(created, now)))
                                .size(LabelSize::XSmall)
                                .color(Color::Placeholder),
                        );
                    }
                    section = section.child(
                        v_flex()
                            .id(SharedString::from(format!("inbox-issue-comment-{index}")))
                            .gap_1()
                            .p_2()
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(header)
                            .child(
                                MarkdownElement::new(
                                    entry.markdown.clone(),
                                    issue_markdown_style(window, cx),
                                )
                                .into_any_element(),
                            ),
                    );
                }
            }
        }
        section
    }
}

impl Render for GithubIssueDetailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = now_unix();
        v_flex()
            .key_context("GithubIssueDetail")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| {
                cx.emit(GithubIssueDetailEvent::Closed);
            }))
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .id("inbox-issue-detail-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll_handle)
                            .gap_2()
                            .pt_2()
                            .child(
                                div().px_3().child(
                                    Label::new(self.issue.title.clone())
                                        .size(LabelSize::Large)
                                        .weight(gpui::FontWeight::BOLD),
                                ),
                            )
                            .child(self.render_meta(now, cx))
                            .child(div().px_3().map(|this| {
                                if self.issue.body.as_deref().is_none_or(str::is_empty) {
                                    this.child(
                                        Label::new("No description.")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                } else {
                                    this.child(
                                        MarkdownElement::new(
                                            self.body_markdown.clone(),
                                            issue_markdown_style(window, cx),
                                        )
                                        .into_any_element(),
                                    )
                                }
                            }))
                            .child(
                                div()
                                    .mx_3()
                                    .border_t_1()
                                    .border_color(cx.theme().colors().border_variant),
                            )
                            .child(self.render_comments(now, window, cx)),
                    )
                    .custom_scrollbars(
                        Scrollbars::new(ScrollAxes::Vertical)
                            .tracked_scroll_handle(&self.scroll_handle)
                            .tracked_entity(cx.entity_id()),
                        window,
                        cx,
                    ),
            )
    }
}

/// Resolves a GitHub label color (hex RGB without `#`) to a concrete color,
/// falling back to the muted color for malformed values.
pub(crate) fn github_label_color(hex: &str, cx: &App) -> Hsla {
    gpui::Rgba::try_from(format!("#{hex}").as_str())
        .map(Hsla::from)
        .unwrap_or_else(|_| Color::Muted.color(cx))
}

/// The small colored dot marking a GitHub label. Dots, not filled chips, so
/// GitHub's arbitrary label colors can't produce unreadable text on either
/// theme.
pub(crate) fn github_label_dot(color: Hsla) -> impl IntoElement {
    crate::color_swatch(color, true)
}

/// The read-only markdown style of the issue body and its comments: the
/// crate's shared base recipe plus the document-level extras (code blocks,
/// rules, quotes) that the block editor's single-line blocks never need.
fn issue_markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    let theme_settings = ThemeSettings::get_global(cx);
    let colors = cx.theme().colors();

    let mut base_text_style = window.text_style();
    base_text_style.refine(&TextStyleRefinement {
        font_family: Some(theme_settings.ui_font.family.clone()),
        font_fallbacks: theme_settings.ui_font.fallbacks.clone(),
        font_features: Some(theme_settings.ui_font.features.clone()),
        font_size: Some(rems(0.875).into()),
        color: Some(colors.text),
        ..Default::default()
    });

    MarkdownStyle {
        code_block_overflow_x_scroll: true,
        code_block: gpui::StyleRefinement::default()
            .my_1()
            .px_2()
            .py_1()
            .bg(colors.editor_background)
            .rounded_sm(),
        rule_color: colors.border_variant,
        block_quote_border_color: colors.border,
        block_quote: TextStyleRefinement {
            color: Some(Color::Muted.color(cx)),
            ..Default::default()
        },
        ..crate::detail_view::base_markdown_style(base_text_style, cx)
    }
}
