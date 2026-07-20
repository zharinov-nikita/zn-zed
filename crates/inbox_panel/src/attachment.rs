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
use fs::Fs;
use gpui::{
    App, BackgroundExecutor, ClipboardEntry, ClipboardItem, Context, Entity, Image, ImageFormat,
    Task, WeakEntity, Window,
};
use language::{Buffer, CodeLabel, ToOffset};
use project::{
    Candidates, Completion, CompletionDisplayOptions, CompletionIntent, CompletionResponse,
    CompletionSource, PathMatchCandidateSet, Project,
};
use ui::IconName;
use workspace::Workspace;

use crate::inbox_model::AttachmentRef;
use crate::inbox_store::InboxStore;

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
    match project
        .read(cx)
        .project_path_for_absolute_path(abs_path, cx)
    {
        Some(project_path) => AttachmentRef::Project {
            path: project_path.path.as_unix_str().to_string(),
        },
        None => AttachmentRef::External {
            path: abs_path.to_string_lossy().into_owned(),
        },
    }
}

/// Opens the OS file dialog and hands each picked file to `sink` as an
/// attachment. Shared by the capture box and the detail view, so the prompt
/// options and the unwrap dance live in one place.
pub(crate) fn pick_and_attach(
    project: Entity<Project>,
    cx: &mut App,
    sink: impl FnMut(AttachmentRef, &mut App) + 'static,
) {
    let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
        files: true,
        directories: false,
        multiple: true,
        prompt: Some("Attach files".into()),
    });
    let mut sink = sink;
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(paths))) = receiver.await else {
            return;
        };
        cx.update(|cx| attach_external_paths(&paths, &project, cx, &mut sink));
    })
    .detach();
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

/// Whether a clipboard paste should be intercepted as an attachment. A leading
/// String entry (or an empty clipboard) means "this is a text paste" and falls
/// through to the editor, respecting the priority order set by the source
/// application (a browser copy carries text first, an image second).
pub(crate) fn should_intercept_paste(item: &ClipboardItem) -> bool {
    !matches!(item.entries().first(), Some(ClipboardEntry::String(_)) | None)
}

/// Directory holding a project's pasted-image attachments; a sibling of the
/// backup ring under Zed's global data dir (see `backup_dir_for_key` in
/// `inbox_store.rs`). `None` (no bound project) files under a shared
/// "unkeyed" bucket so paste still works in a worktree-less window.
pub(crate) fn attachment_dir_for_key(key: Option<&str>) -> PathBuf {
    paths::data_dir()
        .join("inbox_attachments")
        .join(key.unwrap_or("unkeyed"))
}

/// Saves one pasted image under `dir` as `<stem>.<ext>`, uniquified with
/// `-1`, `-2`, … suffixes. Bmp (how Windows delivers CF_DIB screenshots) is
/// transcoded to PNG on the background executor; every other format is
/// written byte-for-byte. Like every other attachment, the saved file is
/// reference-only: removing its chip later does not delete it.
pub(crate) async fn save_pasted_image(
    fs: &Arc<dyn Fs>,
    dir: &Path,
    stem: &str,
    image: Image,
    executor: BackgroundExecutor,
) -> anyhow::Result<PathBuf> {
    let (bytes, extension) = if image.format() == ImageFormat::Bmp {
        let png = executor
            .spawn(async move {
                let decoded = image::load_from_memory_with_format(
                    image.bytes(),
                    image::ImageFormat::Bmp,
                )?;
                let mut png = Vec::new();
                decoded.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
                anyhow::Ok(png)
            })
            .await?;
        (png, "png")
    } else {
        let extension = image.format().extension();
        (image.bytes, extension)
    };
    fs.create_dir(dir).await?;
    let mut path = dir.join(format!("{stem}.{extension}"));
    let mut suffix = 0usize;
    while fs.is_file(&path).await {
        suffix += 1;
        path = dir.join(format!("{stem}-{suffix}.{extension}"));
    }
    fs.write(&path, &bytes).await?;
    Ok(path)
}

/// Timestamped filename stem for a pasted image, `pasted-YYYY-MM-DD-HHMMSS`
/// in local time. Same-second collisions are resolved by
/// [`save_pasted_image`]'s uniquify loop, not here.
fn pasted_image_stem() -> String {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    format!(
        "pasted-{:04}-{:02}-{:02}-{:02}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

/// Intercepts a paste whose clipboard holds images or copied files, turning
/// them into attachments delivered through `sink`: images are saved under
/// [`attachment_dir_for_key`] and attached as external files, copied files
/// are attached by reference exactly like drag & drop. Returns whether the
/// paste was handled; `false` means "let the editor paste text".
pub(crate) fn handle_attachment_paste(
    workspace: WeakEntity<Workspace>,
    store: Entity<InboxStore>,
    sink: OnPick,
    cx: &mut App,
) -> bool {
    let Some(item) = cx.read_from_clipboard() else {
        return false;
    };
    if !should_intercept_paste(&item) {
        return false;
    }

    let mut images = Vec::new();
    let mut paths = Vec::new();
    for entry in item.into_entries() {
        match entry {
            ClipboardEntry::Image(image) if !image.bytes.is_empty() => images.push(image),
            ClipboardEntry::ExternalPaths(external) => {
                paths.extend(external.paths().iter().cloned())
            }
            _ => {}
        }
    }

    if !paths.is_empty()
        && let Some(workspace) = workspace.upgrade()
    {
        let project = workspace.read(cx).project().clone();
        let sink = sink.clone();
        attach_external_paths(&paths, &project, cx, move |attachment, cx| {
            sink(attachment, cx)
        });
    }

    if !images.is_empty() {
        let fs = store.read(cx).fs();
        let dir = attachment_dir_for_key(store.read(cx).bound_project_key());
        let executor = cx.background_executor().clone();
        let stem = pasted_image_stem();
        cx.spawn(async move |cx| {
            for image in images {
                match save_pasted_image(&fs, &dir, &stem, image, executor.clone()).await {
                    Ok(path) => {
                        let attachment = AttachmentRef::External {
                            path: path.to_string_lossy().into_owned(),
                        };
                        cx.update(|cx| sink(attachment, cx));
                    }
                    Err(error) => {
                        log::warn!("inbox: failed to save pasted image: {error:#}");
                        cx.update(|cx| {
                            crate::show_inbox_toast(
                                &workspace,
                                "inbox-paste-attachment-failed",
                                format!("Failed to save pasted image: {error:#}"),
                                cx,
                            )
                        });
                    }
                }
            }
        })
        .detach();
    }

    true
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
                    include_ignored: worktree.root_entry().is_some_and(|entry| entry.is_ignored),
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
                    let name = mat
                        .path
                        .file_name()
                        .unwrap_or_else(|| mat.path.as_unix_str())
                        .to_string();
                    let path = mat.path.as_unix_str().to_string();
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
    use fs::{FakeFs, Fs};
    use gpui::{ClipboardEntry, ClipboardItem, ExternalPaths, Image, ImageFormat, TestAppContext};
    use serde_json::json;
    use gpui::AppContext as _;
    use settings::SettingsStore;
    use util::path;

    use super::*;

    fn project(path: &str) -> AttachmentRef {
        AttachmentRef::Project { path: path.into() }
    }

    fn png_image(bytes: Vec<u8>) -> Image {
        Image::from_bytes(ImageFormat::Png, bytes)
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            // A fresh in-memory database per test (see CLAUDE.md): without it
            // the store falls back to the process-wide shared test DB.
            cx.set_global(db::AppDatabase::test_new());
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    /// A store on a FakeFs project plus a sink collecting into an
    /// [`AttachmentSet`], mirroring the capture box's staging path.
    async fn build_paste_fixture(
        cx: &mut TestAppContext,
    ) -> (
        Arc<FakeFs>,
        Entity<InboxStore>,
        Entity<AttachmentSet>,
        OnPick,
    ) {
        init_test(cx);
        let fake_fs = FakeFs::new(cx.executor());
        fake_fs.insert_tree(path!("/root"), json!({})).await;
        let project = Project::test(fake_fs.clone(), [path!("/root").as_ref() as &Path], cx).await;
        let store = cx.new(|cx| InboxStore::new(project, fake_fs.clone(), cx));
        cx.run_until_parked();

        let set = cx.new(|_| AttachmentSet::default());
        let sink: OnPick = {
            let set = set.downgrade();
            Arc::new(move |attachment, cx: &mut App| {
                set.update(cx, |set, _| {
                    set.add(attachment);
                })
                .ok();
            })
        };
        (fake_fs, store, set, sink)
    }

    #[gpui::test]
    async fn test_handle_attachment_paste_saves_image_and_feeds_sink(cx: &mut TestAppContext) {
        let (fake_fs, store, set, sink) = build_paste_fixture(cx).await;

        cx.update(|cx| {
            cx.write_to_clipboard(ClipboardItem::new_image(&png_image(vec![9, 9, 9])));
            assert!(handle_attachment_paste(
                WeakEntity::new_invalid(),
                store,
                sink,
                cx,
            ));
        });
        cx.run_until_parked();

        let list = set.read_with(cx, |set, _| set.list().to_vec());
        assert_eq!(list.len(), 1);
        let AttachmentRef::External { path } = &list[0] else {
            panic!("expected an external attachment, got {:?}", list[0]);
        };
        let path = Path::new(path);
        assert!(path.starts_with(paths::data_dir().join("inbox_attachments")));
        assert!(fake_fs.is_file(path).await);
        assert_eq!(fake_fs.load_bytes(path).await.unwrap(), vec![9, 9, 9]);
    }

    /// Regression coverage for the (previously untested) drag & drop /
    /// dialog classification the paste path reuses.
    #[gpui::test]
    async fn test_attach_external_paths_classifies_project_vs_external(cx: &mut TestAppContext) {
        init_test(cx);
        let fake_fs = FakeFs::new(cx.executor());
        fake_fs
            .insert_tree(path!("/root"), json!({ "inside.txt": "a" }))
            .await;
        fake_fs
            .insert_tree(path!("/elsewhere"), json!({ "outside.txt": "b" }))
            .await;
        let project = Project::test(fake_fs, [path!("/root").as_ref() as &Path], cx).await;

        let mut picked = Vec::new();
        cx.update(|cx| {
            attach_external_paths(
                &[
                    PathBuf::from(path!("/root/inside.txt")),
                    PathBuf::from(path!("/elsewhere/outside.txt")),
                ],
                &project,
                cx,
                |attachment, _| picked.push(attachment),
            );
        });

        assert_eq!(
            picked,
            vec![
                AttachmentRef::Project {
                    path: "inside.txt".into()
                },
                AttachmentRef::External {
                    path: path!("/elsewhere/outside.txt").into()
                },
            ]
        );
    }

    #[gpui::test]
    async fn test_handle_attachment_paste_ignores_text_clipboard(cx: &mut TestAppContext) {
        let (_fake_fs, store, set, sink) = build_paste_fixture(cx).await;

        cx.update(|cx| {
            cx.write_to_clipboard(ClipboardItem::new_string("hello".into()));
            assert!(!handle_attachment_paste(
                WeakEntity::new_invalid(),
                store,
                sink,
                cx,
            ));
        });
        cx.run_until_parked();

        assert!(set.read_with(cx, |set, _| set.list().is_empty()));
    }

    #[test]
    fn test_should_intercept_paste() {
        // Plain text (and an empty clipboard) belongs to the editor.
        assert!(!should_intercept_paste(&ClipboardItem::new_string(
            "hello".into()
        )),);
        assert!(!should_intercept_paste(&ClipboardItem {
            entries: Vec::new()
        }));

        let image = png_image(vec![1, 2, 3]);
        assert!(should_intercept_paste(&ClipboardItem::new_image(&image)));

        let paths_first = ClipboardItem {
            entries: vec![ClipboardEntry::ExternalPaths(ExternalPaths(
                vec![PathBuf::from("a.txt")].into(),
            ))],
        };
        assert!(should_intercept_paste(&paths_first));

        // The source app's priority order decides mixed clipboards: a leading
        // String entry (browser/Word copy) means "this is a text paste".
        let text_then_image = ClipboardItem {
            entries: vec![
                ClipboardEntry::String(gpui::ClipboardString::new("hello".into())),
                ClipboardEntry::Image(image.clone()),
            ],
        };
        assert!(!should_intercept_paste(&text_then_image));

        let image_then_text = ClipboardItem {
            entries: vec![
                ClipboardEntry::Image(image),
                ClipboardEntry::String(gpui::ClipboardString::new("hello".into())),
            ],
        };
        assert!(should_intercept_paste(&image_then_text));
    }

    #[test]
    fn test_attachment_dir_for_key() {
        assert_eq!(
            attachment_dir_for_key(Some("abc")),
            paths::data_dir().join("inbox_attachments").join("abc")
        );
        assert_eq!(
            attachment_dir_for_key(None),
            paths::data_dir().join("inbox_attachments").join("unkeyed")
        );
    }

    #[gpui::test]
    async fn test_save_pasted_image_writes_bytes_and_uniquifies(cx: &mut TestAppContext) {
        let fake_fs = FakeFs::new(cx.executor());
        fake_fs.insert_tree(path!("/data"), json!({})).await;
        let fs: Arc<dyn Fs> = fake_fs;
        let dir = Path::new(path!("/data/attachments"));
        let image = png_image(vec![1, 2, 3]);

        let first = save_pasted_image(&fs, dir, "pasted-x", image.clone(), cx.executor().clone())
            .await
            .unwrap();
        assert_eq!(first, dir.join("pasted-x.png"));
        assert_eq!(fs.load_bytes(&first).await.unwrap(), vec![1, 2, 3]);

        // A second paste with the same stem must not overwrite the first file.
        let second = save_pasted_image(&fs, dir, "pasted-x", image, cx.executor().clone())
            .await
            .unwrap();
        assert_eq!(second, dir.join("pasted-x-1.png"));
        assert_eq!(fs.load_bytes(&second).await.unwrap(), vec![1, 2, 3]);
    }

    #[gpui::test]
    async fn test_save_pasted_image_transcodes_bmp_to_png(cx: &mut TestAppContext) {
        let fake_fs = FakeFs::new(cx.executor());
        fake_fs.insert_tree(path!("/data"), json!({})).await;
        let fs: Arc<dyn Fs> = fake_fs;
        let dir = Path::new(path!("/data/attachments"));

        // A 2x1 BMP encoded with the same crate the transcode uses.
        let mut bmp = Vec::new();
        image::ImageBuffer::from_pixel(2, 1, image::Rgb([255u8, 0, 0]))
            .write_to(&mut std::io::Cursor::new(&mut bmp), image::ImageFormat::Bmp)
            .unwrap();

        let path = save_pasted_image(
            &fs,
            dir,
            "pasted-x",
            Image::from_bytes(ImageFormat::Bmp, bmp),
            cx.executor().clone(),
        )
        .await
        .unwrap();

        assert_eq!(path, dir.join("pasted-x.png"));
        let decoded = image::load_from_memory_with_format(
            &fs.load_bytes(&path).await.unwrap(),
            image::ImageFormat::Png,
        )
        .unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 1));
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
