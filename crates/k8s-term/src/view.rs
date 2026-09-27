//! Terminal input, search, selection, scrolling, and grid rendering.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Point;
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{ClipboardType, TermMode};
use gpui::{
    App, Bounds, ClipboardItem, Context, DispatchPhase, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, Font, FontFeatures, HitboxBehavior, Hsla,
    KeyDownEvent, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Render, Role, ScrollDelta, ScrollWheelEvent, ShapedLine, SharedString, Task,
    TextInputConfiguration, TextRun, UTF16Selection, Window, canvas, div, fill, point, prelude::*,
    px, rgba, size,
};
use k8s_actions::{Copy, Cut, Paste, Redo, SelectAll, Undo};
use tokio::sync::mpsc;
use unicode_segmentation::UnicodeSegmentation;

use crate::blink::{BLINK_INTERVAL, BlinkState};
use crate::element::TerminalElement;
use crate::ime::ImeState;
use crate::keys::{TerminalShortcut, encode_key, ime_owns_key, is_printed_text, terminal_shortcut};
use crate::layout::{PreeditLayout, SearchHighlights};
use crate::mouse::{
    accumulate_scroll_delta, alt_scroll, mouse_button_report, mouse_mode_active,
    mouse_moved_report, scroll_report,
};
use crate::palette::Palette;
use crate::scrollbar::{display_offset_for_thumb_top, scrollbar_geometry};
use crate::search::{
    SEARCH_REFRESH_DELAY, SearchIndexes, SearchRefresh, SearchRefreshOutcome, SearchState,
    find_matches_in_index,
};
use crate::selection::{drag_line_delta, grid_point_and_side};
use crate::session::{SessionEvent, SessionExit, TermSize, TerminalSession};

/// Routes mouse input to the terminal application or local selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseInputMode {
    ReportToTerminal,
    LocalSelection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WheelTarget {
    Mouse,
    Alternate,
    Viewport,
}

fn wheel_target(mouse_input_mode: MouseInputMode, mode: TermMode, shift: bool) -> WheelTarget {
    if mouse_input_mode == MouseInputMode::ReportToTerminal && mouse_mode_active(mode) && !shift {
        WheelTarget::Mouse
    } else if mouse_input_mode == MouseInputMode::ReportToTerminal
        && mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
        && !shift
    {
        WheelTarget::Alternate
    } else {
        WheelTarget::Viewport
    }
}

#[derive(Clone, Debug)]
struct SearchTextMetrics {
    points: Vec<(usize, f32)>,
}

impl SearchTextMetrics {
    fn from_line(line: &ShapedLine) -> Self {
        let mut points = vec![(0, 0.0)];
        let mut cursor = line.cursor();
        let mut x = 0.0;
        for (start, grapheme) in line.text.grapheme_indices(true) {
            let end = start + grapheme.len();
            let piece = cursor.take_until(end);
            x += f32::from(piece.width());
            points.push((end, x));
        }
        Self { points }
    }

    fn x_for_byte(&self, byte: usize) -> f32 {
        self.points
            .binary_search_by_key(&byte, |(boundary, _)| *boundary)
            .map(|index| self.points[index].1)
            .unwrap_or_else(|index| self.points[index.saturating_sub(1)].1)
    }

    fn hit_test(&self, x: f32) -> usize {
        self.points
            .iter()
            .min_by(|(_, left), (_, right)| (left - x).abs().total_cmp(&(right - x).abs()))
            .map(|(boundary, _)| *boundary)
            .unwrap_or(0)
    }
}

#[derive(Clone, Debug)]
struct SearchInputLayout {
    bounds: Bounds<Pixels>,
    metrics: SearchTextMetrics,
    content_width: f32,
    scroll_x: f32,
}

fn keep_search_scroll(current: f32, caret_x: f32, viewport_width: f32, content_width: f32) -> f32 {
    let max_scroll = (content_width + SEARCH_CARET_WIDTH - viewport_width).max(0.0);
    let next = if caret_x < current {
        caret_x
    } else if caret_x + SEARCH_CARET_WIDTH > current + viewport_width {
        caret_x + SEARCH_CARET_WIDTH - viewport_width
    } else {
        current
    };
    next.clamp(0.0, max_scroll)
}

fn search_caret_bounds(
    bounds: Bounds<Pixels>,
    metrics: &SearchTextMetrics,
    cursor: usize,
    scroll_x: f32,
) -> Bounds<Pixels> {
    Bounds::new(
        point(
            bounds.origin.x + px(metrics.x_for_byte(cursor) - scroll_x),
            bounds.origin.y,
        ),
        size(px(SEARCH_CARET_WIDTH), bounds.size.height),
    )
}

fn pointer_capture_needed(selecting: bool, scrollbar_dragging: bool) -> bool {
    selecting || scrollbar_dragging
}

/// Returns true when the grid no longer matches the applied terminal size.
///
/// The element owns the grid size because only it knows the painted bounds. The view
/// follows the applied size so a resize only reaches the PTY once, and the search index,
/// the accessible text, and the preedit all describe the same generation.
fn grid_size_changed(indexed: Option<(usize, usize)>, applied: TermSize) -> bool {
    indexed != Some((applied.columns, applied.screen_lines))
}

#[derive(Clone, Debug, PartialEq)]
pub struct HoveredLink {
    pub start: Point,
    pub end: Point,
    pub uri: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalSecurityPolicy {
    pub allow_osc52: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SelectionSummary {
    cells: usize,
    rows: usize,
}

#[derive(Clone, Debug)]
struct AccessibilitySnapshot {
    generation: u64,
    text: SharedString,
    cursor: Option<(usize, usize)>,
    selection: Option<SelectionSummary>,
}

fn selection_summary(range: SelectionRange, columns: usize) -> SelectionSummary {
    let columns = columns.max(1);
    let rows = range
        .end
        .line
        .0
        .saturating_sub(range.start.line.0)
        .saturating_add(1)
        .max(1) as usize;
    let cells = if range.is_block {
        let width = range.end.column.0.saturating_sub(range.start.column.0) + 1;
        rows.saturating_mul(width)
    } else if rows == 1 {
        range
            .end
            .column
            .0
            .saturating_sub(range.start.column.0)
            .saturating_add(1)
    } else {
        columns
            .saturating_sub(range.start.column.0)
            .saturating_add(columns.saturating_mul(rows.saturating_sub(2)))
            .saturating_add(range.end.column.0 + 1)
    };
    SelectionSummary { cells, rows }
}

fn accessibility_metadata<T: alacritty_terminal::event::EventListener>(
    term: &alacritty_terminal::term::Term<T>,
) -> (Option<(usize, usize)>, Option<SelectionSummary>) {
    let content = term.renderable_content();
    let cursor = crate::session::viewport_cursor_row(
        content.cursor.point.line.0,
        content.display_offset,
        term.screen_lines(),
    )
    .map(|row| (row, content.cursor.point.column.0));
    let selection = content
        .selection
        .map(|range| selection_summary(range, term.columns()));
    (cursor, selection)
}

fn accessibility_description(
    cursor: Option<(usize, usize)>,
    selection: Option<SelectionSummary>,
    resize_error: Option<&str>,
) -> String {
    let mut description =
        "Visible terminal output. Press Ctrl+Tab to move to the next control.".to_owned();
    if let Some((row, column)) = cursor {
        description.push_str(&format!(" Cursor at row {row}, column {column}."));
    }
    if let Some(selection) = selection {
        description.push_str(&format!(
            " Selection available: {} cells across {} rows.",
            selection.cells, selection.rows
        ));
    }
    if let Some(resize_error) = resize_error {
        description.push(' ');
        description.push_str(resize_error);
    }
    description
}

fn safe_link_uri(uri: &str) -> bool {
    if uri
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return false;
    }
    let Some((scheme, rest)) = uri.split_once(':') else {
        return false;
    };
    matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https")
        && rest.starts_with("//")
        && rest.len() > 2
}

pub(crate) fn link_open_allowed(uri: &str, modifiers: Modifiers) -> bool {
    (modifiers.control || modifiers.platform || modifiers.alt || modifiers.shift)
        && safe_link_uri(uri)
}

const CONTEXT_MENU_ITEM_COUNT: usize = 4;
const CONTEXT_MENU_WIDTH: f32 = 160.0;
const CONTEXT_MENU_ROW_HEIGHT: f32 = 28.0;
const CONTEXT_MENU_PADDING: f32 = 8.0;
const CONTEXT_MENU_GAP: f32 = 4.0;
const CONTEXT_MENU_MARGIN: f32 = 8.0;
const CONTEXT_MENU_ITEM_FIND: usize = 0;
const CONTEXT_MENU_ITEM_COPY: usize = 1;
const CONTEXT_MENU_ITEM_PASTE: usize = 2;
const CONTEXT_MENU_ITEM_SELECT_ALL: usize = 3;
const SEARCH_CARET_WIDTH: f32 = 1.0;
const SEARCH_BAR_MAX_WIDTH: f32 = 320.0;
const SEARCH_BAR_HORIZONTAL_INSET: f32 = 20.0;
/// Key context that owns terminal keystrokes. It is declared once, on the surface that
/// holds the terminal focus, so the search bar and the context menu keep their own keys.
const TERMINAL_KEY_CONTEXT: &str = "Terminal";
pub const TERMINAL_CELL_PADDING: Pixels = px(1.);

fn search_bar_width(viewport_width: f32) -> f32 {
    (viewport_width - SEARCH_BAR_HORIZONTAL_INSET).clamp(0.0, SEARCH_BAR_MAX_WIDTH)
}

fn move_context_menu_selection(current: usize, forward: bool) -> usize {
    if forward {
        (current + 1) % CONTEXT_MENU_ITEM_COUNT
    } else {
        (current + CONTEXT_MENU_ITEM_COUNT - 1) % CONTEXT_MENU_ITEM_COUNT
    }
}

/// Returns true when an open menu no longer owns the focus and has to be dismissed.
fn context_menu_needs_dismiss(open: bool, menu_focused: bool) -> bool {
    open && !menu_focused
}

/// What the menu can do right now. Copy and Paste are the documented exceptions that may
/// appear unavailable, so they are dimmed and explain why instead of disappearing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ContextMenuState {
    has_selection: bool,
    has_clipboard: bool,
}

impl ContextMenuState {
    fn enabled(&self, index: usize) -> bool {
        match index {
            CONTEXT_MENU_ITEM_COPY => self.has_selection,
            CONTEXT_MENU_ITEM_PASTE => self.has_clipboard,
            _ => true,
        }
    }

    fn unavailable_reason(&self, index: usize) -> Option<&'static str> {
        match index {
            CONTEXT_MENU_ITEM_COPY if !self.has_selection => {
                Some("Unavailable until text is selected.")
            }
            CONTEXT_MENU_ITEM_PASTE if !self.has_clipboard => {
                Some("Unavailable while the clipboard is empty.")
            }
            _ => None,
        }
    }
}

/// Moves the menu highlight to the next item the user can actually run.
fn next_enabled_menu_item(state: ContextMenuState, current: usize, forward: bool) -> usize {
    let mut index = current;
    for _ in 0..CONTEXT_MENU_ITEM_COUNT {
        index = move_context_menu_selection(index, forward);
        if state.enabled(index) {
            return index;
        }
    }
    current
}

fn first_enabled_context_menu_item(state: ContextMenuState) -> usize {
    (0..CONTEXT_MENU_ITEM_COUNT)
        .find(|index| state.enabled(*index))
        .unwrap_or(0)
}

fn last_enabled_context_menu_item(state: ContextMenuState) -> usize {
    (0..CONTEXT_MENU_ITEM_COUNT)
        .rev()
        .find(|index| state.enabled(*index))
        .unwrap_or(0)
}

/// Snaps a context menu request into the viewport. Both points are relative to the
/// viewport origin, which is also the origin the menu is positioned against.
fn context_menu_position(
    requested: gpui::Point<Pixels>,
    viewport: gpui::Size<Pixels>,
) -> gpui::Point<Pixels> {
    let height = CONTEXT_MENU_ROW_HEIGHT * CONTEXT_MENU_ITEM_COUNT as f32 + CONTEXT_MENU_PADDING;
    let left = CONTEXT_MENU_MARGIN;
    let top = CONTEXT_MENU_MARGIN;
    let right = f32::from(viewport.width) - CONTEXT_MENU_MARGIN;
    let bottom = f32::from(viewport.height) - CONTEXT_MENU_MARGIN;
    let requested_x = f32::from(requested.x);
    let requested_y = f32::from(requested.y);
    let mut x = requested_x;
    if requested_x + CONTEXT_MENU_WIDTH + CONTEXT_MENU_MARGIN > right {
        x = requested_x - CONTEXT_MENU_WIDTH - CONTEXT_MENU_GAP;
    }
    let mut y = requested_y;
    if requested_y + height + CONTEXT_MENU_MARGIN > bottom {
        y = requested_y - height - CONTEXT_MENU_GAP;
    }
    let max_x = (right - CONTEXT_MENU_WIDTH).max(left);
    let max_y = (bottom - height).max(top);
    point(px(x.clamp(left, max_x)), px(y.clamp(top, max_y)))
}

fn terminal_font(mut font: Font) -> Font {
    let mut features = font.features.0.as_ref().clone();
    features.retain(|(feature, _)| !matches!(feature.as_str(), "calt" | "liga" | "dlig"));
    features.extend([
        ("calt".to_owned(), 0),
        ("liga".to_owned(), 0),
        ("dlig".to_owned(), 0),
    ]);
    font.features = FontFeatures(Arc::new(features));
    font
}

pub struct TerminalView {
    session: Arc<TerminalSession>,
    _event_task: Task<()>,
    focus_handle: FocusHandle,
    font: Font,
    font_size: Pixels,
    palette: Palette,
    cell_width: Pixels,
    line_height: Pixels,
    scroll_accumulator: f32,
    title: Option<String>,
    exit: Option<SessionExit>,
    resize_error: Option<String>,
    selecting: bool,
    selection_dragged: bool,
    mouse_input_mode: MouseInputMode,
    hovered_link: Option<HoveredLink>,
    hovered_link_generation: u64,
    scrollbar_drag: Option<Pixels>,
    scrollbar_hovered: bool,
    was_focused: bool,
    search_was_focused: bool,
    blinking: bool,
    blink: BlinkState,
    blink_epoch: u64,
    blink_task: Option<Task<()>>,
    search: SearchState,
    search_indexes: SearchIndexes,
    accessibility: Option<AccessibilitySnapshot>,
    accessibility_selection_dirty: bool,
    term_generation: u64,
    security_policy: TerminalSecurityPolicy,
    search_open: bool,
    search_focus_handle: FocusHandle,
    search_clear_focus_handle: FocusHandle,
    search_input: Rc<RefCell<Option<SearchInputLayout>>>,
    ime: ImeState,
    ime_generation: u64,
    context_menu: Option<gpui::Point<Pixels>>,
    context_menu_focus: FocusHandle,
    context_menu_selected: usize,
    context_menu_state: ContextMenuState,
    context_menu_previous_focus: Option<FocusHandle>,
    reduce_motion: bool,
}

impl TerminalView {
    pub fn new(
        session: Arc<TerminalSession>,
        mut events: mpsc::UnboundedReceiver<SessionEvent>,
        font: Font,
        font_size: Pixels,
        line_height: Pixels,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle().tab_stop(true).tab_index(0);
        let search_focus_handle = cx.focus_handle().tab_stop(false).tab_index(0);
        let search_clear_focus_handle = cx.focus_handle().tab_stop(false).tab_index(1);
        let context_menu_focus = cx.focus_handle();
        let mut resize_failures = session.subscribe_resize_failures();
        let search_indexes = {
            let term = session.term().lock();
            SearchIndexes::capture(&term)
        };
        let _event_task = cx.spawn(async move |this, cx| {
            loop {
                let event = tokio::select! {
                    event = events.recv() => event,
                    changed = resize_failures.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        let resize_error = resize_failures.borrow_and_update().clone();
                        if this
                            .update(cx, |view, cx| {
                                view.resize_error = resize_error;
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                };
                let Some(event) = event else {
                    break;
                };
                match event {
                    SessionEvent::Wakeup => {
                        if this
                            .update(cx, |view, cx| {
                                view.session.ack_wakeup();
                                let search_index_update = {
                                    let term = view.session.term().lock();
                                    view.search_indexes.capture_update(&term)
                                };
                                view.search_indexes.apply_update(search_index_update);
                                view.advance_generation();
                                view.schedule_search_refresh(cx);
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    SessionEvent::Title(title) => {
                        if this
                            .update(cx, |view, cx| {
                                view.title = Some(title);
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    SessionEvent::Exited(status) => {
                        if this
                            .update(cx, |view, cx| {
                                view.exit = Some(status);
                                if view.search_was_focused {
                                    view.restart_blink(cx);
                                } else {
                                    view.stop_blink_timer();
                                }
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                        break;
                    }
                    SessionEvent::CursorBlinkingChanged => {
                        if this
                            .update(cx, |view, cx| {
                                let terminal = view.session.cursor_blinking();
                                view.blink.set_terminal(terminal);
                                view.restart_blink(cx);
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    SessionEvent::ClipboardStore(kind, text) => {
                        if this
                            .update(cx, |view, cx| {
                                if !view.security_policy.allow_osc52 {
                                    return;
                                }
                                match kind {
                                    ClipboardType::Clipboard => {
                                        cx.write_to_clipboard(ClipboardItem::new_string(text))
                                    }
                                    ClipboardType::Selection => Self::write_primary_text(cx, text),
                                }
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });

        Self {
            session,
            _event_task,
            focus_handle,
            font: terminal_font(font),
            font_size,
            palette,
            cell_width: px(8.0),
            line_height: line_height.max(px(1.0)),
            scroll_accumulator: 0.0,
            title: None,
            exit: None,
            resize_error: None,
            selecting: false,
            selection_dragged: false,
            mouse_input_mode: MouseInputMode::ReportToTerminal,
            hovered_link: None,
            hovered_link_generation: 0,
            scrollbar_drag: None,
            scrollbar_hovered: false,
            was_focused: false,
            search_was_focused: false,
            blinking: true,
            blink: BlinkState::new(),
            blink_epoch: 0,
            blink_task: None,
            search: SearchState::default(),
            search_indexes,
            accessibility: None,
            accessibility_selection_dirty: true,
            term_generation: 0,
            security_policy: TerminalSecurityPolicy::default(),
            search_open: false,
            search_focus_handle,
            search_clear_focus_handle,
            search_input: Rc::new(RefCell::new(None)),
            ime: ImeState::default(),
            ime_generation: 0,
            context_menu: None,
            context_menu_focus,
            context_menu_selected: 0,
            context_menu_state: ContextMenuState::default(),
            context_menu_previous_focus: None,
            reduce_motion: cx.reduce_motion(),
        }
    }

    pub fn session(&self) -> &Arc<TerminalSession> {
        &self.session
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn sync_focus_handles(&mut self) {
        self.focus_handle = self
            .focus_handle
            .clone()
            .tab_stop(!self.search_open)
            .tab_index(0);
        self.search_focus_handle = self
            .search_focus_handle
            .clone()
            .tab_stop(self.search_open)
            .tab_index(0);
        self.search_clear_focus_handle = self
            .search_clear_focus_handle
            .clone()
            .tab_stop(self.search_open)
            .tab_index(1);
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn exit_status(&self) -> Option<SessionExit> {
        self.exit
    }

    pub fn set_mouse_input_mode(&mut self, mode: MouseInputMode) {
        self.mouse_input_mode = mode;
    }

    pub fn security_policy(&self) -> TerminalSecurityPolicy {
        self.security_policy
    }

    pub fn set_security_policy(&mut self, policy: TerminalSecurityPolicy) {
        self.security_policy = policy;
    }

    fn advance_generation(&mut self) {
        self.term_generation = self.term_generation.wrapping_add(1);
        self.search.clear_results();
        self.search.invalidate_refresh();
        self.accessibility_selection_dirty = true;
        self.hovered_link = None;
        self.hovered_link_generation = self.term_generation;
        self.ime_generation = self.term_generation;
    }

    fn sync_grid_size(&mut self, cx: &mut Context<Self>) {
        if !grid_size_changed(self.search_indexes.screen_size(), self.session.size()) {
            return;
        }
        let search_index_update = {
            let term = self.session.term().lock();
            self.search_indexes.capture_update(&term)
        };
        self.search_indexes.apply_update(search_index_update);
        self.advance_generation();
        self.schedule_search_refresh(cx);
    }

    fn refresh_accessibility_metadata(&mut self) {
        let (cursor, selection) = {
            let term = self.session.term().lock();
            accessibility_metadata(&term)
        };
        if let Some(snapshot) = &mut self.accessibility {
            snapshot.cursor = cursor;
            snapshot.selection = selection;
        }
        self.accessibility_selection_dirty = false;
    }

    fn ensure_accessibility_cache(&mut self) {
        if self
            .accessibility
            .as_ref()
            .is_some_and(|snapshot| snapshot.generation == self.term_generation)
        {
            if self.accessibility_selection_dirty {
                self.refresh_accessibility_metadata();
            }
            return;
        }
        let snapshot = {
            let term = self.session.term().lock();
            let columns = term.columns();
            let content = term.renderable_content();
            let mut text = String::new();
            let mut line = String::new();
            let mut cell_text = String::new();
            let mut current_line = None;
            for indexed in content.display_iter {
                if current_line != Some(indexed.point.line.0) {
                    if current_line.is_some() {
                        text.push_str(&line);
                        text.push('\n');
                    }
                    current_line = Some(indexed.point.line.0);
                    line.clear();
                }
                let cell = &indexed.cell;
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                cell_text.clear();
                crate::layout::append_cell_text(
                    &mut cell_text,
                    cell,
                    crate::layout::cell_columns(cell, indexed.point.column.0, columns),
                );
                line.extend(
                    cell_text
                        .chars()
                        .filter(|character| !character.is_control()),
                );
            }
            if current_line.is_some() {
                text.push_str(&line);
            }
            let (cursor, selection) = accessibility_metadata(&term);
            AccessibilitySnapshot {
                generation: self.term_generation,
                text: SharedString::from(crate::session::accessible_text(&text)),
                cursor,
                selection,
            }
        };
        self.accessibility = Some(snapshot);
        self.accessibility_selection_dirty = false;
    }

    fn cached_cursor(&mut self) -> Option<(usize, usize)> {
        self.ensure_accessibility_cache();
        self.accessibility
            .as_ref()
            .and_then(|snapshot| snapshot.cursor)
    }

    /// Anchor for the IME candidate window. It follows the caret, and falls back to the last
    /// visible cell while the viewport is scrolled away from the cursor, so the candidate list
    /// stays attached to the terminal instead of falling back to a platform default position.
    fn candidate_window_origin(&mut self, element_bounds: Bounds<Pixels>) -> gpui::Point<Pixels> {
        let cell_width = f32::from(self.cell_width);
        let line_height = f32::from(self.line_height);
        let visible_rows = f32::from(element_bounds.size.height) / line_height;
        let visible_columns = f32::from(element_bounds.size.width) / cell_width;
        let rows = (visible_rows.floor().max(1.0) as usize).max(1);
        let columns = (visible_columns.floor().max(1.0) as usize).max(1);
        let (row, column) = match self.cached_cursor() {
            Some((row, column)) => (row.min(rows - 1), column.min(columns - 1)),
            None => (rows - 1, 0),
        };
        point(
            element_bounds.origin.x + px(column as f32 * cell_width),
            element_bounds.origin.y + px(row as f32 * line_height),
        )
    }

    pub fn mouse_input_mode(&self) -> MouseInputMode {
        self.mouse_input_mode
    }

    pub fn scrollbar_dragging(&self) -> bool {
        self.scrollbar_drag.is_some()
    }

    pub fn scrollbar_hovered(&self) -> bool {
        self.scrollbar_hovered
    }

    pub fn hovered_link(&self) -> Option<&HoveredLink> {
        (self.hovered_link_generation == self.term_generation)
            .then_some(self.hovered_link.as_ref())
            .flatten()
    }

    pub fn set_palette(&mut self, palette: Palette, cx: &mut Context<Self>) {
        if self.palette != palette {
            self.palette = palette;
            cx.notify();
        }
    }

    /// Applies a new terminal font and size to a session that is already open.
    ///
    /// The grid keeps its scrollback and selection: only the cell metrics change, and the
    /// next paint resizes the PTY to the new row and column count.
    pub fn set_font(
        &mut self,
        font: Font,
        font_size: Pixels,
        line_height: Pixels,
        cx: &mut Context<Self>,
    ) {
        let font = terminal_font(font);
        let line_height = line_height.max(px(1.0));
        if self.font == font && self.font_size == font_size && self.line_height == line_height {
            return;
        }
        self.font = font;
        self.font_size = font_size;
        self.line_height = line_height;
        // The cell metrics feed the search index, the accessible text, and the preedit, so the
        // generation moves with them and the next paint re-measures the grid.
        self.advance_generation();
        cx.notify();
    }

    /// Sets cursor blinking. Blinking is enabled by default.
    pub fn set_blinking(&mut self, blinking: bool, cx: &mut Context<Self>) {
        self.blinking = blinking;
        self.blink.set_setting(blinking);
        self.search.set_caret_blinking(blinking);
        self.restart_blink(cx);
    }

    pub fn blinking(&self) -> bool {
        self.blinking
    }

    /// Reports whether the focused cursor is solid.
    pub fn cursor_visible(&self) -> bool {
        self.reduce_motion || self.blink.visible()
    }

    pub fn search_open(&self) -> bool {
        self.search_open
    }

    pub fn search_query(&self) -> &str {
        self.search.query()
    }

    pub fn search_match_count(&self) -> usize {
        self.search.count()
    }

    /// Opens the context menu at a window position for acceptance tests.
    pub fn open_context_menu(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_context_menu_at(position, window, cx);
    }

    pub fn search_current_index(&self) -> Option<usize> {
        self.search.current_display_index()
    }

    /// Stops the blink timer and keeps the cursor visible.
    pub fn stop_blink_timer(&mut self) {
        self.blink_task.take();
        self.blink_epoch += 1;
        self.blink.show();
        self.search.reset_caret_blink();
    }

    fn restart_blink(&mut self, cx: &mut Context<Self>) {
        self.stop_blink_timer();
        if (!self.blink.active()
            && !(self.search_was_focused && self.search.caret_blinking_active()))
            || cx.reduce_motion()
        {
            cx.notify();
            return;
        }
        let epoch = self.blink_epoch;
        self.blink_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(BLINK_INTERVAL).await;
                let keep_going = this
                    .update(cx, |view, cx| {
                        let search_active =
                            view.search_was_focused && view.search.caret_blinking_active();
                        if view.blink_epoch != epoch
                            || view.reduce_motion
                            || (!view.blink.active() && !search_active)
                        {
                            return false;
                        }
                        let terminal_changed = view.blink.tick();
                        let search_changed = search_active && view.search.tick_caret_blink();
                        if terminal_changed || search_changed {
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// Opens the context menu. The position is a window position, and the menu is rendered
    /// in the terminal view, so the render pass snaps it into the visible viewport.
    fn open_context_menu_at(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.context_menu.is_none() {
            self.context_menu_previous_focus = window.focused(cx);
        }
        // The clipboard is read once, when the menu opens, so the item states do not depend
        // on the render pass.
        self.context_menu_state = ContextMenuState {
            has_selection: self.session.has_selection(),
            has_clipboard: cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .is_some_and(|text| !text.is_empty()),
        };
        self.context_menu = Some(position);
        self.context_menu_selected = first_enabled_context_menu_item(self.context_menu_state);
        window.focus(&self.context_menu_focus, cx);
        cx.notify();
    }

    fn close_context_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.context_menu.take().is_none() {
            return;
        }
        self.context_menu_selected = 0;
        self.context_menu_state = ContextMenuState::default();
        if let Some(previous) = self.context_menu_previous_focus.take() {
            window.focus(&previous, cx);
        } else {
            window.focus(&self.focus_handle, cx);
        }
        cx.notify();
    }

    /// Drops a menu that lost the focus, for example after a tab change or a click outside.
    /// The focus stays where the user moved it, so the menu is only cleared here.
    fn dismiss_unfocused_context_menu(&mut self, window: &Window) {
        if context_menu_needs_dismiss(
            self.context_menu.is_some(),
            self.context_menu_focus.is_focused(window),
        ) {
            self.context_menu = None;
            self.context_menu_selected = 0;
            self.context_menu_state = ContextMenuState::default();
            self.context_menu_previous_focus = None;
        }
    }

    fn activate_context_menu_item(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.context_menu_state.enabled(index) {
            return;
        }
        if index == CONTEXT_MENU_ITEM_FIND {
            self.close_context_menu(window, cx);
            self.toggle_search(window, cx);
            return;
        }
        match index {
            CONTEXT_MENU_ITEM_COPY => self.copy(cx),
            CONTEXT_MENU_ITEM_PASTE => self.paste(cx),
            CONTEXT_MENU_ITEM_SELECT_ALL => self.select_all(cx),
            _ => return,
        }
        self.close_context_menu(window, cx);
    }

    fn on_context_menu_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.context_menu_state;
        match event.keystroke.key.as_str() {
            "escape" => self.close_context_menu(window, cx),
            "down" | "arrowdown" => {
                self.context_menu_selected =
                    next_enabled_menu_item(state, self.context_menu_selected, true);
                cx.notify();
            }
            "up" | "arrowup" => {
                self.context_menu_selected =
                    next_enabled_menu_item(state, self.context_menu_selected, false);
                cx.notify();
            }
            "home" => {
                self.context_menu_selected = first_enabled_context_menu_item(state);
                cx.notify();
            }
            "end" => {
                self.context_menu_selected = last_enabled_context_menu_item(state);
                cx.notify();
            }
            "enter" | "return" | "space" => {
                self.activate_context_menu_item(self.context_menu_selected, window, cx);
            }
            "tab" => {
                self.close_context_menu(window, cx);
                if event.keystroke.modifiers.shift {
                    window.focus_prev(cx);
                } else {
                    window.focus_next(cx);
                }
            }
            _ => {}
        }
        cx.stop_propagation();
    }

    fn toggle_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_open {
            self.close_search(window, cx);
        } else {
            self.search_open = true;
            self.sync_focus_handles();
            self.search.reset_to_first();
            self.reveal_current_match();
            window.focus(&self.search_focus_handle, cx);
            cx.notify();
        }
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_open = false;
        self.sync_focus_handles();
        self.search.clear();
        self.search_input.borrow_mut().take();
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn schedule_search_refresh(&mut self, cx: &mut Context<Self>) {
        self.schedule_search_refresh_after(SEARCH_REFRESH_DELAY, cx);
    }

    fn schedule_search_refresh_after(&mut self, delay: Duration, cx: &mut Context<Self>) {
        if !self.search_open {
            return;
        }
        let Some(epoch) = self.search.request_refresh() else {
            return;
        };
        let Some(index) = self.search_indexes.active() else {
            self.search.invalidate_refresh();
            return;
        };
        let query = self.search.query().to_owned();
        let generation = self.term_generation;
        let background = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let scan_query = query.clone();
            let matches = background
                .spawn(async move { find_matches_in_index(&index, &scan_query) })
                .await;
            let _ = this.update(cx, |view, cx| {
                let reveal = view.search.count() == 0;
                let outcome = view.search.apply_refresh(
                    SearchRefresh {
                        epoch,
                        generation,
                        query,
                        matches,
                    },
                    view.term_generation,
                );
                match outcome {
                    SearchRefreshOutcome::Applied => {
                        if reveal {
                            view.search.reset_to_first();
                            view.reveal_current_match();
                        }
                        cx.notify();
                    }
                    SearchRefreshOutcome::Stale => view.schedule_search_refresh(cx),
                    SearchRefreshOutcome::Superseded => {}
                }
            });
        })
        .detach();
    }

    fn flush_search_refresh(&mut self, cx: &mut Context<Self>) {
        if self.search.take_refresh() {
            self.schedule_search_refresh_after(Duration::ZERO, cx);
        }
    }

    fn reveal_current_match(&mut self) {
        if let Some(search_match) = self.search.current_match() {
            self.session.scroll_to_point(search_match.start);
            self.accessibility_selection_dirty = true;
        }
    }

    fn set_search_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.search.set_query(query);
        self.restart_blink(cx);
        self.update_search_scroll();
        self.schedule_search_refresh(cx);
        cx.notify();
    }

    fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_search_query(String::new(), cx);
        window.focus(&self.search_focus_handle, cx);
    }

    fn edit_search(&mut self, edit: impl FnOnce(&mut SearchState), cx: &mut Context<Self>) {
        let previous_query = self.search.query().to_owned();
        edit(&mut self.search);
        self.restart_blink(cx);
        self.update_search_scroll();
        if previous_query != self.search.query() {
            self.schedule_search_refresh(cx);
        }
        cx.notify();
    }

    fn update_search_scroll(&mut self) {
        let Some(mut layout) = self.search_input.borrow().clone() else {
            return;
        };
        let caret_x = layout.metrics.x_for_byte(self.search.display_cursor());
        let next = keep_search_scroll(
            layout.scroll_x,
            caret_x,
            f32::from(layout.bounds.size.width),
            layout.content_width,
        );
        if next != layout.scroll_x {
            layout.scroll_x = next;
            *self.search_input.borrow_mut() = Some(layout);
        }
    }

    fn paste_search(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let text: String = text
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect();
        self.edit_search(|search| search.insert_text(&text), cx);
    }

    fn on_search_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if self.search_open && self.search_clear_focus_handle.is_focused(window) {
            match key {
                "enter" | "return" | "space" => self.clear_search(window, cx),
                "escape" | "tab" => window.focus(&self.search_focus_handle, cx),
                _ => {
                    // The Clear button only exists while the query is not empty, so the caret
                    // must never rest on a control that is gone. The key belongs to the
                    // search input.
                    window.focus(&self.search_focus_handle, cx);
                    self.insert_typed_search_text(event, cx);
                }
            }
            cx.stop_propagation();
            return;
        }
        if !self.search_focus_handle.is_focused(window) {
            return;
        }
        cx.stop_propagation();
        let modifiers = event.keystroke.modifiers;
        if ime_owns_key(self.search.is_composing(), &event.keystroke) {
            return;
        }
        if (modifiers.control || modifiers.platform)
            && !modifiers.alt
            && key.eq_ignore_ascii_case("v")
        {
            self.paste_search(cx);
            return;
        }
        if (modifiers.control || modifiers.platform)
            && !modifiers.alt
            && key.eq_ignore_ascii_case("a")
        {
            self.edit_search(SearchState::select_all, cx);
            return;
        }
        if (modifiers.control || modifiers.platform)
            && !modifiers.alt
            && key.eq_ignore_ascii_case("u")
        {
            self.set_search_query(String::new(), cx);
            return;
        }
        match key {
            "escape" => {
                self.close_search(window, cx);
                return;
            }
            "enter" => {
                self.flush_search_refresh(cx);
                if modifiers.shift {
                    self.search.previous_match();
                } else {
                    self.search.next_match();
                }
                self.reveal_current_match();
                cx.notify();
                return;
            }
            "home" => {
                self.edit_search(SearchState::home, cx);
                return;
            }
            "end" => {
                self.edit_search(SearchState::end, cx);
                return;
            }
            "backspace" => {
                self.edit_search(SearchState::backspace, cx);
                return;
            }
            "delete" => {
                self.edit_search(SearchState::delete, cx);
                return;
            }
            "left" => {
                self.edit_search(|search| search.move_left(modifiers.shift), cx);
                return;
            }
            "right" => {
                self.edit_search(|search| search.move_right(modifiers.shift), cx);
                return;
            }
            "up" => {
                self.flush_search_refresh(cx);
                self.search.previous_match();
                self.reveal_current_match();
                cx.notify();
                return;
            }
            "down" => {
                self.flush_search_refresh(cx);
                self.search.next_match();
                self.reveal_current_match();
                cx.notify();
                return;
            }
            "tab" => {
                self.close_search(window, cx);
                if modifiers.shift {
                    window.focus_prev(cx);
                } else {
                    window.focus_next(cx);
                }
                return;
            }
            _ => {}
        }
        self.insert_typed_search_text(event, cx);
    }

    /// Inserts the character a key press printed. AltGr is reported as Control+Alt and prints
    /// the third level of the keyboard layout, so its character is typed instead of being
    /// dropped as a shortcut chord.
    fn insert_typed_search_text(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let modifiers = event.keystroke.modifiers;
        if (modifiers.control || modifiers.alt || modifiers.platform)
            && !is_printed_text(&event.keystroke)
        {
            return;
        }
        if let Some(text) = event
            .keystroke
            .key_char
            .as_deref()
            .filter(|text| !text.is_empty())
        {
            self.edit_search(|search| search.insert_text(text), cx);
        }
    }

    fn search_highlights(&self, generation: u64) -> Option<SearchHighlights> {
        (self.search_open && self.search.matches_for_generation(generation).is_some())
            .then(|| self.search.highlights())
    }

    fn term_mode(&self) -> TermMode {
        self.session.mode()
    }

    /// Reports whether mouse input goes to the terminal. Shift selects locally.
    fn reports_mouse(&self, shift: bool) -> bool {
        self.mouse_input_mode == MouseInputMode::ReportToTerminal
            && mouse_mode_active(self.term_mode())
            && !shift
    }

    fn report_modifiers(modifiers: Modifiers) -> Modifiers {
        Modifiers {
            alt: modifiers.alt || modifiers.platform,
            platform: false,
            ..modifiers
        }
    }

    fn grid_point(
        &self,
        position: gpui::Point<Pixels>,
        bounds: Bounds<Pixels>,
    ) -> (Point, alacritty_terminal::index::Side) {
        let size = self.session.size();
        grid_point_and_side(
            position,
            bounds,
            self.cell_width,
            self.line_height,
            size.columns,
            size.screen_lines,
            self.session.display_offset(),
        )
    }

    fn scroll_rows(&self) -> usize {
        self.session.size().screen_lines.max(1)
    }

    fn scrollbar(&self, bounds: Bounds<Pixels>) -> Option<crate::scrollbar::ScrollbarGeometry> {
        let rows = self.scroll_rows();
        scrollbar_geometry(
            bounds,
            rows,
            rows + self.session.history_size(),
            self.session.display_offset(),
        )
    }

    fn pointer_routes(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        div()
            .id("terminal-pointer-routes")
            .absolute()
            .inset_0()
            .child(canvas(
                |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                move |_, hitbox, window, _cx| {
                    let bounds = hitbox.bounds;
                    let down_view = view.clone();
                    let down_hitbox = hitbox.clone();
                    window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                        if phase != DispatchPhase::Bubble
                            || event.button != MouseButton::Left
                            || !down_hitbox.is_hovered(window)
                        {
                            return;
                        }
                        if let Some(view) = down_view.upgrade() {
                            view.update(cx, |view, _| {
                                if pointer_capture_needed(
                                    view.selecting,
                                    view.scrollbar_drag.is_some(),
                                ) {
                                    window.capture_pointer(down_hitbox.id);
                                }
                            });
                        }
                    });
                    let move_view = view.clone();
                    let move_hitbox = hitbox.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                        if phase != DispatchPhase::Capture
                            || window.captured_hitbox() != Some(move_hitbox.id)
                        {
                            return;
                        }
                        if let Some(view) = move_view.upgrade() {
                            view.update(cx, |view, cx| {
                                if pointer_capture_needed(
                                    view.selecting,
                                    view.scrollbar_drag.is_some(),
                                ) {
                                    view.mouse_move(event, bounds, false, cx);
                                }
                            });
                            cx.stop_propagation();
                        }
                    });
                    let up_view = view.clone();
                    let up_hitbox = hitbox;
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                        if phase != DispatchPhase::Capture
                            || event.button != MouseButton::Left
                            || window.captured_hitbox() != Some(up_hitbox.id)
                        {
                            return;
                        }
                        if let Some(view) = up_view.upgrade() {
                            view.update(cx, |view, cx| {
                                if pointer_capture_needed(
                                    view.selecting,
                                    view.scrollbar_drag.is_some(),
                                ) {
                                    view.mouse_up(event, bounds, cx);
                                }
                            });
                        }
                        window.release_pointer();
                        cx.stop_propagation();
                    });
                },
            ))
    }

    fn write_primary_text(cx: &mut Context<Self>, text: String) {
        #[cfg(target_os = "linux")]
        cx.write_to_primary(ClipboardItem::new_string(text));
        #[cfg(not(target_os = "linux"))]
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn read_primary_text(cx: &mut Context<Self>) -> Option<String> {
        #[cfg(target_os = "linux")]
        {
            cx.read_from_primary().and_then(|item| item.text())
        }
        #[cfg(not(target_os = "linux"))]
        {
            cx.read_from_clipboard().and_then(|item| item.text())
        }
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        self.copy_in_background(cx, false);
    }

    fn cut(&mut self, cx: &mut Context<Self>) {
        self.copy_in_background(cx, true);
    }

    fn copy_in_background(&mut self, cx: &mut Context<Self>, cut: bool) {
        let session = self.session.clone();
        let background = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let (selection_revision, text) = background
                .spawn(async move { session.selection_text_in_chunks().await })
                .await;
            let _ = this.update(cx, |view, cx| {
                if let Some(text) = text
                    && !text.is_empty()
                {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
                if cut && view.session.selection_is_current(selection_revision) {
                    view.session.clear_selection();
                    view.accessibility_selection_dirty = true;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.session.paste(&text);
            self.session.scroll_to_bottom();
            self.accessibility_selection_dirty = true;
            cx.notify();
        }
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.session.select_all();
        self.accessibility_selection_dirty = true;
        cx.notify();
    }

    fn copy_search(&self, cx: &mut Context<Self>) {
        if let Some(text) = self.search.selected_display_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn cut_search(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self.search.selected_display_text() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.edit_search(
            |search| {
                search.cut();
            },
            cx,
        );
    }

    fn search_undo_action(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search_focus_handle.is_focused(window) {
            return;
        }
        self.edit_search(
            |search| {
                search.undo();
            },
            cx,
        );
        cx.stop_propagation();
    }

    fn search_redo_action(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search_focus_handle.is_focused(window) {
            return;
        }
        self.edit_search(
            |search| {
                search.redo();
            },
            cx,
        );
        cx.stop_propagation();
    }

    fn search_cut_action(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search_focus_handle.is_focused(window) {
            return;
        }
        self.cut_search(cx);
        cx.stop_propagation();
    }

    fn search_copy_action(&mut self, _: &Copy, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search_focus_handle.is_focused(window) {
            return;
        }
        self.copy_search(cx);
        cx.stop_propagation();
    }

    fn search_paste_action(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search_focus_handle.is_focused(window) {
            return;
        }
        self.paste_search(cx);
        cx.stop_propagation();
    }

    fn search_select_all_action(
        &mut self,
        _: &SelectAll,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.search_focus_handle.is_focused(window) {
            return;
        }
        self.edit_search(SearchState::select_all, cx);
        cx.stop_propagation();
    }

    fn cut_action(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus_handle.is_focused(window) {
            self.cut(cx);
            cx.stop_propagation();
        }
    }

    fn copy_action(&mut self, _: &Copy, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus_handle.is_focused(window) {
            self.copy(cx);
            cx.stop_propagation();
        }
    }

    fn paste_action(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus_handle.is_focused(window) {
            self.paste(cx);
            cx.stop_propagation();
        }
    }

    fn select_all_action(&mut self, _: &SelectAll, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus_handle.is_focused(window) {
            self.select_all(cx);
            cx.stop_propagation();
        }
    }

    /// Encodes a keystroke for the child process. Only the terminal surface calls this, so
    /// keys that belong to the search bar or the context menu never reach the PTY.
    fn on_terminal_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mods = event.keystroke.modifiers;
        if ime_owns_key(self.ime.preedit().is_some(), &event.keystroke) {
            if event.keystroke.key.eq_ignore_ascii_case("escape") {
                self.ime.unmark();
                self.ime_generation = self.term_generation;
                cx.notify();
            }
            cx.stop_propagation();
            return;
        }
        if let Some(shortcut) = terminal_shortcut(&event.keystroke) {
            match shortcut {
                TerminalShortcut::Copy => self.copy(cx),
                TerminalShortcut::Paste => self.paste(cx),
                TerminalShortcut::SelectAll => self.select_all(cx),
                TerminalShortcut::Cut => self.cut(cx),
                TerminalShortcut::Find => self.toggle_search(window, cx),
                // Find next only has a meaning once the scrollback is searched, so a closed
                // search bar opens instead of doing nothing.
                TerminalShortcut::FindNext => {
                    if self.search_open {
                        self.flush_search_refresh(cx);
                        self.search.next_match();
                        self.reveal_current_match();
                        cx.notify();
                    } else {
                        self.toggle_search(window, cx);
                    }
                }
            }
            cx.stop_propagation();
            return;
        }
        if self.blink.active() {
            self.restart_blink(cx);
        }

        if mods.shift && !mods.control && !mods.alt {
            let page = self.session.size().screen_lines as i32;
            match event.keystroke.key.as_str() {
                "pageup" => {
                    self.session.scroll(page);
                    self.accessibility_selection_dirty = true;
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                "pagedown" => {
                    self.session.scroll(-page);
                    self.accessibility_selection_dirty = true;
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                "home" => {
                    self.session.scroll_to_top();
                    self.accessibility_selection_dirty = true;
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                "end" => {
                    self.session.scroll_to_bottom();
                    self.accessibility_selection_dirty = true;
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }

        let mode = self.term_mode();
        if let Some(bytes) = encode_key(&event.keystroke, mode, false) {
            self.session.clear_selection();
            self.accessibility_selection_dirty = true;
            self.session.write(bytes);
            self.session.scroll_to_bottom();
            cx.notify();
            cx.stop_propagation();
        }
    }

    /// Handles a left mouse press. Returns a link URI for the element to open.
    pub fn mouse_down(
        &mut self,
        event: &MouseDownEvent,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if event.button == MouseButton::Right {
            if !self.reports_mouse(event.modifiers.shift) {
                self.open_context_menu_at(event.position, window, cx);
            }
            return None;
        }
        if event.button == MouseButton::Middle {
            if self.mouse_input_mode == MouseInputMode::ReportToTerminal
                && let Some(text) = Self::read_primary_text(cx)
            {
                self.session.paste(&text);
                self.session.scroll_to_bottom();
                self.accessibility_selection_dirty = true;
                cx.notify();
            }
            return None;
        }
        if event.button != MouseButton::Left {
            return None;
        }
        let (point, side) = self.grid_point(event.position, bounds);
        let secondary = event.modifiers.secondary();
        let mode = self.term_mode();

        if event.modifiers.alt && !event.modifiers.control && !secondary && event.click_count == 1 {
            if event.modifiers.shift && self.session.has_selection() {
                self.session.update_selection(point, side);
            } else {
                self.session.start_block_selection(point, side);
            }
            self.accessibility_selection_dirty = true;
            self.selecting = true;
            self.selection_dragged = false;
            cx.notify();
            return None;
        }

        if self.reports_mouse(event.modifiers.shift) {
            if secondary
                && let Some(link) = self.session.link_at(point)
                && link_open_allowed(&link, event.modifiers)
            {
                return Some(link);
            }
            if let Some(bytes) = mouse_button_report(
                point,
                event.button,
                Self::report_modifiers(event.modifiers),
                true,
                mode,
            ) {
                self.session.write(bytes);
            }
            self.selecting = false;
            return None;
        }

        if let Some(geometry) = self.scrollbar(bounds)
            && geometry.contains_track_at(event.position.x, event.position.y)
        {
            if geometry.contains_thumb(event.position.x, event.position.y) {
                self.scrollbar_drag = Some(event.position.y - geometry.thumb.top());
            } else {
                let rows = self.scroll_rows();
                let target = display_offset_for_thumb_top(
                    &geometry,
                    event.position.y - geometry.thumb.size.height / 2.0,
                    rows,
                    rows + self.session.history_size(),
                );
                let current = self.session.display_offset();
                self.session.scroll(target as i32 - current as i32);
                self.accessibility_selection_dirty = true;
            }
            self.selecting = false;
            cx.notify();
            return None;
        }

        match event.click_count {
            0 => return None,
            1 if event.modifiers.shift && self.session.has_selection() => {
                self.session.update_selection(point, side);
            }
            1 => self.session.start_selection(point, side),
            2 => self.session.select_word(point, side),
            3 => self.session.select_line(point, side),
            _ => return None,
        }
        self.accessibility_selection_dirty = true;
        self.selecting = true;
        self.selection_dragged = false;
        cx.notify();
        None
    }

    pub fn mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        bounds: Bounds<Pixels>,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(grab) = self.scrollbar_drag {
            if let Some(geometry) = self.scrollbar(bounds) {
                let rows = self.scroll_rows();
                let target = display_offset_for_thumb_top(
                    &geometry,
                    event.position.y - grab,
                    rows,
                    rows + self.session.history_size(),
                );
                let current = self.session.display_offset();
                if target != current {
                    self.session.scroll(target as i32 - current as i32);
                    self.accessibility_selection_dirty = true;
                    cx.notify();
                }
            }
            return;
        }

        let (point, side) = self.grid_point(event.position, bounds);

        if !self.selecting && self.reports_mouse(event.modifiers.shift) {
            if let Some(bytes) = mouse_moved_report(
                point,
                event.pressed_button,
                Self::report_modifiers(event.modifiers),
                self.term_mode(),
            ) {
                self.session.write(bytes);
            }
            return;
        }

        if self.selecting {
            self.selection_dragged = true;
            if let Some(lines) = drag_line_delta(event.position, bounds, self.line_height) {
                self.session.scroll_preserving_selection(lines);
                self.accessibility_selection_dirty = true;
                let (point, side) = self.grid_point(event.position, bounds);
                self.session.update_selection(point, side);
            } else {
                self.session.update_selection(point, side);
            }
            self.accessibility_selection_dirty = true;
            cx.notify();
            return;
        }

        let hovered_link = if hovered {
            self.session
                .hyperlink_range(point)
                .filter(|(_, _, uri)| safe_link_uri(uri))
                .map(|(start, end, uri)| HoveredLink { start, end, uri })
                .or_else(|| {
                    self.session
                        .url_span_at(point)
                        .filter(|(_, _, uri)| safe_link_uri(uri))
                        .map(|(start, end, uri)| HoveredLink { start, end, uri })
                })
        } else {
            None
        };
        let scrollbar_hovered = hovered
            && self.scrollbar(bounds).is_some_and(|geometry| {
                geometry.contains_track_at(event.position.x, event.position.y)
            });
        if hovered_link != self.hovered_link || scrollbar_hovered != self.scrollbar_hovered {
            self.hovered_link = hovered_link;
            self.hovered_link_generation = self.term_generation;
            self.scrollbar_hovered = scrollbar_hovered;
            cx.notify();
        }
    }

    /// Handles a left mouse release. Returns a link URI when the mouse clicked a link.
    pub fn mouse_up(
        &mut self,
        event: &MouseUpEvent,
        bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if self.scrollbar_drag.take().is_some() {
            cx.notify();
            return None;
        }
        if event.button != MouseButton::Left {
            return None;
        }

        let (point, _) = self.grid_point(event.position, bounds);
        if !self.selecting && self.reports_mouse(event.modifiers.shift) {
            if let Some(bytes) = mouse_button_report(
                point,
                event.button,
                Self::report_modifiers(event.modifiers),
                false,
                self.term_mode(),
            ) {
                self.session.write(bytes);
            }
            self.selecting = false;
            return None;
        }

        let was_click = self.selecting && !self.selection_dragged;
        self.selecting = false;
        cx.notify();

        if was_click {
            return self
                .session
                .link_at(point)
                .filter(|link| link_open_allowed(link, event.modifiers));
        }
        None
    }

    pub fn scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        let mode = self.term_mode();
        let target = wheel_target(self.mouse_input_mode, mode, event.modifiers.shift);
        let (point, _) = self.grid_point(event.position, bounds);
        let delta_y = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => f32::from(delta.y),
        };
        let pixels_per_line = match event.delta {
            ScrollDelta::Lines(_) => 1.0,
            ScrollDelta::Pixels(_) => f32::from(self.line_height),
        };
        let lines = accumulate_scroll_delta(&mut self.scroll_accumulator, delta_y, pixels_per_line);
        if lines == 0 {
            return;
        }

        match target {
            WheelTarget::Mouse => {
                for report in
                    scroll_report(point, lines, Self::report_modifiers(event.modifiers), mode)
                {
                    self.session.write(report);
                }
            }
            WheelTarget::Alternate => {
                self.session.write(alt_scroll(lines));
            }
            WheelTarget::Viewport => {
                self.session.scroll(lines);
                self.accessibility_selection_dirty = true;
                cx.notify();
            }
        }
    }

    fn search_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.search_focus_handle.is_focused(window);
        let caret_visible = self.search.caret_visible(focused, self.reduce_motion);
        let caret_color = self.palette.cursor;
        let query = self.search.display_text().into_owned();
        let selection = self.search.display_selection();
        let cursor = self.search.display_cursor();
        let count = self.search.count();
        let viewport_width = f32::from(window.content_mask().bounds.size.width);
        let bar_width = search_bar_width(viewport_width);
        let has_query = !self.search.query().is_empty() || !query.is_empty();
        let show_counter = bar_width >= 180.0 && has_query;
        let search_refreshing = self.search.refreshing();
        let current_match = self.search.current_display_index().unwrap_or(0);
        let (counter, match_status) = if !has_query {
            (String::new(), String::new())
        } else if search_refreshing {
            (
                "Searching…".to_owned(),
                "Searching terminal output.".to_owned(),
            )
        } else if count == 0 {
            ("No Matches".to_owned(), "No matches found.".to_owned())
        } else {
            (
                format!("{current_match} of {count}"),
                format!("Match {current_match} of {count}."),
            )
        };
        let search_actions = "Press Enter for the next match, Shift Enter for the previous match, or Escape to close search.";
        let search_description = if !has_query {
            format!("Type to search terminal output. {search_actions}")
        } else if search_refreshing {
            "Searching terminal output. Results will update when the search finishes.".to_owned()
        } else if count == 0 {
            "No matches found. Enter different text or clear the search.".to_owned()
        } else {
            format!("{match_status} {search_actions}")
        };
        let run = TextRun {
            len: query.len(),
            font: self.font.clone(),
            color: self.palette.foreground,
            ..Default::default()
        };
        let shaped = window.text_system().shape_line(
            SharedString::from(query.clone()),
            px(12.0),
            &[run],
            None,
        );
        let metrics = SearchTextMetrics::from_line(&shaped);
        let content_width = f32::from(shaped.width());
        let previous_layout = self.search_input.borrow().clone();
        let previous_scroll = previous_layout
            .as_ref()
            .map(|layout| layout.scroll_x)
            .unwrap_or(0.0);
        let input_width = previous_layout
            .as_ref()
            .map(|layout| f32::from(layout.bounds.size.width))
            .unwrap_or(bar_width);
        let effective_scroll = keep_search_scroll(
            previous_scroll,
            metrics.x_for_byte(cursor),
            input_width,
            content_width,
        );
        let search_input = self.search_input.clone();
        let search_input_paint = search_input.clone();
        let input_entity = cx.entity();
        let input_focus = self.search_focus_handle.clone();
        let input_view = cx.entity().downgrade();
        let input_metrics_prepaint = metrics.clone();
        let input_metrics_paint = metrics.clone();
        let input_scroll = effective_scroll;
        let input_content_width = content_width;
        let input_cursor = cursor;
        let input_overlay = canvas(
            move |bounds, window, _| {
                let scroll_x = keep_search_scroll(
                    input_scroll,
                    input_metrics_prepaint.x_for_byte(input_cursor),
                    f32::from(bounds.size.width),
                    input_content_width,
                );
                *search_input_paint.borrow_mut() = Some(SearchInputLayout {
                    bounds,
                    metrics: input_metrics_prepaint.clone(),
                    content_width: input_content_width,
                    scroll_x,
                });
                if scroll_x != input_scroll {
                    window.refresh();
                }
                (
                    window.insert_hitbox(bounds, HitboxBehavior::Normal),
                    scroll_x,
                )
            },
            move |bounds, (hitbox, _actual_scroll_x), window, cx| {
                if caret_visible {
                    window.paint_quad(fill(
                        search_caret_bounds(
                            bounds,
                            &input_metrics_paint,
                            input_cursor,
                            input_scroll,
                        ),
                        caret_color,
                    ));
                }
                window.handle_input(
                    &input_focus,
                    ElementInputHandler::new(hitbox.bounds, input_entity.clone()),
                    cx,
                );
                let input_view = input_view.clone();
                let hitbox = hitbox.clone();
                let search_input = search_input.clone();
                window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble
                        || event.button != MouseButton::Left
                        || !hitbox.is_hovered(window)
                    {
                        return;
                    }
                    let Some(layout) = search_input.borrow().clone() else {
                        return;
                    };
                    let x = f32::from(event.position.x - layout.bounds.origin.x) + layout.scroll_x;
                    let byte = layout.metrics.hit_test(x);
                    if let Some(view) = input_view.upgrade() {
                        view.update(cx, |view, cx| {
                            let byte = view.search.display_byte_to_query_byte(byte);
                            view.search.select_caret(byte, event.modifiers.shift);
                            window.focus(&view.search_focus_handle, cx);
                            view.restart_blink(cx);
                            view.update_search_scroll();
                            cx.notify();
                        });
                        cx.stop_propagation();
                    }
                });
            },
        )
        .absolute()
        .inset_0();
        let query_view = if query.is_empty() {
            div()
                .text_color(self.palette.dim_foreground)
                .child(SharedString::from("Find in Terminal…"))
        } else if selection.start < selection.end {
            div()
                .flex()
                .flex_none()
                .child(SharedString::from(&query[..selection.start]))
                .child(
                    div()
                        .bg(self.palette.selection)
                        .text_color(self.palette.selection_foreground())
                        .child(SharedString::from(&query[selection.start..selection.end])),
                )
                .child(SharedString::from(&query[selection.end..]))
        } else {
            div()
                .flex()
                .flex_none()
                .child(SharedString::from(query.clone()))
        };
        let search_entity = cx.entity().downgrade();
        let clear = has_query.then(|| {
            let search_entity = search_entity.clone();
            div()
                .id("terminal-search-clear")
                .role(Role::Button)
                .aria_label("Clear Terminal Search")
                .track_focus(&self.search_clear_focus_handle)
                .tab_stop(has_query)
                .tab_index(1)
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(px(20.0))
                .h(px(20.0))
                .rounded(px(4.0))
                .text_size(px(14.0))
                .text_color(self.palette.foreground)
                .cursor_pointer()
                .hover(|this| this.bg(self.palette.foreground.alpha(0.14)))
                .on_click(cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.set_search_query(String::new(), cx);
                    window.focus(&this.search_focus_handle, cx);
                }))
                .on_a11y_action(gpui::AccessibleAction::Click, move |_, window, cx| {
                    let _ = search_entity.update(cx, |this, cx| {
                        this.set_search_query(String::new(), cx);
                        window.focus(&this.search_focus_handle, cx);
                    });
                })
                .child("×")
        });
        div()
            .flex()
            .id("terminal-search")
            .absolute()
            .top(px(6.0))
            .right(px(10.0))
            .w(px(bar_width))
            .max_w(px(bar_width))
            .min_w(px(0.0))
            .h(px(28.0))
            .items_center()
            .gap(px(4.0))
            .overflow_hidden()
            .px_1()
            .rounded(px(4.0))
            .bg(self.palette.background.alpha(0.95))
            .border_1()
            .border_color(if focused {
                self.palette.cursor
            } else {
                self.palette.foreground.alpha(0.35)
            })
            .text_size(px(12.0))
            .font(self.font.clone())
            .track_focus(&self.search_focus_handle)
            .key_context(TERMINAL_KEY_CONTEXT)
            .on_action(cx.listener(Self::search_undo_action))
            .on_action(cx.listener(Self::search_redo_action))
            .on_action(cx.listener(Self::search_cut_action))
            .on_action(cx.listener(Self::search_copy_action))
            .on_action(cx.listener(Self::search_paste_action))
            .on_action(cx.listener(Self::search_select_all_action))
            .on_key_down(cx.listener(Self::on_search_key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    window.focus(&this.search_focus_handle, cx);
                }),
            )
            .role(Role::SearchInput)
            .aria_label("Search Terminal")
            .aria_description(search_description)
            .aria_placeholder("Find in Terminal…")
            .aria_value(query.clone())
            .cursor_text()
            .child(
                div()
                    .id("terminal-search-input")
                    .flex_1()
                    .min_w(px(0.0))
                    .relative()
                    .overflow_hidden()
                    .child(query_view.left(-px(effective_scroll)))
                    .child(input_overlay),
            )
            .when(show_counter, |this| {
                this.child(
                    div()
                        .id("terminal-search-status")
                        .role(Role::Status)
                        .aria_label(match_status)
                        .flex_none()
                        .text_color(self.palette.dim_foreground)
                        .child(SharedString::from(counter)),
                )
            })
            .children(clear)
    }

    /// Builds the terminal context menu and blocks input to the terminal below.
    fn context_menu_element(
        &self,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity().downgrade();
        let state = self.context_menu_state;
        let dim = self.palette.dim_foreground;
        let item = |index: usize, label: &'static str| {
            let entity = entity.clone();
            let selected = index == self.context_menu_selected;
            let enabled = state.enabled(index);
            let reason = state.unavailable_reason(index);
            div()
                .id(("terminal-menu-item", index))
                .role(Role::MenuItem)
                .aria_label(label)
                .aria_selected(selected)
                .when_some(reason, |this, reason| this.aria_description(reason))
                .when(selected && enabled, |this| {
                    this.aria_active_descendant()
                        .bg(self.palette.foreground.alpha(0.14))
                })
                .flex()
                .items_center()
                .h(px(CONTEXT_MENU_ROW_HEIGHT))
                .px_3()
                .text_size(px(12.0))
                .text_color(if enabled {
                    self.palette.foreground
                } else {
                    dim
                })
                .when(enabled, |this| {
                    this.cursor_pointer()
                        .hover(|style| style.bg(self.palette.foreground.alpha(0.12)))
                })
                .when(enabled, |this| {
                    this.on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, _event, window, cx| {
                            view.activate_context_menu_item(index, window, cx);
                            cx.stop_propagation();
                        }),
                    )
                })
                .when(enabled, |this| {
                    this.on_a11y_action(gpui::AccessibleAction::Click, move |_data, window, cx| {
                        let _ = entity.update(cx, |view, cx| {
                            view.activate_context_menu_item(index, window, cx);
                        });
                    })
                })
                .child(SharedString::from(label))
        };
        div()
            .id("terminal-context-menu")
            .role(Role::Menu)
            .aria_label("Terminal Actions")
            .aria_description(
                "Press Up or Down to choose an action. Actions that are unavailable are skipped. Press Enter to run it or Escape to close the menu.",
            )
            .track_focus(&self.context_menu_focus)
            .tab_stop(false)
            .key_context(TERMINAL_KEY_CONTEXT)
            .capture_key_down(cx.listener(Self::on_context_menu_key_down))
            .absolute()
            .w(px(CONTEXT_MENU_WIDTH))
            .left(position.x)
            .top(position.y)
            .occlude()
            .flex()
            .flex_col()
            .py_1()
            .rounded(px(4.0))
            .bg(self.palette.background.alpha(0.98))
            .border_1()
            .border_color(self.palette.foreground.alpha(0.35))
            .focus(|style| style.border_color(self.palette.cursor))
            .child(item(CONTEXT_MENU_ITEM_FIND, "Find"))
            .child(item(CONTEXT_MENU_ITEM_COPY, "Copy"))
            .child(item(CONTEXT_MENU_ITEM_PASTE, "Paste"))
            .child(item(CONTEXT_MENU_ITEM_SELECT_ALL, "Select All"))
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if self.search_focus_handle.is_focused(window) {
            let (text, actual) = self.search.text_for_utf16_range(range_utf16.clone())?;
            if actual != range_utf16 {
                *adjusted_range = Some(actual);
            }
            return Some(text);
        }
        let _ = cx;
        None
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        self.search_focus_handle.is_focused(window).then(|| {
            let (range, reversed) = self.search.selected_text_range_utf16();
            UTF16Selection { range, reversed }
        })
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.search_focus_handle.is_focused(window) {
            self.search.set_selection_utf16(range_utf16);
            self.restart_blink(cx);
            self.update_search_scroll();
            cx.notify();
        }
    }

    fn text_length_utf16(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> Option<usize> {
        self.search_focus_handle
            .is_focused(window)
            .then(|| self.search.display_text().encode_utf16().count())
    }

    fn marked_text_range(
        &self,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        if self.search_focus_handle.is_focused(window) {
            self.search.marked_range_utf16()
        } else {
            self.ime.marked_range()
        }
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_focus_handle.is_focused(window) {
            self.restart_blink(cx);
            if self.search.discard_composition() {
                self.update_search_scroll();
                cx.notify();
            }
            return;
        }
        if self.ime.preedit().is_some() {
            self.ime.unmark();
            self.ime_generation = self.term_generation;
            cx.notify();
        }
    }

    /// Commits IME text to the PTY. Single ASCII characters arrive as key events.
    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.search_focus_handle.is_focused(window) {
            self.edit_search(|search| search.commit_text(range_utf16, text), cx);
            return;
        }
        if let Some(text) = self.ime.commit(text) {
            self.session.write(text.into_bytes());
            self.session.scroll_to_bottom();
            self.accessibility_selection_dirty = true;
        }
        self.ime_generation = self.term_generation;
        cx.notify();
    }

    /// Updates IME preview text and commits it to the PTY later.
    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.search_focus_handle.is_focused(window) {
            self.search
                .replace_and_mark_text(range_utf16, new_text, new_selected_range);
            self.restart_blink(cx);
            self.update_search_scroll();
            cx.notify();
            return;
        }
        self.ime.mark(new_text);
        self.ime_generation = self.term_generation;
        // The preview and the candidate window are anchored to the caret, so the viewport
        // follows the cursor while a composition is open.
        self.session.scroll_to_bottom();
        cx.notify();
    }

    /// Positions the IME candidate window at the cursor cell.
    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        if self.search_focus_handle.is_focused(window)
            && let Some(layout) = self.search_input.borrow().clone()
        {
            let byte = self.search.display_byte_for_utf16(range_utf16.end);
            return Some(Bounds::new(
                point(
                    layout.bounds.origin.x + px(layout.metrics.x_for_byte(byte) - layout.scroll_x),
                    layout.bounds.origin.y,
                ),
                size(self.cell_width, layout.bounds.size.height),
            ));
        }
        let origin = self.candidate_window_origin(element_bounds);
        Some(Bounds::new(origin, size(self.cell_width, self.line_height)))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        if self.search_focus_handle.is_focused(window)
            && let Some(layout) = self.search_input.borrow().clone()
        {
            let x = f32::from(point.x - layout.bounds.origin.x) + layout.scroll_x;
            let byte = layout.metrics.hit_test(x);
            return Some(self.search.display_utf16_for_byte(byte));
        }
        None
    }

    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        true
    }

    fn text_input_configuration(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> TextInputConfiguration {
        TextInputConfiguration::default()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text_system = window.text_system().clone();
        let font_id = text_system.resolve_font(&self.font);
        self.cell_width = text_system
            .ch_advance(font_id, self.font_size)
            .unwrap_or(px(8.0))
            + TERMINAL_CELL_PADDING;
        self.sync_grid_size(cx);

        if let Some(title) = &self.title
            && !title.is_empty()
        {
            window.set_window_title(title);
        }

        self.sync_focus_handles();
        let focused = self.focus_handle.is_focused(window);
        let search_focused = self.search_open && self.search_focus_handle.is_focused(window);
        let reduce_motion = cx.reduce_motion();
        if reduce_motion != self.reduce_motion {
            self.reduce_motion = reduce_motion;
            if reduce_motion {
                self.stop_blink_timer();
            } else if focused || search_focused {
                self.restart_blink(cx);
            }
        }
        if focused != self.was_focused {
            self.was_focused = focused;
            if focused {
                self.session.focus_in();
                if !reduce_motion {
                    self.restart_blink(cx);
                }
            } else {
                self.session.focus_out();
                self.stop_blink_timer();
                // The preview was never sent to the child process, so it is dropped instead
                // of staying painted behind a menu, the search bar, or another tab.
                if self.ime.preedit().is_some() {
                    self.ime.unmark();
                    self.ime_generation = self.term_generation;
                }
            }
        }
        if search_focused != self.search_was_focused {
            self.search_was_focused = search_focused;
            if search_focused || focused {
                self.restart_blink(cx);
            } else {
                self.stop_blink_timer();
            }
        }

        let background: Hsla = self.palette.background;
        let border_transparent = Hsla::from(rgba(0x00000000));
        let border_focused = self.palette.focus;
        if self.hovered_link_generation != self.term_generation {
            self.hovered_link = None;
            self.hovered_link_generation = self.term_generation;
        }
        self.ensure_accessibility_cache();
        let accessibility = self
            .accessibility
            .as_ref()
            .expect("accessibility cache is initialized");
        let accessible_text = accessibility.text.clone();
        let description = accessibility_description(
            accessibility.cursor,
            accessibility.selection,
            self.resize_error.as_deref(),
        );
        let frame_generation = self.term_generation;
        let hover_link = (self.hovered_link_generation == frame_generation)
            .then_some(self.hovered_link.as_ref())
            .flatten()
            .map(|link| (link.start, link.end, self.palette.foreground));
        let preedit = (self.ime_generation == frame_generation)
            .then_some(self.ime.preedit())
            .flatten()
            .map(|text| {
                let (line, col) = accessibility.cursor.unwrap_or((0, 0));
                PreeditLayout {
                    text: text.to_owned(),
                    line,
                    col,
                    cells: crate::layout::text_cluster_display_cells(text),
                }
            });
        let search_highlights = self.search_highlights(frame_generation);
        let element = TerminalElement::new(
            self.session.clone(),
            cx.entity().downgrade(),
            self.focus_handle.clone(),
            focused,
            focused && self.cursor_visible(),
            self.font.clone(),
            self.font_size,
            self.cell_width,
            self.line_height,
            self.palette.clone(),
            hover_link,
            search_highlights,
            preedit,
        );

        let terminal_surface = div()
            .size_full()
            .border_1()
            .border_color(border_transparent)
            .focus_visible(|style| style.border_color(border_focused))
            .track_focus(&self.focus_handle)
            .key_context(TERMINAL_KEY_CONTEXT)
            .on_action(cx.listener(Self::cut_action))
            .on_action(cx.listener(Self::copy_action))
            .on_action(cx.listener(Self::paste_action))
            .on_action(cx.listener(Self::select_all_action))
            .capture_key_down(cx.listener(Self::on_terminal_key_down))
            .child(self.pointer_routes(cx))
            .child(element);

        self.dismiss_unfocused_context_menu(window);
        let resize_error = self.resize_error.clone();
        let context_menu = self.context_menu.map(|position| {
            let viewport = window.content_mask().bounds;
            context_menu_position(position - viewport.origin, viewport.size)
        });
        let resize_error_background = self.palette.background;
        let resize_error_border = self.palette.ansi[1];
        let resize_error_foreground = self.palette.foreground;

        div()
            .id("terminal-view")
            .size_full()
            .bg(background.alpha(1.0))
            .key_context(TERMINAL_KEY_CONTEXT)
            .role(Role::Term)
            .aria_label("Terminal Session")
            .aria_description(description)
            .aria_value(accessible_text)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _event, window, cx| {
                    if view.context_menu.is_some() {
                        view.close_context_menu(window, cx);
                    } else {
                        window.focus(&view.focus_handle, cx);
                    }
                }),
            )
            .on_mouse_down_out(cx.listener(|view, _event, window, cx| {
                if view.context_menu.is_some() {
                    view.close_context_menu(window, cx);
                }
            }))
            .child(terminal_surface)
            .when(self.search_open, |this| {
                this.child(self.search_bar(window, cx))
            })
            .when_some(context_menu, |this, position| {
                this.child(self.context_menu_element(position, cx))
            })
            .when_some(resize_error, |this, message| {
                this.child(
                    div()
                        .id("terminal-resize-error")
                        .role(Role::Alert)
                        .absolute()
                        .top(px(8.0))
                        .left(px(8.0))
                        .max_w(px(320.0))
                        .occlude()
                        .rounded(px(4.0))
                        .border_1()
                        .border_color(resize_error_border)
                        .bg(resize_error_background)
                        .px_2()
                        .py_1()
                        .text_color(resize_error_foreground)
                        .text_size(px(12.0))
                        .child(SharedString::from(message)),
                )
            })
    }
}

/// Creates a local PTY terminal view.
pub fn local_terminal_view(
    session: Arc<TerminalSession>,
    events: mpsc::UnboundedReceiver<SessionEvent>,
    font: Font,
    font_size: Pixels,
    line_height: Pixels,
    palette: Palette,
    cx: &mut App,
) -> Entity<TerminalView> {
    cx.new(|cx| TerminalView::new(session, events, font, font_size, line_height, palette, cx))
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    #[derive(Default)]
    struct RecordingIo {
        writes: std::sync::Mutex<Vec<Vec<u8>>>,
    }

    impl crate::io::TerminalIo for RecordingIo {
        fn write(&self, bytes: Cow<'static, [u8]>) {
            self.writes
                .lock()
                .expect("recording io")
                .push(bytes.into_owned());
        }

        fn resize(&self, _size: TermSize) {}

        fn shutdown(&self) {}
    }

    fn test_palette() -> Palette {
        let color = |value: u32| Hsla::from(rgba(value));
        crate::palette::TerminalTheme {
            ansi: [color(0xff000000); 16],
            dim_ansi: [color(0xff404040); 8],
            foreground: color(0xffffffff),
            bright_foreground: color(0xffffffff),
            dim_foreground: color(0xff808080),
            background: color(0xff000000),
            cursor: color(0xffffff00),
            selection: color(0xff264f78),
            search_match: color(0xffcc0033),
            search_match_active: color(0xff554433),
            focus: color(0xff61afef),
            scrollbar_thumb: color(0xff606060),
            scrollbar_thumb_hover: color(0xff909090),
            minimum_contrast: crate::palette::DEFAULT_MINIMUM_CONTRAST,
        }
        .into()
    }

    fn test_view(cx: &mut gpui::TestAppContext) -> (Entity<TerminalView>, Arc<RecordingIo>) {
        let io = Arc::new(RecordingIo::default());
        let transport = io.clone();
        let (session, _events) =
            TerminalSession::from_transport_facade_bounded(TermSize::new(20, 6), 64, move |_| {
                Ok(transport as Arc<dyn crate::io::TerminalIo>)
            })
            .expect("test session");
        // The view owns its event stream, so the test decides when events arrive.
        let (_events_tx, events) = mpsc::unbounded_channel();
        let view = cx.update(|cx| {
            crate::local_terminal_view(
                session,
                events,
                gpui::font("monospace"),
                px(12.0),
                px(18.0),
                test_palette(),
                cx,
            )
        });
        (view, io)
    }

    /// Opens a window with the terminal view as its root.
    fn add_terminal_window<'a>(
        cx: &'a mut gpui::TestAppContext,
        view: &Entity<TerminalView>,
    ) -> &'a mut gpui::VisualTestContext {
        let window = cx.update(|cx| {
            cx.open_window(gpui::WindowOptions::default(), |_, _| view.clone())
                .expect("terminal window")
        });
        let cx = gpui::VisualTestContext::from_window(window.into(), cx).into_mut();
        cx.run_until_parked();
        cx
    }

    fn writes(io: &RecordingIo) -> Vec<Vec<u8>> {
        io.writes.lock().expect("recording io").clone()
    }

    #[test]
    fn terminal_font_disables_programming_ligatures_without_losing_features() {
        let mut font = gpui::font("JetBrainsMono Nerd Font");
        font.features = FontFeatures(Arc::new(vec![
            ("calt".to_owned(), 1),
            ("liga".to_owned(), 1),
            ("ss01".to_owned(), 1),
        ]));
        let font = terminal_font(font);
        assert_eq!(
            font.features
                .0
                .iter()
                .find(|(tag, _)| tag == "calt")
                .map(|(_, value)| *value),
            Some(0)
        );
        assert_eq!(
            font.features
                .0
                .iter()
                .find(|(tag, _)| tag == "liga")
                .map(|(_, value)| *value),
            Some(0)
        );
        assert_eq!(
            font.features
                .0
                .iter()
                .find(|(tag, _)| tag == "ss01")
                .map(|(_, value)| *value),
            Some(1)
        );
    }

    /// A font-size change has to reach a session that is already open, otherwise the setting only
    /// shows up in the next terminal the user starts.
    #[gpui::test]
    fn font_metrics_apply_to_an_open_session(cx: &mut gpui::TestAppContext) {
        let (view, _io) = test_view(cx);
        let generation = view.read_with(cx, |view, _| view.term_generation);
        cx.update(|cx| {
            view.update(cx, |view, cx| {
                view.set_font(gpui::font("monospace"), px(18.0), px(26.0), cx)
            })
        });
        view.read_with(cx, |view, _| {
            assert_eq!(view.font_size, px(18.0));
            assert_eq!(view.line_height, px(26.0));
            assert!(
                view.term_generation > generation,
                "the search index, the accessible text, and the preedit move with the metrics"
            );
        });
        // The same values again are a no-op, so a re-render does not churn the generation.
        cx.update(|cx| {
            view.update(cx, |view, cx| {
                view.set_font(gpui::font("monospace"), px(18.0), px(26.0), cx)
            })
        });
        view.read_with(cx, |view, _| {
            assert_eq!(view.font_size, px(18.0));
        });
    }

    #[test]
    fn search_bar_width_stays_inside_narrow_terminal_bounds() {
        assert_eq!(search_bar_width(0.0), 0.0);
        assert_eq!(search_bar_width(20.0), 0.0);
        assert_eq!(search_bar_width(200.0), 180.0);
        assert_eq!(search_bar_width(1000.0), 320.0);
    }

    #[test]
    fn viewport_cursor_mapping_matches_session_convention() {
        assert_eq!(crate::session::viewport_cursor_row(0, 0, 24), Some(0));
        assert_eq!(crate::session::viewport_cursor_row(-3, 3, 24), Some(0));
        assert_eq!(crate::session::viewport_cursor_row(24, 0, 24), None);
    }

    #[test]
    fn ordinary_shell_wheel_scrolls_the_viewport() {
        assert_eq!(
            wheel_target(
                MouseInputMode::ReportToTerminal,
                TermMode::BRACKETED_PASTE,
                false
            ),
            WheelTarget::Viewport
        );
        assert_eq!(
            wheel_target(
                MouseInputMode::ReportToTerminal,
                TermMode::ALT_SCREEN,
                false
            ),
            WheelTarget::Viewport
        );
        assert_eq!(
            wheel_target(
                MouseInputMode::ReportToTerminal,
                TermMode::ALTERNATE_SCROLL,
                false
            ),
            WheelTarget::Viewport
        );
    }

    #[test]
    fn alternate_scroll_requires_both_modes_and_mouse_reports_take_priority() {
        let mouse = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(
            wheel_target(MouseInputMode::ReportToTerminal, mouse, false),
            WheelTarget::Mouse
        );
        assert_eq!(
            wheel_target(
                MouseInputMode::ReportToTerminal,
                mouse | TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL,
                false
            ),
            WheelTarget::Mouse
        );
        assert_eq!(
            wheel_target(
                MouseInputMode::ReportToTerminal,
                TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL,
                false
            ),
            WheelTarget::Alternate
        );
        assert_eq!(
            wheel_target(
                MouseInputMode::ReportToTerminal,
                TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL | TermMode::BRACKETED_PASTE,
                false
            ),
            WheelTarget::Alternate
        );
        assert_eq!(
            wheel_target(
                MouseInputMode::ReportToTerminal,
                TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL,
                true
            ),
            WheelTarget::Viewport
        );
        assert_eq!(
            wheel_target(MouseInputMode::LocalSelection, TermMode::default(), false),
            WheelTarget::Viewport
        );
    }

    #[test]
    fn grapheme_hit_test_snaps_to_search_boundaries() {
        let metrics = SearchTextMetrics {
            points: vec![(0, 0.0), (1, 7.0), (9, 21.0), (10, 28.0)],
        };
        assert_eq!(metrics.hit_test(-4.0), 0);
        assert_eq!(metrics.hit_test(3.0), 0);
        assert_eq!(metrics.hit_test(4.0), 1);
        assert_eq!(metrics.hit_test(15.0), 9);
        assert_eq!(metrics.hit_test(30.0), 10);
    }

    #[test]
    fn search_caret_bounds_follow_scrolled_grapheme_metrics() {
        let input = Bounds::new(point(px(10.0), px(20.0)), size(px(100.0), px(28.0)));
        let metrics = SearchTextMetrics {
            points: vec![(0, 0.0), (1, 7.0), (9, 21.0), (10, 28.0)],
        };
        let caret = search_caret_bounds(input, &metrics, 9, 3.0);
        assert_eq!(
            caret,
            Bounds::new(point(px(28.0), px(20.0)), size(px(1.0), px(28.0)))
        );
    }

    #[test]
    fn long_query_scroll_keeps_the_caret_visible() {
        assert_eq!(keep_search_scroll(0.0, 900.0, 200.0, 1000.0), 701.0);
        assert_eq!(keep_search_scroll(0.0, 1000.0, 200.0, 1000.0), 801.0);
        assert_eq!(keep_search_scroll(700.0, 100.0, 200.0, 1000.0), 100.0);
        assert_eq!(keep_search_scroll(900.0, 500.0, 200.0, 1000.0), 500.0);
    }

    #[test]
    fn pointer_capture_tracks_selection_and_scrollbar_drags_only() {
        assert!(pointer_capture_needed(true, false));
        assert!(pointer_capture_needed(false, true));
        assert!(!pointer_capture_needed(false, false));
    }

    #[test]
    fn grid_size_changes_only_follow_the_applied_terminal_size() {
        let size = TermSize::new(80, 24);
        assert!(!grid_size_changed(Some((80, 24)), size));
        assert!(grid_size_changed(Some((79, 24)), size));
        assert!(grid_size_changed(Some((80, 23)), size));
        assert!(grid_size_changed(None, size));
    }

    #[test]
    fn context_menu_is_dismissed_when_it_loses_the_focus() {
        assert!(!context_menu_needs_dismiss(false, false));
        assert!(!context_menu_needs_dismiss(false, true));
        assert!(!context_menu_needs_dismiss(true, true));
        assert!(context_menu_needs_dismiss(true, false));
    }

    #[test]
    fn context_menu_flips_and_snaps_inside_viewport() {
        let viewport = size(px(320.0), px(240.0));
        let near_bottom_right = context_menu_position(point(px(300.0), px(220.0)), viewport);
        assert_eq!(
            near_bottom_right.x,
            px(300.0 - CONTEXT_MENU_WIDTH - CONTEXT_MENU_GAP)
        );
        assert_eq!(near_bottom_right.y, px(220.0 - 120.0 - CONTEXT_MENU_GAP));
        let outside = context_menu_position(point(px(400.0), px(400.0)), viewport);
        assert!(outside.x <= px(320.0 - CONTEXT_MENU_MARGIN - CONTEXT_MENU_WIDTH));
        assert!(outside.y <= px(240.0 - CONTEXT_MENU_MARGIN - 120.0));
    }

    #[test]
    fn context_menu_snap_ignores_the_viewport_origin() {
        let viewport = Bounds::new(point(px(100.0), px(200.0)), size(px(320.0), px(240.0)));
        let window_point = point(px(150.0), px(260.0));
        let local = context_menu_position(window_point - viewport.origin, viewport.size);
        assert_eq!(local, point(px(50.0), px(60.0)));
    }

    #[test]
    fn accessibility_description_reports_selection_size_without_source_text() {
        let description = accessibility_description(
            Some((2, 4)),
            Some(SelectionSummary { cells: 17, rows: 2 }),
            None,
        );
        assert!(description.contains("17 cells across 2 rows"));
        assert!(!description.contains("secret"));
    }

    #[test]
    fn link_opening_requires_a_modifier_and_web_scheme() {
        let control = Modifiers {
            control: true,
            ..Default::default()
        };
        assert!(link_open_allowed("https://example.com", control));
        assert!(link_open_allowed("HTTP://example.com", control));
        assert!(!link_open_allowed(
            "https://example.com",
            Modifiers::default()
        ));
        assert!(!link_open_allowed("javascript:alert(1)", control));
        assert!(!link_open_allowed("file:///tmp/secret", control));
        assert!(!TerminalSecurityPolicy::default().allow_osc52);
    }

    #[test]
    fn context_menu_selection_wraps_in_both_directions() {
        assert_eq!(move_context_menu_selection(3, true), 0);
        assert_eq!(move_context_menu_selection(0, false), 3);
    }

    /// Copy and Paste stay in the menu while they are unavailable, and the highlight and the
    /// keyboard both skip them.
    #[test]
    fn context_menu_reports_unavailable_actions() {
        let empty = ContextMenuState {
            has_selection: false,
            has_clipboard: false,
        };
        let ready = ContextMenuState {
            has_selection: true,
            has_clipboard: true,
        };
        assert!(!empty.enabled(CONTEXT_MENU_ITEM_COPY));
        assert!(!empty.enabled(CONTEXT_MENU_ITEM_PASTE));
        assert!(empty.enabled(CONTEXT_MENU_ITEM_FIND));
        assert!(empty.enabled(CONTEXT_MENU_ITEM_SELECT_ALL));
        assert!(empty.unavailable_reason(CONTEXT_MENU_ITEM_COPY).is_some());
        assert!(empty.unavailable_reason(CONTEXT_MENU_ITEM_FIND).is_none());
        assert!(ready.unavailable_reason(CONTEXT_MENU_ITEM_PASTE).is_none());

        assert_eq!(
            first_enabled_context_menu_item(empty),
            CONTEXT_MENU_ITEM_FIND
        );
        assert_eq!(
            last_enabled_context_menu_item(empty),
            CONTEXT_MENU_ITEM_SELECT_ALL
        );
        assert_eq!(
            next_enabled_menu_item(empty, CONTEXT_MENU_ITEM_FIND, true),
            CONTEXT_MENU_ITEM_SELECT_ALL,
            "Down skips Copy and Paste"
        );
        assert_eq!(
            next_enabled_menu_item(empty, CONTEXT_MENU_ITEM_SELECT_ALL, false),
            CONTEXT_MENU_ITEM_FIND,
            "Up skips Copy and Paste"
        );
        assert_eq!(
            next_enabled_menu_item(ready, CONTEXT_MENU_ITEM_FIND, true),
            CONTEXT_MENU_ITEM_COPY
        );
    }

    #[test]
    fn platform_modifier_uses_xterm_meta_bit_for_mouse_reports() {
        let modifiers = TerminalView::report_modifiers(Modifiers {
            platform: true,
            ..Default::default()
        });
        assert!(modifiers.alt);
        assert!(!modifiers.platform);
    }

    fn focus_terminal(cx: &mut gpui::VisualTestContext, view: &Entity<TerminalView>) {
        let handle = view.read_with(cx, |view, _| view.focus_handle());
        cx.update(|window, cx| window.focus(&handle, cx));
    }

    #[gpui::test]
    fn terminal_keys_reach_the_pty_only_while_the_terminal_has_focus(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, io) = test_view(cx);
        let cx = add_terminal_window(cx, &view);
        focus_terminal(cx, &view);

        cx.simulate_keystrokes("a");
        cx.run_until_parked();
        assert_eq!(writes(&io), vec![b"a".to_vec()]);

        // The search bar owns its own keys, so typing in it never reaches the PTY.
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.toggle_search(window, cx));
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("b");
        cx.run_until_parked();

        assert_eq!(writes(&io), vec![b"a".to_vec()]);
        assert_eq!(
            view.read_with(cx, |view, _| view.search_query().to_owned()),
            "b"
        );
    }

    fn menu_is_open(cx: &gpui::VisualTestContext, view: &Entity<TerminalView>) -> bool {
        view.read_with(cx, |view, _| view.context_menu.is_some())
    }

    fn focused_handle(
        cx: &mut gpui::VisualTestContext,
        view: &Entity<TerminalView>,
    ) -> Option<gpui::FocusHandle> {
        let candidates = [
            view.read_with(cx, |view, _| view.context_menu_focus.clone()),
            view.read_with(cx, |view, _| view.focus_handle()),
        ];
        cx.update(|window, _| {
            candidates
                .into_iter()
                .find(|handle| handle.is_focused(window))
        })
    }

    #[gpui::test]
    fn context_menu_consumes_keys_and_closes_on_escape(cx: &mut gpui::TestAppContext) {
        let (view, io) = test_view(cx);
        let cx = add_terminal_window(cx, &view);
        focus_terminal(cx, &view);

        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.open_context_menu(point(px(40.0), px(40.0)), window, cx)
            });
        });
        cx.run_until_parked();
        assert!(menu_is_open(cx, &view));
        let menu_focus = view.read_with(cx, |view, _| view.context_menu_focus.clone());
        assert_eq!(focused_handle(cx, &view), Some(menu_focus));

        // The menu reads the selection and the clipboard when it opens, and both are empty in
        // this window. The test states them so every item is reachable.
        cx.update(|_, cx| {
            view.update(cx, |view, _| {
                view.context_menu_state = ContextMenuState {
                    has_selection: true,
                    has_clipboard: true,
                }
            });
        });
        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.context_menu_selected), 2);

        cx.simulate_keystrokes("a");
        cx.run_until_parked();
        assert!(
            writes(&io).is_empty(),
            "menu keys must not be encoded for the child process"
        );

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(!menu_is_open(cx, &view));
        assert_eq!(
            focused_handle(cx, &view),
            Some(view.read_with(cx, |view, _| view.focus_handle())),
            "the focus returns to the terminal"
        );
    }

    #[gpui::test]
    fn context_menu_is_dismissed_when_the_focus_moves_away(cx: &mut gpui::TestAppContext) {
        let (view, io) = test_view(cx);
        let cx = add_terminal_window(cx, &view);
        focus_terminal(cx, &view);

        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.open_context_menu(point(px(40.0), px(40.0)), window, cx)
            });
        });
        cx.run_until_parked();
        assert!(menu_is_open(cx, &view));

        cx.update(|window, cx| window.blur(cx));
        cx.run_until_parked();

        assert!(!menu_is_open(cx, &view));
        assert!(writes(&io).is_empty());
    }

    #[gpui::test]
    fn ime_preview_keeps_keys_out_of_the_pty_until_the_text_is_committed(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, io) = test_view(cx);
        let cx = add_terminal_window(cx, &view);
        focus_terminal(cx, &view);

        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.replace_and_mark_text_in_range(None, "zhong", None, window, cx)
            });
        });
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.ime.preedit().map(str::to_owned)),
            Some("zhong".to_owned())
        );

        // Typing and caret keys belong to the IME while a preview is open.
        cx.simulate_keystrokes("g");
        cx.simulate_keystrokes("left");
        cx.run_until_parked();
        assert!(writes(&io).is_empty());

        // Control keys still interrupt the child process.
        cx.simulate_keystrokes("ctrl-c");
        cx.run_until_parked();
        assert_eq!(writes(&io), vec![vec![0x03]]);

        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.replace_text_in_range(None, "中", window, cx)
            });
        });
        cx.run_until_parked();
        assert_eq!(
            writes(&io),
            vec![vec![0x03], "中".as_bytes().to_vec()],
            "the committed text is written once"
        );
        assert_eq!(
            view.read_with(cx, |view, _| view.ime.preedit().map(str::to_owned)),
            None
        );
    }
}
