//! Stores editable YAML in a rope with cached tokens and edit history.

use std::cell::OnceCell;
use std::ops::{Range, RangeInclusive};
use std::sync::Arc;

use rope::{OffsetUtf16, Point, Rope};
use unicode_segmentation::UnicodeSegmentation;

use super::tokenizer::{Token, tokenize_line};

/// Defines the two-space YAML indent.
pub const INDENT: &str = "  ";

/// Byte offset of the last grapheme boundary at or before `byte`.
pub fn grapheme_start(text: &str, byte: usize) -> usize {
    let byte = floor_char_boundary(text, byte);
    let start = last_grapheme_start(text, byte);
    // The prefix scan stops before `byte`, so check whether a grapheme starts there.
    if text[start..]
        .graphemes(true)
        .next()
        .is_some_and(|grapheme| start + grapheme.len() == byte)
    {
        byte
    } else {
        start
    }
}

/// Byte offset of the grapheme boundary before `byte`, for caret movement.
pub fn previous_grapheme_start(text: &str, byte: usize) -> usize {
    last_grapheme_start(text, floor_char_boundary(text, byte))
}

fn last_grapheme_start(text: &str, byte: usize) -> usize {
    text[..byte]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(index, _)| index)
}

/// Byte offset of the first grapheme boundary at or after `byte`.
pub fn grapheme_end(text: &str, byte: usize) -> usize {
    let byte = floor_char_boundary(text, byte);
    text[byte..]
        .grapheme_indices(true)
        .next()
        .map_or(text.len(), |(index, grapheme)| {
            byte + index + grapheme.len()
        })
}

fn floor_char_boundary(text: &str, mut byte: usize) -> usize {
    byte = byte.min(text.len());
    while !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

/// Characters that end a word. YAML punctuation is included so word-wise motion lands on
/// a value instead of on its key separator.
const WORD_BREAK: &str = ":-{}[],&*!#|>%@\"'\\?/=~+";

fn is_word_break(ch: char) -> bool {
    ch.is_whitespace() || WORD_BREAK.contains(ch)
}

/// Bytes of indentation an outdent removes: at most one step, and only leading spaces.
fn outdent_width(text: &str) -> usize {
    text.chars()
        .take(INDENT.len())
        .take_while(|ch| *ch == ' ')
        .map(char::len_utf8)
        .sum()
}

/// Start of the word before `byte`, skipping the separators in front of it.
fn previous_word_start(text: &str, byte: usize) -> usize {
    let mut start = floor_char_boundary(text, byte);
    while start > 0 {
        let Some((index, ch)) = text[..start].char_indices().next_back() else {
            break;
        };
        if !is_word_break(ch) {
            break;
        }
        start = index;
    }
    while start > 0 {
        let Some((index, ch)) = text[..start].char_indices().next_back() else {
            break;
        };
        if is_word_break(ch) {
            break;
        }
        start = index;
    }
    start
}

/// Start of the word after `byte`. Inside a word the caret stops at its end, on a
/// separator it stops at the end of the next word.
fn next_word_start(text: &str, byte: usize) -> usize {
    let mut index = floor_char_boundary(text, byte);
    let on_break = text[index..].chars().next().is_none_or(is_word_break);
    if !on_break {
        while index < text.len() {
            let ch = text[index..].chars().next().expect("index is in bounds");
            if is_word_break(ch) {
                break;
            }
            index += ch.len_utf8();
        }
        return index;
    }
    while index < text.len() {
        let ch = text[index..].chars().next().expect("index is in bounds");
        if !is_word_break(ch) {
            break;
        }
        index += ch.len_utf8();
    }
    index
}

/// One edit at the start of a line: how many bytes to remove, how many to add, and the
/// bytes that take their place.
///
/// A block operation replaces `remove` bytes at the start of the line with `text`, which
/// is `insert` bytes long. The two counts are kept next to the text so the caret can be
/// moved by the same amount without measuring the line twice.
struct LineEdit {
    remove: usize,
    insert: usize,
    text: String,
}

impl LineEdit {
    /// Keeps `remove` bytes and puts `text` in front of them.
    fn insert(text: &str) -> Self {
        Self {
            remove: 0,
            insert: text.len(),
            text: text.to_owned(),
        }
    }

    /// Drops `remove` leading bytes.
    fn remove(remove: usize) -> Self {
        Self {
            remove,
            insert: 0,
            text: String::new(),
        }
    }
}

/// Caches one line's text, tokens, character count, and grid width.
///
/// The text is shared so a background search can snapshot the document without copying
/// every line again. The tokens are tokenised on first use: loading a document must not
/// tokenise every line on the UI thread, and only the visible rows are ever painted.
#[derive(Debug)]
pub struct Line {
    text: Arc<str>,
    tokens: OnceCell<Vec<Token>>,
    char_len: usize,
    cells: usize,
}

impl Line {
    fn new(text: &str) -> Self {
        Self {
            text: Arc::from(text),
            tokens: OnceCell::new(),
            char_len: text.chars().count(),
            cells: text.chars().map(super::search::char_cells).sum(),
        }
    }

    /// Builds a line whose tokens are already known, used by the incremental edit path.
    fn tokenized(text: &str) -> Self {
        let line = Self::new(text);
        let _ = line.tokens.set(tokenize_line(text));
        line
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// The shared line text, cheap to clone into a background snapshot.
    pub fn shared_text(&self) -> Arc<str> {
        self.text.clone()
    }

    pub fn tokens(&self) -> &[Token] {
        self.tokens.get_or_init(|| tokenize_line(&self.text))
    }

    pub fn char_len(&self) -> usize {
        self.char_len
    }

    /// Character cells the line occupies, counting fullwidth characters as two.
    pub fn cells(&self) -> usize {
        self.cells
    }

    /// Leading whitespace width in bytes.
    pub fn indent_len(&self) -> usize {
        self.text.len() - self.text.trim_start_matches([' ', '\t']).len()
    }
}

impl Clone for Line {
    fn clone(&self) -> Self {
        Self {
            text: self.text.clone(),
            // A cloned line is only used for snapshots, so the tokens are not shared.
            tokens: OnceCell::new(),
            char_len: self.char_len,
            cells: self.cells,
        }
    }
}

impl AsRef<str> for Line {
    fn as_ref(&self) -> &str {
        &self.text
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Coalesce {
    None,
    Insert { end: usize },
    Delete { start: usize },
}

#[derive(Clone, Debug)]
struct Change {
    range: Range<usize>,
    removed: String,
    inserted: String,
    cursor_before: usize,
    anchor_before: usize,
    cursor_after: usize,
    anchor_after: usize,
}

/// Limits how many bytes can merge into one undo step.
const COALESCE_LIMIT: usize = 128;

pub struct EditBuffer {
    rope: Rope,
    lines: Vec<Line>,
    /// Lines touched since the document was loaded or last saved, for the change gutter.
    modified: Vec<bool>,
    longest: usize,
    longest_chars: usize,
    cursor: usize,
    anchor: usize,
    desired_column: Option<usize>,
    undo: Vec<Vec<Change>>,
    redo: Vec<Vec<Change>>,
    /// The saved undo depth. `usize::MAX` means the save point is invalid.
    saved_depth: usize,
    coalesce: Coalesce,
    /// While set, every edit appends to the same undo group, so a replace-all is one step.
    grouping: bool,
}

impl EditBuffer {
    pub fn new(text: &str) -> Self {
        let mut buffer = Self {
            rope: Rope::new(),
            lines: Vec::new(),
            modified: Vec::new(),
            longest: 0,
            longest_chars: 0,
            cursor: 0,
            anchor: 0,
            desired_column: None,
            undo: Vec::new(),
            redo: Vec::new(),
            saved_depth: 0,
            coalesce: Coalesce::None,
            grouping: false,
        };
        buffer.set_text(text);
        buffer
    }

    /// Replaces the document and resets editing state.
    pub fn set_text(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.rope = Rope::from(normalized.as_str());
        self.lines = normalized.split('\n').map(Line::new).collect();
        if self.lines.is_empty() {
            self.lines.push(Line::new(""));
        }
        self.modified = vec![false; self.lines.len()];
        self.cursor = 0;
        self.anchor = 0;
        self.desired_column = None;
        self.undo.clear();
        self.redo.clear();
        self.saved_depth = 0;
        self.coalesce = Coalesce::None;
        self.grouping = false;
        self.recompute_longest();
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    pub fn byte_len(&self) -> usize {
        self.rope.len()
    }

    pub fn clip_range(&self, range: Range<usize>) -> Range<usize> {
        let start = self.clip(range.start);
        let end = self.clip(range.end);
        if start <= end { start..end } else { end..start }
    }

    pub fn slice(&self, range: Range<usize>) -> String {
        self.rope.slice(range).to_string()
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    pub fn line(&self, row: usize) -> &Line {
        &self.lines[row]
    }

    pub fn longest_line(&self) -> usize {
        self.longest
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn anchor(&self) -> usize {
        self.anchor
    }

    pub fn selection(&self) -> Option<Range<usize>> {
        (self.cursor != self.anchor)
            .then(|| self.cursor.min(self.anchor)..self.cursor.max(self.anchor))
    }

    /// Returns the cursor line and byte offset within that line.
    pub fn cursor_in_line(&self) -> (usize, usize) {
        let row = self.row_for_offset(self.cursor);
        (row, self.cursor - self.line_start(row))
    }

    /// Returns the cursor character column for vertical movement.
    pub fn cursor_char_column(&self) -> usize {
        let (row, offset) = self.cursor_in_line();
        self.lines[row].text[..offset].chars().count()
    }

    pub fn is_dirty(&self) -> bool {
        self.undo.len() != self.saved_depth
    }

    pub fn mark_saved(&mut self) {
        self.saved_depth = self.undo.len();
        self.coalesce = Coalesce::None;
        self.clear_modified();
    }

    /// True when the line was touched since the document was loaded or last saved.
    pub fn line_is_modified(&self, row: usize) -> bool {
        self.modified.get(row).copied().unwrap_or(false)
    }

    pub fn line_start(&self, row: usize) -> usize {
        self.rope.point_to_offset(Point::new(row as u32, 0))
    }

    pub fn line_end(&self, row: usize) -> usize {
        self.line_start(row) + self.lines[row].text.len()
    }

    pub fn row_for_offset(&self, offset: usize) -> usize {
        self.rope.offset_to_point(offset.min(self.rope.len())).row as usize
    }

    pub fn set_cursor(&mut self, offset: usize, extend: bool) {
        let offset = self.clip(offset);
        self.cursor = offset;
        if !extend {
            self.anchor = offset;
        }
        self.desired_column = None;
        self.coalesce = Coalesce::None;
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.rope.len();
        self.desired_column = None;
        self.coalesce = Coalesce::None;
    }

    /// Converts a byte offset to UTF-16.
    pub fn byte_to_utf16(&self, byte: usize) -> usize {
        self.rope.offset_to_offset_utf16(self.clip(byte)).0
    }

    pub fn utf16_to_byte(&self, utf16: usize) -> usize {
        let mut byte = self.rope.offset_utf16_to_offset(OffsetUtf16(utf16));
        if self.byte_to_utf16(byte) > utf16 {
            byte = byte.saturating_sub(
                self.rope
                    .reversed_chars_at(byte)
                    .next()
                    .map_or(0, char::len_utf8),
            );
        }
        self.rope.floor_char_boundary(byte.min(self.rope.len()))
    }

    pub fn text_len_utf16(&self) -> usize {
        self.byte_to_utf16(self.byte_len())
    }

    pub fn slice_utf16(&self, range: Range<usize>) -> (String, Range<usize>) {
        let start = self.utf16_to_byte(range.start);
        let end = self.utf16_to_byte(range.end).max(start);
        (
            self.slice(start..end),
            self.byte_to_utf16(start)..self.byte_to_utf16(end),
        )
    }

    /// Replaces a range without merging the edit into an undo group.
    pub fn replace(&mut self, range: Range<usize>, text: &str) -> bool {
        if range.is_empty() && text.is_empty() {
            return false;
        }
        let range = range.start.min(range.end)..range.start.max(range.end);
        let Some(change) = self.edit_range(range, text) else {
            return false;
        };
        self.record(change, Merge::None);
        true
    }

    pub fn insert(&mut self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        let range = self.selection().unwrap_or(self.cursor..self.cursor);
        let single_char = !text.contains('\n') && text.chars().count() == 1;
        let Some(change) = self.edit_range(range, text) else {
            return false;
        };
        let merge = if single_char {
            Merge::Typing
        } else {
            Merge::None
        };
        self.record(change, merge);
        true
    }

    /// Inserts a newline and copies the current indentation.
    pub fn insert_newline(&mut self) -> bool {
        let row = self.row_for_offset(self.cursor);
        let text = {
            let line = self.lines[row].text();
            let indent_len = line.len() - line.trim_start_matches([' ', '\t']).len();
            let mut text = String::from("\n");
            text.push_str(&line[..indent_len]);
            if line.trim_end().ends_with(':') {
                text.push_str(INDENT);
            }
            text
        };
        self.insert(&text)
    }

    /// Byte range of the current line plus the cursor offset inside it.
    fn cursor_line(&self) -> (usize, usize, &str) {
        let row = self.row_for_offset(self.cursor);
        let start = self.line_start(row);
        (start, self.cursor - start, &self.lines[row].text)
    }

    pub fn backspace(&mut self) -> bool {
        if let Some(selection) = self.selection() {
            let Some(change) = self.edit_range(selection, "") else {
                return false;
            };
            self.record(change, Merge::None);
            return true;
        }
        let (start, offset, text) = self.cursor_line();
        if offset == 0 {
            return false;
        }
        // Delete the whole grapheme: a combining mark or a joined emoji is one unit.
        let range = start + previous_grapheme_start(text, offset)..self.cursor;
        let removed = self.rope.slice(range.clone()).to_string();
        let Some(change) = self.edit_range(range, "") else {
            return false;
        };
        let merge = if removed == "\n" {
            Merge::None
        } else {
            Merge::Backspace
        };
        self.record(change, merge);
        true
    }

    pub fn delete_forward(&mut self) -> bool {
        if let Some(selection) = self.selection() {
            let Some(change) = self.edit_range(selection, "") else {
                return false;
            };
            self.record(change, Merge::None);
            return true;
        }
        let (start, offset, text) = self.cursor_line();
        if offset >= text.len() {
            return false;
        }
        let range = self.cursor..start + grapheme_end(text, offset);
        let Some(change) = self.edit_range(range, "") else {
            return false;
        };
        self.record(change, Merge::None);
        true
    }

    pub fn delete_selection(&mut self) -> bool {
        let Some(selection) = self.selection() else {
            return false;
        };
        let Some(change) = self.edit_range(selection, "") else {
            return false;
        };
        self.record(change, Merge::None);
        true
    }

    pub fn move_horizontal(&mut self, delta: isize, extend: bool) {
        if !extend && let Some(selection) = self.selection() {
            self.set_cursor(
                if delta < 0 {
                    selection.start
                } else {
                    selection.end
                },
                false,
            );
            return;
        }
        let (start, offset, text) = self.cursor_line();
        let end = start + text.len();
        let next = if delta < 0 {
            if offset == 0 {
                // Step over the newline into the previous line.
                self.cursor.saturating_sub(1)
            } else {
                start + previous_grapheme_start(text, offset)
            }
        } else if offset >= text.len() {
            if end < self.byte_len() {
                end + 1
            } else {
                self.cursor
            }
        } else {
            start + grapheme_end(text, offset)
        };
        self.set_cursor(next, extend);
    }

    pub fn move_vertical(&mut self, delta: isize, extend: bool) {
        let (row, _) = self.cursor_in_line();
        let column = self
            .desired_column
            .unwrap_or_else(|| self.cursor_char_column());
        self.desired_column = Some(column);
        let last = self.lines.len().saturating_sub(1) as isize;
        let target = (row as isize + delta).clamp(0, last) as usize;
        let offset = self.offset_for_row_column(target, column);
        self.cursor = offset;
        if !extend {
            self.anchor = offset;
        }
        self.coalesce = Coalesce::None;
    }

    pub fn move_page(&mut self, pages: isize, rows: usize, extend: bool) {
        let delta = pages * rows.max(1) as isize;
        self.move_vertical(delta, extend);
    }

    pub fn move_home(&mut self, extend: bool) {
        let row = self.row_for_offset(self.cursor);
        self.set_cursor(self.line_start(row), extend);
    }

    pub fn move_end(&mut self, extend: bool) {
        let row = self.row_for_offset(self.cursor);
        self.set_cursor(self.line_end(row), extend);
    }

    /// Smart home: the first press lands on the first non-whitespace character, a second
    /// press on the same column reaches the real start of the line.
    pub fn move_line_start(&mut self, extend: bool) {
        let row = self.row_for_offset(self.cursor);
        let first = self.line_start(row) + self.lines[row].indent_len();
        if self.cursor == first {
            self.move_home(extend);
        } else {
            self.set_cursor(first, extend);
        }
    }

    /// Smart end: the first press lands after the last non-whitespace character, a second
    /// press on the same column reaches the real end of the line.
    pub fn move_line_end(&mut self, extend: bool) {
        let row = self.row_for_offset(self.cursor);
        let trimmed = self.line_start(row) + self.lines[row].text().trim_end().len();
        if self.cursor == trimmed {
            self.move_end(extend);
        } else {
            self.set_cursor(trimmed, extend);
        }
    }

    pub fn move_document_start(&mut self, extend: bool) {
        self.set_cursor(0, extend);
    }

    pub fn move_document_end(&mut self, extend: bool) {
        self.set_cursor(self.byte_len(), extend);
    }

    /// Word-wise motion. A word is a run of characters that are neither whitespace nor
    /// YAML punctuation, so `image: nginx:1.2` moves between `image`, `nginx`, and `1.2`.
    pub fn move_word(&mut self, delta: isize, extend: bool) {
        if !extend
            && delta < 0
            && let Some(selection) = self.selection()
        {
            self.set_cursor(selection.start, false);
            return;
        }
        let (start, offset, text) = self.cursor_line();
        let next = if delta < 0 {
            if offset == 0 {
                self.cursor.saturating_sub(1)
            } else {
                start + previous_word_start(text, offset)
            }
        } else {
            let end = start + text.len();
            if offset >= text.len() {
                if end < self.byte_len() {
                    end + 1
                } else {
                    self.cursor
                }
            } else {
                start + next_word_start(text, offset)
            }
        };
        self.set_cursor(next, extend);
    }

    /// Byte range of the word around `byte`, for a double click. The word is bounded by
    /// whitespace and YAML punctuation.
    pub fn word_range_at(&self, byte: usize) -> Option<Range<usize>> {
        let row = self.row_for_offset(byte);
        let start = self.line_start(row);
        let end = self.line_end(row);
        if start == end {
            return None;
        }
        let text = self.lines[row].text();
        let byte = floor_char_boundary(text, byte.clamp(start, end) - start);
        let is_word = |offset: usize| {
            text[offset..]
                .chars()
                .next()
                .is_some_and(|ch| !is_word_break(ch))
        };
        // A click past the last character of a line still selects the word before it.
        let probe = if byte == text.len() && byte > 0 {
            previous_grapheme_start(text, byte)
        } else {
            byte
        };
        if !is_word(probe) {
            return None;
        }
        // Stepping by graphemes keeps the walks off a UTF-8 boundary.
        let mut first = byte;
        while first > 0 && !is_word(first) {
            first = previous_grapheme_start(text, first);
        }
        while first > 0 && is_word(previous_grapheme_start(text, first)) {
            first = previous_grapheme_start(text, first);
        }
        let mut last = byte;
        while last < text.len() && is_word(last) {
            last = grapheme_end(text, last);
        }
        Some(start + first..start + last)
    }

    /// Byte range of a line without its trailing newline, for a triple click.
    pub fn line_text_range(&self, row: usize) -> Range<usize> {
        let start = self.line_start(row);
        start..self.line_end(row)
    }

    /// The last row that holds a line. A document that ends with a newline has an empty
    /// row after its last line, and that row is not a line a block operation can reach.
    fn last_line(&self) -> usize {
        let last = self.lines.len().saturating_sub(1);
        if last > 0 && self.lines[last].text().is_empty() {
            last - 1
        } else {
            last
        }
    }

    /// Whether the document's last byte is a newline.
    fn ends_with_newline(&self) -> bool {
        self.rope.ends_with("\n")
    }

    /// Rows a line operation applies to: the selected lines, or the caret line. The range
    /// is inclusive, so `end` is the last row the operation touches.
    fn target_rows(&self) -> RangeInclusive<usize> {
        let last = self.last_line();
        let caret = self.row_for_offset(self.cursor);
        let Some(selection) = self.selection() else {
            return caret..=caret;
        };
        let first = self.row_for_offset(selection.start);
        let end_row = self.row_for_offset(selection.end);
        // A selection that stops at the start of a line does not include that line.
        let end = if end_row > first && selection.end == self.line_start(end_row) {
            end_row - 1
        } else {
            end_row
        };
        first..=end.min(last)
    }

    /// Applies one edit at the start of every row and keeps the selection on the same
    /// text: the caret follows the bytes added or removed before it.
    ///
    /// The whole block is one undo step, so a single Tab undoes the whole indent.
    fn edit_line_starts(
        &mut self,
        rows: RangeInclusive<usize>,
        mut edit: impl FnMut(&str) -> LineEdit,
    ) -> bool {
        let anchor = self.line_position(self.anchor);
        let cursor = self.line_position(self.cursor);
        let mut deltas: Vec<(usize, usize, usize)> = Vec::new();
        let changed = self.grouped(|buffer| {
            for row in rows {
                // The bytes come from the line as it is now, because the rows above the
                // edit have already shifted.
                let line = edit(buffer.lines[row].text());
                if line.remove == 0 && line.insert == 0 {
                    continue;
                }
                let start = buffer.line_start(row);
                if buffer.replace(start..start + line.remove, &line.text) {
                    deltas.push((row, line.insert, line.remove));
                }
            }
            !deltas.is_empty()
        });
        if changed {
            self.restore_positions(anchor, cursor, &deltas);
        }
        changed
    }

    /// Runs `edit` with every change collected into one undo group, and drops the group
    /// again when nothing changed.
    fn grouped(&mut self, edit: impl FnOnce(&mut Self) -> bool) -> bool {
        self.grouping = true;
        self.undo.push(Vec::new());
        let changed = edit(self);
        self.grouping = false;
        if !changed || self.undo.last().is_some_and(Vec::is_empty) {
            self.undo.pop();
        }
        changed
    }

    /// Caret position as a row plus an offset inside that row.
    fn line_position(&self, offset: usize) -> (usize, usize) {
        let row = self.row_for_offset(offset);
        (row, offset - self.line_start(row))
    }

    /// Rebuilds the selection after `edit_line_starts`, using the byte counts each line
    /// gained or lost.
    fn restore_positions(
        &mut self,
        anchor: (usize, usize),
        cursor: (usize, usize),
        deltas: &[(usize, usize, usize)],
    ) {
        let map =
            |(row, offset): (usize, usize)| match deltas.iter().find(|(edited, ..)| *edited == row)
            {
                // An offset inside the removed bytes lands on the first byte the edit
                // left in front of it; an offset at the edit point moves with the edit.
                Some((_, insert, remove)) => (
                    row,
                    if offset < *remove {
                        0
                    } else {
                        offset - remove + insert
                    },
                ),
                None => (row, offset),
            };
        let anchor = map(anchor);
        let cursor = map(cursor);
        self.anchor = self.line_start(anchor.0) + anchor.1;
        self.cursor = self.line_start(cursor.0) + cursor.1;
        self.desired_column = None;
    }

    /// Adds one indent step to every target line, or to the caret line.
    pub fn indent(&mut self) -> bool {
        self.edit_line_starts(self.target_rows(), |_| LineEdit::insert(INDENT))
    }

    /// Removes up to one indent step from every target line.
    pub fn outdent(&mut self) -> bool {
        self.edit_line_starts(self.target_rows(), |text| {
            LineEdit::remove(outdent_width(text))
        })
    }

    /// Inserts an empty line above or below the caret line and moves the caret to it.
    pub fn insert_line(&mut self, below: bool) -> bool {
        let row = self.row_for_offset(self.cursor);
        let length = self.byte_len();
        let offset = if below {
            // A last line without a newline needs one before the new line can start.
            (self.line_end(row) + 1).min(length)
        } else {
            self.line_start(row)
        };
        self.replace(offset..offset, "\n");
        let target = if below { row + 1 } else { row };
        let row = target.min(self.lines.len().saturating_sub(1));
        self.set_cursor(self.line_start(row), false);
        true
    }

    /// Copies the caret line, or every selected line, right below the selection.
    pub fn duplicate_lines(&mut self) -> bool {
        let rows = self.target_rows();
        let block: String = rows
            .clone()
            .map(|row| format!("{}\n", self.lines[row].text()))
            .collect();
        let last = self.lines.len().saturating_sub(1);
        // A last line without a newline needs one before the copy can start, and the copy
        // must not add a trailing newline the document did not have.
        let (offset, text) = if self.line_end(*rows.end()) + 1 > self.byte_len() {
            (
                self.byte_len(),
                format!("\n{}", block.trim_end_matches('\n')),
            )
        } else {
            (self.line_end(*rows.end()) + 1, block)
        };
        self.replace(offset..offset, &text);
        let row = (*rows.end() + 1).min(self.lines.len().saturating_sub(1));
        self.set_cursor(self.line_start(row.min(last)), false);
        true
    }

    /// Moves the caret line, or every selected line, one row up or down.
    ///
    /// The block rotates with its neighbour, so moving several lines is one edit and one
    /// undo step.
    pub fn move_lines(&mut self, delta: isize) -> bool {
        let rows = self.target_rows();
        let (first, final_row) = (*rows.start(), *rows.end());
        let last = self.last_line();
        if (delta < 0 && first == 0) || (delta > 0 && final_row >= last) {
            return false;
        }
        let neighbor = if delta < 0 { first - 1 } else { final_row + 1 };
        // The offsets are read before the edit, while they still address the old text.
        let (anchor, cursor) = (self.anchor, self.cursor);
        // The byte just past a row, which is the start of the next one or the document end.
        let region = if delta < 0 {
            let end = if final_row < last {
                self.line_start(final_row + 1)
            } else {
                self.byte_len()
            };
            self.line_start(neighbor)..end
        } else {
            let end = if neighbor < last {
                self.line_start(neighbor + 1)
            } else {
                self.byte_len()
            };
            self.line_start(first)..end
        };
        let span = if delta < 0 {
            neighbor..=final_row
        } else {
            first..=final_row + 1
        };
        let mut lines: Vec<String> = span.map(|row| self.lines[row].text().to_owned()).collect();
        // Moving down swaps the block with the single line below it, so the line that
        // moves is the neighbour rather than the first line of the block.
        if delta < 0 {
            let moved = lines.remove(0);
            lines.push(moved);
        } else {
            let moved = lines.pop().expect("the span always has a neighbour line");
            lines.insert(0, moved);
        }
        let mut text: String = lines.iter().map(|line| format!("{line}\n")).collect();
        if region.end >= self.byte_len() && !self.ends_with_newline() {
            // The document has no trailing newline, so the rotation must not add one.
            text.pop();
        }
        self.replace(region.start..region.end, &text);
        // Every byte of the block moved by the same distance, so the caret follows the
        // text it was on and keeps its place inside the block.
        let moved = first as isize + delta;
        let shift = if moved >= 0 {
            self.line_start(moved as usize) as isize - self.line_start(first) as isize
        } else {
            0
        };
        let limit = self.byte_len() as isize;
        let anchor = (anchor as isize + shift).clamp(0, limit) as usize;
        let cursor = (cursor as isize + shift).clamp(0, limit) as usize;
        // Anchor first, then the caret, so the selection direction survives.
        self.set_cursor(anchor, false);
        if cursor != anchor {
            self.set_cursor(cursor, true);
        }
        true
    }

    /// Deletes the caret line, or every selected line, and exactly one newline.
    ///
    /// The block takes the newline in front of it, so the line behind it moves up into
    /// its place. A block that starts at the top of the document has no newline in front
    /// of it and takes the one behind instead, which also covers a last line that has no
    /// newline of its own: deleting it leaves no empty line behind.
    pub fn delete_lines(&mut self) -> bool {
        let rows = self.target_rows();
        let (first, final_row) = (*rows.start(), *rows.end());
        let (start, end) = if first > 0 {
            (self.line_end(first - 1), self.line_end(final_row))
        } else {
            // The trailing empty line of a document that ends with a newline is a row, so
            // the byte past the block is the document end unless another line follows.
            let end = if final_row + 1 < self.lines.len() {
                self.line_start(final_row + 1)
            } else {
                self.byte_len()
            };
            (self.line_start(first), end)
        };
        self.replace(start..end, "");
        let row = first.min(self.lines.len().saturating_sub(1));
        self.set_cursor(self.line_start(row), false);
        true
    }

    /// Replaces several ranges in one undo step, for replace-all.
    ///
    /// The ranges are document byte ranges ordered from the start of the document, and
    /// they are applied in reverse so an earlier offset stays valid while the later ones
    /// change.
    pub fn replace_all(&mut self, replacements: &[(Range<usize>, String)]) -> bool {
        if replacements.is_empty() {
            return false;
        }
        let changed = self.grouped(|buffer| {
            for (range, text) in replacements.iter().rev() {
                if let Some(change) = buffer.edit_range(range.clone(), text) {
                    buffer.record(change, Merge::None);
                }
            }
            !buffer.undo.last().is_some_and(Vec::is_empty)
        });
        if !changed {
            return false;
        }
        if !self.is_dirty() {
            self.clear_modified();
        }
        let (first, text) = &replacements[0];
        let end = (first.start + text.len()).min(self.byte_len());
        self.set_cursor(end, false);
        true
    }

    pub fn undo(&mut self) -> bool {
        let Some(group) = self.undo.pop() else {
            return false;
        };
        for change in group.iter().rev() {
            let range = change.range.start..change.range.start + change.inserted.len();
            self.replace_range(range, &change.removed);
        }
        if let Some(first) = group.first() {
            self.cursor = first.cursor_before;
            self.anchor = first.anchor_before;
        }
        self.redo.push(group);
        self.after_history();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(group) = self.redo.pop() else {
            return false;
        };
        for change in &group {
            let range = change.range.start..change.range.start + change.removed.len();
            self.replace_range(range, &change.inserted);
        }
        if let Some(last) = group.last() {
            self.cursor = last.cursor_after;
            self.anchor = last.anchor_after;
        }
        self.undo.push(group);
        self.after_history();
        true
    }

    fn after_history(&mut self) {
        self.desired_column = None;
        self.coalesce = Coalesce::None;
        if !self.is_dirty() {
            self.clear_modified();
        }
    }

    fn clip(&self, offset: usize) -> usize {
        self.rope.floor_char_boundary(offset.min(self.rope.len()))
    }

    fn offset_for_row_column(&self, row: usize, column: usize) -> usize {
        let line = self.lines[row].text();
        let mut offset = self.line_start(row);
        for (seen, ch) in line.chars().enumerate() {
            if seen == column {
                break;
            }
            offset += ch.len_utf8();
        }
        offset
    }

    /// Applies one replacement and records the resulting cursor state.
    fn edit_range(&mut self, range: Range<usize>, text: &str) -> Option<Change> {
        let removed = self.rope.slice(range.clone()).to_string();
        if removed == text {
            return None;
        }
        let cursor_before = self.cursor;
        let anchor_before = self.anchor;
        self.replace_range(range.clone(), text);
        let end = range.start + text.len();
        self.cursor = end;
        self.anchor = end;
        Some(Change {
            range,
            removed,
            inserted: text.to_owned(),
            cursor_before,
            anchor_before,
            cursor_after: end,
            anchor_after: end,
        })
    }

    fn record(&mut self, change: Change, merge: Merge) {
        if !self.redo.is_empty() {
            self.redo.clear();
            self.saved_depth = usize::MAX;
        }
        let continues = match merge {
            Merge::None => false,
            Merge::Typing => {
                matches!(self.coalesce, Coalesce::Insert { end } if end == change.range.start)
                    && change.range.is_empty()
                    && !change.inserted.contains('\n')
            }
            Merge::Backspace => {
                matches!(self.coalesce, Coalesce::Delete { start } if start == change.range.end)
                    && change.inserted.is_empty()
                    && !change.removed.contains('\n')
            }
        };
        let mergeable = continues
            && self.undo.last().is_some_and(|group| {
                group.last().is_some_and(|last| match merge {
                    Merge::Typing => last.inserted.len() + change.inserted.len() <= COALESCE_LIMIT,
                    Merge::Backspace => last.removed.len() + change.removed.len() <= COALESCE_LIMIT,
                    Merge::None => false,
                })
            });
        if mergeable {
            let last = self
                .undo
                .last_mut()
                .and_then(|group| group.last_mut())
                .expect("a mergeable edit must have an undo group");
            match merge {
                Merge::Typing => {
                    last.inserted.push_str(&change.inserted);
                }
                Merge::Backspace => {
                    last.removed = format!("{}{}", change.removed, last.removed);
                    last.range.start = change.range.start;
                }
                Merge::None => {}
            }
            last.cursor_after = change.cursor_after;
            last.anchor_after = change.anchor_after;
        } else if self.grouping {
            self.undo
                .last_mut()
                .expect("a grouped edit must have an open undo group")
                .push(change.clone());
        } else {
            self.undo.push(vec![change.clone()]);
        }
        self.coalesce = match merge {
            Merge::Typing => Coalesce::Insert {
                end: change.cursor_after,
            },
            Merge::Backspace => Coalesce::Delete {
                start: change.range.start,
            },
            Merge::None => Coalesce::None,
        };
    }

    /// Replaces rope text and refreshes tokens for changed lines.
    fn replace_range(&mut self, range: Range<usize>, text: &str) {
        if range.is_empty() && text.is_empty() {
            return;
        }
        let start_row = self.row_for_offset(range.start);
        let old_end_row = self.row_for_offset(range.end);
        self.rope.replace(range.clone(), text);
        let new_end = range.start + text.len();
        let new_end_row = self.row_for_offset(new_end);

        let mut new_lines = Vec::with_capacity(new_end_row - start_row + 1);
        let mut best = (0usize, 0usize);
        for row in start_row..=new_end_row {
            let line = Line::tokenized(&self.line_text_at(row));
            if line.char_len() > best.0 {
                best = (line.char_len(), row - start_row);
            }
            new_lines.push(line);
        }

        let old_count = old_end_row - start_row + 1;
        let new_count = new_lines.len();
        let longest_affected = self.longest >= start_row && self.longest < start_row + old_count;
        if self.longest >= start_row + old_count {
            self.longest = self.longest + new_count - old_count;
        }
        self.lines.splice(start_row..=old_end_row, new_lines);
        // Every row the edit touched counts as changed, and the rows after it shift with
        // the splice, so the flag always has one entry per line.
        self.modified
            .splice(start_row..=old_end_row, vec![true; new_count]);
        if self.modified.len() < self.lines.len() {
            self.modified.resize(self.lines.len(), false);
        }
        if best.0 > self.longest_chars {
            self.longest_chars = best.0;
            self.longest = start_row + best.1;
        } else if longest_affected {
            self.recompute_longest();
        }
    }

    /// Clears the change flags when the document is back at its saved state.
    fn clear_modified(&mut self) {
        if self.modified.iter().any(|flag| *flag) {
            self.modified.iter_mut().for_each(|flag| *flag = false);
        }
    }

    fn line_text_at(&self, row: usize) -> String {
        let start = self.line_start(row);
        let end = self.line_start(row + 1);
        let mut text = self.rope.slice(start..end).to_string();
        if text.ends_with('\n') {
            text.pop();
        }
        text
    }

    fn recompute_longest(&mut self) {
        let mut best = (0usize, 0usize);
        for (row, line) in self.lines.iter().enumerate() {
            if line.char_len() > best.0 {
                best = (line.char_len(), row);
            }
        }
        self.longest_chars = best.0;
        self.longest = best.1;
    }
}

#[derive(Clone, Copy)]
enum Merge {
    None,
    Typing,
    Backspace,
}

#[cfg(test)]
mod tests {
    use super::{EditBuffer, INDENT};

    fn text_of(buffer: &EditBuffer) -> String {
        buffer.text()
    }

    fn move_to_document_end(buffer: &mut EditBuffer) {
        let end = buffer.text().len();
        buffer.set_cursor(end, false);
    }

    #[test]
    fn insert_and_backspace_roundtrip() {
        let mut buffer = EditBuffer::new("name: app");
        move_to_document_end(&mut buffer);
        buffer.insert("x");
        buffer.insert("y");
        assert_eq!(text_of(&buffer), "name: appxy");
        buffer.backspace();
        buffer.backspace();
        assert_eq!(text_of(&buffer), "name: app");
    }

    #[test]
    fn typing_run_undoes_as_one_step() {
        let mut buffer = EditBuffer::new("");
        buffer.insert("a");
        buffer.insert("b");
        buffer.insert("c");
        assert_eq!(text_of(&buffer), "abc");
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "");
        assert!(buffer.redo());
        assert_eq!(text_of(&buffer), "abc");
    }

    #[test]
    fn backspace_run_undoes_as_one_step() {
        let mut buffer = EditBuffer::new("abc");
        buffer.move_end(false);
        buffer.backspace();
        buffer.backspace();
        buffer.backspace();
        assert_eq!(text_of(&buffer), "");
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "abc");
        assert!(buffer.redo());
        assert_eq!(text_of(&buffer), "");
    }

    #[test]
    fn movement_breaks_typing_coalescing() {
        let mut buffer = EditBuffer::new("");
        buffer.insert("a");
        buffer.move_horizontal(-1, false);
        buffer.insert("b");
        assert_eq!(text_of(&buffer), "ba");
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "a");
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "");
    }

    #[test]
    fn enter_auto_indents_after_colon() {
        let mut buffer = EditBuffer::new("spec:");
        buffer.move_end(false);
        buffer.insert_newline();
        assert_eq!(text_of(&buffer), format!("spec:\n{INDENT}"));
    }

    #[test]
    fn enter_keeps_existing_indent() {
        let mut buffer = EditBuffer::new("spec:\n  replicas: 3");
        move_to_document_end(&mut buffer);
        buffer.insert_newline();
        assert_eq!(text_of(&buffer), "spec:\n  replicas: 3\n  ");
    }

    #[test]
    fn caret_and_backspace_step_over_whole_graphemes() {
        // One base letter plus a combining mark, then a joined family emoji.
        let text = "e\u{301}x👨\u{200d}👩\u{200d}👧";
        let mut buffer = EditBuffer::new(text);
        move_to_document_end(&mut buffer);
        assert_eq!(buffer.cursor(), text.len());
        assert!(buffer.backspace(), "backspace deletes the emoji");
        assert_eq!(text_of(&buffer), "e\u{301}x");
        assert!(buffer.backspace(), "backspace deletes the letter");
        assert_eq!(text_of(&buffer), "e\u{301}");
        assert!(
            buffer.backspace(),
            "backspace deletes the letter and the mark"
        );
        assert_eq!(text_of(&buffer), "");

        let mut buffer = EditBuffer::new(text);
        buffer.set_cursor(0, false);
        assert!(
            buffer.delete_forward(),
            "delete removes the letter and the mark"
        );
        assert_eq!(text_of(&buffer), "x👨\u{200d}👩\u{200d}👧");
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), text);

        // Horizontal movement never lands inside a grapheme.
        move_to_document_end(&mut buffer);
        let mut offsets = vec![buffer.cursor()];
        for _ in 0..3 {
            buffer.move_horizontal(-1, false);
            offsets.push(buffer.cursor());
        }
        assert_eq!(offsets, vec![text.len(), 4, 3, 0]);
        for _ in 0..3 {
            buffer.move_horizontal(1, false);
        }
        assert_eq!(buffer.cursor(), text.len());
    }

    #[test]
    fn horizontal_movement_crosses_lines_one_grapheme_at_a_time() {
        let mut buffer = EditBuffer::new("ab\n中x");
        buffer.set_cursor(2, false);
        buffer.move_horizontal(1, false);
        assert_eq!(buffer.cursor(), 3, "the newline is one step");
        buffer.move_horizontal(1, false);
        assert_eq!(buffer.cursor(), 6, "a whole CJK character is one step");
        buffer.move_horizontal(1, false);
        assert_eq!(buffer.cursor(), 7, "the end of the document stays put");
        let mut back = Vec::new();
        for _ in 0..4 {
            buffer.move_horizontal(-1, false);
            back.push(buffer.cursor());
        }
        assert_eq!(back, vec![6, 3, 2, 1], "back over the newline");
        buffer.move_horizontal(-1, false);
        assert_eq!(buffer.cursor(), 0, "the first character has no step back");
    }

    #[test]
    fn selection_replace_and_delete() {
        let mut buffer = EditBuffer::new("apiVersion: v1");
        buffer.select_all();
        buffer.insert("kind: Pod");
        assert_eq!(text_of(&buffer), "kind: Pod");
        buffer.select_all();
        assert!(buffer.delete_selection());
        assert_eq!(text_of(&buffer), "");
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "kind: Pod");
    }

    #[test]
    fn multiline_edit_updates_line_cache_and_longest() {
        let mut buffer = EditBuffer::new("a\nbb\nccc");
        assert_eq!(buffer.line_count(), 3);
        assert_eq!(buffer.longest_line(), 2);
        move_to_document_end(&mut buffer);
        buffer.insert("\nlonger line here");
        assert_eq!(buffer.line_count(), 4);
        assert_eq!(buffer.line(3).text(), "longer line here");
        assert_eq!(buffer.longest_line(), 3);
        buffer.select_all();
        buffer.insert("x");
        assert_eq!(buffer.line_count(), 1);
        assert_eq!(buffer.longest_line(), 0);
    }

    #[test]
    fn crlf_is_normalized_on_set_text() {
        let buffer = EditBuffer::new("a: 1\r\nb: 2\r\n");
        assert_eq!(buffer.line_count(), 3);
        assert_eq!(buffer.line(0).text(), "a: 1");
        assert_eq!(buffer.line(1).text(), "b: 2");
        assert_eq!(buffer.line(2).text(), "");
    }

    #[test]
    fn dirty_semantics_follow_save_depth() {
        let mut buffer = EditBuffer::new("a");
        assert!(!buffer.is_dirty());
        buffer.move_end(false);
        buffer.insert("b");
        assert!(buffer.is_dirty());
        buffer.mark_saved();
        assert!(!buffer.is_dirty());
        buffer.insert("c");
        assert!(buffer.is_dirty());
        assert!(buffer.undo());
        assert!(!buffer.is_dirty());
        assert!(buffer.redo());
        assert!(buffer.is_dirty());
        buffer.mark_saved();
        assert!(!buffer.is_dirty());
        buffer.undo();
        buffer.insert("z");
        assert!(buffer.is_dirty());
        buffer.set_text("fresh");
        assert!(!buffer.is_dirty());
        assert!(!buffer.undo());
    }

    #[test]
    fn vertical_movement_keeps_desired_column() {
        let mut buffer = EditBuffer::new("abcdef\nx\nabcdef");
        buffer.set_cursor(4, false);
        buffer.move_vertical(1, false);
        assert_eq!(buffer.cursor(), 8);
        buffer.move_vertical(1, false);
        assert_eq!(buffer.cursor(), 13);
    }

    #[test]
    fn home_end_and_selection_movement() {
        let mut buffer = EditBuffer::new("one\ntwo");
        buffer.set_cursor(6, false);
        buffer.move_home(false);
        assert_eq!(buffer.cursor(), 4);
        buffer.move_end(false);
        assert_eq!(buffer.cursor(), 7);
        buffer.move_home(true);
        assert_eq!(buffer.selection(), Some(4..7));
        buffer.move_horizontal(-1, false);
        assert_eq!(buffer.selection(), None);
        assert_eq!(buffer.cursor(), 4);
    }

    #[test]
    fn page_movement_clamps_at_document_edges() {
        let mut buffer = EditBuffer::new("1\n2\n3\n4\n5\n6");
        buffer.move_page(1, 4, false);
        assert_eq!(buffer.cursor_in_line().0, 4);
        buffer.move_page(1, 4, false);
        assert_eq!(buffer.cursor_in_line().0, 5);
        buffer.move_page(-1, 4, false);
        assert_eq!(buffer.cursor_in_line().0, 1);
        buffer.move_page(-1, 4, false);
        assert_eq!(buffer.cursor_in_line().0, 0);
    }

    #[test]
    fn edit_after_undo_drops_redo_branch() {
        let mut buffer = EditBuffer::new("a");
        buffer.move_end(false);
        buffer.insert("b");
        assert!(buffer.undo());
        buffer.insert("c");
        assert!(!buffer.redo());
        assert_eq!(text_of(&buffer), "ac");
    }

    #[test]
    fn search_matches_recomputed_from_edited_lines() {
        let mut buffer = EditBuffer::new("name: coredns\nimage: nginx");
        assert_eq!(
            super::super::search::find_matches(buffer.lines(), "coredns").len(),
            1
        );
        buffer.select_all();
        buffer.insert("name: coredns-v2\nimage: coredns");
        let matches = super::super::search::find_matches(buffer.lines(), "coredns");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].line, 0);
        assert_eq!(matches[1].line, 1);
    }

    #[test]
    fn utf16_offsets_and_ranges_map_across_surrogate_pairs() {
        let buffer = EditBuffer::new("a中b𝄞c");
        assert_eq!(buffer.byte_len(), 10);
        assert_eq!(buffer.text_len_utf16(), 6);
        assert_eq!(buffer.byte_to_utf16(0), 0);
        assert_eq!(buffer.byte_to_utf16(1), 1);
        assert_eq!(buffer.byte_to_utf16(4), 2);
        assert_eq!(buffer.byte_to_utf16(5), 3);
        assert_eq!(buffer.byte_to_utf16(9), 5);
        assert_eq!(buffer.byte_to_utf16(10), 6);
        assert_eq!(buffer.utf16_to_byte(2), 4);
        assert_eq!(buffer.utf16_to_byte(3), 5);
        assert_eq!(buffer.utf16_to_byte(4), 5);
        assert_eq!(buffer.utf16_to_byte(5), 9);
        assert_eq!(buffer.utf16_to_byte(99), 10);
        assert_eq!(buffer.slice_utf16(1..2), ("中".to_owned(), 1..2));
        assert_eq!(buffer.slice_utf16(3..5), ("𝄞".to_owned(), 3..5));
        assert_eq!(buffer.slice_utf16(3..4), (String::new(), 3..3));
        assert_eq!(buffer.slice_utf16(4..5), ("𝄞".to_owned(), 3..5));
    }

    #[test]
    fn no_op_edits_report_unchanged() {
        let mut buffer = EditBuffer::new("a");
        assert!(!buffer.undo());
        assert!(!buffer.redo());
        assert!(!buffer.backspace());
        buffer.move_end(false);
        assert!(!buffer.delete_forward());
        buffer.move_home(false);
        assert!(!buffer.delete_selection());
        assert!(!buffer.replace(0..0, ""));
        assert!(!buffer.insert(""));
        buffer.select_all();
        assert!(!buffer.replace(0..1, "a"));
        assert!(!buffer.is_dirty());
        assert_eq!(text_of(&buffer), "a");
    }

    #[test]
    fn plain_replace_never_coalesces() {
        let mut buffer = EditBuffer::new("abc");
        buffer.replace(1..2, "X");
        buffer.replace(1..2, "Y");
        assert_eq!(text_of(&buffer), "aYc");
        assert_eq!(buffer.undo.len(), 2);
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "aXc");
    }

    #[test]
    fn cursor_clips_to_char_boundaries() {
        let mut buffer = EditBuffer::new("中文");
        buffer.set_cursor(1, false);
        assert_eq!(buffer.cursor(), 0);
        buffer.set_cursor(3, false);
        assert_eq!(buffer.cursor(), 3);
        buffer.move_horizontal(1, false);
        assert_eq!(buffer.cursor(), 6);
        buffer.move_horizontal(1, false);
        assert_eq!(buffer.cursor(), 6);
    }

    #[test]
    fn tab_indents_the_caret_line() {
        let mut buffer = EditBuffer::new("spec:\n  replicas: 3");
        buffer.set_cursor(0, false);
        assert!(buffer.indent());
        assert_eq!(text_of(&buffer), "  spec:\n  replicas: 3");
        assert_eq!(buffer.cursor(), 2, "the caret keeps its column");
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "spec:\n  replicas: 3");
    }

    #[test]
    fn tab_indents_every_selected_line() {
        let mut buffer = EditBuffer::new("a: 1\nb: 2\nc: 3");
        buffer.set_cursor(2, false);
        buffer.set_cursor(6, true);
        assert!(buffer.indent());
        assert_eq!(text_of(&buffer), "  a: 1\n  b: 2\nc: 3");
        assert_eq!(
            buffer.selection(),
            Some(4..10),
            "the selection follows the text it covered"
        );
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "a: 1\nb: 2\nc: 3");
    }

    #[test]
    fn shift_tab_outdents_and_reports_a_line_without_indent() {
        let mut buffer = EditBuffer::new("  a: 1\nb: 2");
        buffer.set_cursor(0, false);
        assert!(buffer.outdent());
        assert_eq!(text_of(&buffer), "a: 1\nb: 2");
        assert_eq!(buffer.cursor(), 0);
        assert!(
            !buffer.outdent(),
            "a line with no indent has nothing to remove"
        );
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "  a: 1\nb: 2");
    }

    #[test]
    fn outdent_removes_at_most_one_step_and_keeps_the_selection() {
        let mut buffer = EditBuffer::new("    a: 1\n    b: 2");
        buffer.set_cursor(0, false);
        buffer.set_cursor(12, true);
        assert!(buffer.outdent());
        assert_eq!(text_of(&buffer), "  a: 1\n  b: 2");
        assert_eq!(buffer.selection(), Some(0..8));
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "    a: 1\n    b: 2");
    }

    #[test]
    fn word_motion_moves_between_words_and_punctuation() {
        let mut buffer = EditBuffer::new("image: nginx:1.2");
        buffer.set_cursor(0, false);
        buffer.move_word(1, false);
        assert_eq!(buffer.cursor(), 5, "the first press ends the current word");
        buffer.move_word(1, false);
        assert_eq!(buffer.cursor(), 7, "the second press starts the next word");
        buffer.move_word(1, false);
        assert_eq!(buffer.cursor(), 12);
        buffer.move_word(1, false);
        assert_eq!(buffer.cursor(), 13, "the value is a word of its own");
        buffer.move_word(1, false);
        assert_eq!(buffer.cursor(), 16, "the end of the line stays put");
        buffer.move_word(1, false);
        assert_eq!(buffer.cursor(), 16);
        buffer.move_word(-1, false);
        assert_eq!(buffer.cursor(), 13);
        buffer.move_word(-1, false);
        assert_eq!(buffer.cursor(), 7, "the start of the previous word");
        buffer.move_word(-1, false);
        assert_eq!(buffer.cursor(), 0, "the first character has no step back");
        buffer.move_word(-1, false);
        assert_eq!(buffer.cursor(), 0);
    }

    #[test]
    fn word_motion_crosses_lines_and_extends_the_selection() {
        let mut buffer = EditBuffer::new("name: app\nimage: x");
        buffer.set_cursor(9, false);
        buffer.move_word(1, false);
        assert_eq!(buffer.cursor(), 10, "the newline is one step");
        buffer.move_word(1, false);
        assert_eq!(buffer.cursor(), 15, "the next line ends the first word");
        buffer.set_cursor(16, false);
        buffer.move_word(-1, true);
        assert_eq!(buffer.selection(), Some(10..16));
    }

    #[test]
    fn word_range_selects_a_word_for_a_double_click() {
        let buffer = EditBuffer::new("image: nginx:1.2\n  note: 中 文");
        assert_eq!(buffer.word_range_at(9), Some(7..12));
        assert_eq!(
            buffer.word_range_at(8),
            Some(7..12),
            "clicking the far side of a word selects it too"
        );
        assert_eq!(buffer.word_range_at(6), None, "a separator has no word");
        assert_eq!(buffer.word_range_at(0), Some(0..5));
        assert_eq!(
            buffer.word_range_at(13),
            Some(13..16),
            "a number with its dot is one word"
        );
        // The second line starts after the first one and its newline.
        let line_start = "image: nginx:1.2\n".len();
        let wide = line_start + "  note: ".len();
        assert_eq!(
            buffer.word_range_at(wide),
            Some(wide..wide + "中".len()),
            "a fullwidth character is one word"
        );
    }

    #[test]
    fn line_text_range_covers_one_line() {
        let buffer = EditBuffer::new("a: 1\nb: 2");
        assert_eq!(buffer.line_text_range(0), 0..4);
        assert_eq!(buffer.line_text_range(1), 5..9);
    }

    #[test]
    fn smart_home_and_end_walk_the_indentation() {
        let mut buffer = EditBuffer::new("  name: app");
        buffer.set_cursor(9, false);
        buffer.move_line_start(false);
        assert_eq!(buffer.cursor(), 2, "the first press skips the indent");
        buffer.move_line_start(false);
        assert_eq!(buffer.cursor(), 0, "the second press reaches column zero");
        buffer.move_line_end(false);
        assert_eq!(buffer.cursor(), 11, "there is no trailing space to skip");
        buffer.move_line_end(false);
        assert_eq!(buffer.cursor(), 11, "the end of the line is where it is");
        let mut buffer = EditBuffer::new("name: app  ");
        buffer.set_cursor(5, false);
        buffer.move_line_end(false);
        assert_eq!(buffer.cursor(), 9, "the first press skips trailing space");
        buffer.move_line_end(false);
        assert_eq!(buffer.cursor(), 11, "the second press reaches the line end");
    }

    #[test]
    fn document_start_and_end_reach_both_ends() {
        let mut buffer = EditBuffer::new("a\nb\nc");
        buffer.set_cursor(2, false);
        buffer.move_document_end(false);
        assert_eq!(buffer.cursor(), 5);
        buffer.move_document_start(false);
        assert_eq!(buffer.cursor(), 0);
        buffer.move_document_end(true);
        assert_eq!(buffer.selection(), Some(0..5));
    }

    #[test]
    fn insert_line_adds_an_empty_line_above_or_below() {
        let mut buffer = EditBuffer::new("a: 1\nb: 2");
        buffer.set_cursor(2, false);
        assert!(buffer.insert_line(false));
        assert_eq!(text_of(&buffer), "\na: 1\nb: 2");
        assert_eq!(buffer.cursor_in_line(), (0, 0));
        assert!(buffer.insert_line(true));
        assert_eq!(text_of(&buffer), "\n\na: 1\nb: 2");
        assert_eq!(buffer.cursor_in_line(), (1, 0));
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "\na: 1\nb: 2");
    }

    #[test]
    fn insert_line_below_the_last_line_creates_the_line() {
        let mut buffer = EditBuffer::new("a: 1");
        buffer.move_end(false);
        assert!(buffer.insert_line(true));
        assert_eq!(text_of(&buffer), "a: 1\n");
        assert_eq!(buffer.cursor_in_line(), (1, 0));
    }

    #[test]
    fn duplicate_line_copies_the_caret_or_selection() {
        let mut buffer = EditBuffer::new("a: 1\nb: 2");
        buffer.set_cursor(0, false);
        assert!(buffer.duplicate_lines());
        assert_eq!(text_of(&buffer), "a: 1\na: 1\nb: 2");
        assert_eq!(
            buffer.cursor_in_line(),
            (1, 0),
            "the caret follows the copy"
        );
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "a: 1\nb: 2");

        buffer.set_cursor(0, false);
        buffer.set_cursor(9, true);
        assert!(buffer.duplicate_lines());
        assert_eq!(text_of(&buffer), "a: 1\nb: 2\na: 1\nb: 2");
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "a: 1\nb: 2");
    }

    #[test]
    fn duplicate_line_works_on_a_document_without_a_trailing_newline() {
        let mut buffer = EditBuffer::new("a: 1");
        buffer.set_cursor(1, false);
        assert!(buffer.duplicate_lines());
        assert_eq!(text_of(&buffer), "a: 1\na: 1");
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "a: 1");
    }

    #[test]
    fn move_lines_swaps_one_line_up_and_down() {
        let mut buffer = EditBuffer::new("a\nb\nc\n");
        buffer.set_cursor(4, false);
        assert!(buffer.move_lines(-1));
        assert_eq!(text_of(&buffer), "a\nc\nb\n");
        assert_eq!(buffer.cursor(), 2, "the caret follows the line");
        assert!(buffer.move_lines(1));
        assert_eq!(text_of(&buffer), "a\nb\nc\n");
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "a\nc\nb\n");
    }

    #[test]
    fn move_lines_keeps_the_document_without_a_trailing_newline() {
        let mut buffer = EditBuffer::new("a\nb\nc");
        buffer.set_cursor(4, false);
        assert!(buffer.move_lines(-1));
        assert_eq!(
            text_of(&buffer),
            "a\nc\nb",
            "moving a line never invents a trailing newline"
        );
        buffer.set_cursor(2, false);
        assert!(buffer.move_lines(1));
        assert_eq!(text_of(&buffer), "a\nb\nc");
    }

    #[test]
    fn move_lines_stops_at_the_document_edges() {
        let mut buffer = EditBuffer::new("a\nb");
        buffer.set_cursor(0, false);
        assert!(!buffer.move_lines(-1));
        buffer.set_cursor(2, false);
        assert!(!buffer.move_lines(1));
        assert_eq!(text_of(&buffer), "a\nb");
    }

    #[test]
    fn move_lines_carries_a_multi_line_selection() {
        let mut buffer = EditBuffer::new("a\nb\nc\nd");
        buffer.set_cursor(2, false);
        buffer.set_cursor(5, true);
        assert!(buffer.move_lines(-1));
        assert_eq!(text_of(&buffer), "b\nc\na\nd");
        assert_eq!(buffer.selection(), Some(0..3));
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "a\nb\nc\nd");
        buffer.set_cursor(2, false);
        buffer.set_cursor(5, true);
        assert!(buffer.move_lines(1));
        assert_eq!(text_of(&buffer), "a\nd\nb\nc");
        assert_eq!(buffer.selection(), Some(4..7));
    }

    #[test]
    fn delete_line_removes_the_caret_line() {
        let mut buffer = EditBuffer::new("a\nb\nc");
        buffer.set_cursor(2, false);
        assert!(buffer.delete_lines());
        assert_eq!(text_of(&buffer), "a\nc");
        assert_eq!(buffer.cursor_in_line(), (1, 0));
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "a\nb\nc");
    }

    #[test]
    fn delete_line_handles_the_last_line_and_a_whole_document() {
        let mut buffer = EditBuffer::new("a\nb");
        buffer.set_cursor(2, false);
        assert!(buffer.delete_lines());
        assert_eq!(text_of(&buffer), "a");
        let mut buffer = EditBuffer::new("only");
        buffer.set_cursor(2, false);
        assert!(buffer.delete_lines());
        assert_eq!(text_of(&buffer), "");
        assert_eq!(buffer.line_count(), 1);
    }

    #[test]
    fn delete_line_removes_every_selected_line() {
        let mut buffer = EditBuffer::new("a\nb\nc\nd");
        buffer.set_cursor(0, false);
        buffer.set_cursor(4, true);
        assert!(buffer.delete_lines());
        assert_eq!(text_of(&buffer), "c\nd");
        assert_eq!(buffer.cursor_in_line(), (0, 0));
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "a\nb\nc\nd");
    }

    #[test]
    fn a_block_operation_covers_the_selection_and_nothing_more() {
        // A selection that reaches the document end covers every line it touches, and the
        // trailing empty line of a document that ends with a newline is not one of them.
        let mut buffer = EditBuffer::new("a: 1\nb: 2\n");
        buffer.select_all();
        assert!(buffer.indent());
        assert_eq!(text_of(&buffer), "  a: 1\n  b: 2\n");
        assert!(buffer.outdent());
        assert_eq!(text_of(&buffer), "a: 1\nb: 2\n");
        assert!(
            !buffer.outdent(),
            "nothing is left to outdent after one step"
        );
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "  a: 1\n  b: 2\n");

        // A selection that stops at the start of a line does not take that line.
        let mut buffer = EditBuffer::new("a: 1\nb: 2\nc: 3");
        buffer.set_cursor(0, false);
        buffer.set_cursor(5, true);
        assert!(buffer.indent());
        assert_eq!(text_of(&buffer), "  a: 1\nb: 2\nc: 3");
    }

    #[test]
    fn block_operations_work_on_a_single_line_document() {
        let mut buffer = EditBuffer::new("name: app");
        assert!(buffer.indent());
        assert_eq!(text_of(&buffer), "  name: app");
        assert!(buffer.outdent());
        assert_eq!(text_of(&buffer), "name: app");
        assert!(!buffer.move_lines(-1), "the only line is the top");
        assert!(!buffer.move_lines(1), "the only line is the bottom");
        assert!(buffer.duplicate_lines());
        assert_eq!(text_of(&buffer), "name: app\nname: app");
        assert!(buffer.delete_lines());
        assert_eq!(text_of(&buffer), "name: app");
    }

    #[test]
    fn block_operations_work_at_the_last_line_without_a_trailing_newline() {
        let mut buffer = EditBuffer::new("a: 1\nb: 2\nc: 3");
        buffer.set_cursor(10, false);
        assert_eq!(buffer.cursor_in_line(), (2, 0));
        assert!(buffer.indent());
        assert_eq!(text_of(&buffer), "a: 1\nb: 2\n  c: 3");
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "a: 1\nb: 2\nc: 3");
        assert!(buffer.duplicate_lines());
        assert_eq!(text_of(&buffer), "a: 1\nb: 2\nc: 3\nc: 3");
        assert!(buffer.undo());
        assert!(buffer.delete_lines());
        assert_eq!(
            text_of(&buffer),
            "a: 1\nb: 2",
            "the newline in front of the last line goes with it"
        );
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "a: 1\nb: 2\nc: 3");
    }

    #[test]
    fn block_operations_keep_the_trailing_newline_of_the_document() {
        let mut buffer = EditBuffer::new("a\nb\nc\n");
        buffer.set_cursor(4, false);
        assert!(
            !buffer.move_lines(1),
            "the empty row past the last line is not a line to swap with"
        );
        assert!(buffer.move_lines(-1));
        assert_eq!(
            text_of(&buffer),
            "a\nc\nb\n",
            "a document that ends with a newline keeps it"
        );
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "a\nb\nc\n");
        buffer.set_cursor(2, false);
        assert!(buffer.move_lines(1));
        assert_eq!(text_of(&buffer), "a\nc\nb\n");
        assert!(buffer.undo());
        buffer.set_cursor(4, false);
        assert!(buffer.delete_lines());
        assert_eq!(text_of(&buffer), "a\nb\n");
    }

    #[test]
    fn a_selection_reaching_the_end_of_the_document_moves_with_the_text() {
        let mut buffer = EditBuffer::new("a: 1\nb: 2");
        buffer.set_cursor(5, false);
        buffer.set_cursor(9, true);
        assert_eq!(buffer.selection(), Some(5..9));
        assert!(buffer.move_lines(-1));
        assert_eq!(text_of(&buffer), "b: 2\na: 1");
        assert_eq!(
            buffer.selection(),
            Some(0..4),
            "the selection still covers the same text"
        );
        assert!(buffer.undo());
        assert_eq!(text_of(&buffer), "a: 1\nb: 2");
    }

    #[test]
    fn replace_all_is_one_undo_step() {
        let mut buffer = EditBuffer::new("image: coredns\nimage: coredns");
        let edits = vec![
            (7..14, "coredns-2".to_owned()),
            (22..29, "coredns-2".to_owned()),
        ];
        assert!(buffer.replace_all(&edits));
        assert_eq!(text_of(&buffer), "image: coredns-2\nimage: coredns-2");
        assert!(buffer.undo(), "the whole replace-all undoes at once");
        assert_eq!(text_of(&buffer), "image: coredns\nimage: coredns");
        assert!(buffer.redo());
        assert_eq!(text_of(&buffer), "image: coredns-2\nimage: coredns-2");
        assert!(!buffer.replace_all(&[]), "no replacements is not an edit");
    }

    #[test]
    fn replace_all_with_no_change_reports_nothing() {
        let mut buffer = EditBuffer::new("a");
        assert!(!buffer.replace_all(&[(0..1, "a".to_owned())]));
        assert!(!buffer.is_dirty());
        assert!(!buffer.undo());
    }

    #[test]
    fn modified_lines_track_edits_until_the_document_is_saved() {
        let mut buffer = EditBuffer::new("a\nb\nc");
        assert!(!buffer.line_is_modified(0));
        buffer.set_cursor(0, false);
        buffer.insert("x");
        assert!(buffer.line_is_modified(0));
        assert!(!buffer.line_is_modified(2));
        assert!(buffer.undo());
        assert!(
            !buffer.line_is_modified(0),
            "undo back to the save point is clean"
        );
        buffer.insert("\nnew");
        assert!(buffer.line_is_modified(0));
        assert!(buffer.line_is_modified(1));
        buffer.mark_saved();
        assert!(!buffer.line_is_modified(0));
        assert!(!buffer.line_is_modified(1));
    }

    #[test]
    fn line_cells_count_fullwidth_characters_twice() {
        let buffer = EditBuffer::new("name: 中文\nascii: ab");
        assert_eq!(
            buffer.line(0).cells(),
            10,
            "six ASCII cells plus two fullwidth ones"
        );
        assert_eq!(buffer.line(1).cells(), 9);
        assert_eq!(buffer.line(0).char_len(), 8);
        assert_eq!(buffer.line(0).indent_len(), 0);
        assert_eq!(EditBuffer::new("  a: 1").line(0).indent_len(), 2);
    }

    #[test]
    fn line_text_is_shared_for_a_background_snapshot() {
        let mut buffer = EditBuffer::new("name: app");
        let snapshot = buffer.line(0).shared_text();
        buffer.set_cursor(0, false);
        buffer.insert("x");
        assert_eq!(&*snapshot, "name: app", "the snapshot keeps the old line");
        assert_eq!(buffer.line(0).text(), "xname: app");
    }
}
