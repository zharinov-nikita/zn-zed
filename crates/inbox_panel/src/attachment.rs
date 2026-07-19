//! File attachments for inbox items.
//!
//! Attachments are reference-only: we store a path, never file content, so the
//! git-committed `.zed/inbox.json` stays small. Files are picked either by
//! typing `@` in the capture box or an item's title (a fuzzy project-file
//! completion) or through an OS file dialog / drag & drop for arbitrary files.
//! A pick never inserts inline text — it hands an [`AttachmentRef`] to the
//! owner, which stages it (capture) or writes it to the store (detail); the
//! attachment then renders as a removable chip.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use editor::{CompletionContext, CompletionProvider, Editor};
use gpui::{App, Context, Entity, Task, WeakEntity, Window};
use language::{Buffer, CodeLabel, ToOffset};
use project::{
    Candidates, Completion, CompletionDisplayOptions, CompletionIntent, CompletionResponse,
    CompletionSource, PathMatchCandidateSet, Project,
};
use ui::IconName;
use workspace::Workspace;

use crate::inbox_model::AttachmentRef;

/// Callback the owner supplies to receive a picked attachment. The capture box
/// stages it locally; the detail view writes it straight to the store.
pub(crate) type OnPick = Arc<dyn Fn(AttachmentRef, &mut App) + Send + Sync>;

/// Staging list for attachments added in the capture box, before the item
/// exists. The panel observes it to re-render chips and drains it on capture.
#[derive(Default)]
pub(crate) struct AttachmentSet {
    list: Vec<AttachmentRef>,
}

impl AttachmentSet {
    pub fn list(&self) -> &[AttachmentRef] {
        &self.list
    }

    /// Appends `attachment` unless already present. Returns whether it changed.
    pub fn add(&mut self, attachment: AttachmentRef) -> bool {
        if self.list.contains(&attachment) {
            false
        } else {
            self.list.push(attachment);
            true
        }
    }

    pub fn remove(&mut self, attachment: &AttachmentRef) {
        self.list.retain(|existing| existing != attachment);
    }

    /// Drains the staged attachments, leaving the set empty.
    pub fn take(&mut self) -> Vec<AttachmentRef> {
        std::mem::take(&mut self.list)
    }
}

/// Classifies an absolute path as a project-relative or an external attachment.
/// A file that resolves inside an open worktree becomes [`AttachmentRef::Project`]
/// (worktree-relative, survives repo moves); anything else is external.
pub(crate) fn classify_attachment(
    project: &Entity<Project>,
    abs_path: &Path,
    cx: &App,
) -> AttachmentRef {
    match project.read(cx).project_path_for_absolute_path(abs_path, cx) {
        Some(project_path) => AttachmentRef::Project {
            path: project_path.path.as_unix_str().to_string(),
        },
        None => AttachmentRef::External {
            path: abs_path.to_string_lossy().into_owned(),
        },
    }
}

/// Classifies picked or dropped absolute paths into attachments and hands each
/// to `sink`. Skips directories and anything that resolves to an empty path, so
/// dropping a folder (or a worktree root) never produces a nameless chip.
/// Shared by the capture box and the detail view, for both the OS file dialog
/// and drag & drop.
pub(crate) fn attach_external_paths(
    paths: &[PathBuf],
    project: &Entity<Project>,
    cx: &mut App,
    mut sink: impl FnMut(AttachmentRef, &mut App),
) {
    for abs_path in paths {
        if abs_path.is_dir() {
            continue;
        }
        let attachment = classify_attachment(project, abs_path, cx);
        if attachment.path().is_empty() {
            continue;
        }
        sink(attachment, cx);
    }
}

/// `@`-mention file completion. On accept it removes the typed `@query` (the
/// completion's `new_text` is empty) and hands the picked file to `on_pick`; it
/// never inserts inline text.
pub(crate) struct AttachmentCompletionProvider {
    workspace: WeakEntity<Workspace>,
    on_pick: OnPick,
}

impl AttachmentCompletionProvider {
    pub fn new(workspace: WeakEntity<Workspace>, on_pick: OnPick) -> Self {
        Self { workspace, on_pick }
    }
}

/// Finds a trailing `@query` ending at `position`. Returns the byte offset of
/// the `@` and the query text after it. A mention starts at line start, after
/// whitespace, or after an opening bracket, with no whitespace right after `@`.
fn mention_at(buffer: &Buffer, position: language::Anchor) -> Option<(usize, String)> {
    let offset = position.to_offset(buffer);
    let mut query_chars = Vec::new();
    let mut query_len = 0usize;
    let mut chars = buffer.reversed_chars_at(position);
    let mut char_before_at = None;
    let mut found = false;
    for ch in chars.by_ref() {
        if ch == '@' {
            found = true;
            char_before_at = chars.next();
            break;
        }
        if ch.is_whitespace() {
            break;
        }
        query_chars.push(ch);
        query_len += ch.len_utf8();
    }
    if !found {
        return None;
    }
    let boundary_ok = match char_before_at {
        None => true,
        Some(c) => c.is_whitespace() || matches!(c, '(' | '[' | '{'),
    };
    if !boundary_ok {
        return None;
    }
    let at_offset = offset.checked_sub(query_len + 1)?;
    let query: String = query_chars.iter().rev().collect();
    Some((at_offset, query))
}

impl CompletionProvider for AttachmentCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: language::Anchor,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<Vec<CompletionResponse>>> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(Ok(Vec::new()));
        };
        let buffer_ref = buffer.read(cx);
        let Some((at_offset, query)) = mention_at(buffer_ref, buffer_position) else {
            return Task::ready(Ok(Vec::new()));
        };
        let end_offset = buffer_position.to_offset(buffer_ref);
        let source_range =
            buffer_ref.anchor_before(at_offset)..buffer_ref.anchor_before(end_offset);

        let candidate_sets = workspace
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| {
                let worktree = worktree.read(cx);
                PathMatchCandidateSet {
                    snapshot: worktree.snapshot(),
                    include_ignored: worktree
                        .root_entry()
                        .is_some_and(|entry| entry.is_ignored),
                    include_root_name: false,
                    candidates: Candidates::Entries,
                }
            })
            .collect::<Vec<_>>();

        let on_pick = self.on_pick.clone();
        let executor = cx.background_executor().clone();
        let cancel = Arc::new(AtomicBool::new(false));
        cx.foreground_executor().spawn(async move {
            let matches = fuzzy::match_path_sets(
                candidate_sets.as_slice(),
                query.as_str(),
                &None,
                false,
                100,
                &cancel,
                executor,
            )
            .await;

            let completions = matches
                .into_iter()
                .filter(|mat| !mat.is_dir && !mat.path.as_unix_str().is_empty())
                .map(|mat| {
                    let path = mat.path.as_unix_str().to_string();
                    let name = path.rsplit('/').next().unwrap_or(path.as_str()).to_string();
                    let attachment = AttachmentRef::Project { path };
                    let on_pick = on_pick.clone();
                    Completion {
                        replace_range: source_range.clone(),
                        new_text: String::new(),
                        label: CodeLabel::plain(name, None),
                        documentation: None,
                        source: CompletionSource::Custom,
                        icon_path: Some(IconName::File.path().into()),
                        icon_color: None,
                        match_start: None,
                        snippet_deduplication_key: None,
                        insert_text_mode: None,
                        confirm: Some(Arc::new(
                            move |_intent: CompletionIntent, _window: &mut Window, cx: &mut App| {
                                on_pick(attachment.clone(), cx);
                                false
                            },
                        )),
                        group: None,
                    }
                })
                .collect::<Vec<_>>();

            Ok(vec![CompletionResponse {
                completions,
                display_options: CompletionDisplayOptions {
                    dynamic_width: true,
                },
                is_incomplete: true,
            }])
        })
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        _text: &str,
        _trigger_in_words: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        mention_at(buffer.read(cx), position).is_some()
    }

    fn sort_completions(&self) -> bool {
        false
    }

    fn filter_completions(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(path: &str) -> AttachmentRef {
        AttachmentRef::Project { path: path.into() }
    }

    #[test]
    fn test_attachment_set_add_dedups_remove_and_take() {
        let mut set = AttachmentSet::default();
        assert!(set.add(project("a.rs")));
        // Adding the same reference again is a no-op.
        assert!(!set.add(project("a.rs")));
        assert!(set.add(project("b.rs")));
        assert_eq!(set.list(), &[project("a.rs"), project("b.rs")]);

        set.remove(&project("a.rs"));
        assert_eq!(set.list(), &[project("b.rs")]);

        let taken = set.take();
        assert_eq!(taken, vec![project("b.rs")]);
        assert!(set.list().is_empty());
    }
}
