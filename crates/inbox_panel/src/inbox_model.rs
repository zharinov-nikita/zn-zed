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
            created: None,
            cleared: None,
        };
        assert!(!item.is_cleared());
        item.cleared = Some(1);
        assert!(item.is_cleared());
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
                created: None,
                cleared: None,
            }],
            types: Vec::new(),
            archived: Vec::new(),
        };
        let json = serde_json::to_string(&file).unwrap();
        assert!(!json.contains("types"));
        assert!(!json.contains("archived"));
        assert!(!json.contains("kind"));

        let parsed: InboxFile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, file);
    }

    #[test]
    fn test_missing_id_is_backfilled_on_deserialize() {
        let parsed: InboxFile = serde_json::from_str(r#"{ "inbox": [{ "text": "x" }] }"#).unwrap();
        assert!(!parsed.inbox[0].id.is_empty());
    }
}
