//! Turns a grid selection into copyable text, one lock-sized chunk at a time.

use std::cmp;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::{Flags, LineLength};

use crate::layout::{append_cell_text, cell_columns};

/// Lines one grid-lock acquisition converts. A selection that spans the whole scrollback is
/// copied in several of these, so the terminal keeps accepting output and repainting while a
/// copy runs.
pub const COPY_CHUNK_LINES: usize = 512;

/// Default tab stop interval, matching the grid a fresh terminal starts with.
const TAB_STOP_INTERVAL: usize = 8;

/// How the selected cells are laid out into lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyShape {
    /// A character range: one line per grid line.
    Rows,
    /// A rectangle, trimmed on the right and joined with newlines.
    Block,
    /// Whole lines, always ending with a newline.
    Lines,
}

/// A selection captured as a plan, so the text can be produced in chunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionCopy {
    shape: CopyShape,
    start: Point,
    end: Point,
}

impl SelectionCopy {
    pub fn shape(&self) -> CopyShape {
        self.shape
    }

    /// Number of line groups the selection is copied in.
    pub fn chunks(&self) -> usize {
        self.chunks_within(COPY_CHUNK_LINES)
    }

    fn chunks_within(&self, chunk_lines: usize) -> usize {
        self.lines().div_ceil(chunk_lines.max(1)).max(1)
    }

    fn lines(&self) -> usize {
        let count = self
            .end
            .line
            .0
            .saturating_sub(self.start.line.0)
            .saturating_add(1);
        usize::try_from(count).unwrap_or(1).max(1)
    }
}

/// Captures the current selection as a copy plan. Returns `None` when nothing is selected.
pub fn plan_selection<T: EventListener>(term: &Term<T>) -> Option<SelectionCopy> {
    let selection = term.selection.as_ref()?;
    let range = selection.to_range(term)?;
    let shape = match selection.ty {
        SelectionType::Block => CopyShape::Block,
        SelectionType::Lines => CopyShape::Lines,
        SelectionType::Simple | SelectionType::Semantic => CopyShape::Rows,
    };
    Some(SelectionCopy {
        shape,
        start: range.start,
        end: range.end,
    })
}

/// Converts one group of lines. Chunks join in order, and only the last chunk applies the
/// trailing newline rule of the selection.
pub fn copy_chunk<T: EventListener>(term: &Term<T>, copy: SelectionCopy, chunk: usize) -> String {
    copy_chunk_within(term, copy, chunk, COPY_CHUNK_LINES)
}

fn copy_chunk_within<T: EventListener>(
    term: &Term<T>,
    copy: SelectionCopy,
    chunk: usize,
    chunk_lines: usize,
) -> String {
    let chunk_lines = chunk_lines.max(1);
    let offset = chunk.saturating_mul(chunk_lines);
    let first = i32::try_from(offset)
        .unwrap_or(i32::MAX)
        .saturating_add(copy.start.line.0);
    let last = first
        .saturating_add(
            i32::try_from(chunk_lines)
                .unwrap_or(i32::MAX)
                .saturating_sub(1),
        )
        .min(copy.end.line.0);
    let is_last = chunk.saturating_add(1) >= copy.chunks_within(chunk_lines);
    let top = term.topmost_line().0;
    let bottom = term.bottommost_line().0;
    let mut text = String::new();
    for line in first.max(top)..=last.min(bottom) {
        let line = Line(line);
        match copy.shape {
            CopyShape::Block => {
                let mut line_text = String::new();
                append_line(term, copy, line, &mut line_text);
                text.push_str(line_text.trim_end());
                if !(is_last && line == copy.end.line) {
                    text.push('\n');
                }
            }
            CopyShape::Rows | CopyShape::Lines => {
                append_line(term, copy, line, &mut text);
            }
        }
    }
    if is_last {
        match copy.shape {
            CopyShape::Rows => {
                if text.ends_with('\n') {
                    text.pop();
                }
            }
            CopyShape::Lines => {
                if !text.ends_with('\n') {
                    text.push('\n');
                }
            }
            CopyShape::Block => {}
        }
    }
    text
}

/// Appends the selected part of one grid line.
fn append_line<T: EventListener>(
    term: &Term<T>,
    copy: SelectionCopy,
    line: Line,
    text: &mut String,
) {
    let columns = term.columns();
    if columns == 0 {
        return;
    }
    let last_column = term.last_column();
    let block = copy.shape == CopyShape::Block;
    let mut start = copy.start.column.min(last_column);
    if !block && line != copy.start.line {
        start = Column(0);
    }
    let end = if block || line == copy.end.line {
        copy.end.column.min(last_column)
    } else {
        last_column
    };
    let include_wrapped_wide = (block && start.0 != 0) || line == copy.end.line;

    let grid = term.grid();
    let row = &grid[line];
    let line_length = cmp::min(row.line_length(), end + 1);
    if start > line_length {
        start = line_length;
    }
    if start.0 > 0 && row[start].flags.contains(Flags::WIDE_CHAR_SPACER) {
        start -= 1;
    }

    let mut tab_mode = false;
    for column in start.0..line_length.0 {
        let cell = &row[Column(column)];
        if tab_mode {
            if column % TAB_STOP_INTERVAL == 0 || cell.c != ' ' {
                tab_mode = false;
            } else {
                continue;
            }
        }
        if cell.c == '\t' {
            tab_mode = true;
        }
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        append_cell_text(text, cell, cell_columns(cell, column, columns));
    }

    if end >= last_column
        && (line_length.0 == 0
            || !row[Column(line_length.0.saturating_sub(1))]
                .flags
                .contains(Flags::WRAPLINE))
    {
        text.push('\n');
    }

    let topmost = term.topmost_line();
    if line_length.0 == columns
        && columns >= 2
        && row[Column(line_length.0.saturating_sub(1))]
            .flags
            .contains(Flags::LEADING_WIDE_CHAR_SPACER)
        && include_wrapped_wide
        && line.0 > topmost.0
    {
        text.push(grid[Line(line.0 - 1)][Column(0)].c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::index::Side;
    use alacritty_terminal::selection::Selection;
    use alacritty_terminal::term::test::mock_term;

    fn select(term: &mut Term<VoidListener>, shape: SelectionType, start: Point, end: Point) {
        let mut selection = Selection::new(shape, start, Side::Left);
        selection.update(end, Side::Right);
        term.selection = Some(selection);
    }

    fn copied(term: &Term<VoidListener>) -> String {
        let plan = plan_selection(term).expect("selection");
        (0..plan.chunks())
            .map(|chunk| copy_chunk(term, plan, chunk))
            .collect()
    }

    fn copied_in_chunks_of(term: &Term<VoidListener>, chunk_lines: usize) -> String {
        let plan = plan_selection(term).expect("selection");
        (0..plan.chunks_within(chunk_lines))
            .map(|chunk| copy_chunk_within(term, plan, chunk, chunk_lines))
            .collect()
    }

    #[test]
    fn rows_copy_keeps_wrapped_lines_joined() {
        let mut term = mock_term("aaa\nbbb\nccc");
        select(
            &mut term,
            SelectionType::Simple,
            Point::new(Line(0), Column(0)),
            Point::new(Line(2), Column(2)),
        );

        assert_eq!(copied(&term), "aaabbbccc");
        assert_eq!(
            copied_in_chunks_of(&term, 1),
            "aaabbbccc",
            "one line per chunk still produces the same text"
        );
        assert_eq!(copied_in_chunks_of(&term, 2), "aaabbbccc");
    }

    #[test]
    fn hidden_cells_are_copied_as_spaces() {
        let mut term = mock_term("secret");
        for column in 1..=4 {
            term.grid_mut()[Line(0)][Column(column)]
                .flags
                .insert(Flags::HIDDEN);
        }
        select(
            &mut term,
            SelectionType::Simple,
            Point::new(Line(0), Column(0)),
            Point::new(Line(0), Column(5)),
        );

        let text = copied(&term);
        assert_eq!(text, "s    t");
        assert!(!text.contains("ecret"), "concealed text is never copied");
    }

    #[test]
    fn block_copy_trims_and_joins_rows() {
        let mut term = mock_term("abcd\nefgh");
        select(
            &mut term,
            SelectionType::Block,
            Point::new(Line(0), Column(0)),
            Point::new(Line(1), Column(1)),
        );

        assert_eq!(copied(&term), "ab\nef");
    }

    #[test]
    fn whole_line_copy_always_ends_with_a_newline() {
        let mut term = mock_term("ab\r\ncd");
        select(
            &mut term,
            SelectionType::Lines,
            Point::new(Line(1), Column(1)),
            Point::new(Line(1), Column(1)),
        );
        let plan = plan_selection(&term).expect("selection");

        assert_eq!(plan.shape(), CopyShape::Lines);
        assert_eq!(copied(&term), "cd\n");
    }

    #[test]
    fn a_selection_over_the_scrollback_is_copied_in_groups_of_lines() {
        let lines: Vec<String> = (0..COPY_CHUNK_LINES + 3)
            .map(|index| format!("{index:04}"))
            .collect();
        let mut term = mock_term(&lines.join("\n"));
        let last = i32::try_from(COPY_CHUNK_LINES + 2).expect("line index");
        select(
            &mut term,
            SelectionType::Simple,
            Point::new(Line(0), Column(0)),
            Point::new(Line(last), Column(3)),
        );
        let plan = plan_selection(&term).expect("selection");

        assert_eq!(plan.chunks(), 2);
        assert_eq!(
            copy_chunk(&term, plan, 0)
                .chars()
                .filter(|character| *character == '\n')
                .count(),
            0,
            "a wrapped line does not end the chunk with a newline"
        );
        let joined: String = (0..plan.chunks())
            .map(|chunk| copy_chunk(&term, plan, chunk))
            .collect();
        assert_eq!(joined, lines.join(""));
    }
}
