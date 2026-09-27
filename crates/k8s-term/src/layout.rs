//! Converts terminal grid content into drawable line layouts.

use std::collections::HashMap;
use std::sync::Arc;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{RenderableContent, RenderableCursor};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use gpui::{Hsla, StrikethroughStyle, UnderlineStyle, px};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::palette::Palette;
use crate::search::SearchMatch;

/// Subcell grid for block glyphs: 8 columns by 24 rows per cell.
pub const BLOCK_SUBCELL_COLUMNS: i32 = 8;
pub const BLOCK_SUBCELL_LINES: i32 = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellStyle {
    pub bold: bool,
    pub italic: bool,
    pub fg: Hsla,
    pub underline: Option<UnderlineStyle>,
    pub strikethrough: Option<StrikethroughStyle>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextRunLayout {
    pub col: usize,
    pub cells: usize,
    pub text: String,
    pub style: CellStyle,
}

/// Inclusive background column range on one line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BackgroundSpan {
    pub start: usize,
    pub end: usize,
    pub color: Hsla,
}

/// Block rectangle in subcell coordinates relative to the viewport.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlockRect {
    pub col: i32,
    pub line: i32,
    pub columns: i32,
    pub lines: i32,
    pub color: Hsla,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SelectionSpan {
    pub start: usize,
    pub end: usize,
}

/// Inclusive search match columns on one line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SearchSpan {
    pub start: usize,
    pub end: usize,
    pub current: bool,
}

/// Search matches and the current match index.
#[derive(Clone, Debug, Default)]
pub struct SearchHighlights {
    pub matches: Arc<[SearchMatch]>,
    pub current: Option<usize>,
}

/// IME preview position in viewport coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct PreeditLayout {
    pub text: String,
    pub line: usize,
    pub col: usize,
    pub cells: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LineLayout {
    pub backgrounds: Vec<BackgroundSpan>,
    pub blocks: Vec<BlockRect>,
    pub runs: Vec<TextRunLayout>,
    pub selection: Vec<SelectionSpan>,
    pub search: Vec<SearchSpan>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CursorLayout {
    pub line: usize,
    pub col: usize,
    pub shape: CursorShape,
    pub text: String,
    pub cells: usize,
    pub color: Hsla,
    pub text_color: Hsla,
}

#[derive(Clone, Debug)]
pub struct GridLayout {
    pub lines: Vec<LineLayout>,
    pub cursor: Option<CursorLayout>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RegionKey {
    start_col: usize,
    end_col: usize,
    color: Hsla,
}

type Region = (usize, usize, usize, usize, Hsla);

/// Merges adjacent regions with the same color and columns.
fn merge_regions(regions: impl IntoIterator<Item = Region>) -> Vec<Region> {
    let mut merged: Vec<Region> = Vec::new();
    let mut last_by_key: HashMap<RegionKey, usize> = HashMap::new();

    for (start_line, end_line, start_col, end_col, color) in regions {
        let key = RegionKey {
            start_col,
            end_col,
            color,
        };
        if let Some(&index) = last_by_key.get(&key)
            && merged[index].1 + 1 == start_line
        {
            merged[index].1 = end_line;
            continue;
        }
        merged.push((start_line, end_line, start_col, end_col, color));
        last_by_key.insert(key, merged.len() - 1);
    }

    merged
}

/// Visible cell snapshot copied while the `Term` lock is held.
#[derive(Clone)]
pub struct CellSnapshot {
    cells: Vec<(Point, Cell)>,
    selection: Option<SelectionRange>,
    cursor: RenderableCursor,
    colors: Colors,
    display_offset: usize,
}

impl CellSnapshot {
    pub fn capture<T: EventListener>(term: &Term<T>) -> Self {
        let mut content = term.renderable_content();
        Self::from_content(&mut content)
    }

    pub fn from_content(content: &mut RenderableContent<'_>) -> Self {
        let cells = content
            .display_iter
            .by_ref()
            .map(|indexed| (indexed.point, indexed.cell.clone()))
            .collect();
        Self {
            cells,
            selection: content.selection,
            cursor: content.cursor,
            colors: *content.colors,
            display_offset: content.display_offset,
        }
    }

    pub fn cells(&self) -> impl Iterator<Item = (Point, &Cell)> {
        self.cells.iter().map(|(point, cell)| (*point, cell))
    }
}

fn selection_contains(range: &SelectionRange, point: Point, cell: &Cell) -> bool {
    if range.contains(point) {
        return true;
    }
    cell.flags.contains(Flags::WIDE_CHAR)
        && range.contains(Point::new(
            point.line,
            Column(point.column.0.saturating_add(1)),
        ))
}

fn is_default_background(color: Color) -> bool {
    matches!(color, Color::Named(NamedColor::Background))
}

fn normalize_text_point(grid: &Grid<Cell>, point: Point) -> Point {
    let cell = &grid[point];
    if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) && point.line < grid.bottommost_line() {
        Point::new(Line(point.line.0 + 1), Column(0))
    } else if cell.flags.contains(Flags::WIDE_CHAR_SPACER) && point.column.0 > 0 {
        Point::new(point.line, Column(point.column.0 - 1))
    } else {
        point
    }
}

pub(crate) fn cell_is_hidden(cell: &Cell) -> bool {
    cell.flags.contains(Flags::HIDDEN)
        || cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
}

fn text_char(grid: &Grid<Cell>, line: Line, column: usize) -> Option<char> {
    let cell = &grid[Point::new(line, Column(column))];
    (!cell_is_hidden(cell)).then_some(cell.c)
}

pub fn text_cluster_display_cells(text: &str) -> usize {
    let mut width = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = if grapheme.contains('\u{200d}') {
            grapheme
                .split('\u{200d}')
                .map(UnicodeWidthStr::width)
                .max()
                .unwrap_or(0)
        } else if grapheme.contains('\u{fe0e}') || grapheme.contains('\u{fe0f}') {
            let base: String = grapheme
                .chars()
                .filter(|ch| !matches!(ch, '\u{fe0e}' | '\u{fe0f}'))
                .collect();
            UnicodeWidthStr::width(base.as_str())
        } else {
            UnicodeWidthStr::width(grapheme)
        };
        width += grapheme_width.max(1);
    }
    width.max(1)
}

pub(crate) fn cell_columns(cell: &Cell, column: usize, columns: usize) -> usize {
    if column >= columns {
        0
    } else if cell.flags.contains(Flags::WIDE_CHAR) && column.saturating_add(1) < columns {
        2
    } else {
        1
    }
}

pub(crate) fn append_cell_text(text: &mut String, cell: &Cell, cell_width: usize) {
    if cell.flags.contains(Flags::HIDDEN) {
        text.extend(std::iter::repeat_n(' ', cell_width));
    } else {
        text.push(cell.c);
        if let Some(zerowidth) = cell.zerowidth() {
            text.extend(zerowidth.iter().copied());
        }
    }
}

fn flush_run(run: &mut Option<(usize, TextRunLayout)>, lines: &mut [LineLayout]) {
    if let Some((row, run)) = run.take()
        && let Some(line) = lines.get_mut(row)
    {
        line.runs.push(run);
    }
}

/// Lays out terminal text, backgrounds, selection, search, blocks, and the cursor.
pub fn layout_grid(
    snapshot: &CellSnapshot,
    palette: &Palette,
    rows: usize,
    columns: usize,
    hover: Option<(Point, Point, Hsla)>,
    search: Option<&SearchHighlights>,
) -> GridLayout {
    let display_offset = i32::try_from(snapshot.display_offset).unwrap_or(i32::MAX);
    let mut lines: Vec<LineLayout> = (0..rows).map(|_| LineLayout::default()).collect();
    let mut bg_regions: Vec<Region> = Vec::new();
    let mut block_regions: Vec<Region> = Vec::new();
    let mut open_run: Option<(usize, TextRunLayout)> = None;
    let mut pending_spaces: Option<(usize, usize, usize)> = None;
    let mut open_selection: Option<(usize, usize, usize)> = None;
    let mut cursor_text = None;
    let mut cursor_cells = 1;

    for (point, cell) in snapshot.cells() {
        let row = point.line.0.saturating_add(display_offset);
        if row < 0 || row as usize >= rows {
            continue;
        }
        let row = row as usize;
        if open_run
            .as_ref()
            .is_some_and(|(open_row, _)| *open_row != row)
        {
            flush_run(&mut open_run, &mut lines);
            pending_spaces = None;
        }
        let col = point.column.0;
        let cell_width = cell_columns(cell, col, columns);
        if cell_width == 0 {
            flush_run(&mut open_run, &mut lines);
            pending_spaces = None;
            if let Some((selection_row, start, end)) = open_selection.take() {
                lines[selection_row]
                    .selection
                    .push(SelectionSpan { start, end });
            }
            continue;
        }

        let block = !cell_is_hidden(cell) && is_block_char(cell.c);
        if point == snapshot.cursor.point && !block && !cell_is_hidden(cell) {
            cursor_cells = cell_width;
            let mut text = String::new();
            text.push(cell.c);
            if let Some(zerowidth) = cell.zerowidth() {
                text.extend(zerowidth.iter().copied());
            }
            cursor_text = Some(text);
        }

        let selected = snapshot
            .selection
            .as_ref()
            .is_some_and(|range| selection_contains(range, point, cell));
        if selected {
            match &mut open_selection {
                Some((selection_row, _, end)) if *selection_row == row && *end + 1 == col => {
                    *end = col + cell_width - 1;
                }
                _ => {
                    if let Some((selection_row, start, end)) = open_selection.take() {
                        lines[selection_row]
                            .selection
                            .push(SelectionSpan { start, end });
                    }
                    open_selection = Some((row, col, col + cell_width - 1));
                }
            }
        } else if let Some((selection_row, start, end)) = open_selection.take() {
            lines[selection_row]
                .selection
                .push(SelectionSpan { start, end });
        }

        let inverse = cell.flags.contains(Flags::INVERSE);
        let fg_color = if inverse { cell.bg } else { cell.fg };
        let bg_color = if inverse { cell.fg } else { cell.bg };
        let bg = palette.resolve(bg_color, &snapshot.colors);
        if !is_default_background(bg_color) {
            match bg_regions.last_mut() {
                Some((region_line, _, start, end, color))
                    if *region_line == row && *end + 1 == col && *color == bg =>
                {
                    *end = col + cell_width - 1;
                }
                _ => bg_regions.push((row, row, col, col + cell_width - 1, bg)),
            }
        }

        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }

        if is_blank(cell) {
            let can_buffer = open_run
                .as_ref()
                .is_some_and(|(open_row, run)| *open_row == row && run.col + run.cells == col);
            if can_buffer {
                let pending = pending_spaces.get_or_insert((row, col, 0));
                if pending.0 == row && pending.1 + pending.2 == col {
                    pending.2 += cell_width;
                } else {
                    *pending = (row, col, cell_width);
                }
            } else {
                pending_spaces = None;
            }
            continue;
        }

        let fg = palette.resolve(fg_color, &snapshot.colors);
        let mut fg = palette.adjust_fg(fg, bg);
        if cell.flags.contains(Flags::DIM) {
            fg.a *= palette.dim_opacity;
        }
        let underline_color = cell
            .underline_color()
            .map(|color| palette.resolve(color, &snapshot.colors))
            .unwrap_or(fg);
        let underline = cell
            .flags
            .intersects(Flags::ALL_UNDERLINES)
            .then(|| UnderlineStyle {
                thickness: px(1.0),
                color: Some(underline_color),
                wavy: cell.flags.contains(Flags::UNDERCURL),
            });
        let strikethrough = cell
            .flags
            .contains(Flags::STRIKEOUT)
            .then(|| StrikethroughStyle {
                thickness: px(1.0),
                color: Some(fg),
            });
        let hovered = hover
            .as_ref()
            .is_some_and(|(start, end, _)| point_in_range(point, *start, *end));
        let underline = if hovered {
            let color = hover.as_ref().map(|(_, _, color)| *color);
            Some(UnderlineStyle {
                thickness: px(1.0),
                color,
                wavy: false,
            })
        } else {
            underline
        };

        let style = CellStyle {
            bold: cell.flags.contains(Flags::BOLD),
            italic: cell.flags.contains(Flags::ITALIC),
            fg,
            underline,
            strikethrough,
        };

        if block {
            pending_spaces = None;
            flush_run(&mut open_run, &mut lines);
            collect_block_regions(
                Point::new(Line(row as i32), Column(col)),
                cell.c,
                fg,
                &mut block_regions,
            );
            continue;
        }

        if let Some((pending_row, pending_col, pending_cells)) = pending_spaces.take() {
            let can_append = open_run.as_ref().is_some_and(|(open_row, run)| {
                *open_row == pending_row && run.col + run.cells == pending_col && run.style == style
            });
            if can_append {
                let (_, run) = open_run.as_mut().expect("run opened above");
                run.text.extend(std::iter::repeat_n(' ', pending_cells));
                run.cells += pending_cells;
            }
        }

        let continues = open_run.as_ref().is_some_and(|(open_row, run)| {
            *open_row == row && run.col + run.cells == col && run.style == style
        });
        if continues {
            let (_, run) = open_run.as_mut().expect("run opened above");
            append_cell_text(&mut run.text, cell, cell_width);
            run.cells += cell_width;
        } else {
            flush_run(&mut open_run, &mut lines);
            let mut text = String::new();
            append_cell_text(&mut text, cell, cell_width);
            open_run = Some((
                row,
                TextRunLayout {
                    col,
                    cells: cell_width,
                    text,
                    style,
                },
            ));
        }
    }

    flush_run(&mut open_run, &mut lines);
    if let Some((selection_row, start, end)) = open_selection.take() {
        lines[selection_row]
            .selection
            .push(SelectionSpan { start, end });
    }

    for (start_line, end_line, start_col, end_col, color) in merge_regions(bg_regions) {
        for line in lines.iter_mut().take(end_line + 1).skip(start_line) {
            line.backgrounds.push(BackgroundSpan {
                start: start_col,
                end: end_col,
                color,
            });
        }
    }
    for (start_line, end_line, start_col, end_col, color) in merge_regions(block_regions) {
        if let Some(first_line) = lines.get_mut(start_line / BLOCK_SUBCELL_LINES as usize) {
            first_line.blocks.push(BlockRect {
                col: start_col as i32,
                line: start_line as i32,
                columns: (end_col - start_col + 1) as i32,
                lines: (end_line - start_line + 1) as i32,
                color,
            });
        }
    }

    if let Some(search) = search
        && columns > 0
    {
        let last_column = columns - 1;
        for (index, search_match) in search.matches.iter().enumerate() {
            let current = search.current == Some(index);
            for line in search_match.start.line.0..=search_match.end.line.0 {
                let row = line.saturating_add(display_offset);
                if row < 0 || row as usize >= rows {
                    continue;
                }
                let start = if line == search_match.start.line.0 {
                    search_match.start.column.0.min(last_column)
                } else {
                    0
                };
                let end = if line == search_match.end.line.0 {
                    search_match.end.column.0.min(last_column)
                } else {
                    last_column
                };
                if start > end {
                    continue;
                }
                lines[row as usize].search.push(SearchSpan {
                    start,
                    end,
                    current,
                });
            }
        }
    }

    let cursor_point = snapshot.cursor.point;
    let cursor_line = cursor_point.line.0.saturating_add(display_offset);
    let cursor_column = cursor_point.column.0.min(columns.saturating_sub(1));
    let cursor_cells = cursor_cells.min(columns.saturating_sub(cursor_column).max(1));
    let cursor = (cursor_line >= 0 && (cursor_line as usize) < rows).then(|| CursorLayout {
        line: cursor_line as usize,
        col: cursor_column,
        shape: snapshot.cursor.shape,
        text: cursor_text.unwrap_or_else(|| " ".to_owned()),
        cells: cursor_cells.max(1),
        color: palette.cursor,
        text_color: palette.background,
    });

    GridLayout { lines, cursor }
}

fn point_in_range(point: Point, start: Point, end: Point) -> bool {
    (point.line > start.line || (point.line == start.line && point.column >= start.column))
        && (point.line < end.line || (point.line == end.line && point.column <= end.column))
}

/// Returns true for an unstyled space with no text extras.
fn is_blank(cell: &Cell) -> bool {
    !cell.flags.contains(Flags::HIDDEN)
        && cell.c == ' '
        && is_default_background(cell.bg)
        && cell.hyperlink().is_none()
        && !cell.flags.intersects(
            Flags::INVERSE
                | Flags::ALL_UNDERLINES
                | Flags::STRIKEOUT
                | Flags::BOLD
                | Flags::ITALIC
                | Flags::DIM,
        )
        && cell
            .zerowidth()
            .is_none_or(|zerowidth| zerowidth.is_empty())
}

fn block_char_to_rect(ch: char) -> Option<(i32, i32, i32, i32)> {
    let codepoint = ch as u32;
    Some(match codepoint {
        // Upper half block.
        0x2580 => (0, 0, 8, 12),
        // Lower blocks from one eighth to full height.
        0x2581..=0x2588 => {
            let eighths = (codepoint - 0x2580) as i32;
            (0, 24 - eighths * 3, 8, eighths * 3)
        }
        // Left blocks from seven eighths to one eighth.
        0x2589..=0x258F => (0, 0, (0x2590 - codepoint) as i32, 24),
        // Right half block.
        0x2590 => (4, 0, 4, 24),
        // Upper one eighth block.
        0x2594 => (0, 0, 8, 3),
        // Right one eighth block.
        0x2595 => (7, 0, 1, 24),
        _ => return None,
    })
}

fn quadrant_char_to_filled_bits(ch: char) -> Option<u8> {
    Some(match ch {
        '▘' => 0b0001,
        '▝' => 0b0010,
        '▖' => 0b0100,
        '▗' => 0b1000,
        '▚' => 0b1001,
        '▞' => 0b0110,
        '▛' => 0b0111,
        '▜' => 0b1011,
        '▙' => 0b1101,
        '▟' => 0b1110,
        _ => return None,
    })
}

fn sextant_char_to_filled_bits(ch: char) -> Option<u8> {
    let offset = (ch as u32).checked_sub(0x1FB00)?;
    if offset > 0x3B {
        return None;
    }
    Some((offset + 1 + u32::from(offset >= 20) + u32::from(offset >= 40)) as u8)
}

fn shade_char_to_opacity(ch: char) -> Option<f32> {
    match ch {
        '░' => Some(0.25),
        '▒' => Some(0.5),
        '▓' => Some(0.75),
        _ => None,
    }
}

fn is_block_char(ch: char) -> bool {
    block_char_to_rect(ch).is_some()
        || quadrant_char_to_filled_bits(ch).is_some()
        || sextant_char_to_filled_bits(ch).is_some()
        || shade_char_to_opacity(ch).is_some()
}

fn collect_block_regions(point: Point, ch: char, color: Hsla, regions: &mut Vec<Region>) -> bool {
    if let Some((column, line, columns, lines)) = block_char_to_rect(ch) {
        push_block_region(point, column, line, columns, lines, color, regions);
        return true;
    }
    if let Some(filled) = quadrant_char_to_filled_bits(ch) {
        for row in 0..2 {
            for column in 0..2 {
                if filled & (1 << (row * 2 + column)) != 0 {
                    push_block_region(point, column * 4, row * 12, 4, 12, color, regions);
                }
            }
        }
        return true;
    }
    if let Some(filled) = sextant_char_to_filled_bits(ch) {
        for row in 0..3 {
            for column in 0..2 {
                if filled & (1 << (row * 2 + column)) != 0 {
                    push_block_region(point, column * 4, row * 8, 4, 8, color, regions);
                }
            }
        }
        return true;
    }
    if let Some(opacity) = shade_char_to_opacity(ch) {
        push_block_region(point, 0, 0, 8, 24, color.opacity(opacity), regions);
        return true;
    }
    false
}

fn push_block_region(
    point: Point,
    column: i32,
    line: i32,
    columns: i32,
    lines: i32,
    color: Hsla,
    regions: &mut Vec<Region>,
) {
    let start_line = (point.line.0 * BLOCK_SUBCELL_LINES + line) as usize;
    let start_col = (point.column.0 as i32 * BLOCK_SUBCELL_COLUMNS + column) as usize;
    let end_line = start_line + lines as usize - 1;
    let end_col = start_col + columns as usize - 1;
    regions.push((start_line, end_line, start_col, end_col, color));
}

/// Hashes line text for the GPUI shape cache.
pub fn text_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Full grid range of the hyperlink under the cursor.
#[derive(Clone, Debug, PartialEq)]
pub struct HyperlinkSpan {
    pub start: Point,
    pub end: Point,
    pub uri: String,
}

/// Finds an OSC 8 hyperlink and expands it to adjacent cells with the same URI.
pub fn hyperlink_at<T: EventListener>(term: &Term<T>, point: Point) -> Option<HyperlinkSpan> {
    let grid = term.grid();
    let columns = term.columns();
    if point.column.0 >= columns {
        return None;
    }
    let point = normalize_text_point(grid, point);
    let column = point.column.0;
    if column >= columns {
        return None;
    }
    let hyperlink = grid[point].hyperlink()?;
    let uri = hyperlink.uri().to_owned();
    let line = point.line;

    let same = |column: usize| -> bool {
        let cell = &grid[Point::new(line, Column(column))];
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) && column > 0 {
            grid[Point::new(line, Column(column - 1))]
                .hyperlink()
                .is_some_and(|link| link.uri() == uri)
        } else {
            cell.hyperlink().is_some_and(|link| link.uri() == uri)
        }
    };

    let mut start = column;
    while start > 0 && same(start - 1) {
        start -= 1;
    }
    let mut end = column;
    while end + 1 < columns && same(end + 1) {
        end += 1;
    }
    Some(HyperlinkSpan {
        start: Point::new(line, Column(start)),
        end: Point::new(line, Column(end)),
        uri,
    })
}

/// Finds an HTTP or HTTPS URL and removes punctuation at both ends.
pub fn url_span_at<T: EventListener>(
    term: &Term<T>,
    point: Point,
) -> Option<(Point, Point, String)> {
    let grid = term.grid();
    let columns = term.columns();
    if point.column.0 >= columns {
        return None;
    }
    let point = normalize_text_point(grid, point);
    let column = point.column.0;
    if column >= columns {
        return None;
    }
    let line = point.line;
    let char_at = |column| text_char(grid, line, column);

    let mut start = column;
    while start > 0 {
        let Some(ch) = char_at(start - 1) else {
            break;
        };
        if ch.is_whitespace() {
            break;
        }
        start -= 1;
    }
    let mut end = column;
    while end + 1 < columns {
        let Some(ch) = char_at(end + 1) else {
            break;
        };
        if ch.is_whitespace() {
            break;
        }
        end += 1;
    }

    let mut text = String::new();
    let mut text_columns = Vec::new();
    let mut last_content = start;
    for current in start..=end {
        let Some(ch) = char_at(current) else {
            continue;
        };
        let cell = &grid[Point::new(line, Column(current))];
        text.push(ch);
        text_columns.push(current);
        for zerowidth in cell.zerowidth().into_iter().flatten() {
            text.push(*zerowidth);
            text_columns.push(current);
        }
        last_content = current;
    }
    if text.is_empty() {
        return None;
    }
    let leading = text.len() - text.trim_start_matches(is_trailing_punctuation).len();
    let trailing = text.len() - text.trim_end_matches(is_trailing_punctuation).len();
    if leading + trailing >= text.len() {
        return None;
    }
    let trimmed = text[leading..text.len() - trailing].to_owned();
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return None;
    }
    let leading_chars = text[..leading].chars().count();
    let trailing_chars = text[text.len() - trailing..].chars().count();
    let start = text_columns.get(leading_chars).copied().unwrap_or(start);
    let end = text_columns
        .get(text_columns.len().saturating_sub(trailing_chars + 1))
        .copied()
        .unwrap_or(last_content);
    Some((
        Point::new(line, Column(start)),
        Point::new(line, Column(end)),
        trimmed,
    ))
}

fn is_trailing_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | ',' | ';' | '.'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;

    use alacritty_terminal::index::{Column, Line, Side};

    use alacritty_terminal::selection::{Selection, SelectionType};
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::Processor;

    use crate::contrast::wcag_contrast_ratio;

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

    fn layout(term: &Term<VoidListener>, rows: usize) -> GridLayout {
        let snapshot = CellSnapshot::capture(term);
        layout_grid(
            &snapshot,
            &Palette::one_dark(),
            rows,
            term.columns(),
            None,
            None,
        )
    }

    fn line_text(line: &LineLayout) -> String {
        line.runs.iter().map(|run| run.text.as_str()).collect()
    }

    #[test]
    fn wide_chars_occupy_two_cells_and_share_a_run() {
        let mut term = term(20, 4);
        feed(&mut term, "你好");
        let layout = layout(&term, 4);
        let runs = &layout.lines[0].runs;
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].text, "你好");
        assert_eq!(runs[0].col, 0);
        assert_eq!(runs[0].cells, 4);
    }

    #[test]
    fn wide_char_at_viewport_edge_stays_inside_columns() {
        let mut term = term(4, 4);
        let cell = &mut term.grid_mut()[Line(0)][Column(3)];
        cell.c = '界';
        cell.flags.insert(Flags::WIDE_CHAR);
        let layout = layout(&term, 4);
        let run = &layout.lines[0].runs[0];
        assert_eq!((run.col, run.cells), (3, 1));
        assert!(run.col + run.cells <= term.columns());
    }

    #[test]
    fn grapheme_clusters_are_kept_for_shaping() {
        let mut term = term(20, 4);
        feed(&mut term, "e\u{301} ❤️ 👩‍💻 界");
        let layout = layout(&term, 4);
        let runs = &layout.lines[0].runs;
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].text, "e\u{301} ❤️ 👩‍💻 界");
        assert_eq!(text_cluster_display_cells(&runs[0].text), 9);
    }

    #[test]
    fn grapheme_clusters_stay_within_grid_columns() {
        let mut term = term(12, 4);
        feed(&mut term, "e\u{301} ❤️ 👩‍💻 界");
        let layout = layout(&term, 4);
        let mut previous_end = 0;
        for run in &layout.lines[0].runs {
            assert!(run.col >= previous_end);
            assert!(run.col < term.columns());
            assert!(run.col + run.cells <= term.columns());
            previous_end = run.col + run.cells;
        }
    }

    #[test]
    fn zero_width_chars_attach_to_base_cell() {
        let mut term = term(20, 4);
        feed(&mut term, "e\u{301}");
        let layout = layout(&term, 4);
        let runs = &layout.lines[0].runs;
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].text, "e\u{301}");
        assert_eq!(runs[0].cells, 1);
    }

    #[test]
    fn variation_selector_keeps_following_space() {
        let mut term = term(20, 4);
        feed(&mut term, "\u{2764}\u{fe0f} x");
        let layout = layout(&term, 4);
        let runs = &layout.lines[0].runs;
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].text, "❤️ x");
        assert_eq!(runs[0].col, 0);
        assert_eq!(runs[0].cells, 3);
    }

    #[test]
    fn contiguous_text_includes_spaces_in_one_run() {
        let mut term = term(20, 4);
        feed(&mut term, "a b");
        let layout = layout(&term, 4);
        let runs = &layout.lines[0].runs;
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(
            (runs[0].text.as_str(), runs[0].col, runs[0].cells),
            ("a b", 0, 3)
        );
    }

    #[test]
    fn text_runs_stop_at_style_boundaries() {
        let mut term = term(20, 4);
        feed(&mut term, "\x1b[31mAB\x1b[32mCD");
        let runs = &layout(&term, 4).lines[0].runs;
        assert_eq!(
            runs.iter()
                .map(|run| (run.text.as_str(), run.col, run.cells))
                .collect::<Vec<_>>(),
            vec![("AB", 0, 2), ("CD", 2, 2)]
        );
    }

    #[test]
    fn contiguous_same_style_cells_share_one_run() {
        let mut term = term(20, 4);
        feed(&mut term, "\x1b[31mAB\x1b[31mCD");
        let runs = &layout(&term, 4).lines[0].runs;
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(
            (runs[0].text.as_str(), runs[0].col, runs[0].cells),
            ("ABCD", 0, 4)
        );
    }

    #[test]
    fn empty_zerowidth_storage_is_treated_as_blank() {
        let mut cell = Cell::default();
        cell.push_zerowidth('x');
        cell.clear_wide();
        assert!(is_blank(&cell));
    }

    #[test]
    fn backgrounds_merge_horizontally_and_vertically() {
        let mut term = term(20, 4);
        feed(&mut term, "\x1b[41mAB\x1b[0m\r\n\x1b[41mCD\x1b[0m");
        let layout = layout(&term, 4);
        let red = Palette::one_dark().ansi[1];
        assert_eq!(
            layout.lines[0].backgrounds,
            vec![BackgroundSpan {
                start: 0,
                end: 1,
                color: red
            }]
        );
        assert_eq!(
            layout.lines[1].backgrounds,
            vec![BackgroundSpan {
                start: 0,
                end: 1,
                color: red
            }]
        );
        let merged = merge_regions([(0, 0, 0, 1, red), (1, 1, 0, 1, red), (3, 3, 0, 1, red)]);
        assert_eq!(merged, vec![(0, 1, 0, 1, red), (3, 3, 0, 1, red)]);
    }

    #[test]
    fn configured_contrast_reaches_ansi_cells() {
        let mut term = term(32, 4);
        feed(&mut term, "\x1b[31mR\x1b[32mG\x1b[34mB\x1b[35mM");
        let palette = Palette::one_dark().with_minimum_contrast(7.0);
        let snapshot = CellSnapshot::capture(&term);
        let layout = layout_grid(&snapshot, &palette, 4, term.columns(), None, None);
        assert!(!layout.lines[0].runs.is_empty());
        assert!(layout.lines[0].runs.iter().all(|run| {
            wcag_contrast_ratio(run.style.fg, palette.background) >= palette.minimum_contrast
        }));
    }

    #[test]
    fn inverse_swaps_foreground_and_background() {
        let mut term = term(20, 4);
        feed(&mut term, "\x1b[7mX");
        let layout = layout(&term, 4);
        let palette = Palette::one_dark();
        let style = layout.lines[0].runs[0].style;
        assert!(wcag_contrast_ratio(style.fg, palette.foreground) >= palette.minimum_contrast);
        assert_eq!(
            layout.lines[0].backgrounds,
            vec![BackgroundSpan {
                start: 0,
                end: 0,
                color: palette.foreground
            }]
        );
    }

    #[test]
    fn block_elements_become_rects_not_text() {
        let mut term = term(20, 4);
        feed(&mut term, "█▀░");
        let layout = layout(&term, 4);
        let blocks = &layout.lines[0].blocks;
        assert_eq!(blocks.len(), 3, "{blocks:?}");
        assert_eq!((blocks[0].col, blocks[0].line), (0, 0));
        assert_eq!((blocks[0].columns, blocks[0].lines), (8, 24));
        assert_eq!((blocks[1].col, blocks[1].line), (8, 0));
        assert_eq!((blocks[1].columns, blocks[1].lines), (8, 12));
        assert_eq!((blocks[2].col, blocks[2].line), (16, 0));
        assert_eq!((blocks[2].columns, blocks[2].lines), (8, 24));
        assert!(layout.lines[0].runs.is_empty());
    }

    #[test]
    fn block_quadrant_and_sextant_do_not_enter_text_runs() {
        let mut term = term(20, 4);
        feed(&mut term, "a█▘🬀░b");
        let line = &layout(&term, 4).lines[0];
        assert_eq!(
            line.runs
                .iter()
                .map(|run| run.text.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert!(!line.blocks.is_empty());
    }

    #[test]
    fn block_cursor_does_not_restore_the_glyph() {
        let mut term = term(20, 4);
        feed(&mut term, "█");
        term.grid_mut().cursor.point = Point::new(Line(0), Column(0));
        let layout = layout(&term, 4);
        assert_eq!(layout.cursor.expect("cursor missing").text, " ");
        assert!(layout.lines[0].runs.is_empty());
    }

    #[test]
    fn selection_spans_cover_selected_cells() {
        let mut term = term(20, 4);
        feed(&mut term, "hello");
        let mut selection = Selection::new(
            SelectionType::Simple,
            Point::new(Line(0), Column(0)),
            Side::Left,
        );
        selection.update(Point::new(Line(0), Column(2)), Side::Right);
        term.selection = Some(selection);
        let layout = layout(&term, 4);
        assert_eq!(
            layout.lines[0].selection,
            vec![SelectionSpan { start: 0, end: 2 }]
        );
    }

    #[test]
    fn wide_char_selection_includes_spacer() {
        let mut term = term(20, 4);
        feed(&mut term, "你好");
        let mut selection = Selection::new(
            SelectionType::Simple,
            Point::new(Line(0), Column(0)),
            Side::Left,
        );
        selection.update(Point::new(Line(0), Column(0)), Side::Right);
        term.selection = Some(selection);
        let layout = layout(&term, 4);
        assert_eq!(
            layout.lines[0].selection,
            vec![SelectionSpan { start: 0, end: 1 }]
        );
    }

    #[test]
    fn cursor_position_is_reported() {
        let mut term = term(20, 4);
        feed(&mut term, "abc");
        let layout = layout(&term, 4);
        let cursor = layout.cursor.expect("cursor missing");
        assert_eq!(cursor.col, 3);
        assert_eq!(cursor.line, 0);
    }

    #[test]
    fn hyperlink_span_expands_to_link_bounds() {
        let mut term = term(20, 4);
        feed(
            &mut term,
            "\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\",
        );
        let span = hyperlink_at(&term, Point::new(Line(0), Column(2))).expect("hyperlink missing");
        assert_eq!(span.start.column, 0);
        assert_eq!(span.end.column, 3);
        assert_eq!(span.uri, "https://example.com");
        assert!(hyperlink_at(&term, Point::new(Line(0), Column(10))).is_none());
    }

    #[test]
    fn hyperlink_on_wrapped_wide_char_uses_base_cell() {
        let mut term = term(4, 4);
        feed(
            &mut term,
            "\x1b]8;;https://example.com\x1b\\abc界\x1b]8;;\x1b\\",
        );
        let span = hyperlink_at(&term, Point::new(Line(0), Column(3))).expect("hyperlink missing");
        assert_eq!(span.start, Point::new(Line(1), Column(0)));
        assert_eq!(span.end, Point::new(Line(1), Column(1)));
    }

    #[test]
    fn url_span_at_detects_plain_url() {
        let mut term = term(40, 4);
        feed(&mut term, "see https://example.com/path, ok");
        let (_, _, url) = url_span_at(&term, Point::new(Line(0), Column(10))).expect("url missing");
        assert_eq!(url, "https://example.com/path");
        assert!(url_span_at(&term, Point::new(Line(0), Column(0))).is_none());
    }

    #[test]
    fn wrapped_wide_chars_start_next_line() {
        let mut term = term(4, 4);
        feed(&mut term, "你好啊");
        let layout = layout(&term, 4);
        assert_eq!(line_text(&layout.lines[0]), "你好");
        assert_eq!(
            layout.lines[0]
                .runs
                .iter()
                .map(|run| run.cells)
                .sum::<usize>(),
            4
        );
        assert_eq!(line_text(&layout.lines[1]), "啊");
        assert_eq!(layout.lines[1].runs[0].col, 0);
    }

    #[test]
    fn styled_spaces_are_not_blank() {
        let mut term = term(20, 4);
        feed(&mut term, "\x1b[4m \x1b[0m");
        let layout = layout(&term, 4);
        let runs = &layout.lines[0].runs;
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].text, " ");
        assert!(runs[0].style.underline.is_some());
    }

    #[test]
    fn zero_width_does_not_hide_following_styled_space() {
        let mut term = term(20, 4);
        feed(&mut term, "e\u{301}\x1b[4m \x1b[0m");
        let runs = &layout(&term, 4).lines[0].runs;
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert_eq!(runs[1].text, " ");
        assert!(runs[1].style.underline.is_some());
    }

    #[test]
    fn zero_width_state_does_not_hide_styled_space_on_next_line() {
        let mut term = term(20, 4);
        feed(&mut term, "e\u{301}\r\n\x1b[4m \x1b[0m");
        let runs = &layout(&term, 4).lines[1].runs;
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].text, " ");
        assert!(runs[0].style.underline.is_some());
    }

    #[test]
    fn background_merge_spans_multiple_lines() {
        let mut term = term(20, 4);
        feed(
            &mut term,
            "\x1b[44mA\x1b[0m\r\n\x1b[44mA\x1b[0m\r\n\x1b[44mA\x1b[0m",
        );
        let merged = merge_regions([
            (0, 0, 0, 0, Palette::one_dark().ansi[4]),
            (1, 1, 0, 0, Palette::one_dark().ansi[4]),
            (2, 2, 0, 0, Palette::one_dark().ansi[4]),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].0, 0);
        assert_eq!(merged[0].1, 2);
    }

    #[test]
    fn hover_underlines_link_cells() {
        let mut term = term(40, 4);
        feed(&mut term, "see https://example.com now");
        let snapshot = CellSnapshot::capture(&term);
        let palette = Palette::one_dark();
        let layout = layout_grid(
            &snapshot,
            &palette,
            4,
            term.columns(),
            Some((
                Point::new(Line(0), Column(4)),
                Point::new(Line(0), Column(22)),
                palette.cursor,
            )),
            None,
        );
        let runs = &layout.lines[0].runs;
        let url_run = runs
            .iter()
            .find(|run| run.col == 4)
            .expect("url run missing");
        assert!(url_run.text.starts_with("https"));
        assert!(url_run.style.underline.is_some());
        let plain_run = runs
            .iter()
            .find(|run| run.col == 0)
            .expect("plain run missing");
        assert_eq!(plain_run.text, "see");
        assert!(plain_run.style.underline.is_none());
    }

    #[test]
    fn search_highlights_map_to_viewport_spans() {
        let mut term = term(20, 4);
        feed(&mut term, "foo bar foo");
        let matches: Arc<[SearchMatch]> = vec![
            SearchMatch {
                start: Point::new(Line(0), Column(0)),
                end: Point::new(Line(0), Column(2)),
            },
            SearchMatch {
                start: Point::new(Line(0), Column(8)),
                end: Point::new(Line(0), Column(10)),
            },
        ]
        .into();
        let highlights = SearchHighlights {
            matches,
            current: Some(1),
        };
        let snapshot = CellSnapshot::capture(&term);
        let layout = layout_grid(
            &snapshot,
            &Palette::one_dark(),
            4,
            term.columns(),
            None,
            Some(&highlights),
        );
        assert_eq!(
            layout.lines[0].search,
            vec![
                SearchSpan {
                    start: 0,
                    end: 2,
                    current: false
                },
                SearchSpan {
                    start: 8,
                    end: 10,
                    current: true
                },
            ]
        );
        assert!(layout.lines[1].search.is_empty());
    }

    #[test]
    fn search_highlights_skip_offscreen_matches() {
        let mut term = term(20, 2);
        feed(&mut term, "needle\r\nline\r\nmore\r\n");
        let matches: Arc<[SearchMatch]> = vec![SearchMatch {
            start: Point::new(Line(-2), Column(0)),
            end: Point::new(Line(-2), Column(5)),
        }]
        .into();
        let highlights = SearchHighlights {
            matches,
            current: Some(0),
        };
        let snapshot = CellSnapshot::capture(&term);
        let layout = layout_grid(
            &snapshot,
            &Palette::one_dark(),
            2,
            term.columns(),
            None,
            Some(&highlights),
        );
        assert!(layout.lines.iter().all(|line| line.search.is_empty()));
    }

    #[test]
    fn text_hash_is_stable_and_content_sensitive() {
        assert_eq!(text_hash("你好🎉"), text_hash("你好🎉"));
        assert_ne!(text_hash("你好🎉"), text_hash("你好 🎉"));
    }

    #[test]
    fn resize_keeps_layout_consistent() {
        let mut term = term(20, 4);
        feed(&mut term, "hello");
        term.resize(TestSize {
            columns: 10,
            lines: 6,
        });
        let layout = layout(&term, 6);
        assert_eq!(layout.lines.len(), 6);
        assert_eq!(line_text(&layout.lines[0]), "hello");
    }
}
