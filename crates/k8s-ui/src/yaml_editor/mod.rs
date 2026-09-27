//! Provides an editable YAML buffer, virtualized highlighting, and search.

mod buffer;
mod diagnostics;
mod search;
mod tokenizer;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Bounds, ClickEvent, ClipboardItem, Context, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, Font, Hsla, IntoElement, KeyDownEvent,
    ListHorizontalSizingBehavior, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement,
    Pixels, Point, Render, Role, ScrollStrategy, ShapedLine, SharedString, Styled, Task,
    TextInputConfiguration, TextRun, UTF16Selection, UniformListScrollHandle, Window, canvas, div,
    point, px, size, uniform_list,
};
use ui::prelude::*;
use ui::{
    ButtonStyle, KeyBinding as UiKeyBinding, ScrollAxes, ScrollbarStyle, Scrollbars, TintColor,
    Tooltip, WithScrollbar,
};

use crate::design::{self, Severity, space};
use crate::session::TextInput;
use crate::settings::DataTypography;
use crate::shell::{Copy, Cut, FocusNext, FocusPrevious, Paste, Redo, SelectAll, Undo};

use self::buffer::{EditBuffer, INDENT, grapheme_start};
use self::diagnostics::{first_by_line, validate};
use self::search::{
    LineHits, Match, SearchOptions, byte_for_cell_column, cell_column, find_matches_with,
    group_by_line, line_segments_window, match_column, window_cells,
};
use self::tokenizer::TokenKind;

pub use self::diagnostics::Diagnostic;

/// The state shown when no document is available, shared with the Inspector's YAML tab.
///
/// The two surfaces used to carry their own copy of this sentence and disagree on punctuation, so
/// the same state read differently depending on which panel held it. One definition, one string.
pub const EMPTY_TITLE: &str = YAML_EMPTY_TITLE;
/// See [`EMPTY_TITLE`].
pub const EMPTY_HINT: &str = YAML_EMPTY_HINT;

/// The em ratio of the buffer font, used as the grid unit for the segment positions.
///
/// The buffer font advances exactly 600/1000 em, so this grid and a shaped line agree
/// on every ASCII column and the gutter, the indent guides and the scroll unit land
/// where the text is painted. A shaped line is still the authority for glyph positions,
/// because a character outside the ASCII set is not obliged to advance by this ratio.
const MONO_ADVANCE: f32 = 0.6;
const SEARCH_FIELD_WIDTH: Pixels = px(220.);
/// Extends the selection through a selected newline.
const NEWLINE_SELECTION_WIDTH: Pixels = px(4.);
/// Sets the caret width.
const CURSOR_WIDTH: Pixels = px(2.);
/// Width of the change rail and the diagnostic marker in the gutter.
const GUTTER_RAIL: Pixels = px(3.);

type ApplyRequest = Box<dyn Fn(String, &mut Window, &mut App)>;
type EditHandler = Box<dyn Fn(&mut App)>;

gpui::actions!(k8s_yaml, [Apply]);

const SCROLLBAR_HIT_PADDING: Pixels = px(20.);
const LINE_WINDOW_CHARS: usize = 256;
/// Extra characters kept before the first visible column.
const LINE_WINDOW_MARGIN: usize = 32;
const MAX_SCROLL_CHARS: usize = 16_384;
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);
/// Typing pauses before the document is parsed, so validation never runs per keystroke.
const VALIDATE_DEBOUNCE: Duration = Duration::from_millis(300);
const CARET_BLINK_INTERVAL: Duration = Duration::from_millis(500);
/// Rows the document highlight wash may cover, so a huge block cannot flood the viewport.
const BLOCK_HIGHLIGHT_LIMIT: usize = 200;
/// Grid cells of one indent step, which is the YAML indent width.
const INDENT_CELLS: usize = INDENT.len();
/// Upper bound on the indent guides one line paints.
const MAX_INDENT_GUIDES: usize = 32;

// The shortcut sheet's three sizes were literals: 190px for the key column, 460px for the panel,
// 520px for its height. A fixed 520px is 81% of the 640px window `design::size::WINDOW_MIN`
// allows, so on the smallest supported window the sheet filled the editor it was explaining.
// `UI-REVIEW-M3` M-4 set the precedent for this exact panel with `min(520px, 60vh)` and
// `min(460px, 40vw)`, and that is what it uses now: the sheet is a dialog over the document, so it
// has to leave the document visible on a small window and stop growing on a large one.

/// Fixed slot for the key column, so every description starts at the same x.
///
/// The widest entry is `Ctrl+Enter in the document`, 26 characters at `design::TEXT_SMALL` (11px).
/// A narrower slot wraps the keys, and a wrapped key stops reading as paired with its description.
const CHEAT_SHEET_KEY_COLUMN: f32 = 190.;

/// Caps the sheet on a window taller than 520 / `CHEAT_SHEET_HEIGHT_FRACTION`, so it keeps being a
/// dialog over the document rather than growing into a full-height panel.
const CHEAT_SHEET_MAX_HEIGHT: f32 = 520.;

/// Caps the sheet on a window wider than 460 / `CHEAT_SHEET_WIDTH_FRACTION`. What is left after
/// the key column and the panel's `space::LG` padding is a 230px measure, about 40 characters at
/// `design::TEXT_SMALL`; a wider sheet would only add line length to a reference list.
const CHEAT_SHEET_MAX_WIDTH: f32 = 460.;

/// Fraction of the window height the shortcut sheet may take.
const CHEAT_SHEET_HEIGHT_FRACTION: f32 = 0.6;
/// Fraction of the window width the shortcut sheet may take.
const CHEAT_SHEET_WIDTH_FRACTION: f32 = 0.4;

/// The state shown when no document has been handed to the editor.
///
/// Two surfaces show it, this editor and the Inspector's YAML tab, and they used to carry their own
/// copy of the sentence and disagree on punctuation. The strings are one definition and
/// `panels::common::empty_state` is the one implementation, so the same state cannot read two ways.
const YAML_EMPTY_TITLE: &str = "No YAML to show";
const YAML_EMPTY_HINT: &str = "Select a row to inspect its YAML.";

/// The shortcut sheet's height in a window of `viewport_height`.
fn cheat_sheet_height(viewport_height: Pixels) -> Pixels {
    px(f32::from(viewport_height) * CHEAT_SHEET_HEIGHT_FRACTION).min(px(CHEAT_SHEET_MAX_HEIGHT))
}

/// The shortcut sheet's width in a window of `viewport_width`.
fn cheat_sheet_width(viewport_width: Pixels) -> Pixels {
    px(f32::from(viewport_width) * CHEAT_SHEET_WIDTH_FRACTION).min(px(CHEAT_SHEET_MAX_WIDTH))
}

#[derive(Clone, Debug)]
struct Composition {
    range: Range<usize>,
    text: String,
    selected: Option<Range<usize>>,
    selected_reversed: bool,
}

pub struct YamlView {
    buffer: Option<Rc<RefCell<EditBuffer>>>,
    matches: Vec<Match>,
    by_line: Arc<LineHits>,
    active: usize,
    query: String,
    search_yank: Option<String>,
    search_input: Entity<TextInput>,
    /// The replace field, shown by the search bar when the user asks for it.
    replace_input: Entity<TextInput>,
    replace_visible: bool,
    replace_text: String,
    /// Literal by default, with optional case sensitivity and regular expressions.
    search_options: SearchOptions,
    /// Why the current pattern cannot run, shown instead of a match count.
    search_error: Option<String>,
    search_visible: bool,
    search_task: Option<Task<()>>,
    search_epoch: u64,
    search_pending: bool,
    editable: bool,
    input_locked: bool,
    /// True while a caller is still fetching the document text, so a caller can tell a load
    /// in flight from a document that is simply unavailable.
    yaml_loading: bool,
    caret_blink_visible: bool,
    caret_blink_epoch: u64,
    caret_blink_task: Option<Task<()>>,
    caret_focused: bool,
    /// Bumped by every keystroke and caret move, so the blink loop can tell whether the
    /// user touched the document during the last interval.
    caret_activity: u64,
    reduce_motion: bool,
    focus_handle: FocusHandle,
    scroll: UniformListScrollHandle,
    list_origin: Rc<Cell<Point<Pixels>>>,
    dragging: bool,
    page_rows: usize,
    on_apply: Option<ApplyRequest>,
    on_edit: Option<EditHandler>,
    /// Stores sorted inline diagnostics for rendering.
    diagnostics: Vec<Diagnostic>,
    diagnostics_by_line: Arc<HashMap<usize, usize>>,
    /// Bumped by every keystroke, so a slow parse cannot overwrite a newer result.
    validate_epoch: u64,
    validate_task: Option<Task<()>>,
    validate_pending: bool,
    /// True while a caller owns the diagnostics, so live validation stays quiet until the
    /// next edit instead of overwriting an apply error.
    external_diagnostics: bool,
    /// Rows of the block the caret is in, for the document highlight.
    block_highlight: Option<Range<usize>>,
    /// The bracket next to the caret, for the bracket highlight.
    bracket_highlight: Option<usize>,
    /// The caret position the highlights were computed for, so a frame that only scrolls
    /// or repaints does not rescan the block.
    highlight_caret: Option<(usize, usize)>,
    cheat_sheet_visible: bool,
    composition: Option<Composition>,
    /// Bumped whenever the document or the composition changes.
    view_generation: u64,
    /// Display text for the current generation, shared with the input protocol.
    display_cache: Option<(u64, Rc<String>)>,
}

impl YamlView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let view = cx.weak_entity();
        let search_input = cx.new(|cx| {
            let view = view.clone();
            TextInput::new(
                "Find in YAML",
                cx,
                move |text, cx| {
                    let view = view.clone();
                    let text = text.to_owned();
                    cx.defer(move |cx| {
                        view.update(cx, |view, cx| {
                            view.set_search_query_from_input(&text, cx)
                        })
                        .ok();
                    });
                },
            )
            .with_accessibility(
                "Find in YAML",
                "Search YAML as you type. Press Enter for the next match, Shift+Enter for the previous match, or Escape to close.",
                "Clear YAML Search",
            )
            .with_width(SEARCH_FIELD_WIDTH)
            .without_escape_hint()
        });
        let replace_input = cx.new(|cx| {
            let view = view.clone();
            TextInput::new(
                "Replace in YAML",
                cx,
                move |text, cx| {
                    let view = view.clone();
                    let text = text.to_owned();
                    cx.defer(move |cx| {
                        view.update(cx, |view, _| view.replace_text = text).ok();
                    });
                },
            )
            .with_accessibility(
                "Replace in YAML",
                "Enter replaces the current match and moves on, Control+Enter replaces every match, or Escape closes the search.",
                "Clear Replace Text",
            )
            .with_width(SEARCH_FIELD_WIDTH)
            .without_escape_hint()
        });
        search_input.read(cx).focus_handle(cx).tab_index(1);
        replace_input.read(cx).focus_handle(cx).tab_index(1);
        Self {
            buffer: None,
            matches: Vec::new(),
            by_line: Arc::new(LineHits::new()),
            active: 0,
            query: String::new(),
            search_yank: None,
            search_input,
            replace_input,
            replace_visible: false,
            replace_text: String::new(),
            search_options: SearchOptions::default(),
            search_error: None,
            search_visible: false,
            search_task: None,
            search_epoch: 0,
            search_pending: false,
            editable: true,
            input_locked: false,
            yaml_loading: false,
            caret_blink_visible: true,
            caret_blink_epoch: 0,
            caret_blink_task: None,
            caret_focused: false,
            caret_activity: 0,
            reduce_motion: cx.reduce_motion(),
            focus_handle: cx.focus_handle().tab_stop(true).tab_index(0),
            scroll: UniformListScrollHandle::new(),
            list_origin: Rc::new(Cell::new(point(px(0.), px(0.)))),
            dragging: false,
            page_rows: 20,
            on_apply: None,
            on_edit: None,
            diagnostics: Vec::new(),
            diagnostics_by_line: Arc::new(HashMap::new()),
            validate_epoch: 0,
            validate_task: None,
            validate_pending: false,
            external_diagnostics: false,
            block_highlight: None,
            bracket_highlight: None,
            highlight_caret: None,
            cheat_sheet_visible: false,
            composition: None,
            view_generation: 0,
            display_cache: None,
        }
    }

    /// Replaces the document and clears the search state.
    ///
    /// The caret, selection, scroll, and IME composition survive when a document already
    /// exists, so a live update of the same object does not interrupt typing. Use
    /// [`Self::reset_view_state`] when the new content is a different object.
    pub fn set_text(&mut self, text: Option<String>, cx: &mut Context<Self>) {
        let caret = self
            .buffer
            .as_ref()
            .filter(|_| text.is_some())
            .map(|buffer| {
                let buffer = buffer.borrow();
                (buffer.cursor(), buffer.anchor())
            });
        self.buffer = text.map(|text| Rc::new(RefCell::new(EditBuffer::new(&text))));
        self.invalidate_display();
        self.clear_diagnostics_silent();
        match caret {
            Some((cursor, anchor)) => {
                if let Some(buffer) = self.buffer.clone() {
                    let mut buffer = buffer.borrow_mut();
                    // Anchor first, then the caret, so the selection direction survives.
                    buffer.set_cursor(anchor, false);
                    buffer.set_cursor(cursor, true);
                    let length = buffer.byte_len();
                    drop(buffer);
                    if let Some(composition) = self.composition.as_mut() {
                        composition.range = clip_range_to_len(composition.range.clone(), length);
                    }
                }
            }
            None => {
                self.scroll
                    .0
                    .borrow()
                    .base_handle
                    .set_offset(point(px(0.), px(0.)));
                self.composition = None;
            }
        }
        let query = self.search_input.read(cx).text().to_owned();
        self.query = query.clone();
        self.search_input
            .update(cx, |input, cx| input.set_text(query, cx));
        self.input_locked = false;
        self.external_diagnostics = false;
        self.show_caret();
        self.recompute();
        self.schedule_validation(cx);
        cx.notify();
    }

    /// Resets caret, scroll, and IME state after loading a different document.
    pub fn reset_view_state(&mut self, cx: &mut Context<Self>) {
        self.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(0.)));
        self.composition = None;
        self.invalidate_display();
        self.show_caret();
        cx.notify();
    }

    /// Sorts diagnostics and scrolls to the first one.
    ///
    /// A caller owns the diagnostics from here: live validation waits for the next edit
    /// before it reports again, so an apply error is not overwritten by a parse of the
    /// same text.
    pub fn set_diagnostics(&mut self, diagnostics: Vec<Diagnostic>, cx: &mut Context<Self>) {
        self.external_diagnostics = true;
        self.cancel_validation();
        self.store_diagnostics(diagnostics);
        if let Some(first) = self.diagnostics.first() {
            self.scroll_row_to(
                first.line,
                ScrollStrategy::Center,
                &DataTypography::from_theme_settings(cx),
            );
        }
        cx.notify();
    }

    pub fn clear_diagnostics(&mut self, cx: &mut Context<Self>) {
        if self.diagnostics.is_empty() {
            return;
        }
        self.external_diagnostics = true;
        self.cancel_validation();
        self.clear_diagnostics_silent();
        cx.notify();
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    fn store_diagnostics(&mut self, mut diagnostics: Vec<Diagnostic>) {
        diagnostics.sort_by_key(|diagnostic| (diagnostic.line, diagnostic.column));
        self.diagnostics_by_line = Arc::new(first_by_line(&diagnostics));
        self.diagnostics = diagnostics;
    }

    fn clear_diagnostics_silent(&mut self) {
        if !self.diagnostics.is_empty() {
            self.diagnostics.clear();
            self.diagnostics_by_line = Arc::new(HashMap::new());
        }
    }

    fn cancel_validation(&mut self) {
        self.validate_epoch = self.validate_epoch.wrapping_add(1);
        self.validate_task.take();
        self.validate_pending = false;
    }

    /// Parses the document after a typing pause and reports the result.
    ///
    /// The parse runs on the background executor, so a large document never blocks a
    /// frame, and the epoch drops a result that a newer edit has already superseded.
    fn schedule_validation(&mut self, cx: &mut Context<Self>) {
        self.cancel_validation();
        let Some(text) = self.buffer.as_ref().map(|buffer| buffer.borrow().text()) else {
            return;
        };
        let epoch = self.validate_epoch;
        self.validate_pending = true;
        self.validate_task = Some(cx.spawn(async move |view, cx| {
            cx.background_executor().timer(VALIDATE_DEBOUNCE).await;
            let diagnostics = cx
                .background_executor()
                .spawn(async move { validate(&text) })
                .await;
            view.update(cx, |view, cx| {
                if view.validate_epoch != epoch {
                    return;
                }
                view.validate_task.take();
                view.validate_pending = false;
                if view.external_diagnostics {
                    // A caller owns the diagnostics until the next edit.
                    return;
                }
                view.store_diagnostics(diagnostics);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Returns the current document text.
    pub fn text(&self) -> Option<String> {
        self.buffer.as_ref().map(|buffer| buffer.borrow().text())
    }

    pub fn is_dirty(&self) -> bool {
        self.buffer
            .as_ref()
            .is_some_and(|buffer| buffer.borrow().is_dirty())
    }

    pub fn mark_saved(&mut self, cx: &mut Context<Self>) {
        if let Some(buffer) = &self.buffer {
            buffer.borrow_mut().mark_saved();
            cx.notify();
        }
    }

    pub fn set_editable(&mut self, editable: bool, cx: &mut Context<Self>) {
        if self.editable == editable {
            return;
        }
        self.editable = editable;
        if !editable {
            self.dragging = false;
            self.composition = None;
            self.invalidate_display();
        }
        self.show_caret();
        cx.notify();
    }

    pub fn is_editable(&self) -> bool {
        self.editable
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// Caret and anchor byte offsets, for tests.
    #[cfg(test)]
    pub(crate) fn caret(&self) -> (usize, usize) {
        self.buffer.as_ref().map_or((0, 0), |buffer| {
            let buffer = buffer.borrow();
            (buffer.cursor(), buffer.anchor())
        })
    }

    /// Places the caret, or the selection when `anchor` differs, for tests.
    #[cfg(test)]
    pub(crate) fn place_caret(&mut self, anchor: usize, cursor: usize, cx: &mut Context<Self>) {
        self.with_buffer(|buffer| {
            buffer.set_cursor(anchor, false);
            if cursor != anchor {
                buffer.set_cursor(cursor, true);
            }
        });
        self.after_cursor_change(cx);
    }

    pub fn set_input_locked(&mut self, locked: bool, cx: &mut Context<Self>) {
        if self.input_locked == locked {
            return;
        }
        self.input_locked = locked;
        if locked {
            self.dragging = false;
            self.composition = None;
            self.invalidate_display();
        }
        self.show_caret();
        cx.notify();
    }

    pub fn is_input_locked(&self) -> bool {
        self.input_locked
    }

    /// Records that the document text is on its way or has arrived.
    ///
    /// A document that has not arrived yet is not the same as a document that does not
    /// exist, so the caller sets this when a load starts and clears it when the load
    /// callback runs, and shows a progress state instead of an empty one.
    pub fn set_yaml_loading(&mut self, loading: bool, cx: &mut Context<Self>) {
        if self.yaml_loading == loading {
            return;
        }
        self.yaml_loading = loading;
        cx.notify();
    }

    /// True while the document text is still being fetched.
    pub fn is_yaml_loading(&self) -> bool {
        self.yaml_loading
    }

    fn can_edit(&self) -> bool {
        self.editable && !self.input_locked
    }

    fn stop_caret_blink(&mut self) {
        self.caret_blink_task.take();
        self.caret_blink_epoch = self.caret_blink_epoch.wrapping_add(1);
        self.caret_blink_visible = true;
    }

    /// Shows the caret solid and records the interaction.
    ///
    /// The blink loop compares this counter, so the caret stays solid while the user types
    /// or moves it and only starts blinking again after a pause.
    fn show_caret(&mut self) {
        self.caret_activity = self.caret_activity.wrapping_add(1);
        self.caret_blink_visible = true;
    }

    fn restart_caret_blink(&mut self, cx: &mut Context<Self>) {
        self.stop_caret_blink();
        if self.reduce_motion {
            return;
        }
        let epoch = self.caret_blink_epoch;
        let mut seen = self.caret_activity;
        self.caret_blink_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(CARET_BLINK_INTERVAL).await;
                let keep_going = this
                    .update(cx, |view, cx| {
                        if view.caret_blink_epoch != epoch || !view.caret_focused {
                            return false;
                        }
                        if view.caret_activity != seen {
                            // Typing during the last interval: hold the caret solid.
                            seen = view.caret_activity;
                            view.caret_blink_visible = true;
                        } else {
                            view.caret_blink_visible = !view.caret_blink_visible;
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        }));
    }

    fn sync_caret_blink(&mut self, window: &Window, cx: &mut Context<Self>) {
        let focused = self.document_focused(window, cx);
        let reduce_motion = cx.reduce_motion();
        let motion_changed = reduce_motion != self.reduce_motion;
        let focus_changed = focused != self.caret_focused;
        self.reduce_motion = reduce_motion;
        if motion_changed && reduce_motion {
            self.stop_caret_blink();
        }
        if focus_changed {
            self.caret_focused = focused;
            if focused {
                self.restart_caret_blink(cx);
            } else {
                self.stop_caret_blink();
            }
        } else if motion_changed && !reduce_motion && focused {
            self.restart_caret_blink(cx);
        }
    }

    pub fn set_on_apply_requested(
        &mut self,
        callback: impl Fn(String, &mut Window, &mut App) + 'static,
    ) {
        self.on_apply = Some(Box::new(callback));
    }

    pub fn set_on_edit(&mut self, callback: impl Fn(&mut App) + 'static) {
        self.on_edit = Some(Box::new(callback));
    }

    /// Gives keyboard focus to the editor.
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Opens the search field and gives it keyboard focus.
    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_visible = true;
        self.show_caret();
        self.composition = None;
        self.invalidate_display();
        let focus = self.search_input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Scrolls to a zero-based line.
    pub fn scroll_to_line(&self, line: usize, cx: &mut Context<Self>) {
        self.scroll_row_to(
            line,
            ScrollStrategy::Top,
            &DataTypography::from_theme_settings(cx),
        );
        cx.notify();
    }

    /// Scrolls to a zero-based line and column and puts the caret there.
    ///
    /// The column counts a fullwidth character as two cells, the same grid the click path
    /// uses, so a position taken from a diagnostic lands on the character the user sees.
    /// Only the caret moves, and the selection collapses with it.
    pub fn focus_line(&self, line: usize, column: usize, cx: &mut Context<Self>) {
        self.scroll_to_line(line, cx);
        let Some(buffer) = &self.buffer else {
            return;
        };
        let offset = byte_offset(&buffer.borrow(), line, column);
        buffer.borrow_mut().set_cursor(offset, false);
    }

    /// Sets the search query and reveals the first match.
    pub fn set_search_query(&mut self, query: impl Into<String>, cx: &mut Context<Self>) {
        let query = query.into();
        self.search_input
            .update(cx, |input, cx| input.set_text(query, cx));
        self.query = self.search_input.read(cx).text().to_owned();
        self.search_visible = true;
        self.recompute();
        self.reveal_active(&DataTypography::from_theme_settings(cx));
        cx.notify();
    }

    /// Shows the replace field and gives it keyboard focus.
    pub fn focus_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_visible = true;
        self.search_visible = true;
        self.show_caret();
        self.composition = None;
        self.invalidate_display();
        let focus = self.replace_input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Searches with the given options, so a caller can reach case sensitivity and
    /// regular expressions without the search bar.
    pub fn set_search_options(&mut self, options: SearchOptions, cx: &mut Context<Self>) {
        if self.search_options == options {
            return;
        }
        self.search_options = options;
        self.recompute();
        cx.notify();
    }

    pub fn search_options(&self) -> SearchOptions {
        self.search_options
    }

    fn set_search_query_from_input(&mut self, query: &str, cx: &mut Context<Self>) {
        if self.query == query {
            return;
        }
        self.query = query.to_owned();
        self.schedule_search(cx);
        cx.notify();
    }

    fn search_focused(&self, window: &Window, cx: &App) -> bool {
        self.search_input
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx)
            || self
                .replace_input
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
    }

    fn cancel_search(&mut self) {
        self.search_epoch = self.search_epoch.wrapping_add(1);
        self.search_task.take();
        self.search_pending = false;
    }

    fn apply_matches(&mut self, matches: Vec<Match>) {
        self.matches = matches;
        self.active = if self.matches.is_empty() {
            0
        } else {
            self.active.min(self.matches.len() - 1)
        };
        self.by_line = Arc::new(group_by_line(&self.matches, self.active));
    }

    fn recompute(&mut self) {
        self.cancel_search();
        let found = match &self.buffer {
            Some(buffer) if !self.query.is_empty() => {
                find_matches_with(buffer.borrow().lines(), &self.query, self.search_options)
            }
            _ => Ok(Vec::new()),
        };
        match found {
            Ok(matches) => {
                self.search_error = None;
                self.apply_matches(matches);
            }
            // An invalid pattern is not "no matches": the search bar says why.
            Err(reason) => {
                self.search_error = Some(reason);
                self.apply_matches(Vec::new());
            }
        }
    }

    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        self.cancel_search();
        self.matches.clear();
        self.active = 0;
        self.by_line = Arc::new(LineHits::new());
        self.search_error = None;
        if self.query.is_empty() {
            return;
        }
        let query = self.query.clone();
        let options = self.search_options;
        // The line text is shared with the buffer, so the snapshot copies pointers, not
        // the document, and the background scan never sees a document that changes.
        let lines: Vec<Arc<str>> = self.buffer.as_ref().map_or_else(Vec::new, |buffer| {
            buffer
                .borrow()
                .lines()
                .iter()
                .map(|line| line.shared_text())
                .collect()
        });
        let epoch = self.search_epoch;
        self.search_pending = true;
        self.search_task = Some(cx.spawn(async move |view, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let matches = cx
                .background_executor()
                .spawn(async move { find_matches_with(&lines, &query, options) })
                .await;
            view.update(cx, |view, cx| {
                if view.search_epoch != epoch {
                    return;
                }
                view.search_pending = false;
                match matches {
                    Ok(matches) => {
                        view.search_error = None;
                        view.apply_matches(matches);
                    }
                    Err(reason) => {
                        view.search_error = Some(reason);
                        view.apply_matches(Vec::new());
                    }
                }
                view.reveal_active(&DataTypography::from_theme_settings(cx));
                cx.notify();
            })
            .ok();
        }));
    }

    fn document_focused(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx) && !self.search_focused(window, cx)
    }

    fn remember_search_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.focused_search_input(window, cx);
        let yank = input.update(cx, |input, cx| {
            input.unmark_text(window, cx);
            let text = input.text().to_owned();
            let selection = input.selected_text_range(false, window, cx)?;
            if selection.range.start >= selection.range.end {
                return None;
            }
            let start = string_utf16_to_byte(&text, selection.range.start);
            let end = string_utf16_to_byte(&text, selection.range.end);
            Some(text[start..end].to_owned())
        });
        if let Some(text) = yank {
            self.search_yank = Some(text);
        }
    }

    fn edit_search_command(&mut self, command: &str, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.focused_search_input(window, cx);
        let yank = self.search_yank.clone();
        let mut deleted = None;
        input.update(cx, |input, cx| {
            input.unmark_text(window, cx);
            let text = input.text().to_owned();
            let selection = input.selected_text_range(false, window, cx);
            if command == "y" {
                if let Some(yank) = yank {
                    input.replace_text_in_range(None, &yank, window, cx);
                }
                return;
            }
            let (cursor, selected) = selection.map_or((utf16_len(&text), None), |selection| {
                let cursor = if selection.reversed {
                    selection.range.start
                } else {
                    selection.range.end
                };
                let selected =
                    (selection.range.start < selection.range.end).then_some(selection.range);
                (cursor, selected)
            });
            let range = selected.unwrap_or_else(|| {
                let cursor = string_utf16_to_byte(&text, cursor);
                match command {
                    "u" => 0..cursor,
                    "k" => cursor..text.len(),
                    "w" => previous_search_word_start(&text, cursor)..cursor,
                    _ => cursor..cursor,
                }
            });
            if range.start >= range.end {
                return;
            }
            deleted = Some(text[range.clone()].to_owned());
            let range =
                string_byte_to_utf16(&text, range.start)..string_byte_to_utf16(&text, range.end);
            input.replace_text_in_range(Some(range), "", window, cx);
        });
        if let Some(text) = deleted {
            self.search_yank = Some(text);
        }
    }

    fn search_composing(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let input = self.focused_search_input(window, cx);
        input.update(cx, |input, cx| {
            input.marked_text_range(window, cx).is_some()
        })
    }

    fn toggle_case_sensitive(&mut self, cx: &mut Context<Self>) {
        self.set_search_options(
            SearchOptions {
                case_sensitive: !self.search_options.case_sensitive,
                ..self.search_options
            },
            cx,
        );
    }

    fn toggle_regex(&mut self, cx: &mut Context<Self>) {
        self.set_search_options(
            SearchOptions {
                regex: !self.search_options.regex,
                ..self.search_options
            },
            cx,
        );
    }

    fn reset_search(&mut self, cx: &mut Context<Self>) {
        self.query.clear();
        self.search_input.update(cx, |input, cx| input.clear(cx));
        self.recompute();
    }

    fn jump(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_composing(window, cx) {
            return;
        }
        if self.search_pending {
            self.recompute();
        }
        if self.matches.is_empty() {
            return;
        }
        let len = self.matches.len() as isize;
        self.active = (self.active as isize + delta).rem_euclid(len) as usize;
        self.by_line = Arc::new(group_by_line(&self.matches, self.active));
        self.reveal_active(&DataTypography::from_theme_settings(cx));
        let focus = self.search_input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Document byte range of one match.
    fn match_range(&self, hit: &Match) -> Option<Range<usize>> {
        let buffer = self.buffer.as_ref()?.borrow();
        let line = buffer.line(hit.line);
        let start = buffer.line_start(hit.line);
        let line_len = line.text().len();
        let from = start + hit.start.min(line_len);
        let to = (start + hit.end).clamp(from, start + line_len);
        (from < to).then_some(from..to)
    }

    /// Document byte ranges of every current match.
    fn match_ranges(&self) -> Vec<Range<usize>> {
        self.matches
            .iter()
            .filter_map(|hit| self.match_range(hit))
            .collect()
    }

    /// Replaces the match the user is on, then moves to the next or previous one.
    fn replace_current(&mut self, delta: isize, cx: &mut Context<Self>) {
        if !self.can_edit() {
            return;
        }
        let Some(hit) = self.matches.get(self.active).cloned() else {
            return;
        };
        let text = self.replace_text.clone();
        let Some(range) = self.match_range(&hit) else {
            return;
        };
        let replaced = self
            .buffer
            .as_ref()
            .map(|buffer| buffer.borrow().slice(range.clone()))
            .unwrap_or_default();
        if replaced == text {
            // Nothing changed, so the caret only moves to the next match.
            self.jump_without_focus(delta, cx);
            return;
        }
        let replaced_start = range.start;
        let replaced_end = replaced_start + text.len();
        if !self.edit_buffer(|buffer| buffer.replace(range, &text)) {
            return;
        }
        self.after_edit(cx);
        // The document moved under the match list, so search the new text before moving.
        self.recompute();
        self.drop_matches_inside(std::slice::from_ref(&(replaced_start..replaced_end)));
        // The next match is the one after the text just written: a match inside the
        // replacement is spent, so Replace walks on instead of rewriting the same one.
        self.active = self.first_match_from(replaced_end);
        self.by_line = Arc::new(group_by_line(&self.matches, self.active));
        self.reveal_active(&DataTypography::from_theme_settings(cx));
        cx.notify();
    }

    /// Drops the matches that fall inside text a replacement has just written.
    ///
    /// A replacement can contain the query, and then the text the user wrote matches
    /// itself. That occurrence is spent rather than outstanding work: keeping it would
    /// make Replace rewrite one occurrence for as long as the key is held, and Replace
    /// All write a second copy over every occurrence it had already done.
    fn drop_matches_inside(&mut self, spans: &[Range<usize>]) {
        if spans.is_empty() {
            return;
        }
        let hits = std::mem::take(&mut self.matches);
        let mut pending = Vec::with_capacity(hits.len());
        for hit in hits {
            let spent = self.match_range(&hit).is_some_and(|found| {
                spans
                    .iter()
                    .any(|span| found.start >= span.start && found.end <= span.end)
            });
            if !spent {
                pending.push(hit);
            }
        }
        self.apply_matches(pending);
    }

    /// Index of the first match at or after `byte`, or zero when there is none.
    fn first_match_from(&self, byte: usize) -> usize {
        let mut active = 0;
        for (index, hit) in self.matches.iter().enumerate() {
            if self
                .match_range(hit)
                .is_some_and(|range| range.start >= byte)
            {
                active = index;
                break;
            }
        }
        active
    }

    /// Replaces every match in one undo step.
    fn replace_all_matches(&mut self, cx: &mut Context<Self>) {
        if !self.can_edit() {
            return;
        }
        let text = self.replace_text.clone();
        let ranges = self.match_ranges();
        if ranges.is_empty() {
            return;
        }
        // `replace_all` writes from the end of the document backwards, so every range
        // before the one it wrote moves by what that write added or took. Following the
        // same order is what says where this pass ends up in the document.
        let mut shift = 0isize;
        let written: Vec<Range<usize>> = ranges
            .iter()
            .rev()
            .map(|range| {
                let start = (range.start as isize + shift).max(0) as usize;
                shift += text.len() as isize - (range.end - range.start) as isize;
                start..start + text.len()
            })
            .collect();
        let replacements: Vec<(Range<usize>, String)> = ranges
            .into_iter()
            .map(|range| (range, text.clone()))
            .collect();
        if !self.edit_buffer(|buffer| buffer.replace_all(&replacements)) {
            return;
        }
        self.after_edit(cx);
        // The ranges were the ones the document had when the key went down, so the
        // search runs again on the new text and the matches this pass wrote are dropped.
        self.recompute();
        self.drop_matches_inside(&written);
        self.active = self.first_match_from(written.last().map_or(0, |span| span.start));
        self.by_line = Arc::new(group_by_line(&self.matches, self.active));
        cx.notify();
    }

    /// Moves the active match without stealing focus from the search fields.
    fn jump_without_focus(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.matches.is_empty() {
            return;
        }
        let len = self.matches.len() as isize;
        self.active = (self.active as isize + delta).rem_euclid(len) as usize;
        self.by_line = Arc::new(group_by_line(&self.matches, self.active));
        self.reveal_active(&DataTypography::from_theme_settings(cx));
        cx.notify();
    }

    fn replace_focused(&self, window: &Window, cx: &App) -> bool {
        self.replace_input
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx)
    }

    /// The search field that currently has the keyboard.
    fn focused_search_input(&self, window: &Window, cx: &App) -> Entity<TextInput> {
        if self.replace_focused(window, cx) {
            self.replace_input.clone()
        } else {
            self.search_input.clone()
        }
    }

    fn viewport_bounds(&self) -> Bounds<Pixels> {
        let bounds = self.scroll.0.borrow().base_handle.bounds();
        if bounds.size.width > px(0.) && bounds.size.height > px(0.) {
            bounds
        } else {
            Bounds::new(self.list_origin.get(), size(px(0.), px(0.)))
        }
    }

    fn reveal_column(&self, row: usize, column: usize, typography: &DataTypography) {
        let base = self.scroll.0.borrow().base_handle.clone();
        let viewport = base.bounds().size.width;
        if viewport <= px(0.) {
            return;
        }
        let Some(buffer) = &self.buffer else {
            return;
        };
        let buffer = buffer.borrow();
        let line_count = buffer.line_count();
        let char_width = char_width(typography);
        let gutter = gutter_width(line_count, char_width);
        let line_width = logical_line_width(gutter, buffer.line(row).cells(), char_width);
        let x = gutter + char_width * column as f32;
        let right = (viewport - SCROLLBAR_HIT_PADDING).max(gutter + char_width);
        let offset = base.offset().x;
        let target = if x + offset > right - char_width {
            right - char_width - x
        } else if x + offset < gutter {
            gutter - x
        } else {
            offset
        };
        let max_offset = (line_width - viewport).max(px(0.));
        base.set_offset(point(target.clamp(-max_offset, px(0.)), base.offset().y));
    }

    /// Scrolls a row to the strategy position and applies the offset right away.
    ///
    /// `UniformListScrollHandle::scroll_to_item` defers the request to the list's
    /// next prepaint and skips it when the row is already visible, but
    /// [`Self::bounds_for_range`] reads the offset in the same update to place the
    /// IME candidate window. A pending or skipped request would therefore place the
    /// window against the old offset. This is the vertical counterpart of
    /// [`Self::reveal_column`], and clamps like the list does at prepaint.
    fn scroll_row_to(&self, row: usize, strategy: ScrollStrategy, typography: &DataTypography) {
        let viewport = self.scroll.0.borrow().base_handle.bounds().size.height;
        if viewport <= px(0.) {
            // Before the first layout only the list knows its own geometry.
            self.scroll.scroll_to_item_strict(row, strategy);
            return;
        }
        let Some(buffer) = &self.buffer else {
            return;
        };
        let line_count = buffer.borrow().line_count();
        let row_height = typography.line_height;
        let row_top = row_height * row as f32;
        let row_bottom = row_top + row_height;
        let scroll_top = -self.scroll.0.borrow().base_handle.offset().y;
        let top = match strategy {
            ScrollStrategy::Top => row_top,
            ScrollStrategy::Center => row_top + (row_height - viewport) / 2.,
            ScrollStrategy::Bottom => row_bottom - viewport,
            ScrollStrategy::Nearest => {
                if row_top < scroll_top {
                    row_top
                } else if row_bottom > scroll_top + viewport {
                    row_bottom - viewport
                } else {
                    return;
                }
            }
        };
        let max_offset = (row_height * line_count as f32 - viewport).max(px(0.));
        let base = self.scroll.0.borrow().base_handle.clone();
        let offset = base.offset();
        base.set_offset(point(offset.x, -top.clamp(px(0.), max_offset)));
    }

    fn reveal_active(&self, typography: &DataTypography) {
        let Some(hit) = self.matches.get(self.active) else {
            return;
        };
        self.scroll_row_to(hit.line, ScrollStrategy::Nearest, typography);
        let Some(buffer) = self.buffer.as_ref() else {
            return;
        };
        let buffer = buffer.borrow();
        let column = cell_column(buffer.line(hit.line).text(), hit.start);
        drop(buffer);
        self.reveal_column(hit.line, column, typography);
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_visible = false;
        self.reset_search(cx);
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn with_buffer(&mut self, edit: impl FnOnce(&mut EditBuffer)) {
        if let Some(buffer) = &self.buffer {
            edit(&mut buffer.borrow_mut());
        }
    }

    /// Selects the word around `offset`, for a double click.
    fn select_word(&mut self, offset: usize) {
        self.with_buffer(|buffer| {
            let Some(range) = buffer.word_range_at(offset) else {
                buffer.set_cursor(offset, false);
                return;
            };
            buffer.set_cursor(range.start, false);
            buffer.set_cursor(range.end, true);
        });
    }

    /// Selects the caret line, for a triple click.
    fn select_line(&mut self, row: usize) {
        self.with_buffer(|buffer| {
            let range = buffer.line_text_range(row);
            buffer.set_cursor(range.start, false);
            buffer.set_cursor(range.end, true);
        });
    }

    /// Recomputes the block the caret is in and the bracket next to it.
    ///
    /// The block is the run of lines at the caret's own indent level, so the highlight
    /// shows the mapping or list item the caret belongs to. The scan is bounded in both
    /// directions and only runs when the caret moves, so a huge document or a huge block
    /// never costs a frame.
    fn update_document_highlights(&mut self) {
        let Some(buffer) = self.buffer.clone() else {
            self.block_highlight = None;
            self.bracket_highlight = None;
            self.highlight_caret = None;
            return;
        };
        let buffer = buffer.borrow();
        let (row, column) = buffer.cursor_in_line();
        if self.highlight_caret == Some((row, column)) {
            return;
        }
        self.highlight_caret = Some((row, column));
        let level = indent_cells(buffer.line(row).text());
        let last_row = buffer.line_count().saturating_sub(1);
        let window_start = row.saturating_sub(BLOCK_HIGHLIGHT_LIMIT);
        let first = (window_start..=row)
            .rev()
            .find(|candidate| indent_cells(buffer.line(*candidate).text()) <= level);
        let limit = (row + BLOCK_HIGHLIGHT_LIMIT).min(last_row);
        let mut end = row;
        while end < limit && indent_cells(buffer.line(end + 1).text()) >= level {
            end += 1;
        }
        // A block that reaches either bound is not highlighted: the wash would cover the
        // whole viewport and tell the user nothing.
        self.block_highlight = match first {
            Some(first) if end < limit || limit == last_row => Some(first..end + 1),
            _ => None,
        };
        self.bracket_highlight = matching_bracket(buffer.line(row).text(), column);
    }

    fn ensure_buffer(&mut self) {
        if self.buffer.is_none() {
            self.buffer = Some(Rc::new(RefCell::new(EditBuffer::new(""))));
            self.invalidate_display();
            self.recompute();
        }
    }

    /// Runs a text edit and clears inline diagnostics.
    ///
    /// The stale diagnostics go away at once, and `after_edit` schedules the parse that
    /// reports the new ones, so the editor never shows an error for text that changed.
    fn edit_buffer(&mut self, edit: impl FnOnce(&mut EditBuffer) -> bool) -> bool {
        if self.input_locked {
            return false;
        }
        self.input_locked = true;
        let mut changed = false;
        self.with_buffer(|buffer| changed = edit(buffer));
        self.input_locked = false;
        if changed {
            self.clear_diagnostics_silent();
            self.composition = None;
            self.invalidate_display();
        }
        changed
    }

    fn type_text(&mut self, text: &str) -> bool {
        if !self.can_edit() || text.is_empty() {
            return false;
        }
        self.ensure_buffer();
        if let Some(composition) = self.composition.as_ref() {
            let range = composition.range.clone();
            self.edit_buffer(|buffer| buffer.replace(range, text))
        } else {
            self.edit_buffer(|buffer| buffer.insert(text))
        }
    }

    /// Invalidates search highlights and keeps the cursor visible.
    fn after_edit(&mut self, cx: &mut Context<Self>) {
        self.show_caret();
        if let Some(on_edit) = &self.on_edit {
            on_edit(cx);
        }
        if !self.query.is_empty() {
            self.schedule_search(cx);
        }
        // A caller-owned diagnostic is stale after an edit, so validation takes over again.
        self.external_diagnostics = false;
        self.schedule_validation(cx);
        self.reveal_cursor(&DataTypography::from_theme_settings(cx));
        cx.notify();
    }

    fn after_cursor_change(&mut self, cx: &mut Context<Self>) {
        self.show_caret();
        self.reveal_cursor(&DataTypography::from_theme_settings(cx));
        cx.notify();
    }

    fn reveal_cursor(&self, typography: &DataTypography) {
        let Some(buffer) = &self.buffer else {
            return;
        };
        let (row, column) = {
            let buffer = buffer.borrow();
            let (row, offset) = buffer.cursor_in_line();
            (row, cell_column(buffer.line(row).text(), offset))
        };
        self.scroll_row_to(row, ScrollStrategy::Nearest, typography);
        self.reveal_column(row, column, typography);
    }

    fn copy(&self, cx: &App) {
        let Some(buffer) = &self.buffer else {
            return;
        };
        let buffer = buffer.borrow();
        if let Some(selection) = buffer.selection() {
            cx.write_to_clipboard(ClipboardItem::new_string(buffer.slice(selection)));
        }
    }

    fn cut(&mut self, cx: &mut Context<Self>) -> bool {
        self.copy(cx);
        self.edit_buffer(|buffer| buffer.delete_selection())
    }

    fn paste(&mut self, cx: &App) -> bool {
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .is_some_and(|text| self.type_text(&text))
    }

    fn undo_action(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_focused(window, cx) {
            return;
        }
        if self.can_edit() && self.edit_buffer(|buffer| buffer.undo()) {
            self.after_edit(cx);
        }
        cx.stop_propagation();
    }

    fn redo_action(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_focused(window, cx) {
            return;
        }
        if self.can_edit() && self.edit_buffer(|buffer| buffer.redo()) {
            self.after_edit(cx);
        }
        cx.stop_propagation();
    }

    fn cut_action(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_focused(window, cx) {
            return;
        }
        if self.can_edit() && self.cut(cx) {
            self.after_edit(cx);
        }
        cx.stop_propagation();
    }

    fn copy_action(&mut self, _: &Copy, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_focused(window, cx) {
            return;
        }
        self.copy(cx);
        cx.stop_propagation();
    }

    fn paste_action(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_focused(window, cx) {
            return;
        }
        if self.can_edit() && self.paste(cx) {
            self.after_edit(cx);
        }
        cx.stop_propagation();
    }

    fn select_all_action(&mut self, _: &SelectAll, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_focused(window, cx) {
            return;
        }
        self.with_buffer(|buffer| buffer.select_all());
        self.after_cursor_change(cx);
        cx.stop_propagation();
    }

    fn request_apply(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.composition.is_some() || !self.can_edit() || !self.is_dirty() {
            return false;
        }
        let Some(text) = self.buffer.as_ref().map(|buffer| buffer.borrow().text()) else {
            return false;
        };
        let Some(on_apply) = self.on_apply.take() else {
            return false;
        };
        self.input_locked = true;
        on_apply(text, window, cx);
        if self.on_apply.is_none() {
            self.on_apply = Some(on_apply);
        }
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            this.update(cx, |view, cx| {
                view.input_locked = false;
                cx.notify();
            })
            .ok();
        });
        true
    }

    fn apply_action(&mut self, _: &Apply, window: &mut Window, cx: &mut Context<Self>) {
        if !self.document_focused(window, cx) {
            return;
        }
        if self.request_apply(window, cx) {
            cx.stop_propagation();
        }
    }

    /// Tab indents and Shift+Tab outdents inside the editor.
    ///
    /// `k8s_shell::FocusNext` owns Tab for the shell, so the editor takes the action
    /// instead of the key and gives it back when it cannot indent. Control+Tab still moves
    /// focus, which is the escape hatch HIG's keyboard table documents for a surface where
    /// Tab edits instead of navigating.
    fn focus_next_action(&mut self, _: &FocusNext, window: &mut Window, cx: &mut Context<Self>) {
        self.indent_action(-1, window, cx);
    }

    fn focus_previous_action(
        &mut self,
        _: &FocusPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.indent_action(1, window, cx);
    }

    /// `delta` is -1 for Tab and 1 for Shift+Tab. Without an edit to make, the keystroke
    /// is left to the shell so the caret can still leave the editor.
    fn indent_action(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_focused(window, cx) || !self.document_focused(window, cx) {
            return;
        }
        if !self.can_edit() {
            return;
        }
        let changed = self.edit_buffer(|buffer| {
            if delta < 0 {
                buffer.indent()
            } else {
                buffer.outdent()
            }
        });
        if changed {
            self.after_edit(cx);
            cx.stop_propagation();
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_focused(window, cx) {
            return;
        }
        let keystroke = &event.keystroke;
        let command = keystroke.modifiers.secondary();
        if command && keystroke.key == "f" {
            self.focus_search(window, cx);
            cx.stop_propagation();
            return;
        }
        if !self.document_focused(window, cx) {
            return;
        }
        if keystroke.modifiers.alt {
            match keystroke.key.as_str() {
                // Alt+H, not Ctrl+H: macOS owns the secondary key for Hide.
                "h" => {
                    self.focus_replace(window, cx);
                    cx.stop_propagation();
                    return;
                }
                "c" => {
                    self.toggle_case_sensitive(cx);
                    cx.stop_propagation();
                    return;
                }
                "r" => {
                    self.toggle_regex(cx);
                    cx.stop_propagation();
                    return;
                }
                "left" => {
                    self.with_buffer(|buffer| buffer.move_word(-1, false));
                    self.after_cursor_change(cx);
                    cx.stop_propagation();
                    return;
                }
                "right" => {
                    self.with_buffer(|buffer| buffer.move_word(1, false));
                    self.after_cursor_change(cx);
                    cx.stop_propagation();
                    return;
                }
                "up" if self.can_edit() => {
                    if self.edit_buffer(|buffer| buffer.move_lines(-1)) {
                        self.after_edit(cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                "down" if self.can_edit() => {
                    if self.edit_buffer(|buffer| buffer.move_lines(1)) {
                        self.after_edit(cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                "enter" if self.can_edit() => {
                    let below = !keystroke.modifiers.shift;
                    if self.edit_buffer(|buffer| buffer.insert_line(below)) {
                        self.after_edit(cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                "k" if self.can_edit() && keystroke.modifiers.shift => {
                    if self.edit_buffer(|buffer| buffer.delete_lines()) {
                        self.after_edit(cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }
        if self.cheat_sheet_visible && keystroke.key == "escape" {
            self.cheat_sheet_visible = false;
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if self.composition.is_some() && matches!(keystroke.key.as_str(), "enter" | "return") {
            cx.stop_propagation();
            return;
        }
        if command && keystroke.key == "enter" {
            if self.request_apply(window, cx) {
                cx.stop_propagation();
            }
            return;
        }
        let shift = keystroke.modifiers.shift;
        let page_rows = self.page_rows;
        let mut edited = false;
        let mut cursor_changed = false;
        let handled = if command {
            match keystroke.key.as_str() {
                "a" => {
                    self.with_buffer(|buffer| buffer.select_all());
                    cursor_changed = true;
                    true
                }
                "c" => {
                    self.copy(cx);
                    true
                }
                "x" if self.can_edit() => {
                    edited = self.cut(cx);
                    true
                }
                "v" if self.can_edit() => {
                    edited = self.paste(cx);
                    true
                }
                "z" if self.can_edit() => {
                    edited = if shift {
                        self.edit_buffer(|buffer| buffer.redo())
                    } else {
                        self.edit_buffer(|buffer| buffer.undo())
                    };
                    true
                }
                "d" if self.can_edit() => {
                    edited = self.edit_buffer(|buffer| buffer.duplicate_lines());
                    true
                }
                "home" => {
                    self.with_buffer(|buffer| buffer.move_document_start(shift));
                    cursor_changed = true;
                    true
                }
                "end" => {
                    self.with_buffer(|buffer| buffer.move_document_end(shift));
                    cursor_changed = true;
                    true
                }
                "slash" => {
                    self.cheat_sheet_visible = !self.cheat_sheet_visible;
                    true
                }
                _ => false,
            }
        } else {
            match keystroke.key.as_str() {
                "backspace" if self.can_edit() => {
                    edited = self.edit_buffer(|buffer| buffer.backspace());
                    true
                }
                "delete" if self.can_edit() => {
                    edited = self.edit_buffer(|buffer| buffer.delete_forward());
                    true
                }
                "enter" if self.can_edit() => {
                    edited = self.edit_buffer(|buffer| buffer.insert_newline());
                    true
                }
                "left" => {
                    self.with_buffer(|buffer| buffer.move_horizontal(-1, shift));
                    cursor_changed = true;
                    true
                }
                "right" => {
                    self.with_buffer(|buffer| buffer.move_horizontal(1, shift));
                    cursor_changed = true;
                    true
                }
                "up" => {
                    self.with_buffer(|buffer| buffer.move_vertical(-1, shift));
                    cursor_changed = true;
                    true
                }
                "down" => {
                    self.with_buffer(|buffer| buffer.move_vertical(1, shift));
                    cursor_changed = true;
                    true
                }
                "home" => {
                    self.with_buffer(|buffer| buffer.move_line_start(shift));
                    cursor_changed = true;
                    true
                }
                "end" => {
                    self.with_buffer(|buffer| buffer.move_line_end(shift));
                    cursor_changed = true;
                    true
                }
                "pageup" => {
                    self.with_buffer(|buffer| buffer.move_page(-1, page_rows, shift));
                    cursor_changed = true;
                    true
                }
                "pagedown" => {
                    self.with_buffer(|buffer| buffer.move_page(1, page_rows, shift));
                    cursor_changed = true;
                    true
                }
                _ => {
                    // Only printable keys are text. The keymap owns Tab and gives it to
                    // the editor as an action, but a keymap without that binding would
                    // otherwise type the tab character gpui puts in the keystroke, and a
                    // tab in the indentation makes the document invalid YAML.
                    let typed = keystroke.key_char.as_deref().filter(|text| {
                        self.can_edit()
                            && !keystroke.modifiers.alt
                            && !text.chars().any(char::is_control)
                    });
                    match typed {
                        Some(text) if !text.is_empty() => {
                            edited = self.type_text(text);
                            true
                        }
                        _ => false,
                    }
                }
            }
        };
        if handled {
            cx.stop_propagation();
            if edited {
                self.after_edit(cx);
            } else if cursor_changed {
                self.after_cursor_change(cx);
            } else {
                cx.notify();
            }
        }
    }

    fn search_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.search_focused(window, cx) {
            return;
        }
        let keystroke = &event.keystroke;
        let command = keystroke.modifiers.secondary();
        if keystroke.modifiers.alt {
            match keystroke.key.as_str() {
                // Alt+H, not Ctrl+H: macOS owns the secondary key for Hide.
                "h" => {
                    self.focus_replace(window, cx);
                    cx.stop_propagation();
                    return;
                }
                "c" => {
                    self.toggle_case_sensitive(cx);
                    cx.stop_propagation();
                    return;
                }
                "r" => {
                    self.toggle_regex(cx);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }
        if command {
            match keystroke.key.as_str() {
                "c" | "x" => self.remember_search_selection(window, cx),
                "y" | "u" | "k" | "w" => {
                    self.edit_search_command(keystroke.key.as_str(), window, cx);
                    cx.stop_propagation();
                }
                "enter" if self.replace_focused(window, cx) => {
                    self.replace_all_matches(cx);
                    cx.stop_propagation();
                }
                _ => {}
            }
            return;
        }
        match keystroke.key.as_str() {
            "escape" => {
                self.close_search(window, cx);
                cx.stop_propagation();
            }
            "enter" | "return" => {
                cx.stop_propagation();
                let delta = if keystroke.modifiers.shift { -1 } else { 1 };
                if self.replace_focused(window, cx) {
                    // The replace field owns Enter: replace, then walk to the next match.
                    self.replace_current(delta, cx);
                } else {
                    self.jump(delta, window, cx);
                }
            }
            "tab" => {
                // Tab walks between the find and replace fields instead of leaving the bar.
                let focus = if self.replace_focused(window, cx) {
                    self.search_input.read(cx).focus_handle(cx)
                } else {
                    self.replace_visible = true;
                    self.replace_input.read(cx).focus_handle(cx)
                };
                window.focus(&focus, cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    fn search_bar(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let find = h_flex()
            .id("yaml-search")
            .flex_none()
            .w_full()
            .min_w(px(0.))
            .h(design::size::CONTROL)
            .px(space::SM)
            .gap(space::XS)
            .items_center()
            .child(self.search_toggles(cx))
            .child(self.search_input.clone())
            .children(self.match_count(cx))
            .child(
                IconButton::new("yaml-search-prev", IconName::ChevronUp)
                    .icon_size(IconSize::XSmall)
                    .aria_label("Previous Match")
                    .tooltip(Tooltip::text("Previous Match (Shift+Enter)"))
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.jump(-1, window, cx)),
                    ),
            )
            .child(
                IconButton::new("yaml-search-next", IconName::ChevronDown)
                    .icon_size(IconSize::XSmall)
                    .aria_label("Next Match")
                    .tooltip(Tooltip::text("Next Match (Enter)"))
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.jump(1, window, cx)),
                    ),
            )
            .child(
                IconButton::new("yaml-search-replace", IconName::Replace)
                    .icon_size(IconSize::XSmall)
                    .aria_label("Show Replace Field")
                    .tooltip(Tooltip::text("Replace (Alt+H)"))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.focus_replace(window, cx)
                    })),
            )
            .child(
                IconButton::new("yaml-search-close", IconName::Close)
                    .icon_size(IconSize::XSmall)
                    .aria_label("Close Search")
                    .tooltip(Tooltip::text("Close Search (Esc)"))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.close_search(window, cx)
                    })),
            );
        let mut bar = v_flex()
            .id("yaml-search-bar")
            .debug_selector(|| "yaml-search-bar".to_owned())
            .flex_none()
            .w_full()
            .min_w(px(0.))
            .gap(space::XS)
            .pb(space::XS)
            .capture_key_down(cx.listener(Self::search_key_down))
            .child(find);
        if self.replace_visible {
            bar = bar.child(
                h_flex()
                    .id("yaml-replace")
                    .flex_none()
                    .w_full()
                    .min_w(px(0.))
                    .h(design::size::CONTROL)
                    .px(space::SM)
                    .gap(space::XS)
                    .items_center()
                    .child(self.replace_input.clone())
                    .child(
                        Button::new("yaml-replace-one", "Replace")
                            .size(ButtonSize::Compact)
                            .tooltip(Tooltip::text("Replace this match (Enter)"))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.replace_current(1, cx)
                            })),
                    )
                    .child(
                        Button::new("yaml-replace-all", "All")
                            .size(ButtonSize::Compact)
                            .tooltip(Tooltip::text("Replace every match (Ctrl+Enter)"))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.replace_all_matches(cx)
                            })),
                    ),
            );
        }
        bar.into_any_element()
    }

    /// Case-sensitivity and regular-expression toggles, each with its own shortcut.
    fn search_toggles(&self, cx: &mut Context<Self>) -> AnyElement {
        let case_view = cx.weak_entity();
        let regex_view = cx.weak_entity();
        h_flex()
            .id("yaml-search-toggles")
            .flex_none()
            .gap(space::XS)
            .child(
                IconButton::new("yaml-search-case", IconName::CaseSensitive)
                    .icon_size(IconSize::XSmall)
                    .aria_label("Match Case")
                    .toggle_state(self.search_options.case_sensitive)
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                    .tooltip(Tooltip::text("Match Case (Alt+C)"))
                    .on_click(move |_, _, cx| {
                        case_view
                            .update(cx, |view, cx| view.toggle_case_sensitive(cx))
                            .ok();
                    }),
            )
            .child(
                IconButton::new("yaml-search-regex", IconName::Regex)
                    .icon_size(IconSize::XSmall)
                    .aria_label("Regular Expression")
                    .toggle_state(self.search_options.regex)
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                    .tooltip(Tooltip::text("Regular Expression (Alt+R)"))
                    .on_click(move |_, _, cx| {
                        regex_view.update(cx, |view, cx| view.toggle_regex(cx)).ok();
                    }),
            )
            .into_any_element()
    }

    fn match_count(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.query.is_empty() {
            return None;
        }
        let (text, color) = if let Some(reason) = &self.search_error {
            (reason.clone(), design::Severity::Error.marker(cx))
        } else if self.search_pending {
            ("Searching…".to_owned(), cx.theme().colors().text_muted)
        } else if self.matches.is_empty() {
            ("No Matches".to_owned(), design::Severity::Error.marker(cx))
        } else {
            (
                format!("{} of {}", self.active + 1, self.matches.len()),
                cx.theme().colors().text_muted,
            )
        };
        Some(
            div()
                .text_size(design::TEXT_SMALL)
                .text_color(color)
                .child(SharedString::from(text))
                .into_any_element(),
        )
    }

    fn offset_for_position(
        &self,
        row: usize,
        position: Point<Pixels>,
        window: &Window,
        cx: &App,
        typography: &DataTypography,
    ) -> usize {
        let Some(buffer) = &self.buffer else {
            return 0;
        };
        let buffer = buffer.borrow();
        if row >= buffer.line_count() {
            return buffer.cursor();
        }
        let line = buffer.line(row);
        let byte = self.byte_for_line_position(
            line.text(),
            buffer.line_count(),
            position,
            window,
            cx,
            typography,
        );
        buffer.line_start(row) + byte
    }

    fn line_geometry(
        &self,
        line: &str,
        line_count: usize,
        window: &Window,
        cx: &App,
        typography: &DataTypography,
    ) -> (LineWindow, Pixels, Option<ShapedLine>) {
        let offset = self.scroll.0.borrow().base_handle.offset();
        let viewport = self.viewport_bounds();
        let char_width = char_width(typography);
        let visible = line_window(
            line,
            first_visible_char(offset.x, char_width),
            LINE_WINDOW_CHARS,
        );
        let text_left = viewport.origin.x
            + offset.x
            + gutter_width(line_count, char_width)
            + char_width * visible.cells as f32;
        let shaped = (visible.bytes.start < visible.bytes.end).then(|| {
            shape_line_range(
                window,
                line,
                visible.bytes.clone(),
                &typography.font,
                typography.size,
                cx.theme().colors().editor_foreground,
            )
        });
        (visible, text_left, shaped)
    }

    fn byte_for_line_position(
        &self,
        line: &str,
        line_count: usize,
        position: Point<Pixels>,
        window: &Window,
        cx: &App,
        typography: &DataTypography,
    ) -> usize {
        let (visible, text_left, shaped) =
            self.line_geometry(line, line_count, window, cx, typography);
        let Some(shaped) = shaped else {
            return 0;
        };
        let window_len = visible.bytes.end - visible.bytes.start;
        let relative = position.x - text_left;
        let mut shaped_byte = if relative >= shaped.width() {
            window_len
        } else {
            shaped.index_for_x(relative).unwrap_or(0)
        };
        // `shaped.index_for_x` takes the character in front of a boundary, and the caret
        // for the character behind it is painted exactly on that boundary, so a click on
        // a caret has to land on that character rather than the one before it. Both
        // numbers are f32 pixels measured from the text origin, and the pointer arrives
        // as an absolute pixel, so the subtraction rounds and a click on the boundary can
        // arrive a rounding step in front of it: the comparison gets those steps back.
        // Eight ulps of the origin stays under a tenth of a pixel at the widest offset
        // this editor scrolls to, and far above what the subtraction costs.
        let rounding = text_left.abs() * f32::EPSILON * 8.;
        if shaped_byte < window_len
            && text_left + shaped.x_for_index(shaped_byte + 1) <= position.x + rounding
        {
            shaped_byte += 1;
        }
        let byte = visible
            .bytes
            .start
            .saturating_add(shaped_byte)
            .min(line.len());
        floor_char_boundary(line, byte)
    }

    fn x_for_line_byte(
        &self,
        line: &str,
        line_count: usize,
        byte: usize,
        window: &Window,
        cx: &App,
        typography: &DataTypography,
    ) -> Pixels {
        let (visible, text_left, shaped) =
            self.line_geometry(line, line_count, window, cx, typography);
        let byte = floor_char_boundary(line, byte.min(line.len()));
        let advance = |column: usize| px(f32::from(char_width(typography)) * column as f32);
        if byte < visible.bytes.start {
            return text_left - advance(visible.cells) + advance(cell_column(line, byte));
        }
        let Some(shaped) = shaped else {
            return text_left + advance(cell_column(line, byte));
        };
        if byte >= visible.bytes.end {
            return text_left
                + shaped.width()
                + advance(cell_column(line, byte) - cell_column(line, visible.bytes.end));
        }
        text_left + shaped.x_for_index(byte - visible.bytes.start)
    }

    fn row_for_position(&self, y: Pixels, typography: &DataTypography) -> Option<usize> {
        let line_count = self.buffer.as_ref()?.borrow().line_count();
        self.row_for_line_count(y, typography, line_count)
    }

    fn row_for_line_count(
        &self,
        y: Pixels,
        typography: &DataTypography,
        line_count: usize,
    ) -> Option<usize> {
        let viewport = self.viewport_bounds();
        let offset = self.scroll.0.borrow().base_handle.offset();
        let relative = y - viewport.origin.y - offset.y;
        let content_height = (viewport.size.height - SCROLLBAR_HIT_PADDING).max(px(0.));
        if relative < px(0.) || relative >= content_height {
            return None;
        }
        let row = (relative / typography.line_height).floor() as usize;
        (row < line_count).then_some(row)
    }

    /// Row for an active drag, clamped to the document.
    ///
    /// A drag keeps extending the selection after the pointer leaves the text area:
    /// above the first line it selects to the first row, below the last line to the
    /// last row. Without the clamp the selection would freeze where the pointer left.
    fn row_for_drag(&self, y: Pixels, typography: &DataTypography) -> Option<usize> {
        let line_count = self.buffer.as_ref()?.borrow().line_count();
        let viewport = self.viewport_bounds();
        if (viewport.size.height - SCROLLBAR_HIT_PADDING) <= px(0.) {
            return Some(0);
        }
        let offset = self.scroll.0.borrow().base_handle.offset();
        let row = (y - viewport.origin.y - offset.y) / typography.line_height;
        Some((row.floor().max(0.) as usize).min(line_count.saturating_sub(1)))
    }

    fn empty_document_body(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let focused = self.document_focused(window, cx);
        let show_cursor = caret_visible(
            self.can_edit() && focused,
            self.caret_blink_visible,
            self.reduce_motion,
        );
        let typography = DataTypography::from_theme_settings(cx);
        let line_height = typography.line_height;
        let char_width = char_width(&typography);
        let gutter = gutter_width(1, char_width);
        let mut row = h_flex()
            .relative()
            .h(line_height)
            .w(gutter + CURSOR_WIDTH)
            .flex_none()
            .font(typography.font)
            .font_features(typography.features)
            .text_size(typography.size)
            .line_height(line_height)
            .child(
                h_flex()
                    .absolute()
                    .top_0()
                    .left_0()
                    .h_full()
                    .w(gutter)
                    .justify_end()
                    .pr(space::SM)
                    .bg(colors.editor_gutter_background)
                    .text_color(design::text_on(
                        colors.editor_gutter_background,
                        colors.editor_active_line_number,
                        colors.text,
                    ))
                    .child("1"),
            );
        if show_cursor {
            row = row.child(
                div()
                    .absolute()
                    .left(gutter)
                    .top_0()
                    .h(line_height)
                    .w(CURSOR_WIDTH)
                    .bg(colors.text_accent),
            );
        }
        let focus = self.focus_handle.clone();
        let view = cx.entity();
        div()
            .relative()
            .size_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus_handle, cx);
                    this.show_caret();
                    cx.notify();
                }),
            )
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, cx| {
                        window.handle_input(
                            &focus,
                            ElementInputHandler::new(bounds, view.clone()),
                            cx,
                        );
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .child(row)
            .into_any_element()
    }

    fn body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(buffer) = self.buffer.clone() else {
            if self.can_edit() && self.document_focused(window, cx) {
                return self.empty_document_body(window, cx);
            }
            return crate::panels::empty_state(
                IconName::FileCode,
                YAML_EMPTY_TITLE,
                YAML_EMPTY_HINT,
            );
        };
        let colors = cx.theme().colors();
        let typography = DataTypography::from_theme_settings(cx);
        let row_height = typography.line_height;
        let char_width = char_width(&typography);
        let text_size = typography.size;

        // The buffer already knows where the caret is, so the common case avoids
        // materialising the display text and rescanning it from the start of the document.
        let (cursor_row, cursor_line_byte) = if self.composition.is_some() {
            let display_text = self.display_text();
            let display_cursor = string_utf16_to_byte(&display_text, self.display_cursor_utf16());
            display_position(&display_text, display_cursor)
        } else {
            let buffer = buffer.borrow();
            buffer.cursor_in_line()
        };
        let (line_count, longest, gutter, selection) = {
            let buffer = buffer.borrow();
            (
                buffer.line_count(),
                buffer.longest_line(),
                gutter_width(buffer.line_count(), char_width),
                buffer.selection(),
            )
        };
        let cursor_line = self.can_edit() && self.document_focused(window, cx);
        let show_cursor = caret_visible(cursor_line, self.caret_blink_visible, self.reduce_motion);

        let viewport = self.scroll.0.borrow().base_handle.bounds().size;
        if viewport.height > px(0.) {
            self.page_rows = (viewport.height / row_height).floor().max(1.) as usize;
        }

        let by_line = self.by_line.clone();
        let scroll = self.scroll.clone();
        let base = self.scroll.0.borrow().base_handle.clone();
        let buffer_for_rows = buffer.clone();
        let cursor_color = colors.text_accent;
        let editor_background = colors.editor_background;
        let editor_foreground = colors.editor_foreground;
        let selection_background = design::text_selection::background(cx);
        let increased_contrast = crate::settings::increase_contrast_enabled(cx);
        let muted = colors.text_muted;
        let gutter_background = colors.editor_gutter_background;
        let line_number = design::text_on(gutter_background, colors.editor_line_number, muted);
        let active_line_number = design::text_on(
            gutter_background,
            colors.editor_active_line_number,
            colors.text,
        );
        let active_line_background = design::editor_wash::active_line_overlay(cx);
        let match_background =
            design::increased_contrast_surface(cx, colors.search_match_background);
        let active_match_background =
            design::increased_contrast_surface(cx, colors.search_active_match_background);
        let row_font = typography.font.clone();
        let row_features = typography.features.clone();
        let composition = self.composition.clone();
        let diagnostics = self.diagnostics.clone();
        let diagnostics_by_line = self.diagnostics_by_line.clone();
        let diagnostic_background = design::editor_wash::diagnostic_overlay(cx);
        // Theme tokens the editor paints with: indent guides, the change rail, and the
        // document highlight.
        let wrap_guide = colors.editor_wrap_guide;
        let active_wrap_guide = colors.editor_active_wrap_guide;
        let change_rail =
            design::increased_contrast_surface(cx, colors.editor_diff_hunk_added_background);
        let change_rail_hollow = colors.editor_diff_hunk_added_hollow_border;
        let block_background = design::increased_contrast_surface(
            cx,
            colors.editor_document_highlight_read_background,
        );
        let bracket_background = design::increased_contrast_surface(
            cx,
            colors.editor_document_highlight_bracket_background,
        );
        let block_highlight = self.block_highlight.clone();
        let bracket_highlight = self.bracket_highlight;
        let active_indent_cells = if block_highlight
            .as_ref()
            .is_some_and(|block| block.contains(&cursor_row))
        {
            indent_cells(buffer.borrow().line(cursor_row).text())
        } else {
            0
        };

        let list = uniform_list("yaml-lines", line_count, move |range, window, cx| {
            let buffer = buffer_for_rows.borrow();
            let scroll_x = scroll.0.borrow().base_handle.offset().x;
            range
                .map(|index| {
                    let line = buffer.line(index);
                    let hits = by_line.get(&index).map_or(&[][..], |hits| hits.as_slice());
                    let line_start = buffer.line_start(index);
                    let line_end = buffer.line_end(index);
                    let visible = line_window(
                        line.text(),
                        first_visible_char(scroll_x, char_width),
                        LINE_WINDOW_CHARS,
                    );
                    let visible_range = visible.bytes.clone();
                    // Grid columns count a fullwidth character as two cells, so the text
                    // after one still lands on the guides and the caret. The count stays
                    // inside the window, so a long line does not rescan its prefix.
                    let cell_in = |byte: usize| {
                        visible.cells + window_cells(line.text(), visible_range.start, byte)
                    };
                    let visible_start = visible.cells;
                    let visible_end = cell_in(visible_range.end);
                    let full_width = logical_line_width(gutter, line.cells(), char_width);
                    let windowed = visible_range.end < line.text().len();
                    let diagnostic = diagnostics_by_line
                        .get(&index)
                        .and_then(|&position| diagnostics.get(position));
                    let composition_here = composition.as_ref().filter(|composition| {
                        if composition.range.start == composition.range.end {
                            composition.range.start <= line_end
                                && composition.range.end >= line_start
                        } else {
                            composition.range.start < line_end && composition.range.end > line_start
                        }
                    });
                    let cursor_line_here = cursor_line && index == cursor_row;
                    let in_block = block_highlight
                        .as_ref()
                        .is_some_and(|block| block.contains(&index));
                    let mut line_background = if cursor_line_here {
                        design::composite_surface(editor_background, active_line_background)
                    } else {
                        editor_background
                    };
                    if in_block {
                        line_background =
                            design::composite_surface(line_background, block_background);
                    }
                    let line_background = if diagnostic.is_some() {
                        design::composite_surface(line_background, diagnostic_background)
                    } else {
                        line_background
                    };
                    let selection_foreground =
                        design::text_selection::foreground_on(cx, line_background);
                    let cursor_here = show_cursor && cursor_line_here;
                    let line_indent = indent_cells(line.text());
                    let guide_count = guide_columns(line_indent);
                    let modified = buffer.line_is_modified(index);
                    let bracket_here =
                        bracket_highlight.is_some_and(|byte| byte >= line_start && byte < line_end);

                    let needs_metrics = cursor_here || composition_here.is_some() || bracket_here;
                    let shaped = needs_metrics.then(|| {
                        shape_line_range(
                            window,
                            line.text(),
                            visible_range.clone(),
                            &row_font,
                            text_size,
                            cursor_color,
                        )
                    });

                    let mut row = h_flex()
                        .relative()
                        .h(row_height)
                        .w(full_width)
                        .flex_none()
                        .whitespace_nowrap()
                        .font(row_font.clone())
                        .font_features(row_features.clone())
                        .text_size(text_size)
                        .line_height(row_height);
                    if cursor_line_here {
                        row = row.child(
                            div()
                                .absolute()
                                .left(-scroll_x)
                                .top_0()
                                .h(row_height)
                                .w(viewport.width)
                                .bg(active_line_background),
                        );
                    }

                    if in_block {
                        row = row.child(
                            div()
                                .absolute()
                                .left(-scroll_x)
                                .top_0()
                                .h(row_height)
                                .w(viewport.width)
                                .bg(block_background),
                        );
                    }

                    if diagnostic.is_some() {
                        row = row.child(
                            div()
                                .absolute()
                                .left(-scroll_x)
                                .top_0()
                                .h(row_height)
                                .w(viewport.width)
                                .bg(diagnostic_background),
                        );
                    }

                    // Indent guides, one per nesting level the line is inside, with the
                    // level the caret sits in drawn in the active token colour.
                    for level in 1..=guide_count {
                        let cells = level * INDENT_CELLS;
                        let active_level = cells == active_indent_cells;
                        row = row.child(
                            div()
                                .absolute()
                                .left(gutter + char_width * cells as f32)
                                .top_0()
                                .h(row_height)
                                .w(px(1.))
                                .bg(if active_level {
                                    active_wrap_guide
                                } else {
                                    wrap_guide
                                }),
                        );
                    }

                    if bracket_here
                        && let Some(shaped) = shaped.as_ref()
                        && let Some(byte) = bracket_highlight
                    {
                        let relative = (byte - line_start)
                            .saturating_sub(visible_range.start)
                            .min(visible_range.end.saturating_sub(visible_range.start));
                        let x = shaped.x_for_index(relative);
                        let width = (shaped.x_for_index(
                            (relative + 1).min(visible_range.end - visible_range.start),
                        ) - x)
                            .max(px(2.));
                        row = row.child(
                            div()
                                .absolute()
                                .left(gutter + char_width * visible.cells as f32 + x)
                                .top_0()
                                .h(row_height)
                                .w(width)
                                .bg(bracket_background),
                        );
                    }

                    let mut composition_caret_x = None;
                    if let Some(composition) = composition_here
                        && let Some(shaped) = shaped.as_ref()
                    {
                        // Composition offsets are document offsets, so map them into the
                        // shaped window of this line before measuring.
                        let window_doc_start = line_start + visible_range.start;
                        let window_doc_end = line_start + visible_range.end;
                        let visible_composition_start = composition
                            .range
                            .start
                            .clamp(window_doc_start, window_doc_end);
                        let from = visible_composition_start - line_start;
                        let x0 = char_width * visible_start as f32
                            + shaped.x_for_index(from.saturating_sub(visible_range.start));
                        if cursor_line_byte >= visible_composition_start
                            && cursor_line_byte <= composition.range.start + composition.text.len()
                        {
                            let relative = floor_char_boundary(
                                &composition.text,
                                (cursor_line_byte - visible_composition_start)
                                    .min(composition.text.len()),
                            );
                            composition_caret_x = Some(
                                gutter
                                    + x0
                                    + char_width
                                        * composition.text[..relative].chars().count() as f32,
                            );
                        }
                        let width = char_width * composition.text.chars().count() as f32;
                        row = row.child(
                            div()
                                .absolute()
                                .left(gutter + x0)
                                .top_0()
                                .text_color(cursor_color)
                                .child(SharedString::from(composition.text.clone())),
                        );
                        row = row.child(
                            div()
                                .absolute()
                                .left(gutter + x0)
                                .bottom_0()
                                .h(px(2.))
                                .w(width)
                                .bg(cursor_color),
                        );
                    }

                    for segment in line_segments_window(
                        line.text(),
                        line.tokens(),
                        hits,
                        visible_range.clone(),
                    ) {
                        let segment_start = segment.start();
                        let segment_end = segment_start + segment.text().len();
                        let segment_span = segment_start..segment_end;
                        for (range, selected) in split_selection_spans(
                            std::slice::from_ref(&segment_span),
                            selection.as_ref(),
                        ) {
                            let mut span = div().flex_none().h_full();
                            if selected {
                                span = span
                                    .bg(selection_background)
                                    .text_color(selection_foreground);
                            } else {
                                let match_overlay = if segment.matched {
                                    Some(if segment.active {
                                        active_match_background
                                    } else {
                                        match_background
                                    })
                                } else {
                                    None
                                };
                                let segment_background = match_overlay
                                    .map_or(line_background, |overlay| {
                                        design::composite_surface(line_background, overlay)
                                    });
                                span = span.text_color(design::text_on_for_mode(
                                    segment_background,
                                    token_color(cx, segment.kind),
                                    editor_foreground,
                                    increased_contrast,
                                ));

                                if let Some(overlay) = match_overlay {
                                    span = span.bg(overlay);
                                }
                            }
                            let x = gutter + char_width * cell_in(range.start) as f32;
                            row = row.child(
                                span.absolute()
                                    .left(x)
                                    .top_0()
                                    .h(row_height)
                                    .child(SharedString::from(line.text()[range].to_owned())),
                            );
                        }
                    }
                    if let Some(selection) = selection.as_ref()
                        && selection.start <= line_end
                        && selection.end > line_end
                    {
                        row = row.child(
                            div()
                                .absolute()
                                .left(gutter + char_width * line.cells() as f32)
                                .top_0()
                                .h(row_height)
                                .w(NEWLINE_SELECTION_WIDTH)
                                .bg(selection_background),
                        );
                    }
                    if windowed {
                        row = row.child(
                            div()
                                .absolute()
                                .left(gutter + char_width * visible_end as f32)
                                .top_0()
                                .text_color(muted)
                                .child("…"),
                        );
                    }

                    if cursor_here {
                        let column = cell_column(line.text(), cursor_line_byte);
                        let x = composition_caret_x.unwrap_or(gutter + char_width * column as f32);
                        row = row.child(
                            div()
                                .absolute()
                                .left(x)
                                .top_0()
                                .h(row_height)
                                .w(CURSOR_WIDTH)
                                .bg(cursor_color),
                        );
                    }

                    let number_color = if cursor_line_here {
                        active_line_number
                    } else {
                        line_number
                    };
                    let row = row.child(
                        h_flex()
                            .absolute()
                            .top_0()
                            .left(-scroll_x)
                            .h_full()
                            .w(gutter)
                            .flex_none()
                            .justify_end()
                            .pr(space::SM)
                            .bg(gutter_background)
                            .text_color(number_color)
                            .child(SharedString::from((index + 1).to_string())),
                    );
                    // The change rail sits on the text edge of the gutter, so the error
                    // marker on its left edge and the rail never overlap. The caret row
                    // uses the hollow token, which at rail width reads as an outline.
                    let row = if modified {
                        row.child(
                            div()
                                .absolute()
                                .left(-scroll_x + gutter - GUTTER_RAIL)
                                .top_0()
                                .h(row_height)
                                .w(GUTTER_RAIL)
                                .bg(if cursor_line_here {
                                    change_rail_hollow
                                } else {
                                    change_rail
                                }),
                        )
                    } else {
                        row
                    };
                    match diagnostic {
                        Some(diagnostic) => row
                            .child(
                                div()
                                    .absolute()
                                    .left(-scroll_x)
                                    .top_0()
                                    .h(row_height)
                                    .w(GUTTER_RAIL)
                                    .bg(Severity::Error.marker_on(cx, line_background)),
                            )
                            .id(("yaml-error-line", index))
                            .tooltip(Tooltip::text(diagnostic.message.clone()))
                            .into_any_element(),
                        None => row.into_any_element(),
                    }
                })
                .collect::<Vec<_>>()
        })
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .with_width_from_item(Some(longest))
        .track_scroll(&self.scroll)
        .size_full();

        let origin = self.list_origin.clone();
        div()
            .relative()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .on_children_prepainted(move |bounds, _window, _cx| {
                if let Some(bounds) = bounds.first() {
                    origin.set(bounds.origin);
                }
            })
            .child(
                div()
                    .id("yaml-scroll")
                    .size_full()
                    .custom_scrollbars(
                        Scrollbars::new(ScrollAxes::Both)
                            .style(ScrollbarStyle::Editor)
                            .tracked_scroll_handle(&base),
                        window,
                        cx,
                    )
                    .pr(SCROLLBAR_HIT_PADDING)
                    .pb(SCROLLBAR_HIT_PADDING)
                    .child(
                        canvas(|_, _, _| (), {
                            let view = cx.entity();
                            move |bounds, _, window, cx| {
                                let focus = view.read(cx).focus_handle.clone();
                                window.handle_input(
                                    &focus,
                                    ElementInputHandler::new(bounds, view.clone()),
                                    cx,
                                );
                            }
                        })
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    )
                    .child(list),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    let typography = DataTypography::from_theme_settings(cx);
                    let Some(row) = this.row_for_position(event.position.y, &typography) else {
                        return;
                    };
                    window.focus(&this.focus_handle, cx);
                    let offset =
                        this.offset_for_position(row, event.position, window, cx, &typography);
                    this.dragging = true;
                    if this.composition.take().is_some() {
                        this.invalidate_display();
                    }
                    // A double click takes the word and a triple click the line, so a range
                    // is reachable without holding a modifier.
                    match click_target(event.click_count) {
                        ClickTarget::Line => this.select_line(row),
                        ClickTarget::Word => this.select_word(offset),
                        ClickTarget::Caret => {
                            this.with_buffer(|buffer| {
                                buffer.set_cursor(offset, event.modifiers.shift)
                            });
                        }
                    }
                    this.after_cursor_change(cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                if !this.dragging || event.pressed_button != Some(MouseButton::Left) {
                    return;
                }
                let typography = DataTypography::from_theme_settings(cx);
                // The drag clamps to the document instead of stopping at the text area,
                // so a selection can reach the first and last line.
                let Some(row) = this.row_for_drag(event.position.y, &typography) else {
                    return;
                };
                let offset = this.offset_for_position(row, event.position, window, cx, &typography);
                this.with_buffer(|buffer| buffer.set_cursor(offset, true));
                this.after_cursor_change(cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _window, _cx| {
                    this.dragging = false;
                }),
            )
            // Release the drag when the button comes up outside the editor, otherwise the
            // next hover would keep extending the selection.
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _window, _cx| {
                    this.dragging = false;
                }),
            )
            .into_any_element()
    }
}

impl EntityInputHandler for YamlView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if self.search_focused(window, cx) {
            return self.search_input.update(cx, |input, cx| {
                input.text_for_range(range_utf16, adjusted_range, window, cx)
            });
        }
        if self.composition.is_none() {
            let (value, actual) = self.buffer.as_ref().map_or_else(
                || (String::new(), 0..0),
                |buffer| buffer.borrow().slice_utf16(range_utf16.clone()),
            );
            if actual != range_utf16 {
                adjusted_range.replace(actual);
            }
            return Some(value);
        }
        let text = self.display_text();
        let (value, actual) = utf16_slice(&text, range_utf16.clone());
        if actual != range_utf16 {
            adjusted_range.replace(actual);
        }
        Some(value)
    }

    fn selected_text_range(
        &mut self,
        ignore_disabled_input: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        if self.search_focused(window, cx) {
            return self.search_input.update(cx, |input, cx| {
                input.selected_text_range(ignore_disabled_input, window, cx)
            });
        }
        if let Some(composition) = &self.composition {
            let marked_start = self.buffer.as_ref().map_or(0, |buffer| {
                buffer.borrow().byte_to_utf16(composition.range.start)
            });
            let marked_len = utf16_len(&composition.text);
            let (selected_start, selected_end) =
                composition
                    .selected
                    .as_ref()
                    .map_or((0, marked_len), |selected| {
                        (
                            string_byte_to_utf16(&composition.text, selected.start),
                            string_byte_to_utf16(&composition.text, selected.end),
                        )
                    });
            return Some(UTF16Selection {
                range: marked_start + selected_start..marked_start + selected_end,
                reversed: composition.selected_reversed,
            });
        }
        let Some(buffer) = &self.buffer else {
            return Some(UTF16Selection {
                range: 0..0,
                reversed: false,
            });
        };
        let buffer = buffer.borrow();
        let cursor = buffer.byte_to_utf16(buffer.cursor());
        let anchor = buffer.byte_to_utf16(buffer.anchor());
        Some(UTF16Selection {
            range: anchor.min(cursor)..anchor.max(cursor),
            reversed: cursor < anchor,
        })
    }

    fn marked_text_range(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        if self.search_focused(window, cx) {
            return self
                .search_input
                .update(cx, |input, cx| input.marked_text_range(window, cx));
        }
        let composition = self.composition.as_ref()?;
        let Some(buffer) = self.buffer.as_ref() else {
            return Some(0..utf16_len(&composition.text));
        };
        let buffer = buffer.borrow();
        let start = buffer.byte_to_utf16(composition.range.start);
        Some(start..start + utf16_len(&composition.text))
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_focused(window, cx) {
            self.search_input
                .update(cx, |input, cx| input.unmark_text(window, cx));
            return;
        }
        if self.composition.take().is_some() {
            self.invalidate_display();
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.search_focused(window, cx) {
            self.search_input.update(cx, |input, cx| {
                input.replace_text_in_range(range_utf16, text, window, cx);
            });
            return;
        }
        if !self.can_edit() {
            return;
        }
        // gpui hands a keystroke the editor did not consume over as text input, and the
        // Tab keystroke carries a tab character. A tab in the indentation makes the
        // document invalid YAML, and the editor indents through its own key, so text
        // input never carries one. The other keys that reach here are the ones the
        // document answers as keys, and an IME commit is what the user chose to type.
        if text.contains('\t') {
            return;
        }
        self.ensure_buffer();
        let requested = self
            .composition
            .as_ref()
            .map(|composition| composition.range.clone())
            .or_else(|| range_utf16.map(|range| self.utf16_range_to_bytes(range)));
        let changed = self.edit_buffer(|buffer| {
            let range = requested.unwrap_or_else(|| {
                buffer
                    .selection()
                    .unwrap_or(buffer.cursor()..buffer.cursor())
            });
            buffer.replace(range, text)
        });
        if changed {
            self.after_edit(cx);
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.search_focused(window, cx) {
            self.search_input.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(
                    range_utf16,
                    new_text,
                    new_selected_range,
                    window,
                    cx,
                );
            });
            return;
        }
        if !self.can_edit() {
            return;
        }
        self.ensure_buffer();
        let replace_unfocused_document =
            range_utf16.is_none() && !self.document_focused(window, cx);
        let range = self
            .composition
            .as_ref()
            .map(|composition| composition.range.clone())
            .or_else(|| range_utf16.map(|range| self.utf16_range_to_bytes(range)))
            .unwrap_or_else(|| {
                self.buffer
                    .as_ref()
                    .map(|buffer| {
                        let buffer = buffer.borrow();
                        if replace_unfocused_document && buffer.selection().is_none() {
                            0..buffer.byte_len()
                        } else {
                            buffer
                                .selection()
                                .unwrap_or(buffer.cursor()..buffer.cursor())
                        }
                    })
                    .unwrap_or(0..0)
            });
        let range = self
            .buffer
            .as_ref()
            .map(|buffer| buffer.borrow().clip_range(range))
            .unwrap_or(0..0);
        let selected_reversed = new_selected_range
            .as_ref()
            .is_some_and(|range| range.start > range.end);
        let selected = new_selected_range.map(|selected| {
            let start = string_utf16_to_byte(new_text, selected.start);
            let end = string_utf16_to_byte(new_text, selected.end);
            if start <= end { start..end } else { end..start }
        });
        self.composition = (!new_text.is_empty()).then(|| Composition {
            range,
            text: new_text.to_owned(),
            selected,
            selected_reversed,
        });
        self.invalidate_display();
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let typography = DataTypography::from_theme_settings(cx);
        let char_width = char_width(&typography);
        let display = self.display_text();
        let byte = string_utf16_to_byte(&display, range_utf16.end);
        let (row, line_start, line) = display_line_at(&display, byte)?;
        let line_count = display.split('\n').count();
        let viewport = self.viewport_bounds();
        let origin = if viewport.size.width > px(0.) {
            viewport.origin
        } else {
            element_bounds.origin
        };
        let x = if viewport.size.width > px(0.) {
            self.x_for_line_byte(line, line_count, byte - line_start, window, cx, &typography)
        } else {
            origin.x
                + gutter_width(line_count, char_width)
                + char_width * cell_column(line, byte - line_start) as f32
        };
        let offset = self.scroll.0.borrow().base_handle.offset();
        let y = origin.y + typography.line_height * row as f32 + offset.y;
        Some(Bounds::new(
            point(x, y),
            size(char_width, typography.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let typography = DataTypography::from_theme_settings(cx);
        let display = self.display_text();
        let line_count = display.split('\n').count();
        let row = self.row_for_line_count(point.y, &typography, line_count)?;
        let (line_start, line) = display_line_at_row(&display, row)?;
        let byte = self.byte_for_line_position(line, line_count, point, window, cx, &typography);
        Some(string_byte_to_utf16(&display, line_start + byte))
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.search_focused(window, cx) {
            self.search_input.update(cx, |input, cx| {
                input.set_selected_text_range(range_utf16, window, cx)
            });
            return;
        }
        let display = self.display_text();
        let reversed = range_utf16.start > range_utf16.end;
        let start = string_utf16_to_byte(&display, range_utf16.start);
        let end = string_utf16_to_byte(&display, range_utf16.end);
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let composition_selection = self.composition.as_ref().and_then(|composition| {
            let marked_start = composition.range.start;
            let marked_end = marked_start + composition.text.len();
            (start >= marked_start && end <= marked_end).then(|| {
                clip_byte_range(&composition.text, start - marked_start..end - marked_start)
            })
        });
        if let Some(selected) = composition_selection {
            let composition = self.composition.as_mut().unwrap();
            composition.selected = Some(selected);
            composition.selected_reversed = reversed;
            self.show_caret();
            cx.notify();
            return;
        }
        let document_range = self.buffer.as_ref().map(|_| {
            let start = self.document_byte_for_display_byte(start);
            let end = self.document_byte_for_display_byte(end);
            if start <= end { start..end } else { end..start }
        });
        if self.composition.take().is_some() {
            self.invalidate_display();
        }
        if let (Some(buffer), Some(range)) = (&self.buffer, document_range) {
            let mut buffer = buffer.borrow_mut();
            if reversed {
                buffer.set_cursor(range.end, false);
                buffer.set_cursor(range.start, true);
            } else {
                buffer.set_cursor(range.start, false);
                buffer.set_cursor(range.end, true);
            }
            drop(buffer);
            self.after_cursor_change(cx);
            return;
        }
        self.show_caret();
        cx.notify()
    }

    fn accepts_text_input(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.search_focused(window, cx) || self.can_edit()
    }

    fn text_length_utf16(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<usize> {
        if self.search_focused(window, cx) {
            return self
                .search_input
                .update(cx, |input, cx| input.text_length_utf16(window, cx));
        }
        Some(self.display_text_len_utf16())
    }

    fn text_input_configuration(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> TextInputConfiguration {
        TextInputConfiguration::default()
    }
}

impl YamlView {
    /// Marks the cached display text as stale.
    fn invalidate_display(&mut self) {
        self.view_generation = self.view_generation.wrapping_add(1);
        // The document moved, so the block and bracket highlights have to be found again.
        self.highlight_caret = None;
    }

    /// Document text as the input protocol sees it, including the IME preedit.
    ///
    /// The input protocol asks for the text on every caret, IME, and accessibility event,
    /// so the composed string is cached until the document or the composition changes.
    fn display_text(&mut self) -> Rc<String> {
        if let Some((generation, text)) = &self.display_cache
            && *generation == self.view_generation
        {
            return text.clone();
        }
        let text = Rc::new(match &self.buffer {
            Some(buffer) => {
                let text = buffer.borrow().text();
                match &self.composition {
                    Some(composition) => composed_text(&text, composition),
                    None => text,
                }
            }
            None => self
                .composition
                .as_ref()
                .map_or_else(String::new, |composition| composition.text.clone()),
        });
        self.display_cache = Some((self.view_generation, text.clone()));
        text
    }

    fn display_text_len_utf16(&self) -> usize {
        let Some(composition) = &self.composition else {
            return self
                .buffer
                .as_ref()
                .map_or(0, |buffer| buffer.borrow().text_len_utf16());
        };
        let Some(buffer) = &self.buffer else {
            return utf16_len(&composition.text);
        };
        let buffer = buffer.borrow();
        let replaced_start = buffer.byte_to_utf16(composition.range.start);
        let replaced_end = buffer.byte_to_utf16(composition.range.end);
        buffer.text_len_utf16() - (replaced_end - replaced_start) + utf16_len(&composition.text)
    }

    fn display_cursor_utf16(&self) -> usize {
        if let Some(composition) = &self.composition {
            let marked_start = self.buffer.as_ref().map_or(0, |buffer| {
                buffer.borrow().byte_to_utf16(composition.range.start)
            });
            let relative =
                composition
                    .selected
                    .as_ref()
                    .map_or(composition.text.len(), |selected| {
                        if composition.selected_reversed {
                            selected.start
                        } else {
                            selected.end
                        }
                    });
            return marked_start + string_byte_to_utf16(&composition.text, relative);
        }
        self.buffer.as_ref().map_or(0, |buffer| {
            let buffer = buffer.borrow();
            buffer.byte_to_utf16(buffer.cursor())
        })
    }

    fn utf16_range_to_bytes(&self, range: Range<usize>) -> Range<usize> {
        let Some(buffer) = self.buffer.as_ref() else {
            return 0..0;
        };
        let buffer = buffer.borrow();
        let start = buffer.utf16_to_byte(range.start);
        let end = buffer.utf16_to_byte(range.end);
        if start <= end { start..end } else { end..start }
    }

    fn document_byte_for_display_byte(&self, display_byte: usize) -> usize {
        let Some(buffer) = self.buffer.as_ref() else {
            return 0;
        };
        let buffer = buffer.borrow();
        let Some(composition) = &self.composition else {
            return buffer.clip_range(display_byte..display_byte).start;
        };
        let text = buffer.text();
        let display = composed_text(&text, composition);
        let display_byte = floor_char_boundary(&display, display_byte.min(display.len()));
        let marked_start = composition.range.start;
        let marked_end = marked_start + composition.text.len();
        if display_byte <= marked_start {
            display_byte.min(marked_start)
        } else if display_byte >= marked_end {
            floor_char_boundary(
                &text,
                (composition.range.end + display_byte - marked_end).min(text.len()),
            )
        } else if display_byte - marked_start <= composition.text.len() / 2 {
            marked_start
        } else {
            composition.range.end
        }
    }
}

/// What a click inside the document selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClickTarget {
    /// The caret only, or an extended selection with Shift.
    Caret,
    /// The word under the pointer.
    Word,
    /// The line under the pointer.
    Line,
}

/// A double click takes the word and a triple click takes the line.
fn click_target(click_count: usize) -> ClickTarget {
    match click_count {
        0 | 1 => ClickTarget::Caret,
        2 => ClickTarget::Word,
        _ => ClickTarget::Line,
    }
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn floor_char_boundary(text: &str, mut byte: usize) -> usize {
    byte = byte.min(text.len());
    while !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

fn string_byte_to_utf16(text: &str, byte: usize) -> usize {
    text[..floor_char_boundary(text, byte)]
        .encode_utf16()
        .count()
}

fn string_utf16_to_byte(text: &str, utf16: usize) -> usize {
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

fn clip_byte_range(text: &str, range: Range<usize>) -> Range<usize> {
    let start = floor_char_boundary(text, range.start.min(text.len()));
    let end = floor_char_boundary(text, range.end.min(text.len()));
    if start <= end { start..end } else { end..start }
}

fn clip_range_to_len(range: Range<usize>, length: usize) -> Range<usize> {
    let start = range.start.min(length);
    let end = range.end.min(length);
    if start <= end { start..end } else { end..start }
}

fn utf16_slice(text: &str, range: Range<usize>) -> (String, Range<usize>) {
    let start = string_utf16_to_byte(text, range.start);
    let end = string_utf16_to_byte(text, range.end).max(start);
    (
        text[start..end].to_owned(),
        string_byte_to_utf16(text, start)..string_byte_to_utf16(text, end),
    )
}

/// Document byte offset of a zero-based line and grid column.
///
/// The line index comes from the same line cache the editor renders and measures with, and
/// the column is a grid column, so a fullwidth character counts as two cells. A line past
/// the end of the document resolves to the document end, which keeps the caret in range
/// when a diagnostic points at text that has already been edited away.
fn byte_offset(buffer: &EditBuffer, line: usize, column: usize) -> usize {
    if line >= buffer.line_count() {
        return buffer.byte_len();
    }
    buffer.line_start(line) + byte_for_cell_column(buffer.line(line).text(), column)
}

fn display_line_at(text: &str, byte: usize) -> Option<(usize, usize, &str)> {
    let byte = floor_char_boundary(text, byte.min(text.len()));
    let prefix = &text[..byte];
    let row = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let line_start = prefix.rfind('\n').map_or(0, |index| index + 1);
    let line_end = text[line_start..]
        .find('\n')
        .map_or(text.len(), |index| line_start + index);
    Some((row, line_start, &text[line_start..line_end]))
}

fn display_line_at_row(text: &str, row: usize) -> Option<(usize, &str)> {
    let mut line_start = 0;
    for current in 0..=row {
        if line_start > text.len() {
            return None;
        }
        let line_end = text[line_start..]
            .find('\n')
            .map_or(text.len(), |index| line_start + index);
        if current == row {
            return Some((line_start, &text[line_start..line_end]));
        }
        line_start = line_end + 1;
    }
    None
}

fn display_position(text: &str, byte: usize) -> (usize, usize) {
    let Some((row, line_start, _)) = display_line_at(text, byte) else {
        return (0, 0);
    };
    (
        row,
        floor_char_boundary(text, byte.min(text.len())) - line_start,
    )
}

fn byte_for_char_column(text: &str, column: usize) -> usize {
    text.char_indices()
        .nth(column)
        .map_or(text.len(), |(byte, _)| byte)
}

fn previous_search_word_start(text: &str, cursor: usize) -> usize {
    let mut start = cursor.min(text.len());
    while start > 0 {
        let Some((index, ch)) = text[..start].char_indices().next_back() else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
        start = index;
    }
    while start > 0 {
        let Some((index, ch)) = text[..start].char_indices().next_back() else {
            break;
        };
        if ch.is_whitespace() {
            break;
        }
        start = index;
    }
    start
}

fn composed_text(text: &str, composition: &Composition) -> String {
    let mut result = text.to_owned();
    let range = clip_byte_range(&result, composition.range.clone());
    result.replace_range(range, &composition.text);
    result
}

/// The editor's own shortcut list, so the keys are discoverable without a manual.
///
/// The editor keys are handled in the view instead of the keymap, so they never appear
/// in the Settings keyboard panel. This panel is the in-product answer to that, and it
/// names every key the editor answers to.
const CHEAT_SHEET: &[(&str, &str)] = &[
    ("Tab / Shift+Tab", "Indent or outdent the selected lines"),
    ("Alt+Up / Alt+Down", "Move the selected lines up or down"),
    ("Alt+Enter", "Insert a line below"),
    ("Alt+Shift+Enter", "Insert a line above"),
    ("Ctrl+Shift+D", "Duplicate the selected lines"),
    ("Alt+Shift+K", "Delete the selected lines"),
    ("Alt+Left / Alt+Right", "Move by word"),
    (
        "Home / End",
        "Line start and end, then column zero and line end",
    ),
    ("Ctrl+Home / Ctrl+End", "Document start and end"),
    ("Ctrl+F", "Find"),
    ("Alt+H", "Replace"),
    ("Alt+C / Alt+R", "Match case, regular expression"),
    (
        "Enter / Shift+Enter",
        "Next or previous match; with the replace field, replace",
    ),
    ("Ctrl+Enter", "Replace every match"),
    ("Ctrl+Enter in the document", "Apply"),
    ("Ctrl+/", "Show or hide this list"),
    ("Escape", "Close the search or this list"),
    ("Ctrl+Tab", "Leave the editor"),
];

impl Render for YamlView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_caret_blink(window, cx);
        self.update_document_highlights();
        let colors = cx.theme().colors();
        // The display text is cached per document generation, so the accessibility value
        // no longer rebuilds the whole document on every frame.
        let accessible_text = self.display_text();
        let description = if self.can_edit() {
            "Editable YAML."
        } else {
            "Read-only YAML."
        };
        let description = UiKeyBinding::for_action_in(&Apply, &self.focus_handle, cx)
            .keyboard_shortcut_text(window, cx)
            .map_or_else(
                || description.to_owned(),
                |shortcut| format!("{description} Apply: {shortcut}."),
            );
        let editable = self.can_edit();
        let mut editor = v_flex()
            .id("yaml-editor")
            .relative()
            .size_full()
            .min_w(px(0.))
            .min_h(px(0.))
            .bg(colors.editor_background.alpha(1.0))
            .text_color(colors.editor_foreground)
            .track_focus(&self.focus_handle)
            .key_context("Editor")
            // The ring is always reserved and only recoloured, so taking and leaving
            // focus cannot slide the document sideways.
            .border_l_2()
            .border_color(colors.border_transparent)
            .focus_visible(|style| style.border_color(colors.border_focused))
            .role(Role::MultilineTextInput)
            .aria_label("YAML Editor")
            .aria_description(description)
            .aria_value(accessible_text.as_str())
            .on_action(cx.listener(Self::apply_action))
            .on_action(cx.listener(Self::undo_action))
            .on_action(cx.listener(Self::redo_action))
            .on_action(cx.listener(Self::cut_action))
            .on_action(cx.listener(Self::copy_action))
            .on_action(cx.listener(Self::paste_action))
            .on_action(cx.listener(Self::select_all_action))
            .on_action(cx.listener(Self::focus_next_action))
            .on_action(cx.listener(Self::focus_previous_action))
            .on_key_down(cx.listener(Self::key_down))
            .when(self.search_visible, |this| {
                this.child(self.search_bar(window, cx))
            });
        // The read-only marker and the shortcut button belong to the document, so they live in
        // the same layer as it. They used to be absolutely positioned against the whole editor,
        // which meant they floated over the search field as well as over the text.
        //
        // The layer is a column, not a `div()`. `body` fills its parent with `flex_1`, and
        // `flex_1` is `flex-basis: 0%` on whatever axis is the container's main one: a row layer
        // therefore sized the document on the width and left its height `auto`, the `size_full()`
        // scroll area below it could not resolve a percentage height against, and the whole
        // document collapsed to its scrollbar padding. Keeping the axis vertical is what gives
        // `body` a definite height to fill.
        editor = editor.child(
            v_flex()
                .id("yaml-body-layer")
                .debug_selector(|| "yaml-body-layer".to_owned())
                .relative()
                .flex_1()
                .min_w(px(0.))
                .min_h(px(0.))
                .child(self.body(window, cx))
                .child(self.status_corner(editable, cx)),
        );
        if self.cheat_sheet_visible {
            editor = editor.child(self.cheat_sheet(window, cx));
        }
        editor
    }
}

impl YamlView {
    /// The read-only marker and the shortcut-list button, in the top-right corner.
    ///
    /// Read-only used to be aria text only, so a sighted user could not tell an editable document
    /// from one that silently drops every keystroke.
    ///
    /// The band behind them is the point. They are absolutely positioned over the document, so
    /// without it the overlap was an accident: a chip and a button dropped on top of the text, in
    /// the light appearance the chip's own surface was the editor's, and all that separated it
    /// from the text was a hairline well under the interactive floor. A short opaque band in the
    /// editor's own colour, with an edge, makes the overlap a layer the reader can see - and it
    /// stops the controls from landing on the search field, because the whole band is now scoped to
    /// the document rather than to the editor.
    fn status_corner(&self, editable: bool, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let shortcuts = self.cheat_sheet_visible;
        let editor_background = colors.editor_background.alpha(1.0);
        // The chip and its glyph are graphics a pointer has to find, so both are solved against
        // the surface they are actually painted on rather than read from the chrome role.
        let chip_border = design::graphic_on(editor_background, colors.border);
        let chip_foreground = design::graphic_on(editor_background, colors.text_muted);
        h_flex()
            .id("yaml-status-corner")
            .debug_selector(|| "yaml-status-corner".to_owned())
            .absolute()
            .top_0()
            .right_0()
            .gap(space::XS)
            .items_center()
            .pl(space::MD)
            .pr(space::SM)
            .pt(space::XS)
            .pb(space::XS)
            .bg(editor_background)
            .border_l_1()
            .border_color(colors.border_variant)
            .when(!editable, |this| {
                this.child(
                    h_flex()
                        .id("yaml-read-only")
                        .gap(space::XS)
                        .rounded_sm()
                        .border_1()
                        .border_color(chip_border)
                        .bg(colors.elevated_surface_background)
                        .px(space::SM)
                        .py(px(2.))
                        .text_size(design::TEXT_SMALL)
                        .text_color(colors.text_muted)
                        .child(
                            Icon::new(IconName::Lock)
                                .size(IconSize::XSmall)
                                .color(Color::Custom(chip_foreground)),
                        )
                        .child("Read-only"),
                )
            })
            .child(
                IconButton::new("yaml-shortcuts", IconName::Keyboard)
                    .icon_size(IconSize::XSmall)
                    .aria_label("Editor Shortcuts")
                    .tooltip(Tooltip::text(if shortcuts {
                        "Hide editor shortcuts (Ctrl+/)"
                    } else {
                        "Editor shortcuts (Ctrl+/)"
                    }))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.cheat_sheet_visible = !this.cheat_sheet_visible;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    /// The shortcut list, centred over the document.
    ///
    /// It is sized against the window, not against the document: a fixed 520px height filled the
    /// editor on the smallest supported window, which is the opposite of what a reference sheet
    /// should do. The key column stays a fixed measure because it holds text, not layout.
    fn cheat_sheet(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let viewport = window.viewport_size();
        let max_height = cheat_sheet_height(viewport.height);
        let max_width = cheat_sheet_width(viewport.width);
        let colors = cx.theme().colors();
        let rows = CHEAT_SHEET.iter().map(|(keys, what)| {
            h_flex()
                .gap(space::SM)
                .child(
                    div()
                        .flex_none()
                        .w(px(CHEAT_SHEET_KEY_COLUMN))
                        .text_color(colors.text_muted)
                        .child(SharedString::from(*keys)),
                )
                .child(SharedString::from(*what))
        });
        v_flex()
            .id("yaml-cheat-sheet")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .items_center()
            .justify_center()
            .bg(colors.panel_overlay_background.opacity(0.4))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.cheat_sheet_visible = false;
                    cx.notify();
                }),
            )
            .child(
                v_flex()
                    .id("yaml-cheat-sheet-panel")
                    .w(max_width)
                    .max_w_full()
                    .max_h(max_height)
                    .overflow_y_scroll()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.elevated_surface_background)
                    .p(space::LG)
                    .gap(space::SM)
                    .text_size(design::TEXT_SMALL)
                    // The scrim closes the sheet, so the panel keeps its own clicks.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Label::new("Editor Shortcuts")
                            .size(LabelSize::Custom(rems_from_px(f32::from(design::TEXT)))),
                    )
                    .child(
                        div()
                            .text_color(colors.text_muted)
                            .child("Esc closes this list, and Ctrl+Tab leaves the editor."),
                    )
                    .children(rows),
            )
            .into_any_element()
    }
}

fn split_selection_spans(
    syntax_spans: &[Range<usize>],
    selection: Option<&Range<usize>>,
) -> Vec<(Range<usize>, bool)> {
    let Some(selection) = selection else {
        return syntax_spans
            .iter()
            .filter(|range| range.start < range.end)
            .cloned()
            .map(|range| (range, false))
            .collect();
    };
    let selection_start = selection.start.min(selection.end);
    let selection_end = selection.start.max(selection.end);
    let mut out = Vec::new();
    for span in syntax_spans {
        let span_start = span.start.min(span.end);
        let span_end = span.start.max(span.end);
        if span_start >= span_end {
            continue;
        }
        let selected_start = selection_start.max(span_start).min(span_end);
        let selected_end = selection_end.max(span_start).min(span_end);
        if selected_start >= selected_end {
            out.push((span_start..span_end, false));
            continue;
        }
        if span_start < selected_start {
            out.push((span_start..selected_start, false));
        }
        out.push((selected_start..selected_end, true));
        if selected_end < span_end {
            out.push((selected_end..span_end, false));
        }
    }
    out
}

fn caret_visible(focused: bool, blink_visible: bool, reduce_motion: bool) -> bool {
    focused && (reduce_motion || blink_visible)
}

fn first_visible_char(offset_x: Pixels, char_width: Pixels) -> usize {
    if offset_x >= px(0.) {
        return 0;
    }
    ((-offset_x) / char_width).floor() as usize
}

/// A bounded byte window of one line plus the display column of its first byte.
struct LineWindow {
    bytes: Range<usize>,
    /// Character column of `bytes.start`, counted from the start of the line.
    #[allow(dead_code)]
    column: usize,
    /// Grid cells of `bytes.start`, counting a fullwidth character as two.
    cells: usize,
}

/// Windows a long line for shaping and painting.
///
/// The window never splits a grapheme, holds at most `max_chars` characters, and the
/// scan cost stays proportional to the window instead of the line, so a 100k character
/// line costs the same as a short one. The margin keeps `first_char` inside the window.
fn line_window(line: &str, first_char: usize, max_chars: usize) -> LineWindow {
    // A line has at least one byte per character, so a byte bound larger than the wanted
    // window proves the line is long enough without counting it.
    let needed = first_char + max_chars + LINE_WINDOW_MARGIN;
    let total = if needed <= line.len() {
        needed
    } else {
        line.chars().count()
    };
    let start = if first_char == 0 {
        0
    } else {
        first_char.min(total).saturating_sub(LINE_WINDOW_MARGIN)
    };
    let end = start.saturating_add(max_chars).min(total);
    let start = grapheme_start(line, byte_for_char_column(line, start));
    // `end` is already the offset past the last wanted character, so it only has to round
    // down to a grapheme boundary. Rounding up with `grapheme_end` would pull one extra
    // character into the window and break the `max_chars` bound.
    let end = grapheme_start(line, byte_for_char_column(line, end));
    LineWindow {
        bytes: start..end.max(start),
        column: match_column(line, start),
        cells: cell_column(line, start),
    }
}

/// Leading whitespace of a line in grid cells, which is the nesting level the indent
/// guides and the document highlight are drawn from.
fn indent_cells(line: &str) -> usize {
    let mut cells = 0;
    for ch in line.chars() {
        match ch {
            ' ' => cells += 1,
            // A tab is not legal YAML indentation, but it still moves the caret.
            '\t' => cells += INDENT_CELLS,
            _ => break,
        }
    }
    cells
}

/// How many indent guides a line is inside, capped so a deep line cannot flood the row.
fn guide_columns(indent: usize) -> usize {
    (indent / INDENT_CELLS).min(MAX_INDENT_GUIDES)
}

fn closing_bracket(ch: char) -> Option<char> {
    match ch {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        _ => None,
    }
}

fn opening_bracket(ch: char) -> Option<char> {
    match ch {
        ')' => Some('('),
        ']' => Some('['),
        '}' => Some('{'),
        _ => None,
    }
}

/// Byte offset of the bracket or quote that pairs with the one next to `byte`.
///
/// The document highlight uses it to mark the pair the caret is inside. `None` means
/// there is no partner on this line, so nothing is painted.
fn matching_bracket(line: &str, byte: usize) -> Option<usize> {
    let byte = floor_char_boundary(line, byte.min(line.len()));
    let before = byte.checked_sub(1).filter(|at| line.is_char_boundary(*at));
    for start in [before, Some(byte)].into_iter().flatten() {
        let ch = line[start..].chars().next()?;
        if let Some(close) = closing_bracket(ch) {
            return scan_forward(line, start, ch, close);
        }
        if let Some(open) = opening_bracket(ch) {
            return scan_backward(line, start, ch, open);
        }
        if ch == '"' || ch == '\'' {
            return scan_quote(line, start, ch);
        }
    }
    None
}

fn scan_forward(line: &str, start: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0;
    for (index, ch) in line[start..].char_indices() {
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                return Some(start + index);
            }
        }
    }
    None
}

fn scan_backward(line: &str, start: usize, ch: char, open: char) -> Option<usize> {
    let mut depth = 0;
    for (index, found) in line[..start + ch.len_utf8()].char_indices().rev() {
        if found == ch {
            depth += 1;
        } else if found == open {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

/// The other quote of a pair on the same line, skipping an escaped one.
fn scan_quote(line: &str, start: usize, quote: char) -> Option<usize> {
    let mut index = start + quote.len_utf8();
    while index < line.len() {
        let ch = line[index..].chars().next()?;
        if ch == '\\' && quote == '"' {
            index += ch.len_utf8();
            index += line[index..].chars().next()?.len_utf8();
            continue;
        }
        if ch == quote {
            return Some(index);
        }
        index += ch.len_utf8();
    }
    None
}

fn shape_line_range(
    window: &Window,
    text: &str,
    range: Range<usize>,
    font: &Font,
    size: Pixels,
    color: Hsla,
) -> ShapedLine {
    let start = range.start.min(text.len());
    let end = range.end.min(text.len()).max(start);
    let text = &text[start..end];
    let run = TextRun {
        len: text.len(),
        font: font.clone(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(SharedString::from(text.to_owned()), size, &[run], None)
}

fn token_color(cx: &App, kind: TokenKind) -> gpui::Hsla {
    let fallback = cx.theme().colors().editor_foreground;
    let name = match kind {
        TokenKind::Plain => return fallback,
        TokenKind::Key => "property",
        TokenKind::String => "string",
        TokenKind::Number => "number",
        TokenKind::Bool => "boolean",
        TokenKind::Null => "constant",
        TokenKind::Comment => "comment",
        TokenKind::Punctuation => "punctuation",
        TokenKind::Anchor | TokenKind::Alias => "variant",
        TokenKind::Tag => "type",
    };
    cx.theme()
        .syntax()
        .style_for_name(name)
        .and_then(|style| style.color)
        .unwrap_or(fallback)
}

/// The grid unit: one character cell of the buffer font.
///
/// The font's own advance is used wherever a shaped line is available, so the caret and
/// the IME bounds follow the real glyphs. This ratio is the fallback for the segment
/// positions, which the editor places on a grid so fullwidth characters line up.
fn char_width(typography: &DataTypography) -> Pixels {
    px(f32::from(typography.size) * MONO_ADVANCE)
}

/// Width of one line, in grid cells, capped so a huge line cannot stretch the list.
fn logical_line_width(gutter: Pixels, cells: usize, char_width: Pixels) -> Pixels {
    gutter + char_width * cells.clamp(1, MAX_SCROLL_CHARS) as f32
}

/// Line-number digits the gutter always reserves.
///
/// The width used to follow the digit count of the current line count, so a document
/// with 9 lines and one with 10 put the text at different x positions. Switching
/// resources then slid the whole document sideways, which read as the text jumping.
/// A fixed floor keeps every document on the same origin; three digits covers any
/// resource short of a thousand lines, which every Kubernetes object stays under.
const MIN_GUTTER_DIGITS: usize = 3;

/// The left inset of the text. Constant for a given font, so nothing shifts sideways.
fn gutter_width(count: usize, char_width: Pixels) -> Pixels {
    let digits = count.max(1).to_string().len().max(MIN_GUTTER_DIGITS);
    px(digits as f32 * f32::from(char_width)) + space::MD
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;

    use gpui::{
        Bounds, EntityInputHandler as _, Hsla, KeyBinding, Modifiers, TestAppContext,
        VisualTestContext, point, px, rgba, size,
    };
    use theme::LoadThemes;

    use super::buffer::{EditBuffer, grapheme_start};
    use super::{
        CARET_BLINK_INTERVAL, CHEAT_SHEET_HEIGHT_FRACTION, CHEAT_SHEET_MAX_HEIGHT,
        CHEAT_SHEET_MAX_WIDTH, CHEAT_SHEET_WIDTH_FRACTION, ClickTarget, Diagnostic, INDENT_CELLS,
        LINE_WINDOW_CHARS, LINE_WINDOW_MARGIN, MAX_SCROLL_CHARS, MIN_GUTTER_DIGITS,
        SEARCH_DEBOUNCE, VALIDATE_DEBOUNCE, YAML_EMPTY_HINT, YAML_EMPTY_TITLE, YamlView,
        caret_visible, char_width, cheat_sheet_height, cheat_sheet_width, click_target,
        first_visible_char, guide_columns, gutter_width, indent_cells, line_window,
        logical_line_width, match_column, matching_bracket, split_selection_spans,
    };
    use crate::design;
    use crate::session::TextInput;
    use crate::settings::{DataTypography, SettingsStore};
    use crate::shell::{Copy, Cut, FocusNext, FocusPrevious, Paste, Redo, SelectAll, Undo};
    use crate::yaml_editor::search::{SearchOptions, cell_column};

    #[test]
    fn buffer_splits_lines_and_tracks_longest() {
        let buffer = EditBuffer::new("a: 1\nbb: 22\nccc: 333");
        assert_eq!(buffer.line_count(), 3);
        assert_eq!(buffer.longest_line(), 2);
    }

    #[test]
    fn buffer_keeps_trailing_empty_line_and_strips_crlf() {
        let buffer = EditBuffer::new("a: 1\r\n");
        assert_eq!(buffer.line_count(), 2);
        assert_eq!(buffer.line(0).text(), "a: 1");
        assert_eq!(buffer.line(1).text(), "");
        assert_eq!(EditBuffer::new("").line_count(), 1);
    }

    // The gutter must not follow the digit count, or the text slides sideways whenever
    // a click swaps in a document of a different length.
    #[test]
    fn gutter_width_is_stable_across_line_counts() {
        let char_width = px(7.2);
        let reference = gutter_width(1, char_width);
        for count in [1, 9, 10, 99, 100, 999] {
            assert_eq!(
                gutter_width(count, char_width),
                reference,
                "{count} lines keep the text on the same origin"
            );
        }
        assert_eq!(
            reference,
            px(MIN_GUTTER_DIGITS as f32 * 7.2) + design::space::MD,
            "the gutter reserves the fixed digit count"
        );
    }

    // A document long enough to need more digits than the gutter reserves still has to
    // grow, otherwise the number would be clipped.
    #[test]
    fn gutter_width_grows_past_the_reserved_digits() {
        let char_width = px(7.2);
        assert!(gutter_width(10_000, char_width) > gutter_width(999, char_width));
    }

    #[test]
    fn line_window_stays_within_line() {
        // Two-byte characters: the window must cover the request without splitting one.
        let line = "中文".repeat(10);
        let window = line_window(&line, 0, 7);
        let text = &line[window.bytes.clone()];
        assert!(
            text.chars().count() >= 7,
            "the window covers the requested characters: {text:?}"
        );
        assert!(line.starts_with(text), "{text:?}");
        assert_eq!(window.column, 0);
        let short = line_window("short", 0, 7);
        assert_eq!(short.bytes, 0..5);
        assert_eq!(short.column, 0);
    }

    #[test]
    fn long_logical_line_keeps_a_bounded_shaping_window() {
        let line = "x".repeat(20_000);
        let window = line_window(&line, 18_000, LINE_WINDOW_CHARS);
        assert_eq!(
            line[window.bytes.clone()].chars().count(),
            LINE_WINDOW_CHARS
        );
        assert!(window.bytes.start < 18_000 && 18_000 < window.bytes.end);
    }

    #[test]
    fn a_line_longer_than_the_scroll_cap_keeps_a_small_window() {
        let line = "x".repeat(MAX_SCROLL_CHARS * 4);
        let window = line_window(&line, MAX_SCROLL_CHARS + 100, LINE_WINDOW_CHARS);
        assert!(
            window.bytes.end - window.bytes.start <= LINE_WINDOW_CHARS + LINE_WINDOW_MARGIN,
            "shaping and painting stay proportional to the window: {:?}",
            window.bytes
        );
        assert_eq!(window.column, MAX_SCROLL_CHARS + 100 - LINE_WINDOW_MARGIN);
        let windowed = &line[window.bytes.clone()];
        assert_eq!(
            match_column(windowed, 10),
            10,
            "columns inside the window are counted from the window, not the line"
        );
        assert_eq!(
            window.column + windowed.chars().count(),
            match_column(line.as_str(), window.bytes.end),
            "windowed columns agree with a full scan"
        );
    }

    #[test]
    fn a_window_never_splits_a_grapheme() {
        // A base letter with a combining mark and a joined emoji are one grapheme each.
        let line = "e\u{301}".repeat(40) + &"👨\u{200d}👩\u{200d}👧".repeat(20);
        assert!(
            line_window(&line, 0, 8).bytes.end > 0,
            "window is not empty"
        );
        for first in [0, 1, 7, 39, 41] {
            let line = line.as_str();
            let window = line_window(line, first, 8);
            let text = &line[window.bytes.clone()];
            assert!(
                !text.starts_with('\u{301}') && !text.starts_with('\u{200d}'),
                "window {first} starts inside a grapheme: {text:?}"
            );
            assert!(
                grapheme_start(line, window.bytes.end) == window.bytes.end,
                "window {first} ends inside a grapheme"
            );
        }
    }

    #[test]
    fn long_logical_line_has_a_bounded_layout_width() {
        let width = logical_line_width(px(8.), MAX_SCROLL_CHARS + 100, px(2.));
        assert_eq!(width, px(8. + 2. * MAX_SCROLL_CHARS as f32));
    }

    #[test]
    fn a_click_count_picks_the_caret_the_word_or_the_line() {
        assert_eq!(click_target(1), ClickTarget::Caret);
        assert_eq!(click_target(2), ClickTarget::Word);
        assert_eq!(click_target(3), ClickTarget::Line);
        assert_eq!(
            click_target(4),
            ClickTarget::Line,
            "a quadruple click still takes the line"
        );
    }

    #[test]
    fn indent_guides_follow_the_nesting_level() {
        assert_eq!(INDENT_CELLS, 2);
        assert_eq!(indent_cells(""), 0);
        assert_eq!(indent_cells("name: app"), 0);
        assert_eq!(indent_cells("  name: app"), 2);
        assert_eq!(indent_cells("      name: app"), 6);
        assert_eq!(indent_cells("  \tname: app"), 2 + INDENT_CELLS);
        assert_eq!(guide_columns(0), 0);
        assert_eq!(guide_columns(6), 3);
        assert_eq!(
            guide_columns(10_000),
            32,
            "a deep line cannot flood the row with guides"
        );
    }

    #[test]
    fn a_fullwidth_character_occupies_two_grid_cells() {
        let line = "中文: value";
        assert_eq!(cell_column(line, 0), 0);
        assert_eq!(cell_column(line, "中".len()), 2);
        assert_eq!(cell_column(line, "中文".len()), 4);
        assert_eq!(cell_column(line, "中文:".len()), 5);
        assert_eq!(match_column(line, "中文:".len()), 3);
        let window = line_window(line, 0, LINE_WINDOW_CHARS);
        assert_eq!(window.cells, window.column, "no fullwidth character yet");
    }

    #[test]
    fn the_bracket_highlight_finds_the_partner_of_the_caret() {
        let line = "items: [a, b]";
        assert_eq!(matching_bracket(line, 7), Some(12), "the opening bracket");
        assert_eq!(matching_bracket(line, 8), Some(12), "the caret is after it");
        assert_eq!(matching_bracket(line, 12), Some(7), "the closing bracket");
        assert_eq!(
            matching_bracket(line, 13),
            Some(7),
            "the caret is past the end"
        );
        assert_eq!(matching_bracket(line, 0), None, "no partner in sight");
        assert_eq!(matching_bracket("name: app", 2), None);
        assert_eq!(matching_bracket("name: \"app\"", 7), Some(10));
        assert_eq!(
            matching_bracket("a: {b: [1]}", 3),
            Some(10),
            "the outer pair"
        );
        assert_eq!(
            matching_bracket("a: {b: [1]}", 9),
            Some(7),
            "nested brackets pair with the nearest one"
        );
        assert_eq!(matching_bracket("a: [1", 3), None, "an unclosed bracket");
    }

    #[gpui::test]
    fn layout_cap_does_not_truncate_long_line_editing(cx: &mut TestAppContext) {
        let text = "x".repeat(MAX_SCROLL_CHARS + 100);
        let end = text.len();
        let (view, cx) = setup(cx, &text);
        view.update(cx, |view, _| {
            view.with_buffer(|buffer| buffer.set_cursor(end, false));
            assert!(view.type_text("y"));
        });
        assert_eq!(text_of(&view, cx), format!("{text}y"));
    }

    #[test]
    fn selection_spans_split_at_syntax_boundaries() {
        let spans = vec![0..4, 4..7, 7..10];
        assert_eq!(
            split_selection_spans(&spans, Some(&(3..8))),
            vec![
                (0..3, false),
                (3..4, true),
                (4..7, true),
                (7..8, true),
                (8..10, false),
            ]
        );
    }

    #[test]
    fn syntax_token_is_adjusted_for_a_low_contrast_line_background() {
        let syntax: Hsla = rgba(0x666666ff).into();
        let editor_foreground: Hsla = rgba(0xffffffff).into();
        let background: Hsla = rgba(0x5a5a5aff).into();
        let adjusted = design::text_on(background, syntax, editor_foreground);
        assert_ne!(adjusted, syntax);
        assert_eq!(adjusted, editor_foreground);
    }

    #[test]
    fn caret_visibility_requires_focus_and_honors_reduce_motion() {
        assert!(caret_visible(true, true, false));
        assert!(!caret_visible(false, true, false));
        assert!(!caret_visible(true, false, false));
        assert!(caret_visible(true, false, true));
    }

    /// The shortcut sheet is a dialog over the document, so it is sized against the window rather
    /// than against the document. A fixed 520px height was 81% of the smallest window the app
    /// supports, which is the opposite of what a reference sheet should do with the editor it is
    /// explaining.
    #[test]
    fn the_shortcut_sheet_leaves_room_for_the_document_it_explains() {
        let (min_width, min_height) = design::size::WINDOW_MIN;
        let height = cheat_sheet_height(px(min_height));
        assert!(
            f32::from(height) <= f32::from(px(min_height)) * CHEAT_SHEET_HEIGHT_FRACTION + 0.5,
            "the sheet is capped at {CHEAT_SHEET_HEIGHT_FRACTION} of the window: {height:?}"
        );
        assert!(
            f32::from(height) < f32::from(px(min_height)) * 0.7,
            "the sheet must leave the document visible on the smallest window: {height:?}"
        );
        let width = cheat_sheet_width(px(min_width));
        assert!(
            f32::from(width) <= f32::from(px(min_width)) * CHEAT_SHEET_WIDTH_FRACTION + 0.5,
            "the sheet is capped at {CHEAT_SHEET_WIDTH_FRACTION} of the window: {width:?}"
        );
        // A large window still gets the absolute cap, so the sheet does not become a wall.
        assert_eq!(cheat_sheet_height(px(2000.)), px(CHEAT_SHEET_MAX_HEIGHT));
        assert_eq!(cheat_sheet_width(px(2000.)), px(CHEAT_SHEET_MAX_WIDTH));
        // A small window is not squeezed below the rows it has to show.
        assert_eq!(cheat_sheet_height(px(500.)), px(300.));
    }

    /// The read-only marker and the shortcut button are layered over the document, not over the
    /// whole editor. They were absolutely positioned against the editor, so with the search field
    /// open they landed on top of it.
    #[gpui::test]
    fn the_status_corner_stays_over_the_document(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        cx.run_until_parked();

        let layer = cx
            .debug_bounds("yaml-body-layer")
            .expect("the document layer");
        let corner = cx
            .debug_bounds("yaml-status-corner")
            .expect("the status corner");
        assert!(
            corner.top() >= layer.top() && corner.bottom() <= layer.bottom() + px(1.),
            "the corner must belong to the document layer: corner {corner:?}, layer {layer:?}"
        );
        assert!(corner.right() <= layer.right() + px(1.));

        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.focus_search(window, cx));
        });
        cx.run_until_parked();
        let search = cx.debug_bounds("yaml-search-bar").expect("the search bar");
        let corner = cx
            .debug_bounds("yaml-status-corner")
            .expect("the status corner");
        assert!(
            corner.top() >= search.bottom() - px(1.),
            "the corner must not land on the search field: corner {corner:?}, search {search:?}"
        );
    }

    /// One sentence, one punctuation. The two surfaces used to carry their own copy of this state
    /// and disagree, so the same message read differently depending on which panel held it.
    #[test]
    fn the_yaml_empty_state_reads_the_same_wherever_it_appears() {
        assert_eq!(
            YAML_EMPTY_TITLE, "No YAML to show",
            "a full stop here and none in the Inspector made the same state read as two"
        );
        assert_eq!(YAML_EMPTY_HINT, "Select a row to inspect its YAML.");
        assert_eq!(crate::yaml_editor::EMPTY_TITLE, YAML_EMPTY_TITLE);
        assert_eq!(crate::yaml_editor::EMPTY_HINT, YAML_EMPTY_HINT);
    }

    /// One implementation, so the same state cannot come back with its own glyph size.
    ///
    /// Sharing the strings was not enough on its own: the editor's own `empty_state` used
    /// `IconSize::XLarge` (48px) where the shared one uses `design::size::ICON_LARGE` (32px), so
    /// the same sentence arrived at two sizes. The Inspector draws `panels::common::empty_state`,
    /// so if the editor stops doing the same this test sees it.
    #[gpui::test]
    fn the_editor_draws_the_shared_empty_state(cx: &mut TestAppContext) {
        init_app(cx);
        let (_view, cx) = cx.add_window_view(|_, cx| YamlView::new(cx));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("empty-state").is_some(),
            "a document-less editor must draw the shared empty state, not a second copy of it"
        );
    }

    #[test]
    fn product_theme_uses_zed_flattened_style_fields() {
        let family = theme_settings::deserialize_user_theme(include_bytes!(
            "../../../k8s-app/assets/themes/k8s-studio.json"
        ))
        .expect("K8s Studio theme must deserialize");
        let style = &family.themes[0].style;
        assert!(style.colors.text.is_some());
        assert!(style.colors.element_selection_background.is_some());
        assert!(style.colors.editor_gutter_background.is_some());
        assert!(style.colors.editor_line_number.is_some());
        assert!(style.status.success.is_some());
        assert!(style.status.created.is_some());
        assert!(style.syntax.contains_key("property"));
        assert!(style.syntax.contains_key("string"));
        assert_eq!(style.players.len(), 1);
        for content in &family.themes {
            assert!(content.style.syntax.contains_key("comment"));
            assert!(content.style.syntax.contains_key("number"));
        }
        let refined = theme_settings::refine_theme_family(family);
        for theme in &refined.themes {
            assert!(theme.syntax().style_for_name("property").is_some());
            assert!(theme.syntax().style_for_name("string").is_some());
        }
    }

    fn init_app(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            crate::settings::install_product_typography_defaults(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
            // Tab reaches the editor as the FocusNext action, which the editor takes over
            // to indent: the global keymap owns the key, the editor owns the action (see
            // KEYMAP.md 4.4). Only that pair is bound here, so these tests do not depend
            // on the rest of the default keymap.
            cx.bind_keys([
                KeyBinding::new("tab", FocusNext, None),
                KeyBinding::new("shift-tab", FocusPrevious, None),
            ]);
        });
    }

    fn setup<'a>(
        cx: &'a mut TestAppContext,
        text: &str,
    ) -> (gpui::Entity<YamlView>, &'a mut VisualTestContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| YamlView::new(cx));
        view.update(cx, |view, cx| {
            view.set_text(Some(text.to_owned()), cx);
            view.set_editable(true, cx);
        });
        (view, cx)
    }

    #[gpui::test]
    fn new_view_defaults_to_editable_input(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| YamlView::new(cx));
        assert!(view.read_with(cx, |view, _| view.is_editable()));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.focus(window, cx));
        });
        cx.simulate_input("x");
        assert_eq!(
            view.read_with(cx, |view, _| view.text()),
            Some("x".to_owned())
        );
    }

    #[gpui::test]
    fn input_lock_rejects_document_edits(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        view.update(cx, |view, cx| view.set_input_locked(true, cx));
        click_at(cx, point(px(5.), px(9.)));
        cx.simulate_input("x");
        assert_eq!(text_of(&view, cx), "name: app");
    }

    #[gpui::test]
    fn apply_request_locks_text_input(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        view.update(cx, |view, _cx| {
            view.set_on_apply_requested(|_, _, _| {});
            view.type_text("x");
        });
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                assert!(view.request_apply(window, cx));
                assert!(view.is_input_locked());
            });
        });
    }

    #[gpui::test]
    fn clean_document_does_not_request_apply(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        let requested = Arc::new(AtomicBool::new(false));
        let sink = requested.clone();
        view.update(cx, |view, _| {
            view.set_on_apply_requested(move |_, _, _| {
                sink.store(true, Ordering::SeqCst);
            });
        });
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                assert!(!view.request_apply(window, cx));
                assert!(!view.is_input_locked());
            });
        });
        assert!(!requested.load(Ordering::SeqCst));
    }

    #[gpui::test]
    fn edit_callback_runs_after_document_changes(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        let edits = Arc::new(AtomicBool::new(false));
        let sink = edits.clone();
        view.update(cx, |view, _| {
            view.set_on_edit(move |_| sink.store(true, Ordering::SeqCst));
        });
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.focus(window, cx));
        });
        cx.simulate_input("x");
        assert!(edits.load(Ordering::SeqCst));
    }

    fn text_of(view: &gpui::Entity<YamlView>, cx: &VisualTestContext) -> String {
        view.read_with(cx, |view, _| view.text().unwrap_or_default())
    }

    // Move first because mouse-down handling uses the previous hover position.
    fn click_at(cx: &mut VisualTestContext, position: gpui::Point<gpui::Pixels>) {
        cx.simulate_mouse_move(position, None, Modifiers::none());
        cx.simulate_click(position, Modifiers::none());
    }

    // The caret x of the first character, which is where the text column starts.
    fn text_column_x(view: &gpui::Entity<YamlView>, cx: &mut VisualTestContext) -> gpui::Pixels {
        let element = Bounds::new(point(px(0.), px(0.)), size(px(400.), px(400.)));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.bounds_for_range(0..0, element, window, cx)
                    .expect("caret bounds")
                    .origin
                    .x
            })
        })
    }

    // Clicking a resource swaps the whole document, and the text must not move sideways
    // when the new one has a different number of lines. This is the reported defect: the
    // gutter used to follow the digit count, so every length change slid the text.
    #[gpui::test]
    fn swapping_documents_keeps_the_text_column(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "a: 1\n");
        // Documents either side of the 10- and 100-line boundaries, which is where the
        // old gutter changed width.
        let lengths = [1, 9, 10, 99, 100, 250];
        let mut seen: Option<gpui::Pixels> = None;
        for lines in lengths {
            let text: String = (0..lines).map(|i| format!("k{i}: {i}\n")).collect();
            view.update(cx, |view, cx| view.set_text(Some(text), cx));
            cx.run_until_parked();
            let x = text_column_x(&view, cx);
            if let Some(first) = seen {
                assert_eq!(
                    x, first,
                    "a {lines}-line document keeps the text where it was"
                );
            } else {
                seen = Some(x);
            }
        }
    }

    // A click in the gutter focuses the editor and places the cursor at column 0.
    #[gpui::test]
    fn click_focuses_and_typing_inserts(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        click_at(cx, point(px(5.), px(9.)));
        cx.simulate_input("kind: Pod\n");
        assert_eq!(text_of(&view, cx), "kind: Pod\nname: app");
        assert!(view.read_with(cx, |view, _| view.is_dirty()));
    }

    // A click on the third visible row targets that row.
    #[gpui::test]
    fn click_targets_visible_row(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "a: 1\nb: 2\nc: 3");
        let line_height = cx.update(|_, cx| DataTypography::from_theme_settings(cx).line_height);
        let y = line_height * 2. + px(6.);
        click_at(cx, point(px(5.), y));
        cx.simulate_input("X");
        assert_eq!(text_of(&view, cx), "a: 1\nb: 2\nXc: 3");
    }

    #[gpui::test]
    fn custom_buffer_typography_drives_yaml_geometry(cx: &mut TestAppContext) {
        init_app(cx);
        cx.update(|cx| {
            SettingsStore::update(cx, |store, cx| {
                store
                    .set_user_settings(
                        r#"{
                            "buffer_font_size": 16,
                            "buffer_line_height": { "custom": 1.5 }
                        }"#,
                        cx,
                    )
                    .result()
                    .expect("valid buffer typography settings");
            });
        });
        let (view, cx) = cx.add_window_view(|_, cx| YamlView::new(cx));
        view.update(cx, |view, cx| {
            view.set_text(Some("a: 1\nb: 2\nc: 3".to_owned()), cx);
        });

        let geometry = cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                let typography = DataTypography::from_theme_settings(cx);
                let char_width = char_width(&typography);
                let element_bounds = Bounds::new(point(px(0.), px(0.)), size(px(300.), px(100.)));
                let second_line_column_two = view
                    .bounds_for_range(6..6, element_bounds, window, cx)
                    .expect("document caret bounds");
                let hit = view.character_index_for_point(
                    point(
                        second_line_column_two.origin.x,
                        second_line_column_two.origin.y + typography.line_height + px(1.),
                    ),
                    window,
                    cx,
                );
                let row = view.row_for_position(
                    second_line_column_two.origin.y + typography.line_height + px(1.),
                    &typography,
                );

                view.set_text(None, cx);
                view.focus(window, cx);
                let empty_caret = view
                    .bounds_for_range(0..0, element_bounds, window, cx)
                    .expect("empty document caret bounds");

                (
                    typography.size,
                    typography.line_height,
                    char_width,
                    second_line_column_two.size,
                    hit,
                    row,
                    empty_caret.size.height,
                    first_visible_char(-char_width, char_width),
                )
            })
        });

        assert_eq!(geometry.0, px(16.));
        assert_eq!(geometry.1, px(24.));
        assert!((f32::from(geometry.2) - 9.6).abs() < 0.001);
        assert_eq!(geometry.3, size(px(9.6), px(24.)));
        assert_eq!(geometry.4, Some(11));
        assert_eq!(geometry.5, Some(2));
        assert_eq!(geometry.6, px(24.));
        assert_eq!(geometry.7, 1);
    }

    #[gpui::test]
    fn selection_keys_replace_and_undo(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "apiVersion: v1");
        click_at(cx, point(px(5.), px(9.)));
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("kind: Pod");
        assert_eq!(text_of(&view, cx), "kind: Pod");
        cx.simulate_keystrokes("secondary-z");
        assert_eq!(text_of(&view, cx), "apiVersion: v1");
        cx.simulate_keystrokes("secondary-shift-z");
        assert_eq!(text_of(&view, cx), "kind: Pod");
    }

    #[gpui::test]
    fn native_edit_actions_drive_buffer(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "apiVersion: v1");
        click_at(cx, point(px(5.), px(9.)));
        cx.simulate_input("x");
        assert_eq!(text_of(&view, cx), "xapiVersion: v1");

        cx.dispatch_action(Undo);
        assert_eq!(text_of(&view, cx), "apiVersion: v1");
        cx.dispatch_action(Redo);
        assert_eq!(text_of(&view, cx), "xapiVersion: v1");

        cx.dispatch_action(SelectAll);
        cx.dispatch_action(Copy);
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("xapiVersion: v1")
        );
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("replacement".to_owned()));
        cx.dispatch_action(Paste);
        assert_eq!(text_of(&view, cx), "replacement");

        cx.dispatch_action(SelectAll);
        cx.dispatch_action(Cut);
        assert_eq!(text_of(&view, cx), "");
        cx.dispatch_action(Undo);
        assert_eq!(text_of(&view, cx), "replacement");
        cx.dispatch_action(Redo);
        assert_eq!(text_of(&view, cx), "");
    }

    #[gpui::test]
    fn read_only_mode_ignores_typing(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        view.update(cx, |view, cx| view.set_editable(false, cx));
        click_at(cx, point(px(5.), px(9.)));
        cx.simulate_input("x");
        assert_eq!(text_of(&view, cx), "name: app");
    }

    #[gpui::test]
    fn read_only_mode_allows_selection_but_rejects_edits(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        view.update(cx, |view, cx| view.set_editable(false, cx));
        click_at(cx, point(px(5.), px(9.)));
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("x");
        assert_eq!(text_of(&view, cx), "name: app");
        assert!(view.read_with(cx, |view, _| {
            view.buffer
                .as_ref()
                .is_some_and(|buffer| buffer.borrow().selection().is_some())
        }));
    }

    #[gpui::test]
    fn read_only_mode_rejects_ctrl_enter_apply(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        let applied = Arc::new(AtomicBool::new(false));
        let request = applied.clone();
        view.update(cx, |view, cx| {
            view.set_on_apply_requested(move |_, _, _| {
                request.store(true, Ordering::SeqCst);
            });
            view.set_editable(false, cx);
        });
        click_at(cx, point(px(5.0), px(9.0)));
        cx.simulate_keystrokes("secondary-enter");
        assert!(!applied.load(Ordering::SeqCst));
    }

    #[gpui::test]
    fn set_text_resets_dirty_and_cursor(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        click_at(cx, point(px(5.), px(9.)));
        cx.simulate_input("x");
        assert!(view.read_with(cx, |view, _| view.is_dirty()));
        view.update(cx, |view, cx| {
            view.set_text(Some("kind: Pod".to_owned()), cx);
            assert!(!view.is_dirty(), "set_text must reset dirty state");
        });
        assert_eq!(text_of(&view, cx), "kind: Pod");
    }

    #[gpui::test]
    fn edits_invalidate_search_matches(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        view.update(cx, |view, cx| view.set_search_query("app", cx));
        assert_eq!(view.read_with(cx, |view, _| view.matches.len()), 1);
        click_at(cx, point(px(5.), px(37.)));
        cx.simulate_input("app: x\n");
        cx.executor().advance_clock(SEARCH_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.matches.len()),
            2,
            "edits recompute matches from the new text"
        );
    }

    #[gpui::test]
    fn search_input_debounces_and_drops_stale_queries(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app\nname: other");
        focus_search(&view, cx);
        cx.simulate_input("name");
        cx.run_until_parked();
        cx.simulate_input("x");
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, _| view.search_pending));
        assert!(view.read_with(cx, |view, _| view.matches.is_empty()));

        cx.executor()
            .advance_clock(SEARCH_DEBOUNCE - Duration::from_millis(1));
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, _| view.matches.is_empty()));
        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, _| view.matches.is_empty()));
        assert!(!view.read_with(cx, |view, _| view.search_pending));

        cx.simulate_keystrokes("backspace");
        cx.run_until_parked();
        cx.executor().advance_clock(SEARCH_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.query.clone()), "name");
        assert_eq!(view.read_with(cx, |view, _| view.matches.len()), 2);
    }

    #[gpui::test]
    fn search_input_keeps_editing_and_navigation_separate_from_document(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app\nname: other");
        focus_search(&view, cx);
        view.update(cx, |view, cx| view.set_search_query("name", cx));
        cx.simulate_input("x");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.query.clone()), "namex");
        assert_eq!(text_of(&view, cx), "name: app\nname: other");
        cx.simulate_keystrokes("backspace");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.query.clone()), "name");
        cx.simulate_keystrokes("secondary-z");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.query.clone()), "namex");
        cx.simulate_keystrokes("secondary-u");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.query.clone()), "");
        cx.simulate_keystrokes("secondary-y");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.query.clone()), "namex");
        cx.simulate_input(" alpha beta");
        cx.simulate_keystrokes("secondary-w");
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.query.clone()),
            "namex alpha "
        );
        cx.simulate_keystrokes("secondary-k");
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.query.clone()),
            "namex alpha "
        );
        cx.simulate_keystrokes("escape");
        assert!(!view.read_with(cx, |view, _| view.search_visible));
        assert_eq!(text_of(&view, cx), "name: app\nname: other");
    }

    #[gpui::test]
    fn typing_long_line_scrolls_horizontally(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "short");
        click_at(cx, point(px(5.), px(9.)));
        cx.simulate_input(&"x".repeat(300));
        let offset_x = view.read_with(cx, |view, _| view.scroll.0.borrow().base_handle.offset().x);
        assert!(
            offset_x < px(-100.),
            "long input must scroll horizontally, offset_x={offset_x:?}"
        );
    }

    // A drag that leaves the text area must keep extending the selection, so it can reach
    // the last line of the document.
    #[gpui::test]
    fn dragging_past_the_last_line_selects_to_the_end_of_the_document(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "one\ntwo\nthree");
        let line_height = cx.update(|_, cx| DataTypography::from_theme_settings(cx).line_height);
        let length = text_of(&view, cx).len();
        let selection = |cx: &VisualTestContext| {
            view.read_with(cx, |view, _| {
                view.buffer
                    .as_ref()
                    .and_then(|buffer| buffer.borrow().selection())
            })
        };
        cx.simulate_mouse_move(point(px(5.), px(1.)), None, Modifiers::none());
        cx.simulate_mouse_down(
            point(px(5.), px(1.)),
            gpui::MouseButton::Left,
            Modifiers::none(),
        );
        // Below the last line: the selection reaches the end of the document.
        cx.simulate_mouse_move(
            point(px(400.), line_height * 40.),
            Some(gpui::MouseButton::Left),
            Modifiers::none(),
        );
        assert_eq!(
            selection(cx),
            Some(0..length),
            "a drag below the last line selects to the end of the document"
        );
        // Back inside the text area: the selection follows the pointer again.
        cx.simulate_mouse_move(
            point(px(5.), line_height),
            Some(gpui::MouseButton::Left),
            Modifiers::none(),
        );
        assert_eq!(
            selection(cx),
            Some(0..4),
            "dragging back into the text area shrinks the selection"
        );
        cx.simulate_mouse_up(
            point(px(5.), line_height),
            gpui::MouseButton::Left,
            Modifiers::none(),
        );
        assert!(
            !view.read_with(cx, |view, _| view.dragging),
            "releasing the pointer ends the drag"
        );
    }

    // The blink loop only hides the caret when the last interval had no typing, so a long
    // typing burst never flickers.
    #[gpui::test]
    fn caret_stays_solid_while_typing_and_blinks_after_a_pause(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.focus(window, cx));
        });
        cx.run_until_parked();
        let caret_visible =
            |cx: &VisualTestContext| view.read_with(cx, |view, _| view.caret_blink_visible);
        assert!(caret_visible(cx), "focus starts the caret visible");

        cx.executor().advance_clock(CARET_BLINK_INTERVAL);
        cx.run_until_parked();
        assert!(
            !caret_visible(cx),
            "an idle caret blinks away after one interval"
        );
        cx.executor().advance_clock(CARET_BLINK_INTERVAL);
        cx.run_until_parked();
        assert!(caret_visible(cx), "the caret blinks back on");

        // Every interval with a keystroke keeps the caret solid.
        for _ in 0..3 {
            cx.simulate_input("x");
            cx.executor().advance_clock(CARET_BLINK_INTERVAL);
            cx.run_until_parked();
            assert!(
                caret_visible(cx),
                "typing must not let the caret blink away"
            );
        }
        cx.executor().advance_clock(CARET_BLINK_INTERVAL);
        cx.run_until_parked();
        assert!(!caret_visible(cx), "blinking resumes once typing pauses");
    }

    #[gpui::test]
    fn reduce_motion_keeps_the_caret_solid(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        // The app applies the stored setting to the window, so the editor follows it here.
        cx.update(|_, cx| cx.set_reduce_motion(true));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.focus(window, cx));
        });
        cx.run_until_parked();
        for _ in 0..3 {
            cx.executor().advance_clock(CARET_BLINK_INTERVAL);
            cx.run_until_parked();
            assert!(
                view.read_with(cx, |view, _| view.caret_blink_visible),
                "reduce motion never blinks the caret"
            );
        }
    }

    // The IME candidate window follows the caret, so its bounds have to move with the
    // document: line by line, with the horizontal scroll, and inside a composition.
    #[gpui::test]
    fn ime_caret_bounds_follow_the_caret(cx: &mut TestAppContext) {
        // Fixed-width rows, so a byte offset maps to a known row and column.
        let row = |index: usize| format!("k{index:03}: 1\n");
        let text: String = (0..200).map(&row).collect();
        let byte_of = |row_index: usize| row(row_index).len() * row_index;
        let (view, cx) = setup(cx, &text);
        // A focused document composes at the caret instead of replacing the whole text.
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.focus(window, cx));
        });
        let line_height = cx.update(|_, cx| DataTypography::from_theme_settings(cx).line_height);
        let element = Bounds::new(point(px(0.), px(0.)), size(px(400.), px(400.)));
        let caret_bounds = |cx: &mut VisualTestContext, byte: usize| {
            cx.update(|window, cx| {
                view.update(cx, |view, cx| {
                    view.bounds_for_range(byte..byte, element, window, cx)
                        .expect("caret bounds")
                })
            })
        };
        let first = caret_bounds(cx, 0);
        let middle = caret_bounds(cx, byte_of(50));
        assert_eq!(first.size.height, line_height);
        assert_eq!(
            middle.origin.y - first.origin.y,
            line_height * 50.,
            "the candidate window follows the caret to its line"
        );
        assert_eq!(
            middle.origin.x, first.origin.x,
            "the caret starts at the first column"
        );
        let middle_end = caret_bounds(cx, byte_of(50) + 4);
        assert!(
            middle_end.origin.x > middle.origin.x,
            "a later column moves the candidate window right"
        );
        assert_eq!(
            middle_end.origin.y, middle.origin.y,
            "the row does not move"
        );

        // Scrolling moves the bounds with the content, so the window stays on the caret.
        view.update(cx, |view, cx| view.scroll_to_line(50, cx));
        cx.run_until_parked();
        let scrolled = caret_bounds(cx, byte_of(50));
        assert_eq!(
            scrolled.origin.y, first.origin.y,
            "the scrolled caret reaches the top of the viewport"
        );

        // A composition reports bounds inside the preedit text.
        view.update(cx, |view, cx| {
            view.place_caret(byte_of(50), byte_of(50), cx)
        });
        ime_preedit(&view, "zhong", cx);
        let composition_start = caret_bounds(cx, byte_of(50));
        let composition_end = caret_bounds(cx, byte_of(50) + 5);
        assert!(
            composition_end.origin.x > composition_start.origin.x,
            "the candidate window follows the composition"
        );
        assert_eq!(composition_end.origin.y, composition_start.origin.y);
    }

    fn diagnostic(line: usize, column: usize, message: &str) -> Diagnostic {
        Diagnostic {
            line,
            column,
            message: message.to_owned(),
        }
    }

    #[gpui::test]
    fn set_diagnostics_sorts_and_jumps_to_first_error(cx: &mut TestAppContext) {
        let text = (0..2000)
            .map(|index| format!("key{index}: value"))
            .collect::<Vec<_>>()
            .join("\n");
        let (view, cx) = setup(cx, &text);
        view.update(cx, |view, cx| {
            view.set_diagnostics(
                vec![
                    diagnostic(1500, 0, "second"),
                    diagnostic(1400, 6, "first-b"),
                    diagnostic(1400, 2, "first-a"),
                ],
                cx,
            );
        });
        view.read_with(cx, |view, _| {
            let lines: Vec<usize> = view
                .diagnostics()
                .iter()
                .map(|diagnostic| diagnostic.line)
                .collect();
            assert_eq!(
                lines,
                vec![1400, 1400, 1500],
                "diagnostics sort by line and column"
            );
            assert_eq!(view.diagnostics()[0].column, 2);
            assert_eq!(view.diagnostics_by_line.get(&1400), Some(&0));
        });
        let offset_y = view.read_with(cx, |view, _| view.scroll.0.borrow().base_handle.offset().y);
        assert!(
            offset_y < px(-1000.),
            "the view must scroll near the first error, offset_y={offset_y:?}"
        );
    }

    #[gpui::test]
    fn focus_line_puts_the_caret_on_a_position(cx: &mut TestAppContext) {
        let text = "a: 1\nname: 中文: x\nc: 3";
        let (view, cx) = setup(cx, text);
        let line_start = "a: 1\n".len();
        view.update(cx, |view, cx| view.focus_line(1, 8, cx));
        view.read_with(cx, |view, _| {
            let expected = line_start + "name: 中".len();
            assert_eq!(
                view.caret(),
                (expected, expected),
                "a grid column lands on the character under it"
            );
        });
        view.update(cx, |view, cx| view.focus_line(1, 7, cx));
        view.read_with(cx, |view, _| {
            let expected = line_start + "name: ".len();
            assert_eq!(
                view.caret(),
                (expected, expected),
                "a column inside a fullwidth character snaps to its start"
            );
        });
        view.update(cx, |view, cx| view.focus_line(9, 0, cx));
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.caret(),
                (text.len(), text.len()),
                "a line past the end of the document keeps the caret in range"
            );
        });
    }

    #[gpui::test]
    fn no_op_undo_backspace_and_paste_preserve_diagnostics(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "a");
        click_at(cx, point(px(5.), px(9.)));
        view.update(cx, |view, cx| {
            view.set_diagnostics(vec![diagnostic(0, 0, "bad")], cx)
        });
        cx.dispatch_action(Undo);
        assert_eq!(view.read_with(cx, |view, _| view.diagnostics().len()), 1);
        cx.simulate_keystrokes("backspace");
        assert_eq!(view.read_with(cx, |view, _| view.diagnostics().len()), 1);
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(String::new()));
        cx.dispatch_action(Paste);
        assert_eq!(view.read_with(cx, |view, _| view.diagnostics().len()), 1);
        assert!(!view.read_with(cx, |view, _| view.is_dirty()));
    }

    #[gpui::test]
    fn editing_clears_diagnostics_and_empty_set_is_clear(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        view.update(cx, |view, cx| {
            view.set_diagnostics(vec![diagnostic(0, 0, "bad")], cx)
        });
        assert_eq!(view.read_with(cx, |view, _| view.diagnostics().len()), 1);

        click_at(cx, point(px(5.), px(9.)));
        view.update(cx, |view, _| {
            view.with_buffer(|buffer| buffer.set_cursor(0, false));
        });
        view.update(cx, |view, cx| view.clear_diagnostics(cx));
        assert!(view.read_with(cx, |view, _| view.diagnostics().is_empty()));
        view.update(cx, |view, cx| {
            view.set_diagnostics(vec![diagnostic(0, 0, "bad")], cx)
        });
        cx.simulate_input("x");
        assert!(
            view.read_with(cx, |view, _| view.diagnostics().is_empty()),
            "edits clear diagnostics"
        );
        view.update(cx, |view, cx| {
            view.set_diagnostics(vec![diagnostic(0, 0, "bad")], cx);
            view.set_text(Some("kind: Pod".to_owned()), cx);
        });
        assert!(
            view.read_with(cx, |view, _| view.diagnostics().is_empty()),
            "document replacement clears diagnostics"
        );
    }

    // Live validation reports a parse error after a typing pause, and an external
    // diagnostic survives until the next edit.
    #[gpui::test]
    fn editing_invalid_yaml_reports_a_diagnostic_after_the_debounce(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        cx.executor().advance_clock(VALIDATE_DEBOUNCE);
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |view, _| view.diagnostics().is_empty()),
            "a valid document reports nothing"
        );

        click_at(cx, point(px(5.), px(9.)));
        cx.simulate_input("bad: [\n");
        assert!(
            view.read_with(cx, |view, _| view.diagnostics().is_empty()),
            "the stale error is gone at once, before the new one is parsed"
        );
        cx.executor()
            .advance_clock(VALIDATE_DEBOUNCE - Duration::from_millis(1));
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |view, _| view.diagnostics().is_empty()),
            "validation waits for the typing to pause"
        );
        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        let diagnostics = view.read_with(cx, |view, _| view.diagnostics().to_vec());
        assert_eq!(
            diagnostics.len(),
            1,
            "the broken line is reported: {diagnostics:?}"
        );
        assert!(
            view.read_with(cx, |view, _| !view.diagnostics_by_line.is_empty()),
            "the reported line is marked in the gutter"
        );
    }

    #[gpui::test]
    fn a_later_edit_supersedes_a_slow_parse(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        view.update(cx, |view, cx| view.set_text(Some("bad: [".to_owned()), cx));
        view.update(cx, |view, cx| {
            view.set_text(Some("name: app".to_owned()), cx)
        });
        cx.executor().advance_clock(VALIDATE_DEBOUNCE);
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |view, _| view.diagnostics().is_empty()),
            "the parse of the older text is dropped"
        );
    }

    #[gpui::test]
    fn an_external_diagnostic_is_not_overwritten_by_live_validation(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        view.update(cx, |view, cx| {
            view.set_diagnostics(vec![diagnostic(0, 0, "apply failed")], cx)
        });
        cx.executor().advance_clock(VALIDATE_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.diagnostics()[0].message.clone()),
            "apply failed",
            "the Inspector owns the diagnostics until the next edit"
        );
        click_at(cx, point(px(5.), px(9.)));
        cx.simulate_input("x");
        cx.executor().advance_clock(VALIDATE_DEBOUNCE);
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |view, _| view.diagnostics().is_empty()),
            "an edit hands the diagnostics back to validation"
        );
    }

    // Editing model: Tab, the line operations, and word motion all answer their keys.
    #[gpui::test]
    fn tab_indents_and_shift_tab_outdents(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        click_at(cx, point(px(5.), px(9.)));
        cx.simulate_keystrokes("tab");
        assert_eq!(text_of(&view, cx), "  name: app");
        cx.simulate_keystrokes("shift-tab");
        assert_eq!(text_of(&view, cx), "name: app");
    }

    // The block path walks the rows it was given, so the ends of the document are where
    // an off-by-one shows: the first row, the last row, and a selection that reaches the
    // end of the document.
    #[gpui::test]
    fn tab_indents_a_block_from_the_first_row_to_the_last(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "a: 1\nb: 2\nc: 3");
        let line_height = cx.update(|_, cx| DataTypography::from_theme_settings(cx).line_height);
        let row_y = |row: usize| point(px(5.), line_height * row as f32 + px(4.));
        click_at(cx, row_y(0));
        cx.simulate_keystrokes("tab");
        assert_eq!(
            text_of(&view, cx),
            "  a: 1\nb: 2\nc: 3",
            "the caret row is the row that is indented"
        );
        click_at(cx, row_y(2));
        cx.simulate_keystrokes("tab");
        assert_eq!(
            text_of(&view, cx),
            "  a: 1\nb: 2\n  c: 3",
            "the last row indents without moving the rows above it"
        );
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_keystrokes("tab");
        assert_eq!(
            text_of(&view, cx),
            "    a: 1\n  b: 2\n    c: 3",
            "a selection that reaches the end of the document indents every row once"
        );
        cx.simulate_keystrokes("shift-tab");
        assert_eq!(
            text_of(&view, cx),
            "  a: 1\nb: 2\n  c: 3",
            "and Shift+Tab takes one step off every row of it"
        );
    }

    // The last row of a document with no trailing newline has no line break after it, so
    // the insert at its start is the one a block edit can get wrong.
    #[gpui::test]
    fn tab_indents_a_last_line_without_a_trailing_newline(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "a: 1\nb: 2");
        let line_height = cx.update(|_, cx| DataTypography::from_theme_settings(cx).line_height);
        click_at(cx, point(px(5.), line_height + px(4.)));
        cx.simulate_keystrokes("tab");
        assert_eq!(text_of(&view, cx), "a: 1\n  b: 2");
        cx.simulate_keystrokes("shift-tab");
        assert_eq!(text_of(&view, cx), "a: 1\nb: 2");
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_keystrokes("tab");
        assert_eq!(
            text_of(&view, cx),
            "  a: 1\n  b: 2",
            "and a selection to the end of that document indents both rows"
        );
    }

    // Shift+Tab on a row with no indent to remove changes nothing, and a key the editor
    // does not consume still leaves the surface.
    #[gpui::test]
    fn shift_tab_reports_a_row_with_no_indent(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "  a: 1\nb: 2");
        let line_height = cx.update(|_, cx| DataTypography::from_theme_settings(cx).line_height);
        click_at(cx, point(px(5.), line_height + px(4.)));
        cx.simulate_keystrokes("shift-tab");
        assert_eq!(
            text_of(&view, cx),
            "  a: 1\nb: 2",
            "the row had no indent step to remove"
        );
        click_at(cx, point(px(5.), px(4.)));
        cx.simulate_keystrokes("shift-tab");
        assert_eq!(
            text_of(&view, cx),
            "a: 1\nb: 2",
            "the row that had one gives up exactly one step"
        );
    }

    // The keymap owns Tab and hands it to the editor as an action. A keymap without that
    // binding must not fall through to the key: gpui puts a tab character in the
    // keystroke, and a tab in the indentation makes the manifest invalid YAML.
    #[gpui::test]
    fn an_unbound_tab_key_types_nothing(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        cx.update(|_, cx| cx.clear_key_bindings());
        cx.run_until_parked();
        click_at(cx, point(px(5.), px(9.)));
        cx.simulate_keystrokes("tab");
        assert_eq!(
            text_of(&view, cx),
            "name: app",
            "no binding means no action to indent and no tab character in the document"
        );
    }

    #[gpui::test]
    fn read_only_documents_keep_tab_for_focus(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        view.update(cx, |view, cx| view.set_editable(false, cx));
        click_at(cx, point(px(5.), px(9.)));
        cx.simulate_keystrokes("tab");
        assert_eq!(
            text_of(&view, cx),
            "name: app",
            "a read-only document has nothing to indent, so the key is left to the shell"
        );
    }

    #[gpui::test]
    fn the_line_operations_answer_their_keys(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "a: 1\nb: 2\nc: 3");
        let line_height = cx.update(|_, cx| DataTypography::from_theme_settings(cx).line_height);
        let row_y = |row: usize| point(px(5.), line_height * row as f32 + px(4.));
        click_at(cx, row_y(0));
        cx.simulate_keystrokes("alt-down");
        assert_eq!(text_of(&view, cx), "b: 2\na: 1\nc: 3");
        cx.simulate_keystrokes("alt-up");
        assert_eq!(text_of(&view, cx), "a: 1\nb: 2\nc: 3");
        cx.simulate_keystrokes("secondary-shift-d");
        assert_eq!(text_of(&view, cx), "a: 1\na: 1\nb: 2\nc: 3");
        cx.simulate_keystrokes("alt-shift-k");
        assert_eq!(
            text_of(&view, cx),
            "a: 1\nb: 2\nc: 3",
            "the caret is on the copy, so the copy is the line that goes"
        );
        click_at(cx, row_y(0));
        cx.simulate_keystrokes("alt-enter");
        assert_eq!(text_of(&view, cx), "a: 1\n\nb: 2\nc: 3");
        cx.simulate_keystrokes("alt-shift-enter");
        assert_eq!(text_of(&view, cx), "a: 1\n\n\nb: 2\nc: 3");
    }

    #[gpui::test]
    fn alt_arrows_move_by_word_and_home_and_end_walk_the_line(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "  image: nginx");
        click_at(cx, point(px(5.), px(9.)));
        cx.simulate_keystrokes("alt-right");
        assert_eq!(
            view.read_with(cx, |view, _| view.caret().0),
            2,
            "from the indent the caret reaches the first word"
        );
        cx.simulate_keystrokes("alt-right");
        assert_eq!(
            view.read_with(cx, |view, _| view.caret().0),
            7,
            "inside a word the caret reaches its end"
        );
        cx.simulate_keystrokes("alt-left");
        assert_eq!(view.read_with(cx, |view, _| view.caret().0), 2);
        cx.simulate_keystrokes("alt-left");
        assert_eq!(view.read_with(cx, |view, _| view.caret().0), 0);
        cx.simulate_keystrokes("secondary-end");
        assert_eq!(view.read_with(cx, |view, _| view.caret().0), 14);
        cx.simulate_keystrokes("secondary-home");
        assert_eq!(view.read_with(cx, |view, _| view.caret().0), 0);
    }

    #[gpui::test]
    fn the_shortcut_list_opens_from_the_keyboard_and_closes(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.focus(window, cx));
        });
        assert!(!view.read_with(cx, |view, _| view.cheat_sheet_visible));
        cx.simulate_keystrokes("secondary-slash");
        assert!(
            view.read_with(cx, |view, _| view.cheat_sheet_visible),
            "Ctrl+/ lists the editor keys, which the keymap does not publish"
        );
        cx.simulate_keystrokes("escape");
        assert!(!view.read_with(cx, |view, _| view.cheat_sheet_visible));
    }

    fn focus_replace(view: &gpui::Entity<YamlView>, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.focus_replace(window, cx));
        });
    }

    #[gpui::test]
    fn the_replace_field_replaces_the_active_match_and_then_every_match(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "image: coredns\nimage: coredns");
        view.update(cx, |view, cx| view.set_search_query("coredns", cx));
        focus_replace(&view, cx);
        cx.simulate_input("coredns-2");
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.replace_text.clone()),
            "coredns-2",
            "the replace field feeds the document replace"
        );

        cx.simulate_keystrokes("enter");
        assert_eq!(text_of(&view, cx), "image: coredns-2\nimage: coredns");
        cx.simulate_keystrokes("secondary-enter");
        assert_eq!(text_of(&view, cx), "image: coredns-2\nimage: coredns-2");
        // The document owns undo, so the caret goes back to the document first.
        cx.simulate_keystrokes("escape");
        cx.simulate_keystrokes("secondary-z");
        assert_eq!(
            text_of(&view, cx),
            "image: coredns-2\nimage: coredns",
            "replace-all is one undo step"
        );
    }

    #[gpui::test]
    fn the_case_and_regex_toggles_answer_their_keys(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "image: CoreDNS\nimage: coredns");
        focus_search(&view, cx);
        assert!(!view.read_with(cx, |view, _| view.search_options.case_sensitive));
        cx.simulate_keystrokes("alt-c");
        assert!(view.read_with(cx, |view, _| view.search_options.case_sensitive));
        cx.simulate_keystrokes("alt-r");
        assert!(view.read_with(cx, |view, _| view.search_options.regex));
        cx.simulate_keystrokes("alt-c");
        assert!(!view.read_with(cx, |view, _| view.search_options.case_sensitive));
    }

    #[gpui::test]
    fn a_regex_query_matches_and_a_bad_pattern_explains_itself(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "image: coredns-2\nimage: nginx");
        view.update(cx, |view, cx| {
            view.set_search_options(
                SearchOptions {
                    case_sensitive: false,
                    regex: true,
                },
                cx,
            );
            view.set_search_query(r"coredns-\d", cx);
        });
        assert_eq!(view.read_with(cx, |view, _| view.matches.len()), 1);
        view.update(cx, |view, cx| view.set_search_query("(unclosed", cx));
        assert!(view.read_with(cx, |view, _| view.matches.is_empty()));
        let reason = view.read_with(cx, |view, _| view.search_error.clone());
        assert!(
            reason.as_ref().is_some_and(|reason| reason.contains('`')),
            "the search bar says why the pattern cannot run: {reason:?}"
        );
    }

    #[gpui::test]
    fn case_sensitive_search_is_reachable_through_the_view(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "image: CoreDNS\nimage: coredns");
        view.update(cx, |view, cx| view.set_search_query("coredns", cx));
        assert_eq!(view.read_with(cx, |view, _| view.matches.len()), 2);
        view.update(cx, |view, cx| {
            view.set_search_options(
                SearchOptions {
                    case_sensitive: true,
                    regex: false,
                },
                cx,
            )
        });
        assert_eq!(
            view.read_with(cx, |view, _| view.matches.len()),
            1,
            "case sensitivity narrows the same query"
        );
    }

    fn ime_mark(
        view: &gpui::Entity<YamlView>,
        range: Option<std::ops::Range<usize>>,
        text: &str,
        selected: Option<std::ops::Range<usize>>,
        cx: &mut VisualTestContext,
    ) {
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.replace_and_mark_text_in_range(range, text, selected, window, cx);
            });
        });
    }

    // Calls the same input handler path used by platform IME callbacks.
    fn ime_preedit(view: &gpui::Entity<YamlView>, text: &str, cx: &mut VisualTestContext) {
        ime_mark(view, None, text, None, cx);
    }

    fn ime_commit(view: &gpui::Entity<YamlView>, text: &str, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.replace_text_in_range(None, text, window, cx);
            });
        });
    }

    fn ime_marked(
        view: &gpui::Entity<YamlView>,
        cx: &mut VisualTestContext,
    ) -> Option<std::ops::Range<usize>> {
        cx.update(|window, cx| view.update(cx, |view, cx| view.marked_text_range(window, cx)))
    }

    fn search_input(
        view: &gpui::Entity<YamlView>,
        cx: &VisualTestContext,
    ) -> gpui::Entity<TextInput> {
        view.read_with(cx, |view, _| view.search_input.clone())
    }

    fn focus_search(view: &gpui::Entity<YamlView>, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.focus_search(window, cx));
        });
    }

    fn search_ime_mark(
        view: &gpui::Entity<YamlView>,
        range: Option<std::ops::Range<usize>>,
        text: &str,
        selected: Option<std::ops::Range<usize>>,
        cx: &mut VisualTestContext,
    ) {
        let input = search_input(view, cx);
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(range, text, selected, window, cx);
            });
        });
    }

    fn search_ime_commit(view: &gpui::Entity<YamlView>, text: &str, cx: &mut VisualTestContext) {
        let input = search_input(view, cx);
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.replace_text_in_range(None, text, window, cx);
            });
        });
    }

    fn search_ime_cancel(view: &gpui::Entity<YamlView>, cx: &mut VisualTestContext) {
        let input = search_input(view, cx);
        cx.update(|window, cx| {
            input.update(cx, |input, cx| input.unmark_text(window, cx));
        });
    }

    #[gpui::test]
    fn search_selection_is_visible_without_composition(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        view.update(cx, |view, cx| view.set_search_query("name", cx));
        let input = search_input(&view, cx);
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.set_selected_text_range(0..4, window, cx);
                assert_eq!(
                    input
                        .selected_text_range(false, window, cx)
                        .map(|range| range.range),
                    Some(0..4)
                );
            });
        });
    }

    #[gpui::test]
    fn search_composition_uses_shared_input_protocol(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        focus_search(&view, cx);
        view.update(cx, |view, cx| view.set_search_query("a中", cx));
        search_ime_mark(&view, Some(1..2), "𝄞x", Some(0..3), cx);
        let input = search_input(&view, cx);
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                let mut adjusted = None;
                assert_eq!(
                    input
                        .text_for_range(0..4, &mut adjusted, window, cx)
                        .as_deref(),
                    Some("a𝄞x")
                );
                assert!(adjusted.is_none());
                assert_eq!(input.text_length_utf16(window, cx), Some(4));
                assert_eq!(input.marked_text_range(window, cx), Some(1..4));
                assert_eq!(
                    input
                        .selected_text_range(false, window, cx)
                        .map(|selection| selection.range),
                    Some(1..4)
                );
            });
        });
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.query.clone()), "a中");
    }

    #[gpui::test]
    fn search_composition_commit_is_exactly_one_step(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        focus_search(&view, cx);
        view.update(cx, |view, cx| view.set_search_query("ab", cx));
        search_ime_mark(&view, None, "zh", None, cx);
        search_ime_mark(&view, Some(0..99), "zho", None, cx);
        search_ime_commit(&view, "中", cx);
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.query.clone()), "ab中");
        assert_eq!(
            search_input(&view, cx).read_with(cx, |input, _| input.text().to_owned()),
            "ab中"
        );
        cx.simulate_keystrokes("secondary-z");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.query.clone()), "ab");
        cx.simulate_keystrokes("secondary-shift-z");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.query.clone()), "ab中");
    }

    #[gpui::test]
    fn search_composition_cancel_and_clear_leave_no_state(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        focus_search(&view, cx);
        view.update(cx, |view, cx| view.set_search_query("a中", cx));
        search_ime_mark(&view, Some(1..2), "𝄞", Some(0..2), cx);
        search_ime_cancel(&view, cx);
        let input = search_input(&view, cx);
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                assert_eq!(input.text(), "a中");
                assert_eq!(input.marked_text_range(window, cx), None);
                assert_eq!(input.text_length_utf16(window, cx), Some(2));
            });
        });
        search_ime_mark(&view, None, "x", None, cx);
        cx.update(|_, cx| input.update(cx, |input, cx| input.clear(cx)));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.query.clone()), "");
        assert_eq!(input.read_with(cx, |input, _| input.text().to_owned()), "");
    }

    #[gpui::test]
    fn search_enter_does_not_jump_while_composing(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "a\na\na");
        focus_search(&view, cx);
        view.update(cx, |view, cx| view.set_search_query("a", cx));
        search_ime_mark(&view, None, "中", None, cx);
        cx.simulate_keystrokes("enter");
        assert_eq!(view.read_with(cx, |view, _| view.active), 0);
        let input = search_input(&view, cx);
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                assert!(input.marked_text_range(window, cx).is_some());
            });
        });
    }

    #[gpui::test]
    fn long_search_query_uses_shared_input_history(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        let query = "x".repeat(100);
        focus_search(&view, cx);
        view.update(cx, |view, cx| view.set_search_query(query.clone(), cx));
        cx.run_until_parked();
        let input = search_input(&view, cx);
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            query
        );
        cx.simulate_keystrokes("secondary-z");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.query.clone()), "");
    }

    #[gpui::test]
    fn ime_composition_protocol_uses_composed_utf16(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "a");
        ime_mark(&view, None, "中𝄞x", Some(0..3), cx);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                let mut adjusted = None;
                assert_eq!(
                    view.text_for_range(0..4, &mut adjusted, window, cx)
                        .as_deref(),
                    Some("中𝄞x")
                );
                assert!(adjusted.is_none());
                assert_eq!(view.marked_text_range(window, cx), Some(0..4));
                assert_eq!(view.text_length_utf16(window, cx), Some(4));
                assert_eq!(
                    view.selected_text_range(false, window, cx)
                        .map(|selection| selection.range),
                    Some(0..3)
                );
                let bounds = view
                    .bounds_for_range(
                        4..4,
                        Bounds::new(point(px(0.), px(0.)), size(px(300.), px(100.))),
                        window,
                        cx,
                    )
                    .expect("composition caret bounds");
                assert_eq!(
                    view.character_index_for_point(
                        point(bounds.origin.x + px(1.), bounds.origin.y + px(1.)),
                        window,
                        cx,
                    ),
                    Some(4)
                );
            });
        });
        ime_mark(&view, None, "中𝄞x", None, cx);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                assert_eq!(
                    view.selected_text_range(false, window, cx)
                        .map(|selection| selection.range),
                    Some(0..4)
                );
            });
        });
    }

    #[gpui::test]
    fn empty_buffer_has_a_complete_ime_display_protocol(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| YamlView::new(cx));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.focus(window, cx));
        });
        ime_mark(&view, None, "中", None, cx);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                let mut adjusted = None;
                assert_eq!(
                    view.text_for_range(0..1, &mut adjusted, window, cx)
                        .as_deref(),
                    Some("中")
                );
                assert!(adjusted.is_none());
                assert_eq!(view.marked_text_range(window, cx), Some(0..1));
                assert_eq!(view.text_length_utf16(window, cx), Some(1));
                assert_eq!(
                    view.selected_text_range(false, window, cx)
                        .map(|selection| selection.range),
                    Some(0..1)
                );
                assert!(
                    view.bounds_for_range(
                        1..1,
                        Bounds::new(point(px(0.), px(0.)), size(px(300.), px(100.))),
                        window,
                        cx,
                    )
                    .is_some()
                );
            });
        });
        ime_commit(&view, "中", cx);
        assert_eq!(text_of(&view, cx), "中");
    }

    #[gpui::test]
    fn cjk_hit_testing_and_selection_use_utf16(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "中文");
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.set_selected_text_range(1..2, window, cx);
                let bounds = view
                    .bounds_for_range(
                        1..1,
                        Bounds::new(point(px(0.), px(0.)), size(px(300.), px(100.))),
                        window,
                        cx,
                    )
                    .expect("CJK caret bounds");
                assert_eq!(
                    view.character_index_for_point(
                        point(bounds.origin.x + px(1.), bounds.origin.y + px(1.)),
                        window,
                        cx,
                    ),
                    Some(1)
                );
                assert_eq!(
                    view.selected_text_range(false, window, cx)
                        .map(|selection| selection.range),
                    Some(1..2)
                );
            });
        });
    }

    #[gpui::test]
    fn enter_does_not_edit_or_cancel_an_active_composition(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "a\nb");
        click_at(cx, point(px(60.), px(9.)));
        ime_preedit(&view, "zhong", cx);
        cx.simulate_keystrokes("enter");
        assert_eq!(text_of(&view, cx), "a\nb");
        assert_eq!(ime_marked(&view, cx), Some(1..6));
    }

    #[gpui::test]
    fn search_focus_hides_the_document_caret(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "a");
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.focus(window, cx);
                assert!(view.document_focused(window, cx));
                view.focus_search(window, cx);
                assert!(!view.document_focused(window, cx));
            });
        });
    }

    #[gpui::test]
    fn ime_preedit_updates_marked_range_and_commit_replaces_it(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "k: ");
        click_at(cx, point(px(60.), px(9.)));
        ime_preedit(&view, "zhong", cx);
        assert_eq!(text_of(&view, cx), "k: ");
        assert_eq!(ime_marked(&view, cx), Some(3..8));
        ime_preedit(&view, "zhongg", cx);
        assert_eq!(text_of(&view, cx), "k: ");
        assert_eq!(ime_marked(&view, cx), Some(3..9));
        ime_commit(&view, "中", cx);
        assert_eq!(text_of(&view, cx), "k: 中");
        assert_eq!(ime_marked(&view, cx), None);
    }

    #[gpui::test]
    fn typing_while_composing_commits_the_preedit(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "k: ");
        click_at(cx, point(px(60.), px(9.)));
        ime_preedit(&view, "´", cx);
        assert_eq!(text_of(&view, cx), "k: ");
        cx.simulate_input("é");
        assert_eq!(text_of(&view, cx), "k: é");
        assert_eq!(ime_marked(&view, cx), None);
    }

    #[gpui::test]
    fn one_composition_is_one_undo_step(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "k: ");
        click_at(cx, point(px(60.), px(9.)));
        ime_preedit(&view, "zh", cx);
        ime_preedit(&view, "zho", cx);
        ime_preedit(&view, "zhong", cx);
        ime_commit(&view, "中", cx);
        assert_eq!(text_of(&view, cx), "k: 中");
        cx.simulate_keystrokes("secondary-z");
        assert_eq!(text_of(&view, cx), "k: ", "composition is one undo step");
    }

    #[gpui::test]
    fn ime_cancel_discards_preedit_without_touching_undo(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "k: ");
        click_at(cx, point(px(60.), px(9.)));
        ime_preedit(&view, "zhong", cx);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.unmark_text(window, cx));
        });
        assert_eq!(text_of(&view, cx), "k: ");
        cx.simulate_input("x");
        assert_eq!(text_of(&view, cx), "k: x");
        cx.simulate_keystrokes("secondary-z");
        assert_eq!(text_of(&view, cx), "k: ");
    }

    #[gpui::test]
    fn ime_utf16_ranges_handle_wide_characters(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "中: 文");
        // Both clicks are measured from the text column, so they do not depend on the
        // gutter width or the font size the settings happen to give the test.
        let column = text_column_x(&view, cx);
        let unit = cx.update(|_, cx| char_width(&DataTypography::from_theme_settings(cx)));
        // The product font draws 中, the colon, the space and 文 as four glyphs of one
        // advance each, so a click three and a half cells in is inside the fullwidth
        // character. Its three UTF-8 bytes are one glyph, so the caret takes the leading
        // edge: a caret between the bytes is a position no click reaches and no key leaves.
        click_at(cx, point(column + unit * 3.5, px(9.)));
        assert_eq!(
            view.read_with(cx, |view, _| view.caret().0),
            "中: ".len(),
            "the click on the fullwidth character lands on its leading edge"
        );
        // Past the last glyph the caret goes to the end of the line.
        click_at(cx, point(column + unit * 5.5, px(9.)));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                let mut adjusted = None;
                assert_eq!(
                    view.text_for_range(0..2, &mut adjusted, window, cx),
                    Some("中:".to_owned()),
                    "two UTF-16 code units cover four UTF-8 bytes"
                );
                assert!(
                    adjusted.is_none(),
                    "a character boundary needs no adjustment"
                );
                let adjusted_to = view
                    .selected_text_range(false, window, cx)
                    .map(|selection| selection.range);
                assert_eq!(
                    adjusted_to,
                    Some(4..4),
                    "the end of the line is four UTF-16 code units in"
                );
            });
        });
    }
}
