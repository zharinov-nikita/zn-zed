use std::cmp::Reverse;
use std::sync::Arc;

use gpui::{App, Hsla};
use serde::{Deserialize, Serialize};
use ui::prelude::*;

/// A short random identifier of an inbox item (base36 of a random `u64`).
pub type ItemId = Arc<str>;

/// Generates a new random [`ItemId`].
pub fn new_item_id() -> ItemId {
    use std::hash::{BuildHasher as _, Hasher as _};

    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    if let Ok(duration) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        hasher.write_u128(duration.as_nanos());
    }
    to_base36(hasher.finish()).into()
}

/// Returns the current unix timestamp in seconds (UTC).
pub fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

fn to_base36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

/// How open items are ordered in the list view.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SortMode {
    /// Items stay in the order they are stored in (the user can rearrange
    /// them by hand). This is the default.
    #[default]
    Manual,
    /// Newest captured first.
    Newest,
    /// Oldest captured first.
    Oldest,
    /// Alphabetical A→Z by text.
    Az,
    /// Alphabetical Z→A by text.
    Za,
}

impl SortMode {
    /// All modes, in menu order.
    pub const ALL: [SortMode; 5] = [
        SortMode::Manual,
        SortMode::Newest,
        SortMode::Oldest,
        SortMode::Az,
        SortMode::Za,
    ];

    pub fn is_manual(&self) -> bool {
        matches!(self, SortMode::Manual)
    }

    /// Short label for the sort menu.
    pub fn label(&self) -> &'static str {
        match self {
            SortMode::Manual => "Manual",
            SortMode::Newest => "Newest",
            SortMode::Oldest => "Oldest",
            SortMode::Az => "A–Z",
            SortMode::Za => "Z–A",
        }
    }

    /// Reorders `items` in place. Uses a stable sort, so the existing (manual)
    /// order breaks ties.
    pub fn apply(&self, items: &mut [InboxItem]) {
        match self {
            SortMode::Manual => {}
            SortMode::Newest => items.sort_by_key(|item| Reverse(item.created)),
            SortMode::Oldest => items.sort_by_key(|item| item.created),
            SortMode::Az => items.sort_by_key(|item| item.text.to_lowercase()),
            SortMode::Za => items.sort_by_key(|item| Reverse(item.text.to_lowercase())),
        }
    }
}

/// A metadata field shown on an item row, which the user can hide. The keys
/// are stable strings persisted in `hidden_fields`; adding a new variant makes
/// the field hideable automatically and it stays visible until hidden.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetaField {
    /// The list (type) chip.
    List,
    /// The captured-age label.
    Age,
    /// The subtask counter.
    Subtasks,
    /// The captured-from file context link.
    Context,
    /// The file attachments chip.
    Attachments,
}

impl MetaField {
    /// All fields, in display order.
    pub const ALL: [MetaField; 5] = [
        MetaField::List,
        MetaField::Age,
        MetaField::Subtasks,
        MetaField::Context,
        MetaField::Attachments,
    ];

    /// Stable key persisted in `hidden_fields`.
    pub fn key(&self) -> &'static str {
        match self {
            MetaField::List => "list",
            MetaField::Age => "age",
            MetaField::Subtasks => "subtasks",
            MetaField::Context => "context",
            MetaField::Attachments => "attachments",
        }
    }

    /// Human label for the fields menu.
    pub fn label(&self) -> &'static str {
        match self {
            MetaField::List => "List",
            MetaField::Age => "Time",
            MetaField::Subtasks => "Subtasks",
            MetaField::Context => "Context",
            MetaField::Attachments => "Attachments",
        }
    }
}

/// The on-disk representation of `.zed/inbox.json`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct InboxFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default)]
    pub inbox: Vec<InboxItem>,
    /// Custom item types. Empty by default (no built-in types).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<InboxType>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub archived: Vec<InboxItem>,
    /// How open items are ordered. Omitted from disk when set to `Manual`.
    #[serde(default, skip_serializing_if = "SortMode::is_manual")]
    pub sort: SortMode,
    /// Keys of [`MetaField`]s hidden on item rows. Empty (all visible) by
    /// default; unknown keys are preserved so forward-compat is safe.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_fields: Vec<String>,
}

impl InboxFile {
    /// Whether this state holds user data worth backing up: any open item,
    /// archived item, or custom list. Bare settings (sort, hidden fields) do
    /// not count, so a backup is never overwritten with an effectively empty
    /// snapshot.
    pub fn has_content(&self) -> bool {
        !self.inbox.is_empty() || !self.archived.is_empty() || !self.types.is_empty()
    }
}

/// A reference-only pointer to a file attached to an inbox item. Only the
/// path is stored — never file content — so it is safe to persist verbatim to
/// the git-committed `.zed/inbox.json`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum AttachmentRef {
    /// A file inside the worktree. `path` is unix-style and worktree-relative,
    /// the same convention as [`InboxItem::from`].
    Project { path: String },
    /// A file outside the project. `path` is an absolute, platform-native path.
    External { path: String },
}

impl AttachmentRef {
    /// The stored path, regardless of kind.
    pub fn path(&self) -> &str {
        match self {
            AttachmentRef::Project { path } | AttachmentRef::External { path } => path,
        }
    }

    /// The last path component, for display in a chip. Splits on both `/` and
    /// `\` so external Windows paths render sensibly too.
    pub fn display_name(&self) -> &str {
        let path = self.path();
        path.rsplit(['/', '\\'])
            .next()
            .filter(|component| !component.is_empty())
            .unwrap_or(path)
    }
}

/// A single captured inbox entry.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct InboxItem {
    /// Note: an entry stored without an id gets a *freshly generated* one
    /// every time the file is loaded, so its id is not stable across
    /// reloads until the file is written back (which persists the
    /// generated id).
    #[serde(default = "new_item_id")]
    pub id: ItemId,
    pub text: String,
    /// Key of an [`InboxType`]. `None` means "note".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Capture context, e.g. `"src/editor.rs:1240"` (unix-style, relative to the worktree).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Markdown body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Reference-only file attachments (paths only, never content).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentRef>,
    /// Unix seconds, UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<i64>,
    /// `Some(timestamp)` when the item has been processed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleared: Option<i64>,
}

impl InboxItem {
    pub fn is_cleared(&self) -> bool {
        self.cleared.is_some()
    }
}

/// A user-defined kind of inbox items.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct InboxType {
    pub key: String,
    pub label: String,
    /// Name of a theme color token, see [`TYPE_COLOR_TOKENS`].
    pub color: String,
}

/// Theme color tokens available for inbox types, in cycling order.
pub const TYPE_COLOR_TOKENS: &[&str] = &[
    "accent", "created", "modified", "deleted", "info", "hint", "muted", "conflict",
];

/// Resolves a color token name to a concrete theme color.
/// Unknown tokens fall back to the muted color.
pub fn type_color(token: &str, cx: &App) -> Hsla {
    match token {
        "created" => cx.theme().status().created,
        "modified" => cx.theme().status().modified,
        "deleted" => cx.theme().status().deleted,
        "conflict" => cx.theme().status().conflict,
        "accent" => Color::Accent.color(cx),
        "info" => Color::Info.color(cx),
        "hint" => Color::Hint.color(cx),
        _ => Color::Muted.color(cx),
    }
}

/// Formats the age of an item in compact English notation.
pub fn format_age(created_unix: i64, now_unix: i64) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;

    let seconds = now_unix.saturating_sub(created_unix).max(0);
    if seconds < MINUTE {
        "now".to_string()
    } else if seconds < HOUR {
        format!("{}m", seconds / MINUTE)
    } else if seconds < DAY {
        format!("{}h", seconds / HOUR)
    } else if seconds < WEEK {
        format!("{}d", seconds / DAY)
    } else {
        format!("{}w", seconds / WEEK)
    }
}

/// Counts markdown checkboxes (`- [ ]` / `- [x]`) in a body.
/// Returns `(done, total)`, or `None` if the body contains no checkboxes.
pub fn subtask_counts(body: &str) -> Option<(usize, usize)> {
    let mut done = 0;
    let mut total = 0;
    for line in body.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed
            .strip_prefix("- [")
            .or_else(|| trimmed.strip_prefix("* ["))
        else {
            continue;
        };
        let mut chars = rest.chars();
        let (Some(state), Some(']')) = (chars.next(), chars.next()) else {
            continue;
        };
        match state {
            ' ' => total += 1,
            'x' | 'X' => {
                done += 1;
                total += 1;
            }
            _ => {}
        }
    }
    if total == 0 {
        None
    } else {
        Some((done, total))
    }
}

/// Parses a capture context like `"src/editor.rs:1240"` into a path and an
/// optional 1-based line number.
pub fn parse_context(from: &str) -> Option<(String, Option<u32>)> {
    let from = from.trim();
    if from.is_empty() {
        return None;
    }
    if let Some((path, line)) = from.rsplit_once(':')
        && !path.is_empty()
        && let Ok(line) = line.parse::<u32>()
    {
        return Some((path.to_string(), Some(line)));
    }
    Some((from.to_string(), None))
}

/// Formats a unix-seconds timestamp as an ISO date (`YYYY-MM-DD`, UTC).
/// Returns `None` for a timestamp outside the representable range.
fn format_date(unix_secs: i64) -> Option<String> {
    time::OffsetDateTime::from_unix_timestamp(unix_secs)
        .ok()
        .map(|datetime| datetime.date().to_string())
}

/// Renders an item as a standalone Markdown document: a title heading, a
/// metadata line, the body, and an attachments list. Empty parts are omitted.
/// `type_label` is the resolved list label (the caller looks it up via the
/// store, so this function stays free of theme/store dependencies).
pub fn item_to_markdown(item: &InboxItem, type_label: Option<&str>) -> String {
    let mut out = String::new();

    let title = item.text.trim();
    out.push_str("# ");
    out.push_str(if title.is_empty() { "(untitled)" } else { title });

    // Metadata line: only the fields that are present.
    let mut meta = Vec::new();
    if let Some(label) = type_label {
        meta.push(format!("**Type:** {label}"));
    }
    if let Some(created) = item.created
        && let Some(date) = format_date(created)
    {
        meta.push(format!("**Created:** {date}"));
    }
    if let Some(from) = item
        .from
        .as_deref()
        .map(str::trim)
        .filter(|from| !from.is_empty())
    {
        meta.push(format!("**From:** {from}"));
    }
    if !meta.is_empty() {
        out.push_str("\n\n");
        out.push_str(&meta.join(" · "));
    }

    if let Some(body) = item
        .body
        .as_deref()
        .map(str::trim)
        .filter(|body| !body.is_empty())
    {
        out.push_str("\n\n");
        out.push_str(body);
    }

    if !item.attachments.is_empty() {
        out.push_str("\n\n**Attachments:**\n");
        let attachments = item
            .attachments
            .iter()
            .map(|attachment| format!("- {} — {}", attachment.display_name(), attachment.path()))
            .collect::<Vec<_>>();
        out.push_str(&attachments.join("\n"));
    }

    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_new_item_id() {
        let a = new_item_id();
        let b = new_item_id();
        assert!(!a.is_empty());
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a, b);
    }

    #[test]
    fn test_is_cleared() {
        let mut item = InboxItem {
            id: new_item_id(),
            text: "text".into(),
            kind: None,
            from: None,
            body: None,
            attachments: Vec::new(),
            created: None,
            cleared: None,
        };
        assert!(!item.is_cleared());
        item.cleared = Some(1);
        assert!(item.is_cleared());
    }

    fn sample_item() -> InboxItem {
        InboxItem {
            id: "id".into(),
            text: "Ship the release".into(),
            kind: Some("task".into()),
            from: None,
            body: None,
            attachments: Vec::new(),
            created: None,
            cleared: None,
        }
    }

    #[test]
    fn test_item_to_markdown_title_only() {
        let item = sample_item();
        assert_eq!(item_to_markdown(&item, None), "# Ship the release\n");
    }

    #[test]
    fn test_item_to_markdown_empty_title_falls_back() {
        let mut item = sample_item();
        item.text = "   ".into();
        assert_eq!(item_to_markdown(&item, None), "# (untitled)\n");
    }

    #[test]
    fn test_item_to_markdown_full() {
        let mut item = sample_item();
        item.from = Some("src/editor.rs:1240".into());
        item.body = Some("- [x] done\n- [ ] todo".into());
        item.created = Some(0); // 1970-01-01
        item.attachments = vec![
            AttachmentRef::Project {
                path: "src/main.rs".into(),
            },
            AttachmentRef::External {
                path: "/tmp/note.txt".into(),
            },
        ];
        let expected = "# Ship the release\n\n\
             **Type:** Task · **Created:** 1970-01-01 · **From:** src/editor.rs:1240\n\n\
             - [x] done\n- [ ] todo\n\n\
             **Attachments:**\n\
             - main.rs — src/main.rs\n\
             - note.txt — /tmp/note.txt\n";
        assert_eq!(item_to_markdown(&item, Some("Task")), expected);
    }

    #[test]
    fn test_item_to_markdown_omits_empty_body_and_meta() {
        let mut item = sample_item();
        item.body = Some("   \n\t".into());
        item.from = Some("  ".into());
        // No type label, blank body, blank from → title only.
        assert_eq!(item_to_markdown(&item, None), "# Ship the release\n");
    }

    #[test]
    fn test_format_age() {
        assert_eq!(format_age(100, 100), "now");
        assert_eq!(format_age(100, 159), "now");
        // Future timestamps are clamped.
        assert_eq!(format_age(200, 100), "now");
        assert_eq!(format_age(100, 160), "1m");
        assert_eq!(format_age(0, 3599), "59m");
        assert_eq!(format_age(0, 3600), "1h");
        assert_eq!(format_age(0, 86399), "23h");
        assert_eq!(format_age(0, 86400), "1d");
        assert_eq!(format_age(0, 604799), "6d");
        assert_eq!(format_age(0, 604800), "1w");
        assert_eq!(format_age(0, 3 * 604800), "3w");
    }

    #[test]
    fn test_subtask_counts() {
        assert_eq!(subtask_counts(""), None);
        assert_eq!(subtask_counts("plain text\nwith lines"), None);
        assert_eq!(subtask_counts("- [] malformed\n-[ ] also"), None);
        assert_eq!(subtask_counts("- [ ] one"), Some((0, 1)));
        assert_eq!(subtask_counts("* [X] shouty"), Some((1, 1)));
        assert_eq!(
            subtask_counts("intro\n- [x] done\n  * [ ] nested\n- [x] more\ntail"),
            Some((2, 3))
        );
    }

    #[test]
    fn test_parse_context() {
        assert_eq!(parse_context(""), None);
        assert_eq!(parse_context("   "), None);
        assert_eq!(
            parse_context("src/editor.rs:1240"),
            Some(("src/editor.rs".to_string(), Some(1240)))
        );
        assert_eq!(
            parse_context("src/editor.rs"),
            Some(("src/editor.rs".to_string(), None))
        );
        assert_eq!(parse_context("a:b:12"), Some(("a:b".to_string(), Some(12))));
        assert_eq!(parse_context("notes:"), Some(("notes:".to_string(), None)));
    }

    #[test]
    fn test_serde_defaults_are_skipped() {
        let file = InboxFile {
            version: Some(1),
            inbox: vec![InboxItem {
                id: "abc".into(),
                text: "hello".into(),
                kind: None,
                from: None,
                body: None,
                attachments: Vec::new(),
                created: None,
                cleared: None,
            }],
            types: Vec::new(),
            archived: Vec::new(),
            sort: SortMode::Manual,
            hidden_fields: Vec::new(),
        };
        let json = serde_json::to_string(&file).unwrap();
        assert!(!json.contains("types"));
        assert!(!json.contains("archived"));
        assert!(!json.contains("kind"));
        assert!(!json.contains("attachments"));
        assert!(!json.contains("sort"));
        assert!(!json.contains("hidden_fields"));

        let parsed: InboxFile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, file);
    }

    #[test]
    fn test_missing_id_is_backfilled_on_deserialize() {
        let parsed: InboxFile = serde_json::from_str(r#"{ "inbox": [{ "text": "x" }] }"#).unwrap();
        assert!(!parsed.inbox[0].id.is_empty());
    }

    #[test]
    fn test_sort_mode_apply() {
        let item = |text: &str, created: i64| InboxItem {
            id: new_item_id(),
            text: text.to_string(),
            kind: None,
            from: None,
            body: None,
            attachments: Vec::new(),
            created: Some(created),
            cleared: None,
        };
        let base = vec![item("banana", 30), item("apple", 10), item("Cherry", 20)];
        let texts = |items: &[InboxItem]| items.iter().map(|i| i.text.clone()).collect::<Vec<_>>();

        let mut manual = base.clone();
        SortMode::Manual.apply(&mut manual);
        assert_eq!(texts(&manual), ["banana", "apple", "Cherry"]);

        let mut newest = base.clone();
        SortMode::Newest.apply(&mut newest);
        assert_eq!(texts(&newest), ["banana", "Cherry", "apple"]);

        let mut oldest = base.clone();
        SortMode::Oldest.apply(&mut oldest);
        assert_eq!(texts(&oldest), ["apple", "Cherry", "banana"]);

        // Case-insensitive alphabetical order.
        let mut az = base.clone();
        SortMode::Az.apply(&mut az);
        assert_eq!(texts(&az), ["apple", "banana", "Cherry"]);

        let mut za = base;
        SortMode::Za.apply(&mut za);
        assert_eq!(texts(&za), ["Cherry", "banana", "apple"]);
    }

    #[test]
    fn test_attachment_ref_display_name() {
        let project = AttachmentRef::Project {
            path: "src/editor.rs".into(),
        };
        assert_eq!(project.path(), "src/editor.rs");
        assert_eq!(project.display_name(), "editor.rs");

        let external = AttachmentRef::External {
            path: r"C:\Users\me\notes.txt".into(),
        };
        assert_eq!(external.display_name(), "notes.txt");

        let unix_external = AttachmentRef::External {
            path: "/home/me/todo.md".into(),
        };
        assert_eq!(unix_external.display_name(), "todo.md");
    }

    #[test]
    fn test_attachment_ref_serde_roundtrip() {
        let refs = vec![
            AttachmentRef::Project {
                path: "src/main.rs".into(),
            },
            AttachmentRef::External {
                path: "/tmp/scratch.log".into(),
            },
        ];
        let json = serde_json::to_string(&refs).unwrap();
        assert!(json.contains(r#""kind":"project""#));
        assert!(json.contains(r#""kind":"external""#));
        let parsed: Vec<AttachmentRef> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, refs);
    }

    #[test]
    fn test_sort_mode_serde_roundtrip() {
        assert_eq!(serde_json::to_string(&SortMode::Az).unwrap(), "\"az\"");
        assert_eq!(
            serde_json::to_string(&SortMode::Newest).unwrap(),
            "\"newest\""
        );
        let parsed: SortMode = serde_json::from_str("\"za\"").unwrap();
        assert_eq!(parsed, SortMode::Za);
    }
}
