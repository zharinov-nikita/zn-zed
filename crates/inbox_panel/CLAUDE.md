# inbox_panel

GTD-style capture panel. Per-project data lives in `.zed/inbox.json` (schema: `InboxFile` in `inbox_model.rs`); out-of-repo backup ring keeps the last 10 saves.

## Commands

```bash
cargo test -p inbox_panel      # full crate test suite (gpui::test + FakeFs)
./script/clippy -p inbox_panel # lint (never plain `cargo clippy`, per repo .rules)
```

## Layout

- `inbox_panel.rs` — **crate root** (`[lib] path`), the panel view + shared UI helpers.
- `inbox_store.rs` — persistence, reload/recovery, mutations; single owner of `InboxFile` state.
- `inbox_model.rs` — serde types (`InboxFile`, `InboxItem`, `CatalogEntry`), pure helpers (`item_to_markdown`, `format_age`).
- `detail_view.rs` + `block.rs` + `markdown_codec.rs` + `slash_menu.rs` — block-based item editor.
- `type_editor.rs` — Lists & Tags catalog overlay; `attachment.rs` — file attachments.

## Gotchas

- The lib root is `src/inbox_panel.rs`, so its items are at `crate::…`, **not** `crate::inbox_panel::…` — `use crate::{catalog_swatch, item_markdown}` etc.
- Saves are debounced (`SAVE_DEBOUNCE`, 250ms). Tests must call `flush_saves` + `run_until_parked` before asserting on disk state; a mutation is not on disk synchronously.
- `InboxStore.last_saved_content` suppresses the file-watcher echo of our own writes; the `dirty` flag stops a disk reload from clobbering unsaved edits. Any new write/reload path must keep both updated or edits get silently reverted.
- Recovery offers (`restorable: Option<(RestoreSource, InboxFile)>`) have two lifetimes: `Memory`-sourced offers (snapshot taken on corruption/deletion) are retired **synchronously in `on_mutated`** by any mutation; `Backup`-sourced offers survive edits until the user decides. Restoring into non-empty state goes through `merge_missing` (dedup by item id / catalog key, current state wins) — never a blind replace.
- `ItemId` is `Arc<str>`, not `String` — collect into `HashSet<ItemId>`, compare with `.clone()`d ids.
- `item_to_markdown` is pure: type/tag labels are resolved by the caller. UI code must go through `item_markdown` (in `inbox_panel.rs`) so copy, send-to-chat and drag all produce identical markdown — don't call `item_to_markdown` directly from views.
- The colored catalog square has one owner: `catalog_swatch`. Chips, menu rows and drag ghosts all use it; don't hand-roll `div().size(px(7.))…`.
- `remote_connection` in dev-dependencies is load-bearing for test builds — see the comment in `Cargo.toml` before removing it.
