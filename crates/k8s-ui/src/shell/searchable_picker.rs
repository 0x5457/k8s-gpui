//! The context and namespace switcher card.
//!
//! The card is a searchable list in a popover. gpui-kit's `List` owns the rows,
//! the scroll and the cursor; `SearchablePicker` owns the app's part of that:
//! which options exist, what the shared label decoration is, which value is
//! current, and the card's one geometry.
//!
//! The geometry is the point. One content inset, one row height, one icon slot
//! and one check lane, drawn by one helper that the title band, the scope line
//! and every result row all call, so a lane cannot be one width in the header and
//! another in the rows. Three heights carry the whole card: [`ROW_HEIGHT`] for a
//! row and for the band that holds the title and the count, `text::LABEL`'s line
//! for the scope line, and `size::CONTROL` for the field.

use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::IndexPath;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::empty::{
    Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle,
};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::label::Label;
use gpui_kit::component::list::{List, ListDelegate, ListState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{FocusableExt as _, Icon, RoleOverride, Selectable, Sizable, Size};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, Context, Div, Entity, FocusHandle, Focusable, Global, Hsla, IntoElement,
    MouseButton, ParentElement, Pixels, Render, RenderOnce, Role, SharedString, Styled,
    Subscription, Task, Window, div, px,
};
use k8s_core::fuzzy;

use crate::design;
use crate::panels::common::label_panel_title;

/// The inset every band of the card shares.
///
/// The title band, the scope line, the search field and the rows all start and
/// end here, so the selected row's fill — which is bounded by this inset — lands
/// on the same leading and trailing edge as the field above it. It is
/// `space::MD` because that is the one inset the other two lists in this product
/// use: `panels::search` gives its rows and its column header `space::MD`, and the
/// command palette's `PALETTE_LANE_PAD` is the same number. A second inset here
/// would be a column that lines up with nothing.
const CONTENT_INSET: Pixels = design::space::MD;

/// The height of every row, and of the band that holds the title and the count.
///
/// `size::PALETTE_ROW`: these are menu rows, and it is the number the resource
/// search's rows and the command palette's rows are drawn at, so the lists a
/// reader moves between with the same gesture are the same height.
const ROW_HEIGHT: Pixels = design::size::PALETTE_ROW;

/// Width of the lane every row's glyph sits in, and the size of the glyph in it.
///
/// One lane, reserved whether or not the row has anything to put in it, because
/// `Design guides > Alignment details` asks for exactly this: "Center icons in a
/// fixed slot so labels do not move when icons differ in intrinsic width". An
/// option without an icon used to pull its label left of every option that had
/// one, which is a list that cannot be scanned down a column.
///
/// `size::KIND_ICON` rather than `size::ICON`: this is the chrome glyph set — a
/// cluster is a server, a namespace is a folder — drawn and judged at fourteen
/// pixels, and the same glyphs are fourteen pixels in the toolbar that opened this
/// card.
const ICON_SLOT: Pixels = design::size::KIND_ICON;

/// Width of the lane the current-value check sits in, reserved on every row.
///
/// "Keep trailing row actions and disclosure indicators in fixed-width lanes."
/// Drawn on the current row only, which is why the lane has to be there on the
/// others: a check that appears by taking space from the label moves the label.
const CHECK_LANE: Pixels = design::size::KIND_ICON;

/// How many rows the list shows before it scrolls, and therefore how tall this
/// card can ever be.
///
/// The list owns the cap rather than the card owning a height, because the list
/// is the only elastic part of the card: the title, the scope line and the field
/// are fixed, so a cap on the rows is a cap on the card, and it is one number
/// rather than a height that has to be re-derived whenever a band moves.
const LIST_ROWS: f32 = 8.0;

/// The share of the card's width the trailing lane may take before the name has
/// to truncate.
///
/// The trailing lane is a status word, a reason and the check lane, and on a
/// 260px card those are the parts that can grow without end — a kubeconfig path
/// as a detail is as long as the reader's home directory. Capping the lane as a
/// fraction rather than as a pixel count keeps the name's measure the same at
/// every card width, including the narrow ones a small window forces.
const TRAILING_MAX: f32 = 0.4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerKind {
    Cluster,
    Namespace,
}

impl PickerKind {
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
    pub icon: Option<IconName>,
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

/// The title of the empty state: what is missing, and — when the reader typed
/// something — which query asked for it.
///
/// A title that only says "No matching namespaces" leaves the reader looking at
/// the field above to work out why. Naming the query is the one fact this state
/// can add, and it is the one the repair below it acts on. No terminal period: it
/// is a label, not a sentence.
fn empty_title(kind: PickerKind, query: &str) -> String {
    let query = query.trim();
    if query.is_empty() {
        return match kind {
            PickerKind::Cluster => "No contexts available".to_owned(),
            PickerKind::Namespace => "No namespaces available".to_owned(),
        };
    }
    match kind {
        PickerKind::Cluster => format!("No context matches \"{query}\""),
        PickerKind::Namespace => format!("No namespace matches \"{query}\""),
    }
}

/// The sentence a row is read as: what it is, how it is doing, and which scope it
/// is already the current value of.
///
/// One sentence with two readers — the row's accessible name, and the tooltip
/// that carries the value a narrow card truncated out of the name lane. A name
/// that is only ever read in full on screen has no second reader, and the
/// namespaces this card is for are longer than a 260px popover.
fn row_description(option: &PickerOption, kind: &PickerKind) -> String {
    let mut sentence = option.label.to_string();
    if let Some(status) = &option.status_label {
        sentence.push_str(&format!(", {status}"));
    }
    if let Some(detail) = &option.detail {
        sentence.push_str(&format!(". {detail}"));
    }
    if option.current {
        format!("Current {}: {sentence}", kind.singular())
    } else {
        sentence
    }
}

fn label(value: impl Into<SharedString>, size: gpui_kit::Pixels) -> Label {
    Label::new(value).text_size(size)
}

// ---------------------------------------------------------------------------
// The card's one geometry
// ---------------------------------------------------------------------------

/// The leading lane: a fixed slot, reserved whether or not there is a glyph in it.
///
/// The slot is the icon's width and the glyph is centred in it, so a row with a
/// glyph, a row without one, and a row whose glyph is a wider one all put the
/// name in the same place — and all three are `ROW_HEIGHT` tall, because the
/// glyph cannot change a row's height.
fn icon_lane(icon: Option<IconName>, ink: Hsla) -> Div {
    div()
        .w(ICON_SLOT)
        .h(ICON_SLOT)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .when_some(icon, |this, icon| {
            this.child(
                Icon::new(icon)
                    .with_size(Size::Size(ICON_SLOT))
                    .text_color(ink),
            )
        })
}

/// The trailing lane: what the row has to say after its name, and the check lane
/// that says which value is current.
///
/// `meta` is dropped rather than reserved: a namespace row has no status and no
/// detail, and a lane that held space for words it does not have would push the
/// check lane right by the gap in front of it. The check lane itself is always
/// drawn, so a name never moves when the cursor crosses the current row.
///
/// The check is `fg_primary` and not the accent on purpose. The accent on this
/// card means one thing — the keyboard is here — and it is spent on the selected
/// row's fill. The tick says a different fact: which value the card would answer
/// with if the reader dismissed it now, which is true whether or not the cursor
/// is anywhere near it.
fn trailing_lane(meta: Option<AnyElement>, current: bool, ink: Hsla) -> Div {
    h_flex()
        .flex_none()
        .items_center()
        .gap(design::space::SM)
        .max_w(gpui_kit::relative(TRAILING_MAX))
        .when_some(meta, |this, meta| {
            this.child(h_flex().min_w(px(0.0)).flex_shrink_1().child(meta))
        })
        .child(
            div()
                .w(CHECK_LANE)
                .h(ICON_SLOT)
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .when(current, |this| {
                    this.child(
                        Icon::new(IconName::Check)
                            .with_size(Size::Size(ICON_SLOT))
                            .text_color(ink),
                    )
                }),
        )
}

/// The three lanes every line in this card is built from: a fixed icon slot, the
/// name, and the trailing lane.
///
/// One builder, called by the title band, the scope line and every result row, so
/// the card has a single horizontal answer. The command palette's `palette_lanes`
/// exists for the same reason and says the same thing in its own file: two rows
/// that each describe their own columns is how a check column and a chip column
/// came to sit at two different distances from the card's edge, and how a group
/// heading came to read as an ordinary row with nothing in it.
fn lanes(icon: Option<IconName>, icon_ink: Hsla, name: impl IntoElement, trailing: Div) -> Div {
    h_flex()
        .w_full()
        .min_w(px(0.0))
        .items_center()
        .gap(design::space::SM)
        .child(icon_lane(icon, icon_ink))
        // The name lane takes every pixel the trailing lane does not want, and
        // truncates rather than pushing the trailing lane out of the card.
        .child(h_flex().min_w(px(0.0)).flex_1().child(name))
        .child(trailing)
}

pub(super) type PickerSelectHandler = Rc<dyn Fn(SharedString, &mut Window, &mut App)>;
pub(super) type PickerDismissHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// A picker row: the list's own item, in a box that carries the product's row
/// states and the option's test selector.
///
/// The list marks the row selected and hands the item `Selectable` after the item
/// has been built, which is why the row is built here rather than borrowed from
/// `ListItem`: a `ListItem` brings gpui-kit's own padding, radius, hover wash and
/// selected fill, and none of the four can be reconciled with this card's
/// geometry from the outside. It is drawn as a band that carries
/// [`CONTENT_INSET`] with the row inside it, so the fill is bounded by the card's
/// inset on both sides instead of bleeding to one edge and stopping short of the
/// other.
#[derive(IntoElement)]
struct PickerRow {
    index: usize,
    selector: Option<SharedString>,
    icon: Option<IconName>,
    icon_ink: Hsla,
    check_ink: Hsla,
    name: AnyElement,
    meta: Option<AnyElement>,
    /// Which value is the card's current one — the row's own persistent state,
    /// and the fact its check lane and `aria_selected` both answer to.
    current: bool,
    description: SharedString,
    pick: PickerSelectHandler,
    value: SharedString,
    fill: Hsla,
    hover: Hsla,
    /// Whether the keyboard cursor is on this row.
    selected: bool,
}

impl Selectable for PickerRow {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }

    fn secondary_selected(self, _: bool) -> Self {
        self
    }
}

impl RenderOnce for PickerRow {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            index,
            selector,
            icon,
            icon_ink,
            check_ink,
            name,
            meta,
            current,
            description,
            pick,
            value,
            fill,
            hover,
            selected,
        } = self;
        let press = {
            let pick = pick.clone();
            let value = value.clone();
            move |_: &gpui_kit::MouseDownEvent, window: &mut Window, cx: &mut App| {
                pick(value.clone(), window, cx)
            }
        };
        div()
            .w_full()
            .h(ROW_HEIGHT)
            .flex_none()
            .px(CONTENT_INSET)
            .child(
                h_flex()
                    .id(("searchable-picker-option", index))
                    .w_full()
                    .h_full()
                    .items_center()
                    // The row's own silhouette, so the selection is a rounded row
                    // rather than a rectangle cut out of a rounded card.
                    .rounded(design::radius::SM)
                    .when_some(selector, |this, selector| {
                        this.debug_selector(move || selector.to_string())
                    })
                    .role(Role::ListBoxOption)
                    .aria_label(description.clone())
                    // The row's own selected state is *which value is current*;
                    // the list's wrapper div answers for the keyboard cursor, and
                    // those are two different facts about a row that can be both at
                    // once.
                    .aria_selected(current)
                    .when(selected, |this| this.aria_active_descendant())
                    // Rest, selected and hover are three states of one plane. The
                    // selected fill is the accent wash solved against the plane the
                    // row is painted on, and the hover is the product's own
                    // `state::hover_on` over the same plane — a wash of the local
                    // ink, which is invisible-in-the-right-way in both appearances.
                    // There is no leading-edge rail: the guide is explicit that a
                    // one-sided border as the selection marker breaks the item's
                    // rounded silhouette, and nothing else in the product draws one.
                    .when(selected, |this| this.bg(fill))
                    .hover(|this| {
                        this.bg(if selected {
                            design::state::hover_on(fill, design::role::accent(cx))
                        } else {
                            hover
                        })
                    })
                    .active(|this| {
                        this.bg(if selected {
                            design::state::press_on(fill, design::role::accent(cx))
                        } else {
                            design::state::press_on(
                                design::role::surface_raised(cx),
                                design::role::fg_primary(cx),
                            )
                        })
                    })
                    .tooltip(move |window, cx| Tooltip::new(description.clone()).build(window, cx))
                    // The press, not the click. A list row answers to a press on
                    // the desktop — every native menu, table and list row does —
                    // and a press does not depend on the pointer having settled
                    // into a hover first, which is the only thing a click waits for.
                    .on_mouse_down(MouseButton::Left, press)
                    .child(lanes(
                        icon,
                        icon_ink,
                        name,
                        trailing_lane(meta, current, check_ink),
                    )),
            )
    }
}

/// The picker's list: the options, the query, and the two answers the card gives
/// back to the Shell.
struct PickerDelegate {
    kind: PickerKind,
    options: Vec<PickerOption>,
    visible: Vec<PickerOption>,
    query: String,
    selected: Option<usize>,
    on_select: PickerSelectHandler,
    on_dismiss: PickerDismissHandler,
    /// How a row reaches this delegate: a row is built by value, so the press it
    /// carries can only be a handle back to the card that owns the row.
    pick: PickerSelectHandler,
    clear: PickerDismissHandler,
}

impl PickerDelegate {
    fn new(options: Vec<PickerOption>, kind: PickerKind) -> Self {
        let visible = filter_options(&options, "");
        let selected = if visible.is_empty() { None } else { Some(0) };
        Self {
            kind,
            options,
            visible,
            query: String::new(),
            selected,
            on_select: Rc::new(|_, _, _| {}),
            on_dismiss: Rc::new(|_, _| {}),
            pick: Rc::new(|_, _, _| {}),
            clear: Rc::new(|_, _| {}),
        }
    }

    fn take_options(&mut self, options: Vec<PickerOption>) -> bool {
        if options == self.options {
            return false;
        }
        self.options = options;
        self.refilter();
        true
    }

    /// A new query, and the cursor that goes with it.
    ///
    /// Typing replaces the result set, so the row the cursor was on belongs to
    /// the previous one and the cursor goes to the top of what is left. That is
    /// what this card has always done, and it is the only answer that makes Enter
    /// mean "the first match" rather than "whatever row happened to be under the
    /// cursor when the letters started arriving".
    fn take_query(&mut self, query: String) {
        self.query = query.trim().to_owned();
        self.refilter();
        self.selected = (!self.visible.is_empty()).then_some(0);
    }

    fn refilter(&mut self) {
        self.visible = filter_options(&self.options, &self.query);
        if !self
            .selected
            .is_some_and(|index| index < self.visible.len())
        {
            self.selected = self.position_of_current().or(if self.visible.is_empty() {
                None
            } else {
                Some(0)
            });
        }
    }

    /// The row the current scope sits on, when the query still lets it through.
    fn position_of_current(&self) -> Option<usize> {
        let current = self.options.iter().find(|option| option.current)?;
        self.visible
            .iter()
            .position(|option| option.value == current.value)
    }

    fn current_label(&self) -> Option<SharedString> {
        self.options
            .iter()
            .find(|option| option.current)
            .map(|option| option.label.clone())
    }

    fn select(&mut self, window: &mut Window, cx: &mut App) {
        let Some(value) = self.selected.and_then(|ix| self.visible.get(ix)) else {
            return;
        };
        let value = value.value.clone();
        (self.on_select)(value, window, cx);
        (self.on_dismiss)(window, cx);
    }

    /// A press on a row: the value moves the cursor, then answers the Shell.
    ///
    /// The answer is the delegate's own, not a trip back through the card: a
    /// delegate lives inside the list the card owns, so re-entering the card from
    /// here would ask for an entity that is already being updated.
    fn press(&mut self, value: SharedString, window: &mut Window, cx: &mut App) {
        let Some(index) = self.visible.iter().position(|option| option.value == value) else {
            return;
        };
        self.selected = Some(index);
        self.select(window, cx);
    }

    fn fill(&self, cx: &App) -> Hsla {
        design::row_selected_bg_on(cx, design::role::surface_raised(cx))
    }

    /// The status cell: the mark, and the word that says what the mark means.
    ///
    /// §4.4's status cell, in the order it reads: a 6px mark, `space::ICON` of air,
    /// and the word at `text::LABEL`. The mark is the channel's mark ink solved
    /// against the plane the row is painted on — the selected wash when the row is
    /// selected — and the word is the channel's *word* ink. Two roles at two floors,
    /// which is why they are not one colour, and why a context that failed to load
    /// is a red dot beside a red word rather than a red dot beside a grey one that
    /// says nothing.
    ///
    /// The cell shrinks and the word truncates rather than the lane overflowing:
    /// "No Namespaces Found" is a real state of this card and at a 260px popover it
    /// is longer than the trailing lane is allowed to be. The whole sentence, the
    /// dot included, is what the row's tooltip carries when it does not fit.
    fn status_cell(&self, option: &PickerOption, selected: bool, cx: &App) -> Option<AnyElement> {
        let status = option.status?;
        let plane = if selected {
            self.fill(cx)
        } else {
            design::role::surface_raised(cx)
        };
        Some(
            h_flex()
                .flex_shrink_1()
                .min_w(px(0.0))
                .items_center()
                .gap(design::space::ICON)
                .child(
                    div()
                        .size(design::size::STATUS_DOT)
                        .flex_none()
                        .rounded_full()
                        .bg(status.marker_on(cx, plane)),
                )
                .when_some(option.status_label.clone(), |this, word| {
                    this.child(
                        label(word, design::text::LABEL)
                            .text_color(design::role::status_word_for(status, cx))
                            .truncate(),
                    )
                })
                .into_any_element(),
        )
    }

    /// The reason a row is not doing what the others are doing, if it has one.
    fn detail_cell(&self, option: &PickerOption, cx: &App) -> Option<AnyElement> {
        option.detail.clone().map(|detail| {
            label(detail, design::text::LABEL)
                .text_color(design::role::fg_tertiary(cx))
                .truncate()
                .into_any_element()
        })
    }

    /// One row: the option's identity, its status, and what it says.
    fn row(&self, index: usize, option: &PickerOption, selected: bool, cx: &App) -> PickerRow {
        // An option that cannot do what the others do is quiet rather than loud.
        // `fg_primary` is a name's ink on every other row, and the one step down
        // the ladder — the product's own `state::disabled` ink — is a state the
        // reader can see, rather than a second colour reserved for namespaces. The
        // row stays pressable: refusing the press would change what this card does,
        // and the status word beside the name is what says why it is in trouble.
        let name_ink = match option.status {
            Some(design::Severity::Error) => {
                design::state::disabled(cx, design::role::surface_raised(cx))
            }
            _ => design::role::fg_primary(cx),
        };
        let meta = self.status_cell(option, selected, cx).map(|status| {
            match self.detail_cell(option, cx) {
                Some(detail) => h_flex()
                    .flex_shrink_1()
                    .min_w(px(0.0))
                    .items_center()
                    .gap(design::space::SM)
                    .child(status)
                    .child(detail)
                    .into_any_element(),
                None => status,
            }
        });
        PickerRow {
            index,
            selector: option.debug_selector.clone(),
            icon: option.icon,
            // The mark is `fg_tertiary` on every row, selected or not: a folder is
            // what the row *is*, not a state it is in.
            icon_ink: design::role::fg_tertiary(cx),
            check_ink: design::role::fg_primary(cx),
            name: label(option.label.clone(), design::text::BODY)
                .text_color(name_ink)
                .truncate()
                .into_any_element(),
            meta,
            current: option.current,
            description: row_description(option, &self.kind).into(),
            pick: self.pick.clone(),
            value: option.value.clone(),
            fill: self.fill(cx),
            hover: design::state::hover_on(
                design::role::surface_raised(cx),
                design::role::fg_primary(cx),
            ),
            selected,
        }
    }

    /// What the picker shows when the list has nothing in it.
    ///
    /// `writing.md > Best practices` asks an empty screen to guide people on what
    /// they can do and to give them a control for it where one exists, so a search
    /// that matched nothing offers the clear that repairs it.
    ///
    /// The frame is undone rather than inherited, for the same reasons and with the
    /// same three choices `common::empty_state_with_action` and the resource
    /// search make: `Empty`'s own dashed 1px border and 24px padding are a
    /// page-level cue, and inside a popover whose surface *is* the frame a second
    /// frame reads as a hole cut in it. The glyph is unframed, because
    /// `EmptyMediaVariant::Icon` is a `size_8()` `bg(muted)` plate and §4.13 asks
    /// for a bare 24px `fg.tertiary` glyph.
    fn empty(&self, cx: &App) -> AnyElement {
        let searching = !self.query.trim().is_empty();
        let title = empty_title(self.kind, &self.query);
        let clear = self.clear.clone();
        let clear_button = searching.then(|| {
            div()
                .id("searchable-picker-clear-search")
                .debug_selector(|| "searchable-picker-clear-search".to_owned())
                .flex_none()
                .child(
                    Button::new("searchable-picker-clear-button")
                        .label("Clear search")
                        .ghost()
                        .with_size(Size::Size(design::size::CONTROL))
                        .accessibility_label("Clear Picker Search")
                        .on_click(move |_, window, cx| clear(window, cx)),
                )
        });
        div()
            .id("searchable-picker-empty")
            .w_full()
            .flex_none()
            // The same inset as every band above it, so an empty card's sentence
            // starts on the spine the rows would have started on.
            .px(CONTENT_INSET)
            .py(design::space::MD)
            .items_center()
            .justify_center()
            .gap(design::space::SM)
            .role(Role::Status)
            .aria_label(title.clone())
            .child(
                Empty::new()
                    .border_0()
                    .p_0()
                    .rounded(design::radius::LG)
                    .flex_none()
                    .gap(design::space::SM)
                    .text_color(design::role::fg_primary(cx))
                    .header(
                        EmptyHeader::new()
                            // A percentage of a 260px popover is not a measure.
                            // The same 40ch `UI-SPEC` §2.3 gives every other
                            // state's description.
                            .max_w(px(f32::from(design::text::BODY)
                                * 0.6
                                * crate::panels::common::EMPTY_MEASURE_CH))
                            .gap(design::space::SM)
                            .media(
                                EmptyMedia::new()
                                    .with_variant(EmptyMediaVariant::Default)
                                    .mb_0()
                                    .child(
                                        Icon::new(IconName::Search)
                                            .with_size(Size::Size(design::size::ICON_LARGE))
                                            .text_color(design::role::fg_tertiary(cx)),
                                    ),
                            )
                            .title(
                                EmptyTitle::new()
                                    .text_size(design::text::TITLE)
                                    .line_height(design::text::TITLE_LINE_HEIGHT)
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .text_color(design::role::fg_primary(cx))
                                    .child(title),
                            )
                            .description(
                                EmptyDescription::new()
                                    .line_height(design::text::BODY_LINE_HEIGHT)
                                    .child(
                                        label(self.empty_guidance(), design::text::BODY)
                                            .text_color(design::role::fg_secondary(cx)),
                                    ),
                            ),
                    )
                    .when_some(clear_button, Empty::child),
            )
            .into_any_element()
    }

    fn empty_guidance(&self) -> &'static str {
        if self.query.trim().is_empty() {
            match self.kind {
                PickerKind::Cluster => {
                    "Add a context to a kubeconfig file, then reload kubeconfigs."
                }
                PickerKind::Namespace => "Refresh the namespace list, then try again.",
            }
        } else {
            "Clear the search, or type another name."
        }
    }
}

impl ListDelegate for PickerDelegate {
    type Item = PickerRow;

    fn perform_search(
        &mut self,
        query: &str,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        self.take_query(query.to_owned());
        Task::ready(())
    }

    fn items_count(&self, _: usize, _: &App) -> usize {
        self.visible.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<PickerRow> {
        let option = self.visible.get(ix.row)?.clone();
        Some(self.row(ix.row, &option, self.selected == Some(ix.row), cx))
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        self.empty(cx)
    }

    fn cancel(&mut self, _: &mut Window, _: &mut Context<ListState<Self>>) {}

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) {
        self.selected = ix.map(|ix| ix.row);
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        if secondary {
            return;
        }
        self.select(window, cx);
    }
}

pub(super) struct PickerConfig {
    pub kind: PickerKind,
    pub options: Vec<PickerOption>,
    pub on_select: PickerSelectHandler,
    pub on_dismiss: PickerDismissHandler,
}

/// The card's search field.
///
/// gpui-kit's `List` draws its own search input with `.appearance(false)` and
/// `.p_0()` and a hairline *under* it, which is how the field in this card arrived
/// with no box, no border and no background: a search field with no frame is not a
/// field, it is a line of placeholder text. So the list keeps the rows, the scroll
/// and the cursor, and the field is the app's — the same `InputState` every other
/// field in the product is built from, with the product's own treatment:
///
/// - the input plane, so the field is a plane inside a plane rather than a hole
///   cut in one;
/// - a 1px `role::border_base` hairline, which is the only kind of stroke an input
///   gets in this design;
/// - `radius::SM`, the input's own tier;
/// - a leading search glyph in the same [`ICON_SLOT`] the rows reserve, so the two
///   leading glyphs in this card are one size in one column;
/// - the editor's own placeholder and caret, IME and selection, because
///   `InputState` owns them rather than a drawn-on stand-in;
/// - focus as a 1px accent border — `design::focus::border` — rather than
///   gpui-kit's 3px ring. The ring is a shadow on the element's own paint pass, so
///   it cannot be clipped, and on a text field it reads as a browser default;
/// - `design::size::CONTROL` tall, the height this product gives a search box, on
///   the same leading and trailing edges as the rows because the wrapper carries
///   [`CONTENT_INSET`].
fn search_field(
    query: &Entity<InputState>,
    placeholder: SharedString,
    clear: impl Fn(&mut Window, &mut App) + 'static,
    focused: bool,
    cx: &App,
) -> AnyElement {
    let plane = design::role::surface_raised(cx);
    let mut field = Input::new(query)
        .id(("searchable-picker-search", query.entity_id()))
        .role(RoleOverride::from(Role::SearchInput))
        .aria_label(placeholder)
        .h(design::size::CONTROL)
        .w_full()
        // The height is the control's. One horizontal inset inside it, and no
        // vertical padding at all, so the text is centred in the 28 rather than sat
        // in a twelve-pixel content box.
        .px(design::space::SM)
        .py_0()
        .text_size(design::text::BODY)
        .line_height(design::text::BODY_LINE_HEIGHT)
        .bg(plane)
        .rounded(design::radius::SM)
        .border_1()
        .border_color(if focused {
            design::focus::border(cx)
        } else {
            design::role::border_base(cx)
        })
        // The product's focus treatment is the field's own border ink, so the edge
        // the pointer aims at is the edge that lights up. gpui-kit's ring is the
        // third language of focus in this app and cannot be turned off from the
        // outside, so it is turned off here.
        .focus_ring(false)
        .prefix(
            div().w(ICON_SLOT).flex_none().child(
                Icon::new(IconName::Search)
                    .with_size(Size::Size(ICON_SLOT))
                    .text_color(design::graphic_on(plane, design::role::fg_tertiary(cx))),
            ),
        );
    // The clear control is the field's own suffix, as it was when the list drew the
    // field: a repair for this field, on this field, reachable without the keyboard.
    // It is not a tab stop, so Tab out of the field leaves the card rather than
    // stepping through a button inside it.
    if !query.read(cx).value().is_empty() {
        field = field.suffix(
            Button::new(("searchable-picker-clear-query", query.entity_id()))
                .icon(Icon::new(IconName::Close))
                .text()
                .ghost()
                .with_size(Size::Size(design::size::ICON_BUTTON))
                .tab_stop(false)
                .accessibility_label("Clear search")
                .on_click(move |_, window, cx| clear(window, cx)),
        );
    }
    div()
        .id("searchable-picker-search-field")
        .debug_selector(|| "searchable-picker-search-field".to_owned())
        .w_full()
        .flex_none()
        .px(CONTENT_INSET)
        .child(field)
        .into_any_element()
}

/// The four keys this card owns while its field holds the keyboard.
enum PickerKey {
    Up,
    Down,
    Confirm,
    Cancel,
}

pub(super) struct SearchablePicker {
    list: Entity<ListState<PickerDelegate>>,
    query: Entity<InputState>,
    kind: PickerKind,
    _query_changed: Subscription,
    _navigation: Subscription,
}

impl SearchablePicker {
    pub fn new(config: PickerConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let kind = config.kind;
        let mut delegate = PickerDelegate::new(config.options, kind);
        delegate.on_select = config.on_select;
        delegate.on_dismiss = config.on_dismiss.clone();
        let for_pick = cx.weak_entity();
        delegate.pick = Rc::new(move |value, window, cx| {
            let _ = for_pick.update(cx, |picker, cx| picker.pick(value, window, cx));
        });
        // The field is the card's own `InputState` and its value is the query. It
        // is created here rather than by the list because the list's field is drawn
        // by gpui-kit with no appearance at all — see [`search_field`].
        let query = cx.new(|cx| {
            InputState::new(window, cx).placeholder(SharedString::from(kind.search_label()))
        });
        let query_changed = cx.subscribe_in(
            &query,
            window,
            |picker: &mut Self, query, event: &InputEvent, window, cx| {
                if let InputEvent::Change = event {
                    picker.set_query(query.read(cx).value().to_string(), window, cx);
                }
            },
        );
        let for_clear = cx.weak_entity();
        delegate.clear = Rc::new(move |window, cx| {
            let _ = for_clear.update(cx, |picker, cx| picker.clear_query(window, cx));
        });
        let list = cx.new(|cx| ListState::new(delegate, window, cx));
        // A single-line search field keeps `up` and `down` to itself: it has one
        // line to move a caret through, so the pair never reaches a list binding.
        // `enter` and `escape` are the other two keys this card owns, and they are
        // answered here for the same reason the arrows are: the field is a sibling
        // of the list rather than a descendant of it, so an action dispatched at the
        // caret does not bubble into the list at all. Four keys are all this card
        // intercepts. A card outlives its popover — the Shell keeps one per
        // switcher for the life of the app — so an interceptor that answered
        // without asking would move a closed card's cursor and swallow the arrows
        // the surface behind it owns.
        let for_navigation = cx.weak_entity();
        let navigation = cx.intercept_keystrokes(move |event, window, cx| {
            let modifiers = event.keystroke.modifiers;
            if modifiers.alt
                || modifiers.function
                || modifiers.control
                || modifiers.platform
                || modifiers.shift
            {
                return;
            }
            let key = match event.keystroke.key.as_str() {
                "up" => PickerKey::Up,
                "down" => PickerKey::Down,
                "enter" => PickerKey::Confirm,
                "escape" => PickerKey::Cancel,
                _ => return,
            };
            let Ok(answered) = for_navigation.update(cx, |picker, cx| {
                if !picker.owns_focus(window, cx) {
                    return false;
                }
                match key {
                    PickerKey::Up => picker.move_selection(-1, window, cx),
                    PickerKey::Down => picker.move_selection(1, window, cx),
                    PickerKey::Confirm => {
                        let list = picker.list.clone();
                        list.update(cx, |list, cx| list.delegate_mut().select(window, cx));
                    }
                    PickerKey::Cancel => picker.on_cancel(window, cx),
                }
                true
            }) else {
                return;
            };
            if answered {
                cx.stop_propagation();
            }
        });
        let initial = list.read(cx).delegate().selected.map(IndexPath::new);
        if let Some(index) = initial {
            list.update(cx, |list, cx| {
                list.set_selected_index(Some(index), window, cx)
            });
        }
        Self {
            list,
            query,
            kind,
            _query_changed: query_changed,
            _navigation: navigation,
        }
    }

    /// The handle the search field owns, so a host can put the caret in it.
    pub fn input_focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.read(cx).focus_handle(cx)
    }

    /// Takes a new list without building a new card.
    ///
    /// The list is the one thing about this card that changes while it is open:
    /// the clusters behind it reload, and the namespaces follow the cluster. A
    /// card per frame would keep that current, at the price of a frame loop that
    /// never ends, so the host hands the list in instead and the card answers
    /// only when the list is actually different.
    pub fn set_options(&mut self, options: Vec<PickerOption>, cx: &mut Context<Self>) {
        self.list.update(cx, |list, cx| {
            if list.delegate_mut().take_options(options) {
                cx.notify();
            }
        });
    }

    /// A press on a row: the value moves the cursor, then answers the Shell.
    fn pick(&mut self, value: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        let list = self.list.clone();
        list.update(cx, |list, cx| list.delegate_mut().press(value, window, cx));
    }

    /// True while the keyboard is answering this card.
    fn owns_focus(&self, window: &Window, cx: &App) -> bool {
        self.input_focus_handle(cx).is_focused(window)
    }

    /// The query the list answers, and the rows it left.
    ///
    /// The cursor is put on the list as well as on the delegate, because the list is
    /// what measures the rows and draws the selection, and it only learns where the
    /// cursor is from its own answer.
    fn set_query(&mut self, query: String, window: &mut Window, cx: &mut Context<Self>) {
        let list = self.list.clone();
        list.update(cx, |list, cx| {
            list.delegate_mut().take_query(query);
            let selected = list.delegate().selected.map(IndexPath::new);
            list.set_selected_index(selected, window, cx);
            list.scroll_to_selected_item(window, cx);
            cx.notify();
        });
    }

    fn clear_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The value is written rather than cleaned: `clean` puts back the value the
        // state was built with, which for a field a host prefilled is exactly the
        // value the reader is trying to get rid of. This field is never prefilled,
        // and the filter is run here rather than left to the event, because a
        // programmatic value is not one of the edits that reports itself.
        self.query
            .update(cx, |query, cx| query.set_value(String::new(), window, cx));
        self.set_query(String::new(), window, cx);
    }

    /// The two Escape steps: the first clears the field, the next one closes.
    fn on_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let searching = !self.list.read(cx).delegate().query.trim().is_empty();
        if searching {
            self.clear_query(window, cx);
            return;
        }
        let dismiss = self.list.read(cx).delegate().on_dismiss.clone();
        dismiss(window, cx);
    }

    /// The row the cursor is on, and the command edge that moves it.
    fn move_selection(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        self.list.update(cx, |list, cx| {
            let count = list.delegate().visible.len();
            if count == 0 {
                return;
            }
            let current = list.delegate().selected;
            let next = match current {
                Some(row) => row.saturating_add_signed(delta).min(count - 1),
                None if delta.is_negative() => count - 1,
                None => 0,
            };
            list.set_selected_index(Some(IndexPath::new(next)), window, cx);
            list.scroll_to_selected_item(window, cx);
        });
    }
}

impl Render for SearchablePicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (visible, total, current) = {
            let delegate = self.list.read(cx).delegate();
            (
                delegate.visible.len(),
                delegate.options.len(),
                delegate.current_label(),
            )
        };
        let count = result_label(visible, total);
        let kind = self.kind;
        let list = self.list.clone();
        let placeholder = SharedString::from(kind.search_label());
        let focus = self.input_focus_handle(cx);
        let focused = focus.is_focused(window);
        let for_clear = cx.weak_entity();
        let clear = move |window: &mut Window, cx: &mut App| {
            if let Some(picker) = for_clear.upgrade() {
                picker.update(cx, |picker, cx| picker.clear_query(window, cx));
            }
            // The control sits inside the field, so the caret goes back where it
            // was rather than leaving the reader with nowhere to type.
            window.focus(&focus, cx);
        };
        v_flex()
            .id("searchable-picker")
            .debug_selector(|| "searchable-picker".to_owned())
            .w(px(f32::from(design::size::INSPECTOR_MIN)))
            .min_w(px(0.0))
            .max_w_full()
            // One boundary, and it is the popover's. The hosting `Popover` and the
            // menu it builds already give this card the raised surface, a corner
            // radius, a hairline ring and `shadow::popover`, so a second line one
            // step inside it was two surfaces faking a level they did not have. This
            // is why the card draws no radius and no shadow of its own: the
            // popover's treatment *is* the card's, and a popover sits close to the
            // surface it belongs to rather than casting the palette's
            // `shadow::overlay` across the window.
            .bg(design::role::surface_raised(cx).alpha(1.0))
            // The content inset is one constant, restated by the bands that carry
            // it and by nothing else: a card whose children each name their own
            // padding is a card whose columns drift.
            .pt(CONTENT_INSET)
            .pb(CONTENT_INSET)
            .gap(design::space::SM)
            .role(Role::Group)
            .aria_label(kind.title())
            .aria_keyshortcuts("ArrowUp ArrowDown Enter Escape")
            // The title names the task in the panel-title role and the primary ink;
            // the count is a label in the tertiary ink, in a trailing lane of its
            // own. Two runs on one baseline read as one run-on string — "Switch
            // namespace 12 of 21 results" — and the reader has to work out which
            // half is the name. Both are built by the shared lanes helper, so the
            // title starts on the same spine as every row name and the count's right
            // edge is on the same line as every row's check lane.
            .child(
                h_flex()
                    .id("searchable-picker-title")
                    .debug_selector(|| "searchable-picker-title".to_owned())
                    .w_full()
                    .flex_none()
                    .h(ROW_HEIGHT)
                    .px(CONTENT_INSET)
                    .child(lanes(
                        None,
                        design::role::fg_tertiary(cx),
                        label_panel_title(kind.title()),
                        trailing_lane(
                            Some(
                                label(count, design::text::LABEL)
                                    .text_color(design::role::fg_tertiary(cx))
                                    .truncate()
                                    .into_any_element(),
                            ),
                            false,
                            design::role::fg_primary(cx),
                        ),
                    )),
            )
            // The current value is the second line of the header block, not a
            // footer: it says what the answer would be if the reader dismissed the
            // card now, and the toolbar trigger behind it already says the same
            // thing. It is a caption, so it is `text::LABEL` in the tertiary ink —
            // quiet enough that the list under it is what the card is for, and on
            // the same spine as the title above it.
            .when_some(current, |this, current| {
                this.child(
                    h_flex()
                        .id("searchable-picker-scope")
                        .debug_selector(|| "searchable-picker-scope".to_owned())
                        .w_full()
                        .flex_none()
                        .px(CONTENT_INSET)
                        .aria_label(format!("Current {}: {current}", kind.singular()))
                        .child(lanes(
                            None,
                            design::role::fg_tertiary(cx),
                            label(format!("Current: {current}"), design::text::LABEL)
                                .text_color(design::role::fg_tertiary(cx))
                                .truncate(),
                            trailing_lane(None, false, design::role::fg_primary(cx)),
                        )),
                )
            })
            .child(search_field(&self.query, placeholder, clear, focused, cx))
            .child(
                div()
                    .id("searchable-picker-results")
                    .debug_selector(|| "searchable-picker-results".to_owned())
                    .w_full()
                    .min_w(px(0.0))
                    .aria_label(kind.results_label())
                    // The list owns the cap, and it caps the rows rather than the
                    // field: eight results is the card's budget, and the field above
                    // them is not part of it. Every other style the list takes is
                    // read as one of its own options, so only this cap crosses the
                    // boundary — the rows' own inset lives on the row, so the
                    // list's scrollbar still sits against the card's trailing edge
                    // instead of being pulled in by a padding. The eight is counted
                    // in the same row token the rows are drawn at.
                    .child(List::new(&list).max_h(px(f32::from(ROW_HEIGHT) * LIST_ROWS))),
            )
    }
}

/// The two picker cards, one per switcher, kept for as long as the app runs.
///
/// A `Popover` renders its content on every frame, so a card built per frame
/// takes focus on the next frame, and that focus is what asks for the frame after
/// it: the open switcher never stopped rendering, and neither did the loop. One
/// card per kind is what ends it. The Shell's own open flag still decides
/// whether a card is on screen, so a cached card that is not open costs nothing.
#[derive(Default)]
pub(super) struct PickerCards {
    cluster: Option<Entity<SearchablePicker>>,
    namespace: Option<Entity<SearchablePicker>>,
}

impl PickerCards {
    pub(super) fn slot_mut(&mut self, kind: PickerKind) -> &mut Option<Entity<SearchablePicker>> {
        match kind {
            PickerKind::Cluster => &mut self.cluster,
            PickerKind::Namespace => &mut self.namespace,
        }
    }
}

impl Global for PickerCards {}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use gpui_kit::{TestAppContext, VisualTestContext};

    use super::*;

    fn option(value: &str, label: &str) -> PickerOption {
        PickerOption::new(value, label)
    }

    /// A card with the Shell's two answers wired to cells, so a test can read
    /// what the card chose without a Shell behind it.
    #[allow(clippy::type_complexity)]
    fn shell(
        options: Vec<PickerOption>,
    ) -> (
        impl FnOnce(&mut Window, &mut Context<SearchablePicker>) -> SearchablePicker,
        Rc<Cell<Option<String>>>,
        Rc<Cell<bool>>,
    ) {
        let selected = Rc::new(Cell::new(None));
        let dismissed = Rc::new(Cell::new(false));
        let on_select = selected.clone();
        let on_dismiss = dismissed.clone();
        let view = move |window: &mut Window, cx: &mut Context<SearchablePicker>| {
            SearchablePicker::new(
                PickerConfig {
                    kind: PickerKind::Namespace,
                    options,
                    on_select: Rc::new(move |value, _, _| {
                        on_select.set(Some(value.to_string()));
                    }),
                    on_dismiss: Rc::new(move |_, _| on_dismiss.set(true)),
                },
                window,
                cx,
            )
        };
        (view, selected, dismissed)
    }

    /// Builds a card and puts the caret in its search field.
    #[allow(clippy::type_complexity)]
    fn open(
        cx: &mut gpui_kit::TestAppContext,
        options: Vec<PickerOption>,
    ) -> (
        Entity<SearchablePicker>,
        &mut VisualTestContext,
        Rc<Cell<Option<String>>>,
        Rc<Cell<bool>>,
    ) {
        let (view, selected, dismissed) = shell(options);
        let (picker, cx) = cx.add_window_view(view);
        cx.run_until_parked();
        let focus = picker.read_with(cx, |picker, cx| picker.input_focus_handle(cx));
        cx.update(|window, cx| window.focus(&focus, cx));
        (picker, cx, selected, dismissed)
    }

    /// The value under the cursor, read through the list the card renders.
    fn cursor(picker: &Entity<SearchablePicker>, cx: &VisualTestContext) -> Option<String> {
        picker.read_with(cx, |picker, cx| {
            let list = picker.list.read(cx);
            let index = list.selected_index()?;
            list.delegate()
                .visible
                .get(index.row)
                .map(|option| option.value.to_string())
        })
    }

    fn query(picker: &Entity<SearchablePicker>, cx: &VisualTestContext) -> String {
        picker.read_with(cx, |picker, cx| {
            picker.list.read(cx).delegate().query.clone()
        })
    }

    fn count(picker: &Entity<SearchablePicker>, cx: &VisualTestContext) -> usize {
        picker.read_with(cx, |picker, cx| {
            picker.list.read(cx).delegate().visible.len()
        })
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
    fn duplicate_labels_use_deterministic_disambiguators() {
        let mut options = vec![option("ctx-a", "shared"), option("ctx-b", "shared")];
        options[0].source = Some("https://a.example".into());
        disambiguate_options(&mut options);
        assert_eq!(options[0].label, "shared · https://a.example");
        assert_eq!(options[1].label, "shared · Context ctx-b");
    }

    /// The cursor stays inside the results, in both directions, and a query that
    /// matches nothing leaves nothing to move through.
    #[gpui_kit::test]
    fn the_cursor_stays_within_the_results(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (picker, cx) = cx.add_window_view(|window, cx| {
            SearchablePicker::new(
                PickerConfig {
                    kind: PickerKind::Namespace,
                    options: vec![
                        option("alpha", "Alpha"),
                        option("beta", "Beta"),
                        option("gamma", "Gamma"),
                    ],
                    on_select: Rc::new(|_, _, _| {}),
                    on_dismiss: Rc::new(|_, _| {}),
                },
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let focus = picker.read_with(cx, |picker, cx| picker.input_focus_handle(cx));
        cx.update(|window, cx| window.focus(&focus, cx));
        assert_eq!(cursor(&picker, cx).as_deref(), Some("alpha"));
        cx.simulate_keystrokes("down");
        assert_eq!(cursor(&picker, cx).as_deref(), Some("beta"));
        cx.simulate_keystrokes("up up");
        assert_eq!(
            cursor(&picker, cx).as_deref(),
            Some("alpha"),
            "the cursor stops at the first row instead of wrapping"
        );
        cx.simulate_keystrokes("down down down");
        assert_eq!(
            cursor(&picker, cx).as_deref(),
            Some("gamma"),
            "the cursor stops at the last row instead of wrapping"
        );

        cx.simulate_input("zzz");
        cx.run_until_parked();
        assert_eq!(count(&picker, cx), 0, "nothing matches");
        cx.simulate_keystrokes("down up");
        assert_eq!(
            count(&picker, cx),
            0,
            "a cursor with no row to move to is not a crash"
        );
    }

    #[gpui_kit::test]
    fn enter_activates_filtered_selection(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (picker, cx, selected, dismissed) = open(
            cx,
            vec![
                option("alpha", "Alpha"),
                option("beta", "Beta"),
                option("gamma", "Gamma"),
            ],
        );
        cx.simulate_input("bet");
        cx.run_until_parked();
        assert_eq!(cursor(&picker, cx).as_deref(), Some("beta"));
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(selected.take().as_deref(), Some("beta"));
        assert!(dismissed.get());
    }

    /// The card's own key contract: Up and Down walk the rows, Enter chooses,
    /// and Escape takes two steps -- the first clears the field, the second
    /// closes the card.
    #[gpui_kit::test]
    fn the_two_escape_steps_clear_the_field_before_they_close_the_card(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (picker, cx, selected, dismissed) =
            open(cx, vec![option("alpha", "Alpha"), option("beta", "Beta")]);
        cx.simulate_input("bet");
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(query(&picker, cx), "", "the first Escape clears the search");
        assert_eq!(count(&picker, cx), 2, "and brings every row back");
        assert!(
            !dismissed.get(),
            "clearing the search is not closing the card"
        );
        assert_eq!(selected.take(), None);
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(dismissed.get(), "the second Escape closes the card");
    }

    /// The rows scroll to keep the cursor in view, and the field keeps its own
    /// caret: `home` and `end` are the field's, not the list's.
    #[gpui_kit::test]
    fn the_list_scrolls_to_the_cursor_and_the_field_keeps_its_caret(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (picker, cx, _, _) = open(
            cx,
            (0..20)
                .map(|index| {
                    let value = format!("item-{index}");
                    let label = format!("Item {index}");
                    option(&value, &label)
                })
                .collect(),
        );
        for _ in 0..19 {
            cx.simulate_keystrokes("down");
        }
        cx.run_until_parked();
        assert_eq!(cursor(&picker, cx).as_deref(), Some("item-19"));
        let scroll = picker.read_with(cx, |picker, cx| {
            picker.list.read(cx).scroll_handle().clone()
        });
        assert!(
            scroll.base_handle().max_offset().y > px(0.0),
            "twenty rows do not fit in the card's eight-row budget"
        );
        assert!(
            scroll.base_handle().offset().y < px(0.0),
            "and the cursor at the end has scrolled the list past the fold"
        );

        // `home` and `end` belong to the field while the field holds the caret.
        let before = cursor(&picker, cx);
        cx.simulate_keystrokes("home end");
        assert_eq!(cursor(&picker, cx), before);
    }

    /// `ctrl-home` and `ctrl-end` are caret motions in a text field, on every
    /// platform, and the list's command edge is Up, Down, Enter and Escape.
    #[gpui_kit::test]
    fn the_command_edge_belongs_to_the_field_and_the_list_keeps_the_arrows(
        cx: &mut TestAppContext,
    ) {
        crate::init_ui(cx);
        let (picker, cx, _, _) = open(
            cx,
            (0..6)
                .map(|index| option(&format!("item-{index}"), &format!("Item {index}")))
                .collect(),
        );
        let before = cursor(&picker, cx);
        cx.simulate_keystrokes("ctrl-end");
        assert_eq!(
            cursor(&picker, cx),
            before,
            "ctrl-end moves the caret to the end of the query, not to the last row"
        );
        cx.simulate_keystrokes("ctrl-home");
        assert_eq!(cursor(&picker, cx), before);
        cx.simulate_keystrokes("down");
        assert_eq!(cursor(&picker, cx).as_deref(), Some("item-1"));
    }

    /// A press on a row chooses it, and each row is where its selector says it
    /// is: the popover host blocks hover, so a row that answered only a click
    /// would never be chosen at all.
    #[gpui_kit::test]
    fn a_press_on_a_row_chooses_that_row(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (view, selected, _) = shell(
            (0..3)
                .map(|index| {
                    let mut option = option(&format!("item-{index}"), &format!("Item {index}"));
                    option.debug_selector = Some(format!("picker-option-{index}").into());
                    option
                })
                .collect(),
        );
        let (picker, cx) = cx.add_window_view(view);
        cx.run_until_parked();
        let selectors: [&'static str; 3] =
            ["picker-option-0", "picker-option-1", "picker-option-2"];
        let boxes: Vec<_> = selectors
            .iter()
            .map(|selector| {
                cx.debug_bounds(selector)
                    .unwrap_or_else(|| panic!("{selector} is laid out"))
            })
            .collect();
        assert!(boxes[0].top() < boxes[1].top(), "rows run downwards");
        assert!(boxes[1].top() < boxes[2].top(), "rows run downwards");
        cx.simulate_click(boxes[1].center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(selected.take().as_deref(), Some("item-1"));
        assert!(picker.read_with(cx, |_, _| true), "the card is still alive");
    }

    /// The card has one boundary, and it is the popover's.
    ///
    /// `elevation_2` already gives the hosting popover the raised surface, a
    /// radius, a 1px `border_variant` and a shadow. The card drew a second line
    /// in the same token one step inside it, and under Taffy's `BorderBox` that
    /// border also came out of the content box. The command palette's card is
    /// bounded the same way this one now is.
    #[gpui_kit::test]
    fn the_card_inside_padding_is_the_padding_and_nothing_else(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (_picker, cx) = cx.add_window_view(|window, cx| {
            SearchablePicker::new(
                PickerConfig {
                    kind: PickerKind::Namespace,
                    options: vec![option("alpha", "Alpha"), option("beta", "Beta")],
                    on_select: Rc::new(|_, _, _| {}),
                    on_dismiss: Rc::new(|_, _| {}),
                },
                window,
                cx,
            )
        });
        cx.simulate_resize(gpui_kit::size(px(1200.), px(800.)));
        cx.run_until_parked();
        let card = cx.debug_bounds("searchable-picker").expect("card");
        let title = cx
            .debug_bounds("searchable-picker-title")
            .expect("title row");
        // The rows carry the padding, so their own boxes are the card's box.
        // `debug_bounds` reports border boxes, so a row that has been pushed in
        // would report the push and nothing else.
        assert!(
            f32::from(title.left() - card.left()).abs() <= 0.5
                && f32::from(title.right() - card.right()).abs() <= 0.5,
            "the title row is the card's own width and holds its padding inside itself, so the \
             card draws no second border inside the popover's: {}px in, {}px out",
            f32::from(title.left() - card.left()),
            f32::from(title.right() - card.right()),
        );
    }

    /// A repair the picker cannot run itself needs a control to run, and the
    /// repair must never read as choosing a namespace.
    #[gpui_kit::test]
    fn the_empty_state_repairs_the_search_without_selecting_a_namespace(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (picker, cx, selected, _) = open(cx, vec![option("perf", "perf")]);
        cx.simulate_input("zzz");
        cx.run_until_parked();
        assert_eq!(count(&picker, cx), 0, "nothing matches");
        let clear = cx
            .debug_bounds("searchable-picker-clear-search")
            .expect("the empty state offers a way out");
        assert!(clear.size.height > px(0.0));
        cx.simulate_click(clear.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(query(&picker, cx), "", "the control clears the search");
        assert_eq!(count(&picker, cx), 1, "and brings the row back");
        assert_eq!(
            selected.take(),
            None,
            "repairing the search must not activate a namespace"
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

    /// A title above the search field, and a control in the empty state that
    /// only exists when there is something to clear.
    #[gpui_kit::test]
    fn the_title_above_the_field_stays_a_title_and_the_empty_state_offers_clear_search(
        cx: &mut TestAppContext,
    ) {
        crate::init_ui(cx);
        let (picker, cx, _, _) = open(cx, vec![option("perf", "perf")]);
        let title = cx
            .debug_bounds("searchable-picker-title")
            .expect("the picker names its task");
        let list = cx
            .debug_bounds("searchable-picker-results")
            .expect("the picker has a search field");
        assert!(
            title.bottom() <= list.top(),
            "the title sits above the field, so it cannot read as the field's value"
        );
        assert!(
            cx.debug_bounds("searchable-picker-clear-search").is_none(),
            "nothing to clear while the field is empty"
        );

        let focus = picker.read_with(cx, |picker, cx| picker.input_focus_handle(cx));
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_input("zzz");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("searchable-picker-clear-search").is_some(),
            "the empty state offers a way out"
        );
    }

    /// The current selection is stated on its own line, and the line is not
    /// truncated by the picker's width.
    #[gpui_kit::test]
    fn the_current_scope_gets_its_own_line_under_the_field(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (_picker, cx) = cx.add_window_view(|window, cx| {
            let mut current = PickerOption::new("perf", "perf");
            current.current = true;
            SearchablePicker::new(
                PickerConfig {
                    kind: PickerKind::Namespace,
                    options: vec![current],
                    on_select: Rc::new(|_, _, _| {}),
                    on_dismiss: Rc::new(|_, _| {}),
                },
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let results = cx
            .debug_bounds("searchable-picker-results")
            .expect("the results are laid out");
        let scope = cx
            .debug_bounds("searchable-picker-scope")
            .expect("the current selection is stated");
        assert!(
            scope.bottom() <= results.top(),
            "the scope line is under the field and above the list"
        );
    }

    /// A narrow window must not push the card's own rows outside it.
    #[gpui_kit::test]
    fn narrow_widths_keep_the_card_rows_inside_the_card(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        for width in [240.0, 150.0] {
            let (_picker, cx) = cx.add_window_view(|window, cx| {
                SearchablePicker::new(
                    PickerConfig {
                        kind: PickerKind::Namespace,
                        options: vec![option("alpha", "Alpha"), option("beta", "Beta")],
                        on_select: Rc::new(|_, _, _| {}),
                        on_dismiss: Rc::new(|_, _| {}),
                    },
                    window,
                    cx,
                )
            });
            cx.simulate_resize(gpui_kit::size(px(width), px(480.0)));
            cx.run_until_parked();
            let picker = cx
                .debug_bounds("searchable-picker")
                .expect("picker is laid out");
            for row in ["searchable-picker-title", "searchable-picker-results"] {
                let row = cx
                    .debug_bounds(row)
                    .unwrap_or_else(|| panic!("{row} is laid out"));
                assert!(row.left() >= picker.left(), "{row} at width={width}");
                assert!(row.right() <= picker.right(), "{row} at width={width}");
            }
        }
    }
}
