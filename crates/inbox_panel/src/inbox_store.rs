use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use collections::HashSet;
use db::kvp::KeyValueStore;
use fs::{Fs, RemoveOptions};
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, Subscription, Task,
    TaskExt as _, WeakEntity,
};
use project::{Project, WorktreeId};
use sha2::{Digest as _, Sha256};
use util::ResultExt as _;

use crate::inbox_model::{
    AttachmentRef, CATALOG_COLOR_TOKENS, CatalogEntry, CatalogKind, InboxFile, InboxItem, ItemId,
    SortMode, new_item_id, now_unix, now_unix_millis,
};

const SAVE_DEBOUNCE: Duration = Duration::from_millis(250);

/// How many backup snapshots to keep per project in the out-of-repo ring.
const BACKUP_KEEP: usize = 10;

/// Namespace of the inbox entries in Zed's scoped key-value store; the key
/// within it identifies the project (see [`project_key`]).
const INBOX_KV_NAMESPACE: &str = "inbox_panel";

/// Version pinned into every write. Loads refuse a document written by a
/// newer Zed so an older build can't destroy data it doesn't understand.
const CURRENT_INBOX_VERSION: u32 = 1;

/// Key of a project's entry in the scoped key-value store: a hash of the
/// worktree root, so different projects never collide. The backup ring uses
/// the same key, keeping a project's backups findable even when its stored
/// entry is corrupt.
fn project_key(worktree_root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(worktree_root.to_string_lossy().as_bytes());
    let key = format!("{:x}", hasher.finalize());
    key[..16].to_string()
}

/// Directory holding a project's out-of-repo backups. Lives under Zed's
/// global data dir, so it survives DB-level problems and bulk deletes.
fn backup_dir_for_key(key: &str) -> PathBuf {
    paths::data_dir().join("inbox_backups").join(key)
}

/// Process-wide registry of live inbox stores, so out-of-band consumers
/// (the embedded MCP server) can reach the per-window stores. Holds weak
/// handles only: a store dies with its panel/window, and dead handles are
/// pruned on every access — there is no explicit unregister.
#[derive(Default)]
pub struct InboxStoreRegistry {
    stores: Vec<WeakEntity<InboxStore>>,
}

impl Global for InboxStoreRegistry {}

impl InboxStoreRegistry {
    fn register(store: WeakEntity<InboxStore>, cx: &mut App) {
        let registry = cx.default_global::<Self>();
        registry.stores.retain(|weak| weak.upgrade().is_some());
        registry.stores.push(store);
    }

    /// All currently live stores, in registration order.
    pub fn live_stores(cx: &mut App) -> Vec<Entity<InboxStore>> {
        let registry = cx.default_global::<Self>();
        registry.stores.retain(|weak| weak.upgrade().is_some());
        registry
            .stores
            .iter()
            .filter_map(|weak| weak.upgrade())
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum InboxStoreEvent {
    Changed,
    Reloaded,
    ItemDeleted(ItemId),
}

/// Merges `snapshot` into `state` non-destructively: items and catalog
/// entries whose id/key `state` already has keep their current (newer)
/// version; everything else is appended from the snapshot. Settings (sort,
/// hidden fields) keep their current values.
fn merge_missing(state: &mut InboxFile, snapshot: InboxFile) {
    let ids: HashSet<ItemId> = state
        .inbox
        .iter()
        .chain(state.archived.iter())
        .map(|item| item.id.clone())
        .collect();
    state.inbox.extend(
        snapshot
            .inbox
            .into_iter()
            .filter(|item| !ids.contains(&item.id)),
    );
    state.archived.extend(
        snapshot
            .archived
            .into_iter()
            .filter(|item| !ids.contains(&item.id)),
    );
    for (entries, snapshot_entries) in [
        (&mut state.types, snapshot.types),
        (&mut state.tags, snapshot.tags),
    ] {
        let keys: HashSet<String> = entries.iter().map(|entry| entry.key.clone()).collect();
        entries.extend(
            snapshot_entries
                .into_iter()
                .filter(|entry| !keys.contains(&entry.key)),
        );
    }
}

/// Result of reading the persisted inbox document, computed off the main
/// thread so parsing and the backup lookup don't block the UI.
enum ReloadOutcome {
    /// No entry exists in the key-value store and no legacy file was found.
    Missing,
    /// The stored value could not be read, or was read but is not valid JSON
    /// for our schema; the error text carries the distinction.
    Failed(String),
    /// The stored value parsed successfully. Boxed to keep the enum small.
    Loaded(Box<InboxFile>),
    /// No entry exists in the key-value store, but a legacy `.zed/inbox.json`
    /// parsed successfully; adopting it schedules a save that imports it.
    LegacyImported(Box<InboxFile>),
    /// The stored value was written by a newer Zed (`version` above ours);
    /// it must be neither loaded nor offered for a downgrading restore.
    NewerVersion,
}

/// Holds the in-memory inbox state for the first visible worktree of a
/// project and persists it (debounced) to Zed's SQLite key-value store, one
/// entry per project. Legacy `.zed/inbox.json` files are imported once and
/// left untouched.
pub struct InboxStore {
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    key_value_store: KeyValueStore,
    state: InboxFile,
    worktree_id: Option<WorktreeId>,
    /// KV key of the bound worktree's project, cached while the worktree is
    /// alive: on a `WorktreeRemoved` event the worktree is already gone from
    /// the project, but a pending edit must still be flushed to the entry it
    /// owned. The backup directory is derived via [`backup_dir_for_key`].
    bound_project_key: Option<String>,
    load_error: Option<String>,
    /// Set when the most recent debounced save failed to write to the
    /// database. The mutation stays `dirty` in that case so it keeps being
    /// retried on the next mutation instead of being silently lost.
    save_error: Option<String>,
    /// Whether the in-memory state has mutations that are not persisted yet.
    dirty: bool,
    /// Snapshot recovered from the backup ring when the stored document went
    /// missing or corrupt; `Some` while the recovery banner is offered.
    restorable: Option<InboxFile>,
    /// Monotonic counter that keeps backup filenames unique within a session,
    /// even for two saves that land in the same millisecond.
    backup_seq: u64,
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
            key_value_store: KeyValueStore::global(cx),
            state: InboxFile::default(),
            worktree_id,
            bound_project_key: None,
            load_error: None,
            save_error: None,
            dirty: false,
            restorable: None,
            backup_seq: 0,
            pending_save: Task::ready(()),
            _subscriptions: vec![subscription],
        };
        InboxStoreRegistry::register(cx.weak_entity(), cx);
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
        if worktree_id == self.worktree_id {
            return;
        }
        if self.dirty {
            // The pending debounced save would run after the switch, see
            // `dirty == false` and silently drop the edit; write it to the
            // outgoing project's entry now, while its key is still bound.
            self.flush_state_to_database(cx);
        }
        self.worktree_id = worktree_id;
        // Reset to a clean slate before reloading: carrying the old
        // worktree's items across would leak them into the new project (and
        // a missing entry there would even offer to "restore" them).
        self.state = InboxFile::default();
        self.dirty = false;
        self.restorable = None;
        self.load_error = None;
        self.save_error = None;
        cx.emit(InboxStoreEvent::Reloaded);
        self.reload(cx);
    }

    /// Immediately writes the current state to the bound project's entry and
    /// backup ring, bypassing the save debounce. Fire-and-forget: the store
    /// may be rebinding away from this worktree, so failures are only logged.
    fn flush_state_to_database(&mut self, cx: &mut Context<Self>) {
        // The cached key, not a live worktree lookup: on a `WorktreeRemoved`
        // event the worktree is already gone from the project, but the entry
        // it owned must still receive the pending edit.
        let Some(key) = self.bound_project_key.clone() else {
            return;
        };
        let backup_seq = self.next_backup_seq();
        cx.background_spawn(persist_snapshot(
            self.key_value_store.clone(),
            self.fs.clone(),
            key,
            self.state.clone(),
            backup_seq,
        ))
        .detach_and_log_err(cx);
    }

    /// Allocates the next backup sequence number. One owner for the
    /// monotonic-uniqueness contract: two snapshots written in the same
    /// millisecond must never share a filename.
    fn next_backup_seq(&mut self) -> u64 {
        let backup_seq = self.backup_seq;
        self.backup_seq += 1;
        backup_seq
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let worktree_root = self
            .worktree_id
            .and_then(|worktree_id| self.project.read(cx).worktree_for_id(worktree_id, cx))
            .map(|worktree| worktree.read(cx).abs_path());
        let Some(worktree_root) = worktree_root else {
            self.bound_project_key = None;
            if self.dirty {
                // Unsaved local mutations win over whatever is stored right
                // now; the pending save will persist them shortly.
                return;
            }
            if self.state != InboxFile::default() {
                self.state = InboxFile::default();
                self.load_error = None;
                self.save_error = None;
                self.restorable = None;
                cx.emit(InboxStoreEvent::Reloaded);
            }
            return;
        };
        let key = project_key(&worktree_root);
        let backup_dir = backup_dir_for_key(&key);
        self.bound_project_key = Some(key.clone());
        let fs = self.fs.clone();
        let key_value_store = self.key_value_store.clone();
        let legacy_path = worktree_root.join(".zed").join("inbox.json");
        cx.spawn(async move |this, cx| {
            // Read and parse off the UI thread: a large document would
            // otherwise stall the foreground executor.
            let quarantine_dir = backup_dir.clone();
            let fs_for_load = fs.clone();
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    load_outcome(
                        &key_value_store,
                        &key,
                        &fs_for_load,
                        &legacy_path,
                        &quarantine_dir,
                    )
                    .await
                })
                .await;
            // Only reach for a backup when the store didn't yield usable
            // data; a healthy reload never touches the backup ring, and a
            // newer-version document must not surface a downgrading restore.
            let backup = match &outcome {
                ReloadOutcome::Loaded(_)
                | ReloadOutcome::LegacyImported(_)
                | ReloadOutcome::NewerVersion => None,
                ReloadOutcome::Missing | ReloadOutcome::Failed(_) => {
                    load_latest_backup(&fs, &backup_dir).await
                }
            };
            this.update(cx, |this, cx| this.finish_reload(outcome, backup, cx))
                .ok();
        })
        .detach();
    }

    fn finish_reload(
        &mut self,
        outcome: ReloadOutcome,
        backup: Option<InboxFile>,
        cx: &mut Context<Self>,
    ) {
        if self.dirty {
            // Unsaved local mutations win over whatever is stored right now;
            // the pending save will persist them shortly.
            return;
        }
        match outcome {
            ReloadOutcome::Loaded(state) => {
                self.adopt_loaded_state(*state, cx);
            }
            ReloadOutcome::LegacyImported(state) => {
                self.adopt_loaded_state(*state, cx);
                // Persist the imported legacy file through the one normal
                // mutation path; the file itself is left untouched and never
                // written again.
                self.on_mutated(cx);
            }
            ReloadOutcome::NewerVersion => {
                self.load_error =
                    Some("inbox data was written by a newer version of Zed".to_string());
                // Never offer a restore here: re-saving an older snapshot
                // (or the parsed newer document itself) would downgrade and
                // truncate data written by the newer build. The raw value is
                // preserved in the quarantine file.
                self.restorable = None;
                cx.emit(InboxStoreEvent::Changed);
            }
            ReloadOutcome::Missing => {
                self.load_error = None;
                match backup {
                    Some(offer) => {
                        // Don't auto-adopt the backup: offer a restore banner
                        // so a deliberate fresh start isn't silently undone.
                        self.restorable = Some(offer);
                        cx.emit(InboxStoreEvent::Changed);
                    }
                    None => {
                        self.restorable = None;
                        if self.state != InboxFile::default() {
                            self.state = InboxFile::default();
                            cx.emit(InboxStoreEvent::Reloaded);
                        }
                    }
                }
            }
            ReloadOutcome::Failed(error) => {
                // Keep the in-memory state, surface the error, and offer the
                // newest backup holding data, if any.
                self.load_error = Some(error);
                self.restorable = backup;
                cx.emit(InboxStoreEvent::Changed);
            }
        }
    }

    /// Installs a freshly loaded document as the live state, backfilling
    /// missing `created` timestamps.
    fn adopt_loaded_state(&mut self, mut state: InboxFile, cx: &mut Context<Self>) {
        backfill_created(&mut state);
        self.state = state;
        self.load_error = None;
        self.restorable = None;
        cx.emit(InboxStoreEvent::Reloaded);
    }

    fn on_mutated(&mut self, cx: &mut Context<Self>) {
        self.dirty = true;
        self.load_error = None;
        // A pending recovery offer is always backup-sourced: its data exists
        // nowhere else in memory, so it survives edits until the user decides.
        cx.emit(InboxStoreEvent::Changed);
        self.schedule_save(cx);
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.pending_save = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            let Ok(Some((key_value_store, fs, key, file, backup_seq))) =
                this.update(cx, |this, _| {
                    if !this.dirty {
                        return None;
                    }
                    let key = this.bound_project_key.clone()?;
                    // Optimistically clear `dirty`: a mutation landing while
                    // the write is in flight re-marks it and replaces this
                    // task with a fresh save.
                    this.dirty = false;
                    Some((
                        this.key_value_store.clone(),
                        this.fs.clone(),
                        key,
                        this.state.clone(),
                        this.next_backup_seq(),
                    ))
                })
            else {
                return;
            };

            let write_result = cx
                .background_spawn(persist_snapshot(key_value_store, fs, key, file, backup_seq))
                .await;

            this.update(cx, |this, cx| match write_result {
                Ok(()) => this.save_error = None,
                Err(error) => {
                    // The write failed: restore `dirty` so the mutation is
                    // retried on the next edit instead of being silently
                    // lost.
                    this.dirty = true;
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

    pub fn types(&self) -> &[CatalogEntry] {
        self.catalog(CatalogKind::List)
    }

    pub fn tags(&self) -> &[CatalogEntry] {
        self.catalog(CatalogKind::Tag)
    }

    /// The entries of one catalog, read-side twin of [`Self::catalog_mut`].
    pub fn catalog(&self, kind: CatalogKind) -> &[CatalogEntry] {
        match kind {
            CatalogKind::List => &self.state.types,
            CatalogKind::Tag => &self.state.tags,
        }
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

    /// Looks up a type by its key.
    pub fn type_by_key(&self, key: &str) -> Option<&CatalogEntry> {
        self.types().iter().find(|inbox_type| inbox_type.key == key)
    }

    /// Resolves the type of an item. Returns `None` when the item has no
    /// kind, or when its kind matches no existing type.
    pub fn resolve_kind(&self, item: &InboxItem) -> Option<&CatalogEntry> {
        self.type_by_key(item.kind.as_deref()?)
    }

    /// Looks up a tag by its key.
    pub fn tag_by_key(&self, key: &str) -> Option<&CatalogEntry> {
        self.tags().iter().find(|tag| tag.key == key)
    }

    /// Resolves the item's tags against the tag catalog, in catalog order
    /// (stable display order regardless of assignment order). Dangling keys
    /// are silently skipped, the same tolerance as [`Self::resolve_kind`].
    pub fn resolve_tags<'a>(
        &'a self,
        item: &'a InboxItem,
    ) -> impl Iterator<Item = &'a CatalogEntry> + 'a {
        self.tags()
            .iter()
            .filter(|entry| item.tags.iter().any(|key| key == &entry.key))
    }

    /// The subset of `keys` that exists in the tag catalog, in catalog
    /// order — the one owner of the "tags persist in catalog order" rule.
    /// `HashSet` iteration order is random; writing it verbatim would
    /// serialize identical selections differently across captures.
    pub fn catalog_ordered_tag_keys(&self, keys: &HashSet<String>) -> Vec<String> {
        self.tags()
            .iter()
            .filter(|tag| keys.contains(&tag.key))
            .map(|tag| tag.key.clone())
            .collect()
    }

    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// Set when the most recent debounced save failed to write to the
    /// database. The mutation remains dirty and will be retried on the next
    /// save attempt.
    pub fn save_error(&self) -> Option<&str> {
        self.save_error.as_deref()
    }

    pub fn has_worktree(&self) -> bool {
        self.worktree_id.is_some()
    }

    /// KV key of the currently bound project, if any. Lets the panel detect
    /// a project switch that happened while a file dialog was open.
    pub fn bound_project_key(&self) -> Option<&str> {
        self.bound_project_key.as_deref()
    }

    /// Absolute root of the bound worktree, if it is still part of the
    /// project.
    pub fn worktree_root(&self, cx: &App) -> Option<Arc<Path>> {
        let worktree = self
            .project
            .read(cx)
            .worktree_for_id(self.worktree_id?, cx)?;
        Some(worktree.read(cx).abs_path())
    }

    /// Whether the stored document went missing or corrupt while a backup
    /// with data is available to restore. Drives the recovery banner.
    pub fn can_restore(&self) -> bool {
        self.restorable.is_some()
    }

    /// Re-persists the recovered snapshot to the key-value store (and a fresh
    /// backup). Edits made while the offer was pending are kept: the snapshot
    /// only fills in what they don't cover.
    pub fn restore_from_backup(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.restorable.take() else {
            return;
        };
        self.adopt_snapshot(snapshot, cx);
    }

    /// A copy of the current document for exporting to a file, with the
    /// version pinned like every persisted write.
    pub fn export_snapshot(&self) -> InboxFile {
        let mut file = self.state.clone();
        file.version = Some(CURRENT_INBOX_VERSION);
        file
    }

    /// Merges a document imported from a file into the current state (same
    /// non-destructive semantics as a backup restore: dedup by item id and
    /// catalog key, current state wins) and persists it. Returns how many
    /// items the import added. Refuses documents written by a newer Zed,
    /// mirroring the load policy — re-saving one would silently downgrade it.
    pub fn import_snapshot(
        &mut self,
        snapshot: InboxFile,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<usize> {
        anyhow::ensure!(
            snapshot
                .version
                .is_none_or(|version| version <= CURRENT_INBOX_VERSION),
            "the file was exported by a newer version of Zed"
        );
        let item_count_before = self.state.inbox.len() + self.state.archived.len();
        self.adopt_snapshot(snapshot, cx);
        Ok(self.state.inbox.len() + self.state.archived.len() - item_count_before)
    }

    /// Adopts `snapshot` into the live state — merged under current data when
    /// any exists, wholesale otherwise — and persists it through the normal
    /// mutation path. Shared by backup restore and file import.
    fn adopt_snapshot(&mut self, mut snapshot: InboxFile, cx: &mut Context<Self>) {
        backfill_created(&mut snapshot);
        if self.state.has_content() {
            merge_missing(&mut self.state, snapshot);
        } else {
            self.state = snapshot;
        }
        self.load_error = None;
        // Marks dirty and schedules the debounced save, which rewrites the
        // stored entry.
        self.on_mutated(cx);
        cx.emit(InboxStoreEvent::Reloaded);
    }

    /// Dismisses the recovery offer, leaving whatever the user has built up
    /// since untouched. Nothing is written until the next real edit.
    pub fn dismiss_restore(&mut self, cx: &mut Context<Self>) {
        if self.restorable.take().is_none() {
            return;
        }
        self.load_error = None;
        cx.emit(InboxStoreEvent::Changed);
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
                tags: Vec::new(),
                from,
                body: None,
                attachments: Vec::new(),
                created: Some(now_unix()),
                cleared: None,
            },
        );
        self.on_mutated(cx);
        id
    }

    /// Applies `f` to the item with the given id, searching both the inbox and
    /// the archive. `f` returns whether it changed the item; when it didn't,
    /// nothing is marked dirty, no event is emitted and no save is scheduled.
    pub fn update_item(
        &mut self,
        id: &ItemId,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut InboxItem) -> bool,
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
        if f(item) {
            self.on_mutated(cx);
        }
    }

    pub fn set_kind(&mut self, id: &ItemId, kind: Option<String>, cx: &mut Context<Self>) {
        self.update_item(id, cx, |item| {
            if item.kind == kind {
                return false;
            }
            item.kind = kind;
            true
        });
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
        if move_before(
            &mut self.state.inbox,
            |item| &item.id == id,
            |item| &item.id == target_id,
        ) {
            self.on_mutated(cx);
        }
    }

    pub fn set_text(&mut self, id: &ItemId, text: String, cx: &mut Context<Self>) {
        self.update_item(id, cx, |item| {
            if item.text == text {
                return false;
            }
            item.text = text;
            true
        });
    }

    pub fn set_body(&mut self, id: &ItemId, body: Option<String>, cx: &mut Context<Self>) {
        self.update_item(id, cx, |item| {
            if item.body == body {
                return false;
            }
            item.body = body;
            true
        });
    }

    /// Replaces the item's attachment list.
    pub fn set_attachments(
        &mut self,
        id: &ItemId,
        attachments: Vec<AttachmentRef>,
        cx: &mut Context<Self>,
    ) {
        self.update_item(id, cx, |item| {
            if item.attachments == attachments {
                return false;
            }
            item.attachments = attachments;
            true
        });
    }

    /// Appends a reference, de-duplicated by full equality.
    pub fn add_attachment(
        &mut self,
        id: &ItemId,
        attachment: AttachmentRef,
        cx: &mut Context<Self>,
    ) {
        self.update_item(id, cx, |item| {
            if item.attachments.contains(&attachment) {
                return false;
            }
            item.attachments.push(attachment);
            true
        });
    }

    /// Removes a reference if present.
    pub fn remove_attachment(
        &mut self,
        id: &ItemId,
        attachment: &AttachmentRef,
        cx: &mut Context<Self>,
    ) {
        self.update_item(id, cx, |item| {
            let len = item.attachments.len();
            item.attachments.retain(|existing| existing != attachment);
            item.attachments.len() != len
        });
    }

    /// Replaces the item's tag keys.
    pub fn set_tags(&mut self, id: &ItemId, tags: Vec<String>, cx: &mut Context<Self>) {
        self.update_item(id, cx, |item| {
            if item.tags == tags {
                return false;
            }
            item.tags = tags;
            true
        });
    }

    /// Adds the tag to the item if absent, removes it if present.
    pub fn toggle_item_tag(&mut self, id: &ItemId, tag_key: &str, cx: &mut Context<Self>) {
        self.update_item(id, cx, |item| {
            match item.tags.iter().position(|key| key == tag_key) {
                Some(index) => {
                    item.tags.remove(index);
                }
                None => item.tags.push(tag_key.to_string()),
            }
            true
        });
    }

    pub fn toggle_cleared(&mut self, id: &ItemId, cx: &mut Context<Self>) {
        self.update_item(id, cx, |item| {
            item.cleared = if item.cleared.is_some() {
                None
            } else {
                Some(now_unix())
            };
            true
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

    // Catalog (list/tag) mutations, parameterized by [`CatalogKind`]. The
    // two catalogs share all mechanics; only deleting differs in how it
    // cascades into items (clearing the single `kind` vs filtering `tags`).

    /// All items, open and archived, for cascade cleanups.
    fn all_items_mut(&mut self) -> impl Iterator<Item = &mut InboxItem> {
        self.state
            .inbox
            .iter_mut()
            .chain(self.state.archived.iter_mut())
    }

    fn catalog_mut(&mut self, kind: CatalogKind) -> &mut Vec<CatalogEntry> {
        match kind {
            CatalogKind::List => &mut self.state.types,
            CatalogKind::Tag => &mut self.state.tags,
        }
    }

    /// Renames the catalog entry `key`.
    pub fn rename_entry(
        &mut self,
        kind: CatalogKind,
        key: &str,
        label: String,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .catalog_mut(kind)
            .iter_mut()
            .find(|entry| entry.key == key)
        else {
            return;
        };
        if entry.label == label {
            return;
        }
        entry.label = label;
        self.on_mutated(cx);
    }

    /// Switches the entry's color to the next token in
    /// [`CATALOG_COLOR_TOKENS`].
    pub fn cycle_entry_color(&mut self, kind: CatalogKind, key: &str, cx: &mut Context<Self>) {
        let Some(entry) = self
            .catalog_mut(kind)
            .iter_mut()
            .find(|entry| entry.key == key)
        else {
            return;
        };
        let next = match CATALOG_COLOR_TOKENS
            .iter()
            .position(|token| *token == entry.color)
        {
            Some(index) => CATALOG_COLOR_TOKENS[(index + 1) % CATALOG_COLOR_TOKENS.len()],
            None => CATALOG_COLOR_TOKENS[0],
        };
        entry.color = next.to_string();
        self.on_mutated(cx);
    }

    /// Deletes a catalog entry. Items referencing it are cleaned up: deleting
    /// a list unassigns its items, deleting a tag strips it from every item.
    /// Any entry can be deleted, including the last one — both catalogs start
    /// empty by default.
    pub fn delete_entry(&mut self, kind: CatalogKind, key: &str, cx: &mut Context<Self>) {
        let entries = self.catalog_mut(kind);
        let Some(index) = entries.iter().position(|entry| entry.key == key) else {
            return;
        };
        entries.remove(index);
        match kind {
            CatalogKind::List => {
                for item in self.all_items_mut() {
                    if item.kind.as_deref() == Some(key) {
                        item.kind = None;
                    }
                }
            }
            CatalogKind::Tag => {
                for item in self.all_items_mut() {
                    item.tags.retain(|tag_key| tag_key != key);
                }
            }
        }
        self.on_mutated(cx);
    }

    /// Adds a new catalog entry with a generated key and the next color in
    /// the palette. Returns the new key.
    pub fn add_entry(&mut self, kind: CatalogKind, cx: &mut Context<Self>) -> String {
        // Distinct "k"/"t" key prefixes purely for readability when
        // hand-inspecting inbox.json; type and tag keys live in disjoint item
        // fields, so the namespaces never actually need to be distinct.
        let (key_prefix, default_label) = match kind {
            CatalogKind::List => ("k", "New list"),
            CatalogKind::Tag => ("t", "New tag"),
        };
        let key = format!("{key_prefix}{}", new_item_id());
        let entries = self.catalog_mut(kind);
        let color = CATALOG_COLOR_TOKENS[entries.len() % CATALOG_COLOR_TOKENS.len()];
        entries.push(CatalogEntry {
            key: key.clone(),
            label: default_label.to_string(),
            color: color.to_string(),
        });
        self.on_mutated(cx);
        key
    }

    /// Reorders the catalog alphabetically by label (case-insensitive).
    pub fn sort_entries_alpha(&mut self, kind: CatalogKind, cx: &mut Context<Self>) {
        let entries = self.catalog_mut(kind);
        if entries
            .iter()
            .map(|entry| entry.label.to_lowercase())
            .is_sorted()
        {
            return;
        }
        entries.sort_by_cached_key(|entry| entry.label.to_lowercase());
        self.on_mutated(cx);
    }

    /// Moves the catalog entry `key` to just before `target_key`. No-op if
    /// either key is missing, they are equal, or the order would not change.
    pub fn move_entry_before(
        &mut self,
        kind: CatalogKind,
        key: &str,
        target_key: &str,
        cx: &mut Context<Self>,
    ) {
        if key == target_key {
            return;
        }
        if move_before(
            self.catalog_mut(kind),
            |entry| entry.key == key,
            |entry| entry.key == target_key,
        ) {
            self.on_mutated(cx);
        }
    }
}

/// Moves the element matching `is_source` to just before the element matching
/// `is_target` (or back to its place when the target is missing). Returns
/// whether the order actually changed. Shared by item and type reordering.
fn move_before<T>(
    items: &mut Vec<T>,
    is_source: impl Fn(&T) -> bool,
    is_target: impl Fn(&T) -> bool,
) -> bool {
    let Some(from) = items.iter().position(is_source) else {
        return false;
    };
    let item = items.remove(from);
    // Removing and re-inserting at the same index restores the original
    // order, so `insert_at == from` is exactly the no-op case.
    let insert_at = items
        .iter()
        .position(is_target)
        .unwrap_or_else(|| from.min(items.len()));
    items.insert(insert_at, item);
    insert_at != from
}

/// Backfills missing `created` timestamps with "now" so age labels and
/// date sorting have something to work with.
fn backfill_created(state: &mut InboxFile) {
    let now = now_unix();
    for item in state.inbox.iter_mut().chain(state.archived.iter_mut()) {
        if item.created.is_none() {
            item.created = Some(now);
        }
    }
}

/// Serializes `file` and writes it to the project's entry, mirroring the
/// content into the backup ring when it carries data. The one owner of the
/// "what a persisted document looks like" invariant (pinned version, compact
/// JSON, content-gated backup) shared by the debounced save and the rebind
/// flush.
async fn persist_snapshot(
    key_value_store: KeyValueStore,
    fs: Arc<dyn Fs>,
    key: String,
    mut file: InboxFile,
    backup_seq: u64,
) -> anyhow::Result<()> {
    file.version = Some(CURRENT_INBOX_VERSION);
    let should_backup = file.has_content();
    let backup_dir = backup_dir_for_key(&key);
    let content = serde_json::to_string(&file)?;
    key_value_store
        .scoped(INBOX_KV_NAMESPACE)
        .write(key, content.clone())
        .await?;
    if should_backup {
        write_backup(&fs, backup_dir, content, now_unix_millis(), backup_seq).await;
    }
    Ok(())
}

/// Reads and parses the project's stored document, falling back to a legacy
/// `.zed/inbox.json` when no entry exists yet. An unusable raw value
/// (corrupt or newer-versioned) is quarantined first, so a later save
/// overwriting the entry can't destroy it.
async fn load_outcome(
    key_value_store: &KeyValueStore,
    key: &str,
    fs: &Arc<dyn Fs>,
    legacy_path: &Path,
    quarantine_dir: &Path,
) -> ReloadOutcome {
    let text = match key_value_store.scoped(INBOX_KV_NAMESPACE).read(key) {
        Err(error) => return ReloadOutcome::Failed(format!("{error:#}")),
        Ok(Some(text)) => text,
        Ok(None) => {
            if !fs.is_file(legacy_path).await {
                return ReloadOutcome::Missing;
            }
            return match fs.load(legacy_path).await {
                Err(error) => ReloadOutcome::Failed(format!("{error:#}")),
                Ok(text) => match serde_json::from_str::<InboxFile>(&text) {
                    Ok(state) => ReloadOutcome::LegacyImported(Box::new(state)),
                    Err(error) => ReloadOutcome::Failed(error.to_string()),
                },
            };
        }
    };
    match serde_json::from_str::<InboxFile>(&text) {
        Ok(state)
            if state
                .version
                .is_none_or(|version| version <= CURRENT_INBOX_VERSION) =>
        {
            ReloadOutcome::Loaded(Box::new(state))
        }
        parsed => {
            write_quarantine(fs, quarantine_dir, text).await;
            match parsed {
                Ok(_) => ReloadOutcome::NewerVersion,
                Err(error) => ReloadOutcome::Failed(error.to_string()),
            }
        }
    }
}

/// Preserves an unusable raw stored value as a single overwrite-in-place
/// file next to the backup ring, so a later save overwriting the entry can't
/// destroy it. Deliberately named without a `.json` extension: the ring's
/// key listing (and thus trimming and the restore lookup) must never pick it
/// up — repeated reloads over a bad entry would otherwise fill the ring with
/// copies of the unusable blob and evict every good snapshot.
async fn write_quarantine(fs: &Arc<dyn Fs>, dir: &Path, text: String) {
    let write = async {
        fs.create_dir(dir).await?;
        fs.atomic_write(dir.join("quarantine"), text).await
    };
    if let Err(error) = write.await {
        log::warn!("inbox: failed to quarantine unreadable stored value: {error:#}");
    }
}

/// Sort key for a backup file: its `<timestamp>-<seq>` stem. `None` for any
/// entry that isn't one of our `.json` snapshots.
fn backup_sort_key(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    name.strip_suffix(".json").map(str::to_owned)
}

/// Sorted (oldest-first) stems of the `.json` snapshots in `dir`. Empty when the
/// directory can't be read (e.g. no backups written yet).
async fn list_backup_keys(fs: &Arc<dyn Fs>, dir: &Path) -> Vec<String> {
    let Ok(mut entries) = fs.read_dir(dir).await else {
        return Vec::new();
    };
    let mut keys = Vec::new();
    while let Some(entry) = entries.next().await {
        if let Ok(path) = entry
            && let Some(key) = backup_sort_key(&path)
        {
            keys.push(key);
        }
    }
    keys.sort();
    keys
}

/// Writes `content` as a new snapshot in `dir`, then trims the ring to the
/// newest [`BACKUP_KEEP`] snapshots. Backup failures are logged, not fatal —
/// they must never break the primary save.
async fn write_backup(fs: &Arc<dyn Fs>, dir: PathBuf, content: String, now_ms: u64, seq: u64) {
    // Fixed-width fields so lexicographic order matches chronological order;
    // the millisecond timestamp dominates across sessions, `seq` disambiguates
    // within one.
    let file_name = format!("{now_ms:013}-{seq:06}.json");
    let write = async {
        fs.create_dir(&dir).await?;
        fs.atomic_write(dir.join(&file_name), content).await
    };
    if let Err(error) = write.await {
        log::warn!("inbox: failed to write backup: {error:#}");
        return;
    }

    let keys = list_backup_keys(fs, &dir).await;
    if keys.len() <= BACKUP_KEEP {
        return;
    }
    let remove_count = keys.len() - BACKUP_KEEP;
    for key in keys.into_iter().take(remove_count) {
        fs.remove_file(&dir.join(format!("{key}.json")), RemoveOptions::default())
            .await
            .log_err();
    }
}

/// Reads the newest backup snapshot in `dir` that parses and holds data, if any.
async fn load_latest_backup(fs: &Arc<dyn Fs>, dir: &Path) -> Option<InboxFile> {
    // Newest first; return the first snapshot that parses with content.
    for key in list_backup_keys(fs, dir).await.into_iter().rev() {
        if let Ok(text) = fs.load(&dir.join(format!("{key}.json"))).await
            && let Ok(state) = serde_json::from_str::<InboxFile>(&text)
            && state.has_content()
        {
            return Some(state);
        }
    }
    None
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

    /// Names of the backup snapshots currently in `dir`, sorted oldest-first.
    async fn backup_keys(fs: &Arc<FakeFs>, dir: &Path) -> Vec<String> {
        let fs: Arc<dyn Fs> = fs.clone();
        list_backup_keys(&fs, dir).await
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            // A fresh in-memory database per test: without it the store would
            // fall back to the process-wide shared test DB and tests would
            // pollute each other through it.
            cx.set_global(db::AppDatabase::test_new());
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    /// The raw stored inbox document for the project at `root`, if any.
    fn stored_for_root(cx: &mut TestAppContext, root: &Path) -> Option<String> {
        let key_value_store = cx.update(|cx| KeyValueStore::global(cx));
        key_value_store
            .scoped(INBOX_KV_NAMESPACE)
            .read(&project_key(root))
            .unwrap()
    }

    /// Seeds the stored inbox document for the project at `root`.
    async fn seed_kv(cx: &mut TestAppContext, root: &Path, value: &str) {
        let key_value_store = cx.update(|cx| KeyValueStore::global(cx));
        key_value_store
            .scoped(INBOX_KV_NAMESPACE)
            .write(project_key(root), value.to_string())
            .await
            .unwrap();
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
    async fn test_load_existing_document(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        seed_kv(
            cx,
            Path::new(path!("/root")),
            r#"{
                "version": 1,
                "inbox": [
                    { "id": "abc", "text": "first", "kind": "task", "created": 100 },
                    { "text": "second" }
                ],
                "archived": [
                    { "id": "old", "text": "done", "cleared": 200 }
                ]
            }"#,
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
    async fn test_capture_persists(cx: &mut TestAppContext) {
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

        let content = stored_for_root(cx, Path::new(path!("/root"))).unwrap();
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
        // The legacy file is never created.
        assert!(!fs.is_file(path!("/root/.zed/inbox.json").as_ref()).await);
    }

    #[gpui::test]
    async fn test_migrates_legacy_file(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let legacy = r#"{ "inbox": [{ "id": "one", "text": "from file" }] }"#;
        fs.insert_tree(path!("/root"), json!({ ".zed": { "inbox.json": legacy } }))
            .await;
        let (_project, store) = build_store(fs.clone(), cx).await;

        store.read_with(cx, |store, _| {
            assert_eq!(store.load_error(), None);
            assert_eq!(store.items().len(), 1);
            assert_eq!(store.items()[0].text, "from file");
        });

        // The import lands in the key-value store through the normal save
        // path...
        flush_saves(cx);
        let stored = stored_for_root(cx, Path::new(path!("/root"))).unwrap();
        let parsed: InboxFile = serde_json::from_str(&stored).unwrap();
        assert_eq!(parsed.version, Some(1));
        assert_eq!(parsed.inbox.len(), 1);
        assert_eq!(parsed.inbox[0].text, "from file");

        // ...while the legacy file keeps its original bytes.
        let on_disk = fs
            .load(path!("/root/.zed/inbox.json").as_ref())
            .await
            .unwrap();
        assert_eq!(on_disk, legacy);
    }

    #[gpui::test]
    async fn test_kv_wins_over_legacy_file(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".zed": {
                    "inbox.json": r#"{ "inbox": [{ "id": "f", "text": "from file" }] }"#
                }
            }),
        )
        .await;
        seed_kv(
            cx,
            Path::new(path!("/root")),
            r#"{ "version": 1, "inbox": [{ "id": "k", "text": "from kv" }] }"#,
        )
        .await;
        let (_project, store) = build_store(fs, cx).await;

        store.read_with(cx, |store, _| {
            assert_eq!(store.load_error(), None);
            assert_eq!(store.items().len(), 1);
            assert_eq!(store.items()[0].text, "from kv");
        });
    }

    #[gpui::test]
    async fn test_legacy_file_edits_after_migration_are_ignored(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".zed": {
                    "inbox.json": r#"{ "inbox": [{ "id": "one", "text": "imported" }] }"#
                }
            }),
        )
        .await;
        let (_project, store) = build_store(fs.clone(), cx).await;
        flush_saves(cx);

        // Nothing watches the legacy file anymore: an external edit to it
        // must not affect the store.
        fs.save(
            path!("/root/.zed/inbox.json").as_ref(),
            &r#"{ "inbox": [{ "id": "two", "text": "changed on disk" }] }"#.into(),
            Default::default(),
        )
        .await
        .unwrap();
        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            assert_eq!(store.items().len(), 1);
            assert_eq!(store.items()[0].text, "imported");
        });
    }

    #[gpui::test]
    async fn test_broken_stored_value_sets_load_error(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        seed_kv(cx, Path::new(path!("/root")), r#"{ "inbox": [ broken"#).await;
        let (_project, store) = build_store(fs.clone(), cx).await;

        store.read_with(cx, |store, _| {
            assert!(store.load_error().is_some());
            assert!(store.items().is_empty());
            // The unparseable raw value doesn't parse as a snapshot, so
            // there is nothing to offer for restore.
            assert!(!store.can_restore());
        });

        // The raw value is preserved in the quarantine file, without
        // consuming backup-ring slots.
        let backup_dir = backup_dir_for_key(&project_key(Path::new(path!("/root"))));
        assert!(
            fs.is_file(&backup_dir.join("quarantine")).await,
            "the unparseable raw value must be quarantined"
        );
        assert!(backup_keys(&fs, &backup_dir).await.is_empty());

        // An explicit user mutation clears the error and overwrites the entry.
        store.update(cx, |store, cx| {
            store.capture("fresh".to_string(), None, None, cx);
        });
        flush_saves(cx);
        store.read_with(cx, |store, _| assert_eq!(store.load_error(), None));
        let stored = stored_for_root(cx, Path::new(path!("/root"))).unwrap();
        let parsed: InboxFile = serde_json::from_str(&stored).unwrap();
        assert_eq!(parsed.inbox.len(), 1);
        assert_eq!(parsed.inbox[0].text, "fresh");
    }

    #[gpui::test]
    async fn test_newer_version_is_not_loaded(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        seed_kv(
            cx,
            Path::new(path!("/root")),
            r#"{ "version": 99, "inbox": [{ "id": "n", "text": "from the future" }] }"#,
        )
        .await;
        let (_project, store) = build_store(fs.clone(), cx).await;

        store.read_with(cx, |store, _| {
            assert!(
                store
                    .load_error()
                    .is_some_and(|error| error.contains("newer version")),
                "a newer-version document must be refused with a specific error"
            );
            assert!(store.items().is_empty());
            assert!(
                !store.can_restore(),
                "no restore may be offered for a newer-version document — \
                 re-saving it (or an older snapshot over it) would downgrade \
                 data written by the newer build"
            );
        });

        // The raw newer-version payload is preserved in the quarantine file,
        // outside the ring, so it survives a later overwriting save.
        let backup_dir = backup_dir_for_key(&project_key(Path::new(path!("/root"))));
        let quarantined = fs.load(&backup_dir.join("quarantine")).await.unwrap();
        assert!(quarantined.contains("from the future"));
        assert!(backup_keys(&fs, &backup_dir).await.is_empty());
    }

    #[gpui::test]
    async fn test_import_snapshot_merges_and_persists(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, store) = build_store(fs, cx).await;

        let local_id = store.update(cx, |store, cx| {
            store.capture("local".to_string(), None, None, cx)
        });
        flush_saves(cx);

        // An export snapshot carries the pinned version.
        let exported = store.read_with(cx, |store, _| store.export_snapshot());
        assert_eq!(exported.version, Some(1));
        assert_eq!(exported.inbox.len(), 1);

        // Import a snapshot sharing one id with the current state: the
        // duplicate is skipped (current wins), the new item is appended.
        let snapshot: InboxFile = serde_json::from_str(&format!(
            r#"{{ "version": 1, "inbox": [
                {{ "id": "{local_id}", "text": "stale copy" }},
                {{ "id": "imported", "text": "from export" }}
            ] }}"#
        ))
        .unwrap();
        let imported =
            store.update(cx, |store, cx| store.import_snapshot(snapshot, cx).unwrap());
        assert_eq!(imported, 1, "only the genuinely new item counts");
        store.read_with(cx, |store, _| {
            assert_eq!(store.items().len(), 2);
            assert_eq!(store.item(&local_id).unwrap().text, "local");
            assert!(store.items().iter().any(|item| item.text == "from export"));
        });

        // The merge is persisted like any other mutation.
        flush_saves(cx);
        let stored = stored_for_root(cx, Path::new(path!("/root"))).unwrap();
        assert!(stored.contains("from export"));
    }

    #[gpui::test]
    async fn test_import_snapshot_refuses_newer_version(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, store) = build_store(fs, cx).await;

        let snapshot: InboxFile = serde_json::from_str(
            r#"{ "version": 99, "inbox": [{ "id": "n", "text": "future" }] }"#,
        )
        .unwrap();
        let result = store.update(cx, |store, cx| store.import_snapshot(snapshot, cx));
        assert!(
            result.is_err(),
            "importing a newer-version snapshot would downgrade it on the next save"
        );
        store.read_with(cx, |store, _| assert!(store.items().is_empty()));
    }

    #[gpui::test]
    async fn test_restore_and_delete(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        seed_kv(
            cx,
            Path::new(path!("/root")),
            r#"{ "archived": [{ "id": "b", "text": "b", "cleared": 1 }] }"#,
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
        let content = stored_for_root(cx, Path::new(path!("/root"))).unwrap();
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
        let value: serde_json::Value =
            serde_json::from_str(&stored_for_root(cx, Path::new(path!("/root"))).unwrap()).unwrap();
        assert!(
            value.get("types").is_none(),
            "no types by default must not be written"
        );

        let key = store.update(cx, |store, cx| {
            let key = store.add_entry(CatalogKind::List, cx);
            store.rename_entry(CatalogKind::List, &key, "TODO".to_string(), cx);
            key
        });
        flush_saves(cx);
        let parsed: InboxFile =
            serde_json::from_str(&stored_for_root(cx, Path::new(path!("/root"))).unwrap()).unwrap();
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
            let key_banana = store.add_entry(CatalogKind::List, cx);
            store.rename_entry(CatalogKind::List, &key_banana, "Banana".to_string(), cx);
            let key_apple = store.add_entry(CatalogKind::List, cx);
            store.rename_entry(CatalogKind::List, &key_apple, "apple".to_string(), cx);
            (key_banana, key_apple)
        });
        store.update(cx, |store, cx| {
            store.sort_entries_alpha(CatalogKind::List, cx);
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
            let one = store.add_entry(CatalogKind::List, cx);
            store.rename_entry(CatalogKind::List, &one, "one".to_string(), cx);
            let two = store.add_entry(CatalogKind::List, cx);
            store.rename_entry(CatalogKind::List, &two, "two".to_string(), cx);
            let three = store.add_entry(CatalogKind::List, cx);
            store.rename_entry(CatalogKind::List, &three, "three".to_string(), cx);
            (one, two, three)
        });
        store.update(cx, |store, cx| {
            store.move_entry_before(CatalogKind::List, &key_three, &key_one, cx)
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
            let key_a = store.add_entry(CatalogKind::List, cx);
            assert_eq!(store.types()[0].label, "New list");
            assert_eq!(store.types()[0].color, "accent");

            let key_b = store.add_entry(CatalogKind::List, cx);
            assert_eq!(store.types()[1].color, "created");
            (key_a, key_b)
        });

        let item_id = store.update(cx, |store, cx| {
            store.capture("idea item".to_string(), Some(key_b.clone()), None, cx)
        });

        store.update(cx, |store, cx| {
            // Cycling moves the color to the next token.
            store.cycle_entry_color(CatalogKind::List, &key_a, cx);
            assert_eq!(store.types()[0].color, "created");

            // Deleting a type unassigns its items (kind cleared to None).
            store.delete_entry(CatalogKind::List, &key_b, cx);
            assert!(store.types().iter().all(|t| t.key != key_b));
        });
        store.read_with(cx, |store, _| {
            assert_eq!(store.item(&item_id).unwrap().kind, None);
        });

        store.update(cx, |store, cx| {
            // The last remaining type can be deleted, leaving no lists.
            store.delete_entry(CatalogKind::List, &key_a, cx);
            assert_eq!(store.types().len(), 0);

            // Adding a type appends a fresh one.
            let key = store.add_entry(CatalogKind::List, cx);
            assert_eq!(store.types().len(), 1);
            assert_eq!(store.types()[0].key, key);
            assert_eq!(store.types()[0].label, "New list");
        });
    }

    #[gpui::test]
    async fn test_tag_mutations(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, store) = build_store(fs.clone(), cx).await;

        // No tags exist by default.
        store.read_with(cx, |store, _| assert!(store.tags().is_empty()));

        let (key_a, key_b) = store.update(cx, |store, cx| {
            let key_a = store.add_entry(CatalogKind::Tag, cx);
            assert_eq!(store.tags()[0].label, "New tag");
            assert_eq!(store.tags()[0].color, "accent");
            let key_b = store.add_entry(CatalogKind::Tag, cx);
            assert_eq!(store.tags()[1].color, "created");
            (key_a, key_b)
        });

        // Toggling adds; resolve_tags yields catalog order.
        let item_id = store.update(cx, |store, cx| {
            let id = store.capture("tagged item".to_string(), None, None, cx);
            store.toggle_item_tag(&id, &key_b, cx);
            store.toggle_item_tag(&id, &key_a, cx);
            id
        });
        store.read_with(cx, |store, _| {
            let item = store.item(&item_id).unwrap();
            assert_eq!(item.tags, vec![key_b.clone(), key_a.clone()]);
            let resolved: Vec<_> = store
                .resolve_tags(item)
                .map(|tag| tag.key.clone())
                .collect();
            assert_eq!(
                resolved,
                vec![key_a.clone(), key_b.clone()],
                "tags must resolve in catalog order, not assignment order"
            );
        });

        store.update(cx, |store, cx| {
            store.rename_entry(CatalogKind::Tag, &key_a, "Urgent".to_string(), cx);
            assert_eq!(store.tag_by_key(&key_a).unwrap().label, "Urgent");

            store.cycle_entry_color(CatalogKind::Tag, &key_a, cx);
            assert_eq!(store.tag_by_key(&key_a).unwrap().color, "created");

            // Toggling an assigned tag off removes it from the item.
            store.toggle_item_tag(&item_id, &key_b, cx);
        });
        store.read_with(cx, |store, _| {
            assert_eq!(store.item(&item_id).unwrap().tags, vec![key_a.clone()]);
        });

        // Deleting a tag strips it from every item.
        store.update(cx, |store, cx| {
            store.delete_entry(CatalogKind::Tag, &key_a, cx)
        });
        store.read_with(cx, |store, _| {
            assert!(store.tag_by_key(&key_a).is_none());
            assert!(store.item(&item_id).unwrap().tags.is_empty());
        });
    }

    #[gpui::test]
    async fn test_tag_reorder_sort_and_dangling_keys(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, store) = build_store(fs.clone(), cx).await;

        let (key_one, _key_two, key_three) = store.update(cx, |store, cx| {
            let one = store.add_entry(CatalogKind::Tag, cx);
            store.rename_entry(CatalogKind::Tag, &one, "one".to_string(), cx);
            let two = store.add_entry(CatalogKind::Tag, cx);
            store.rename_entry(CatalogKind::Tag, &two, "two".to_string(), cx);
            let three = store.add_entry(CatalogKind::Tag, cx);
            store.rename_entry(CatalogKind::Tag, &three, "three".to_string(), cx);
            (one, two, three)
        });

        // Move three before one -> three, one, two.
        store.update(cx, |store, cx| {
            store.move_entry_before(CatalogKind::Tag, &key_three, &key_one, cx)
        });
        store.read_with(cx, |store, _| {
            let labels: Vec<_> = store.tags().iter().map(|tag| tag.label.clone()).collect();
            assert_eq!(labels, ["three", "one", "two"]);
        });

        // Sort alphabetically by label (case-insensitive).
        store.update(cx, |store, cx| {
            store.sort_entries_alpha(CatalogKind::Tag, cx)
        });
        store.read_with(cx, |store, _| {
            let labels: Vec<_> = store.tags().iter().map(|tag| tag.label.clone()).collect();
            assert_eq!(labels, ["one", "three", "two"]);
        });

        // set_tags replaces wholesale; unknown keys are kept in the item but
        // silently skipped by resolve_tags (same tolerance as resolve_kind).
        let item_id = store.update(cx, |store, cx| {
            let id = store.capture("x".to_string(), None, None, cx);
            store.set_tags(&id, vec!["missing".to_string(), key_one.clone()], cx);
            id
        });
        store.read_with(cx, |store, _| {
            let item = store.item(&item_id).unwrap();
            assert_eq!(item.tags.len(), 2);
            let resolved: Vec<_> = store
                .resolve_tags(item)
                .map(|tag| tag.key.clone())
                .collect();
            assert_eq!(resolved, vec![key_one.clone()]);
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
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, store) = build_store(fs.clone(), cx).await;
        let events = track_events(&store, cx);

        // Swap in a connection whose database has no tables at all, so the
        // debounced save's write genuinely fails ("no such table") without
        // needing any error-injection hook.
        let broken = KeyValueStore::from_app_db(&db::AppDatabase(
            db::open_test_db::<()>("inbox-broken-kv").await,
        ));
        store.update(cx, |store, _| store.key_value_store = broken);

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
            assert_eq!(store.items().len(), 1);
            assert_eq!(store.items()[0].text, "keep me");
        });
        assert!(events.borrow().contains(&InboxStoreEvent::Changed));

        // A later reload must not clobber the unsaved mutation.
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

    #[gpui::test]
    async fn test_backup_written_after_save(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, store) = build_store(fs.clone(), cx).await;

        store.update(cx, |store, cx| {
            store.capture("precious".to_string(), None, None, cx);
        });
        flush_saves(cx);

        // A backup snapshot lands in the out-of-repo ring after the save.
        let backup_dir = backup_dir_for_key(&project_key(Path::new(path!("/root"))));
        assert_eq!(
            backup_keys(&fs, &backup_dir).await.len(),
            1,
            "one save must produce one backup snapshot"
        );
    }

    #[gpui::test]
    async fn test_backup_recovers_on_fresh_start(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (project, store) = build_store(fs.clone(), cx).await;

        // First session writes data (and thus a backup), then the stored
        // entry is lost between sessions (e.g. the local database is wiped).
        store.update(cx, |store, cx| {
            store.capture("from last time".to_string(), None, None, cx);
        });
        flush_saves(cx);
        drop(store);
        let key_value_store = cx.update(|cx| KeyValueStore::global(cx));
        key_value_store
            .scoped(INBOX_KV_NAMESPACE)
            .delete(project_key(Path::new(path!("/root"))))
            .await
            .unwrap();

        // A brand-new store (empty memory) over the same project finds no
        // stored entry but recovers the data from the backup ring.
        let store2 = cx.new(|cx| InboxStore::new(project, fs.clone(), cx));
        cx.run_until_parked();
        store2.read_with(cx, |store, _| {
            assert!(
                store.can_restore(),
                "a fresh start with a lost entry must offer to restore from backup"
            );
        });
        store2.update(cx, |store, cx| store.restore_from_backup(cx));
        flush_saves(cx);
        let content = stored_for_root(cx, Path::new(path!("/root"))).unwrap();
        let parsed: InboxFile = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.inbox.len(), 1);
        assert_eq!(parsed.inbox[0].text, "from last time");
    }

    #[gpui::test]
    async fn test_backup_ring_is_bounded(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, store) = build_store(fs.clone(), cx).await;

        // Each save adds one snapshot; the ring keeps only the newest N.
        for i in 0..(BACKUP_KEEP + 3) {
            store.update(cx, |store, cx| {
                store.capture(format!("item {i}"), None, None, cx);
            });
            flush_saves(cx);
        }

        let backup_dir = backup_dir_for_key(&project_key(Path::new(path!("/root"))));
        assert_eq!(
            backup_keys(&fs, &backup_dir).await.len(),
            BACKUP_KEEP,
            "the backup ring must not grow past BACKUP_KEEP"
        );
    }

    #[gpui::test]
    async fn test_empty_state_is_not_backed_up(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, store) = build_store(fs.clone(), cx).await;

        // A save that carries no user data (only a settings toggle) must not
        // overwrite the ring with an empty snapshot.
        store.update(cx, |store, cx| store.toggle_field("age", cx));
        flush_saves(cx);

        let backup_dir = backup_dir_for_key(&project_key(Path::new(path!("/root"))));
        assert!(
            backup_keys(&fs, &backup_dir).await.is_empty(),
            "an empty state must not produce a backup"
        );
    }

    #[gpui::test]
    async fn test_backup_sourced_offer_survives_edits_and_merges_on_restore(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (project, store) = build_store(fs.clone(), cx).await;

        // First session writes data (and thus a backup), then the stored
        // entry is lost between sessions.
        store.update(cx, |store, cx| {
            store.capture("from last time".to_string(), None, None, cx);
        });
        flush_saves(cx);
        drop(store);
        let key_value_store = cx.update(|cx| KeyValueStore::global(cx));
        key_value_store
            .scoped(INBOX_KV_NAMESPACE)
            .delete(project_key(Path::new(path!("/root"))))
            .await
            .unwrap();

        // A fresh store (empty memory) offers the backup. The user ignores
        // the banner and captures a new item; the offer must survive that
        // edit — the backup's data exists nowhere else.
        let store2 = cx.new(|cx| InboxStore::new(project, fs.clone(), cx));
        cx.run_until_parked();
        store2.read_with(cx, |store, _| assert!(store.can_restore()));
        store2.update(cx, |store, cx| {
            store.capture("typed before deciding".to_string(), None, None, cx);
        });
        flush_saves(cx);
        store2.read_with(cx, |store, _| {
            assert!(
                store.can_restore(),
                "an unrelated edit must not retire a backup-sourced offer"
            );
        });

        // Restore merges the snapshot under the newer edits instead of
        // overwriting them.
        store2.update(cx, |store, cx| store.restore_from_backup(cx));
        flush_saves(cx);
        store2.read_with(cx, |store, _| {
            assert!(!store.can_restore());
            let texts: Vec<_> = store
                .items()
                .iter()
                .map(|item| item.text.as_str())
                .collect();
            assert!(texts.contains(&"typed before deciding"));
            assert!(texts.contains(&"from last time"));
        });
    }

    #[gpui::test]
    async fn test_dismissing_backup_sourced_offer_keeps_current_state(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (project, store) = build_store(fs.clone(), cx).await;

        store.update(cx, |store, cx| {
            store.capture("from last time".to_string(), None, None, cx);
        });
        flush_saves(cx);
        drop(store);
        let key_value_store = cx.update(|cx| KeyValueStore::global(cx));
        key_value_store
            .scoped(INBOX_KV_NAMESPACE)
            .delete(project_key(Path::new(path!("/root"))))
            .await
            .unwrap();

        let store2 = cx.new(|cx| InboxStore::new(project, fs.clone(), cx));
        cx.run_until_parked();
        store2.read_with(cx, |store, _| assert!(store.can_restore()));
        store2.update(cx, |store, cx| {
            store.capture("typed before deciding".to_string(), None, None, cx);
        });

        // Declining a backup-sourced offer only drops the offer; it must not
        // wipe what the user built up since the banner appeared.
        store2.update(cx, |store, cx| store.dismiss_restore(cx));
        flush_saves(cx);
        store2.read_with(cx, |store, _| {
            assert!(!store.can_restore());
            assert_eq!(store.items().len(), 1);
            assert_eq!(store.items()[0].text, "typed before deciding");
        });
    }

    #[gpui::test]
    async fn test_rebind_worktree_flushes_unsaved_edits(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        fs.insert_tree(path!("/other"), json!({})).await;
        let (project, store) = build_store(fs.clone(), cx).await;
        project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(path!("/other"), true, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        // Capture an edit and, while its debounced save is still pending,
        // remove the tracked worktree so the store rebinds to the other one.
        store.update(cx, |store, cx| {
            store.capture("typed right before the switch".to_string(), None, None, cx);
        });
        let root_id = project.read_with(cx, |project, cx| {
            project.visible_worktrees(cx).next().unwrap().read(cx).id()
        });
        project.update(cx, |project, cx| project.remove_worktree(root_id, cx));
        cx.run_until_parked();

        let stored = stored_for_root(cx, Path::new(path!("/root"))).unwrap();
        assert!(
            stored.contains("typed right before the switch"),
            "an edit pending in the save debounce must be flushed to the \
             outgoing project's entry, not silently dropped"
        );
        store.read_with(cx, |store, _| {
            assert_eq!(store.items().len(), 0, "the new worktree starts empty");
            assert!(!store.can_restore());
        });
    }

    #[gpui::test]
    async fn test_rebind_worktree_does_not_leak_state_into_new_worktree(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        fs.insert_tree(path!("/other"), json!({})).await;
        seed_kv(
            cx,
            Path::new(path!("/root")),
            r#"{ "inbox": [ { "id": "a", "text": "root item" } ] }"#,
        )
        .await;
        let (project, store) = build_store(fs.clone(), cx).await;
        store.read_with(cx, |store, _| assert_eq!(store.items().len(), 1));
        project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(path!("/other"), true, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        let root_id = project.read_with(cx, |project, cx| {
            project.visible_worktrees(cx).next().unwrap().read(cx).id()
        });
        project.update(cx, |project, cx| project.remove_worktree(root_id, cx));
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            assert_eq!(
                store.items().len(),
                0,
                "the previous project's items must not leak into the new worktree"
            );
            assert!(
                !store.can_restore(),
                "a missing entry for the new worktree must not offer the \
                 old project's data for restore"
            );
        });
        flush_saves(cx);
        assert!(
            stored_for_root(cx, Path::new(path!("/other"))).is_none(),
            "nothing may be written for the new worktree without an edit there"
        );
    }

    #[gpui::test]
    async fn test_registry_tracks_live_stores(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project_a, store_a) = build_store(fs.clone(), cx).await;
        let (_project_b, store_b) = build_store(fs.clone(), cx).await;

        cx.update(|cx| {
            assert_eq!(
                InboxStoreRegistry::live_stores(cx),
                vec![store_a.clone(), store_b.clone()]
            );
        });

        // Dropping a store's last strong handle removes it from the registry
        // on the next access — closing a window must not leave a dead entry.
        drop(store_b);
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(InboxStoreRegistry::live_stores(cx), vec![store_a.clone()]);
        });

        store_a.read_with(cx, |store, cx| {
            assert_eq!(
                store.worktree_root(cx).as_deref(),
                Some(Path::new(path!("/root")))
            );
        });
    }
}
