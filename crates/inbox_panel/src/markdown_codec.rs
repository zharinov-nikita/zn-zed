//! Markdown ⇄ [`Block`] codec used by the detail block editor.
//!
//! The mapping is intentionally simple (line-oriented, no inline markdown
//! parsing) — it only needs to round-trip the block types the editor
//! understands.
//!
//! Known v1 limitation: the codec does no escaping, because the item body is
//! stored as plain markdown by design. A paragraph whose text itself starts
//! with block syntax (`# `, `- `, `> `, ` ``` `, `---`, …) is therefore
//! serialized verbatim and reinterpreted as that block type by
//! [`parse_blocks`] the next time the item is opened. See
//! [`serialize_blocks`] for details.

use crate::block::{Block, BlockId, BlockType};

/// Parses `src` into a sequence of [`Block`]s, allocating ids from
/// `next_id` (monotonically increasing, matching the id allocation inside
/// [`BlockDocument`]).
///
/// [`BlockDocument`]: crate::block::BlockDocument
pub fn parse_blocks(src: &str, next_id: &mut u64) -> Vec<Block> {
    let mut alloc = || {
        let id = BlockId(*next_id);
        *next_id += 1;
        id
    };

    let mut blocks = Vec::new();
    let mut lines = src.lines();

    while let Some(line) = lines.next() {
        let (block_type, text, checked) = if let Some(rest) = line.strip_prefix("```") {
            // Fence: collect until a closing ``` or end of input. The
            // (optional) language tag right after the opening fence is
            // discarded.
            let _language = rest.trim();
            let mut code_lines = Vec::new();
            for code_line in lines.by_ref() {
                if code_line.starts_with("```") {
                    break;
                }
                code_lines.push(code_line);
            }
            (BlockType::Code, code_lines.join("\n"), false)
        } else if is_divider(line) {
            (BlockType::Divider, String::new(), false)
        } else if let Some(rest) = line.strip_prefix("# ") {
            (BlockType::H1, rest.to_string(), false)
        } else if let Some(rest) = line.strip_prefix("## ") {
            (BlockType::H2, rest.to_string(), false)
        } else if let Some((checked, rest)) = parse_todo(line) {
            (BlockType::Todo, rest.to_string(), checked)
        } else if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            (BlockType::Bullet, rest.to_string(), false)
        } else if let Some(rest) = line.strip_prefix('>') {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            (BlockType::Quote, rest.to_string(), false)
        } else if line.trim().is_empty() {
            continue;
        } else {
            (BlockType::Paragraph, line.to_string(), false)
        };
        blocks.push(Block {
            id: alloc(),
            block_type,
            text,
            checked,
        });
    }

    if blocks.is_empty() {
        blocks.push(Block {
            id: alloc(),
            block_type: BlockType::Paragraph,
            text: String::new(),
            checked: false,
        });
    }

    blocks
}

/// Serializes `blocks` back into markdown text.
///
/// Known v1 limitation: block texts are emitted verbatim, with no escaping —
/// the body is plain markdown by design, so there is no escape syntax to
/// round-trip through. As a consequence the serialize → [`parse_blocks`]
/// round trip is not lossless for texts that themselves begin with markdown
/// block syntax: a `Paragraph` whose text starts with `# `, `- `, `> `,
/// ` ``` `, `---`, etc. is reinterpreted as a heading/bullet/quote/code/
/// divider block on the next open (and similarly a `Bullet` whose text
/// starts with `[ ] ` comes back as a `Todo`). The text itself is preserved;
/// only the block type silently changes.
pub fn serialize_blocks(blocks: &[Block]) -> String {
    blocks
        .iter()
        .map(|block| match block.block_type {
            BlockType::H1 => format!("# {}", block.text),
            BlockType::H2 => format!("## {}", block.text),
            BlockType::Todo => {
                format!(
                    "- [{}] {}",
                    if block.checked { "x" } else { " " },
                    block.text
                )
            }
            BlockType::Bullet => format!("- {}", block.text),
            BlockType::Quote => format!("> {}", block.text),
            BlockType::Code => format!("```\n{}\n```", block.text),
            BlockType::Divider => "---".to_string(),
            BlockType::Paragraph => block.text.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Matches `^---+\s*$`.
fn is_divider(line: &str) -> bool {
    let trimmed = line.trim_end();
    trimmed.len() >= 3 && trimmed.chars().all(|c| c == '-')
}

/// Matches `^[-*] \[( |x|X)\] ` and returns `(checked, rest)`. The one todo
/// matcher in the crate: the list rows' subtask counter
/// ([`crate::inbox_model::subtask_counts`]) delegates here too, so the row
/// badge and the detail view can never disagree on what counts as a todo.
pub(crate) fn parse_todo(line: &str) -> Option<(bool, &str)> {
    let rest = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))?;
    let rest = rest.strip_prefix('[')?;
    let mut chars = rest.char_indices();
    let (_, marker) = chars.next()?;
    let checked = match marker {
        ' ' => false,
        'x' | 'X' => true,
        _ => return None,
    };
    let (close_idx, close_char) = chars.next()?;
    if close_char != ']' {
        return None;
    }
    let after_bracket = &rest[close_idx + close_char.len_utf8()..];
    let text = after_bracket.strip_prefix(' ')?;
    Some((checked, text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn parse(src: &str) -> Vec<Block> {
        let mut next_id = 0;
        parse_blocks(src, &mut next_id)
    }

    #[test]
    fn test_empty_source_yields_one_empty_paragraph() {
        assert_eq!(
            parse(""),
            vec![Block {
                id: BlockId(0),
                block_type: BlockType::Paragraph,
                text: String::new(),
                checked: false,
            }]
        );
        assert_eq!(
            parse("   \n\t\n  "),
            vec![Block {
                id: BlockId(0),
                block_type: BlockType::Paragraph,
                text: String::new(),
                checked: false,
            }]
        );
    }

    #[test]
    fn test_parse_h1_h2() {
        let blocks = parse("# Title\n## Subtitle");
        assert_eq!(blocks[0].block_type, BlockType::H1);
        assert_eq!(blocks[0].text, "Title");
        assert_eq!(blocks[1].block_type, BlockType::H2);
        assert_eq!(blocks[1].text, "Subtitle");
    }

    #[test]
    fn test_parse_bullet_dash_and_star() {
        let blocks = parse("- dash item\n* star item");
        assert_eq!(blocks[0].block_type, BlockType::Bullet);
        assert_eq!(blocks[0].text, "dash item");
        assert_eq!(blocks[1].block_type, BlockType::Bullet);
        assert_eq!(blocks[1].text, "star item");
    }

    #[test]
    fn test_parse_todo_variants() {
        let blocks = parse("- [ ] open\n- [x] done lower\n- [X] done upper\n* [x] star todo");
        assert_eq!(blocks[0].block_type, BlockType::Todo);
        assert!(!blocks[0].checked);
        assert_eq!(blocks[0].text, "open");
        assert_eq!(blocks[1].block_type, BlockType::Todo);
        assert!(blocks[1].checked);
        assert_eq!(blocks[1].text, "done lower");
        assert_eq!(blocks[2].block_type, BlockType::Todo);
        assert!(blocks[2].checked);
        assert_eq!(blocks[2].text, "done upper");
        assert_eq!(blocks[3].block_type, BlockType::Todo);
        assert!(blocks[3].checked);
        assert_eq!(blocks[3].text, "star todo");
    }

    #[test]
    fn test_parse_quote_with_and_without_space() {
        let blocks = parse("> quoted with space\n>no space");
        assert_eq!(blocks[0].block_type, BlockType::Quote);
        assert_eq!(blocks[0].text, "quoted with space");
        assert_eq!(blocks[1].block_type, BlockType::Quote);
        assert_eq!(blocks[1].text, "no space");
    }

    #[test]
    fn test_parse_divider_variants() {
        let blocks = parse("---\n----\n-----   ");
        assert!(
            blocks
                .iter()
                .all(|block| block.block_type == BlockType::Divider && block.text.is_empty())
        );
        assert_eq!(blocks.len(), 3);
    }

    #[test]
    fn test_dash_dash_is_not_a_divider() {
        // Only two dashes: not a divider, falls through to paragraph.
        let blocks = parse("--");
        assert_eq!(blocks[0].block_type, BlockType::Paragraph);
        assert_eq!(blocks[0].text, "--");
    }

    #[test]
    fn test_parse_code_fence_with_language() {
        let blocks = parse("```rust\nfn main() {}\nlet x = 1;\n```");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_type, BlockType::Code);
        assert_eq!(blocks[0].text, "fn main() {}\nlet x = 1;");
    }

    #[test]
    fn test_parse_unclosed_code_fence_runs_to_end_of_text() {
        let blocks = parse("```\nline one\nline two");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_type, BlockType::Code);
        assert_eq!(blocks[0].text, "line one\nline two");
    }

    #[test]
    fn test_parse_empty_lines_are_skipped() {
        let blocks = parse("first\n\n\nsecond");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "first");
        assert_eq!(blocks[1].text, "second");
    }

    #[test]
    fn test_parse_plain_paragraph() {
        let blocks = parse("just some text");
        assert_eq!(blocks[0].block_type, BlockType::Paragraph);
        assert_eq!(blocks[0].text, "just some text");
    }

    #[test]
    fn test_ids_are_monotonic_across_calls() {
        let mut next_id = 5;
        let blocks = parse_blocks("one\ntwo", &mut next_id);
        assert_eq!(blocks[0].id, BlockId(5));
        assert_eq!(blocks[1].id, BlockId(6));
        assert_eq!(next_id, 7);
    }

    #[test]
    fn test_serialize_all_types() {
        let blocks = vec![
            Block {
                id: BlockId(0),
                block_type: BlockType::H1,
                text: "Title".into(),
                checked: false,
            },
            Block {
                id: BlockId(1),
                block_type: BlockType::H2,
                text: "Subtitle".into(),
                checked: false,
            },
            Block {
                id: BlockId(2),
                block_type: BlockType::Todo,
                text: "done".into(),
                checked: true,
            },
            Block {
                id: BlockId(3),
                block_type: BlockType::Todo,
                text: "open".into(),
                checked: false,
            },
            Block {
                id: BlockId(4),
                block_type: BlockType::Bullet,
                text: "item".into(),
                checked: false,
            },
            Block {
                id: BlockId(5),
                block_type: BlockType::Quote,
                text: "quoted".into(),
                checked: false,
            },
            Block {
                id: BlockId(6),
                block_type: BlockType::Code,
                text: "let x = 1;".into(),
                checked: false,
            },
            Block {
                id: BlockId(7),
                block_type: BlockType::Divider,
                text: String::new(),
                checked: false,
            },
            Block {
                id: BlockId(8),
                block_type: BlockType::Paragraph,
                text: "plain".into(),
                checked: false,
            },
        ];
        let expected = "# Title\n\
                         ## Subtitle\n\
                         - [x] done\n\
                         - [ ] open\n\
                         - item\n\
                         > quoted\n\
                         ```\n\
                         let x = 1;\n\
                         ```\n\
                         ---\n\
                         plain";
        assert_eq!(serialize_blocks(&blocks), expected);
    }

    #[test]
    fn test_round_trip_parse_then_serialize_is_stable() {
        let src = "# Title\n\
                    ## Subtitle\n\
                    - [x] done\n\
                    - [ ] open\n\
                    - item\n\
                    > quoted\n\
                    ```\n\
                    let x = 1;\n\
                    ```\n\
                    ---\n\
                    plain text";
        let blocks = parse(src);
        let serialized = serialize_blocks(&blocks);
        assert_eq!(serialized, src);
        // parse ∘ serialize is idempotent too.
        let reparsed = parse(&serialized);
        assert_eq!(reparsed, blocks);
    }

    #[test]
    fn test_round_trip_serialize_then_parse_preserves_fields() {
        let blocks =
            parse("# H\n## H2\n- [x] a\n- [ ] b\n- bullet\n> quote\n```\ncode\n```\n---\npara");
        let serialized = serialize_blocks(&blocks);
        let reparsed = parse(&serialized);
        assert_eq!(reparsed.len(), blocks.len());
        for (original, reparsed) in blocks.iter().zip(reparsed.iter()) {
            assert_eq!(original.block_type, reparsed.block_type);
            assert_eq!(original.text, reparsed.text);
            assert_eq!(original.checked, reparsed.checked);
        }
    }
}
