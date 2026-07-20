//! MCP tool definitions over the inbox stores.
//!
//! Every tool follows the `context_server::listener` pattern: the input
//! struct's doc comment becomes the tool description, and its schema is
//! generated with draft07 + inlined subschemas. Handlers run synchronously on
//! the foreground executor, so they may freely read and mutate the per-window
//! [`InboxStore`] entities — the panel re-renders live.

use anyhow::{Result, anyhow};
use collections::HashSet;
use context_server::types::{Tool, ToolAnnotations};
use gpui::App;
use inbox_panel::InboxStore;
use inbox_panel::inbox_model::{InboxItem, ItemId};
use schemars::JsonSchema;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::project_resolve::{ResolvedProject, project_summaries, resolve_store};

/// What a tool handler returns: a human-readable text block plus optional
/// structured content, both forwarded verbatim into the MCP `tools/call`
/// response.
pub(crate) struct ToolOutput {
    pub text: String,
    pub structured: Option<Value>,
}

pub(crate) trait InboxTool: 'static {
    type Input: DeserializeOwned + JsonSchema;

    const NAME: &'static str;

    fn annotations(&self) -> ToolAnnotations;

    fn run(&self, input: Self::Input, cx: &mut App) -> Result<ToolOutput>;
}

pub(crate) struct RegisteredTool {
    pub tool: Tool,
    handler: Box<dyn Fn(Option<Value>, &mut App) -> Result<ToolOutput>>,
}

impl RegisteredTool {
    pub fn call(&self, arguments: Option<Value>, cx: &mut App) -> Result<ToolOutput> {
        (self.handler)(arguments, cx)
    }
}

/// All inbox tools, in the order they are listed to clients.
pub(crate) fn all_tools() -> Vec<RegisteredTool> {
    vec![
        register(ListProjects),
        register(ListItems),
        register(GetItem),
        register(Capture),
        register(UpdateItem),
        register(ToggleItemTag),
        register(ToggleCleared),
        register(RestoreItem),
        register(DeleteItem),
        register(MoveItemBefore),
        register(ListCatalogs),
    ]
}

fn register<T: InboxTool>(tool: T) -> RegisteredTool {
    let mut settings = schemars::generate::SchemaSettings::draft07();
    settings.inline_subschemas = true;
    let mut generator = settings.into_generator();

    let input_schema = generator.root_schema_for::<T::Input>();
    let description = input_schema
        .get("description")
        .and_then(|description| description.as_str())
        .map(|description| description.to_string());
    debug_assert!(
        description.is_some(),
        "input schema struct must include a doc comment for the tool description"
    );

    RegisteredTool {
        tool: Tool {
            name: T::NAME.into(),
            title: None,
            description,
            input_schema: input_schema.into(),
            output_schema: None,
            annotations: Some(tool.annotations()),
        },
        handler: Box::new(move |arguments, cx| {
            // A missing `arguments` means "no arguments": deserialize from an
            // empty object, which fills every `#[serde(default)]` field.
            let input: T::Input =
                serde_json::from_value(arguments.unwrap_or_else(|| json!({})))
                    .map_err(|error| anyhow!("invalid arguments: {error}"))?;
            tool.run(input, cx)
        }),
    }
}

fn annotations(read_only: bool, destructive: bool, idempotent: bool) -> ToolAnnotations {
    ToolAnnotations {
        title: None,
        read_only_hint: Some(read_only),
        destructive_hint: Some(destructive),
        idempotent_hint: Some(idempotent),
        open_world_hint: Some(false),
    }
}

// Shared helpers

/// Requires `kind` to be a key of the list catalog. The panel silently hides
/// dangling keys, so accepting one here would look like data loss to the
/// caller.
fn validate_kind(store: &InboxStore, kind: &str) -> Result<()> {
    if store.type_by_key(kind).is_some() {
        return Ok(());
    }
    Err(anyhow!(
        "unknown list key {kind:?}; valid keys: {}",
        catalog_keys(store.types())
    ))
}

/// Requires every entry of `tags` to be a key of the tag catalog.
fn validate_tags(store: &InboxStore, tags: &[String]) -> Result<()> {
    for tag in tags {
        if store.tag_by_key(tag).is_none() {
            return Err(anyhow!(
                "unknown tag key {tag:?}; valid keys: {}",
                catalog_keys(store.tags())
            ));
        }
    }
    Ok(())
}

fn catalog_keys(entries: &[inbox_panel::inbox_model::CatalogEntry]) -> String {
    if entries.is_empty() {
        return "none (the catalog is empty)".to_string();
    }
    entries
        .iter()
        .map(|entry| format!("{:?} ({})", entry.key, entry.label))
        .collect::<Vec<_>>()
        .join(", ")
}

fn item_id(id: &str) -> ItemId {
    ItemId::from(id)
}

/// Looks up an item and clones it, erroring with the id when it is missing —
/// store mutations silently no-op on unknown ids, which would read as
/// success.
fn require_item(store: &InboxStore, id: &str) -> Result<InboxItem> {
    store
        .item(&item_id(id))
        .cloned()
        .ok_or_else(|| anyhow!("no inbox item with id {id:?}"))
}

/// Structured payload for a single item: the raw document plus resolved
/// list/tag labels, so agents don't need a second call to display it.
fn item_payload(project: &ResolvedProject, item: &InboxItem, cx: &App) -> Value {
    let store = project.store.read(cx);
    json!({
        "project": project.worktree_root,
        "item": item,
        "kind_label": store.resolve_kind(item).map(|entry| entry.label.clone()),
        "tag_labels": store
            .resolve_tags(item)
            .map(|entry| entry.label.clone())
            .collect::<Vec<_>>(),
    })
}

fn item_markdown(project: &ResolvedProject, item: &InboxItem, cx: &App) -> String {
    inbox_panel::item_markdown(&project.store, item, cx)
}

/// One list row per item for `inbox_list_items` text output.
fn item_line(store: &InboxStore, item: &InboxItem) -> String {
    let mut line = format!("- {} — {}", item.id, item.text);
    if let Some(kind) = store.resolve_kind(item) {
        line.push_str(&format!(" [{}]", kind.label));
    }
    let tags = store
        .resolve_tags(item)
        .map(|entry| entry.label.clone())
        .collect::<Vec<_>>();
    if !tags.is_empty() {
        line.push_str(&format!(" #{}", tags.join(" #")));
    }
    if item.is_cleared() {
        line.push_str(" (cleared)");
    }
    line
}

// Tools

struct ListProjects;

/// List the open Zed projects that have an inbox, with open/archived item
/// counts. Use a returned `worktree_root` or `project_key` as the `project`
/// argument of the other inbox tools when more than one project is open.
#[derive(Deserialize, JsonSchema)]
struct ListProjectsInput {}

impl InboxTool for ListProjects {
    type Input = ListProjectsInput;

    const NAME: &'static str = "inbox_list_projects";

    fn annotations(&self) -> ToolAnnotations {
        annotations(true, false, true)
    }

    fn run(&self, _: Self::Input, cx: &mut App) -> Result<ToolOutput> {
        let projects = project_summaries(cx);
        let text = if projects.is_empty() {
            "No project with an inbox is open in Zed.".to_string()
        } else {
            projects
                .iter()
                .map(|project| {
                    format!(
                        "- {} (key {}): {} open, {} archived",
                        project.worktree_root,
                        project.project_key,
                        project.open_count,
                        project.archived_count
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(ToolOutput {
            text,
            structured: Some(json!({ "projects": projects })),
        })
    }
}

struct ListItems;

/// List the inbox items of a project in the panel's display order. Returns
/// open items by default; set `include_archived` to also get processed
/// (archived) items.
#[derive(Deserialize, JsonSchema)]
struct ListItemsInput {
    /// Absolute worktree root path (or a project_key prefix) of the target
    /// project. May be omitted when exactly one project is open.
    #[serde(default)]
    project: Option<String>,
    /// Also return archived (processed) items. Defaults to false.
    #[serde(default)]
    include_archived: bool,
}

impl InboxTool for ListItems {
    type Input = ListItemsInput;

    const NAME: &'static str = "inbox_list_items";

    fn annotations(&self) -> ToolAnnotations {
        annotations(true, false, true)
    }

    fn run(&self, input: Self::Input, cx: &mut App) -> Result<ToolOutput> {
        let project = resolve_store(input.project.as_deref(), cx)?;
        let store = project.store.read(cx);
        // The panel displays open items through the sort mode; mirror that
        // order so "the first item" means the same thing in both views.
        let mut items = store.items().to_vec();
        store.sort_mode().apply(&mut items);
        let archived = input.include_archived.then(|| store.archived().to_vec());

        let mut lines = vec![format!(
            "Inbox of {} — {} open item(s):",
            project.worktree_root,
            items.len()
        )];
        lines.extend(items.iter().map(|item| item_line(store, item)));
        if let Some(archived) = &archived {
            lines.push(format!("Archived — {} item(s):", archived.len()));
            lines.extend(archived.iter().map(|item| item_line(store, item)));
        }

        Ok(ToolOutput {
            text: lines.join("\n"),
            structured: Some(json!({
                "project": project.worktree_root,
                "sort": store.sort_mode(),
                "items": items,
                "archived": archived,
            })),
        })
    }
}

struct GetItem;

/// Get one inbox item as Markdown (identical to the panel's "copy as
/// Markdown") together with its raw fields and resolved list/tag labels.
#[derive(Deserialize, JsonSchema)]
struct GetItemInput {
    /// Absolute worktree root path (or a project_key prefix) of the target
    /// project. May be omitted when exactly one project is open.
    #[serde(default)]
    project: Option<String>,
    /// Id of the item, as returned by inbox_list_items or inbox_capture.
    id: String,
}

impl InboxTool for GetItem {
    type Input = GetItemInput;

    const NAME: &'static str = "inbox_get_item";

    fn annotations(&self) -> ToolAnnotations {
        annotations(true, false, true)
    }

    fn run(&self, input: Self::Input, cx: &mut App) -> Result<ToolOutput> {
        let project = resolve_store(input.project.as_deref(), cx)?;
        let item = require_item(project.store.read(cx), &input.id)?;
        Ok(ToolOutput {
            text: item_markdown(&project, &item, cx),
            structured: Some(item_payload(&project, &item, cx)),
        })
    }
}

struct Capture;

/// Add a new item to the top of a project's inbox and return it. `kind` and
/// `tags` must be existing catalog keys — call inbox_list_catalogs first to
/// discover them (both catalogs may be empty).
#[derive(Deserialize, JsonSchema)]
struct CaptureInput {
    /// Absolute worktree root path (or a project_key prefix) of the target
    /// project. May be omitted when exactly one project is open.
    #[serde(default)]
    project: Option<String>,
    /// The item's title, a single line of plain text.
    text: String,
    /// Key of a list catalog entry. Omit for a plain note.
    #[serde(default)]
    kind: Option<String>,
    /// Capture context shown on the item, e.g. "src/editor.rs:1240"
    /// (unix-style, relative to the worktree).
    #[serde(default)]
    from: Option<String>,
    /// Markdown body of the item.
    #[serde(default)]
    body: Option<String>,
    /// Keys of tag catalog entries to assign.
    #[serde(default)]
    tags: Vec<String>,
}

impl InboxTool for Capture {
    type Input = CaptureInput;

    const NAME: &'static str = "inbox_capture";

    fn annotations(&self) -> ToolAnnotations {
        annotations(false, false, false)
    }

    fn run(&self, input: Self::Input, cx: &mut App) -> Result<ToolOutput> {
        let project = resolve_store(input.project.as_deref(), cx)?;
        {
            let store = project.store.read(cx);
            if let Some(kind) = &input.kind {
                validate_kind(store, kind)?;
            }
            validate_tags(store, &input.tags)?;
        }
        let id = project.store.update(cx, |store, cx| {
            let id = store.capture(input.text, input.kind, input.from, cx);
            if let Some(body) = input.body.filter(|body| !body.trim().is_empty()) {
                store.set_body(&id, Some(body), cx);
            }
            if !input.tags.is_empty() {
                // Catalog order, the same normalization the panel applies.
                let keys: HashSet<String> = input.tags.into_iter().collect();
                let tags = store.catalog_ordered_tag_keys(&keys);
                store.set_tags(&id, tags, cx);
            }
            id
        });
        let item = require_item(project.store.read(cx), &id)?;
        Ok(ToolOutput {
            text: format!("Captured item {id}."),
            structured: Some(item_payload(&project, &item, cx)),
        })
    }
}

struct UpdateItem;

/// Update fields of an existing inbox item. Omitted fields are left
/// unchanged; pass an empty string for `body` or `kind` to clear them.
/// `kind`/`tags` must be existing catalog keys (see inbox_list_catalogs).
#[derive(Deserialize, JsonSchema)]
struct UpdateItemInput {
    /// Absolute worktree root path (or a project_key prefix) of the target
    /// project. May be omitted when exactly one project is open.
    #[serde(default)]
    project: Option<String>,
    /// Id of the item to update.
    id: String,
    /// New title. Omit to keep the current one.
    #[serde(default)]
    text: Option<String>,
    /// New Markdown body. Empty string clears the body.
    #[serde(default)]
    body: Option<String>,
    /// New list key. Empty string turns the item into a plain note.
    #[serde(default)]
    kind: Option<String>,
    /// Replacement tag keys. An empty array removes all tags.
    #[serde(default)]
    tags: Option<Vec<String>>,
}

impl InboxTool for UpdateItem {
    type Input = UpdateItemInput;

    const NAME: &'static str = "inbox_update_item";

    fn annotations(&self) -> ToolAnnotations {
        annotations(false, false, true)
    }

    fn run(&self, input: Self::Input, cx: &mut App) -> Result<ToolOutput> {
        let project = resolve_store(input.project.as_deref(), cx)?;
        {
            let store = project.store.read(cx);
            require_item(store, &input.id)?;
            if let Some(kind) = input.kind.as_deref().filter(|kind| !kind.is_empty()) {
                validate_kind(store, kind)?;
            }
            if let Some(tags) = &input.tags {
                validate_tags(store, tags)?;
            }
        }
        let id = item_id(&input.id);
        project.store.update(cx, |store, cx| {
            if let Some(text) = input.text {
                store.set_text(&id, text, cx);
            }
            if let Some(body) = input.body {
                store.set_body(&id, Some(body).filter(|body| !body.is_empty()), cx);
            }
            if let Some(kind) = input.kind {
                store.set_kind(&id, Some(kind).filter(|kind| !kind.is_empty()), cx);
            }
            if let Some(tags) = input.tags {
                let keys: HashSet<String> = tags.into_iter().collect();
                let tags = store.catalog_ordered_tag_keys(&keys);
                store.set_tags(&id, tags, cx);
            }
        });
        let item = require_item(project.store.read(cx), &input.id)?;
        Ok(ToolOutput {
            text: format!("Updated item {}.", input.id),
            structured: Some(item_payload(&project, &item, cx)),
        })
    }
}

struct ToggleItemTag;

/// Toggle one tag on an inbox item: adds the tag if absent, removes it if
/// present. The tag key must exist in the tag catalog.
#[derive(Deserialize, JsonSchema)]
struct ToggleItemTagInput {
    /// Absolute worktree root path (or a project_key prefix) of the target
    /// project. May be omitted when exactly one project is open.
    #[serde(default)]
    project: Option<String>,
    /// Id of the item.
    id: String,
    /// Key of the tag catalog entry to toggle.
    tag_key: String,
}

impl InboxTool for ToggleItemTag {
    type Input = ToggleItemTagInput;

    const NAME: &'static str = "inbox_toggle_item_tag";

    fn annotations(&self) -> ToolAnnotations {
        annotations(false, false, false)
    }

    fn run(&self, input: Self::Input, cx: &mut App) -> Result<ToolOutput> {
        let project = resolve_store(input.project.as_deref(), cx)?;
        {
            let store = project.store.read(cx);
            require_item(store, &input.id)?;
            validate_tags(store, std::slice::from_ref(&input.tag_key))?;
        }
        let id = item_id(&input.id);
        project.store.update(cx, |store, cx| {
            store.toggle_item_tag(&id, &input.tag_key, cx);
        });
        let item = require_item(project.store.read(cx), &input.id)?;
        let state = if item.tags.iter().any(|tag| tag == &input.tag_key) {
            "added to"
        } else {
            "removed from"
        };
        Ok(ToolOutput {
            text: format!("Tag {:?} {state} item {}.", input.tag_key, input.id),
            structured: Some(item_payload(&project, &item, cx)),
        })
    }
}

struct ToggleCleared;

/// Toggle an item's processed state: marks an open item as processed
/// (cleared), or re-opens a processed one.
#[derive(Deserialize, JsonSchema)]
struct ToggleClearedInput {
    /// Absolute worktree root path (or a project_key prefix) of the target
    /// project. May be omitted when exactly one project is open.
    #[serde(default)]
    project: Option<String>,
    /// Id of the item.
    id: String,
}

impl InboxTool for ToggleCleared {
    type Input = ToggleClearedInput;

    const NAME: &'static str = "inbox_toggle_cleared";

    fn annotations(&self) -> ToolAnnotations {
        annotations(false, false, false)
    }

    fn run(&self, input: Self::Input, cx: &mut App) -> Result<ToolOutput> {
        let project = resolve_store(input.project.as_deref(), cx)?;
        require_item(project.store.read(cx), &input.id)?;
        let id = item_id(&input.id);
        project.store.update(cx, |store, cx| {
            store.toggle_cleared(&id, cx);
        });
        let item = require_item(project.store.read(cx), &input.id)?;
        let state = if item.is_cleared() {
            "processed"
        } else {
            "open"
        };
        Ok(ToolOutput {
            text: format!("Item {} is now {state}.", input.id),
            structured: Some(item_payload(&project, &item, cx)),
        })
    }
}

struct RestoreItem;

/// Move an archived item back to the top of the inbox, un-clearing it. Only
/// items listed under `archived` can be restored.
#[derive(Deserialize, JsonSchema)]
struct RestoreItemInput {
    /// Absolute worktree root path (or a project_key prefix) of the target
    /// project. May be omitted when exactly one project is open.
    #[serde(default)]
    project: Option<String>,
    /// Id of the archived item.
    id: String,
}

impl InboxTool for RestoreItem {
    type Input = RestoreItemInput;

    const NAME: &'static str = "inbox_restore_item";

    fn annotations(&self) -> ToolAnnotations {
        annotations(false, false, true)
    }

    fn run(&self, input: Self::Input, cx: &mut App) -> Result<ToolOutput> {
        let project = resolve_store(input.project.as_deref(), cx)?;
        {
            let store = project.store.read(cx);
            require_item(store, &input.id)?;
            let id = item_id(&input.id);
            if !store.archived().iter().any(|item| item.id == id) {
                return Err(anyhow!(
                    "item {:?} is not archived; only archived items can be restored",
                    input.id
                ));
            }
        }
        let id = item_id(&input.id);
        project.store.update(cx, |store, cx| {
            store.restore(&id, cx);
        });
        let item = require_item(project.store.read(cx), &input.id)?;
        Ok(ToolOutput {
            text: format!("Restored item {} to the inbox.", input.id),
            structured: Some(item_payload(&project, &item, cx)),
        })
    }
}

struct DeleteItem;

/// Permanently delete an inbox item (open or archived). This cannot be
/// undone; prefer inbox_toggle_cleared to mark an item as processed instead.
#[derive(Deserialize, JsonSchema)]
struct DeleteItemInput {
    /// Absolute worktree root path (or a project_key prefix) of the target
    /// project. May be omitted when exactly one project is open.
    #[serde(default)]
    project: Option<String>,
    /// Id of the item to delete.
    id: String,
}

impl InboxTool for DeleteItem {
    type Input = DeleteItemInput;

    const NAME: &'static str = "inbox_delete_item";

    fn annotations(&self) -> ToolAnnotations {
        annotations(false, true, true)
    }

    fn run(&self, input: Self::Input, cx: &mut App) -> Result<ToolOutput> {
        let project = resolve_store(input.project.as_deref(), cx)?;
        require_item(project.store.read(cx), &input.id)?;
        let id = item_id(&input.id);
        project.store.update(cx, |store, cx| {
            store.delete_item(&id, cx);
        });
        Ok(ToolOutput {
            text: format!("Deleted item {}.", input.id),
            structured: Some(json!({ "deleted": input.id })),
        })
    }
}

struct MoveItemBefore;

/// Move an open item to just before another open item in manual order. Only
/// affects the displayed order while the sort mode is "manual".
#[derive(Deserialize, JsonSchema)]
struct MoveItemBeforeInput {
    /// Absolute worktree root path (or a project_key prefix) of the target
    /// project. May be omitted when exactly one project is open.
    #[serde(default)]
    project: Option<String>,
    /// Id of the item to move.
    id: String,
    /// Id of the open item to insert before.
    target_id: String,
}

impl InboxTool for MoveItemBefore {
    type Input = MoveItemBeforeInput;

    const NAME: &'static str = "inbox_move_item_before";

    fn annotations(&self) -> ToolAnnotations {
        annotations(false, false, true)
    }

    fn run(&self, input: Self::Input, cx: &mut App) -> Result<ToolOutput> {
        let project = resolve_store(input.project.as_deref(), cx)?;
        {
            let store = project.store.read(cx);
            for id in [&input.id, &input.target_id] {
                let id = item_id(id);
                if !store.items().iter().any(|item| item.id == id) {
                    return Err(anyhow!("no open inbox item with id {:?}", id.as_ref()));
                }
            }
        }
        let id = item_id(&input.id);
        let target_id = item_id(&input.target_id);
        project.store.update(cx, |store, cx| {
            store.move_item_before(&id, &target_id, cx);
        });
        Ok(ToolOutput {
            text: format!("Moved item {} before {}.", input.id, input.target_id),
            structured: None,
        })
    }
}

struct ListCatalogs;

/// List a project's item catalogs: the custom lists (item kinds) and tags,
/// with their keys, labels and colors. Both catalogs may be empty.
#[derive(Deserialize, JsonSchema)]
struct ListCatalogsInput {
    /// Absolute worktree root path (or a project_key prefix) of the target
    /// project. May be omitted when exactly one project is open.
    #[serde(default)]
    project: Option<String>,
}

impl InboxTool for ListCatalogs {
    type Input = ListCatalogsInput;

    const NAME: &'static str = "inbox_list_catalogs";

    fn annotations(&self) -> ToolAnnotations {
        annotations(true, false, true)
    }

    fn run(&self, input: Self::Input, cx: &mut App) -> Result<ToolOutput> {
        let project = resolve_store(input.project.as_deref(), cx)?;
        let store = project.store.read(cx);
        let types = store.types();
        let tags = store.tags();
        let describe = |entries: &[inbox_panel::inbox_model::CatalogEntry]| {
            if entries.is_empty() {
                "none".to_string()
            } else {
                entries
                    .iter()
                    .map(|entry| format!("{:?} ({})", entry.key, entry.label))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        };
        Ok(ToolOutput {
            text: format!(
                "Lists: {}\nTags: {}",
                describe(types),
                describe(tags)
            ),
            structured: Some(json!({
                "project": project.worktree_root,
                "types": types,
                "tags": tags,
            })),
        })
    }
}
