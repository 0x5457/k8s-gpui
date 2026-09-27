use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, Context, ElementInputHandler, Entity, FocusHandle, Focusable,
    InputHandler, IntoElement, KeyBinding, KeyDownEvent, NoAction, ParentElement, Point, Render,
    Role, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Subscription,
    TextInputConfiguration, UTF16Selection, Window, canvas, div, px,
};
use k8s_core::fuzzy;
use ui::prelude::*;
use ui::{
    Clickable, ContextMenu, Indicator, ListItem, ListItemSpacing, PopoverMenuHandle, Toggleable,
};

use crate::design;
use crate::panels::common::label_panel_title;
use crate::table_view::TextInput;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerKind {
    Cluster,
    Namespace,
}

impl PickerKind {
    fn name(self) -> &'static str {
        match self {
            Self::Cluster => "contexts",
            Self::Namespace => "namespaces",
        }
    }

    fn singular(self) -> &'static str {
        match self {
            Self::Cluster => "context",
            Self::Namespace => "namespace",
        }
    }

    /// The task this picker does, in the words a user would use for it.
    ///
    /// The picker covers the toolbar behind it, so a title that repeated the
    /// cluster, the namespace, and the open view would add nothing and could only
    /// truncate. `modality.md > Best practices` asks a modal view for a title that
    /// names its task, so that is all this carries.
    fn title(self) -> &'static str {
        match self {
            Self::Cluster => "Switch context",
            Self::Namespace => "Switch namespace",
        }
    }

    fn search_label(self) -> &'static str {
        match self {
            Self::Cluster => "Search contexts",
            Self::Namespace => "Search namespaces",
        }
    }

    fn clear_label(self) -> &'static str {
        match self {
            Self::Cluster => "Clear context search",
            Self::Namespace => "Clear namespace search",
        }
    }

    fn results_label(self) -> &'static str {
        match self {
            Self::Cluster => "Context results",
            Self::Namespace => "Namespace results",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PickerOption {
    pub value: SharedString,
    pub label: SharedString,
    pub source: Option<SharedString>,
    pub detail: Option<SharedString>,
    pub status: Option<design::Severity>,
    pub status_label: Option<SharedString>,
    pub icon: Option<ui::IconName>,
    pub current: bool,
    pub debug_selector: Option<SharedString>,
}

impl PickerOption {
    pub fn new(value: impl Into<SharedString>, label: impl Into<SharedString>) -> Self {
        let value = value.into();
        Self {
            label: label.into(),
            value,
            source: None,
            detail: None,
            status: None,
            status_label: None,
            icon: None,
            current: false,
            debug_selector: None,
        }
    }
}

pub(super) fn disambiguate_options(options: &mut [PickerOption]) {
    let mut counts = HashMap::<String, usize>::new();
    for option in options.iter() {
        *counts.entry(option.label.to_lowercase()).or_default() += 1;
    }
    for option in options {
        if counts
            .get(&option.label.to_lowercase())
            .copied()
            .unwrap_or(0)
            < 2
        {
            continue;
        }
        let suffix = option
            .source
            .clone()
            .unwrap_or_else(|| SharedString::from(format!("Context {}", option.value)));
        option.label = SharedString::from(format!("{} · {suffix}", option.label));
    }
}

/// The label prefix that every option shares, cut at a word boundary.
///
/// A picker row is often a whole sentence that repeats on every row — a verb, a
/// subject, a breadcrumb. Those words say how the option is reached, not what it
/// is, and a query made of any of them matches every row, which leaves the
/// search box with nothing to narrow. The longest common prefix, trimmed back to
/// a word boundary and only when every row keeps a word, is exactly that
/// decoration.
fn shared_label_prefix(options: &[PickerOption]) -> &str {
    let Some(first) = options.first() else {
        return "";
    };
    let mut shared = first.label.as_ref();
    for option in &options[1..] {
        if shared.is_empty() {
            break;
        }
        // The common length is a character boundary in both labels, because it is
        // a run of whole bytes that matched.
        let common = shared
            .bytes()
            .zip(option.label.as_ref().bytes())
            .take_while(|(left, right)| left == right)
            .count();
        if common == 0 {
            return "";
        }
        shared = &shared[..common];
    }
    // Only a whole word is decoration: a cut in the middle of a word would make
    // the remainder unreadable, and cutting a whole label would leave nothing to
    // search.
    let cut = shared
        .rfind([' ', ':'])
        .map_or("", |index| &shared[..=index]);
    if cut.is_empty() || options.iter().any(|option| option.label.len() <= cut.len()) {
        return "";
    }
    cut
}

/// The text a query is matched against.
///
/// Built from the identifying fields only — the noun in the label, the value,
/// the source, the status, and the detail — with the shared prefix removed. A
/// row's own `value` is always in the target, so dropping a prefix can never
/// make a row unfindable.
fn searchable_text(option: &PickerOption, decoration: &str) -> String {
    let label = option
        .label
        .strip_prefix(decoration)
        .unwrap_or(option.label.as_ref());
    let mut fields: Vec<&str> = vec![label];
    if option.value.as_ref() != label {
        fields.push(option.value.as_ref());
    }
    fields.extend([
        option.source.as_deref().unwrap_or_default(),
        option.status_label.as_deref().unwrap_or_default(),
        option.detail.as_deref().unwrap_or_default(),
    ]);
    fields
        .into_iter()
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn filter_options(options: &[PickerOption], query: &str) -> Vec<PickerOption> {
    // The decoration is a property of the whole set, so it is resolved once here
    // instead of being guessed again for every row.
    let decoration = shared_label_prefix(options);
    let searchable = options
        .iter()
        .map(|option| searchable_text(option, decoration))
        .collect::<Vec<_>>();
    let query = query.trim().to_lowercase();
    fuzzy::rank(&query, searchable.iter().map(String::as_str))
        .into_iter()
        .filter_map(|ranked| options.get(ranked.index).cloned())
        .collect()
}

pub(super) fn selected_index(options: &[PickerOption], selected: Option<&str>) -> Option<usize> {
    let selected = selected?;
    options
        .iter()
        .position(|option| option.value.as_ref() == selected)
}

pub(super) fn move_selection(
    options: &[PickerOption],
    selected: Option<&str>,
    delta: isize,
) -> Option<SharedString> {
    if options.is_empty() || delta == 0 {
        return None;
    }
    let next = match selected_index(options, selected) {
        Some(current) => current.saturating_add_signed(delta).min(options.len() - 1),
        None if delta.is_negative() => options.len() - 1,
        None => 1.min(options.len() - 1),
    };
    options.get(next).map(|option| option.value.clone())
}

/// The count beside the card title.
///
/// `results` is the word the command palette and this card's own list label
/// already use for this job, and the title directly above it says whether the
/// results are contexts or namespaces. Spelling the noun out here gave one task
/// three phrasings across the two sibling pickers, and the `Namespaces` it
/// produced also broke the sentence-case rule `DESIGN.md` §4 sets.
pub(super) fn result_label(visible: usize, total: usize) -> String {
    if visible == total {
        design::format::count_with_noun(visible, "result", "results")
    } else {
        // The head noun stays plural here: "1 of 21 results" is what the reader
        // means, and pluralising the whole line by the visible count would give
        // "1 result of 21 results".
        format!(
            "{} of {} results",
            design::format::count(visible),
            design::format::count(total)
        )
    }
}

fn empty_title(kind: PickerKind, query: &str) -> &'static str {
    if query.trim().is_empty() {
        match kind {
            PickerKind::Cluster => "No contexts available",
            PickerKind::Namespace => "No namespaces available",
        }
    } else {
        match kind {
            PickerKind::Cluster => "No matching contexts",
            PickerKind::Namespace => "No matching namespaces",
        }
    }
}

/// The keys this picker answers, one short item each.
///
/// A single run-on line truncates at the picker's width, and a truncated hint is
/// a hint nobody can read. The two Escape steps are listed separately because
/// they are two steps: the first press clears the field, the next one closes the
/// picker.
fn footer_hints(query: &str) -> Vec<String> {
    let mut hints = vec!["↑↓ Navigate".to_owned(), "Enter Select".to_owned()];
    if query.trim().is_empty() {
        hints.push("Esc Close".to_owned());
    } else {
        hints.push("Esc Clear search".to_owned());
        hints.push("Esc Close".to_owned());
    }
    hints
}

fn label(value: impl Into<SharedString>, size: gpui::Pixels) -> Label {
    Label::new(value).size(LabelSize::Custom(rems_from_px(f32::from(size))))
}

pub(super) type PickerSelectHandler = Rc<dyn Fn(SharedString, &mut Window, &mut App)>;
pub(super) type PickerDismissHandler = Rc<dyn Fn(&mut Window, &mut App)>;

pub(super) struct PickerTrigger {
    button: Button,
    handle: PopoverMenuHandle<ContextMenu>,
}

impl PickerTrigger {
    pub fn new(button: Button, handle: PopoverMenuHandle<ContextMenu>) -> Self {
        Self { button, handle }
    }
}

impl Clickable for PickerTrigger {
    fn on_click(
        mut self,
        handler: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.button = self.button.on_click(handler);
        self
    }

    fn cursor_style(mut self, cursor_style: gpui::CursorStyle) -> Self {
        self.button = self.button.cursor_style(cursor_style);
        self
    }
}

impl Toggleable for PickerTrigger {
    fn toggle_state(mut self, selected: bool) -> Self {
        self.button = self.button.toggle_state(selected);
        self
    }
}

impl IntoElement for PickerTrigger {
    type Element = AnyElement;

    fn into_element(self) -> Self::Element {
        let expanded = self.handle.is_deployed();
        let handle = self.handle;
        div()
            .capture_key_down(
                move |event, window, cx| match event.keystroke.key.as_str() {
                    "down" => {
                        handle.show(window, cx);
                        window.prevent_default();
                        cx.stop_propagation();
                    }
                    "enter" | "return" | "space" => {
                        handle.toggle(window, cx);
                        window.prevent_default();
                        cx.stop_propagation();
                    }
                    _ => {}
                },
            )
            .child(self.button.aria_expanded(expanded))
            .into_any_element()
    }
}

pub(super) struct PickerConfig {
    pub kind: PickerKind,
    pub options: Vec<PickerOption>,
    pub query: Option<String>,
    pub input: Option<Entity<TextInput>>,
    pub on_select: PickerSelectHandler,
    pub on_dismiss: PickerDismissHandler,
}

fn utf16_to_byte(text: &str, target: usize) -> usize {
    let mut utf16 = 0;
    for (byte, ch) in text.char_indices() {
        if utf16 >= target {
            return byte;
        }
        utf16 += ch.len_utf16();
    }
    text.len()
}

struct PickerInputHandler {
    input: Entity<TextInput>,
    inner: ElementInputHandler<TextInput>,
}

impl InputHandler for PickerInputHandler {
    fn selected_text_range(
        &mut self,
        ignore_disabled_input: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<UTF16Selection> {
        self.inner
            .selected_text_range(ignore_disabled_input, window, cx)
    }

    fn marked_text_range(&mut self, window: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        self.inner.marked_text_range(window, cx)
    }

    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<String> {
        self.inner
            .text_for_range(range_utf16, adjusted_range, window, cx)
    }

    fn replace_text_in_range(
        &mut self,
        replacement_range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        let range = replacement_range.clone().or_else(|| {
            self.inner
                .selected_text_range(false, window, cx)
                .map(|selection| selection.range)
        });
        if let Some(range) = range
            && range.start == range.end
        {
            let end = range.start + text.encode_utf16().count();
            self.input.update(cx, |input, cx| {
                let mut value = input.text().to_owned();
                let start = utf16_to_byte(&value, range.start);
                value.insert_str(start, text);
                input.set_text(value, cx);
            });
            self.inner.set_selected_text_range(end..end, window, cx);
            return;
        }
        self.inner
            .replace_text_in_range(replacement_range, text, window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner.replace_and_mark_text_in_range(
            range_utf16,
            new_text,
            new_selected_range,
            window,
            cx,
        );
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut App) {
        self.inner.unmark_text(window, cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Bounds<gpui::Pixels>> {
        self.inner.bounds_for_range(range_utf16, window, cx)
    }

    fn character_index_for_point(
        &mut self,
        point: Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<usize> {
        self.inner.character_index_for_point(point, window, cx)
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner.set_selected_text_range(range_utf16, window, cx);
    }

    fn element_bounds(
        &mut self,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Bounds<gpui::Pixels>> {
        self.inner.element_bounds(window, cx)
    }

    fn text_length_utf16(&mut self, window: &mut Window, cx: &mut App) -> Option<usize> {
        self.inner.text_length_utf16(window, cx)
    }

    fn accepts_text_input(&mut self, window: &mut Window, cx: &mut App) -> bool {
        self.inner.accepts_text_input(window, cx)
    }

    fn text_input_configuration(
        &mut self,
        window: &mut Window,
        cx: &mut App,
    ) -> TextInputConfiguration {
        self.inner.text_input_configuration(window, cx)
    }

    fn prefers_ime_for_printable_keys(&mut self, window: &mut Window, cx: &mut App) -> bool {
        self.inner.prefers_ime_for_printable_keys(window, cx)
    }
}

pub(super) struct SearchablePicker {
    input: Entity<TextInput>,
    _input_observation: Subscription,
    options: Vec<PickerOption>,
    kind: PickerKind,
    query: String,
    current: Option<SharedString>,
    selected: Option<SharedString>,
    scroll: ScrollHandle,
    focus_handle: FocusHandle,
    /// The empty state's next step. A real control, because an empty state that
    /// only describes the problem leaves the reader to guess the repair.
    clear_search_focus: FocusHandle,
    on_select: PickerSelectHandler,
    on_dismiss: PickerDismissHandler,
}

impl SearchablePicker {
    pub fn new(config: PickerConfig, cx: &mut Context<Self>) -> Self {
        cx.bind_keys(
            ["up", "down", "home", "end", "enter", "escape"]
                .into_iter()
                .map(|key| KeyBinding::new(key, NoAction, Some("SearchablePicker"))),
        );
        let view = cx.weak_entity();
        let input = config.input.unwrap_or_else(|| {
            cx.new(|cx| {
                let view = view.clone();
                TextInput::new(
                    config.kind.search_label(),
                    cx,
                    move |text, cx| {
                        let view = view.clone();
                        let text = text.to_owned();
                        cx.defer(move |cx| {
                            view.update(cx, |view, cx| view.set_query(&text, cx))
                                .ok();
                        });
                    },
                )
                .with_accessibility(
                    config.kind.search_label(),
                    format!(
                        "Type to filter {}. Use Up and Down to navigate. Press Enter to select. Press Escape to clear the search or close the list.",
                        config.kind.name()
                    ),
                    config.kind.clear_label(),
                )
                .with_width(px(
                    f32::from(design::size::INSPECTOR_MIN)
                        - f32::from(design::space::MD) * 2.0,
                ))
                .without_escape_hint()
            })
        });
        let input_observation = cx.observe(&input, |view, _, cx| {
            if view.sync_input(cx) {
                cx.notify();
            }
        });
        let configured_query = config.query.clone();
        let query = configured_query
            .clone()
            .unwrap_or_else(|| input.read(cx).text().to_owned());
        if let Some(query) = configured_query
            && input.read(cx).text() != query
        {
            input.update(cx, |input, cx| input.set_text(query, cx));
        }
        let current = config
            .options
            .iter()
            .find(|option| option.current)
            .map(|option| option.value.clone());
        let selected = current.clone();
        let mut picker = Self {
            input,
            _input_observation: input_observation,
            options: config.options,
            kind: config.kind,
            query,
            current,
            selected,
            scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            clear_search_focus: cx.focus_handle().tab_stop(true).tab_index(2isize),
            on_select: config.on_select,
            on_dismiss: config.on_dismiss,
        };
        picker.ensure_selection();
        picker
    }

    pub fn input_focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }

    fn visible_options(&self) -> Vec<PickerOption> {
        filter_options(&self.options, &self.query)
    }

    fn sync_input(&mut self, cx: &App) -> bool {
        let query = self.input.read(cx).text().to_owned();
        if query == self.query {
            return false;
        }
        self.query = query;
        self.ensure_selection();
        true
    }

    fn set_query(&mut self, query: &str, cx: &mut Context<Self>) {
        if query == self.query {
            return;
        }
        self.query = query.to_owned();
        self.ensure_selection();
        cx.notify();
    }

    fn ensure_selection(&mut self) {
        let visible = self.visible_options();
        if selected_index(&visible, self.selected.as_deref()).is_none() {
            self.selected = self
                .current
                .clone()
                .filter(|current| {
                    visible
                        .iter()
                        .any(|option| option.value.as_ref() == current.as_ref())
                })
                .or_else(|| visible.first().map(|option| option.value.clone()));
        }
        self.reveal_selection();
    }

    fn reveal_selection(&self) {
        let visible = self.visible_options();
        if let Some(index) = selected_index(&visible, self.selected.as_deref()) {
            self.scroll.scroll_to_item(index);
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let visible = self.visible_options();
        if let Some(selected) = move_selection(&visible, self.selected.as_deref(), delta) {
            self.selected = Some(selected);
            self.reveal_selection();
            cx.notify();
        }
    }

    fn select_value(&mut self, value: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        if !self
            .visible_options()
            .iter()
            .any(|option| option.value == value)
        {
            return;
        }
        self.selected = Some(value.clone());
        (self.on_select)(value, window, cx);
        self.dismiss(window, cx);
    }

    fn clear_query(&mut self, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.clear(cx));
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
        (self.on_dismiss)(window, cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.alt || event.keystroke.modifiers.function {
            return;
        }
        let command_edge = event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
        if command_edge && !matches!(event.keystroke.key.as_str(), "home" | "end") {
            return;
        }
        let input_focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        let clear_focused = self.input.read(cx).clear_focus_handle().is_focused(window);
        let clear_search_focused = self.clear_search_focus.is_focused(window);
        if clear_focused {
            // The clear button is a real control and answers these itself.
            return;
        }
        if clear_search_focused
            && !matches!(
                event.keystroke.key.as_str(),
                "enter" | "return" | "space" | "escape"
            )
        {
            // Same for the empty state's own clear control, except that the
            // picker owns its activation instead of the button.
            return;
        }
        if input_focused
            && !command_edge
            && matches!(event.keystroke.key.as_str(), "home" | "end" | "space")
        {
            // The caret belongs to the search field while it has focus.
            return;
        }
        match event.keystroke.key.as_str() {
            "escape" => {
                if self.query.is_empty() {
                    self.dismiss(window, cx);
                } else {
                    self.clear_query(cx);
                }
                cx.stop_propagation();
            }
            "up" => {
                self.move_selection(-1, cx);
                cx.stop_propagation();
            }
            "down" => {
                self.move_selection(1, cx);
                cx.stop_propagation();
            }
            "home" => {
                self.selected = self
                    .visible_options()
                    .first()
                    .map(|option| option.value.clone());
                self.reveal_selection();
                cx.notify();
                cx.stop_propagation();
            }
            "end" => {
                self.selected = self
                    .visible_options()
                    .last()
                    .map(|option| option.value.clone());
                self.reveal_selection();
                cx.notify();
                cx.stop_propagation();
            }
            "enter" | "return" | "space" => {
                if clear_search_focused {
                    self.clear_query(cx);
                } else {
                    self.ensure_selection();
                    if let Some(value) = self.selected.clone() {
                        self.select_value(value, window, cx);
                    }
                }
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    /// The state the picker shows when the list has nothing in it.
    ///
    /// `writing.md > Best practices` asks an empty screen to guide people on what
    /// they can do and to give them a control for it where one exists, so a
    /// search that matched nothing offers the clear that repairs it.
    fn empty_state(
        &self,
        title: &'static str,
        guidance: &'static str,
        can_clear: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let clear_search = can_clear.then(|| {
            div()
                .id("searchable-picker-clear-search")
                .debug_selector(|| "searchable-picker-clear-search".to_owned())
                .child(
                    Button::new("searchable-picker-clear-button", "Clear search")
                        .style(ButtonStyle::OutlinedGhost)
                        .size(ButtonSize::Medium)
                        .track_focus(&self.clear_search_focus)
                        .tab_index(2isize)
                        .aria_label("Clear Picker Search")
                        .on_click(cx.listener(|view, _, _, cx| view.clear_query(cx))),
                )
        });
        v_flex()
            .w_full()
            .id("searchable-picker-empty")
            .role(Role::Status)
            .aria_label(title)
            .aria_description(guidance)
            .items_center()
            .gap(design::space::SM)
            .px(design::space::MD)
            .py(design::space::LG)
            .child(
                Icon::new(IconName::MagnifyingGlass)
                    .size(IconSize::Custom(rems_from_px(f32::from(
                        design::size::ICON_LARGE,
                    ))))
                    .color(Color::Muted),
            )
            .child(label(title, design::text::BODY).color(Color::Muted))
            .child(
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .child(label(guidance, design::text::METADATA).color(Color::Muted)),
            )
            .when_some(clear_search, |this, clear| this.child(clear))
            .into_any_element()
    }

    fn row(&self, index: usize, option: PickerOption, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.selected.as_deref() == Some(option.value.as_ref());
        let value = option.value.clone();
        let current = option.current;
        let label_text = option.label.clone();
        let detail = option.detail.clone();
        let status = option.status;
        let status_label = option.status_label.clone();
        let icon = option.icon;
        let aria_label = match (&status_label, &detail) {
            (Some(status), Some(detail)) => format!("{label_text}, {status}. {detail}"),
            (Some(status), None) => format!("{label_text}, {status}"),
            (None, Some(detail)) => format!("{label_text}, {detail}"),
            (None, None) => label_text.to_string(),
        };
        let aria_label = if current {
            format!("Current {}: {aria_label}", self.kind.singular())
        } else {
            aria_label
        };
        let mut leading = h_flex().gap(design::space::XS).flex_none();
        if let Some(icon) = icon {
            leading = leading.child(Icon::new(icon).size(IconSize::XSmall).color(if selected {
                Color::Default
            } else {
                Color::Muted
            }));
        }
        if let Some(status) = status {
            leading = leading.child(Indicator::dot().color(Color::Custom(status.marker(cx))));
        }
        let trailing = h_flex()
            .gap(design::space::SM)
            .max_w(px(f32::from(design::size::SIDEBAR_MIN)))
            .overflow_hidden()
            .flex_none()
            .when_some(status_label, |this, status| {
                this.child(label(status, design::text::METADATA).color(Color::Muted))
            })
            .when_some(detail, |this, detail| {
                this.child(
                    label(detail, design::text::METADATA)
                        .color(Color::Muted)
                        .truncate(),
                )
            })
            .when(current, |this| {
                this.child(
                    Icon::new(IconName::Check)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
            });
        let row =
            ListItem::new(("searchable-picker-option", index))
                .height(design::size::ROW)
                .spacing(ListItemSpacing::ExtraDense)
                .inset(true)
                // `ListItem` has no `selected_style`, and its own selected fill is
                // an opaque neutral wash drawn over whatever the row painted, so
                // `toggle_state` meant this was the only list in the app whose
                // selection was not `DESIGN.md §3.4`'s token. The row below owns
                // the fill, the rail and `aria_selected`; the list only needs to
                // stop drawing its own.
                .selectable(false)
                .aria_role(Role::ListBoxOption)
                .aria_label(aria_label)
                .when(selected, |this| this.aria_active_descendant())
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.select_value(value.clone(), window, cx)
                }))
                .start_slot(leading)
                .child(
                    h_flex()
                        .min_w(px(0.0))
                        .flex_1()
                        .child(label(label_text, design::text::BODY).truncate()),
                )
                .end_slot(trailing);
        // The row owns its selection, because the list no longer does. The fill is
        // solved against `surface::raised` -- the surface these rows actually paint
        // on -- rather than the table's base, so it clears the same floor the
        // sidebar and the table clear on their own surfaces.
        let selector = option.debug_selector.clone();
        let row: AnyElement = div()
            .w_full()
            .when_some(selector, |this, selector| {
                this.debug_selector(move || selector.to_string())
            })
            .when(selected, |this| {
                this.bg(design::row_selected_bg_on(cx, design::surface::raised(cx)))
            })
            .child(row)
            .into_any_element();
        row
    }
}

impl Render for SearchablePicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = self.sync_input(cx);
        let border_variant = cx.theme().colors().border_variant;
        let visible = self.visible_options();
        let total = self.options.len();
        let count = result_label(visible.len(), total);
        let query = self.query.clone();
        let hints = footer_hints(&query);
        let current = self.current.as_ref().and_then(|current| {
            self.options
                .iter()
                .find(|option| option.value.as_ref() == current.as_ref())
                .map(|option| option.label.clone())
        });
        let input = self.input.clone();
        let input_handler = self.input.clone();
        let input_focus = self.input.read(cx).focus_handle(cx);
        let title = empty_title(self.kind, &query);
        let empty_guidance = if query.trim().is_empty() {
            match self.kind {
                PickerKind::Cluster => {
                    "Add a context to a kubeconfig file, then reload kubeconfigs."
                }
                PickerKind::Namespace => "Refresh the namespace list, then try again.",
            }
        } else {
            "Clear the search, or type another name."
        };
        let rows = visible
            .clone()
            .into_iter()
            .enumerate()
            .map(|(index, option)| self.row(index, option, cx))
            .collect::<Vec<_>>();
        let content = if visible.is_empty() {
            vec![self.empty_state(title, empty_guidance, !query.trim().is_empty(), cx)]
        } else {
            rows
        };
        v_flex()
            .id("searchable-picker")
            .debug_selector(|| "searchable-picker".to_owned())
            .w(px(f32::from(design::size::INSPECTOR_MIN)))
            .min_w(px(0.0))
            .max_w_full()
            // One boundary, and it is the popover's. `elevation_2` already gives
            // this card the raised surface, the radius, a 1px `border_variant` and
            // a shadow, so a second line in the same token one step inside it was
            // two surfaces faking a level they did not have. The command palette's
            // card is bounded the same way.
            .bg(design::surface::raised(cx).alpha(1.0))
            .role(Role::Group)
            .aria_label(self.kind.title())
            .aria_keyshortcuts("ArrowUp ArrowDown Home End Enter Escape")
            .key_context("SearchablePicker")
            .track_focus(&self.focus_handle)
            .capture_key_down(cx.listener(Self::on_key_down))
            // The title names the task, and the count sits beside it. On the field
            // row a count reads as part of the field's value, and a title long
            // enough to carry the scope as well can only truncate.
            .child(
                h_flex()
                    .id("searchable-picker-title")
                    .debug_selector(|| "searchable-picker-title".to_owned())
                    .w_full()
                    .flex_none()
                    .gap(design::space::SM)
                    .items_center()
                    .px(design::space::MD)
                    .pt(design::space::SM)
                    .child(label_panel_title(self.kind.title()))
                    .child(div().flex_1())
                    .child(
                        h_flex().min_w(px(0.0)).flex_shrink_1().child(
                            label(count, design::text::METADATA)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .relative()
                    .min_w(px(0.0))
                    .px(design::space::MD)
                    .py(design::space::SM)
                    .child(input)
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |bounds, _, window, cx| {
                                window.handle_input(
                                    &input_focus,
                                    PickerInputHandler {
                                        input: input_handler.clone(),
                                        inner: ElementInputHandler::new(
                                            bounds,
                                            input_handler.clone(),
                                        ),
                                    },
                                    cx,
                                );
                            },
                        )
                        .absolute()
                        .inset_0(),
                    ),
            )
            // The scope gets its own line under the field, where it has the width
            // to spell out and to wrap. Phrased the way the current row is
            // phrased, so the two read as the same fact.
            .when_some(current, |this, current| {
                this.child(
                    h_flex()
                        .id("searchable-picker-scope")
                        .debug_selector(|| "searchable-picker-scope".to_owned())
                        .w_full()
                        .flex_none()
                        .min_w(px(0.0))
                        .px(design::space::MD)
                        .pb(design::space::SM)
                        .child(
                            div().w_full().min_w(px(0.0)).child(
                                label(format!("Current: {current}"), design::text::METADATA)
                                    .color(Color::Muted),
                            ),
                        ),
                )
            })
            .child(
                v_flex()
                    .id("searchable-picker-results")
                    .debug_selector(|| "searchable-picker-results".to_owned())
                    .w_full()
                    .flex_1()
                    .min_h(px(0.0))
                    .max_h(px(f32::from(design::size::ROW) * 8.0))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .role(Role::ListBox)
                    .aria_label(self.kind.results_label())
                    .children(content),
            )
            .child(
                h_flex()
                    .id("searchable-picker-footer")
                    .debug_selector(|| "searchable-picker-footer".to_owned())
                    .w_full()
                    .flex_none()
                    .min_w(px(0.0))
                    .px(design::space::MD)
                    .py(design::space::SM)
                    // One item per key, with a gap instead of a separator: the
                    // footer wraps at the picker's width rather than truncating
                    // a run-on line.
                    .gap(design::space::MD)
                    .flex_wrap()
                    .border_t_1()
                    .border_color(border_variant)
                    .children(
                        hints
                            .into_iter()
                            .map(|hint| label(hint, design::text::METADATA).color(Color::Muted)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use gpui::{TestAppContext, VisualTestContext};
    use theme::LoadThemes;

    use super::*;

    fn option(value: &str, label: &str) -> PickerOption {
        PickerOption::new(value, label)
    }

    #[test]
    fn filtering_matches_name_value_and_detail() {
        let mut options = vec![
            option("alpha", "Alpha"),
            option("beta", "Beta"),
            option("gamma", "Gamma"),
        ];
        options[1].detail = Some("https://beta.example".into());
        let filtered = filter_options(&options, "BETA   example");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].value, "beta");
    }

    #[test]
    fn filtering_supports_substrings_and_subsequences() {
        let options = vec![
            option("kube-system", "kube-system"),
            option("public", "public"),
            option("web", "web"),
        ];
        let prefix = filter_options(&options, "kube");
        assert_eq!(prefix[0].value, "kube-system");
        let substring = filter_options(&options, "system");
        assert_eq!(substring[0].value, "kube-system");
        let subsequence = filter_options(&options, "kbsys");
        assert_eq!(subsequence.len(), 1);
        assert_eq!(subsequence[0].value, "kube-system");
    }

    #[test]
    fn filtering_matches_status_subtitles() {
        let mut option = option("context-a", "context-a");
        option.status_label = Some("Not ready".into());
        let filtered = filter_options(&[option], "ready");
        assert_eq!(filtered.len(), 1);
    }

    /// A word that every row repeats is decoration, not a way to find a row.
    ///
    /// With `Open Resource View: ` in front of every label, the search matched
    /// `open`, `resource`, and `view` against all 71 kinds, so a query narrowed
    /// nothing.
    #[test]
    fn a_shared_verb_in_every_label_does_not_match_every_option() {
        let options = vec![
            option("Pod", "Open Resource View: Pods"),
            option("Job", "Open Resource View: Jobs"),
            option("Node", "Open Resource View: Nodes"),
        ];
        for decoration in ["open", "resource", "view", "open resource view"] {
            assert!(
                filter_options(&options, decoration).is_empty(),
                "{decoration:?} is shared by every row and must not match any of them"
            );
        }
        let pods = filter_options(&options, "pods");
        assert_eq!(pods.len(), 1);
        assert_eq!(pods[0].value, "Pod");
    }

    /// The prefix rule only removes whole words, and only when a word is left.
    #[test]
    fn the_decoration_cut_keeps_every_row_searchable() {
        assert_eq!(shared_label_prefix(&[]), "");
        assert!(
            filter_options(&[], "pods").is_empty(),
            "a picker with no options is an empty state, not a crash"
        );
        assert_eq!(
            shared_label_prefix(&[option("alpha", "Alpha"), option("beta", "Beta")]),
            "",
            "labels that share no whole word are not decoration"
        );
        assert_eq!(
            shared_label_prefix(&[option("shared", "Shared"), option("other", "Shared")]),
            "",
            "a label that is nothing but the shared word must stay searchable"
        );
        assert_eq!(
            shared_label_prefix(&[
                option("kube-system", "kube-system"),
                option("kube-public", "kube-public"),
            ]),
            "",
            "a partial word is never cut"
        );
        assert_eq!(
            shared_label_prefix(&[
                option("All namespaces", "All Namespaces"),
                option("perf", "Switch to Namespace: perf"),
            ]),
            "",
            "rows that disagree on the first word share no decoration"
        );
        // The value is always in the target, so a row is findable by its own
        // name even when its label was trimmed.
        let options = vec![
            option("kube-system", "Namespace: kube-system"),
            option("perf", "Namespace: perf"),
        ];
        let filtered = filter_options(&options, "kube-system");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].value, "kube-system");
    }

    #[test]
    fn empty_query_keeps_picker_order() {
        let options = vec![
            option("gamma", "Gamma"),
            option("alpha", "Alpha"),
            option("beta", "Beta"),
        ];
        let filtered = filter_options(&options, "");
        let values: Vec<_> = filtered
            .iter()
            .map(|option| option.value.to_string())
            .collect();
        assert_eq!(values, ["gamma", "alpha", "beta"]);
    }

    #[test]
    fn selection_navigation_stays_within_results() {
        let options = vec![
            option("alpha", "Alpha"),
            option("beta", "Beta"),
            option("gamma", "Gamma"),
        ];
        assert_eq!(move_selection(&options, None, 1).as_deref(), Some("beta"));
        assert_eq!(move_selection(&options, None, -1).as_deref(), Some("gamma"));
        assert_eq!(
            move_selection(&options, Some("alpha"), -1).as_deref(),
            Some("alpha")
        );
        assert_eq!(
            move_selection(&options, Some("gamma"), 1).as_deref(),
            Some("gamma")
        );
        assert_eq!(
            move_selection(&options, Some("alpha"), 99).as_deref(),
            Some("gamma")
        );
    }

    #[test]
    fn duplicate_labels_use_deterministic_disambiguators() {
        let mut options = vec![option("ctx-a", "shared"), option("ctx-b", "shared")];
        options[0].source = Some("https://a.example".into());
        disambiguate_options(&mut options);
        assert_eq!(options[0].label, "shared · https://a.example");
        assert_eq!(options[1].label, "shared · Context ctx-b");
    }

    #[test]
    fn selected_identity_survives_filtering() {
        let options = vec![option("alpha", "Alpha"), option("beta", "Beta")];
        let selected = selected_index(&filter_options(&options, "bet"), Some("beta"));
        assert_eq!(selected, Some(0));
        assert_eq!(
            move_selection(&filter_options(&options, "missing"), Some("alpha"), 1),
            None
        );
    }

    #[gpui::test]
    fn enter_activates_filtered_selection(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let selected = Rc::new(Cell::new(None));
        let dismissed = Rc::new(Cell::new(false));
        let selected_for_callback = selected.clone();
        let dismissed_for_callback = dismissed.clone();
        let (picker, cx) = cx.add_window_view(|_, cx| {
            SearchablePicker::new(
                PickerConfig {
                    kind: PickerKind::Namespace,
                    options: vec![
                        option("alpha", "Alpha"),
                        option("beta", "Beta"),
                        option("gamma", "Gamma"),
                    ],
                    query: None,
                    input: None,
                    on_select: Rc::new(move |value, _, _| {
                        selected_for_callback.set(Some(value));
                    }),
                    on_dismiss: Rc::new(move |_, _| {
                        dismissed_for_callback.set(true);
                    }),
                },
                cx,
            )
        });
        cx.run_until_parked();
        let focus = picker.read_with(cx, |picker, cx| picker.input_focus_handle(cx));
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_input("bet");
        cx.run_until_parked();
        assert_eq!(
            picker.read_with(cx, |picker, _| {
                picker.selected.as_ref().map(|value| value.to_string())
            }),
            Some("beta".to_owned())
        );
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            selected.take().map(|value| value.to_string()),
            Some("beta".to_owned())
        );
        assert!(dismissed.get());
    }

    #[gpui::test]
    fn picker_scroll_reveals_end_and_home_preserves_input_caret(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (picker, cx) = cx.add_window_view(|_, cx| {
            SearchablePicker::new(
                PickerConfig {
                    kind: PickerKind::Namespace,
                    options: (0..20)
                        .map(|index| {
                            let value = format!("item-{index}");
                            let label = format!("Item {index}");
                            option(&value, &label)
                        })
                        .collect(),
                    query: None,
                    input: None,
                    on_select: Rc::new(|_, _, _| {}),
                    on_dismiss: Rc::new(|_, _| {}),
                },
                cx,
            )
        });
        cx.simulate_resize(gpui::size(px(280.0), px(480.0)));
        cx.run_until_parked();
        let picker_bounds = cx
            .debug_bounds("searchable-picker")
            .expect("picker is laid out");
        let footer = cx
            .debug_bounds("searchable-picker-footer")
            .expect("picker footer is laid out");
        assert!(footer.left() >= picker_bounds.left());
        assert!(footer.right() <= picker_bounds.right());
        let input_focus = picker.read_with(cx, |picker, cx| picker.input_focus_handle(cx));
        let picker_focus = picker.read_with(cx, |picker, _| picker.focus_handle.clone());
        cx.update(|window, cx| window.focus(&input_focus, cx));
        cx.simulate_input("abc");
        cx.simulate_keystrokes("home");
        cx.simulate_input("x");
        assert_eq!(
            picker.read_with(cx, |picker, cx| picker.input.read(cx).text().to_owned()),
            "xabc"
        );
        cx.simulate_keystrokes("end");
        cx.simulate_input("y");
        assert_eq!(
            picker.read_with(cx, |picker, cx| picker.input.read(cx).text().to_owned()),
            "xabcy"
        );
        picker.update(cx, |picker, cx| {
            picker.input.update(cx, |input, cx| input.clear(cx));
        });
        cx.run_until_parked();

        cx.update(|window, cx| window.focus(&picker_focus, cx));
        cx.simulate_keystrokes("end");
        cx.run_until_parked();
        let scroll = picker.read_with(cx, |picker, _| picker.scroll.clone());
        assert_eq!(
            picker.read_with(cx, |picker, _| {
                picker.selected.as_ref().map(|value| value.to_string())
            }),
            Some("item-19".to_owned())
        );
        assert!(scroll.max_offset().y > px(0.0));
        assert!(scroll.offset().y < px(0.0));

        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        assert_eq!(
            picker.read_with(cx, |picker, _| {
                picker.selected.as_ref().map(|value| value.to_string())
            }),
            Some("item-0".to_owned())
        );
        assert_eq!(scroll.offset().y, px(0.0));
    }

    #[gpui::test]
    fn picker_home_and_end_leave_the_input_caret_alone(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (picker, cx) = cx.add_window_view(|_, cx| {
            SearchablePicker::new(
                PickerConfig {
                    kind: PickerKind::Namespace,
                    options: vec![
                        option("alpha", "Alpha"),
                        option("beta", "Beta"),
                        option("gamma", "Gamma"),
                    ],
                    query: None,
                    input: None,
                    on_select: Rc::new(|_, _, _| {}),
                    on_dismiss: Rc::new(|_, _| {}),
                },
                cx,
            )
        });
        cx.run_until_parked();
        let input_focus = picker.read_with(cx, |picker, cx| picker.input_focus_handle(cx));
        cx.update(|window, cx| window.focus(&input_focus, cx));
        cx.simulate_keystrokes("down");
        assert_eq!(
            picker.read_with(cx, |picker, _| {
                picker.selected.as_ref().map(|value| value.to_string())
            }),
            Some("beta".to_owned())
        );
        cx.simulate_keystrokes("home end");
        assert_eq!(
            picker.read_with(cx, |picker, _| {
                picker.selected.as_ref().map(|value| value.to_string())
            }),
            Some("beta".to_owned())
        );
    }

    #[gpui::test]
    fn command_home_and_end_navigate_the_list_from_the_field(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (picker, cx) = cx.add_window_view(|_, cx| {
            SearchablePicker::new(
                PickerConfig {
                    kind: PickerKind::Namespace,
                    options: (0..6)
                        .map(|index| option(&format!("item-{index}"), &format!("Item {index}")))
                        .collect(),
                    query: None,
                    input: None,
                    on_select: Rc::new(|_, _, _| {}),
                    on_dismiss: Rc::new(|_, _| {}),
                },
                cx,
            )
        });
        cx.run_until_parked();
        let input_focus = picker.read_with(cx, |picker, cx| picker.input_focus_handle(cx));
        cx.update(|window, cx| window.focus(&input_focus, cx));
        let selected = |picker: &Entity<SearchablePicker>, cx: &VisualTestContext| {
            picker.read_with(cx, |picker, _| {
                picker.selected.as_ref().map(|value| value.to_string())
            })
        };
        cx.simulate_keystrokes("ctrl-end");
        assert_eq!(selected(&picker, cx), Some("item-5".to_owned()));
        cx.simulate_keystrokes("ctrl-home");
        assert_eq!(selected(&picker, cx), Some("item-0".to_owned()));
    }

    /// The card has one boundary, and it is the popover's.
    ///
    /// `elevation_2` already gives the hosting popover the raised surface, a
    /// radius, a 1px `border_variant` and a shadow. The card drew a second line
    /// in the same token one step inside it, and under Taffy's `BorderBox` that
    /// border also came out of the content box, so the field and the title sat a
    /// pixel right of the padding the card declares. The command palette's card
    /// is bounded the same way this one now is.
    #[gpui::test]
    fn the_card_inside_padding_is_the_padding_and_nothing_else(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (_picker, cx) = cx.add_window_view(|_, cx| {
            SearchablePicker::new(
                PickerConfig {
                    kind: PickerKind::Namespace,
                    options: vec![option("alpha", "Alpha"), option("beta", "Beta")],
                    query: None,
                    input: None,
                    on_select: Rc::new(|_, _, _| {}),
                    on_dismiss: Rc::new(|_, _| {}),
                },
                cx,
            )
        });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        cx.run_until_parked();
        let card = cx.debug_bounds("searchable-picker").expect("card");
        let title = cx
            .debug_bounds("searchable-picker-title")
            .expect("title row");
        let footer = cx.debug_bounds("searchable-picker-footer").expect("footer");
        let field = cx.debug_bounds("shared-text-input").expect("field");
        let padding = f32::from(design::space::MD);
        // The rows carry the padding, so their own boxes are the card's box. The field is the
        // one child without padding of its own, which is why it is the one that measures the
        // inset. `debug_bounds` reports border boxes, so a row that has been pushed in would
        // report the push and nothing else.
        for (name, row) in [("title", title), ("footer", footer)] {
            assert!(
                f32::from(row.left() - card.left()).abs() <= 0.5
                    && f32::from(row.right() - card.right()).abs() <= 0.5,
                "the {name} row is the card's own width and holds its padding inside itself, so \
                 the card draws no second border inside the popover's: {}px in, {}px out",
                f32::from(row.left() - card.left()),
                f32::from(row.right() - card.right()),
            );
        }
        assert!(
            (f32::from(field.left() - card.left()) - padding).abs() <= 0.5,
            "the field starts at the card's own padding ({padding}px), so the card draws no \
             second border inside the popover's: {}px in",
            f32::from(field.left() - card.left())
        );
        assert!((f32::from(card.right() - field.right()) - padding).abs() <= 0.5);
    }

    #[gpui::test]
    fn the_clear_button_answers_enter_without_selecting_a_namespace(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let selected = Rc::new(Cell::new(None));
        let selected_for_callback = selected.clone();
        let (picker, cx) = cx.add_window_view(|_, cx| {
            SearchablePicker::new(
                PickerConfig {
                    kind: PickerKind::Namespace,
                    options: vec![option("alpha", "Alpha"), option("beta", "Beta")],
                    query: None,
                    input: None,
                    on_select: Rc::new(move |value, _, _| {
                        selected_for_callback.set(Some(value));
                    }),
                    on_dismiss: Rc::new(|_, _| {}),
                },
                cx,
            )
        });
        cx.run_until_parked();
        let input_focus = picker.read_with(cx, |picker, cx| picker.input_focus_handle(cx));
        let clear_focus =
            picker.read_with(cx, |picker, cx| picker.input.read(cx).clear_focus_handle());
        cx.update(|window, cx| window.focus(&input_focus, cx));
        cx.simulate_input("alp");
        cx.run_until_parked();
        cx.update(|window, cx| window.focus(&clear_focus, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            selected.take().map(|value| value.to_string()),
            None,
            "Enter on the clear button must not activate a namespace"
        );
        assert_eq!(
            picker.read_with(cx, |picker, cx| picker.input.read(cx).text().to_owned()),
            ""
        );
    }

    /// The count line and the card's own list label name the same thing the same
    /// way, and the command palette's count reads the same as both.
    #[test]
    fn result_label_reports_visible_and_total_counts() {
        assert_eq!(result_label(0, 1), "0 of 1 results");
        assert_eq!(result_label(1, 1), "1 result");
        assert_eq!(result_label(2, 8), "2 of 8 results");
        assert_eq!(result_label(0, 0), "0 results");
        assert_eq!(
            result_label(12, 1_200),
            "12 of 1,200 results",
            "counts use the shared thousands separator"
        );
        assert_eq!(
            result_label(1, 21),
            "1 of 21 results",
            "the head noun does not follow the visible count"
        );
        for kind in [PickerKind::Cluster, PickerKind::Namespace] {
            assert!(
                kind.results_label().ends_with(" results"),
                "the group label and the count line must agree: {}",
                kind.results_label()
            );
        }
    }

    #[test]
    fn footer_hints_are_short_items_and_name_both_escape_steps() {
        let idle = footer_hints("");
        assert_eq!(idle, ["↑↓ Navigate", "Enter Select", "Esc Close"]);
        let searching = footer_hints("perf");
        assert_eq!(
            searching,
            [
                "↑↓ Navigate",
                "Enter Select",
                "Esc Clear search",
                "Esc Close"
            ],
            "the first Escape clears the field and the next one closes the picker"
        );
        for hint in idle.iter().chain(searching.iter()) {
            assert!(!hint.contains('·'), "separator: {hint}");
            assert!(hint.chars().count() <= 24, "too long: {hint}");
        }
    }

    /// An empty state has to offer the next step, and a repair the picker cannot
    /// run itself needs a control to run.
    #[gpui::test]
    fn the_title_above_the_field_stays_a_title_and_the_empty_state_offers_clear_search(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (picker, cx) = cx.add_window_view(|_, cx| {
            SearchablePicker::new(
                PickerConfig {
                    kind: PickerKind::Namespace,
                    options: vec![option("perf", "perf")],
                    query: None,
                    input: None,
                    on_select: Rc::new(|_, _, _| {}),
                    on_dismiss: Rc::new(|_, _| {}),
                },
                cx,
            )
        });
        cx.run_until_parked();
        let title = cx
            .debug_bounds("searchable-picker-title")
            .expect("the picker names its task");
        let input = cx
            .debug_bounds("shared-text-input")
            .expect("the picker has a search field");
        assert!(
            title.bottom() <= input.top(),
            "the title sits above the field, so it cannot read as the field's value"
        );
        assert!(
            cx.debug_bounds("searchable-picker-clear-search").is_none(),
            "nothing to clear while the field is empty"
        );

        let input_focus = picker.read_with(cx, |picker, cx| picker.input_focus_handle(cx));
        cx.update(|window, cx| window.focus(&input_focus, cx));
        cx.simulate_input("zzz");
        cx.run_until_parked();
        let clear = cx
            .debug_bounds("searchable-picker-clear-search")
            .expect("the empty state offers a way out");
        assert!(clear.size.height > px(0.0));
        cx.simulate_click(clear.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, cx| {
            let picker = picker.read(cx);
            assert_eq!(
                picker.input.read(cx).text(),
                "",
                "the empty state's control clears the search"
            );
            let _ = window;
        });
    }

    /// The current selection is stated on its own line, and the line is not
    /// truncated by the picker's width.
    #[gpui::test]
    fn the_current_scope_gets_its_own_line_under_the_field(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (_picker, cx) = cx.add_window_view(|_, cx| {
            let mut current = PickerOption::new("perf", "perf");
            current.current = true;
            SearchablePicker::new(
                PickerConfig {
                    kind: PickerKind::Namespace,
                    options: vec![current],
                    query: None,
                    input: None,
                    on_select: Rc::new(|_, _, _| {}),
                    on_dismiss: Rc::new(|_, _| {}),
                },
                cx,
            )
        });
        cx.run_until_parked();
        let input = cx
            .debug_bounds("shared-text-input")
            .expect("the picker has a search field");
        let scope = cx
            .debug_bounds("searchable-picker-scope")
            .expect("the current selection is stated");
        assert!(
            scope.top() >= input.bottom(),
            "the scope line is under the field"
        );
        let results = cx
            .debug_bounds("searchable-picker-results")
            .expect("the results are laid out");
        assert!(
            scope.bottom() <= results.top(),
            "the scope line does not push into the list"
        );
    }

    #[gpui::test]
    fn narrow_widths_wrap_the_footer_instead_of_squeezing_the_hints(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let mut heights = Vec::new();
        // The picker is capped at INSPECTOR_MIN, so a wide window does not make
        // the footer roomier. Only a width at or below that cap can change how
        // the hints wrap.
        for width in [240.0, 150.0] {
            let (_picker, cx) = cx.add_window_view(|_, cx| {
                SearchablePicker::new(
                    PickerConfig {
                        kind: PickerKind::Namespace,
                        options: vec![option("alpha", "Alpha"), option("beta", "Beta")],
                        query: None,
                        input: None,
                        on_select: Rc::new(|_, _, _| {}),
                        on_dismiss: Rc::new(|_, _| {}),
                    },
                    cx,
                )
            });
            cx.simulate_resize(gpui::size(px(width), px(480.0)));
            cx.run_until_parked();
            let picker = cx
                .debug_bounds("searchable-picker")
                .expect("picker is laid out");
            let footer = cx
                .debug_bounds("searchable-picker-footer")
                .expect("picker footer is laid out");
            assert!(footer.left() >= picker.left(), "width={width}");
            assert!(footer.right() <= picker.right(), "width={width}");
            heights.push(footer.size.height);
        }
        assert!(
            heights[1] > heights[0],
            "the hints must wrap at a narrow width instead of being truncated: {heights:?}"
        );
    }
}
