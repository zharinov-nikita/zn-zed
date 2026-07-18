//! The block model backing the detail block editor (Tasks 7-9).
//!
//! [`BlockDocument`] is a pure, GPUI-free data structure: a flat list of
//! [`Block`]s plus the id allocator. All mutating operations preserve the
//! invariant that the document always has at least one block, and return
//! enough information ([`EditTarget`]) for the UI layer to know which block
//! to focus and where to place the caret afterwards.

use crate::markdown_codec::{parse_blocks, serialize_blocks};

/// A stable identifier for a [`Block`] within a [`BlockDocument`].
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct BlockId(pub u64);

/// The kind of a block, which determines both its markdown syntax and its
/// editing behavior.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum BlockType {
    Paragraph,
    H1,
    H2,
    Todo,
    Bullet,
    Quote,
    Code,
    Divider,
}

impl BlockType {
    /// Whether this block type holds freely-editable inline text.
    /// Everything except `Code` and `Divider`.
    pub fn is_text(&self) -> bool {
        !matches!(self, BlockType::Code | BlockType::Divider)
    }

    /// The block type a new block created by pressing Enter inside this
    /// block should have. List-like types (`Todo`/`Bullet`/`Quote`)
    /// continue as themselves; headings and paragraphs continue as
    /// `Paragraph`. Not meant to be called for `Code`/`Divider`, but
    /// returns `Paragraph` for them too.
    pub fn continuation(&self) -> BlockType {
        match self {
            BlockType::Todo | BlockType::Bullet | BlockType::Quote => *self,
            BlockType::H1 | BlockType::H2 | BlockType::Paragraph => BlockType::Paragraph,
            BlockType::Code | BlockType::Divider => BlockType::Paragraph,
        }
    }
}

/// A single block of content in a [`BlockDocument`].
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub id: BlockId,
    pub block_type: BlockType,
    pub text: String,
    pub checked: bool,
}

/// Where the UI should place the caret after an operation.
/// Offsets are byte offsets into the target block's text.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CaretPos {
    Start,
    End,
    Offset(usize),
}

/// Tells the UI which block to focus and where to place the caret after a
/// mutating operation.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct EditTarget {
    pub block: BlockId,
    pub caret: CaretPos,
}

/// A document made of an ordered list of [`Block`]s. Always has at least
/// one block; every mutating operation restores this invariant before
/// returning.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockDocument {
    blocks: Vec<Block>,
    next_id: u64,
}

impl BlockDocument {
    /// An empty document: a single empty `Paragraph`.
    pub fn new() -> Self {
        let mut next_id = 0;
        let block = Self::new_block(&mut next_id, BlockType::Paragraph, String::new(), false);
        Self {
            blocks: vec![block],
            next_id,
        }
    }

    /// Parses `src` into a document via the markdown codec.
    pub fn from_markdown(src: &str) -> Self {
        let mut next_id = 0;
        let blocks = parse_blocks(src, &mut next_id);
        Self { blocks, next_id }
    }

    /// Serializes the document back to markdown.
    pub fn to_markdown(&self) -> String {
        serialize_blocks(&self.blocks)
    }

    /// Allocates a new monotonically-increasing [`BlockId`].
    pub fn new_id(&mut self) -> BlockId {
        let id = BlockId(self.next_id);
        self.next_id += 1;
        id
    }

    fn new_block(next_id: &mut u64, block_type: BlockType, text: String, checked: bool) -> Block {
        let id = BlockId(*next_id);
        *next_id += 1;
        Block {
            id,
            block_type,
            text,
            checked,
        }
    }

    /// All blocks, in document order.
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// Looks up a block by id.
    pub fn block(&self, id: BlockId) -> Option<&Block> {
        self.blocks.iter().find(|block| block.id == id)
    }

    /// The index of a block by id.
    pub fn index_of(&self, id: BlockId) -> Option<usize> {
        self.blocks.iter().position(|block| block.id == id)
    }

    /// Number of blocks in the document.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Counts `(checked, total)` across all `Todo` blocks.
    pub fn subtask_counts(&self) -> (usize, usize) {
        let mut checked = 0;
        let mut total = 0;
        for block in &self.blocks {
            if block.block_type == BlockType::Todo {
                total += 1;
                if block.checked {
                    checked += 1;
                }
            }
        }
        (checked, total)
    }

    /// Splits the block `id` at `caret_byte_offset`.
    ///
    /// - An empty `Todo`/`Bullet`/`Quote` block converts to `Paragraph` in
    ///   place (exiting the list), caret at `Start`.
    /// - `Code`/`Divider` blocks cannot be split.
    /// - Otherwise the block is split into a `before` part (staying at
    ///   `id`) and an `after` part (a new block, right after, whose type is
    ///   `continuation()` of the original). `caret_byte_offset` is clamped
    ///   to the nearest char boundary at or before it if it doesn't land on
    ///   one.
    pub fn split(&mut self, id: BlockId, caret_byte_offset: usize) -> Option<EditTarget> {
        let index = self.index_of(id)?;
        let block_type = self.blocks[index].block_type;

        if !block_type.is_text() {
            return None;
        }

        if matches!(
            block_type,
            BlockType::Todo | BlockType::Bullet | BlockType::Quote
        ) && self.blocks[index].text.is_empty()
        {
            self.blocks[index].block_type = BlockType::Paragraph;
            self.blocks[index].checked = false;
            return Some(EditTarget {
                block: id,
                caret: CaretPos::Start,
            });
        }

        let offset = floor_char_boundary(&self.blocks[index].text, caret_byte_offset);
        let text = self.blocks[index].text.clone();
        let (before, after) = text.split_at(offset);
        let before = before.to_string();
        let after = after.to_string();

        self.blocks[index].text = before;
        let new_block = Self::new_block(&mut self.next_id, block_type.continuation(), after, false);
        let new_id = new_block.id;
        self.blocks.insert(index + 1, new_block);

        Some(EditTarget {
            block: new_id,
            caret: CaretPos::Start,
        })
    }

    /// Handles pressing Backspace at the start of block `id`'s text.
    ///
    /// - A non-`Paragraph` *text* block becomes a `Paragraph` in place
    ///   (losing `checked` if it was a `Todo`). `Code`/`Divider` blocks are
    ///   not text blocks, so backspace at their start is a no-op (`None`);
    ///   there's nothing sensible to convert them into in place.
    /// - A `Paragraph` at index 0 is a no-op (`None`).
    /// - If the previous block is `Divider` or `Code`, it is removed and
    ///   the current block stays put.
    /// - Otherwise, the current block is merged into the (text) previous
    ///   block: `prev.text += cur.text`, current is removed, caret goes to
    ///   the boundary between them.
    pub fn backspace_at_start(&mut self, id: BlockId) -> Option<EditTarget> {
        let index = self.index_of(id)?;
        let block_type = self.blocks[index].block_type;

        if !block_type.is_text() {
            return None;
        }

        if block_type != BlockType::Paragraph {
            self.blocks[index].block_type = BlockType::Paragraph;
            self.blocks[index].checked = false;
            return Some(EditTarget {
                block: id,
                caret: CaretPos::Start,
            });
        }

        if index == 0 {
            return None;
        }

        let prev_type = self.blocks[index - 1].block_type;
        if matches!(prev_type, BlockType::Divider | BlockType::Code) {
            self.blocks.remove(index - 1);
            return Some(EditTarget {
                block: id,
                caret: CaretPos::Start,
            });
        }

        let prev_len = self.blocks[index - 1].text.len();
        let cur_text = self.blocks[index].text.clone();
        self.blocks[index - 1].text.push_str(&cur_text);
        let prev_id = self.blocks[index - 1].id;
        self.blocks.remove(index);

        Some(EditTarget {
            block: prev_id,
            caret: CaretPos::Offset(prev_len),
        })
    }

    /// Changes the type of block `id`. Text is preserved (callers that want
    /// to clear it, e.g. a slash menu, should follow up with
    /// [`Self::set_text`]). `checked` resets when moving away from `Todo`.
    ///
    /// Converting to `Divider` is special: the block becomes an empty
    /// `Divider`, and a new empty `Paragraph` is inserted right after it;
    /// the result targets that new paragraph.
    ///
    /// Converting a `Code` block (which may legally contain embedded
    /// newlines) into a text block sanitizes the text by replacing
    /// newlines with spaces, since text blocks must hold single-line
    /// content for the markdown codec's serialize/parse round trip to
    /// hold. Converting into `Code` keeps the text as-is.
    pub fn convert(&mut self, id: BlockId, block_type: BlockType) -> Option<EditTarget> {
        let index = self.index_of(id)?;
        let prev_type = self.blocks[index].block_type;

        if block_type != BlockType::Todo {
            self.blocks[index].checked = false;
        }
        self.blocks[index].block_type = block_type;

        if prev_type == BlockType::Code && block_type.is_text() {
            self.blocks[index].text = self.blocks[index].text.replace('\n', " ");
        }

        if block_type == BlockType::Divider {
            self.blocks[index].text.clear();
            let new_block = Self::new_block(
                &mut self.next_id,
                BlockType::Paragraph,
                String::new(),
                false,
            );
            let new_id = new_block.id;
            self.blocks.insert(index + 1, new_block);
            return Some(EditTarget {
                block: new_id,
                caret: CaretPos::Start,
            });
        }

        Some(EditTarget {
            block: id,
            caret: CaretPos::Start,
        })
    }

    /// Overwrites the text of block `id`.
    pub fn set_text(&mut self, id: BlockId, text: String) {
        if let Some(block) = self.blocks.iter_mut().find(|block| block.id == id) {
            block.text = text;
        }
    }

    /// Applies the live editor text of block `id` back into the document.
    ///
    /// Text blocks must stay single-line — the markdown codec's round trip
    /// depends on it — but an auto-height editor can still come to hold
    /// newlines (multi-line paste, shift-enter). When that happens for a
    /// non-`Code` block, the first line stays in the block and the rest is
    /// parsed through the markdown codec into new blocks inserted right
    /// after it, so no text is lost and pasted markdown keeps its
    /// structure. Returns the block/caret the UI should move editing to
    /// when such a restructure happened, `None` when the text was applied
    /// in place.
    ///
    /// `Code` blocks keep newlines verbatim; `Divider` blocks are never
    /// edited and are left untouched.
    pub fn apply_text(&mut self, id: BlockId, text: &str) -> Option<EditTarget> {
        let index = self.index_of(id)?;
        let block_type = self.blocks[index].block_type;

        if block_type == BlockType::Divider {
            return None;
        }
        if block_type == BlockType::Code {
            self.blocks[index].text = text.to_string();
            return None;
        }
        let Some((first, rest)) = text.split_once('\n') else {
            self.blocks[index].text = text.to_string();
            return None;
        };

        self.blocks[index].text = first.to_string();
        // `parse_blocks` never returns an empty list: whitespace-only tails
        // (e.g. a trailing newline) become a single empty paragraph, which
        // matches what pressing Enter at the end of the block would do.
        let new_blocks = parse_blocks(rest, &mut self.next_id);
        let target = new_blocks.last().map(|block| EditTarget {
            block: block.id,
            caret: CaretPos::End,
        });
        for (offset, block) in new_blocks.into_iter().enumerate() {
            self.blocks.insert(index + 1 + offset, block);
        }
        target
    }

    /// Toggles `checked` on a `Todo` block (no-op on other types).
    pub fn toggle_checked(&mut self, id: BlockId) {
        if let Some(block) = self.blocks.iter_mut().find(|block| block.id == id)
            && block.block_type == BlockType::Todo
        {
            block.checked = !block.checked;
        }
    }

    /// Inserts a new empty `Paragraph` right after block `id`. If `id` is
    /// not in the document (e.g. the block was removed by a concurrent
    /// operation), the paragraph is appended at the end instead.
    pub fn insert_after(&mut self, id: BlockId) -> EditTarget {
        let index = self
            .index_of(id)
            .unwrap_or(self.blocks.len().saturating_sub(1));
        let new_block = Self::new_block(
            &mut self.next_id,
            BlockType::Paragraph,
            String::new(),
            false,
        );
        let new_id = new_block.id;
        self.blocks.insert(index + 1, new_block);
        EditTarget {
            block: new_id,
            caret: CaretPos::Start,
        }
    }

    /// Appends a new empty `Paragraph` at the end of the document.
    pub fn append_paragraph(&mut self) -> EditTarget {
        let new_block = Self::new_block(
            &mut self.next_id,
            BlockType::Paragraph,
            String::new(),
            false,
        );
        let new_id = new_block.id;
        self.blocks.push(new_block);
        EditTarget {
            block: new_id,
            caret: CaretPos::Start,
        }
    }

    /// Duplicates block `id`, inserting the copy (with a new id) right
    /// after it. Returns the new block's id.
    pub fn duplicate(&mut self, id: BlockId) -> Option<BlockId> {
        let index = self.index_of(id)?;
        let original = self.blocks[index].clone();
        let new_block = Self::new_block(
            &mut self.next_id,
            original.block_type,
            original.text,
            original.checked,
        );
        let new_id = new_block.id;
        self.blocks.insert(index + 1, new_block);
        Some(new_id)
    }

    /// Swaps block `id` with its neighbor in direction `dir` (`-1` for up,
    /// `+1` for down). Returns `false` (no-op) if that would go out of
    /// bounds.
    pub fn move_block(&mut self, id: BlockId, dir: i32) -> bool {
        let Some(index) = self.index_of(id) else {
            return false;
        };
        let Some(target) = index.checked_add_signed(dir as isize) else {
            return false;
        };
        if target >= self.blocks.len() {
            return false;
        }
        self.blocks.swap(index, target);
        true
    }

    /// Removes block `id`. If it was the last remaining block, it is
    /// replaced by a fresh empty `Paragraph`. Returns the block to focus
    /// next: the previous block (caret at `End`), or the newly-inserted
    /// empty paragraph.
    pub fn remove(&mut self, id: BlockId) -> Option<EditTarget> {
        let index = self.index_of(id)?;
        self.blocks.remove(index);

        if self.blocks.is_empty() {
            let new_block = Self::new_block(
                &mut self.next_id,
                BlockType::Paragraph,
                String::new(),
                false,
            );
            let new_id = new_block.id;
            self.blocks.push(new_block);
            return Some(EditTarget {
                block: new_id,
                caret: CaretPos::Start,
            });
        }

        let prev_index = index.saturating_sub(1);
        let prev_id = self.blocks[prev_index].id;
        Some(EditTarget {
            block: prev_id,
            caret: CaretPos::End,
        })
    }
}

impl Default for BlockDocument {
    fn default() -> Self {
        Self::new()
    }
}

/// Rounds `offset` down to the nearest char boundary in `s`, so slicing
/// never panics even if the caller passes an offset that lands in the
/// middle of a multi-byte character.
fn floor_char_boundary(s: &str, offset: usize) -> usize {
    if offset >= s.len() {
        return s.len();
    }
    let mut offset = offset;
    while offset > 0 && !s.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn doc_from(blocks: Vec<(BlockType, &str, bool)>) -> BlockDocument {
        let mut next_id = 0;
        let blocks = blocks
            .into_iter()
            .map(|(block_type, text, checked)| {
                let id = BlockId(next_id);
                next_id += 1;
                Block {
                    id,
                    block_type,
                    text: text.to_string(),
                    checked,
                }
            })
            .collect();
        BlockDocument { blocks, next_id }
    }

    #[test]
    fn test_new_document_has_one_empty_paragraph() {
        let doc = BlockDocument::new();
        assert_eq!(doc.len(), 1);
        assert_eq!(doc.blocks()[0].block_type, BlockType::Paragraph);
        assert_eq!(doc.blocks()[0].text, "");
    }

    #[test]
    fn test_block_type_is_text() {
        assert!(BlockType::Paragraph.is_text());
        assert!(BlockType::H1.is_text());
        assert!(BlockType::H2.is_text());
        assert!(BlockType::Todo.is_text());
        assert!(BlockType::Bullet.is_text());
        assert!(BlockType::Quote.is_text());
        assert!(!BlockType::Code.is_text());
        assert!(!BlockType::Divider.is_text());
    }

    #[test]
    fn test_block_type_continuation() {
        assert_eq!(BlockType::Todo.continuation(), BlockType::Todo);
        assert_eq!(BlockType::Bullet.continuation(), BlockType::Bullet);
        assert_eq!(BlockType::Quote.continuation(), BlockType::Quote);
        assert_eq!(BlockType::H1.continuation(), BlockType::Paragraph);
        assert_eq!(BlockType::H2.continuation(), BlockType::Paragraph);
        assert_eq!(BlockType::Paragraph.continuation(), BlockType::Paragraph);
        assert_eq!(BlockType::Code.continuation(), BlockType::Paragraph);
        assert_eq!(BlockType::Divider.continuation(), BlockType::Paragraph);
    }

    // --- split ---

    #[test]
    fn test_split_middle_of_line() {
        let mut doc = doc_from(vec![(BlockType::Paragraph, "hello world", false)]);
        let id = doc.blocks()[0].id;
        let target = doc.split(id, 5).unwrap();
        assert_eq!(doc.blocks()[0].text, "hello");
        assert_eq!(doc.blocks()[1].text, " world");
        assert_eq!(target.block, doc.blocks()[1].id);
        assert_eq!(target.caret, CaretPos::Start);
    }

    #[test]
    fn test_split_at_start_and_end() {
        let mut doc = doc_from(vec![(BlockType::Paragraph, "hello", false)]);
        let id = doc.blocks()[0].id;
        doc.split(id, 0).unwrap();
        assert_eq!(doc.blocks()[0].text, "");
        assert_eq!(doc.blocks()[1].text, "hello");

        let mut doc = doc_from(vec![(BlockType::Paragraph, "hello", false)]);
        let id = doc.blocks()[0].id;
        doc.split(id, 5).unwrap();
        assert_eq!(doc.blocks()[0].text, "hello");
        assert_eq!(doc.blocks()[1].text, "");
    }

    #[test]
    fn test_split_multibyte_offset_is_clamped_to_char_boundary() {
        // "привет" — Cyrillic, 2 bytes per char in UTF-8. Byte offset 3
        // lands in the middle of the second character ('р' spans bytes
        // 2..4), so it must be clamped down to 2.
        let text = "привет";
        assert!(!text.is_char_boundary(3));
        let mut doc = doc_from(vec![(BlockType::Paragraph, text, false)]);
        let id = doc.blocks()[0].id;
        doc.split(id, 3).unwrap();
        assert_eq!(doc.blocks()[0].text, "п");
        assert_eq!(doc.blocks()[1].text, "ривет");
    }

    #[test]
    fn test_split_offset_past_end_is_clamped_to_len() {
        let mut doc = doc_from(vec![(BlockType::Paragraph, "hi", false)]);
        let id = doc.blocks()[0].id;
        doc.split(id, 100).unwrap();
        assert_eq!(doc.blocks()[0].text, "hi");
        assert_eq!(doc.blocks()[1].text, "");
    }

    #[test]
    fn test_split_empty_list_item_exits_to_paragraph() {
        for block_type in [BlockType::Todo, BlockType::Bullet, BlockType::Quote] {
            let mut doc = doc_from(vec![(block_type, "", false)]);
            let id = doc.blocks()[0].id;
            let target = doc.split(id, 0).unwrap();
            assert_eq!(doc.len(), 1, "{block_type:?} should not create a new block");
            assert_eq!(doc.blocks()[0].block_type, BlockType::Paragraph);
            assert_eq!(target.block, id);
            assert_eq!(target.caret, CaretPos::Start);
        }
    }

    #[test]
    fn test_split_continuation_types() {
        for (block_type, expected_continuation) in [
            (BlockType::Todo, BlockType::Todo),
            (BlockType::Bullet, BlockType::Bullet),
            (BlockType::Quote, BlockType::Quote),
            (BlockType::H1, BlockType::Paragraph),
            (BlockType::H2, BlockType::Paragraph),
            (BlockType::Paragraph, BlockType::Paragraph),
        ] {
            let mut doc = doc_from(vec![(block_type, "some text", false)]);
            let id = doc.blocks()[0].id;
            doc.split(id, 4).unwrap();
            assert_eq!(
                doc.blocks()[1].block_type,
                expected_continuation,
                "{block_type:?} continuation"
            );
            assert!(!doc.blocks()[1].checked);
        }
    }

    #[test]
    fn test_split_code_and_divider_returns_none() {
        let mut doc = doc_from(vec![(BlockType::Code, "let x = 1;", false)]);
        let id = doc.blocks()[0].id;
        assert_eq!(doc.split(id, 3), None);
        assert_eq!(doc.len(), 1);

        let mut doc = doc_from(vec![(BlockType::Divider, "", false)]);
        let id = doc.blocks()[0].id;
        assert_eq!(doc.split(id, 0), None);
        assert_eq!(doc.len(), 1);
    }

    // --- backspace_at_start ---

    #[test]
    fn test_backspace_non_paragraph_becomes_paragraph() {
        let mut doc = doc_from(vec![(BlockType::Todo, "task", true)]);
        let id = doc.blocks()[0].id;
        let target = doc.backspace_at_start(id).unwrap();
        assert_eq!(doc.blocks()[0].block_type, BlockType::Paragraph);
        assert_eq!(doc.blocks()[0].text, "task");
        assert!(!doc.blocks()[0].checked);
        assert_eq!(target.block, id);
        assert_eq!(target.caret, CaretPos::Start);
    }

    #[test]
    fn test_backspace_paragraph_at_index_zero_is_noop() {
        let mut doc = doc_from(vec![(BlockType::Paragraph, "first", false)]);
        let id = doc.blocks()[0].id;
        assert_eq!(doc.backspace_at_start(id), None);
        assert_eq!(doc.len(), 1);
        assert_eq!(doc.blocks()[0].text, "first");
    }

    #[test]
    fn test_backspace_removes_divider_or_code_before() {
        let mut doc = doc_from(vec![
            (BlockType::Divider, "", false),
            (BlockType::Paragraph, "after", false),
        ]);
        let cur_id = doc.blocks()[1].id;
        let target = doc.backspace_at_start(cur_id).unwrap();
        assert_eq!(doc.len(), 1);
        assert_eq!(doc.blocks()[0].id, cur_id);
        assert_eq!(doc.blocks()[0].text, "after");
        assert_eq!(target.block, cur_id);
        assert_eq!(target.caret, CaretPos::Start);

        let mut doc = doc_from(vec![
            (BlockType::Code, "code", false),
            (BlockType::Paragraph, "after", false),
        ]);
        let cur_id = doc.blocks()[1].id;
        let target = doc.backspace_at_start(cur_id).unwrap();
        assert_eq!(doc.len(), 1);
        assert_eq!(target.block, cur_id);
        assert_eq!(target.caret, CaretPos::Start);
    }

    #[test]
    fn test_backspace_merges_into_previous_text_block() {
        let mut doc = doc_from(vec![
            (BlockType::Paragraph, "hello", false),
            (BlockType::Paragraph, "world", false),
        ]);
        let prev_id = doc.blocks()[0].id;
        let cur_id = doc.blocks()[1].id;
        let target = doc.backspace_at_start(cur_id).unwrap();
        assert_eq!(doc.len(), 1);
        assert_eq!(doc.blocks()[0].id, prev_id);
        assert_eq!(doc.blocks()[0].text, "helloworld");
        assert_eq!(target.block, prev_id);
        assert_eq!(target.caret, CaretPos::Offset(5));
    }

    #[test]
    fn test_backspace_on_code_or_divider_is_noop() {
        let mut doc = doc_from(vec![(BlockType::Code, "let x = 1;\nlet y = 2;", false)]);
        let id = doc.blocks()[0].id;
        assert_eq!(doc.backspace_at_start(id), None);
        assert_eq!(doc.len(), 1);
        assert_eq!(doc.blocks()[0].block_type, BlockType::Code);
        assert_eq!(doc.blocks()[0].text, "let x = 1;\nlet y = 2;");

        let mut doc = doc_from(vec![(BlockType::Divider, "", false)]);
        let id = doc.blocks()[0].id;
        assert_eq!(doc.backspace_at_start(id), None);
        assert_eq!(doc.len(), 1);
        assert_eq!(doc.blocks()[0].block_type, BlockType::Divider);
        assert_eq!(doc.blocks()[0].text, "");
    }

    // --- convert ---

    #[test]
    fn test_convert_to_divider_inserts_paragraph_after() {
        let mut doc = doc_from(vec![(BlockType::Paragraph, "some text", false)]);
        let id = doc.blocks()[0].id;
        let target = doc.convert(id, BlockType::Divider).unwrap();
        assert_eq!(doc.len(), 2);
        assert_eq!(doc.blocks()[0].block_type, BlockType::Divider);
        assert_eq!(doc.blocks()[0].text, "");
        assert_eq!(doc.blocks()[1].block_type, BlockType::Paragraph);
        assert_eq!(doc.blocks()[1].text, "");
        assert_eq!(target.block, doc.blocks()[1].id);
        assert_eq!(target.caret, CaretPos::Start);
    }

    #[test]
    fn test_convert_todo_to_bullet_resets_checked() {
        let mut doc = doc_from(vec![(BlockType::Todo, "task", true)]);
        let id = doc.blocks()[0].id;
        let target = doc.convert(id, BlockType::Bullet).unwrap();
        assert_eq!(doc.blocks()[0].block_type, BlockType::Bullet);
        assert_eq!(doc.blocks()[0].text, "task");
        assert!(!doc.blocks()[0].checked);
        assert_eq!(target.block, id);
    }

    #[test]
    fn test_convert_to_todo_preserves_text() {
        let mut doc = doc_from(vec![(BlockType::Bullet, "task", false)]);
        let id = doc.blocks()[0].id;
        doc.convert(id, BlockType::Todo).unwrap();
        assert_eq!(doc.blocks()[0].block_type, BlockType::Todo);
        assert_eq!(doc.blocks()[0].text, "task");
        assert!(!doc.blocks()[0].checked);
    }

    #[test]
    fn test_convert_code_to_paragraph_sanitizes_newlines() {
        let mut doc = doc_from(vec![(BlockType::Code, "a\nb", false)]);
        let id = doc.blocks()[0].id;
        doc.convert(id, BlockType::Paragraph).unwrap();
        assert_eq!(doc.blocks()[0].block_type, BlockType::Paragraph);
        assert_eq!(doc.blocks()[0].text, "a b");

        // The whole point of sanitizing is to keep serialize ∘ parse an
        // identity for text blocks: a multi-line Code block turned into a
        // Paragraph must not re-split into multiple blocks on round trip.
        let serialized = doc.to_markdown();
        let round_tripped = BlockDocument::from_markdown(&serialized);
        assert_eq!(round_tripped.to_markdown(), serialized);
        assert_eq!(round_tripped.len(), doc.len());
        assert_eq!(round_tripped.blocks()[0].block_type, BlockType::Paragraph);
        assert_eq!(round_tripped.blocks()[0].text, "a b");
    }

    #[test]
    fn test_convert_code_to_other_text_types_sanitizes_newlines() {
        for block_type in [
            BlockType::H1,
            BlockType::H2,
            BlockType::Todo,
            BlockType::Bullet,
            BlockType::Quote,
        ] {
            let mut doc = doc_from(vec![(BlockType::Code, "line1\nline2\nline3", false)]);
            let id = doc.blocks()[0].id;
            doc.convert(id, block_type).unwrap();
            assert_eq!(doc.blocks()[0].block_type, block_type);
            assert_eq!(
                doc.blocks()[0].text,
                "line1 line2 line3",
                "{block_type:?} should have sanitized newlines"
            );
        }
    }

    #[test]
    fn test_convert_code_to_code_keeps_newlines() {
        let mut doc = doc_from(vec![(BlockType::Code, "a\nb", false)]);
        let id = doc.blocks()[0].id;
        doc.convert(id, BlockType::Code).unwrap();
        assert_eq!(doc.blocks()[0].block_type, BlockType::Code);
        assert_eq!(doc.blocks()[0].text, "a\nb");
    }

    #[test]
    fn test_convert_code_to_divider_empties_text() {
        let mut doc = doc_from(vec![(BlockType::Code, "a\nb", false)]);
        let id = doc.blocks()[0].id;
        doc.convert(id, BlockType::Divider).unwrap();
        assert_eq!(doc.blocks()[0].block_type, BlockType::Divider);
        assert_eq!(doc.blocks()[0].text, "");
    }

    // --- remove / move / duplicate ---

    #[test]
    fn test_remove_last_block_yields_empty_paragraph() {
        let mut doc = doc_from(vec![(BlockType::Paragraph, "only", false)]);
        let id = doc.blocks()[0].id;
        let target = doc.remove(id).unwrap();
        assert_eq!(doc.len(), 1);
        assert_eq!(doc.blocks()[0].block_type, BlockType::Paragraph);
        assert_eq!(doc.blocks()[0].text, "");
        assert_ne!(doc.blocks()[0].id, id);
        assert_eq!(target.block, doc.blocks()[0].id);
        assert_eq!(target.caret, CaretPos::Start);
    }

    #[test]
    fn test_remove_non_last_block_targets_previous_at_end() {
        let mut doc = doc_from(vec![
            (BlockType::Paragraph, "first", false),
            (BlockType::Paragraph, "second", false),
        ]);
        let prev_id = doc.blocks()[0].id;
        let cur_id = doc.blocks()[1].id;
        let target = doc.remove(cur_id).unwrap();
        assert_eq!(doc.len(), 1);
        assert_eq!(target.block, prev_id);
        assert_eq!(target.caret, CaretPos::End);
    }

    #[test]
    fn test_remove_first_block_targets_new_first_block() {
        // There is no "previous" block when removing index 0; the only
        // reasonable target is the block that takes its place.
        let mut doc = doc_from(vec![
            (BlockType::Paragraph, "first", false),
            (BlockType::Paragraph, "second", false),
        ]);
        let first_id = doc.blocks()[0].id;
        let second_id = doc.blocks()[1].id;
        let target = doc.remove(first_id).unwrap();
        assert_eq!(doc.len(), 1);
        assert_eq!(doc.blocks()[0].id, second_id);
        assert_eq!(target.block, second_id);
        assert_eq!(target.caret, CaretPos::End);
    }

    #[test]
    fn test_move_block_out_of_bounds_returns_false() {
        let mut doc = doc_from(vec![
            (BlockType::Paragraph, "first", false),
            (BlockType::Paragraph, "second", false),
        ]);
        let first_id = doc.blocks()[0].id;
        let second_id = doc.blocks()[1].id;
        assert!(!doc.move_block(first_id, -1));
        assert!(!doc.move_block(second_id, 1));
        assert_eq!(doc.blocks()[0].id, first_id);
        assert_eq!(doc.blocks()[1].id, second_id);
    }

    #[test]
    fn test_move_block_swaps_neighbors() {
        let mut doc = doc_from(vec![
            (BlockType::Paragraph, "first", false),
            (BlockType::Paragraph, "second", false),
        ]);
        let first_id = doc.blocks()[0].id;
        let second_id = doc.blocks()[1].id;
        assert!(doc.move_block(second_id, -1));
        assert_eq!(doc.blocks()[0].id, second_id);
        assert_eq!(doc.blocks()[1].id, first_id);
    }

    #[test]
    fn test_duplicate_inserts_copy_after_with_new_id() {
        let mut doc = doc_from(vec![(BlockType::Todo, "task", true)]);
        let id = doc.blocks()[0].id;
        let new_id = doc.duplicate(id).unwrap();
        assert_eq!(doc.len(), 2);
        assert_ne!(new_id, id);
        assert_eq!(doc.blocks()[1].id, new_id);
        assert_eq!(doc.blocks()[1].block_type, BlockType::Todo);
        assert_eq!(doc.blocks()[1].text, "task");
        assert!(doc.blocks()[1].checked);
    }

    // --- apply_text ---

    #[test]
    fn test_apply_text_single_line_sets_text_in_place() {
        let mut doc = doc_from(vec![(BlockType::Paragraph, "old", false)]);
        let id = doc.blocks()[0].id;
        assert_eq!(doc.apply_text(id, "new text"), None);
        assert_eq!(doc.len(), 1);
        assert_eq!(doc.blocks()[0].text, "new text");
    }

    #[test]
    fn test_apply_text_code_keeps_newlines_verbatim() {
        let mut doc = doc_from(vec![(BlockType::Code, "old", false)]);
        let id = doc.blocks()[0].id;
        assert_eq!(doc.apply_text(id, "let x = 1;\nlet y = 2;"), None);
        assert_eq!(doc.len(), 1);
        assert_eq!(doc.blocks()[0].text, "let x = 1;\nlet y = 2;");
    }

    #[test]
    fn test_apply_text_divider_is_untouched() {
        let mut doc = doc_from(vec![(BlockType::Divider, "", false)]);
        let id = doc.blocks()[0].id;
        assert_eq!(doc.apply_text(id, "stray\ntext"), None);
        assert_eq!(doc.blocks()[0].text, "");
    }

    #[test]
    fn test_apply_text_multiline_splits_into_new_blocks() {
        let mut doc = doc_from(vec![
            (BlockType::Paragraph, "old", false),
            (BlockType::Paragraph, "tail", false),
        ]);
        let id = doc.blocks()[0].id;
        let target = doc.apply_text(id, "first\nsecond\nthird").unwrap();
        assert_eq!(doc.len(), 4);
        assert_eq!(doc.blocks()[0].text, "first");
        assert_eq!(doc.blocks()[1].text, "second");
        assert_eq!(doc.blocks()[2].text, "third");
        assert_eq!(doc.blocks()[3].text, "tail");
        assert_eq!(doc.blocks()[1].block_type, BlockType::Paragraph);
        assert_eq!(doc.blocks()[2].block_type, BlockType::Paragraph);
        // The caret moves to the end of the last inserted block.
        assert_eq!(target.block, doc.blocks()[2].id);
        assert_eq!(target.caret, CaretPos::End);
    }

    #[test]
    fn test_apply_text_multiline_parses_markdown_in_tail() {
        let mut doc = doc_from(vec![(BlockType::Bullet, "old", false)]);
        let id = doc.blocks()[0].id;
        let target = doc.apply_text(id, "first\n- [x] done\n---").unwrap();
        assert_eq!(doc.len(), 3);
        assert_eq!(doc.blocks()[0].block_type, BlockType::Bullet);
        assert_eq!(doc.blocks()[0].text, "first");
        assert_eq!(doc.blocks()[1].block_type, BlockType::Todo);
        assert_eq!(doc.blocks()[1].text, "done");
        assert!(doc.blocks()[1].checked);
        assert_eq!(doc.blocks()[2].block_type, BlockType::Divider);
        assert_eq!(target.block, doc.blocks()[2].id);
    }

    #[test]
    fn test_apply_text_trailing_newline_appends_empty_paragraph() {
        let mut doc = doc_from(vec![(BlockType::Paragraph, "abc", false)]);
        let id = doc.blocks()[0].id;
        let target = doc.apply_text(id, "abc\n").unwrap();
        assert_eq!(doc.len(), 2);
        assert_eq!(doc.blocks()[0].text, "abc");
        assert_eq!(doc.blocks()[1].block_type, BlockType::Paragraph);
        assert_eq!(doc.blocks()[1].text, "");
        assert_eq!(target.block, doc.blocks()[1].id);
    }

    #[test]
    fn test_apply_text_multiline_round_trips_through_codec() {
        let mut doc = doc_from(vec![(BlockType::Paragraph, "old", false)]);
        let id = doc.blocks()[0].id;
        doc.apply_text(id, "first\nsecond\n\nthird").unwrap();
        // No text block may hold a newline (the codec invariant), so
        // serialize ∘ parse must be an identity.
        assert!(doc.blocks().iter().all(|block| !block.text.contains('\n')));
        let serialized = doc.to_markdown();
        let round_tripped = BlockDocument::from_markdown(&serialized);
        assert_eq!(round_tripped.to_markdown(), serialized);
    }

    #[test]
    fn test_apply_text_new_block_ids_are_fresh() {
        let mut doc = doc_from(vec![(BlockType::Paragraph, "old", false)]);
        let id = doc.blocks()[0].id;
        doc.apply_text(id, "a\nb\nc").unwrap();
        let mut ids: Vec<u64> = doc.blocks().iter().map(|block| block.id.0).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), doc.len(), "ids must be unique");
    }

    // --- subtask_counts ---

    #[test]
    fn test_subtask_counts() {
        let doc = doc_from(vec![
            (BlockType::Paragraph, "intro", false),
            (BlockType::Todo, "done one", true),
            (BlockType::Todo, "open one", false),
            (BlockType::Todo, "done two", true),
            (BlockType::Bullet, "not a todo", false),
        ]);
        assert_eq!(doc.subtask_counts(), (2, 3));
    }

    #[test]
    fn test_subtask_counts_no_todos() {
        let doc = doc_from(vec![(BlockType::Paragraph, "just text", false)]);
        assert_eq!(doc.subtask_counts(), (0, 0));
    }

    // --- set_text / toggle_checked / insert_after / append_paragraph ---

    #[test]
    fn test_set_text_and_toggle_checked() {
        let mut doc = doc_from(vec![(BlockType::Todo, "task", false)]);
        let id = doc.blocks()[0].id;
        doc.set_text(id, "new text".to_string());
        assert_eq!(doc.blocks()[0].text, "new text");
        doc.toggle_checked(id);
        assert!(doc.blocks()[0].checked);
        doc.toggle_checked(id);
        assert!(!doc.blocks()[0].checked);
    }

    #[test]
    fn test_insert_after_and_append_paragraph() {
        let mut doc = doc_from(vec![(BlockType::Paragraph, "first", false)]);
        let id = doc.blocks()[0].id;
        let target = doc.insert_after(id);
        assert_eq!(doc.len(), 2);
        assert_eq!(target.block, doc.blocks()[1].id);
        assert_eq!(doc.blocks()[1].text, "");

        let target = doc.append_paragraph();
        assert_eq!(doc.len(), 3);
        assert_eq!(target.block, doc.blocks()[2].id);
    }

    // --- invariant ---

    #[test]
    fn test_invariant_document_never_empty_after_operations() {
        let mut doc = BlockDocument::new();
        assert!(!doc.blocks().is_empty());

        let id = doc.blocks()[0].id;
        doc.remove(id);
        assert!(!doc.blocks().is_empty());

        let id = doc.blocks()[0].id;
        doc.split(id, 0);
        assert!(!doc.blocks().is_empty());

        let id = doc.blocks()[0].id;
        doc.backspace_at_start(id);
        assert!(!doc.blocks().is_empty());
    }

    // --- markdown round trip via BlockDocument ---

    #[test]
    fn test_from_markdown_to_markdown_round_trip() {
        let src = "# Title\n- [x] done\n- [ ] open\nplain paragraph";
        let doc = BlockDocument::from_markdown(src);
        assert_eq!(doc.to_markdown(), src);
    }
}
