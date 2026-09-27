//! Smart-case substring search across logical lines and scrollback.
//!
//! This module reads `Term` data without GPUI. Match points use grid coordinates.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags};
use unicode_segmentation::UnicodeSegmentation;

use crate::blink::BlinkState;

/// Inclusive grid range for one match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    pub start: Point,
    pub end: Point,
}

impl SearchMatch {
    pub fn contains(&self, point: Point) -> bool {
        (point.line > self.start.line
            || (point.line == self.start.line && point.column >= self.start.column))
            && (point.line < self.end.line
                || (point.line == self.end.line && point.column <= self.end.column))
    }
}

/// Caps match allocation for large scrollback buffers.
pub const MAX_MATCHES: usize = 10_000;
pub const MAX_QUERY_BYTES: usize = 4096;
pub(crate) const SEARCH_REFRESH_DELAY: Duration = Duration::from_millis(150);
const MAX_INDEX_LINES: usize = 20_000;
const MAX_INDEX_CELLS: usize = 8_000_000;
const MAX_HISTORY_DELTA_LINES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SearchCell {
    ch: char,
    column: u16,
    width: u8,
    barrier: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SearchLine {
    cells: Arc<[SearchCell]>,
    wrapped: bool,
}

impl SearchLine {
    fn len(&self) -> usize {
        self.cells.len()
    }
}

#[derive(Clone, Debug)]
struct SearchIndexData {
    lines: VecDeque<SearchLine>,
    topmost_line: i32,
    columns: usize,
    screen_lines: usize,
    history_lines: usize,
    cell_count: usize,
    oldest: Option<SearchLine>,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchIndex(Arc<SearchIndexData>);

#[derive(Clone, Debug)]
struct SearchOutput {
    alternate: bool,
    lines: Vec<SearchLine>,
    columns: usize,
    screen_lines: usize,
    history_lines: usize,
    cell_count: usize,
    oldest: Option<SearchLine>,
    history_tail: Vec<SearchLine>,
    complete: bool,
}

pub(crate) struct SearchIndexUpdate {
    alternate: bool,
    output: Option<SearchOutput>,
    replacement: Option<SearchIndex>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SearchIndexes {
    primary: Option<SearchIndex>,
    alternate: Option<SearchIndex>,
    alternate_active: bool,
}

#[derive(Debug)]
pub(crate) struct SearchRefresh {
    pub epoch: u64,
    pub generation: u64,
    pub query: String,
    pub matches: Vec<SearchMatch>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SearchRefreshOutcome {
    Applied,
    Stale,
    Superseded,
}

fn capture_line(
    grid: &Grid<Cell>,
    line: Line,
    columns: usize,
    cell_limit: usize,
) -> (SearchLine, bool) {
    if columns == 0 {
        return (
            SearchLine {
                cells: Arc::from(Vec::new()),
                wrapped: false,
            },
            true,
        );
    }
    let row = &grid[line];
    let mut cells = Vec::with_capacity(columns.min(cell_limit));
    let mut complete = true;
    'cells: for column in 0..columns {
        if cells.len() >= cell_limit {
            complete = false;
            break;
        }
        let column_index = Column(column);
        let cell = &row[column_index];
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        let hidden = crate::layout::cell_is_hidden(cell);
        cells.push(SearchCell {
            ch: cell.c,
            column: u16::try_from(column).unwrap_or(u16::MAX),
            width: crate::layout::cell_columns(cell, column, columns) as u8,
            barrier: hidden,
        });
        if hidden {
            continue;
        }
        if let Some(zerowidth) = cell.zerowidth() {
            for character in zerowidth {
                if cells.len() >= cell_limit {
                    complete = false;
                    break 'cells;
                }
                cells.push(SearchCell {
                    ch: *character,
                    column: u16::try_from(column).unwrap_or(u16::MAX),
                    width: crate::layout::cell_columns(cell, column, columns) as u8,
                    barrier: false,
                });
            }
        }
    }
    (
        SearchLine {
            cells: cells.into(),
            wrapped: row[Column(columns - 1)].flags.contains(Flags::WRAPLINE),
        },
        complete,
    )
}

fn capture_bounded_lines(
    grid: &Grid<Cell>,
    first: i32,
    last: i32,
    columns: usize,
) -> (VecDeque<SearchLine>, usize, i32, bool) {
    let first = first.max(last.saturating_sub(MAX_INDEX_LINES as i32 - 1));
    let mut lines = VecDeque::new();
    let mut cell_count = 0;
    let mut retained_first = last;
    let mut complete = true;
    for line in (first..=last).rev() {
        let (search_line, line_complete) = capture_line(
            grid,
            Line(line),
            columns,
            MAX_INDEX_CELLS.saturating_sub(cell_count),
        );
        cell_count = cell_count.saturating_add(search_line.len());
        retained_first = line;
        lines.push_front(search_line);
        if !line_complete || cell_count >= MAX_INDEX_CELLS {
            complete = false;
            break;
        }
    }
    (lines, cell_count, retained_first, complete)
}

impl SearchIndexData {
    fn trim(&mut self) {
        while self.lines.len() > MAX_INDEX_LINES || self.cell_count > MAX_INDEX_CELLS {
            let Some(line) = self.lines.pop_front() else {
                break;
            };
            self.cell_count = self.cell_count.saturating_sub(line.len());
            self.topmost_line = self.topmost_line.saturating_add(1);
        }
    }
}

impl SearchIndex {
    fn capture<T: EventListener>(term: &Term<T>) -> Self {
        let grid = term.grid();
        let columns = term.columns();
        let screen_lines = term.screen_lines();
        let history_lines = term.history_size();
        let topmost = grid.topmost_line().0;
        let bottommost = grid.bottommost_line().0;
        let (lines, cell_count, retained_first, _) =
            capture_bounded_lines(grid, topmost, bottommost, columns);
        let oldest = (history_lines > 0)
            .then(|| capture_line(grid, grid.topmost_line(), columns, MAX_INDEX_CELLS).0);
        Self(Arc::new(SearchIndexData {
            lines,
            topmost_line: retained_first,
            columns,
            screen_lines,
            history_lines,
            cell_count,
            oldest,
        }))
    }

    fn from_output(output: &SearchOutput) -> Self {
        Self(Arc::new(SearchIndexData {
            lines: output.lines.iter().cloned().collect(),
            topmost_line: -(output.history_lines as i32),
            columns: output.columns,
            screen_lines: output.screen_lines,
            history_lines: output.history_lines,
            cell_count: output.cell_count,
            oldest: output.oldest.clone(),
        }))
    }

    fn can_apply(&self, output: &SearchOutput) -> bool {
        output.complete
            && output.columns > 0
            && output.screen_lines > 0
            && self.0.columns == output.columns
            && self.0.screen_lines == output.screen_lines
            && self.0.history_lines <= output.history_lines
            && self.0.lines.len() >= self.0.screen_lines
    }

    fn apply(&mut self, output: SearchOutput) {
        let (old_active, old_history, full_history) = {
            let data = &self.0;
            let start = data.lines.len().saturating_sub(data.screen_lines);
            let active = data.lines.iter().skip(start).cloned().collect::<Vec<_>>();
            let history_line = i32::try_from(data.history_lines).unwrap_or(i32::MAX);
            let full_history = data.topmost_line == -history_line
                && data.lines.len() >= data.history_lines.saturating_add(data.screen_lines);
            (active, data.history_lines, full_history)
        };
        let oldest_changed = self.0.oldest != output.oldest;
        let history_shift = (output.history_lines == old_history && full_history)
            .then(|| inferred_history_shift(&self.0.lines, old_history, &output.history_tail))
            .flatten();
        let shift = history_shift
            .or_else(|| {
                inferred_active_shift(&old_active, &output.lines)
                    .filter(|(_, score)| {
                        oldest_changed || score.saturating_mul(2) >= old_active.len()
                    })
                    .filter(|_| old_history > 0)
            })
            .map(|(shift, _)| shift);
        let total_lines = self.0.lines.len();
        let rotation = shift.unwrap_or(0) % total_lines.max(1);
        let replace_history =
            history_shift.is_some() && rotation > output.history_tail.len().min(old_history);
        let retained_old_active = if replace_history {
            0
        } else if history_shift.is_some() {
            if rotation <= old_history {
                rotation.min(old_active.len())
            } else {
                total_lines.saturating_sub(rotation).min(old_active.len())
            }
        } else {
            rotation.min(old_active.len())
        };
        let replaced_start = old_active.len().saturating_sub(retained_old_active);
        let wrapped_lines = if history_shift.is_some() && !replace_history {
            rotation
                .saturating_add(old_history)
                .saturating_sub(total_lines)
                .min(old_history)
        } else {
            0
        };
        let wrapped_prefix = self
            .0
            .lines
            .iter()
            .take(wrapped_lines)
            .cloned()
            .collect::<Vec<_>>();
        let replaced_old_active_cells = old_active[replaced_start..]
            .iter()
            .map(SearchLine::len)
            .sum::<usize>();
        let data = Arc::make_mut(&mut self.0);
        if output.history_lines > old_history {
            let carry = (output.history_lines - old_history).min(old_active.len());
            data.topmost_line = data
                .topmost_line
                .saturating_sub((output.history_lines - old_history) as i32);
            for line in old_active[..carry].iter().rev() {
                data.lines.push_front(line.clone());
                data.cell_count = data.cell_count.saturating_add(line.len());
            }
        } else if replace_history {
            data.lines.clear();
            data.cell_count = 0;
            data.topmost_line = -(output.history_tail.len() as i32);
            data.lines.extend(output.history_tail.iter().cloned());
            data.cell_count = data.lines.iter().map(SearchLine::len).sum::<usize>();
        } else if rotation > 0 {
            for _ in 0..rotation {
                if let Some(line) = data.lines.pop_front() {
                    data.cell_count = data.cell_count.saturating_sub(line.len());
                }
            }
            for line in wrapped_prefix {
                data.lines.push_back(line.clone());
                data.cell_count = data.cell_count.saturating_add(line.len());
            }
            if history_shift.is_some() {
                let overlay = rotation.min(output.history_tail.len());
                let overlay_start = output.history_tail.len() - overlay;
                let line_start = data.lines.len().saturating_sub(overlay);
                for (offset, line) in output.history_tail[overlay_start..].iter().enumerate() {
                    if let Some(previous) = data.lines.get(line_start + offset) {
                        data.cell_count = data.cell_count.saturating_sub(previous.len());
                    }
                    data.lines[line_start + offset] = line.clone();
                    data.cell_count = data.cell_count.saturating_add(line.len());
                }
            }
        }
        let history_to_keep = if full_history {
            output.history_lines
        } else {
            data.lines.len().saturating_sub(old_active.len())
        };
        data.lines.truncate(history_to_keep);
        data.cell_count = data
            .cell_count
            .saturating_sub(replaced_old_active_cells)
            .saturating_add(output.cell_count);
        data.lines.extend(output.lines);
        data.columns = output.columns;
        data.screen_lines = output.screen_lines;
        data.history_lines = output.history_lines;
        data.oldest = output.oldest;
        data.trim();
    }
}

impl SearchOutput {
    fn capture<T: EventListener>(term: &Term<T>) -> Self {
        let grid = term.grid();
        let columns = term.columns();
        let screen_lines = term.screen_lines();
        let history_lines = term.history_size();
        let (lines, cell_count, retained_first, complete) =
            capture_bounded_lines(grid, 0, screen_lines as i32 - 1, columns);
        let oldest = (history_lines > 0)
            .then(|| capture_line(grid, grid.topmost_line(), columns, MAX_INDEX_CELLS).0);
        let history_tail = if history_lines > 0 {
            let count = screen_lines.max(MAX_HISTORY_DELTA_LINES).min(history_lines);
            (-(count as i32)..0)
                .map(|line| capture_line(grid, Line(line), columns, MAX_INDEX_CELLS).0)
                .collect()
        } else {
            Vec::new()
        };
        let line_count = lines.len();
        Self {
            alternate: term.mode().contains(TermMode::ALT_SCREEN),
            lines: lines.into_iter().collect(),
            columns,
            screen_lines,
            history_lines,
            cell_count,
            oldest,
            history_tail,
            complete: complete && retained_first == 0 && line_count == screen_lines,
        }
    }
}

fn inferred_history_shift(
    old_lines: &VecDeque<SearchLine>,
    history_lines: usize,
    history_tail: &[SearchLine],
) -> Option<(usize, usize)> {
    if history_lines == 0 || history_tail.is_empty() {
        return None;
    }
    let total_lines = old_lines.len();
    let mut best_shift = 0;
    let mut best_score = 0;
    for start in 0..total_lines {
        let score = history_tail
            .iter()
            .enumerate()
            .filter(|(offset, new)| old_lines[(start + offset) % total_lines] == **new)
            .count();
        if score > best_score {
            best_shift = ((start + history_tail.len()) as isize - history_lines as isize)
                .rem_euclid(total_lines as isize) as usize;
            if best_shift == 0 {
                best_shift = total_lines;
            }
            best_score = score;
        }
    }
    (best_shift > 0).then_some((best_shift, best_score))
}

fn inferred_active_shift(old: &[SearchLine], new: &[SearchLine]) -> Option<(usize, usize)> {
    if old.is_empty() || new.is_empty() {
        return None;
    }
    let mut best_shift = 0;
    let mut best_score = 0;
    for shift in 0..old.len() {
        let score = old[shift..]
            .iter()
            .zip(new)
            .take_while(|(old, new)| *old == *new)
            .count();
        if score > best_score {
            best_shift = shift;
            best_score = score;
        }
    }
    (best_shift > 0).then_some((best_shift, best_score))
}

impl SearchIndexes {
    pub(crate) fn capture<T: EventListener>(term: &Term<T>) -> Self {
        let index = SearchIndex::capture(term);
        let alternate_active = term.mode().contains(TermMode::ALT_SCREEN);
        Self {
            primary: (!alternate_active).then_some(index.clone()),
            alternate: alternate_active.then_some(index),
            alternate_active,
        }
    }

    pub(crate) fn active(&self) -> Option<SearchIndex> {
        if self.alternate_active {
            self.alternate.clone()
        } else {
            self.primary.clone()
        }
    }

    pub(crate) fn screen_size(&self) -> Option<(usize, usize)> {
        self.active()
            .map(|index| (index.0.columns, index.0.screen_lines))
    }

    pub(crate) fn capture_update<T: EventListener>(&self, term: &Term<T>) -> SearchIndexUpdate {
        let output = SearchOutput::capture(term);
        let alternate = output.alternate;
        let current = self.active_slot(alternate);
        if current
            .as_ref()
            .is_some_and(|index| index.can_apply(&output))
            || current.is_none() && output.history_lines == 0
        {
            SearchIndexUpdate {
                alternate,
                output: Some(output),
                replacement: None,
            }
        } else {
            SearchIndexUpdate {
                alternate,
                output: None,
                replacement: Some(SearchIndex::capture(term)),
            }
        }
    }

    pub(crate) fn apply_update(&mut self, update: SearchIndexUpdate) {
        self.alternate_active = update.alternate;
        if let Some(output) = update.output {
            if let Some(index) = self.active_slot_mut(update.alternate).as_mut() {
                index.apply(output);
            } else {
                self.set_active(update.alternate, SearchIndex::from_output(&output));
            }
        } else if let Some(index) = update.replacement {
            self.set_active(update.alternate, index);
        }
    }

    fn active_slot(&self, alternate: bool) -> &Option<SearchIndex> {
        if alternate {
            &self.alternate
        } else {
            &self.primary
        }
    }

    fn active_slot_mut(&mut self, alternate: bool) -> &mut Option<SearchIndex> {
        if alternate {
            &mut self.alternate
        } else {
            &mut self.primary
        }
    }

    fn set_active(&mut self, alternate: bool, index: SearchIndex) {
        self.alternate_active = alternate;
        *self.active_slot_mut(alternate) = Some(index);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SearchComposition {
    range: Range<usize>,
    text: String,
    selected: Option<Range<usize>>,
    selection: Range<usize>,
    anchor: usize,
    cursor: usize,
}

#[derive(Clone, Debug)]
struct SearchSnapshot {
    query: String,
    selection: Range<usize>,
    anchor: usize,
    cursor: usize,
}

/// Stores the query, matches, current match, and text input state.
#[derive(Clone, Debug, Default)]
pub struct SearchState {
    query: String,
    matches: Arc<[SearchMatch]>,
    matches_generation: Option<u64>,
    current: usize,
    refresh_epoch: u64,
    refresh_pending: bool,
    selection: Range<usize>,
    anchor: usize,
    cursor: usize,
    composition: Option<SearchComposition>,
    undo_history: Vec<SearchSnapshot>,
    redo_history: Vec<SearchSnapshot>,
    caret_blink: BlinkState,
}

impl SearchState {
    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn selection(&self) -> Range<usize> {
        self.selection.clone()
    }

    pub fn anchor(&self) -> usize {
        self.anchor
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn caret_visible(&self, focused: bool, reduce_motion: bool) -> bool {
        focused && (reduce_motion || self.caret_blink.visible())
    }

    pub(crate) fn caret_blinking_active(&self) -> bool {
        self.caret_blink.active()
    }

    pub(crate) fn set_caret_blinking(&mut self, blinking: bool) {
        self.caret_blink.set_setting(blinking);
    }

    pub(crate) fn reset_caret_blink(&mut self) {
        self.caret_blink.show();
    }

    pub(crate) fn tick_caret_blink(&mut self) -> bool {
        self.caret_blink.tick()
    }

    pub fn selection_reversed(&self) -> bool {
        self.cursor < self.anchor
    }

    pub fn selected_text(&self) -> Option<&str> {
        (self.selection.start < self.selection.end).then(|| &self.query[self.selection.clone()])
    }

    pub fn selected_display_text(&self) -> Option<String> {
        let text = self.display_text();
        let selection = self.display_selection();
        text.get(selection).map(str::to_owned)
    }

    pub fn is_composing(&self) -> bool {
        self.composition.is_some()
    }

    pub fn selection_utf16(&self) -> Range<usize> {
        let range = self.display_selection();
        let text = self.display_text();
        byte_to_utf16(&text, range.start)..byte_to_utf16(&text, range.end)
    }

    pub fn selected_text_range_utf16(&self) -> (Range<usize>, bool) {
        if let Some(range) = self.composition_selected_utf16() {
            return (range, false);
        }
        (self.selection_utf16(), self.selection_reversed())
    }

    pub fn set_selection(&mut self, range: Range<usize>) {
        self.set_selection_with_anchor(range.start, range.end);
    }

    pub fn set_selection_with_anchor(&mut self, anchor: usize, cursor: usize) {
        let anchor = self.boundary_at_or_before(anchor);
        let cursor = self.boundary_at_or_before(cursor);
        self.anchor = anchor;
        self.cursor = cursor;
        self.selection = anchor.min(cursor)..anchor.max(cursor);
        self.reset_caret_blink();
    }

    pub fn set_selection_utf16(&mut self, range: Range<usize>) {
        let text = self.display_text();
        let (start, end) = utf16_range_to_grapheme_bytes(&text, range);
        if let Some(composition) = self.composition.as_mut() {
            let composition_start = composition.range.start;
            let composition_end = composition_start + composition.text.len();
            if start >= composition_start && end <= composition_end {
                composition.selected = Some(start - composition_start..end - composition_start);
                self.reset_caret_blink();
                return;
            }
        }
        let query_range =
            self.display_byte_to_query_byte(start)..self.display_byte_to_query_byte(end);
        self.discard_composition();
        self.set_selection(query_range);
    }

    pub fn select_caret(&mut self, byte: usize, extend: bool) {
        let target = self.boundary_at_or_before(byte);
        if extend {
            if self.selection.start == self.selection.end {
                self.anchor = self.cursor;
            }
            let anchor = self.anchor;
            self.set_selection_with_anchor(anchor, target);
        } else {
            self.set_selection_with_anchor(target, target);
        }
        self.composition = None;
    }

    pub fn byte_range_from_utf16(&self, range: Range<usize>) -> Range<usize> {
        let (start, end) = utf16_range_to_grapheme_bytes(&self.query, range);
        start..end
    }

    pub fn display_text(&self) -> Cow<'_, str> {
        let Some(composition) = &self.composition else {
            return Cow::Borrowed(&self.query);
        };
        let mut text = String::with_capacity(
            self.query.len() - composition.range.len() + composition.text.len(),
        );
        text.push_str(&self.query[..composition.range.start]);
        text.push_str(&composition.text);
        text.push_str(&self.query[composition.range.end..]);
        Cow::Owned(text)
    }

    pub fn display_selection(&self) -> Range<usize> {
        if let Some(composition) = &self.composition {
            let start = composition.range.start;
            if let Some(selected) = &composition.selected {
                return start + selected.start..start + selected.end;
            }
        }
        self.query_byte_to_display_byte(self.selection.start)
            ..self.query_byte_to_display_byte(self.selection.end)
    }

    pub fn display_cursor(&self) -> usize {
        if let Some(composition) = &self.composition {
            let offset = composition
                .selected
                .as_ref()
                .map_or(composition.text.len(), |selected| selected.end);
            return composition.range.start
                + grapheme_boundary_at_or_before(&composition.text, offset);
        }
        self.query_byte_to_display_byte(self.cursor)
    }

    pub fn display_byte_to_query_byte(&self, byte: usize) -> usize {
        let Some(composition) = &self.composition else {
            return byte.min(self.query.len());
        };
        let start = composition.range.start;
        let end = start + composition.text.len();
        if byte <= start {
            byte
        } else if byte >= end {
            composition.range.end + byte - end
        } else {
            start
        }
    }

    pub fn query_byte_to_display_byte(&self, byte: usize) -> usize {
        let byte = byte.min(self.query.len());
        let Some(composition) = &self.composition else {
            return byte;
        };
        let start = composition.range.start;
        let end = composition.range.end;
        if byte <= start {
            byte
        } else if byte >= end {
            start + composition.text.len() + byte - end
        } else {
            start + composition.text.len()
        }
    }

    pub fn display_utf16_for_byte(&self, byte: usize) -> usize {
        let text = self.display_text();
        byte_to_utf16(&text, grapheme_boundary_at_or_before(&text, byte))
    }

    pub fn display_byte_for_utf16(&self, utf16: usize) -> usize {
        let text = self.display_text();
        grapheme_boundary_at_or_before(&text, utf16_to_byte(&text, utf16))
    }

    pub fn text_for_utf16_range(&self, range: Range<usize>) -> Option<(String, Range<usize>)> {
        let text = self.display_text();
        let (start, end) = utf16_range_to_grapheme_bytes(&text, range);
        let value = text.get(start..end)?.to_owned();
        let actual = byte_to_utf16(&text, start)..byte_to_utf16(&text, end);
        Some((value, actual))
    }

    pub fn marked_range_utf16(&self) -> Option<Range<usize>> {
        let composition = self.composition.as_ref()?;
        let start = byte_to_utf16(&self.query, composition.range.start);
        Some(start..start + composition.text.encode_utf16().count())
    }

    pub fn composition_selected_utf16(&self) -> Option<Range<usize>> {
        let composition = self.composition.as_ref()?;
        let selected = composition.selected.as_ref()?;
        let start = byte_to_utf16(&self.query, composition.range.start);
        Some(
            start + byte_to_utf16(&composition.text, selected.start)
                ..start + byte_to_utf16(&composition.text, selected.end),
        )
    }

    pub fn replace_and_mark_text(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        selected_range_utf16: Option<Range<usize>>,
    ) {
        self.reset_caret_blink();
        let mut text = sanitize_text(text);
        let previous = self
            .composition
            .as_ref()
            .map(|composition| {
                (
                    composition.selection.clone(),
                    composition.anchor,
                    composition.cursor,
                )
            })
            .unwrap_or_else(|| (self.selection.clone(), self.anchor, self.cursor));
        let range = self
            .composition
            .as_ref()
            .map(|composition| composition.range.clone())
            .or_else(|| range_utf16.map(|range| self.query_byte_range_from_utf16(range)))
            .unwrap_or_else(|| self.selection.clone());
        truncate_text(
            &mut text,
            MAX_QUERY_BYTES.saturating_sub(self.query.len() - range.len()),
        );
        if text.is_empty() {
            self.discard_composition();
            return;
        }
        if self.composition.is_none() {
            self.set_selection(range.clone());
        }
        let selected = selected_range_utf16.map(|selected| {
            let (start, end) = utf16_range_to_grapheme_bytes(&text, selected);
            start..end
        });
        self.composition = Some(SearchComposition {
            range,
            text,
            selected,
            selection: previous.0,
            anchor: previous.1,
            cursor: previous.2,
        });
    }

    pub fn discard_composition(&mut self) -> bool {
        let Some(composition) = self.composition.take() else {
            return false;
        };
        self.set_selection_with_anchor(composition.anchor, composition.cursor);
        true
    }

    pub fn commit_text(&mut self, range_utf16: Option<Range<usize>>, text: &str) {
        let range = self
            .composition
            .take()
            .map(|composition| composition.range)
            .or_else(|| range_utf16.map(|range| self.query_byte_range_from_utf16(range)))
            .unwrap_or_else(|| self.selection.clone());
        self.replace_range_impl(range, text);
    }

    pub fn replace_range(&mut self, range: Range<usize>, text: &str) {
        self.discard_composition();
        self.replace_range_impl(range, text);
    }

    pub fn insert_text(&mut self, text: &str) {
        self.discard_composition();
        let range = self.selection.clone();
        if text.is_empty() && range.start == range.end {
            return;
        }
        self.replace_range_impl(range, text);
    }

    pub fn backspace(&mut self) {
        self.discard_composition();
        if self.selection.start != self.selection.end {
            self.delete_selection();
            return;
        }
        let start = self.previous_boundary(self.cursor);
        if start < self.cursor {
            self.replace_range_impl(start..self.cursor, "");
        }
    }

    pub fn delete(&mut self) {
        self.discard_composition();
        if self.selection.start != self.selection.end {
            self.delete_selection();
            return;
        }
        let end = self.next_boundary(self.cursor);
        if end > self.cursor {
            self.replace_range_impl(self.cursor..end, "");
        }
    }

    pub fn home(&mut self) {
        self.composition = None;
        self.set_caret(0);
    }

    pub fn end(&mut self) {
        self.composition = None;
        self.set_caret(self.query.len());
    }

    pub fn select_all(&mut self) {
        self.composition = None;
        self.set_selection_with_anchor(0, self.query.len());
    }

    pub fn move_left(&mut self, extend: bool) {
        if self.selection.start != self.selection.end && !extend {
            self.composition = None;
            self.set_caret(self.selection.start);
            return;
        }
        let target = self.previous_boundary(self.cursor);
        if extend {
            if self.selection.start == self.selection.end {
                self.anchor = self.cursor;
            }
            let anchor = self.anchor;
            self.composition = None;
            self.set_selection_with_anchor(anchor, target);
        } else {
            self.composition = None;
            self.set_caret(target);
        }
    }

    pub fn move_right(&mut self, extend: bool) {
        if self.selection.start != self.selection.end && !extend {
            self.composition = None;
            self.set_caret(self.selection.end);
            return;
        }
        let target = self.next_boundary(self.cursor);
        if extend {
            if self.selection.start == self.selection.end {
                self.anchor = self.cursor;
            }
            let anchor = self.anchor;
            self.composition = None;
            self.set_selection_with_anchor(anchor, target);
        } else {
            self.composition = None;
            self.set_caret(target);
        }
    }

    pub fn matches(&self) -> &[SearchMatch] {
        &self.matches
    }

    pub(crate) fn matches_for_generation(&self, generation: u64) -> Option<&[SearchMatch]> {
        (self.matches_generation == Some(generation)).then_some(self.matches.as_ref())
    }

    pub(crate) fn clear_results(&mut self) {
        self.matches = Arc::from(Vec::new());
        self.matches_generation = None;
        self.current = 0;
    }

    pub fn count(&self) -> usize {
        self.matches.len()
    }

    pub fn current_index(&self) -> Option<usize> {
        (!self.matches.is_empty()).then(|| self.current.min(self.matches.len() - 1))
    }

    pub fn current_display_index(&self) -> Option<usize> {
        self.current_index().map(|index| index + 1)
    }

    pub fn current_match(&self) -> Option<SearchMatch> {
        self.current_index()
            .and_then(|index| self.matches.get(index).copied())
    }

    pub fn highlights(&self) -> crate::layout::SearchHighlights {
        crate::layout::SearchHighlights {
            matches: self.matches.clone(),
            current: self.current_index(),
        }
    }

    pub fn cut(&mut self) -> Option<String> {
        self.discard_composition();
        let text = self.selected_text()?.to_owned();
        self.replace_range_impl(self.selection.clone(), "");
        Some(text)
    }

    pub fn undo(&mut self) -> bool {
        let canceled = self.discard_composition();
        let Some(snapshot) = self.undo_history.pop() else {
            return canceled;
        };
        self.redo_history.push(self.snapshot());
        self.restore_snapshot(snapshot);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(snapshot) = self.redo_history.pop() else {
            return false;
        };
        self.undo_history.push(self.snapshot());
        self.restore_snapshot(snapshot);
        true
    }

    fn set_caret(&mut self, cursor: usize) {
        self.set_selection_with_anchor(cursor, cursor);
    }

    fn query_byte_range_from_utf16(&self, range: Range<usize>) -> Range<usize> {
        let (start, end) = utf16_range_to_grapheme_bytes(&self.query, range);
        start..end
    }

    fn snapshot(&self) -> SearchSnapshot {
        SearchSnapshot {
            query: self.query.clone(),
            selection: self.selection.clone(),
            anchor: self.anchor,
            cursor: self.cursor,
        }
    }

    fn restore_snapshot(&mut self, snapshot: SearchSnapshot) {
        self.query = snapshot.query;
        self.composition = None;
        self.set_selection_with_anchor(snapshot.anchor, snapshot.cursor);
        self.selection = snapshot.selection;
        self.invalidate_matches();
    }

    fn normalized_range(&self, range: Range<usize>) -> Range<usize> {
        let start = self.boundary_at_or_before(range.start);
        let end = grapheme_boundary_at_or_after(&self.query, range.end);
        start.min(end)..start.max(end)
    }

    fn replace_range_impl(&mut self, range: Range<usize>, text: &str) {
        let range = self.normalized_range(range);
        let mut text = sanitize_text(text);
        truncate_text(
            &mut text,
            MAX_QUERY_BYTES.saturating_sub(self.query.len() - range.len()),
        );
        if text.is_empty() && range.start == range.end {
            return;
        }
        if self.query.get(range.clone()) != Some(text.as_str()) {
            self.undo_history.push(self.snapshot());
            self.redo_history.clear();
        }
        self.query.replace_range(range.clone(), &text);
        self.cursor = range.start + text.len();
        self.anchor = self.cursor;
        self.selection = self.cursor..self.cursor;
        self.composition = None;
        self.reset_caret_blink();
        self.invalidate_matches();
    }

    fn boundary_at_or_before(&self, index: usize) -> usize {
        grapheme_boundary_at_or_before(&self.query, index)
    }

    fn previous_boundary(&self, index: usize) -> usize {
        let index = self.boundary_at_or_before(index);
        self.query[..index]
            .grapheme_indices(true)
            .next_back()
            .map(|(start, _)| start)
            .unwrap_or(0)
    }

    fn next_boundary(&self, index: usize) -> usize {
        let index = self.boundary_at_or_before(index);
        self.query[index..]
            .grapheme_indices(true)
            .next()
            .map(|(start, grapheme)| index + start + grapheme.len())
            .unwrap_or(self.query.len())
    }

    fn delete_selection(&mut self) {
        self.replace_range_impl(self.selection.clone(), "");
    }

    fn invalidate_matches(&mut self) {
        self.clear_results();
        if self.query.is_empty() {
            self.refresh_pending = false;
        }
    }

    pub fn set_query(&mut self, query: String) {
        let mut query = sanitize_text(&query);
        truncate_text(&mut query, MAX_QUERY_BYTES);
        if self.query == query && self.composition.is_none() {
            return;
        }
        self.query = query;
        self.composition = None;
        self.undo_history.clear();
        self.redo_history.clear();
        self.set_caret(self.query.len());
        self.invalidate_matches();
    }

    pub fn set_matches(&mut self, matches: Vec<SearchMatch>) {
        self.set_matches_inner(matches, None);
    }

    pub(crate) fn set_matches_for_generation(
        &mut self,
        generation: u64,
        matches: Vec<SearchMatch>,
    ) {
        self.set_matches_inner(matches, Some(generation));
    }

    fn set_matches_inner(&mut self, matches: Vec<SearchMatch>, generation: Option<u64>) {
        let current = self.current_match();
        self.matches = matches.into();
        self.matches_generation = generation;
        self.current = current
            .and_then(|current| self.matches.iter().position(|item| *item == current))
            .unwrap_or(0);
    }

    pub(crate) fn request_refresh(&mut self) -> Option<u64> {
        if self.query.is_empty() || self.refresh_pending {
            return None;
        }
        self.refresh_epoch = self.refresh_epoch.wrapping_add(1);
        self.refresh_pending = true;
        Some(self.refresh_epoch)
    }

    pub(crate) fn finish_refresh(&mut self, epoch: u64) -> bool {
        if !self.refresh_pending || self.refresh_epoch != epoch {
            return false;
        }
        self.refresh_pending = false;
        true
    }

    pub(crate) fn apply_refresh(
        &mut self,
        refresh: SearchRefresh,
        current_generation: u64,
    ) -> SearchRefreshOutcome {
        if !self.finish_refresh(refresh.epoch) {
            return SearchRefreshOutcome::Superseded;
        }
        if refresh.generation != current_generation || refresh.query != self.query {
            return SearchRefreshOutcome::Stale;
        }
        self.set_matches_for_generation(refresh.generation, refresh.matches);
        SearchRefreshOutcome::Applied
    }

    pub(crate) fn invalidate_refresh(&mut self) {
        self.refresh_epoch = self.refresh_epoch.wrapping_add(1);
        self.refresh_pending = false;
    }

    pub(crate) fn take_refresh(&mut self) -> bool {
        std::mem::take(&mut self.refresh_pending)
    }

    pub(crate) fn refreshing(&self) -> bool {
        self.refresh_pending
    }

    pub fn next_match(&mut self) -> Option<SearchMatch> {
        if self.matches.is_empty() || self.is_composing() {
            return self.current_match();
        }
        self.current = (self.current + 1) % self.matches.len();
        self.current_match()
    }

    pub fn previous_match(&mut self) -> Option<SearchMatch> {
        if self.matches.is_empty() || self.is_composing() {
            return self.current_match();
        }
        self.current = (self.current + self.matches.len() - 1) % self.matches.len();
        self.current_match()
    }

    pub fn reset_to_first(&mut self) -> Option<SearchMatch> {
        self.current = 0;
        self.current_match()
    }

    pub fn clear(&mut self) {
        self.query.clear();
        self.composition = None;
        self.undo_history.clear();
        self.redo_history.clear();
        self.set_caret(0);
        self.invalidate_matches();
    }
}

fn sanitize_text(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

fn truncate_text(text: &mut String, limit: usize) {
    if text.len() <= limit {
        return;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

fn grapheme_boundary_at_or_before(text: &str, index: usize) -> usize {
    let index = index.min(text.len());
    if index == 0 || index == text.len() {
        return index;
    }
    text.grapheme_indices(true)
        .map(|(start, grapheme)| start + grapheme.len())
        .take_while(|end| *end <= index)
        .last()
        .unwrap_or(0)
}

fn grapheme_boundary_at_or_after(text: &str, index: usize) -> usize {
    let index = index.min(text.len());
    if index == 0 {
        return 0;
    }
    text.grapheme_indices(true)
        .map(|(start, grapheme)| start + grapheme.len())
        .find(|end| *end >= index)
        .unwrap_or(text.len())
}

fn utf16_range_to_grapheme_bytes(text: &str, range: Range<usize>) -> (usize, usize) {
    let start = grapheme_boundary_at_or_before(text, utf16_to_byte(text, range.start));
    let end = grapheme_boundary_at_or_after(text, utf16_to_byte_ceil(text, range.end));
    if start <= end {
        (start, end)
    } else {
        (end, start)
    }
}

fn byte_to_utf16(text: &str, byte: usize) -> usize {
    let byte = grapheme_boundary_at_or_before(text, byte);
    text[..byte].chars().map(char::len_utf16).sum()
}

fn utf16_to_byte(text: &str, utf16: usize) -> usize {
    let mut remaining = utf16;
    for (byte, ch) in text.char_indices() {
        let width = ch.len_utf16();
        if remaining < width {
            return byte;
        }
        remaining -= width;
        if remaining == 0 {
            return byte + ch.len_utf8();
        }
    }
    text.len()
}

fn utf16_to_byte_ceil(text: &str, utf16: usize) -> usize {
    if utf16 == 0 {
        return 0;
    }
    let mut remaining = utf16;
    for (byte, ch) in text.char_indices() {
        let width = ch.len_utf16();
        if remaining <= width {
            return byte + ch.len_utf8();
        }
        remaining -= width;
    }
    text.len()
}

/// Finds smart-case substring matches across logical lines and scrollback.
pub fn find_matches<T: EventListener>(term: &Term<T>, query: &str) -> Vec<SearchMatch> {
    find_matches_in_index(&SearchIndex::capture(term), query)
}

pub(crate) fn find_matches_in_index(index: &SearchIndex, query: &str) -> Vec<SearchMatch> {
    find_matches_in_index_after(index, query, || {})
}

fn find_matches_in_index_after(
    index: &SearchIndex,
    query: &str,
    before_scan: impl FnOnce(),
) -> Vec<SearchMatch> {
    before_scan();
    if query.is_empty() || query.len() > MAX_QUERY_BYTES {
        return Vec::new();
    }
    let data = &index.0;
    if data.columns == 0 {
        return Vec::new();
    }
    let query: Vec<char> = query.chars().collect();
    let case_sensitive = query.iter().any(|character| character.is_uppercase());
    let mut prefix = vec![0; query.len()];
    for index in 1..query.len() {
        let mut matched = prefix[index - 1];
        while matched > 0 && !char_eq(query[index], query[matched], case_sensitive) {
            matched = prefix[matched - 1];
        }
        if char_eq(query[index], query[matched], case_sensitive) {
            matched += 1;
        }
        prefix[index] = matched;
    }

    let mut matches = Vec::new();
    let mut starts = VecDeque::with_capacity(query.len());
    let mut matched = 0;
    let mut logical_start = true;
    'lines: for (line_offset, line) in data.lines.iter().enumerate() {
        if logical_start {
            starts.clear();
            matched = 0;
        }
        let line_number = Line(data.topmost_line.saturating_add(line_offset as i32));
        for cell in line.cells.iter() {
            if cell.barrier {
                starts.clear();
                matched = 0;
                continue;
            }
            while matched > 0 && !char_eq(cell.ch, query[matched], case_sensitive) {
                matched = prefix[matched - 1];
            }
            while starts.len() > matched {
                starts.pop_front();
            }
            if char_eq(cell.ch, query[matched], case_sensitive) {
                starts.push_back(SearchChar {
                    point: Point::new(line_number, Column(cell.column as usize)),
                    width: cell.width as usize,
                });
                matched += 1;
            }
            if matched == query.len() {
                let end = *starts.back().expect("query end");
                let start = starts.pop_front().expect("query start");
                matches.push(SearchMatch {
                    start: start.point,
                    end: Point::new(
                        end.point.line,
                        Column(
                            end.point
                                .column
                                .0
                                .saturating_add(end.width)
                                .saturating_sub(1)
                                .min(data.columns - 1),
                        ),
                    ),
                });
                starts.clear();
                matched = 0;
                if matches.len() == MAX_MATCHES {
                    break 'lines;
                }
            }
        }
        logical_start = !line.wrapped;
    }
    matches
}

#[derive(Clone, Copy)]
struct SearchChar {
    point: Point,
    width: usize,
}

fn char_eq(a: char, b: char, case_sensitive: bool) -> bool {
    a == b || (!case_sensitive && a.to_lowercase().eq(b.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::Processor;

    struct TestSize {
        columns: usize,
        lines: usize,
    }

    impl Dimensions for TestSize {
        fn total_lines(&self) -> usize {
            self.lines
        }
        fn screen_lines(&self) -> usize {
            self.lines
        }
        fn columns(&self) -> usize {
            self.columns
        }
    }

    fn term(columns: usize, lines: usize) -> Term<VoidListener> {
        Term::new(
            Config {
                scrolling_history: 200,
                ..Default::default()
            },
            &TestSize { columns, lines },
            VoidListener,
        )
    }

    fn feed(term: &mut Term<VoidListener>, text: &str) {
        let mut parser = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::default();
        parser.advance(term, text.as_bytes());
    }

    fn update_indexes(indexes: &mut SearchIndexes, term: &Term<VoidListener>) {
        let update = indexes.capture_update(term);
        indexes.apply_update(update);
    }

    fn search_match(line: i32) -> SearchMatch {
        SearchMatch {
            start: Point::new(Line(line), Column(0)),
            end: Point::new(Line(line), Column(0)),
        }
    }

    #[test]
    fn finds_all_occurrences_in_order() {
        let mut term = term(40, 4);
        feed(&mut term, "foo bar foo baz foo");
        let matches = find_matches(&term, "foo");
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].start.column, Column(0));
        assert_eq!(matches[1].start.column, Column(8));
        assert_eq!(matches[2].start.column, Column(16));
        assert!(matches[0].contains(Point::new(Line(0), Column(2))));
        assert!(!matches[0].contains(Point::new(Line(0), Column(3))));
    }

    #[test]
    fn hidden_cells_break_matches_without_exposing_content() {
        let mut term = term(20, 2);
        feed(&mut term, "a\x1b[8msecret\x1b[0mb");

        assert!(find_matches(&term, "ab").is_empty());
        assert!(find_matches(&term, "secret").is_empty());
        assert_eq!(find_matches(&term, "a").len(), 1);
        assert_eq!(find_matches(&term, "b")[0].start.column, Column(7));
    }

    #[test]
    fn smart_case_is_insensitive_without_uppercase() {
        let mut term = term(40, 4);
        feed(&mut term, "Hello HELLO hello");
        assert_eq!(find_matches(&term, "hello").len(), 3);
        assert_eq!(find_matches(&term, "Hello").len(), 1);
    }

    #[test]
    fn wide_chars_map_to_two_columns() {
        let mut term = term(20, 4);
        feed(&mut term, "你好世界");
        let matches = find_matches(&term, "好世");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].start, Point::new(Line(0), Column(2)));
        assert_eq!(matches[0].end, Point::new(Line(0), Column(5)));
    }

    #[test]
    fn zero_width_sequences_are_part_of_search_text() {
        let mut term = term(20, 4);
        feed(&mut term, "e\u{301} x");
        let matches = find_matches(&term, "e\u{301}");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].end.column, Column(0));
        assert_eq!(find_matches(&term, "\u{301}").len(), 1);
    }

    #[test]
    fn indexed_search_preserves_unicode_zero_width_and_wide_spans() {
        let mut term = term(12, 2);
        feed(&mut term, "e\u{301} 你好\r\n");
        let mut indexes = SearchIndexes::capture(&term);
        assert_eq!(
            find_matches_in_index(&indexes.active().unwrap(), "e\u{301}"),
            find_matches(&term, "e\u{301}")
        );

        feed(&mut term, "needle\r\n世界\r\n");
        update_indexes(&mut indexes, &term);
        let index = indexes.active().unwrap();
        assert_eq!(find_matches_in_index(&index, "e\u{301}").len(), 1);
        assert_eq!(find_matches_in_index(&index, "你好").len(), 1);
        let wide = find_matches_in_index(&index, "世界");
        assert_eq!(wide.len(), 1);
        assert_eq!(wide[0].start.column, Column(0));
        assert_eq!(wide[0].end.column, Column(3));
    }

    #[test]
    fn incremental_index_tracks_history_rotation_at_the_cap() {
        let size = TestSize {
            columns: 16,
            lines: 2,
        };
        let mut term = Term::new(
            Config {
                scrolling_history: 4,
                ..Default::default()
            },
            &size,
            VoidListener,
        );
        feed(&mut term, "a\r\nb\r\nc\r\nd\r\ne\r\nf\r\n");
        let mut indexes = SearchIndexes::capture(&term);
        feed(&mut term, "needle\r\n");
        update_indexes(&mut indexes, &term);
        let index = indexes.active().unwrap();
        for query in ["a", "b", "needle", "f"] {
            assert_eq!(
                find_matches_in_index(&index, query),
                find_matches(&term, query),
                "{query}"
            );
        }
        for output in ["g", "h", "i"] {
            feed(&mut term, &format!("{output}\r\n"));
            update_indexes(&mut indexes, &term);
            let index = indexes.active().unwrap();
            assert_eq!(
                find_matches_in_index(&index, output),
                find_matches(&term, output),
                "{output}"
            );
        }
        feed(&mut term, "j\r\nk\r\nl\r\nm\r\n");
        update_indexes(&mut indexes, &term);
        let index = indexes.active().unwrap();
        for query in ["i", "j", "k", "l", "m"] {
            assert_eq!(
                find_matches_in_index(&index, query),
                find_matches(&term, query),
                "{query}"
            );
        }
    }

    #[test]
    fn active_search_index_switches_with_the_terminal_screen() {
        let mut term = term(20, 2);
        feed(&mut term, "primary\r\n");
        let mut indexes = SearchIndexes::capture(&term);
        feed(&mut term, "\x1b[?1049halternate");
        update_indexes(&mut indexes, &term);
        assert_eq!(
            find_matches_in_index(&indexes.active().unwrap(), "alternate").len(),
            1
        );
        feed(&mut term, "\x1b[?1049l");
        update_indexes(&mut indexes, &term);
        assert_eq!(
            find_matches_in_index(&indexes.active().unwrap(), "primary").len(),
            1
        );
    }

    #[test]
    fn wide_char_at_last_column_is_clamped_to_the_viewport() {
        let mut term = term(4, 4);
        let cell = &mut term.grid_mut()[Line(0)][Column(3)];
        cell.c = '界';
        cell.flags.insert(Flags::WIDE_CHAR);
        let matches = find_matches(&term, "界");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].end.column, Column(3));
    }

    #[test]
    fn soft_wrapped_lines_match_as_one_logical_line() {
        let mut term = term(4, 4);
        feed(&mut term, "abcdefgh");
        let matches = find_matches(&term, "cdefg");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].start, Point::new(Line(0), Column(2)));
        assert_eq!(matches[0].end, Point::new(Line(1), Column(2)));
    }

    #[test]
    fn matches_in_scrollback_history() {
        let mut term = term(20, 2);
        feed(&mut term, "needle\r\nline\r\nmore\r\n");
        let matches = find_matches(&term, "needle");
        assert_eq!(matches.len(), 1);
        assert!(matches[0].start.line < Line(0), "{:?}", matches[0]);
    }

    #[test]
    fn non_overlapping_matches_advance_by_length() {
        let mut term = term(20, 4);
        feed(&mut term, "aaaa");
        assert_eq!(find_matches(&term, "aa").len(), 2);
    }

    #[test]
    fn indexed_kmp_keeps_only_the_active_prefix_after_mismatch() {
        let mut term = term(20, 4);
        feed(&mut term, "acb ababab");
        let index = SearchIndex::capture(&term);
        let matches = find_matches_in_index(&index, "ab");
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].start.column, Column(4));
        assert_eq!(find_matches_in_index(&index, "ababab").len(), 1);
    }

    #[test]
    fn match_count_is_capped() {
        let mut term = term(100, 200);
        let line = format!("{}\r\n", "a".repeat(100));
        feed(&mut term, &line.repeat(101));
        assert_eq!(find_matches(&term, "a").len(), MAX_MATCHES);
    }

    #[test]
    fn ten_thousand_line_background_search_does_not_hold_term_lock() {
        let size = TestSize {
            columns: 16,
            lines: 4,
        };
        let terminal = alacritty_terminal::sync::FairMutex::new(Term::new(
            Config {
                scrolling_history: MAX_INDEX_LINES,
                ..Default::default()
            },
            &size,
            VoidListener,
        ));
        feed(&mut terminal.lock(), &"needle\r\n".repeat(10_000));
        let index = Arc::new(SearchIndex::capture(&terminal.lock()));
        let started = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let scan = {
            let index = index.clone();
            let started = started.clone();
            let release = release.clone();
            std::thread::spawn(move || {
                find_matches_in_index_after(&index, "needle", move || {
                    started.wait();
                    release.wait();
                })
            })
        };
        started.wait();
        let mut output = terminal.lock();
        feed(&mut output, "live output\r\n");
        drop(output);
        release.wait();
        assert_eq!(scan.join().unwrap().len(), MAX_MATCHES);
    }

    #[test]
    fn query_and_wakeup_burst_coalesces_to_one_scan() {
        let mut state = SearchState::default();
        let mut scheduled = Vec::new();
        for index in 0..100 {
            if index % 2 == 0 {
                state.set_query(format!("q{index}"));
            }
            if let Some(epoch) = state.request_refresh() {
                scheduled.push(epoch);
            }
        }
        assert_eq!(scheduled.len(), 1);
        let scans = scheduled
            .into_iter()
            .filter(|epoch| state.finish_refresh(*epoch))
            .count();
        assert_eq!(scans, 1);
    }

    #[test]
    fn clear_cancels_pending_scan_and_resets_editing() {
        let mut state = SearchState::default();
        state.set_query("needle".into());
        state.set_selection(0..2);
        let epoch = state.request_refresh().unwrap();
        state.clear();
        assert!(!state.finish_refresh(epoch));
        assert!(state.request_refresh().is_none());
        assert_eq!(state.query(), "");
        assert_eq!(state.selection(), 0..0);
        assert_eq!(state.cursor(), 0);
        assert_eq!(state.count(), 0);
        assert_eq!(state.current_index(), None);
    }

    #[test]
    fn empty_query_has_a_zero_positioned_caret() {
        let state = SearchState::default();
        assert_eq!(state.display_text(), "");
        assert_eq!(state.display_selection(), 0..0);
        assert_eq!(state.display_cursor(), 0);
        assert!(state.caret_visible(true, false));
        assert!(!state.caret_visible(false, false));
    }

    #[test]
    fn selection_and_clear_reset_caret_blink() {
        let mut state = SearchState::default();
        assert!(state.tick_caret_blink());
        assert!(!state.caret_visible(true, false));

        state.set_query("a界".into());
        assert!(state.caret_visible(true, false));
        assert!(state.tick_caret_blink());
        state.set_selection_utf16(1..2);
        assert!(state.caret_visible(true, false));
        assert!(state.tick_caret_blink());
        state.clear();
        assert!(state.caret_visible(true, false));
    }

    #[test]
    fn search_input_sanitizes_control_characters() {
        let mut state = SearchState::default();
        state.insert_text("a界\n\u{7f}");
        assert_eq!(state.query(), "a界  ");
        assert_eq!(state.cursor(), state.query().len());
    }

    #[test]
    fn changing_query_drops_stale_match_indices() {
        let mut state = SearchState::default();
        state.set_query("old".into());
        state.set_matches(vec![search_match(0), search_match(1)]);
        state.next_match();
        state.set_query("new".into());
        assert_eq!(state.count(), 0);
        assert_eq!(state.current_index(), None);
    }

    #[test]
    fn selection_can_be_replaced_as_graphemes() {
        let mut state = SearchState::default();
        state.set_query("a👩‍💻b".into());
        state.set_selection(0..("a👩‍💻".len()));
        assert_eq!(state.selected_text(), Some("a👩‍💻"));
        state.insert_text("界");
        assert_eq!(state.query(), "界b");
        assert_eq!(state.selection(), 3..3);
    }

    #[test]
    fn search_editing_supports_home_end_backspace_and_delete() {
        let mut state = SearchState::default();
        state.set_query("a界b".into());
        state.home();
        state.delete();
        assert_eq!(state.query(), "界b");
        state.end();
        state.backspace();
        assert_eq!(state.query(), "界");
        state.home();
        state.move_right(false);
        state.set_selection(0..state.cursor);
        state.insert_text("x");
        assert_eq!(state.query(), "x");
    }

    #[test]
    fn selection_utf16_round_trips_astral_text() {
        let mut state = SearchState::default();
        state.set_query("a🎉界".into());
        state.set_selection_utf16(1..3);
        assert_eq!(state.selected_text(), Some("🎉"));
        assert_eq!(state.selection_utf16(), 1..3);
    }

    #[test]
    fn reversed_astral_selection_reports_utf16_direction() {
        let mut state = SearchState::default();
        state.set_query("a🎉界".into());
        let end = state.query().len();
        state.set_selection_with_anchor(end, 0);
        assert_eq!(state.selected_text_range_utf16(), (0..4, true));
    }

    #[test]
    fn gpui_utf16_ranges_map_to_cjk_and_astral_query_bytes() {
        let mut state = SearchState::default();
        state.set_query("a界👩‍💻🙂b".into());

        state.set_selection_utf16(0..0);
        assert_eq!(state.selection(), 0..0);
        assert_eq!(state.selection_utf16(), 0..0);

        state.set_selection_utf16(3..4);
        assert_eq!(state.selected_text(), Some("👩‍💻"));
        assert_eq!(state.selection(), 4..15);
        assert_eq!(state.selection_utf16(), 2..7);
        assert_eq!(state.byte_range_from_utf16(2..7), 4..15);
        assert_eq!(
            state.text_for_utf16_range(3..4),
            Some(("👩‍💻".to_owned(), 2..7))
        );

        state.set_selection_utf16(7..8);
        assert_eq!(state.selected_text(), Some("🙂"));
        assert_eq!(state.selection(), 15..19);
        assert_eq!(state.selection_utf16(), 7..9);
    }

    #[test]
    fn commit_and_composition_ranges_are_utf16_before_query_bytes() {
        let mut state = SearchState::default();
        state.set_query("a界🙂b".into());
        state.commit_text(Some(2..4), "X");
        assert_eq!(state.query(), "a界Xb");

        state.replace_and_mark_text(Some(1..2), "你", Some(0..1));
        assert_eq!(state.query(), "a界Xb");
        assert_eq!(state.display_text(), "a你Xb");
        assert_eq!(state.marked_range_utf16(), Some(1..2));
        assert_eq!(state.composition_selected_utf16(), Some(1..2));
        state.commit_text(Some(1..2), "好");
        assert_eq!(state.query(), "a好Xb");
        assert_eq!(state.marked_range_utf16(), None);
    }

    #[test]
    fn composition_selection_and_navigation_do_not_leak_into_query() {
        let mut state = SearchState::default();
        state.set_query("a界b".into());
        state.set_selection(1..4);
        state.set_matches(vec![search_match(0), search_match(1)]);
        state.replace_and_mark_text(None, "中🙂", Some(0..3));
        state.set_selection_utf16(1..4);
        assert_eq!(state.selected_display_text().as_deref(), Some("中🙂"));
        assert_eq!(state.current_index(), Some(0));
        assert_eq!(
            state.next_match().map(|item| item.start.line),
            Some(Line(0))
        );
        assert_eq!(
            state.previous_match().map(|item| item.start.line),
            Some(Line(0))
        );

        assert!(state.discard_composition());
        assert_eq!(state.query(), "a界b");
        assert_eq!(state.display_text(), "a界b");
        assert_eq!(state.selection(), 1..4);
        assert_eq!(state.cursor(), 4);
        assert_eq!(state.marked_range_utf16(), None);
        assert_eq!(state.current_index(), Some(0));
        assert_eq!(
            state.next_match().map(|item| item.start.line),
            Some(Line(1))
        );
    }

    #[test]
    fn search_history_restores_cjk_and_emoji_edits() {
        let mut state = SearchState::default();
        state.set_query("a".into());
        state.insert_text("界");
        state.insert_text("👩‍💻");
        assert_eq!(state.query(), "a界👩‍💻");
        assert!(state.undo());
        assert_eq!(state.query(), "a界");
        assert!(state.undo());
        assert_eq!(state.query(), "a");
        assert!(state.redo());
        assert_eq!(state.query(), "a界");
        assert!(state.redo());
        assert_eq!(state.query(), "a界👩‍💻");
    }

    #[test]
    fn cut_and_standard_history_preserve_cjk_selections() {
        let mut state = SearchState::default();
        state.set_query("甲🙂乙".into());
        state.select_all();
        assert_eq!(state.cut().as_deref(), Some("甲🙂乙"));
        assert_eq!(state.query(), "");
        assert!(state.undo());
        assert_eq!(state.query(), "甲🙂乙");
        assert_eq!(state.selection(), 0.."甲🙂乙".len());
        assert!(state.redo());
        assert_eq!(state.query(), "");
    }

    #[test]
    fn match_results_are_scoped_to_term_generation() {
        let mut state = SearchState::default();
        state.set_matches_for_generation(7, vec![search_match(0)]);
        assert_eq!(
            state.matches_for_generation(7).map(|matches| matches.len()),
            Some(1)
        );
        assert!(state.matches_for_generation(8).is_none());
        state.clear_results();
        assert!(state.matches_for_generation(7).is_none());
    }

    #[test]
    fn stale_generation_and_epoch_refreshes_are_rejected() {
        let mut state = SearchState::default();
        state.set_query("needle".into());
        let stale_epoch = state.request_refresh().unwrap();
        state.invalidate_refresh();
        let epoch = state.request_refresh().unwrap();
        assert_eq!(
            state.apply_refresh(
                SearchRefresh {
                    epoch: stale_epoch,
                    generation: 7,
                    query: "needle".into(),
                    matches: vec![search_match(6)],
                },
                7,
            ),
            SearchRefreshOutcome::Superseded
        );
        assert_eq!(
            state.apply_refresh(
                SearchRefresh {
                    epoch,
                    generation: 7,
                    query: "needle".into(),
                    matches: vec![search_match(6)],
                },
                8,
            ),
            SearchRefreshOutcome::Stale
        );
        assert!(state.matches_for_generation(7).is_none());
        assert!(state.request_refresh().is_some());
    }

    #[test]
    fn refresh_preserves_the_current_match_index() {
        let mut state = SearchState::default();
        state.set_query("needle".into());
        state.set_matches(vec![search_match(0), search_match(1)]);
        state.next_match();
        state.set_matches(vec![search_match(-1), search_match(0), search_match(1)]);
        assert_eq!(state.current_index(), Some(2));
    }

    #[test]
    fn state_cycles_through_matches() {
        let mut state = SearchState::default();
        state.set_query("x".into());
        state.set_matches(vec![
            SearchMatch {
                start: Point::new(Line(0), Column(0)),
                end: Point::new(Line(0), Column(0)),
            },
            SearchMatch {
                start: Point::new(Line(1), Column(0)),
                end: Point::new(Line(1), Column(0)),
            },
        ]);
        assert_eq!(state.current_display_index(), Some(1));
        assert_eq!(state.next_match().map(|m| m.start.line), Some(Line(1)));
        assert_eq!(state.current_display_index(), Some(2));
        assert_eq!(state.next_match().map(|m| m.start.line), Some(Line(0)));
        assert_eq!(state.previous_match().map(|m| m.start.line), Some(Line(1)));
    }

    #[test]
    fn empty_query_has_no_matches() {
        let mut term = term(20, 4);
        feed(&mut term, "anything");
        assert!(find_matches(&term, "").is_empty());
        let mut state = SearchState::default();
        state.set_matches(find_matches(&term, ""));
        assert_eq!(state.count(), 0);
        assert_eq!(state.next_match(), None);
        assert_eq!(state.current_display_index(), None);
    }

    #[test]
    fn shift_selection_keeps_anchor_and_direction() {
        let mut state = SearchState::default();
        state.set_query("abcd".into());
        state.set_selection_with_anchor(3, 3);
        state.move_left(true);
        state.move_left(true);
        assert_eq!(state.anchor(), 3);
        assert_eq!(state.cursor(), 1);
        assert_eq!(state.selection(), 1..3);
        assert!(state.selection_reversed());
        state.move_right(true);
        assert_eq!(state.cursor(), 2);
        assert_eq!(state.selection(), 2..3);
        assert!(state.selection_reversed());
        assert_eq!(state.selected_text_range_utf16(), (2..3, true));
    }

    #[test]
    fn composition_caret_uses_grapheme_and_utf16_boundaries() {
        let mut state = SearchState::default();
        state.set_query("ab".into());
        state.set_selection(1..1);
        state.replace_and_mark_text(None, "x👩‍💻y", Some(2..3));

        let text = state.display_text();
        let grapheme = "👩‍💻";
        let start = text.find(grapheme).unwrap();
        let end = start + grapheme.len();
        assert_eq!(state.display_selection(), start..end);
        assert_eq!(state.display_cursor(), end);
        assert_eq!(state.composition_selected_utf16(), Some(2..7));
        assert_eq!(state.display_byte_for_utf16(3), start);
        assert_eq!(state.display_utf16_for_byte(start + 1), 2);
    }

    #[test]
    fn ime_updates_reset_caret_blink() {
        let mut state = SearchState::default();
        state.replace_and_mark_text(None, "zh", None);
        assert!(state.caret_visible(true, false));
        assert!(state.tick_caret_blink());
        state.replace_and_mark_text(None, "中", None);
        assert!(state.caret_visible(true, false));
        assert!(state.tick_caret_blink());
        state.commit_text(None, "中");
        assert!(state.caret_visible(true, false));
    }

    #[test]
    fn ime_composition_is_marked_and_committed_once() {
        let mut state = SearchState::default();
        state.set_query("ab".into());
        state.set_selection(1..1);
        state.replace_and_mark_text(None, "zh", None);
        assert_eq!(state.query(), "ab");
        assert_eq!(state.display_text(), "azhb");
        assert_eq!(state.marked_range_utf16(), Some(1..3));
        state.replace_and_mark_text(None, "中", None);
        assert_eq!(state.query(), "ab");
        assert_eq!(state.marked_range_utf16(), Some(1..2));
        state.commit_text(Some(1..3), "中");
        assert_eq!(state.query(), "a中b");
        assert_eq!(state.display_text(), "a中b");
        assert_eq!(state.marked_range_utf16(), None);
        assert_eq!(state.count(), 0);
    }

    #[test]
    fn ime_unmark_discards_only_the_composition() {
        let mut state = SearchState::default();
        state.set_query("ab".into());
        state.replace_and_mark_text(None, "中", None);
        assert!(state.discard_composition());
        assert_eq!(state.query(), "ab");
        assert_eq!(state.display_text(), "ab");
        assert_eq!(state.marked_range_utf16(), None);
    }

    #[test]
    fn search_query_length_is_capped() {
        let mut state = SearchState::default();
        state.insert_text(&"a".repeat(MAX_QUERY_BYTES + 128));
        assert_eq!(state.query().len(), MAX_QUERY_BYTES);
        let mut term = term(20, 4);
        feed(&mut term, &"a".repeat(MAX_QUERY_BYTES + 128));
        assert!(find_matches(&term, &"a".repeat(MAX_QUERY_BYTES + 1)).is_empty());
    }
}
