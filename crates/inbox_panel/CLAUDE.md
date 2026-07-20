# inbox_panel

GTD-style capture panel. Per-project data lives in Zed's SQLite key-value store (scoped namespace `inbox_panel`; schema: `InboxFile` in `inbox_model.rs`); an out-of-repo backup ring keeps the last 10 saves. The key (`compute_keys` in `inbox_store.rs`) hashes the repo's common git dir when the first visible worktree is inside a git repository — so the main checkout, its linked `git worktree`s and any subdirectory opened as a project root share one entry — and falls back to hashing the worktree root path otherwise. Data saved by pre-repo-identity builds under the path key is auto-migrated on first load; the old entry stays in place as a backup. Legacy `.zed/inbox.json` files are imported once on bind and never written again.

## Commands

```bash
cargo test -p inbox_panel      # full crate test suite (gpui::test + FakeFs)
./script/clippy -p inbox_panel # lint (never plain `cargo clippy`, per repo .rules)
```

## Layout

- `inbox_panel.rs` — **crate root** (`[lib] path`), the panel view + shared UI helpers.
- `inbox_store.rs` — persistence (KV store), reload/recovery, mutations; single owner of `InboxFile` state.
- `inbox_model.rs` — serde types (`InboxFile`, `InboxItem`, `CatalogEntry`), pure helpers (`item_to_markdown`, `format_age`).
- `detail_view.rs` + `block.rs` + `markdown_codec.rs` + `slash_menu.rs` — block-based item editor.
- `type_editor.rs` — Lists & Tags catalog overlay; `attachment.rs` — file attachments.

## Gotchas

- **The MCP surface is a public contract.** Any change to the inbox data model (`inbox_model.rs`), store operations (`inbox_store.rs`) or their semantics MUST be mirrored in the MCP tools in `crates/inbox_mcp` (tool schemas, validation, docs) **in the same PR** — agents talk to the inbox through them. See `crates/inbox_mcp/CLAUDE.md`.
- The lib root is `src/inbox_panel.rs`, so its items are at `crate::…`, **not** `crate::inbox_panel::…` — `use crate::{catalog_swatch, item_markdown}` etc.
- Saves are debounced (`SAVE_DEBOUNCE`, 250ms). Tests must call `flush_saves` + `run_until_parked` before asserting on stored state; a mutation is not persisted synchronously.
- The `dirty` flag stops an async reload from clobbering unsaved edits, and a failed KV write restores it so the mutation is retried on the next edit — any new write/reload path must keep it updated or edits get silently reverted.
- Recovery offers (`restorable: Option<InboxFile>`) are always backup-sourced: their data exists nowhere else in memory, so they survive edits until the user decides. Restoring into non-empty state goes through `merge_missing` (dedup by item id / catalog key, current state wins) — never a blind replace.
- Unparseable and newer-versioned (`version > CURRENT_INBOX_VERSION`) raw KV values are preserved in a single overwrite-in-place `quarantine` file next to the ring before being reported. It deliberately has no `.json` extension: the ring listing (trim + restore lookup) must never see it, or repeated reloads over a bad entry would fill the ring and evict good snapshots. A newer-version document additionally never offers a restore — that would downgrade it to `version: 1`.
- When a `WorktreeRemoved` event arrives, the worktree is already gone from the project, so a live lookup can't resolve it. `rebind_worktree` flushes unsaved edits to the outgoing project's entry through the cached `bound_project_key` — don't "simplify" that into a live lookup.
- `rebind_worktree` compares both the worktree id **and** the primary key: a `WorktreeUpdatedRootRepoCommonDir` event (repo discovered late, `git init`, `.git` deleted) changes the key under an unchanged worktree. On a dirty rebind the reload is chained **after** the flush task completes — the migration probe reads the very entry the flush writes, and the KV store gives no ordering between a queued write and a read from another thread. Auto-migration never fires when the repo key already holds an entry — a main checkout whose path-keyed data was shadowed that way is recoverable via the untouched old entry or Export/Import.
- `refresh` (window-activation / `Panel::set_active` hook, gated on panel visibility) deliberately skips while `dirty` **or** `pending_writes > 0` — reading concurrently with our own in-flight write would revert the UI to the pre-edit document; last writer wins. Stale in-flight reloads are dropped via `load_generation` in `finish_reload`.
- The unchanged-document fast path compares **raw stored text** (`last_seen_text`), not parsed states: parsing regenerates missing item ids and `backfill_created` re-stamps timestamps, so two parses of the same bytes need not be equal. `last_seen_text` is updated on load and on save completion, and reset on rebind/Missing. When the fast path hits while an error/restore banner is showing, it must still emit `Changed` or the banner stays on screen with a dead Restore button.
- `dismiss_restore` sets `restore_dismissed` (cleared on rebind) so a focus-triggered refresh over a Missing entry doesn't resurrect the declined backup offer on every activation.
- Export/Import (the "⋯" header menu) is the manual bridge for moved/renamed projects. `import_snapshot` must stay in sync with the load policy: it refuses `version > CURRENT_INBOX_VERSION` (re-saving would downgrade the file) and merges via the same non-destructive `adopt_snapshot`/`merge_missing` path as backup restore. The panel handlers go through `Workspace::prompt_for_new_path`/`prompt_for_open_path` (not raw `cx.prompt_*`) so remote projects and `use_system_path_prompts` behave like every other dialog.
- Tests must `cx.set_global(db::AppDatabase::test_new())` in `init_test` (a fresh in-memory DB per test); without it the store falls back to the process-wide shared test DB and tests pollute each other.
- `ItemId` is `Arc<str>`, not `String` — collect into `HashSet<ItemId>`, compare with `.clone()`d ids.
- `item_to_markdown` is pure: type/tag labels are resolved by the caller. UI code must go through `item_markdown` (in `inbox_panel.rs`) so copy, send-to-chat and drag all produce identical markdown — don't call `item_to_markdown` directly from views.
- The colored catalog square has one owner: `catalog_swatch`. Chips, menu rows and drag ghosts all use it; don't hand-roll `div().size(px(7.))…`.
- `remote_connection` in dev-dependencies is load-bearing for test builds — see the comment in `Cargo.toml` before removing it.
- Ctrl+V paste of clipboard images/files is intercepted in the **capture phase** (`.capture_action` on the capture-box wrapper and the detail-view root — `handle_attachment_paste` in `attachment.rs`); a leading String clipboard entry falls through to the editor's own paste. Pasted images are saved to `paths::data_dir()/inbox_attachments/<project key>/` and attached as `External`; removing a chip never deletes the file (reference-only doctrine).
