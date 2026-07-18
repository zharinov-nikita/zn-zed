//! The data side of the detail view's slash menu: the catalog of block
//! types offered when the user types "/" in a block, and the query filter
//! over it. Pure and GPUI-free; the rendering lives in `detail_view.rs`.

use crate::block::{BlockId, BlockType};

/// One row of the slash menu: a glyph badge, a label, a dimmed hint and the
/// block type applying the entry converts the block into.
pub struct SlashEntry {
    pub glyph: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
    pub block_type: BlockType,
}

/// All slash menu entries, in menu order.
pub const SLASH_ENTRIES: &[SlashEntry] = &[
    SlashEntry {
        glyph: "H1",
        label: "Heading 1",
        hint: "large",
        block_type: BlockType::H1,
    },
    SlashEntry {
        glyph: "H2",
        label: "Heading 2",
        hint: "medium",
        block_type: BlockType::H2,
    },
    SlashEntry {
        glyph: "☑",
        label: "To-do",
        hint: "checklist",
        block_type: BlockType::Todo,
    },
    SlashEntry {
        glyph: "•",
        label: "List",
        hint: "bullets",
        block_type: BlockType::Bullet,
    },
    SlashEntry {
        glyph: "❝",
        label: "Quote",
        hint: "block",
        block_type: BlockType::Quote,
    },
    SlashEntry {
        glyph: "{}",
        label: "Code",
        hint: "monospaced",
        block_type: BlockType::Code,
    },
    SlashEntry {
        glyph: "—",
        label: "Divider",
        hint: "line",
        block_type: BlockType::Divider,
    },
    SlashEntry {
        glyph: "¶",
        label: "Text",
        hint: "plain paragraph",
        block_type: BlockType::Paragraph,
    },
];

/// The state of an open slash menu: which block it is attached to and which
/// entry (an index into [`filtered`]'s result) is selected.
pub struct SlashMenuState {
    pub block_id: BlockId,
    pub selected: usize,
}

/// A short type name an entry also matches by, in addition to its label —
/// e.g. both "list" and "bullet" find the List entry. `Paragraph` goes by
/// "text" (its label in the design); this also keeps a bare "h" matching
/// only the headings.
fn type_name(block_type: BlockType) -> &'static str {
    match block_type {
        BlockType::H1 => "h1",
        BlockType::H2 => "h2",
        BlockType::Todo => "todo",
        BlockType::Bullet => "bullet",
        BlockType::Quote => "quote",
        BlockType::Code => "code",
        BlockType::Divider => "divider",
        BlockType::Paragraph => "text",
    }
}

/// Entries matching `query` (the text after the "/", may be empty), by
/// case-insensitive substring over the label and the latin type name.
pub fn filtered(query: &str) -> Vec<&'static SlashEntry> {
    let query = query.to_lowercase();
    SLASH_ENTRIES
        .iter()
        .filter(|entry| {
            query.is_empty()
                || entry.label.to_lowercase().contains(&query)
                || type_name(entry.block_type).contains(&query)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filtered_empty_query_returns_all_entries() {
        let entries = filtered("");
        assert_eq!(entries.len(), SLASH_ENTRIES.len());
    }

    #[test]
    fn test_filtered_h_matches_headings_only() {
        let entries = filtered("h");
        let types: Vec<BlockType> = entries.iter().map(|entry| entry.block_type).collect();
        assert_eq!(types, vec![BlockType::H1, BlockType::H2]);
    }

    #[test]
    fn test_filtered_matches_label_and_type_name() {
        for query in ["code", "Code", "CODE"] {
            let entries = filtered(query);
            assert_eq!(entries.len(), 1, "query {query:?}");
            assert_eq!(entries[0].block_type, BlockType::Code);
        }
    }

    #[test]
    fn test_filtered_label_substring() {
        let entries = filtered("head");
        let types: Vec<BlockType> = entries.iter().map(|entry| entry.block_type).collect();
        assert_eq!(types, vec![BlockType::H1, BlockType::H2]);
    }

    #[test]
    fn test_filtered_no_match_returns_empty() {
        assert!(filtered("nonexistent").is_empty());
    }
}
