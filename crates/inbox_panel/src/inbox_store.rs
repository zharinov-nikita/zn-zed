use std::{path::PathBuf, sync::Arc, time::Duration};

use fs::Fs;
use gpui::{App, Context, Entity, EventEmitter, Subscription, Task};
use project::{Project, WorktreeId};
use util::rel_path::RelPath;

use crate::inbox_model::{
    InboxFile, InboxItem, InboxType, ItemId, SortMode, TYPE_COLOR_TOKENS, new_item_id, now_unix,
};

const SAVE_DEBOUNCE: Duration = Duration::from_millis(250);

/// Path of the inbox file, relative to the worktree root.
pub fn inbox_file_relative_path() -> &'static RelPath {
    static CACHED: std::sync::LazyLock<&'static RelPath> =
        std::sync::LazyLock::new(|| RelPath::from_unix_str(".zed/inbox.json").unwrap());
    *CACHED
}

#[derive(Clone, Debug, PartialEq)]
pub enum InboxStoreEvent {
    Changed,
    Reloaded,
    ItemDeleted(ItemId),
}

/// Holds the in-memory inbox state for the first visible worktree of a
/// project, persists it to `.zed/inbox.json` (debounced, atomic), and watches
/// the file for external changes.
pub struct InboxStore {
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    state: InboxFile,
    worktree_id: Option<WorktreeId>,
    /// The last content we wrote (or loaded), used to suppress reloads caused
    /// by our own writes.
    last_saved_content: Option<String>,
    load_error: Option<String>,
    /// Set when the most recent debounced save failed to write to disk. The
    /// mutation stays `dirty` in that case so it keeps being retried on the
    /// next mutation instead of being silently lost.
    save_error: Option<String>,
    /// Whether the in-memory state has mutations that are not on disk yet.
    dirty: bool,
    pending_save: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<InboxStoreEvent> for InboxStore {}

impl InboxStore {
    pub fn new(project: Entity<Project>, fs: Arc<dyn Fs>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&project, Self::handle_project_event);
        let worktree_id = project
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).id());
        let mut this = Self {
            project,
            fs,
            state: InboxFile::default(),
            worktree_id,
            last_saved_content: None,
            load_error: None,
            save_error: None,
            dirty: false,
            pending_save: Task::ready(()),
            _subscriptions: vec![subscription],
        };
        this.reload(cx);
        this
    }

    fn handle_project_event(
        &mut self,
        _: Entity<Project>,
        event: &project::Event,
        cx: &mut Context<Self>,
    ) {
        match event {
            project::Event::WorktreeUpdatedEntries(worktree_id, changes) => {
                if Some(*worktree_id) == self.worktree_id
                    && changes
                        .iter()
                        .any(|(path, _, _)| path.as_ref() == inbox_file_relative_path())
                {
                    self.reload(cx);
                }
            }
            project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_) => {
                self.rebind_worktree(cx);
            }
            _ => {}
        }
    }

    fn rebind_worktree(&mut self, cx: &mut Context<Self>) {
        let worktree_id = self
            .project
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).id());
        if worktree_id != self.worktree_id {
            self.worktree_id = worktree_id;
            self.last_saved_content = None;
            self.dirty = false;
            self.reload(cx);
        }
    }

    fn inbox_abs_path(&self, cx: &App) -> Option<PathBuf> {
        let worktree = self
            .project
            .read(cx)
            .worktree_for_id(self.worktree_id?, cx)?;
        Some(worktree.read(cx).absolutize(inbox_file_relative_path()))
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(abs_path) = self.inbox_abs_path(cx) else {
            if self.dirty {
                // Unsaved local mutations win over whatever is on disk right
                // now; the pending save will persist them shortly.
                return;
            }
            if self.state != InboxFile::default() {
                self.state = InboxFile::default();
                self.last_saved_content = None;
                self.load_error = None;
                self.save_error = None;
                cx.emit(InboxStoreEvent::Reloaded);
            }
            return;
        };
        let fs = self.fs.clone();
        cx.spawn(async move |this, cx| {
            let content = if fs.is_file(&abs_path).await {
                Some(fs.load(&abs_path).await)
            } else {
                None
            };
            this.update(cx, |this, cx| this.finish_reload(content, cx))
                .ok();
        })
        .detach();
    }

    fn finish_reload(&mut self, content: Option<anyhow::Result<String>>, cx: &mut Context<Self>) {
        if self.dirty {
            // Unsaved local mutations win over whatever is on disk right now;
            // the pending save will persist them shortly.
            return;
        }
        match content {
            None => {
                // Missing file is not an error: it means an empty inbox.
                self.load_error = None;
                self.last_saved_content = None;
                if self.state != InboxFile::default() {
                    self.state = InboxFile::default();
                    cx.emit(InboxStoreEvent::Reloaded);
                }
            }
            Some(Err(error)) => {
                self.load_error = Some(format!("{error:#}"));
                cx.emit(InboxStoreEvent::Changed);
            }
            Some(Ok(content)) => {
                if self.last_saved_content.as_deref() == Some(content.as_str()) {
                    // Echo of our own write.
                    return;
                }
                match serde_json::from_str::<InboxFile>(&content) {
                    Ok(mut state) => {
                        let now = now_unix();
                        for item in state.inbox.iter_mut().chain(state.archived.iter_mut()) {
                            if item.created.is_none() {
                                item.created = Some(now);
                            }
                        }
                        self.state = state;
                        self.last_saved_content = Some(content);
                        self.load_error = None;
                        cx.emit(InboxStoreEvent::Reloaded);
                    }
                    Err(error) => {
                        self.load_error = Some(error.to_string());
                        cx.emit(InboxStoreEvent::Changed);
                    }
                }
            }
        }
    }

    fn on_mutated(&mut self, cx: &mut Context<Self>) {
        self.dirty = true;
        self.load_error = None;
        cx.emit(InboxStoreEvent::Changed);
        self.schedule_save(cx);
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.pending_save = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            let Ok(Some((fs, abs_path, file))) = this.update(cx, |this, cx| {
                if !this.dirty {
                    return None;
                }
                let abs_path = this.inbox_abs_path(cx)?;
                let mut file = this.state.clone();
                file.version = Some(1);
                Some((this.fs.clone(), abs_path, file))
            }) else {
                return;
            };
            let Ok(content) = cx
                .background_executor()
                .spawn(async move {
                    serde_json::to_string_pretty(&file).map(|mut content| {
                        content.push('\n');
                        content
                    })
                })
                .await
            else {
                return;
            };
            let Ok(previous_last_saved_content) = this.update(cx, |this, _| {
                let previous = this.last_saved_content.take();
                this.dirty = false;
                this.last_saved_content = Some(content.clone());
                previous
            }) else {
                return;
            };

            let write_result = async {
                if let Some(dir) = abs_path.parent() {
                    fs.create_dir(dir).await?;
                }
                fs.atomic_write(abs_path, content).await
            }
            .await;

            this.update(cx, |this, cx| match write_result {
                Ok(()) => {
                    this.save_error = None;
                }
                Err(error) => {
                    // The write failed: restore `dirty` and the previous
                    // `last_saved_content` so the mutation is retried on the
                    // next edit instead of being silently lost, and so a
                    // later file-change event for the (still stale) on-disk
                    // content isn't mistaken for an echo of our own write.
                    this.dirty = true;
                    this.last_saved_content = previous_last_saved_content;
                    this.save_error = Some(format!("{error:#}"));
                    cx.emit(InboxStoreEvent::Changed);
                }
            })
            .ok();
        });
    }

    // Getters

    pub fn items(&self) -> &[InboxItem] {
        &self.state.inbox
    }

    pub fn archived(&self) -> &[InboxItem] {
        &self.state.archived
    }

    pub fn item(&self, id: &ItemId) -> Option<&InboxItem> {
        self.state
            .inbox
            .iter()
            .chain(self.state.archived.iter())
            .find(|item| &item.id == id)
    }

    pub fn types(&self) -> &[InboxType] {
        &self.state.types
    }

    /// Current ordering of open items.
    pub fn sort_mode(&self) -> SortMode {
        self.state.sort
    }

    /// Whether the meta field with the given key is hidden on item rows.
    pub fn is_field_hidden(&self, key: &str) -> bool {
        self.state.hidden_fields.iter().any(|hidden| hidden == key)
    }

    /// Shows/hides the meta field with the given key on item rows.
    pub fn toggle_field(&mut self, key: &str, cx: &mut Context<Self>) {
        if let Some(index) = self
            .state
            .hidden_fields
            .iter()
            .position(|hidden| hidden == key)
        {
            self.state.hidden_fields.remove(index);
        } else {
            self.state.hidden_fields.push(key.to_string());
        }
        self.on_mutated(cx);
    }

    /// Resolves the type of an item. Returns `None` when the item has no
    /// kind, or when its kind matches no existing type.
    pub fn resolve_kind(&self, item: &InboxItem) -> Option<&InboxType> {
        let key = item.kind.as_deref()?;
        self.types().iter().find(|inbox_type| inbox_type.key == key)
    }

    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// Set when the most recent debounced save failed to write to disk. The
    /// mutation remains dirty and will be retried on the next save attempt.
    pub fn save_error(&self) -> Option<&str> {
        self.save_error.as_deref()
    }

    pub fn has_worktree(&self) -> bool {
        self.worktree_id.is_some()
    }

    // Mutations

    /// Adds a new item to the top of the inbox and returns its id.
    pub fn capture(
        &mut self,
        text: String,
        kind: Option<String>,
        from: Option<String>,
        cx: &mut Context<Self>,
    ) -> ItemId {
        let id = new_item_id();
        self.state.inbox.insert(
            0,
            InboxItem {
                id: id.clone(),
                text,
                kind,
                from,
                body: None,
                created: Some(now_unix()),
                cleared: None,
            },
        );
        self.on_mutated(cx);
        id
    }

    /// Applies `f` to the item with the given id, searching both the inbox and
    /// the archive. When `f` leaves the item unchanged, nothing is marked
    /// dirty, no event is emitted and no save is scheduled.
    pub fn update_item(
        &mut self,
        id: &ItemId,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut InboxItem),
    ) {
        let Some(item) = self
            .state
            .inbox
            .iter_mut()
            .chain(self.state.archived.iter_mut())
            .find(|item| &item.id == id)
        else {
            return;
        };
        let before = item.clone();
        f(item);
        if *item == before {
            return;
        }
        self.on_mutated(cx);
    }

    pub fn set_kind(&mut self, id: &ItemId, kind: Option<String>, cx: &mut Context<Self>) {
        self.update_item(id, cx, |item| item.kind = kind);
    }

    /// Sets how open items are ordered.
    pub fn set_sort(&mut self, mode: SortMode, cx: &mut Context<Self>) {
        if self.state.sort != mode {
            self.state.sort = mode;
            self.on_mutated(cx);
        }
    }

    /// Moves the item `id` to just before `target_id` in manual order. No-op if
    /// either id is missing, they are equal, or the order would not change.
    pub fn move_item_before(&mut self, id: &ItemId, target_id: &ItemId, cx: &mut Context<Self>) {
        if id == target_id {
            return;
        }
        let before: Vec<ItemId> = self
            .state
            .inbox
            .iter()
            .map(|item| item.id.clone())
            .collect();
        let Some(from) = self.state.inbox.iter().position(|item| &item.id == id) else {
            return;
        };
        let item = self.state.inbox.remove(from);
        let insert_at = self
            .state
            .inbox
            .iter()
            .position(|item| &item.id == target_id)
            .unwrap_or_else(|| from.min(self.state.inbox.len()));
        self.state.inbox.insert(insert_at, item);
        let after: Vec<ItemId> = self
            .state
            .inbox
            .iter()
            .map(|item| item.id.clone())
            .collect();
        if before != after {
            self.on_mutated(cx);
        }
    }

    /// Reorders the lists alphabetically by label (case-insensitive).
    pub fn sort_types_alpha(&mut self, cx: &mut Context<Self>) {
        let before: Vec<String> = self.state.types.iter().map(|t| t.key.clone()).collect();
        self.state
            .types
            .sort_by_key(|inbox_type| inbox_type.label.to_lowercase());
        let after: Vec<String> = self.state.types.iter().map(|t| t.key.clone()).collect();
        if before != after {
            self.on_mutated(cx);
        }
    }

    /// Moves the list `key` to just before `target_key`. No-op if either key is
    /// missing, they are equal, or the order would not change.
    pub fn move_type_before(&mut self, key: &str, target_key: &str, cx: &mut Context<Self>) {
        if key == target_key {
            return;
        }
        let before: Vec<String> = self.state.types.iter().map(|t| t.key.clone()).collect();
        let Some(from) = self.state.types.iter().position(|t| t.key == key) else {
            return;
        };
        let inbox_type = self.state.types.remove(from);
        let insert_at = self
            .state
            .types
            .iter()
            .position(|t| t.key == target_key)
            .unwrap_or_else(|| from.min(self.state.types.len()));
        self.state.types.insert(insert_at, inbox_type);
        let after: Vec<String> = self.state.types.iter().map(|t| t.key.clone()).collect();
        if before != after {
            self.on_mutated(cx);
        }
    }

    pub fn set_text(&mut self, id: &ItemId, text: String, cx: &mut Context<Self>) {
        self.update_item(id, cx, |item| item.text = text);
    }

    pub fn set_body(&mut self, id: &ItemId, body: Option<String>, cx: &mut Context<Self>) {
        self.update_item(id, cx, |item| item.body = body);
    }

    pub fn toggle_cleared(&mut self, id: &ItemId, cx: &mut Context<Self>) {
        self.update_item(id, cx, |item| {
            item.cleared = if item.cleared.is_some() {
                None
            } else {
                Some(now_unix())
            };
        });
    }

    pub fn delete_item(&mut self, id: &ItemId, cx: &mut Context<Self>) {
        let inbox_len = self.state.inbox.len();
        let archived_len = self.state.archived.len();
        self.state.inbox.retain(|item| &item.id != id);
        self.state.archived.retain(|item| &item.id != id);
        if self.state.inbox.len() == inbox_len && self.state.archived.len() == archived_len {
            return;
        }
        cx.emit(InboxStoreEvent::ItemDeleted(id.clone()));
        self.on_mutated(cx);
    }

    /// Moves an archived item back to the top of the inbox, un-clearing it.
    pub fn restore(&mut self, id: &ItemId, cx: &mut Context<Self>) {
        let Some(index) = self.state.archived.iter().position(|item| &item.id == id) else {
            return;
        };
        let mut item = self.state.archived.remove(index);
        item.cleared = None;
        self.state.inbox.insert(0, item);
        self.on_mutated(cx);
    }

    // Type mutations.

    pub fn rename_type(&mut self, key: &str, label: String, cx: &mut Context<Self>) {
        let Some(inbox_type) = self
            .state
            .types
            .iter_mut()
            .find(|inbox_type| inbox_type.key == key)
        else {
            return;
        };
        inbox_type.label = label;
        self.on_mutated(cx);
    }

    /// Switches the type's color to the next token in [`TYPE_COLOR_TOKENS`].
    pub fn cycle_type_color(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(inbox_type) = self
            .state
            .types
            .iter_mut()
            .find(|inbox_type| inbox_type.key == key)
        else {
            return;
        };
        let next = match TYPE_COLOR_TOKENS
            .iter()
            .position(|token| *token == inbox_type.color)
        {
            Some(index) => TYPE_COLOR_TOKENS[(index + 1) % TYPE_COLOR_TOKENS.len()],
            None => TYPE_COLOR_TOKENS[0],
        };
        inbox_type.color = next.to_string();
        self.on_mutated(cx);
    }

    /// Deletes a type; items of that type become unassigned. Any list can be
    /// deleted, including the last one — lists start empty by default.
    pub fn delete_type(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(index) = self
            .state
            .types
            .iter()
            .position(|inbox_type| inbox_type.key == key)
        else {
            return;
        };
        self.state.types.remove(index);
        for item in self
            .state
            .inbox
            .iter_mut()
            .chain(self.state.archived.iter_mut())
        {
            if item.kind.as_deref() == Some(key) {
                item.kind = None;
            }
        }
        self.on_mutated(cx);
    }

    /// Adds a new type with a generated key and the next color in the palette.
    /// Returns the new key.
    pub fn add_type(&mut self, cx: &mut Context<Self>) -> String {
        let key = format!("k{}", new_item_id());
        let color = TYPE_COLOR_TOKENS[self.state.types.len() % TYPE_COLOR_TOKENS.len()];
        self.state.types.push(InboxType {
            key: key.clone(),
            label: "New list".to_string(),
            color: color.to_string(),
        });
        self.on_mutated(cx);
        key
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, path::Path, rc::Rc};

    use fs::FakeFs;
    use gpui::{AppContext as _, TestAppContext};
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    use super::*;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    async fn build_store(
        fs: Arc<FakeFs>,
        cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<InboxStore>) {
        let project = Project::test(fs.clone(), [path!("/root").as_ref() as &Path], cx).await;
        let store = cx.new(|cx| InboxStore::new(project.clone(), fs, cx));
        cx.run_until_parked();
        (project, store)
    }

    fn track_events(
        store: &Entity<InboxStore>,
        cx: &mut TestAppContext,
    ) -> Rc<RefCell<Vec<InboxStoreEvent>>> {
        let events = Rc::new(RefCell::new(Vec::new()));
        let captured = events.clone();
        cx.update(|cx| {
            cx.subscribe(store, move |_, event, _| {
                captured.borrow_mut().push(event.clone());
            })
            .detach();
        });
        events
    }

    fn flush_saves(cx: &mut TestAppContext) {
        cx.executor().advance_clock(SAVE_DEBOUNCE * 2);
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_load_existing_file(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".zed": {
                    "inbox.json": r#"{
                        "version": 1,
                        "inbox": [
                            { "id": "abc", "text": "first", "kind": "task", "created": 100 },
                            { "text": "second" }
                        ],
                        "archived": [
                            { "id": "old", "text": "done", "cleared": 200 }
                        ]
                    }"#
                }
            }),
        )
        .await;
        let (_project, store) = build_store(fs, cx).await;

        store.read_with(cx, |store, _| {
            assert!(store.has_worktree());
            assert_eq!(store.load_error(), None);
            assert_eq!(store.items().len(), 2);
            assert_eq!(store.items()[0].id.as_ref(), "abc");
            assert_eq!(store.items()[0].kind.as_deref(), Some("task"));
            assert_eq!(store.items()[0].created, Some(100));
            // Missing id and created are backfilled.
            assert!(!store.items()[1].id.is_empty());
            assert!(store.items()[1].created.is_some());
            assert_eq!(store.archived().len(), 1);
            assert!(store.archived()[0].is_cleared());
            // No custom types in the file means no types at all.
            assert!(store.types().is_empty());
            assert!(store.resolve_kind(&store.items()[1]).is_none());
        });
    }

    #[gpui::test]
    async fn test_capture_creates_file(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, store) = build_store(fs.clone(), cx).await;

        let id = store.update(cx, |store, cx| {
            store.capture(
                "todo: fix panel".to_string(),
                Some("task".to_string()),
                Some("src/main.rs:1".to_string()),
                cx,
            )
        });
        flush_saves(cx);

        let content = fs
            .load(path!("/root/.zed/inbox.json").as_ref())
            .await
            .unwrap();
        assert!(content.ends_with('\n'));
        let parsed: InboxFile = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.version, Some(1));
        assert_eq!(parsed.inbox.len(), 1);
        assert_eq!(parsed.inbox[0].id, id);
        assert_eq!(parsed.inbox[0].text, "todo: fix panel");
        assert_eq!(parsed.inbox[0].kind.as_deref(), Some("task"));
        assert_eq!(parsed.inbox[0].from.as_deref(), Some("src/main.rs:1"));
        assert!(parsed.inbox[0].created.is_some());
        // Default types are not serialized.
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(value.get("types").is_none());
    }

    #[gpui::test]
    async fn test_external_change_reloads(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".zed": {
                    "inbox.json": r#"{ "inbox": [{ "id": "one", "text": "old" }] }"#
                }
            }),
        )
        .await;
        let (_project, store) = build_store(fs.clone(), cx).await;
        let events = track_events(&store, cx);

        fs.save(
            path!("/root/.zed/inbox.json").as_ref(),
            &r#"{ "inbox": [{ "id": "one", "text": "new" }, { "id": "two", "text": "added" }] }"#
                .into(),
            Default::default(),
        )
        .await
        .unwrap();
        flush_saves(cx);

        assert!(events.borrow().contains(&InboxStoreEvent::Reloaded));
        store.read_with(cx, |store, _| {
            assert_eq!(store.load_error(), None);
            assert_eq!(store.items().len(), 2);
            assert_eq!(store.items()[0].text, "new");
            assert_eq!(store.items()[1].id.as_ref(), "two");
        });
    }

    #[gpui::test]
    async fn test_own_save_does_not_reload(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, store) = build_store(fs.clone(), cx).await;
        let events = track_events(&store, cx);

        store.update(cx, |store, cx| {
            store.capture("only item".to_string(), None, None, cx);
        });
        flush_saves(cx);
        // Give the worktree plenty of time to deliver the file event back.
        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();

        assert!(
            fs.is_file(path!("/root/.zed/inbox.json").as_ref()).await,
            "the file should have been written"
        );
        assert_eq!(
            events.borrow().as_slice(),
            &[InboxStoreEvent::Changed],
            "our own write must not produce a Reloaded event"
        );
        store.read_with(cx, |store, _| {
            assert_eq!(store.items().len(), 1);
            assert_eq!(store.items()[0].text, "only item");
        });
    }

    #[gpui::test]
    async fn test_broken_json_sets_load_error(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".zed": {
                    "inbox.json": r#"{ "inbox": [{ "id": "one", "text": "keep me" }] }"#
                }
            }),
        )
        .await;
        let (_project, store) = build_store(fs.clone(), cx).await;
        let events = track_events(&store, cx);

        fs.save(
            path!("/root/.zed/inbox.json").as_ref(),
            &r#"{ "inbox": [ broken"#.into(),
            Default::default(),
        )
        .await
        .unwrap();
        flush_saves(cx);

        assert!(events.borrow().contains(&InboxStoreEvent::Changed));
        assert!(!events.borrow().contains(&InboxStoreEvent::Reloaded));
        store.read_with(cx, |store, _| {
            assert!(store.load_error().is_some());
            // The previous state is kept.
            assert_eq!(store.items().len(), 1);
            assert_eq!(store.items()[0].text, "keep me");
        });

        // An explicit user mutation clears the error and writes the file.
        store.update(cx, |store, cx| {
            store.capture("fresh".to_string(), None, None, cx);
        });
        flush_saves(cx);
        store.read_with(cx, |store, _| assert_eq!(store.load_error(), None));
        let content = fs
            .load(path!("/root/.zed/inbox.json").as_ref())
            .await
            .unwrap();
        let parsed: InboxFile = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.inbox.len(), 2);
    }

    #[gpui::test]
    async fn test_restore_and_delete(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".zed": {
                    "inbox.json": r#"{ "archived": [{ "id": "b", "text": "b", "cleared": 1 }] }"#
                }
            }),
        )
        .await;
        let (_project, store) = build_store(fs.clone(), cx).await;

        let id_a = store.update(cx, |store, cx| {
            store.capture("a".to_string(), None, None, cx)
        });
        let id_b: ItemId = "b".into();

        store.read_with(cx, |store, _| {
            assert_eq!(store.items().len(), 1);
            assert_eq!(store.items()[0].id, id_a);
            assert_eq!(store.archived().len(), 1);
            assert_eq!(store.archived()[0].id, id_b);
            assert!(store.archived()[0].is_cleared());
        });

        store.update(cx, |store, cx| store.restore(&id_b, cx));
        store.read_with(cx, |store, _| {
            assert_eq!(store.archived().len(), 0);
            assert_eq!(store.items().len(), 2);
            assert_eq!(store.items()[0].id, id_b);
            assert!(!store.items()[0].is_cleared());
        });

        let events = track_events(&store, cx);
        store.update(cx, |store, cx| store.delete_item(&id_a, cx));
        assert_eq!(
            events.borrow().as_slice(),
            &[
                InboxStoreEvent::ItemDeleted(id_a.clone()),
                InboxStoreEvent::Changed
            ]
        );
        store.read_with(cx, |store, _| {
            assert_eq!(store.items().len(), 1);
            assert!(store.item(&id_a).is_none());
            assert!(store.item(&id_b).is_some());
        });

        // Everything survives a save/parse round-trip.
        flush_saves(cx);
        let content = fs
            .load(path!("/root/.zed/inbox.json").as_ref())
            .await
            .unwrap();
        let parsed: InboxFile = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.inbox.len(), 1);
        assert_eq!(parsed.inbox[0].id, id_b);
    }

    #[gpui::test]
    async fn test_custom_types_are_persisted(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, store) = build_store(fs.clone(), cx).await;

        store.update(cx, |store, cx| {
            store.capture("x".to_string(), None, None, cx);
        });
        flush_saves(cx);
        let value: serde_json::Value = serde_json::from_str(
            &fs.load(path!("/root/.zed/inbox.json").as_ref())
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(
            value.get("types").is_none(),
            "no types by default must not be written"
        );

        let key = store.update(cx, |store, cx| {
            let key = store.add_type(cx);
            store.rename_type(&key, "TODO".to_string(), cx);
            key
        });
        flush_saves(cx);
        let parsed: InboxFile = serde_json::from_str(
            &fs.load(path!("/root/.zed/inbox.json").as_ref())
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(parsed.types.len(), 1);
        assert_eq!(parsed.types[0].key, key);
        assert_eq!(parsed.types[0].label, "TODO");
    }

    #[gpui::test]
    async fn test_sort_mutations(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, store) = build_store(fs.clone(), cx).await;

        // Default sort is Manual.
        store.read_with(cx, |store, _| {
            assert_eq!(store.sort_mode(), SortMode::Manual)
        });

        store.update(cx, |store, cx| {
            store.set_sort(SortMode::Az, cx);
            assert_eq!(store.sort_mode(), SortMode::Az);
        });

        // Sorting lists alphabetically reorders types by label (case-insensitive).
        let (key_banana, key_apple) = store.update(cx, |store, cx| {
            let key_banana = store.add_type(cx);
            store.rename_type(&key_banana, "Banana".to_string(), cx);
            let key_apple = store.add_type(cx);
            store.rename_type(&key_apple, "apple".to_string(), cx);
            (key_banana, key_apple)
        });
        store.update(cx, |store, cx| {
            store.sort_types_alpha(cx);
            let labels: Vec<_> = store.types().iter().map(|t| t.label.clone()).collect();
            assert_eq!(labels, ["apple", "Banana"]);
            assert_eq!(store.types()[0].key, key_apple);
            assert_eq!(store.types()[1].key, key_banana);
        });

        // Field visibility toggles are additive and reversible; unknown fields
        // default to visible.
        store.update(cx, |store, cx| {
            assert!(!store.is_field_hidden("age"));
            store.toggle_field("age", cx);
            assert!(store.is_field_hidden("age"));
            store.toggle_field("age", cx);
            assert!(!store.is_field_hidden("age"));
        });
    }

    #[gpui::test]
    async fn test_reorder_mutations(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, store) = build_store(fs.clone(), cx).await;

        // Capture inserts at the top, so the order becomes c, b, a.
        let (item_a, _item_b, item_c) = store.update(cx, |store, cx| {
            let a = store.capture("a".to_string(), None, None, cx);
            let b = store.capture("b".to_string(), None, None, cx);
            let c = store.capture("c".to_string(), None, None, cx);
            (a, b, c)
        });
        let texts = |store: &InboxStore| {
            store
                .items()
                .iter()
                .map(|item| item.text.clone())
                .collect::<Vec<_>>()
        };
        store.read_with(cx, |store, _| assert_eq!(texts(store), ["c", "b", "a"]));

        // Move `a` to just before `c`: a, c, b.
        store.update(cx, |store, cx| store.move_item_before(&item_a, &item_c, cx));
        store.read_with(cx, |store, _| assert_eq!(texts(store), ["a", "c", "b"]));

        // Moving onto itself is a no-op.
        store.update(cx, |store, cx| store.move_item_before(&item_a, &item_a, cx));
        store.read_with(cx, |store, _| assert_eq!(texts(store), ["a", "c", "b"]));

        // Lists: order one, two, three; move three before one -> three, one, two.
        let (key_one, _key_two, key_three) = store.update(cx, |store, cx| {
            let one = store.add_type(cx);
            store.rename_type(&one, "one".to_string(), cx);
            let two = store.add_type(cx);
            store.rename_type(&two, "two".to_string(), cx);
            let three = store.add_type(cx);
            store.rename_type(&three, "three".to_string(), cx);
            (one, two, three)
        });
        store.update(cx, |store, cx| {
            store.move_type_before(&key_three, &key_one, cx)
        });
        store.read_with(cx, |store, _| {
            let labels: Vec<_> = store.types().iter().map(|t| t.label.clone()).collect();
            assert_eq!(labels, ["three", "one", "two"]);
        });
    }

    #[gpui::test]
    async fn test_type_mutations(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, store) = build_store(fs.clone(), cx).await;

        // No types exist by default.
        store.read_with(cx, |store, _| assert!(store.types().is_empty()));

        let (key_a, key_b) = store.update(cx, |store, cx| {
            // Adding a type appends a fresh one with the next color and a
            // default label.
            let key_a = store.add_type(cx);
            assert_eq!(store.types()[0].label, "New list");
            assert_eq!(store.types()[0].color, "accent");

            let key_b = store.add_type(cx);
            assert_eq!(store.types()[1].color, "created");
            (key_a, key_b)
        });

        let item_id = store.update(cx, |store, cx| {
            store.capture("idea item".to_string(), Some(key_b.clone()), None, cx)
        });

        store.update(cx, |store, cx| {
            // Cycling moves the color to the next token.
            store.cycle_type_color(&key_a, cx);
            assert_eq!(store.types()[0].color, "created");

            // Deleting a type unassigns its items (kind cleared to None).
            store.delete_type(&key_b, cx);
            assert!(store.types().iter().all(|t| t.key != key_b));
        });
        store.read_with(cx, |store, _| {
            assert_eq!(store.item(&item_id).unwrap().kind, None);
        });

        store.update(cx, |store, cx| {
            // The last remaining type can be deleted, leaving no lists.
            store.delete_type(&key_a, cx);
            assert_eq!(store.types().len(), 0);

            // Adding a type appends a fresh one.
            let key = store.add_type(cx);
            assert_eq!(store.types().len(), 1);
            assert_eq!(store.types()[0].key, key);
            assert_eq!(store.types()[0].label, "New list");
        });
    }

    #[gpui::test]
    async fn test_noop_update_item_does_not_dirty_or_save(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, store) = build_store(fs.clone(), cx).await;

        let id = store.update(cx, |store, cx| {
            store.capture("x".to_string(), Some("task".to_string()), None, cx)
        });
        flush_saves(cx);
        let events = track_events(&store, cx);

        // Re-applying the current values must not dirty the store, emit an
        // event or schedule another save.
        store.update(cx, |store, cx| {
            store.set_kind(&id, Some("task".to_string()), cx);
            store.set_text(&id, "x".to_string(), cx);
            store.set_body(&id, None, cx);
        });
        store.read_with(cx, |store, _| {
            assert!(!store.dirty, "a no-op update must not mark the store dirty");
        });
        assert!(
            events.borrow().is_empty(),
            "a no-op update must not emit events, got {:?}",
            events.borrow()
        );

        // A real change still goes through.
        store.update(cx, |store, cx| store.set_text(&id, "y".to_string(), cx));
        assert_eq!(events.borrow().as_slice(), &[InboxStoreEvent::Changed]);
    }

    #[gpui::test]
    async fn test_save_failure_keeps_mutation_dirty(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        // `inbox.json` already exists as a directory, so the debounced save's
        // `atomic_write` will genuinely fail (writing a file over a
        // directory is not allowed) without needing any error-injection
        // hook.
        fs.insert_tree(
            path!("/root"),
            json!({
                ".zed": {
                    "inbox.json": {}
                }
            }),
        )
        .await;
        let (_project, store) = build_store(fs.clone(), cx).await;
        let events = track_events(&store, cx);

        store.update(cx, |store, cx| {
            store.capture("keep me".to_string(), None, None, cx);
        });
        flush_saves(cx);

        store.read_with(cx, |store, _| {
            assert!(
                store.dirty,
                "a failed write must not be reported as saved, or the \
                 mutation would never be retried and could be lost"
            );
            assert!(
                store.save_error().is_some(),
                "the failure must be surfaced to the UI"
            );
            assert!(
                store.last_saved_content.is_none(),
                "last_saved_content must be rolled back to its pre-write \
                 value on failure, otherwise a later echo of the (still \
                 stale) on-disk content could be mistaken for our own write"
            );
            assert_eq!(store.items().len(), 1);
            assert_eq!(store.items()[0].text, "keep me");
        });
        assert!(events.borrow().contains(&InboxStoreEvent::Changed));

        // A later reload (e.g. triggered by a worktree event for the
        // still-broken path) must not clobber the unsaved mutation.
        store.update(cx, |store, cx| store.reload(cx));
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert_eq!(store.items().len(), 1);
            assert_eq!(store.items()[0].text, "keep me");
        });
    }

    #[gpui::test]
    async fn test_reload_without_worktree_preserves_dirty_state(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, store) = build_store(fs.clone(), cx).await;
        let events = track_events(&store, cx);

        store.update(cx, |store, cx| {
            store.capture("unsaved".to_string(), None, None, cx);
            // Simulate the worktree becoming momentarily unresolvable (e.g. a
            // race between a `WorktreeRemoved` event and a stale file-change
            // event) while a mutation hasn't been saved yet.
            store.worktree_id = None;
            store.reload(cx);
        });

        store.read_with(cx, |store, _| {
            assert!(
                store.dirty,
                "an unsaved mutation must not be discarded just because the \
                 worktree momentarily could not be resolved"
            );
            assert_eq!(store.items().len(), 1);
            assert_eq!(store.items()[0].text, "unsaved");
        });
        assert!(
            !events.borrow().contains(&InboxStoreEvent::Reloaded),
            "no reload should be reported while a mutation is unsaved"
        );
    }
}
