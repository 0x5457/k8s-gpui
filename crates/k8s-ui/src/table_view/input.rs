//! Provides a small single-line text input without the editor dependency.

use std::ops::Range;

use gpui::{
    Animation, AnimationExt as _, AnyElement, App, Bounds, ClickEvent, ClipboardItem, Context,
    DispatchPhase, Element, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    FocusHandle, Focusable, GlobalElementId, Hitbox, HitboxBehavior, Hsla, InteractiveElement,
    IntoElement, KeyBinding, KeyDownEvent, Keystroke, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render, Role, ScrollHandle, ShapedLine,
    SharedString, Styled, TextAlign, TextInputConfiguration, TextRun, TextStyle, UTF16Selection,
    UnderlineStyle, Window, canvas, div, fill, point, px, size,
};
use k8s_actions::{Copy, Cut, Paste, Redo, SelectAll, Undo};
use ui::prelude::*;
use ui::{ButtonLike, Tooltip};
use unicode_segmentation::UnicodeSegmentation;

use crate::design;

pub struct TextInput {
    text: String,
    cursor: usize,
    selection_anchor: Option<usize>,
    placeholder: SharedString,
    aria_label: SharedString,
    aria_description: SharedString,
    clear_label: SharedString,
    width: Pixels,
    role: Role,
    leading_icon: bool,
    digits_only: bool,
    max_length: Option<usize>,
    invalid: bool,
    clear_escape_hint: bool,
    focus_handle: FocusHandle,
    clear_focus: FocusHandle,
    history: EditHistory,
    composition: Option<Composition>,
    dragging: bool,
    drag_origin: Option<(Point<Pixels>, Pixels)>,
    scroll: ScrollHandle,
    display: String,
    shape: Option<ShapeWindow>,
    last_bounds: Option<Bounds<Pixels>>,
    scroll_to_caret: bool,
    last_viewport_width: Pixels,
    blink_epoch: u64,
    last_focused: Option<bool>,
    on_change: ChangeHandler,
    id: SharedString,
}

type ChangeHandler = Box<dyn FnMut(&str, &mut App)>;

const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_TEXT_CHARS: usize = 16 * 1024;
const HISTORY_LIMIT: usize = 100;
const HISTORY_MAX_BYTES: usize = 256 * 1024;
/// A single line input never needs more than a screenful of glyphs, so the
/// layout only shapes a window of this many bytes of display text around the
/// caret, always cut on grapheme boundaries. Longer values still scroll, but
/// the per-frame shaping cost stops growing with the value.
const SHAPE_WINDOW_GRAPHEMES: usize = 512;
/// Pointer travel a multi-click selection survives before it turns into a drag.
const DRAG_THRESHOLD: Pixels = px(3.0);
const INPUT_PADDING: Pixels = design::space::SM;
const CLEAR_BUTTON_SIZE: Pixels = px(24.0);
const CARET_WIDTH: Pixels = px(1.0);

#[derive(Clone, Debug)]
struct HistorySnapshot {
    text: String,
    cursor: usize,
    selection_anchor: Option<usize>,
}

#[derive(Clone, Debug)]
struct Composition {
    range: Range<usize>,
    text: String,
    selected: Option<Range<usize>>,
    before: HistorySnapshot,
}

/// Consecutive edits of the same kind collapse into one undo step, so typing a
/// word costs one snapshot instead of one per character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditRun {
    Insert,
    Backspace,
    Delete,
}

/// The burst of same-kind edits that is still collecting. `entries` counts the
/// undo snapshots the burst owns — the state it started from plus one for every
/// snapshot a break forced — so one undo removes the whole burst. `split` marks
/// that the next edit records its own snapshot even though the kind matches.
#[derive(Clone, Copy, Debug)]
struct OpenRun {
    kind: EditRun,
    entries: usize,
    split: bool,
}

#[derive(Default)]
struct EditHistory {
    undo: Vec<HistorySnapshot>,
    redo: Vec<HistorySnapshot>,
    undo_bytes: usize,
    redo_bytes: usize,
    /// The burst of same-kind edits still collecting, if any.
    run: Option<OpenRun>,
}

fn snapshot_bytes(snapshot: &HistorySnapshot) -> usize {
    snapshot.text.len()
}

fn trim_history(history: &mut EditHistory) {
    while history.undo_bytes + history.redo_bytes > HISTORY_MAX_BYTES {
        if !history.undo.is_empty() {
            let snapshot = history.undo.remove(0);
            history.undo_bytes = history.undo_bytes.saturating_sub(snapshot_bytes(&snapshot));
        } else if !history.redo.is_empty() {
            let snapshot = history.redo.remove(0);
            history.redo_bytes = history.redo_bytes.saturating_sub(snapshot_bytes(&snapshot));
        } else {
            break;
        }
    }
}

/// Whether an edit of this kind joins the open burst instead of costing a
/// snapshot of its own. A burst that was just undone owns no snapshot any more,
/// so it only keeps collecting while the history still holds a state to return
/// to; with an empty history the edit records its own.
fn joins_open_run(history: &EditHistory, run: Option<EditRun>) -> bool {
    let Some(run) = run else {
        return false;
    };
    history
        .run
        .as_ref()
        .is_some_and(|open| open.kind == run && !open.split && !history.undo.is_empty())
}

/// Records the state an edit started from. `run` coalesces the edit into the
/// open burst of the same kind, which is the only case where no new snapshot is
/// needed; the caller then does not have to clone the text at all.
fn record_history(history: &mut EditHistory, snapshot: &HistorySnapshot, run: Option<EditRun>) {
    if joins_open_run(history, run) {
        return;
    }
    // A snapshot forced by a break stays inside the same burst, so one undo
    // still removes the burst as a whole.
    let continues_run = match (run, history.run) {
        (Some(run), Some(open)) => open.kind == run,
        _ => false,
    };
    push_history(history, snapshot.clone(), false);
    if continues_run && let Some(open) = history.run.as_mut() {
        open.entries += 1;
        open.split = false;
    } else {
        history.run = run.map(|kind| OpenRun {
            kind,
            entries: 1,
            split: false,
        });
    }
}

fn push_history(history: &mut EditHistory, snapshot: HistorySnapshot, redo: bool) {
    let bytes = snapshot_bytes(&snapshot);
    if redo {
        history.redo.push(snapshot);
        history.redo_bytes += bytes;
        if history.redo.len() > HISTORY_LIMIT {
            let removed = history.redo.remove(0);
            history.redo_bytes = history.redo_bytes.saturating_sub(snapshot_bytes(&removed));
        }
    } else {
        history.undo.push(snapshot);
        history.undo_bytes += bytes;
        if history.undo.len() > HISTORY_LIMIT {
            let removed = history.undo.remove(0);
            history.undo_bytes = history.undo_bytes.saturating_sub(snapshot_bytes(&removed));
        }
    }
    trim_history(history);
}

fn pop_undo(history: &mut EditHistory) -> Option<HistorySnapshot> {
    let snapshot = history.undo.pop()?;
    history.undo_bytes = history.undo_bytes.saturating_sub(snapshot_bytes(&snapshot));
    Some(snapshot)
}

fn pop_history(history: &mut EditHistory, redo: bool) -> Option<HistorySnapshot> {
    if redo {
        history.run = None;
        let snapshot = history.redo.pop()?;
        history.redo_bytes = history.redo_bytes.saturating_sub(snapshot_bytes(&snapshot));
        return Some(snapshot);
    }
    // One undo removes the whole open burst: the snapshots it owns, the oldest
    // of which is the state the burst started from and is what the caller
    // restores. The burst stays open for the edit that continues it, so a
    // restored state is not recorded twice.
    let burst = history
        .run
        .filter(|open| open.entries > 0)
        .map_or(0, |open| open.entries.min(history.undo.len()));
    if burst > 0 {
        let snapshot = history.undo[history.undo.len() - burst].clone();
        for _ in 0..burst {
            pop_undo(history);
        }
        if let Some(open) = history.run.as_mut() {
            open.entries = 0;
        }
        return Some(snapshot);
    }
    // The burst is spent, so this undo steps through plain snapshots again.
    history.run = None;
    pop_undo(history)
}

fn clear_redo(history: &mut EditHistory) {
    history.redo.clear();
    history.redo_bytes = 0;
}

/// Caret movement, selection changes, and every non-coalescing edit split the
/// open burst: the next keystroke records its own state, and the burst stays
/// open so a single undo still removes the whole thing.
fn break_history_run(history: &mut EditHistory) {
    if let Some(open) = history.run.as_mut() {
        open.split = true;
    }
}

fn truncate_to_budget(text: &mut String, max_chars: usize, max_bytes: usize) {
    let mut end = 0;
    for (chars, (index, ch)) in text.char_indices().enumerate() {
        let next = index + ch.len_utf8();
        if chars >= max_chars || next > max_bytes {
            end = index;
            break;
        }
        end = next;
    }
    text.truncate(end);
}

fn normalize_text(text: &str, digits_only: bool, max_length: Option<usize>) -> String {
    let max_chars = max_length.unwrap_or(MAX_TEXT_CHARS).min(MAX_TEXT_CHARS);
    let mut result = String::with_capacity(text.len().min(MAX_TEXT_BYTES));
    let mut chars = 0;
    for ch in text.chars() {
        if digits_only && !ch.is_ascii_digit() {
            continue;
        }
        let ch = if ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}') {
            ' '
        } else {
            ch
        };
        if chars >= max_chars || result.len() + ch.len_utf8() > MAX_TEXT_BYTES {
            break;
        }
        result.push(ch);
        chars += 1;
    }
    result
}

/// The shaped slice of the display text that is currently laid out, plus the
/// decoration state it was shaped for. Cached on the input so a repaint that
/// does not change the value, the text style, or the decoration reuses it.
#[derive(Clone)]
struct ShapeWindow {
    /// `display[start..end]`, always cut on grapheme boundaries.
    text: String,
    start: usize,
    end: usize,
    line: ShapedLine,
    style: TextStyle,
    selection: Option<Range<usize>>,
    marked: Option<Range<usize>>,
}

impl ShapeWindow {
    fn contains(&self, index: usize) -> bool {
        (self.start..=self.end).contains(&index)
    }

    /// Index inside the shaped window, snapped down to a grapheme boundary.
    fn local_index(&self, display_index: usize) -> usize {
        if display_index <= self.start {
            return 0;
        }
        let local = (display_index - self.start).min(self.text.len());
        floor_grapheme_boundary(&self.text, local)
    }

    fn x_for_display_index(&self, display_index: usize) -> Pixels {
        self.line.x_for_index(self.local_index(display_index))
    }

    fn display_index_for_x(&self, x: Pixels) -> usize {
        let local = floor_char_boundary(&self.text, self.line.closest_index_for_x(x));
        self.start + floor_grapheme_boundary(&self.text, local)
    }

    /// Horizontal advance of the shaped window plus the trailing caret gap.
    fn content_width(&self) -> Pixels {
        self.line.width() + px(2.0)
    }
}

/// Byte range of at most [`SHAPE_WINDOW_GRAPHEMES`] bytes of display text
/// around `focus`, always cut on grapheme boundaries. The whole string when it
/// already fits in that many graphemes, so short values never move.
fn shape_window_range(display: &str, focus: usize) -> (usize, usize) {
    if display
        .graphemes(true)
        .nth(SHAPE_WINDOW_GRAPHEMES)
        .is_none()
    {
        return (0, display.len());
    }
    let focus = floor_char_boundary(display, focus);
    // The budget is bytes, because that is what shaping costs, and the window
    // is cut on grapheme boundaries because the caret may only sit on one.
    // Centering the caret and filling the rest of the budget forward keeps the
    // window a full budget at either edge, instead of the half that would fit.
    let half = SHAPE_WINDOW_GRAPHEMES / 2;
    let start = ceil_grapheme_boundary(display, focus.saturating_sub(half));
    let end = floor_grapheme_boundary(display, (start + SHAPE_WINDOW_GRAPHEMES).min(display.len()));
    (start, end)
}

fn install_edit_key_bindings(cx: &mut Context<TextInput>) {
    let (select_all, cut, copy, paste, undo, redo) = {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let context = gpui::KeyContext::parse("TextInput").ok();
        let key_taken = |spec: &str| {
            let (Ok(key), Some(context)) = (Keystroke::parse(spec), context.as_ref()) else {
                return false;
            };
            let (matches, pending) =
                keymap.bindings_for_input(&[key], std::slice::from_ref(context));
            !matches.is_empty() || pending
        };
        (
            keymap.bindings_for_action(&SelectAll).next().is_none() && !key_taken("secondary-a"),
            keymap.bindings_for_action(&Cut).next().is_none() && !key_taken("secondary-x"),
            keymap.bindings_for_action(&Copy).next().is_none() && !key_taken("secondary-c"),
            keymap.bindings_for_action(&Paste).next().is_none() && !key_taken("secondary-v"),
            keymap.bindings_for_action(&Undo).next().is_none() && !key_taken("secondary-z"),
            keymap.bindings_for_action(&Redo).next().is_none() && !key_taken("secondary-shift-z"),
        )
    };
    let mut bindings = Vec::new();
    if select_all {
        bindings.push(KeyBinding::new("secondary-a", SelectAll, Some("TextInput")));
    }
    if cut {
        bindings.push(KeyBinding::new("secondary-x", Cut, Some("TextInput")));
    }
    if copy {
        bindings.push(KeyBinding::new("secondary-c", Copy, Some("TextInput")));
    }
    if paste {
        bindings.push(KeyBinding::new("secondary-v", Paste, Some("TextInput")));
    }
    if undo {
        bindings.push(KeyBinding::new("secondary-z", Undo, Some("TextInput")));
    }
    if redo {
        bindings.push(KeyBinding::new(
            "secondary-shift-z",
            Redo,
            Some("TextInput"),
        ));
    }
    if !bindings.is_empty() {
        cx.bind_keys(bindings);
    }
}

impl TextInput {
    pub fn new(
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
        on_change: impl FnMut(&str, &mut App) + 'static,
    ) -> Self {
        install_edit_key_bindings(cx);
        let placeholder = placeholder.into();
        Self {
            text: String::new(),
            cursor: 0,
            selection_anchor: None,
            placeholder,
            aria_label: "Filter Resources".into(),
            aria_description: "Enter a resource name. Press Escape to clear the filter.".into(),
            clear_label: "Clear Filter".into(),
            width: px(240.0),
            role: Role::SearchInput,
            leading_icon: true,
            digits_only: false,
            max_length: None,
            invalid: false,
            clear_escape_hint: true,
            // The filter is the first control in the toolbar tab group.
            focus_handle: cx.focus_handle().tab_stop(true).tab_index(0),
            clear_focus: cx.focus_handle().tab_stop(true).tab_index(1),
            history: EditHistory::default(),
            composition: None,
            dragging: false,
            drag_origin: None,
            scroll: ScrollHandle::new(),
            display: String::new(),
            shape: None,
            last_bounds: None,
            scroll_to_caret: true,
            last_viewport_width: px(0.0),
            blink_epoch: 0,
            last_focused: None,
            on_change: Box::new(on_change),
            id: format!("text-input-{}", cx.entity_id()).into(),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn clear_focus_handle(&self) -> FocusHandle {
        self.clear_focus.clone()
    }

    /// The composed value that is drawn: the stored text with the IME preedit
    /// spliced in. Kept in sync by [`Self::invalidate_layout`] so a frame does
    /// not rebuild the whole string for every caret and selection query.
    pub(crate) fn display_text(&self) -> &str {
        &self.display
    }

    pub(crate) fn is_composing(&self) -> bool {
        self.composition.is_some()
    }

    fn max_chars(&self) -> usize {
        self.max_length
            .unwrap_or(MAX_TEXT_CHARS)
            .min(MAX_TEXT_CHARS)
    }

    fn invalidate_layout(&mut self) {
        self.shape = None;
        self.display = composed_text(&self.text, self.composition.as_ref());
    }

    fn reset_blink_phase(&mut self) {
        self.blink_epoch = self.blink_epoch.wrapping_add(1);
    }

    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = normalize_text(&text.into(), self.digits_only, self.max_length);
        self.cursor = self.text.len();
        self.selection_anchor = None;
        self.composition = None;
        self.invalidate_layout();
        self.scroll.set_offset(point(px(0.0), px(0.0)));
        self.scroll_to_caret = true;
        self
    }

    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        let canceled = self.cancel_composition();
        let text = normalize_text(&text.into(), self.digits_only, self.max_length);
        if self.text == text {
            if canceled {
                cx.notify();
            }
            return;
        }
        self.record_history(None);
        self.text = text;
        self.cursor = self.text.len();
        self.selection_anchor = None;
        self.invalidate_layout();
        self.scroll_to_caret = true;
        self.reset_blink_phase();
        self.notify_changed(cx);
    }

    pub fn with_role(mut self, role: Role) -> Self {
        self.role = role;
        self
    }

    pub fn without_leading_icon(mut self) -> Self {
        self.leading_icon = false;
        self
    }

    pub fn without_escape_hint(mut self) -> Self {
        self.clear_escape_hint = false;
        self
    }

    pub fn with_numeric_input(mut self, max_length: usize) -> Self {
        self.digits_only = true;
        self.max_length = Some(max_length);
        self.text = normalize_text(&self.text, self.digits_only, self.max_length);
        self.cursor = self.text.len();
        self.selection_anchor = None;
        self.composition = None;
        self.invalidate_layout();
        self.scroll_to_caret = true;
        self
    }

    pub fn with_max_length(mut self, max_length: usize) -> Self {
        self.max_length = Some(max_length);
        self.text = normalize_text(&self.text, self.digits_only, self.max_length);
        self.cursor = self.text.len();
        self.selection_anchor = None;
        self.composition = None;
        self.invalidate_layout();
        self.scroll_to_caret = true;
        self
    }

    pub fn set_invalid(&mut self, invalid: bool, cx: &mut Context<Self>) {
        if self.invalid == invalid {
            return;
        }
        self.invalid = invalid;
        cx.notify();
    }

    pub fn with_accessibility(
        mut self,
        label: impl Into<SharedString>,
        description: impl Into<SharedString>,
        clear_label: impl Into<SharedString>,
    ) -> Self {
        self.aria_label = label.into();
        self.aria_description = description.into();
        self.clear_label = clear_label.into();
        self
    }

    pub fn with_width(mut self, width: Pixels) -> Self {
        self.width = width;
        self
    }

    pub fn reports_active_descendant(self) -> Self {
        self
    }

    pub fn set_reports_active_descendant(&mut self, _reports: bool, _cx: &mut Context<Self>) {}

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.cancel_composition();
        self.reset_blink_phase();
        if self.text.is_empty() {
            cx.notify();
            return;
        }
        self.record_history(None);
        self.text.clear();
        self.cursor = 0;
        self.selection_anchor = None;
        self.invalidate_layout();
        self.scroll_to_caret = true;
        self.notify_changed(cx);
    }

    fn snapshot(&self) -> HistorySnapshot {
        HistorySnapshot {
            text: self.text.clone(),
            cursor: self.cursor,
            selection_anchor: self.selection_anchor,
        }
    }

    /// Snapshot the current value, coalescing into the open burst of the same
    /// edit kind so a burst of typing or backspacing is one undo step.
    fn record_history(&mut self, run: Option<EditRun>) {
        if joins_open_run(&self.history, run) {
            clear_redo(&mut self.history);
            return;
        }
        let snapshot = self.snapshot();
        record_history(&mut self.history, &snapshot, run);
        clear_redo(&mut self.history);
    }

    fn restore(&mut self, snapshot: HistorySnapshot) {
        self.text = snapshot.text;
        self.cursor = floor_grapheme_boundary(&self.text, snapshot.cursor);
        self.selection_anchor = snapshot
            .selection_anchor
            .map(|anchor| floor_grapheme_boundary(&self.text, anchor));
        self.composition = None;
        self.invalidate_layout();
        self.scroll_to_caret = true;
        self.reset_blink_phase();
    }

    fn undo(&mut self) -> bool {
        let Some(snapshot) = pop_history(&mut self.history, false) else {
            return false;
        };
        let current = self.snapshot();
        push_history(&mut self.history, current, true);
        self.restore(snapshot);
        true
    }

    fn redo(&mut self) -> bool {
        let Some(snapshot) = pop_history(&mut self.history, true) else {
            return false;
        };
        let current = self.snapshot();
        push_history(&mut self.history, current, false);
        self.restore(snapshot);
        true
    }

    fn selection_bounds(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        (anchor != self.cursor).then(|| (anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    fn display_selection_bounds(&self) -> Option<Range<usize>> {
        let display = &self.display;
        let selection = self.composition.as_ref().map_or_else(
            || self.selection_bounds().map(|(start, end)| start..end),
            |composition| {
                composition.selected.as_ref().map(|selected| {
                    (composition.range.start + selected.start)
                        ..(composition.range.start + selected.end)
                })
            },
        );
        selection.map(|range| {
            floor_grapheme_boundary(display, range.start)
                ..ceil_grapheme_boundary(display, range.end)
        })
    }

    fn display_cursor(&self) -> usize {
        let cursor = self
            .composition
            .as_ref()
            .map_or(self.cursor, |composition| {
                composition.selected.as_ref().map_or(
                    composition.range.start + composition.text.len(),
                    |selected| composition.range.start + selected.end,
                )
            });
        floor_grapheme_boundary(&self.display, cursor)
    }

    fn value_byte_for_display(&self, display_byte: usize) -> usize {
        let Some(composition) = &self.composition else {
            return floor_grapheme_boundary(&self.text, display_byte);
        };
        let start = composition.range.start;
        let end = start + composition.text.len();
        let value = if display_byte <= start {
            start.min(display_byte)
        } else if display_byte >= end {
            start + display_byte - end
        } else if display_byte - start <= composition.text.len() / 2 {
            start
        } else {
            end
        };
        floor_grapheme_boundary(&self.text, value)
    }

    pub(crate) fn cancel_composition(&mut self) -> bool {
        let Some(composition) = self.composition.take() else {
            return false;
        };
        self.cursor = floor_grapheme_boundary(&self.text, composition.before.cursor);
        self.selection_anchor = composition
            .before
            .selection_anchor
            .map(|anchor| floor_grapheme_boundary(&self.text, anchor));
        self.invalidate_layout();
        self.scroll_to_caret = true;
        self.reset_blink_phase();
        true
    }

    fn notify_changed(&mut self, cx: &mut Context<Self>) {
        cx.notify();
        let Self {
            text, on_change, ..
        } = self;
        (on_change)(text, cx);
    }

    fn limited_replacement(&self, start: usize, end: usize, text: &str) -> String {
        let mut text = normalize_text(text, self.digits_only, None);
        let existing_chars = self.text[..start].chars().count() + self.text[end..].chars().count();
        let existing_bytes = self.text[..start].len() + self.text[end..].len();
        let available_chars = self.max_chars().saturating_sub(existing_chars);
        let available_bytes = MAX_TEXT_BYTES.saturating_sub(existing_bytes);
        truncate_to_budget(&mut text, available_chars, available_bytes);
        text
    }

    /// Applies a replacement the caller already fitted into the char and byte
    /// budget. The budget and the no-op case are checked against the existing
    /// slices, so the common edit never copies the whole value.
    fn edit_range(
        &mut self,
        range: Range<usize>,
        replacement: String,
        before: Option<HistorySnapshot>,
    ) -> bool {
        self.edit_range_with_run(range, replacement, before, None)
    }

    fn edit_range_with_run(
        &mut self,
        range: Range<usize>,
        replacement: String,
        before: Option<HistorySnapshot>,
        run: Option<EditRun>,
    ) -> bool {
        let Range { start, end } = range;
        if start > end
            || end > self.text.len()
            || !self.text.is_char_boundary(start)
            || !self.text.is_char_boundary(end)
        {
            return false;
        }
        self.reset_blink_phase();
        if self.text[start..end] == replacement {
            self.cursor = start + replacement.len();
            self.selection_anchor = None;
            self.scroll_to_caret = true;
            return false;
        }
        if self.text.len() - (end - start) + replacement.len() > MAX_TEXT_BYTES {
            return false;
        }
        match before {
            Some(snapshot) => {
                record_history(&mut self.history, &snapshot, run);
                clear_redo(&mut self.history);
            }
            None => self.record_history(run),
        }
        self.text.replace_range(start..end, &replacement);
        self.cursor = start + replacement.len();
        self.selection_anchor = None;
        self.invalidate_layout();
        self.scroll_to_caret = true;
        true
    }

    fn delete_range(&mut self, start: usize, end: usize) -> bool {
        self.edit_range(start..end, String::new(), None)
    }

    fn delete_range_with_run(&mut self, start: usize, end: usize, run: EditRun) -> bool {
        self.edit_range_with_run(start..end, String::new(), None, Some(run))
    }

    fn delete_selection(&mut self) -> bool {
        self.selection_bounds()
            .is_some_and(|(start, end)| self.delete_range(start, end))
    }

    fn previous_grapheme_boundary(&self, cursor: usize) -> usize {
        previous_grapheme_boundary(&self.text, cursor)
    }

    fn next_grapheme_boundary(&self, cursor: usize) -> usize {
        next_grapheme_boundary(&self.text, cursor)
    }

    fn set_cursor(&mut self, target: usize, extend: bool) {
        if extend {
            self.selection_anchor.get_or_insert(self.cursor);
        } else {
            self.selection_anchor = None;
        }
        self.cursor = target;
        break_history_run(&mut self.history);
        self.scroll_to_caret = true;
        self.reset_blink_phase();
    }

    fn move_horizontal(&mut self, backwards: bool, extend: bool) {
        self.cancel_composition();
        let target = if let Some((start, end)) = self.selection_bounds()
            && !extend
        {
            if backwards { start } else { end }
        } else if backwards {
            self.previous_grapheme_boundary(self.cursor)
        } else {
            self.next_grapheme_boundary(self.cursor)
        };
        self.set_cursor(target, extend);
    }

    fn move_to_line_edge(&mut self, backwards: bool, extend: bool) {
        self.cancel_composition();
        self.set_cursor(if backwards { 0 } else { self.text.len() }, extend);
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.cancel_composition();
        self.reset_blink_phase();
        if self.text.is_empty() {
            cx.notify();
            return;
        }
        self.selection_anchor = Some(0);
        self.cursor = self.text.len();
        break_history_run(&mut self.history);
        self.scroll_to_caret = true;
        cx.notify();
    }

    fn selected_text(&self) -> Option<String> {
        self.selection_bounds()
            .map(|(start, end)| self.text[start..end].to_owned())
    }

    fn copy(&self, cx: &App) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn cut(&mut self, cx: &mut Context<Self>) {
        let canceled = self.cancel_composition();
        self.copy(cx);
        if self.delete_selection() {
            self.notify_changed(cx);
        } else if canceled {
            cx.notify();
        }
    }

    fn paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn undo_action(&mut self, _: &Undo, _window: &mut Window, cx: &mut Context<Self>) {
        if self.undo() {
            self.notify_changed(cx);
        }
        cx.stop_propagation();
    }

    fn redo_action(&mut self, _: &Redo, _window: &mut Window, cx: &mut Context<Self>) {
        if self.redo() {
            self.notify_changed(cx);
        }
        cx.stop_propagation();
    }

    fn cut_action(&mut self, _: &Cut, _window: &mut Window, cx: &mut Context<Self>) {
        self.cut(cx);
        cx.stop_propagation();
    }

    fn copy_action(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        self.copy(cx);
        cx.stop_propagation();
    }

    fn paste_action(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        self.paste(window, cx);
        cx.stop_propagation();
    }

    fn select_all_action(&mut self, _: &SelectAll, _window: &mut Window, cx: &mut Context<Self>) {
        self.select_all(cx);
        cx.stop_propagation();
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.handle_tab(&event.keystroke, window, cx) {
            cx.stop_propagation();
            return;
        }
        self.handle_keystroke(&event.keystroke, window, cx);
    }

    /// The clear button is a tab stop of this control, so plain Tab reaches it
    /// even when no host binds `FocusNext` for the key. Key listeners run after
    /// the keymap, so a host that already moved focus on Tab — the shell
    /// binding `FocusNext`, or the table selecting the next column — owns the
    /// keystroke and this steps nothing.
    fn handle_tab(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if keystroke.key != "tab"
            || keystroke.modifiers.shift
            || keystroke.modifiers.control
            || keystroke.modifiers.platform
            || keystroke.modifiers.alt
            || keystroke.modifiers.function
        {
            return false;
        }
        // Only the field steps forward; Tab on the button itself, and
        // Shift+Tab out of the control, stay with the host.
        if !self.focus_handle.is_focused(window)
            || self.clear_focus.is_focused(window)
            || self.text.is_empty()
        {
            return false;
        }
        window.focus(&self.clear_focus, cx);
        true
    }

    /// The clear button is a real tab stop, so it has to answer the keys a
    /// button answers. `ButtonLike` only wires the mouse, and the input's own
    /// handler does not own the caret while the button has focus.
    fn handle_clear_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.modifiers.control
            || event.keystroke.modifiers.platform
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.function
        {
            return;
        }
        if !matches!(event.keystroke.key.as_str(), "enter" | "return" | "space") {
            return;
        }
        self.clear(cx);
        window.focus(&self.focus_handle, cx);
        cx.stop_propagation();
    }

    /// Keys the shared control owns. Anything with a command, alternate, or
    /// function modifier belongs to the host, including `Ctrl/Cmd+Home` and
    /// `Ctrl/Cmd+End`, so a result list can use them for result navigation
    /// while plain `Home`/`End` still move the caret.
    pub(crate) fn handle_keystroke(
        &mut self,
        keystroke: &Keystroke,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if keystroke.modifiers.control
            || keystroke.modifiers.platform
            || keystroke.modifiers.alt
            || keystroke.modifiers.function
        {
            return;
        }
        let composing = self.is_composing();
        let changed = match keystroke.key.as_str() {
            "left" => {
                self.move_horizontal(true, keystroke.modifiers.shift);
                cx.notify();
                false
            }
            "right" => {
                self.move_horizontal(false, keystroke.modifiers.shift);
                cx.notify();
                false
            }
            "home" => {
                self.move_to_line_edge(true, keystroke.modifiers.shift);
                cx.stop_propagation();
                cx.notify();
                false
            }
            "end" => {
                self.move_to_line_edge(false, keystroke.modifiers.shift);
                cx.stop_propagation();
                cx.notify();
                false
            }
            "backspace" => {
                self.cancel_composition();
                let mut changed = self.delete_selection();
                if !changed {
                    let cursor = self.cursor;
                    let start = self.previous_grapheme_boundary(cursor);
                    changed = self.delete_range_with_run(start, cursor, EditRun::Backspace);
                }
                changed
            }
            "delete" => {
                self.cancel_composition();
                let mut changed = self.delete_selection();
                if !changed {
                    let cursor = self.cursor;
                    let end = self.next_grapheme_boundary(cursor);
                    changed = self.delete_range_with_run(cursor, end, EditRun::Delete);
                }
                changed
            }
            "escape" => {
                // The preedit is the first thing Escape has to give up, and
                // cancelling it never touches the committed value.
                if self.cancel_composition() {
                    cx.stop_propagation();
                    false
                } else if self.clear_escape_hint && !self.text.is_empty() {
                    let cleared = self.delete_range(0, self.text.len());
                    if cleared {
                        cx.stop_propagation();
                    }
                    cleared
                } else {
                    // Nothing to cancel and nothing to clear: the host decides,
                    // so an empty field does not swallow the key.
                    false
                }
            }
            _ => false,
        };
        if changed {
            self.notify_changed(cx);
        } else if composing && self.composition.is_none() {
            cx.notify();
        }
    }

    fn display_index_for_point(&self, point: Point<Pixels>) -> usize {
        if self.display.is_empty() {
            return 0;
        }
        let (Some(shape), Some(bounds)) = (self.shape.as_ref(), self.last_bounds.as_ref()) else {
            return 0;
        };
        let x = (point.x - bounds.left()).clamp(px(0.0), shape.line.width());
        shape.display_index_for_x(x)
    }

    fn index_for_point(&self, point: Point<Pixels>) -> usize {
        self.value_byte_for_display(self.display_index_for_point(point))
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self.index_for_point(event.position);
        self.cancel_composition();
        self.reset_blink_phase();
        let multi_click = event.click_count >= 2;
        if event.modifiers.shift {
            self.selection_anchor.get_or_insert(self.cursor);
            self.cursor = target;
        } else if event.click_count >= 3 {
            self.selection_anchor = Some(0);
            self.cursor = self.text.len();
        } else if multi_click {
            let range = word_bounds(&self.text, target);
            self.selection_anchor = Some(range.start);
            self.cursor = range.end;
        } else {
            self.selection_anchor = Some(target);
            self.cursor = target;
        }
        self.dragging = true;
        // A word or line selection survives the pointer jitter of the click
        // that made it, and stays grabbable after that click ends: only real
        // travel turns the drag into a new selection.
        self.drag_origin = multi_click.then_some((event.position, DRAG_THRESHOLD));
        break_history_run(&mut self.history);
        self.scroll_to_caret = true;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// A drag is in progress, or a multi-click selection is still armed and a
    /// held button can pick it up again.
    fn can_drag(&self) -> bool {
        self.dragging || self.drag_origin.is_some()
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_drag() {
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            // The button is up, so the gesture is over for good: an armed
            // multi-click that nobody drags dies here.
            self.dragging = false;
            self.drag_origin = None;
            return;
        }
        if let Some((origin, threshold)) = self.drag_origin {
            let travelled = (event.position - origin).magnitude();
            if travelled < f64::from(threshold) {
                return;
            }
            self.drag_origin = None;
        }
        // A re-armed multi-click becomes a real drag here, and the pointer
        // element captures the pointer again for the rest of the travel.
        self.dragging = true;
        self.reset_blink_phase();
        self.cursor = self.index_for_point(event.position);
        self.scroll_to_caret = true;
        cx.notify();
    }

    fn on_mouse_up(&mut self) {
        // The armed origin stays: a word selection outlives the click that made
        // it. A move without the button held, or the next click, drops it.
        self.dragging = false;
    }
}

fn previous_grapheme_boundary(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    let floor = floor_grapheme_boundary(text, cursor);
    let ceil = ceil_grapheme_boundary(text, cursor);
    let char_cursor = floor_char_boundary(text, cursor);
    if floor < cursor && (char_cursor == floor || text[floor..ceil].contains('\u{200d}')) {
        return floor;
    }
    let previous = text[..char_cursor]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index);
    let previous_floor = floor_grapheme_boundary(text, previous);
    let previous_ceil = ceil_grapheme_boundary(text, previous);
    if previous_floor < previous && text[previous_floor..previous_ceil].contains('\u{200d}') {
        previous_floor
    } else {
        previous
    }
}

fn floor_grapheme_boundary(text: &str, cursor: usize) -> usize {
    let cursor = floor_char_boundary(text, cursor);
    // The end of a string is always a boundary, and the common caret position
    // while typing is the end, so skip the scan for it.
    if cursor == text.len() {
        return cursor;
    }
    text.grapheme_indices(true)
        .map(|(start, grapheme)| start + grapheme.len())
        .take_while(|boundary| *boundary <= cursor)
        .last()
        .unwrap_or(0)
}

fn ceil_grapheme_boundary(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    if cursor == 0 {
        return 0;
    }
    text.grapheme_indices(true)
        .map(|(start, grapheme)| start + grapheme.len())
        .find(|boundary| *boundary >= cursor)
        .unwrap_or(text.len())
}

fn next_grapheme_boundary(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    let floor = floor_grapheme_boundary(text, cursor);
    let ceil = ceil_grapheme_boundary(text, cursor);
    if floor < cursor && text[floor..ceil].contains('\u{200d}') {
        return ceil;
    }
    let char_cursor = floor_char_boundary(text, cursor);
    if let Some((offset, grapheme)) = text[char_cursor..].grapheme_indices(true).next() {
        let start = char_cursor + offset;
        let end = start + grapheme.len();
        if grapheme.contains('\u{200d}') && cursor < end {
            return end;
        }
    }
    text[char_cursor..]
        .char_indices()
        .map(|(offset, ch)| char_cursor + offset + ch.len_utf8())
        .find(|boundary| *boundary > cursor)
        .unwrap_or(text.len())
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '-')
}

fn word_bounds(text: &str, cursor: usize) -> Range<usize> {
    if text.is_empty() {
        return 0..0;
    }
    let cursor = floor_grapheme_boundary(text, cursor);
    let probe = if cursor < text.len() {
        text[cursor..].chars().next()
    } else {
        text[..cursor].chars().next_back()
    };
    let Some(probe) = probe else {
        return 0..0;
    };
    if !is_word_char(probe) {
        return cursor..next_grapheme_boundary(text, cursor);
    }
    let mut start = cursor;
    while start > 0 {
        let previous = previous_grapheme_boundary(text, start);
        let ch = text[previous..start].chars().next().unwrap_or(' ');
        if !is_word_char(ch) {
            break;
        }
        start = previous;
    }
    let mut end = cursor;
    while end < text.len() {
        let ch = text[end..].chars().next().unwrap_or(' ');
        if !is_word_char(ch) {
            break;
        }
        end = next_grapheme_boundary(text, end);
    }
    start..end
}

fn floor_char_boundary(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(text.len());
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

fn byte_to_utf16(text: &str, byte: usize) -> usize {
    text[..floor_char_boundary(text, byte)]
        .chars()
        .map(char::len_utf16)
        .sum()
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn utf16_to_byte(text: &str, utf16: usize) -> usize {
    let mut seen = 0;
    for (byte, ch) in text.char_indices() {
        if seen >= utf16 {
            return byte;
        }
        let next = seen + ch.len_utf16();
        if utf16 < next {
            return byte;
        }
        seen = next;
    }
    text.len()
}

fn utf16_slice(text: &str, range: Range<usize>) -> (String, Range<usize>) {
    let start = utf16_to_byte(text, range.start);
    let end = utf16_to_byte(text, range.end).max(start);
    let actual = byte_to_utf16(text, start)..byte_to_utf16(text, end);
    (text[start..end].to_owned(), actual)
}

fn composed_text(text: &str, composition: Option<&Composition>) -> String {
    let Some(composition) = composition else {
        return text.to_owned();
    };
    let mut result = text.to_owned();
    result.replace_range(composition.range.clone(), &composition.text);
    result
}

fn caret_blink_opacity(delta: f32) -> f32 {
    if (delta * 2.0).fract() < 0.55 {
        1.0
    } else {
        0.15
    }
}

fn scroll_offset_for_x(
    content_x: Pixels,
    content_width: Pixels,
    viewport_width: Pixels,
    current: Pixels,
) -> Pixels {
    if viewport_width <= px(0.0) {
        return current;
    }
    let min = -(content_width - viewport_width).max(px(0.0));
    let mut offset = current.clamp(min, px(0.0));
    let visible_start = -offset;
    let visible_end = visible_start + viewport_width;
    if content_x < visible_start {
        offset = (-content_x).clamp(min, px(0.0));
    } else if content_x + px(1.0) > visible_end {
        offset = (viewport_width - content_x - px(1.0)).clamp(min, px(0.0));
    }
    offset
}

impl Focusable for TextInput {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.display_text();
        let (value, actual) = utf16_slice(text, range_utf16.clone());
        if actual != range_utf16 {
            adjusted_range.replace(actual);
        }
        Some(value)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        if let Some(composition) = &self.composition
            && let Some(selected) = &composition.selected
        {
            let start = byte_to_utf16(&self.text, composition.range.start);
            return Some(UTF16Selection {
                range: start + byte_to_utf16(&composition.text, selected.start)
                    ..start + byte_to_utf16(&composition.text, selected.end),
                reversed: false,
            });
        }
        let anchor = self.selection_anchor.unwrap_or(self.cursor);
        Some(UTF16Selection {
            range: byte_to_utf16(&self.text, anchor.min(self.cursor))
                ..byte_to_utf16(&self.text, anchor.max(self.cursor)),
            reversed: self.cursor < anchor,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let composition = self.composition.as_ref()?;
        let start = byte_to_utf16(&self.text, composition.range.start);
        Some(start..start + utf16_len(&composition.text))
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.cancel_composition() {
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let composition = self.composition.take();
        self.reset_blink_phase();
        let range = composition
            .as_ref()
            .map(|composition| composition.range.clone())
            .or_else(|| {
                range_utf16.map(|range| {
                    let start = utf16_to_byte(&self.text, range.start);
                    let end = utf16_to_byte(&self.text, range.end);
                    start.min(end)..start.max(end)
                })
            })
            .unwrap_or_else(|| {
                self.selection_bounds()
                    .map_or(self.cursor..self.cursor, |(start, end)| start..end)
            });
        let mut range = floor_grapheme_boundary(&self.text, range.start)
            ..ceil_grapheme_boundary(&self.text, range.end);
        if range.start > range.end {
            range = self.cursor..self.cursor;
        }
        let replacement = self.limited_replacement(range.start, range.end, text);
        let before = composition.map(|composition| composition.before);
        // A committed preedit is its own undo step, and a plain caret insertion
        // joins the open typing run.
        let run = (before.is_none() && range.is_empty() && replacement.chars().count() == 1)
            .then_some(EditRun::Insert);
        if self.edit_range_with_run(range, replacement, before, run) {
            self.notify_changed(cx);
        } else {
            cx.notify();
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reset_blink_phase();
        let composition = self.composition.take();
        let before = composition
            .as_ref()
            .map(|composition| composition.before.clone())
            .unwrap_or_else(|| self.snapshot());
        let range = composition
            .as_ref()
            .map(|composition| composition.range.clone())
            .or_else(|| {
                range_utf16.map(|range| {
                    let start = utf16_to_byte(&self.text, range.start);
                    let end = utf16_to_byte(&self.text, range.end);
                    start.min(end)..start.max(end)
                })
            })
            .unwrap_or_else(|| {
                self.selection_bounds()
                    .map_or(self.cursor..self.cursor, |(start, end)| start..end)
            });
        let range = floor_grapheme_boundary(&self.text, range.start)
            ..ceil_grapheme_boundary(&self.text, range.end);
        let new_text = self.limited_replacement(range.start, range.end, new_text);
        if new_text.is_empty() {
            self.restore(before);
            cx.notify();
            return;
        }
        let selected = new_selected_range.map(|selected| {
            let start = selected.start.min(selected.end);
            let end = selected.start.max(selected.end);
            let start = floor_grapheme_boundary(&new_text, utf16_to_byte(&new_text, start));
            let end = ceil_grapheme_boundary(&new_text, utf16_to_byte(&new_text, end));
            start..end
        });
        let cursor = range.end;
        self.composition = Some(Composition {
            range,
            text: new_text,
            selected,
            before,
        });
        self.cursor = cursor;
        self.selection_anchor = None;
        self.invalidate_layout();
        self.scroll_to_caret = true;
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let display = self.display_text();
        let shape = self.shape.as_ref()?;
        let index = floor_grapheme_boundary(display, utf16_to_byte(display, range_utf16.end));
        let content_x = shape.x_for_display_index(index);
        let current = self.scroll.offset();
        let viewport = self.scroll.bounds();
        let next_x = scroll_offset_for_x(
            content_x,
            shape.content_width(),
            viewport.size.width - px(1.0),
            current.x,
        );
        if next_x != current.x {
            if let Some(bounds) = &mut self.last_bounds {
                bounds.origin.x += next_x - current.x;
            }
            self.scroll.set_offset(point(next_x, current.y));
            self.scroll_to_caret = false;
            window.refresh();
        }
        Some(Bounds::new(
            point(
                viewport.left() + next_x + content_x,
                viewport.top() + current.y,
            ),
            size(CARET_WIDTH, element_bounds.size.height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let display = self.display_text();
        Some(byte_to_utf16(display, self.display_index_for_point(point)))
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reset_blink_phase();
        let display = self.display_text();
        let mut start = utf16_to_byte(display, range_utf16.start);
        let mut end = utf16_to_byte(display, range_utf16.end);
        if start > end {
            std::mem::swap(&mut start, &mut end);
        }
        let start = floor_grapheme_boundary(display, start);
        let end = ceil_grapheme_boundary(display, end);
        if let Some(composition) = &mut self.composition {
            let marked_start = composition.range.start;
            let marked_end = marked_start + composition.text.len();
            if start >= marked_start && end <= marked_end {
                composition.selected = Some(start - marked_start..end - marked_start);
                self.scroll_to_caret = true;
                cx.notify();
                return;
            }
            self.cancel_composition();
        }
        let start = self.value_byte_for_display(start);
        let end = self.value_byte_for_display(end);
        self.cursor = end;
        self.selection_anchor = Some(start);
        break_history_run(&mut self.history);
        self.scroll_to_caret = true;
        cx.notify();
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(utf16_len(self.display_text()))
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

struct TextPointerElement {
    input: Entity<TextInput>,
    id: ElementId,
    style: gpui::StyleRefinement,
}

struct TextPointerPrepaint {
    hitbox: Hitbox,
}

impl Styled for TextPointerElement {
    fn style(&mut self) -> &mut gpui::StyleRefinement {
        &mut self.style
    }
}

impl IntoElement for TextPointerElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextPointerElement {
    type RequestLayoutState = gpui::Style;
    type PrepaintState = TextPointerPrepaint;

    /// An explicit id keeps the hitbox stable while the query element swaps
    /// between the placeholder and the shaped line, so a drag in progress never
    /// loses its pointer capture to a renamed hitbox.
    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        Some(core::panic::Location::caller())
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = gpui::Style::default();
        style.refine(&self.style);
        (window.request_layout(style.clone(), [], cx), style)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        TextPointerPrepaint {
            hitbox: window.insert_hitbox(bounds, HitboxBehavior::Normal),
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        let hitbox_id = prepaint.hitbox.id;
        let captured = hitbox_id;
        let down_input = self.input.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture
                && event.button == MouseButton::Left
                && hitbox_id.is_hovered(window)
            {
                down_input.update(cx, |input, cx| {
                    input.on_mouse_down(event, window, cx);
                    if input.dragging {
                        window.capture_pointer(captured);
                    }
                });
                cx.stop_propagation();
            }
        });
        let move_input = self.input.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture
                && hitbox_id.is_hovered(window)
                && move_input.read(cx).can_drag()
            {
                let started = move_input.update(cx, |input, cx| {
                    let was_dragging = input.dragging;
                    input.on_mouse_move(event, window, cx);
                    !was_dragging && input.dragging
                });
                if started {
                    window.capture_pointer(hitbox_id);
                }
                cx.stop_propagation();
            }
        });
        let up_input = self.input.clone();
        window.on_mouse_event(move |_event: &MouseUpEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture && hitbox_id.is_hovered(window) {
                let dragging = up_input.read(cx).dragging;
                if dragging {
                    up_input.update(cx, |input, _| input.on_mouse_up());
                    window.release_pointer();
                    cx.stop_propagation();
                }
            }
        });
    }
}

struct TextQueryElement {
    input: Entity<TextInput>,
    selection_foreground: Hsla,
    selection_background: Hsla,
    caret_color: Hsla,
    blink_opacity: f32,
}

struct TextQueryLayout {
    line: ShapedLine,
    caret_x: Pixels,
    focused: bool,
    composing: bool,
    blink_opacity: f32,
}

struct TextQueryPrepaint {
    line: ShapedLine,
    caret: Option<PaintQuad>,
}

impl TextQueryElement {
    /// Shapes `text` — a slice of the display value — with the selection and
    /// preedit decoration clipped to it.
    #[allow(clippy::too_many_arguments)]
    fn shape_window_line(
        &self,
        text: &str,
        start: usize,
        selection: Option<&Range<usize>>,
        marked: Option<&Range<usize>>,
        text_style: &TextStyle,
        window: &mut Window,
    ) -> ShapedLine {
        let clip = |range: &Range<usize>| {
            let from = range.start.clamp(start, start + text.len());
            let to = range.end.clamp(start, start + text.len());
            (from < to).then(|| from - start..to - start)
        };
        let selection = selection.and_then(clip);
        let marked = marked.and_then(clip);
        let base_run = text_style.to_run(text.len());
        let mut boundaries = vec![0, text.len()];
        boundaries.extend(selection.iter().flat_map(|range| [range.start, range.end]));
        boundaries.extend(marked.iter().flat_map(|range| [range.start, range.end]));
        boundaries.sort_unstable();
        boundaries.dedup();
        let runs = boundaries
            .windows(2)
            .filter_map(|bounds| {
                let start = bounds[0];
                let len = bounds[1] - start;
                if len == 0 {
                    return None;
                }
                let middle = start + len / 2;
                let mut run = TextRun {
                    len,
                    ..base_run.clone()
                };
                if selection
                    .as_ref()
                    .is_some_and(|range| range.contains(&middle))
                {
                    run.color = self.selection_foreground;
                    run.background_color = Some(self.selection_background);
                }
                if marked.as_ref().is_some_and(|range| range.contains(&middle)) {
                    run.underline = Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    });
                }
                Some(run)
            })
            .collect::<Vec<_>>();
        window.text_system().shape_line(
            SharedString::from(text),
            text_style.font_size.to_pixels(window.rem_size()),
            &runs,
            None,
        )
    }
}

impl IntoElement for TextQueryElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextQueryElement {
    type RequestLayoutState = TextQueryLayout;
    type PrepaintState = TextQueryPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        Some(core::panic::Location::caller())
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let input = self.input.read(cx);
        let selection = input.display_selection_bounds();
        let marked = input.composition.as_ref().map(|composition| {
            composition.range.start..composition.range.start + composition.text.len()
        });
        let cursor = input.display_cursor();
        let focused = input.focus_handle.is_focused(window);
        let composing = input.is_composing();
        let text_style = window.text_style();
        // Rebuild only when the value, the decorated ranges, or the text style
        // changed, or when the caret left the shaped window. A repaint that
        // only blinks or scrolls reuses the shaped line as is.
        let reusable = input.shape.as_ref().is_some_and(|shape| {
            shape.style == text_style
                && shape.selection == selection
                && shape.marked == marked
                && shape.contains(cursor)
        });
        let shape = if reusable {
            input.shape.clone().unwrap()
        } else {
            let (start, end) = shape_window_range(input.display_text(), cursor);
            let text = input.display_text()[start..end].to_owned();
            let line = self.shape_window_line(
                &text,
                start,
                selection.as_ref(),
                marked.as_ref(),
                &text_style,
                window,
            );
            let shape = ShapeWindow {
                text,
                start,
                end,
                line,
                style: text_style,
                selection,
                marked,
            };
            self.input
                .update(cx, |input, _| input.shape = Some(shape.clone()));
            shape
        };
        let cursor = cursor.max(shape.start).min(shape.end);
        let line = shape.line.clone();
        let mut style = gpui::Style::default();
        style.size.width = (line.width() + px(2.0)).max(px(1.0)).into();
        style.size.height = window.line_height().into();
        style.flex_shrink = 0.0;
        (
            window.request_layout(style, [], cx),
            TextQueryLayout {
                line,
                caret_x: shape.x_for_display_index(cursor),
                focused,
                composing,
                blink_opacity: self.blink_opacity,
            },
        )
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        let caret = (layout.focused && !layout.composing).then(|| {
            fill(
                Bounds::new(
                    point(bounds.left() + layout.caret_x, bounds.top()),
                    size(CARET_WIDTH, bounds.size.height),
                ),
                self.caret_color.opacity(layout.blink_opacity),
            )
        });
        TextQueryPrepaint {
            line: layout.line.clone(),
            caret,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        let line_height = window.line_height();
        let _ = prepaint.line.paint_background(
            bounds.origin,
            line_height,
            TextAlign::Left,
            Some(prepaint.line.width()),
            window,
            cx,
        );
        let _ = prepaint.line.paint(
            bounds.origin,
            line_height,
            TextAlign::Left,
            None,
            window,
            cx,
        );
        if let Some(caret) = prepaint.caret.take() {
            window.paint_quad(caret);
        }
        let content_x = request_layout.caret_x;
        let content_width = request_layout.line.width() + px(2.0);
        let previous_offset = self.input.read(cx).scroll.offset().x;
        let next_offset = self.input.update(cx, |input, _| {
            input.last_bounds = Some(bounds);
            let viewport = input.scroll.bounds();
            if viewport.size.width > px(0.0) {
                if input.scroll_to_caret || input.last_viewport_width != viewport.size.width {
                    let current = input.scroll.offset();
                    let next = scroll_offset_for_x(
                        content_x,
                        content_width,
                        viewport.size.width - px(1.0),
                        current.x,
                    );
                    if next != current.x {
                        if let Some(bounds) = &mut input.last_bounds {
                            bounds.origin.x += next - current.x;
                        }
                        input.scroll.set_offset(point(next, current.y));
                    }
                }
                input.last_viewport_width = viewport.size.width;
                input.scroll_to_caret = false;
            }
            input.scroll.offset().x
        });
        if next_offset != previous_offset {
            window.refresh();
        }
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let input_surface = design::surface::input(cx);
        let focused = self.focus_handle.contains_focused(window, cx);
        let caret_focused = self.focus_handle.is_focused(window);
        if self.last_focused != Some(caret_focused) {
            self.last_focused = Some(caret_focused);
            self.reset_blink_phase();
        }
        let input_id = self.id.clone();
        let display_empty = self.display.is_empty();
        if display_empty {
            self.shape = None;
            self.last_bounds = None;
            let offset = self.scroll.offset();
            self.scroll.set_offset(point(px(0.0), offset.y));
        }
        let empty_caret = (display_empty && caret_focused && !self.is_composing()).then(|| {
            div()
                .absolute()
                .left_0()
                .top_0()
                .h(design::text::BODY_LINE_HEIGHT)
                .w(CARET_WIDTH)
                .bg(colors.text_accent)
                .with_animation(
                    format!("{input_id}-empty-caret-blink-{}", self.blink_epoch),
                    Animation::new(design::motion::CARET).repeat(),
                    |caret, delta| caret.opacity(caret_blink_opacity(delta)),
                )
                .into_any_element()
        });
        let query_content: AnyElement = if display_empty {
            h_flex()
                .relative()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_color(design::text_on(
                    input_surface,
                    colors.text_placeholder,
                    colors.text_muted,
                ))
                .child(self.placeholder.clone())
                .children(empty_caret)
                .into_any_element()
        } else {
            let query = TextQueryElement {
                input: cx.entity(),
                selection_foreground: design::text_selection::foreground_on(cx, input_surface),
                selection_background: design::text_selection::background(cx),
                caret_color: colors.text_accent,
                blink_opacity: 1.0,
            };
            if caret_focused && !self.is_composing() {
                query
                    .with_animation(
                        format!("{input_id}-caret-blink-{}", self.blink_epoch),
                        Animation::new(design::motion::CARET).repeat(),
                        |mut query, delta| {
                            query.blink_opacity = caret_blink_opacity(delta);
                            query
                        },
                    )
                    .into_any_element()
            } else {
                query.into_any_element()
            }
        };
        let input_registration = display_empty.then(|| {
            let focus = self.focus_handle.clone();
            let input = cx.entity();
            canvas(
                |_, _, _| {},
                move |bounds, _, window, cx| {
                    window.handle_input(
                        &focus,
                        ElementInputHandler::new(bounds, input.clone()),
                        cx,
                    );
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .into_any_element()
        });
        let query: AnyElement = h_flex()
            .id(format!("{input_id}-query"))
            .relative()
            .flex_1()
            .min_w(px(0.0))
            .flex_shrink_1()
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&self.scroll)
            .items_center()
            .debug_selector(|| "shared-text-input-query".to_owned())
            .child(query_content)
            .children(input_registration)
            .child(
                TextPointerElement {
                    input: cx.entity(),
                    id: format!("{input_id}-text-pointer").into(),
                    style: Default::default(),
                }
                .absolute()
                .inset_0()
                .size_full()
                .into_any_element(),
            )
            .into_any_element();
        let clear = (!self.text.is_empty()).then(|| {
            let clear_label = self.clear_label.clone();
            let tooltip = if self.clear_escape_hint {
                format!("{clear_label} (Esc)")
            } else {
                clear_label.to_string()
            };
            div()
                .size(CLEAR_BUTTON_SIZE)
                .flex_none()
                .debug_selector(|| "shared-text-input-clear".to_owned())
                .on_key_down(cx.listener(Self::handle_clear_key))
                .child(
                    ButtonLike::new(format!("{input_id}-clear"))
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::None)
                        .width(CLEAR_BUTTON_SIZE)
                        .height(CLEAR_BUTTON_SIZE.into())
                        .tab_index(1isize)
                        .track_focus(&self.clear_focus)
                        .aria_label(clear_label)
                        .tooltip(Tooltip::text(tooltip))
                        .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                            this.clear(cx);
                            window.focus(&this.focus_handle, cx);
                        }))
                        .child(Icon::new(IconName::Close).size(IconSize::XSmall)),
                )
                .into_any_element()
        });

        h_flex()
            .id(input_id.clone())
            .accessibility_id(input_id)
            .debug_selector(|| "shared-text-input".to_owned())
            .w(self.width)
            .min_w(px(0.0))
            .flex_shrink_1()
            .h(design::size::CONTROL)
            .font_ui(cx)
            .text_size(rems_from_px(f32::from(design::text::BODY)))
            .line_height(rems_from_px(f32::from(design::text::BODY_LINE_HEIGHT)))
            .px(INPUT_PADDING)
            .gap(design::space::XS)
            .items_center()
            .rounded_md()
            .border_1()
            .border_color(if self.invalid {
                design::Severity::Error.marker_on(cx, input_surface)
            } else if focused {
                design::graphic_on(input_surface, colors.border_focused)
            } else {
                colors.border_variant
            })
            .bg(input_surface.alpha(1.0))
            .track_focus(&self.focus_handle)
            .key_context("TextInput")
            .on_action(cx.listener(Self::undo_action))
            .on_action(cx.listener(Self::redo_action))
            .on_action(cx.listener(Self::cut_action))
            .on_action(cx.listener(Self::copy_action))
            .on_action(cx.listener(Self::paste_action))
            .on_action(cx.listener(Self::select_all_action))
            .on_key_down(cx.listener(Self::handle_key))
            .role(self.role)
            .aria_label(self.aria_label.clone())
            .aria_description(self.aria_description.clone())
            .when(self.invalid, |this| {
                this.aria_description(format!(
                    "{} Check the value and try again.",
                    self.aria_description
                ))
            })
            .aria_placeholder(self.placeholder.clone())
            .aria_value(self.text.clone())
            .cursor_text()
            .when(self.leading_icon, |this| {
                this.child(
                    Icon::new(IconName::MagnifyingGlass)
                        .size(IconSize::XSmall)
                        .color(Color::Custom(design::graphic_on(
                            input_surface,
                            colors.text_muted,
                        ))),
                )
            })
            .child(query)
            .children(self.invalid.then(|| {
                Icon::new(IconName::Warning)
                    .size(IconSize::XSmall)
                    .color(Color::Custom(
                        design::Severity::Error.marker_on(cx, input_surface),
                    ))
            }))
            .children(clear)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{
        ClipboardItem, EntityInputHandler as _, Hsla, Modifiers, MouseButton, MouseDownEvent,
        MouseUpEvent, TestAppContext, VisualTestContext, point, px,
    };
    use k8s_actions::{Copy, Cut, Paste, Redo, SelectAll, Undo};
    use theme::LoadThemes;
    use ui::ActiveTheme as _;
    use unicode_segmentation::UnicodeSegmentation as _;

    use crate::design;

    use super::{
        CARET_WIDTH, CLEAR_BUTTON_SIZE, EditHistory, EditRun, HISTORY_LIMIT, HISTORY_MAX_BYTES,
        HistorySnapshot, INPUT_PADDING, MAX_TEXT_BYTES, MAX_TEXT_CHARS, SHAPE_WINDOW_GRAPHEMES,
        TextInput, break_history_run, caret_blink_opacity, ceil_grapheme_boundary,
        floor_grapheme_boundary, next_grapheme_boundary, normalize_text, pop_history,
        previous_grapheme_boundary, push_history, record_history, shape_window_range,
        truncate_to_budget, utf16_to_byte, word_bounds,
    };

    fn has_painted_caret(cx: &mut VisualTestContext, accent: Hsla) -> bool {
        cx.update(|window, _| {
            let scale = window.scale_factor();
            let caret_width = f32::from(CARET_WIDTH);
            let caret_height = f32::from(design::text::BODY_LINE_HEIGHT);
            window.painted_quads().into_iter().any(|quad| {
                let width = quad.bounds.size.width.as_f32() / scale;
                let height = quad.bounds.size.height.as_f32() / scale;
                let Some(color) = quad.background.as_solid() else {
                    return false;
                };
                (width - caret_width).abs() <= 0.01
                    && (height - caret_height).abs() <= 0.01
                    && color.a > 0.0
                    && color.h == accent.h
                    && color.s == accent.s
                    && color.l == accent.l
            })
        })
    }

    fn has_painted_color(cx: &mut VisualTestContext, color: Hsla) -> bool {
        cx.update(|window, _| {
            window
                .painted_quads()
                .into_iter()
                .any(|quad| quad.background.as_solid() == Some(color))
        })
    }

    #[test]
    fn cursor_boundaries_handle_cjk_and_graphemes() {
        let text = "a你e\u{301}👩‍💻🇺🇳🙂b";
        for boundary in [0, 1, 4, 5, 7, 18, 22, 26, 30, 31].windows(2) {
            assert_eq!(next_grapheme_boundary(text, boundary[0]), boundary[1]);
            assert_eq!(previous_grapheme_boundary(text, boundary[1]), boundary[0]);
        }
        assert_eq!(floor_grapheme_boundary(text, 8), 7);
        assert_eq!(ceil_grapheme_boundary(text, 8), 18);
        assert_eq!(previous_grapheme_boundary(text, 8), 7);
        assert_eq!(next_grapheme_boundary(text, 8), 18);
        assert_eq!(floor_grapheme_boundary(text, usize::MAX), text.len());
        assert_eq!(ceil_grapheme_boundary(text, usize::MAX), text.len());
    }

    #[gpui::test]
    fn focused_empty_and_nonempty_inputs_draw_accent_carets(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}));
        cx.run_until_parked();
        let accent = cx.update(|_, cx| cx.theme().colors().text_accent);
        let query = cx
            .debug_bounds("shared-text-input-query")
            .expect("query must be laid out");
        cx.simulate_click(query.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            input.read_with(cx, |input, _| input.last_focused),
            Some(true)
        );
        assert!(has_painted_caret(cx, accent));

        input.update(cx, |input, cx| input.set_text("abc", cx));
        cx.run_until_parked();
        let query = cx
            .debug_bounds("shared-text-input-query")
            .expect("query must be laid out");
        cx.simulate_click(query.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            input.read_with(cx, |input, _| input.last_focused),
            Some(true)
        );
        assert!(has_painted_caret(cx, accent));
    }

    #[gpui::test]
    fn focus_edit_selection_and_clear_reset_the_blink_phase(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (input, cx) =
            cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}).with_text("abcd"));
        cx.run_until_parked();
        let query = cx
            .debug_bounds("shared-text-input-query")
            .expect("query must be laid out");
        cx.simulate_click(query.center(), Modifiers::none());
        cx.run_until_parked();
        let mut phase = input.read_with(cx, |input, _| input.blink_epoch);

        cx.simulate_click(query.center(), Modifiers::none());
        cx.run_until_parked();
        let next = input.read_with(cx, |input, _| input.blink_epoch);
        assert!(next > phase);
        phase = next;

        cx.simulate_input("x");
        let next = input.read_with(cx, |input, _| input.blink_epoch);
        assert!(next > phase);
        phase = next;

        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.set_selected_text_range(0..1, window, cx)
            });
        });
        cx.run_until_parked();
        let next = input.read_with(cx, |input, _| input.blink_epoch);
        assert!(next > phase);
        phase = next;

        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_text_in_range(None, "z", window, cx)
            });
        });
        cx.run_until_parked();
        let next = input.read_with(cx, |input, _| input.blink_epoch);
        assert!(next > phase);
        phase = next;

        cx.update(|_, cx| input.update(cx, |input, cx| input.clear(cx)));
        cx.run_until_parked();
        let next = input.read_with(cx, |input, _| input.blink_epoch);
        assert!(next > phase);
        assert_eq!(input.read_with(cx, |input, _| input.text().to_owned()), "");
        let accent = cx.update(|_, cx| cx.theme().colors().text_accent);
        assert!(has_painted_caret(cx, accent));

        let focus = input.read_with(cx, |input, _| input.focus_handle.clone());
        cx.update(|window, cx| window.blur(cx));
        cx.run_until_parked();
        assert!(!has_painted_caret(cx, accent));
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.run_until_parked();
        assert!(input.read_with(cx, |input, _| input.blink_epoch) > next);
    }

    #[test]
    fn caret_indices_are_utf16_and_grapheme_safe() {
        assert_eq!(caret_blink_opacity(0.0), 1.0);
        assert_eq!(caret_blink_opacity(0.8), 0.15);
        let text = "a🙂b";
        assert_eq!(utf16_to_byte(text, 0), 0);
        assert_eq!(utf16_to_byte(text, 1), 1);
        assert_eq!(utf16_to_byte(text, 2), 1);
        assert_eq!(utf16_to_byte(text, 3), 5);
        assert_eq!(floor_grapheme_boundary("a👩‍💻b", 6), 1);
        assert_eq!(ceil_grapheme_boundary("a👩‍💻b", 6), 12);
    }

    #[gpui::test]
    fn selection_keeps_caret_and_range(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (input, cx) =
            cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}).with_text("abcd"));
        cx.run_until_parked();
        let query = cx
            .debug_bounds("shared-text-input-query")
            .expect("query must be laid out");
        cx.simulate_click(query.center(), Modifiers::none());
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.set_selected_text_range(1..3, window, cx)
            });
        });
        cx.run_until_parked();
        assert_eq!(
            input.read_with(cx, |input, _| input.selection_bounds()),
            Some((1, 3))
        );
        let (input_surface, selection_background, selection_foreground, text_accent) =
            cx.update(|_, cx| {
                let input_surface = design::surface::input(cx);
                (
                    input_surface,
                    design::text_selection::background(cx),
                    design::text_selection::foreground_on(cx, input_surface),
                    cx.theme().colors().text_accent,
                )
            });
        assert!(has_painted_color(cx, selection_background));
        assert!(
            ui::utils::calculate_contrast_ratio(
                selection_foreground,
                design::composite_surface(input_surface, selection_background),
            ) >= design::TEXT_MIN_CONTRAST
        );
        assert!(has_painted_caret(cx, text_accent));
    }

    #[gpui::test]
    fn shift_movement_preserves_and_reverses_the_selection_anchor(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (input, cx) =
            cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}).with_text("abcdef"));
        cx.simulate_click(point(px(40.0), px(14.0)), Modifiers::none());
        cx.simulate_keystrokes("home shift-right shift-right");
        assert_eq!(
            input.read_with(cx, |input, _| (input.selection_anchor, input.cursor)),
            (Some(0), 2)
        );
        cx.simulate_keystrokes("left shift-right shift-end shift-home shift-end");
        assert_eq!(
            input.read_with(cx, |input, _| (input.selection_anchor, input.cursor)),
            (Some(0), 6)
        );
        cx.simulate_keystrokes("home end shift-left shift-left shift-right shift-right");
        assert_eq!(
            input.read_with(cx, |input, _| (
                input.selection_anchor,
                input.cursor,
                input.selection_bounds()
            )),
            (Some(6), 6, None)
        );
        cx.simulate_keystrokes("ctrl-left");
        assert_eq!(
            input.read_with(cx, |input, _| (input.selection_anchor, input.cursor)),
            (Some(6), 6)
        );
    }

    #[gpui::test]
    fn mouse_click_drag_and_shift_click_use_shaped_boundaries(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (input, cx) =
            cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}).with_text("a你🙂"));
        cx.run_until_parked();
        let (bounds, start_x, end_x) = input.read_with(cx, |input, _| {
            let shape = input.shape.as_ref().unwrap();
            (
                input.last_bounds.unwrap(),
                shape.x_for_display_index(1),
                shape.x_for_display_index(8),
            )
        });
        let y = bounds.origin.y + bounds.size.height / 2.0;
        let start = point(bounds.left() + start_x, y);
        let end = point(bounds.left() + end_x, y);
        cx.simulate_mouse_move(start, None, Modifiers::none());
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        assert!(cx.update(|window, _| window.captured_hitbox().is_some()));
        let outside = point(bounds.right() + px(40.0), y);
        cx.simulate_mouse_move(outside, Some(MouseButton::Left), Modifiers::none());
        cx.simulate_mouse_up(outside, MouseButton::Left, Modifiers::none());
        assert!(cx.update(|window, _| window.captured_hitbox().is_none()));
        assert_eq!(
            input.read_with(cx, |input, _| (input.selection_anchor, input.cursor)),
            (Some(1), 8)
        );
        cx.simulate_click(end, Modifiers::none());
        cx.simulate_click(start, Modifiers::shift());
        assert_eq!(
            input.read_with(cx, |input, _| (
                input.selection_anchor,
                input.cursor,
                input.selection_bounds()
            )),
            (Some(8), 1, Some((1, 8)))
        );

        let (grapheme_input, cx) =
            cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}).with_text("a👩‍💻b"));
        cx.run_until_parked();
        let point = grapheme_input.read_with(cx, |input, _| {
            let bounds = input.last_bounds.unwrap();
            let shape = input.shape.as_ref().unwrap();
            let start = shape.x_for_display_index(1);
            let end = shape.x_for_display_index(12);
            point(bounds.left() + (start + end) / 2.0, bounds.center().y)
        });
        let index = grapheme_input.read_with(cx, |input, _| input.index_for_point(point));
        assert!(
            index == 1 || index == 12,
            "byte index inside emoji cluster: {index}"
        );
    }

    #[gpui::test]
    fn shaped_hit_test_snaps_to_ascii_cjk_and_emoji_boundaries(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        for text in ["abcd", "你好", "🙂🙂", "a你🙂b", "a👩‍💻b"] {
            let (input, cx) =
                cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}).with_text(text));
            cx.run_until_parked();
            let boundaries = text
                .grapheme_indices(true)
                .map(|(start, grapheme)| start + grapheme.len())
                .collect::<Vec<_>>();
            assert_eq!(boundaries.last().copied(), Some(text.len()), "{text:?}");
            for boundary in &boundaries {
                let boundary = *boundary;
                let at = input.read_with(cx, |input, _| {
                    let bounds = input.last_bounds.unwrap();
                    let shape = input.shape.as_ref().unwrap();
                    point(
                        bounds.left() + shape.x_for_display_index(boundary),
                        bounds.center().y,
                    )
                });
                assert_eq!(
                    input.read_with(cx, |input, _| input.index_for_point(at)),
                    boundary,
                    "boundary {boundary} of {text:?}"
                );
            }
            let (before, after) = input.read_with(cx, |input, _| {
                let bounds = input.last_bounds.unwrap();
                let shape = input.shape.as_ref().unwrap();
                (
                    point(bounds.left() - px(8.0), bounds.center().y),
                    point(
                        bounds.left() + shape.line.width() + px(8.0),
                        bounds.center().y,
                    ),
                )
            });
            assert_eq!(
                input.read_with(cx, |input, _| input.index_for_point(before)),
                0,
                "left of {text:?}"
            );
            assert_eq!(
                input.read_with(cx, |input, _| input.index_for_point(after)),
                text.len(),
                "right of {text:?}"
            );
        }
    }

    #[gpui::test]
    fn ime_preedit_uses_utf16_and_only_commit_changes_the_value(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let changes = Rc::new(RefCell::new(Vec::new()));
        let recorded = changes.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("Search", cx, move |text, _| {
                recorded.borrow_mut().push(text.to_owned());
            })
            .with_text("a🙂b")
        });
        cx.simulate_click(point(px(40.0), px(14.0)), Modifiers::none());
        let (text, selected, length) = cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.set_selected_text_range(1..3, window, cx);
                let mut adjusted = None;
                let text = input.text_for_range(0..4, &mut adjusted, window, cx);
                let selected = input
                    .selected_text_range(false, window, cx)
                    .map(|selection| (selection.range, selection.reversed));
                (text, selected, input.text_length_utf16(window, cx))
            })
        });
        assert_eq!(text.as_deref(), Some("a🙂b"));
        assert_eq!(selected, Some((1..3, false)));
        assert_eq!(length, Some(4));

        let (value, preedit, marked, selected, length) = cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "你", Some(0..1), window, cx);
                let mut adjusted = None;
                (
                    input.text().to_owned(),
                    input.text_for_range(1..2, &mut adjusted, window, cx),
                    input.marked_text_range(window, cx),
                    input
                        .selected_text_range(false, window, cx)
                        .map(|selection| (selection.range, selection.reversed)),
                    input.text_length_utf16(window, cx),
                )
            })
        });
        assert_eq!(value, "a🙂b");
        assert_eq!(preedit.as_deref(), Some("你"));
        assert_eq!(marked, Some(1..2));
        assert_eq!(selected, Some((1..2, false)));
        assert_eq!(length, Some(3));
        assert!(changes.borrow().is_empty());

        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_text_in_range(None, "中", window, cx);
            })
        });
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "a中b"
        );
        assert_eq!(changes.borrow().as_slice(), ["a中b"]);

        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "zhong", None, window, cx);
                input.unmark_text(window, cx);
            })
        });
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "a中b"
        );
        assert_eq!(changes.borrow().as_slice(), ["a中b"]);
        cx.dispatch_action(Undo);
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "a🙂b"
        );
    }

    #[test]
    fn text_and_history_limits_bound_untrusted_input() {
        let text = normalize_text(&"🙂".repeat(MAX_TEXT_CHARS + 128), false, None);
        assert_eq!(text.chars().count(), MAX_TEXT_CHARS);
        assert!(text.len() <= MAX_TEXT_BYTES);
        assert_eq!(normalize_text("abcdef", false, Some(3)), "abc");

        let mut char_limited = "a你🙂".to_owned();
        truncate_to_budget(&mut char_limited, 2, usize::MAX);
        assert_eq!(char_limited, "a你");
        let mut byte_limited = "a你🙂".to_owned();
        truncate_to_budget(&mut byte_limited, 3, 5);
        assert_eq!(byte_limited, "a你");

        let mut history = EditHistory::default();
        for _ in 0..(HISTORY_LIMIT + 10) {
            push_history(
                &mut history,
                HistorySnapshot {
                    text: "a".repeat(MAX_TEXT_BYTES),
                    cursor: 0,
                    selection_anchor: None,
                },
                false,
            );
        }
        assert!(history.undo.len() <= HISTORY_LIMIT);
        assert!(history.undo_bytes + history.redo_bytes <= HISTORY_MAX_BYTES);
    }

    #[test]
    fn word_selection_stays_on_grapheme_and_identifier_boundaries() {
        let text = "one two-three";
        assert_eq!(word_bounds(text, 5), 4..13);
        assert_eq!(word_bounds(text, 8), 4..13);
        assert_eq!(word_bounds("a👩‍💻b", 2), 1..12);
    }

    #[gpui::test]
    fn escape_cancels_composition_without_clearing_text(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let changes = Rc::new(RefCell::new(Vec::new()));
        let recorded = changes.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("Search", cx, move |text, _| {
                recorded.borrow_mut().push(text.to_owned());
            })
            .with_text("abc")
        });
        cx.simulate_click(point(px(20.0), px(14.0)), Modifiers::none());
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "你", None, window, cx)
            });
        });
        assert!(input.read_with(cx, |input, _| input.is_composing()));
        cx.simulate_keystrokes("escape");
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "abc"
        );
        assert!(!input.read_with(cx, |input, _| input.is_composing()));
        assert!(changes.borrow().is_empty());
    }

    #[gpui::test]
    fn long_value_scrolls_to_keep_the_caret_and_candidate_in_view(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let value = "long-query-".repeat(12);
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("Search", cx, |_, _| {})
                .with_width(px(180.0))
                .with_text(value.clone())
        });
        cx.run_until_parked();
        let query = cx
            .debug_bounds("shared-text-input-query")
            .expect("query must be laid out");
        cx.simulate_click(
            point(query.left() + px(1.0), query.center().y),
            Modifiers::none(),
        );
        cx.simulate_keystrokes("end");
        cx.run_until_parked();
        let (offset, max_offset, viewport, bounds, caret_x, cursor) =
            input.read_with(cx, |input, _| {
                let shape = input.shape.as_ref().unwrap();
                (
                    input.scroll.offset().x,
                    input.scroll.max_offset().x,
                    input.scroll.bounds(),
                    input.last_bounds.unwrap(),
                    shape.x_for_display_index(input.cursor),
                    input.cursor,
                )
            });
        assert!(max_offset > px(0.0));
        assert!(offset < px(0.0));
        assert_eq!(cursor, value.len());
        let caret = bounds.left() + caret_x;
        assert!(
            caret >= viewport.left(),
            "caret left: caret={caret}, bounds={bounds:?}, viewport={viewport:?}, offset={offset}",
        );
        assert!(
            caret + px(1.0) <= viewport.right(),
            "caret right: caret={caret}, bounds={bounds:?}, viewport={viewport:?}, offset={offset}",
        );
        let candidate = cx
            .update(|window, cx| {
                input.update(cx, |input, cx| {
                    input.bounds_for_range(cursor..cursor, input.last_bounds.unwrap(), window, cx)
                })
            })
            .unwrap();
        assert!(candidate.left() >= viewport.left());
        assert!(candidate.right() <= viewport.right());
        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        assert_eq!(
            input.read_with(cx, |input, _| input.scroll.offset().x),
            px(0.0)
        );
    }

    #[gpui::test]
    fn query_geometry_does_not_overlap_clear_at_compact_and_wide_widths(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        for width in [280.0, 560.0] {
            let (_input, cx) = cx.add_window_view(|_, cx| {
                TextInput::new(
                    "Search resource names with a deliberately long placeholder",
                    cx,
                    |_, _| {},
                )
                .with_width(px(width))
                .with_text("a query long enough to require truncation at compact width")
            });
            cx.run_until_parked();

            let input = cx
                .debug_bounds("shared-text-input")
                .expect("shared text input must be laid out");
            let query = cx
                .debug_bounds("shared-text-input-query")
                .expect("query must be laid out");
            let clear = cx
                .debug_bounds("shared-text-input-clear")
                .expect("clear button must be laid out");

            assert_eq!(clear.size.width, CLEAR_BUTTON_SIZE, "width={width}");
            assert_eq!(clear.size.height, CLEAR_BUTTON_SIZE, "width={width}");
            assert_eq!(
                clear.right(),
                input.right() - INPUT_PADDING - design::border::LINE,
                "clear right edge at width={width}"
            );
            assert!(
                query.right() + design::space::XS <= clear.left(),
                "query overlaps clear at width={width}"
            );
        }
    }

    #[gpui::test]
    fn selection_replaces_visible_text(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}));
        cx.simulate_click(point(px(20.0), px(14.0)), Modifiers::none());
        cx.simulate_input("abc");
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("x");
        assert_eq!(input.read_with(cx, |input, _| input.text().to_owned()), "x");
    }

    #[gpui::test]
    fn native_edit_actions_drive_text_input_history(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let changes = Rc::new(RefCell::new(Vec::new()));
        let recorded = changes.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("Search", cx, move |text, _| {
                recorded.borrow_mut().push(text.to_owned());
            })
        });
        cx.simulate_click(point(px(20.0), px(14.0)), Modifiers::none());
        cx.simulate_input("abc");

        cx.dispatch_action(SelectAll);
        cx.dispatch_action(Copy);
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("abc")
        );
        cx.write_to_clipboard(ClipboardItem::new_string("replacement".to_owned()));
        cx.dispatch_action(Paste);
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "replacement"
        );

        cx.dispatch_action(Undo);
        assert_eq!(
            input.read_with(cx, |input, _| (
                input.text().to_owned(),
                input.selection_bounds()
            )),
            ("abc".to_owned(), Some((0, 3)))
        );
        cx.dispatch_action(Redo);
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "replacement"
        );

        cx.dispatch_action(SelectAll);
        cx.dispatch_action(Cut);
        assert_eq!(input.read_with(cx, |input, _| input.text().to_owned()), "");
        cx.dispatch_action(Undo);
        assert_eq!(
            input.read_with(cx, |input, _| (
                input.text().to_owned(),
                input.selection_bounds()
            )),
            ("replacement".to_owned(), Some((0, 11)))
        );
        cx.dispatch_action(Redo);
        assert_eq!(input.read_with(cx, |input, _| input.text().to_owned()), "");
        assert_eq!(
            changes.borrow().last().map(String::as_str),
            Some(""),
            "redo must notify on_change"
        );
    }

    #[gpui::test]
    fn text_input_accepts_letters_and_respects_max_length(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (input, cx) = cx
            .add_window_view(|_, cx| TextInput::new("Bank name", cx, |_, _| {}).with_max_length(4));
        cx.simulate_click(point(px(20.0), px(14.0)), Modifiers::none());
        cx.simulate_input("ProdX");
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "Prod"
        );
    }

    #[gpui::test]
    fn programmatic_values_use_the_same_input_policy(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let changes = Rc::new(RefCell::new(Vec::new()));
        let recorded = changes.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("Number", cx, move |text, _| {
                recorded.borrow_mut().push(text.to_owned());
            })
            .with_text("12x\n🙂3")
            .with_numeric_input(3)
        });
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "123"
        );

        input.update(cx, |input, cx| input.set_text("9a\n4", cx));
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "94"
        );
        assert_eq!(changes.borrow().as_slice(), ["94"]);

        let (line_input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("Name", cx, |_, _| {}).with_text("a\nb\u{2028}c")
        });
        assert_eq!(
            line_input.read_with(cx, |input, _| input.text().to_owned()),
            "a b c"
        );
    }

    #[gpui::test]
    fn numeric_input_keeps_caret_and_filters_paste(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("Number", cx, |_, _| {})
                .with_text("12")
                .with_numeric_input(3)
        });
        cx.simulate_click(point(px(20.0), px(14.0)), Modifiers::none());
        cx.write_to_clipboard(ClipboardItem::new_string("9x\n08".to_owned()));
        cx.dispatch_action(SelectAll);
        cx.dispatch_action(Paste);
        cx.simulate_keystrokes("left backspace");
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "98"
        );
    }

    #[gpui::test]
    fn command_home_and_end_are_left_for_the_host(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (input, cx) =
            cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}).with_text("abc"));
        cx.simulate_click(point(px(20.0), px(14.0)), Modifiers::none());
        cx.simulate_keystrokes("home");
        assert_eq!(
            input.read_with(cx, |input, _| (input.cursor, input.selection_anchor)),
            (0, None)
        );
        cx.simulate_keystrokes("end");
        assert_eq!(input.read_with(cx, |input, _| input.cursor), 3);
        // A result list owns the command chords for result navigation, so the
        // shared input must not move the caret for them.
        cx.simulate_keystrokes("ctrl-home ctrl-end");
        assert_eq!(input.read_with(cx, |input, _| input.cursor), 3);
    }

    #[gpui::test]
    fn escape_cancels_the_preedit_first_and_leaves_an_empty_field_to_the_host(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let changes = Rc::new(RefCell::new(Vec::new()));
        let recorded = changes.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("Search", cx, move |text, _| {
                recorded.borrow_mut().push(text.to_owned());
            })
            .with_text("abc")
            .without_escape_hint()
        });
        cx.simulate_click(point(px(20.0), px(14.0)), Modifiers::none());
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "你", None, window, cx)
            });
        });
        cx.simulate_keystrokes("escape");
        assert!(!input.read_with(cx, |input, _| input.is_composing()));
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "abc"
        );
        assert!(changes.borrow().is_empty());

        // Without an escape hint the input never owns Escape, so a host that
        // clears or dismisses still sees it.
        let mut propagated = false;
        input.update(cx, |input, _| {
            input.history.run = None;
            propagated = input.text.is_empty();
        });
        assert!(!propagated);
        input.update(cx, |input, cx| input.clear(cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        assert_eq!(input.read_with(cx, |input, _| input.text().to_owned()), "");
    }

    #[gpui::test]
    fn escape_still_clears_a_field_that_advertises_the_hint(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (input, cx) =
            cx.add_window_view(|_, cx| TextInput::new("Filter", cx, |_, _| {}).with_text("abc"));
        cx.simulate_click(point(px(20.0), px(14.0)), Modifiers::none());
        cx.simulate_keystrokes("escape");
        assert_eq!(input.read_with(cx, |input, _| input.text().to_owned()), "");
        cx.dispatch_action(Undo);
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "abc"
        );
    }

    #[gpui::test]
    fn the_clear_button_is_a_tab_stop_that_activates(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (input, cx) =
            cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}).with_text("abc"));
        cx.run_until_parked();
        let query = cx
            .debug_bounds("shared-text-input-query")
            .expect("query must be laid out");
        cx.simulate_click(query.center(), Modifiers::none());
        cx.run_until_parked();
        cx.simulate_keystrokes("tab");
        cx.run_until_parked();
        assert!(
            cx.update(|window, cx| input
                .read_with(cx, |input, _| input.clear_focus_handle().is_focused(window))),
            "Tab from the field must reach the clear button"
        );
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(input.read_with(cx, |input, _| input.text().to_owned()), "");
        assert!(cx.update(|window, cx| {
            input.read_with(cx, |input, _| input.focus_handle.is_focused(window))
        }));
    }

    #[gpui::test]
    fn multi_click_selects_a_word_and_survives_pointer_jitter(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (input, cx) =
            cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}).with_text("ab cd"));
        cx.run_until_parked();
        // The drag target has to come from the shaped line, not from a pixel
        // literal: a hardcoded offset lands in a different glyph whenever the
        // font metrics change, so the asserted caret index would stop being
        // the one the pointer is actually over.
        let (word, drag) = input.read_with(cx, |input, _| {
            let bounds = input.last_bounds.unwrap();
            let shape = input.shape.as_ref().unwrap();
            let y = bounds.center().y;
            (
                point(bounds.left() + shape.x_for_display_index(4), y),
                point(bounds.left() + shape.x_for_display_index(1), y),
            )
        });
        cx.simulate_event(MouseDownEvent {
            position: word,
            modifiers: Modifiers::none(),
            button: MouseButton::Left,
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            position: word,
            modifiers: Modifiers::none(),
            button: MouseButton::Left,
            click_count: 2,
        });
        cx.run_until_parked();
        assert_eq!(
            input.read_with(cx, |input, _| input.selection_bounds()),
            Some((3, 5))
        );
        // The jitter of the click that made the selection must not drop it.
        cx.simulate_mouse_move(
            point(word.x + px(1.0), word.y),
            Some(MouseButton::Left),
            Modifiers::none(),
        );
        assert_eq!(
            input.read_with(cx, |input, _| input.selection_bounds()),
            Some((3, 5))
        );
        // Real travel turns it into a drag from the same anchor.
        cx.simulate_mouse_move(drag, Some(MouseButton::Left), Modifiers::none());
        assert_eq!(
            input.read_with(cx, |input, _| (input.selection_anchor, input.cursor)),
            (Some(3), 1)
        );
        cx.simulate_mouse_up(drag, MouseButton::Left, Modifiers::none());
        assert!(cx.update(|window, _| window.captured_hitbox().is_none()));
    }

    #[gpui::test]
    fn probe_geometry_for_investigation(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (input, cx) =
            cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}).with_text("ab cd"));
        cx.run_until_parked();
        let probe = input.read_with(cx, |input, _| {
            let bounds = input.last_bounds.unwrap();
            let shape = input.shape.as_ref().unwrap();
            let xs: Vec<f32> = (0..=5)
                .map(|index| f32::from(shape.x_for_display_index(index)))
                .collect();
            let hits: Vec<usize> = xs
                .iter()
                .map(|x| input.index_for_point(point(bounds.left() + px(*x), bounds.center().y)))
                .collect();
            (
                (f32::from(bounds.left()), f32::from(bounds.size.width)),
                xs,
                hits,
                f32::from(shape.line.width()),
            )
        });
        println!("bounds left/width={:?}", probe.0);
        println!("xs={:?}", probe.1);
        println!("hits={:?}", probe.2);
        println!("line width={:?}", probe.3);
    }

    #[gpui::test]
    fn typing_and_backspacing_coalesce_into_one_undo_step(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}));
        cx.simulate_click(point(px(20.0), px(14.0)), Modifiers::none());
        cx.simulate_input("abcd");
        assert_eq!(
            input.read_with(cx, |input, _| input.history.undo.len()),
            1,
            "a run of typing is one snapshot, not one per character"
        );
        cx.simulate_keystrokes("backspace backspace");
        cx.dispatch_action(Undo);
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "abcd"
        );
        cx.dispatch_action(Undo);
        assert_eq!(input.read_with(cx, |input, _| input.text().to_owned()), "");
        cx.dispatch_action(Redo);
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "abcd"
        );
    }

    #[test]
    fn caret_movement_and_pastes_break_the_undo_run() {
        let mut history = EditHistory::default();
        let snapshot = |text: &str| HistorySnapshot {
            text: text.to_owned(),
            cursor: text.len(),
            selection_anchor: None,
        };
        record_history(&mut history, &snapshot(""), Some(EditRun::Insert));
        record_history(&mut history, &snapshot("a"), Some(EditRun::Insert));
        assert_eq!(history.undo.len(), 1);
        record_history(&mut history, &snapshot("ab"), None);
        assert_eq!(history.undo.len(), 2, "a paste is its own step");
        record_history(&mut history, &snapshot("abc"), Some(EditRun::Insert));
        assert_eq!(history.undo.len(), 3, "a new run after a paste");
        break_history_run(&mut history);
        record_history(&mut history, &snapshot("abcd"), Some(EditRun::Insert));
        assert_eq!(history.undo.len(), 4, "caret movement ends the run");
        assert!(pop_history(&mut history, false).is_some());
        record_history(&mut history, &snapshot("abcde"), Some(EditRun::Insert));
        assert_eq!(history.undo.len(), 2, "undo ends the run");
    }

    #[test]
    fn the_shape_window_covers_the_caret_without_growing_with_the_value() {
        let short = "abc";
        assert_eq!(shape_window_range(short, 1), (0, short.len()));
        let long: String = "a".repeat(4 * SHAPE_WINDOW_GRAPHEMES);
        let (start, end) = shape_window_range(&long, long.len());
        assert!(end - start <= SHAPE_WINDOW_GRAPHEMES);
        assert!(long.len() - end < SHAPE_WINDOW_GRAPHEMES);
        let (start, end) = shape_window_range(&long, long.len() / 2);
        assert!(end - start <= SHAPE_WINDOW_GRAPHEMES);
        assert!(long.len() / 2 - start < SHAPE_WINDOW_GRAPHEMES);
        let near_start = shape_window_range(&long, 8);
        assert_eq!(near_start, (0, SHAPE_WINDOW_GRAPHEMES));
    }

    #[gpui::test]
    fn a_maximum_length_value_only_shapes_a_window(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let value = "long-query-".repeat(MAX_TEXT_CHARS / 10);
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("Search", cx, |_, _| {})
                .with_width(px(180.0))
                .with_text(value.clone())
        });
        cx.run_until_parked();
        let (shaped, cursor, contains_cursor) = input.read_with(cx, |input, _| {
            let shape = input.shape.as_ref().unwrap();
            let cursor = input.cursor;
            (
                shape.text.len(),
                cursor,
                shape.contains(cursor) && shape.text == value[shape.start..shape.end],
            )
        });
        assert!(
            shaped <= SHAPE_WINDOW_GRAPHEMES * 4,
            "shaped {shaped} bytes"
        );
        assert!(
            contains_cursor,
            "the window must hold the caret at {cursor}"
        );
    }
}
