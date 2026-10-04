//! Rendering for the shell toolbar, resource tree, center tabs, panels, and command palette.
#[path = "searchable_picker.rs"]
mod searchable_picker;

pub(super) use searchable_picker::PickerKind;

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::badge::Badge;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::label::Label;
use gpui_kit::component::marker::{Marker, MarkerContent, MarkerIcon};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::separator::Separator;
use gpui_kit::component::{
    ActiveTheme, Disableable as _, FocusableExt as _, Icon, RoleOverride, Selectable as _, Sizable,
    Size, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::{
    Anchor, AnyElement, App, Bounds, ClickEvent, ClipboardItem, Context, DefiniteLength,
    DismissEvent, Div, DragMoveEvent, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, Keystroke, MouseButton, MouseDownEvent, ParentElement, Pixels, Render, Role,
    SharedString, Stateful, StatefulInteractiveElement, Styled, Window, div, point, px, size,
};
use k8s_core::cluster::Health;

use super::commands::CommandRun;
use super::tree::{TreeRow, TreeRowKind};
use super::{
    CatalogState, CenterTab, CenterTabDragPayload, CenterTabDragState, ConnectionState,
    DIALOG_WIDTH, Dialog, NamespaceState, Shell, TabContent, TabView, Toast, ToggleCommandPalette,
    ToggleLeftPanel, ToggleRightPanel,
};
use crate::panels::common::{self, label_panel_title};
use crate::panels::helm::HelmView;
use crate::{
    design, keymap,
    table_view::{ClusterSession, TableStatus, TextInput},
    update::UpdatePhase,
};
use searchable_picker::{
    PickerCards, PickerConfig, PickerOption, PickerSelectHandler, SearchablePicker,
    disambiguate_options,
};

/// The widest the palette gets. The card also takes at most 60% of the window width, so a
/// narrow window keeps the list readable instead of letting the card reach the window edge.
/// `shell::mod` sizes the palette's search field from this value.
pub(super) const PALETTE_WIDTH: f32 = 720.0;
/// The tallest the palette gets. The card also takes at most 60% of the window height, so the
/// palette and the space above it fit the shortest supported window.
pub(super) const PALETTE_MAX_HEIGHT: f32 = 560.0;
/// Share of the window one axis of the palette may take before its own maximum applies.
const PALETTE_VIEWPORT_SHARE: f32 = 0.6;
/// Bands in one list edge fade, so a row the window cuts reads as more content.
const PALETTE_FADE_BANDS: usize = 4;
/// Number of actions in the update popover: check, retry, restart.
pub(super) const UPDATE_OVERLAY_ACTIONS: usize = 3;
const ACTION_BUTTON_WIDTH: f32 = 112.0;
const TOAST_MAX_WIDTH: f32 = 560.0;
const TOAST_MAX_HEIGHT: f32 = 64.0;

/// The keys the resource tree answers, in the form `aria-keyshortcuts` wants.
///
/// Every one of these is handled in `Shell::on_tree_key_down`, and the list is the one place a
/// reader can be told about them without trying them. It is the same set the Dock strip and the
/// Inspector tab strip publish for themselves, so the three lists in the app no longer answer to
/// three different keyboard models. The filter field above the tree keeps its own characters: it
/// is a sibling of this container rather than a child, so `tree_focus_handle` is not focused while
/// the reader is typing into it.
pub(super) const TREE_KEYSHORTCUTS: &str =
    "ArrowUp ArrowDown ArrowLeft ArrowRight Home End Enter Space F10 Shift+F10 Menu ContextMenu";

/// The name of the top bar's navigation control in a window that cannot hold the resource tree.
///
/// It is the action's own name — the one the menu and `⌘⇧K` already use for the same list of
/// kinds — because this is not a second way to do something new, it is the way the tree's own
/// slot in the bar does that job at a width the tree cannot be drawn at.
const SIDEBAR_NARROW_LABEL: &str = "Choose resource kind";
/// Why a control is sitting where the tree usually is.
///
/// A control that replaces a panel has to say what it replaced, or the reader spends the width
/// of the title bar wondering where the tree went and never finds the answer.
const SIDEBAR_NARROW_HINT: &str =
    "This window is too narrow for the resource tree, so its list of kinds opens here.";

fn toast_bottom_inset(dock_open: bool, dock_height: f32) -> f32 {
    super::status_bar_height()
        + f32::from(design::space::LG)
        + if dock_open {
            f32::from(design::border::HIT) + dock_height
        } else {
            0.0
        }
}

/// The top of the update card: just under the title bar, at the window's corner.
///
/// It used to be anchored *below* the table — the title bar, the tab strip, the
/// table header and the first row, plus a gap — so that a growing chrome stack
/// could not push the card onto a row. It worked by counting bands, and the
/// moment the resource header arrived the count was one short and the card landed
/// on the first row of the table.
///
/// Counting bands is the wrong shape for the question. The question is "does this
/// float over something the reader came for?", and the answer belongs to the
/// card's own height, not to a sum of unrelated chrome. So the card hangs off the
/// title bar — where a floating notification belongs, and where it is expected —
/// and its height is what keeps it clear. A card taller than the chrome above the
/// first row is a card that is too big, and that is a card worth shrinking.
fn update_overlay_top() -> Pixels {
    design::size::TITLE_BAR + design::space::XS
}

struct CenterTabDragPreview {
    title: SharedString,
    icon: IconName,
}

/// Which tab one center-tab item draws, and where that tab sits in the set.
///
/// The tab bar renders pinned and ordinary tabs from two lists, so the position inside the set
/// and the size of the set are decided by the caller. Keeping them with the tab turns the item
/// renderer's argument list into "this tab, in this window".
struct CenterTabSlot {
    index: usize,
    position: usize,
    total: usize,
    title: SharedString,
    icon: IconName,
    /// `UI-SPEC.md` §4.3's middot between two open views, drawn inside the item that follows it.
    ///
    /// It belongs to the row rather than between two rows so the strip keeps one child per tab.
    /// The scroll handles address their row by child index - `reveal_center_tab` scrolls to
    /// `position`, and so does every test that scrolls to an end - so a separator inserted as a
    /// sibling shifts every index after the first tab and leaves the last tab unreachable.
    separated: bool,
}

impl Render for CenterTabDragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = design::colors(cx);
        h_flex()
            .h(design::size::TAB_BAR)
            .max_w(design::size::SIDEBAR_MIN)
            .px(design::space::SM)
            .gap(design::space::XS)
            .items_center()
            .rounded_md()
            .border_1()
            .border_color(colors.border_focused)
            .bg(colors.elevated_surface_background.alpha(0.78))
            .opacity(0.82)
            .child(Icon::new(self.icon).xsmall().text_color(colors.text))
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .child(text(self.title.clone()).truncate()),
            )
    }
}

/// Height the palette spends on everything except the list: the scope block, the search field,
/// and the footer hints.
///
/// This is the prediction for the first frame, because a card cannot be measured before it has a
/// height. It used to budget one line for a two-line scope block, and it came out 27px short. The
/// card is `overflow_hidden` and its list is `flex_1`, so every missing pixel landed on the list:
/// the last result was cut off with no scrollbar, because the list read its overflow from the same
/// wrong number. `Shell::palette_chrome` replaces this with the measured chrome from the second
/// frame on, so the prediction only has to survive one frame.
///
/// It errs tall on purpose. The title and the scope line are labels, so their line box is the
/// font's rather than `text::CAPTION_LINE_HEIGHT`, and this takes `BODY_LINE_HEIGHT` for them
/// instead: a first frame a few pixels too tall shows a little empty space under the list, while a
/// few pixels too short clips a row the reader is about to choose.
fn palette_chrome_height() -> f32 {
    // The scope block: `space::SM` of padding above, the title row, a `space::XS` gap, and the
    // scope line under it.
    f32::from(design::space::SM)
        + f32::from(design::text::BODY_LINE_HEIGHT) * 2.0
        + f32::from(design::space::XS)
    // The field: its own `size::CONTROL` frame inside a `space::SM` margin and the `space::XS`
    // that gives the card's focus ring room to sit outside the field's own edge.
        + (f32::from(design::space::SM) + f32::from(design::space::XS)) * 2.0
        + f32::from(design::size::CONTROL)
    // The footer. Its rule sits inside its `size::ROW` box, so the row covers the line.
        + f32::from(design::size::ROW)
}

/// How tall the palette's list is before anything is measured. The card has to cover this plus
/// the chrome, or the last row is cut off, and gpui-kit's `Scrollbar` takes the list's own
/// overflow from there.
pub(super) fn palette_list_content(matches: &[&super::commands::Command]) -> f32 {
    let row = f32::from(design::size::PALETTE_ROW);
    let mut content = f32::from(design::space::XS) * 2.0;
    let mut group: Option<&str> = None;
    for command in matches {
        // A group header carries the space that separates it from the group above, and it is
        // not a row: `size::GROUP_HEAD` plus its own margins, which is what
        // `palette_group_head_height` is the one answer to.
        if group != Some(command.group.as_ref()) {
            group = Some(command.group.as_ref());
            content += palette_group_head_height();
        }
        content += row;
    }
    if matches.is_empty() {
        // The empty branch is the shared empty state, and this is what that state draws: a
        // `size::ICON_LARGE` glyph, a `text::TITLE` line, a `text::BODY` line and the
        // `size::CONTROL` control, with `space::SM` between the glyph and the two lines,
        // between the two lines, and between the block and the control. It used to budget one
        // caption line and the control, which is a sentence short of the state — and the card
        // is `overflow_hidden` over a `flex_1` list, so the missing pixels clipped the one
        // control the state exists for, off the bottom of a card that reported itself large
        // enough to hold everything it listed.
        content += f32::from(design::size::ICON_LARGE)
            + f32::from(design::space::SM) * 3.0
            + f32::from(design::text::TITLE_LINE_HEIGHT)
            + f32::from(design::text::BODY_LINE_HEIGHT)
            + f32::from(design::size::CONTROL);
    }
    content
}

/// How tall the card is, given what it spends on everything but the list. The caller passes the
/// measured chrome when there is one, so a wrong constant cannot quietly become the layout.
fn palette_card_height(matches: &[&super::commands::Command], chrome: f32) -> f32 {
    (palette_list_content(matches) + chrome).min(PALETTE_MAX_HEIGHT)
}

/// The card's height for a set of matches, at the predicted chrome.
#[cfg(test)]
pub(super) fn palette_height(matches: &[&super::commands::Command]) -> f32 {
    palette_card_height(matches, palette_chrome_height())
}

pub(super) fn palette_result_label(count: usize) -> String {
    // The palette holds more rows than anywhere else in the app, so it is where a missing
    // thousands separator shows up first. `DESIGN.md` §9 claims the count format is one format.
    design::format::count_with_noun(count, "result", "results")
}

// ── Palette chrome ───────────────────────────────────────────────────────────
//
// One scrim, one focus ring, one keycap, one set of lanes. The resource search in
// `panels::search` answers the same four questions for the sibling overlay, and these
// are deliberately the same answers: two modals that own the whole window must read as
// one component family, or "which of these is the real one" becomes a question the reader
// has to answer before they can use either.

/// Share of the shared scrim this card keeps.
///
/// `design::role::surface_backdrop` is the product's one scrim and it is right for a surface
/// that owns the whole window. At its full strength the app behind the card is gone: the
/// cluster tree, the table and the status bar all drop to one flat value and the card floats
/// in a void. `modality.md > Best practices` warns about exactly that — a modal which obscures
/// its previous context makes people lose track of the task they suspended — and the palette is
/// dismissed straight back into the session it came from.
///
/// Half the app's own canvas ink, composited under the shared token, keeps the state change
/// (everything behind the card moves one step down the ladder) while leaving the toolbar, the
/// tree and the current view legible enough to say what the palette was opened from. It is one
/// number composed with the shared token rather than a second scrim, so a theme that moves the
/// canvas moves the scrim with it.
const PALETTE_SCRIM_INK: f32 = 0.38;

/// The scrim behind the palette card. See [`PALETTE_SCRIM_INK`].
fn palette_scrim(cx: &App) -> gpui_kit::Hsla {
    design::role::surface_backdrop_at(cx, PALETTE_SCRIM_INK)
}

/// One inset every lane in the card shares: the rows, the group heads, the footer and the
/// scope line under the field.
///
/// It is `space::MD` rather than a number of its own because the scope line and the title row
/// already carry it, and a second inset for the list is how the search field ended up lining
/// up with nothing. One value, four call sites: the leading edge of the card is one spine.
const PALETTE_LANE_PAD: Pixels = design::space::MD;

/// Width of the lane every row's icon sits in, and the size of the glyph in it.
///
/// `size::KIND_ICON` rather than `size::ICON`, and for the reason `panels::search` gives: this
/// is the kind set, drawn and judged at fourteen pixels, and the same glyph in the sidebar and
/// in the resource search is fourteen pixels. A row that reserves no lane moves its label when
/// its glyph's intrinsic width differs, which is a different defect from a row without a glyph.
const PALETTE_ICON_SLOT: Pixels = design::size::KIND_ICON;

/// Width of the lane the "current" check sits in.
///
/// Reserved on every row, not drawn on most of them, for the reason `panels/dock.rs` keeps its
/// status dot: a mark only once there is something to report, but the lane is always there so
/// the labels under it stay on one spine.
const PALETTE_CHECK_SLOT: Pixels = design::space::MD;

/// Width of the trailing lane the shortcut chip and the unavailable badge share.
///
/// One lane for both, so a row's label ends on the same vertical line whether it carries a
/// chord, a reason it cannot run, or nothing. It is sized for the widest chord the shell
/// keymap can put in a palette row — `Ctrl+Alt+Left` and `Ctrl+Page Down` are fourteen
/// characters — at `text::MICRO` plus `space::XS` of padding on each side, with room over.
const PALETTE_CHIP_LANE: Pixels = px(96.);

/// A keycap: the one shape a shortcut has in this card.
///
/// `text::MICRO` on the card's own plane, described by a `border_subtle` hairline rather than
/// a fill, and built from its own type and padding rather than from a width, so every cap in
/// the card is the same box without anyone measuring one. A filled cap is a second surface
/// inside a list of rows, which is what made the chips read as the loudest thing on a quiet
/// row.
///
/// The chip keeps the same appearance on the selected row: the selection is said by the row's
/// own fill and ink, so a chip that changed with the row would be a second competing highlight
/// on the one row the reader is looking at.
fn palette_keycap(chord: &str, cx: &App) -> AnyElement {
    h_flex()
        .flex_none()
        .items_center()
        .px(design::space::XS)
        .py(design::space::XXS)
        .rounded(design::radius::SM)
        .bg(design::role::surface_raised(cx))
        .border_1()
        .border_color(design::role::border_subtle(cx))
        .child(
            Label::new(chord.to_owned())
                .text_size(design::text::MICRO)
                .line_height(design::text::MICRO_LINE_HEIGHT)
                .text_color(design::role::fg_secondary(cx)),
        )
        .into_any_element()
}

/// The four lanes every line in the card's list shares: the current-check slot, the icon slot,
/// the label, and the trailing chip lane.
///
/// A hit row and its group heading both build their content from this one helper, so they
/// cannot disagree about where a label starts or where a chip ends. Two rows that each describe
/// their own columns is how the check column and the shortcut column came to sit at two
/// different distances from the card's edge, and why a group heading read as an ordinary row
/// with no content in it.
fn palette_lanes(
    check: Option<AnyElement>,
    icon: Option<AnyElement>,
    label: AnyElement,
    trailing: Option<AnyElement>,
) -> AnyElement {
    h_flex()
        .w_full()
        .min_w(px(0.0))
        .gap(design::space::SM)
        .items_center()
        .child(
            div()
                .w(PALETTE_CHECK_SLOT)
                .flex_none()
                .flex()
                .items_center()
                .children(check),
        )
        .child(
            div()
                .w(PALETTE_ICON_SLOT)
                .flex_none()
                .flex()
                .items_center()
                .children(icon),
        )
        .child(div().min_w(px(0.0)).flex_1().child(label))
        .child(
            // The trailing lane is its own column even when it is empty, so a row with no chord
            // has the same label measure as a row with one.
            h_flex()
                .flex_none()
                .w(PALETTE_CHIP_LANE)
                .justify_end()
                .items_center()
                .children(trailing),
        )
        .into_any_element()
}

/// Height one group heading takes in the list, margins included.
///
/// `palette_list_content` has to agree with what the heading actually draws or the card is
/// sized from a number nothing on screen matches, and the last row is cut off with no
/// scrollbar — the overflow that would have said so comes from the same wrong number.
fn palette_group_head_height() -> f32 {
    f32::from(design::space::SM)
        + f32::from(design::size::GROUP_HEAD)
        + f32::from(design::space::XXS)
}

/// Alpha of the accent wash the active center tab paints on hover and on press.
///
/// `DESIGN.md` §3.4 asks for a large accent fill to mean one clear primary action, and the tab
/// strip is a navigation surface, not a content area. A full `element_selected` slab measured
/// 1.462:1 in dark and 1.229:1 in light there, which is louder than the selected row it sits
/// above. These two steps keep both states visible without repainting the tab's own surface.
///
/// Composited onto `role::surface_chrome` — the plane the tab actually sits on — they measure
/// well under the slab they replace and well over the point at which a wash stops being a state.
/// `the_active_tab_states_are_three_washes` holds both ends of that range.
const TAB_HOVER_WASH_ALPHA: f32 = 0.08;
const TAB_PRESSED_WASH_ALPHA: f32 = 0.12;

/// Height of the centre tab's pill, inside the `size::OPEN_VIEWS` strip.
const CENTER_TAB_HEIGHT: Pixels = design::size::TAB_PILL;

/// The wash the active center tab adds for hover, or for press when `pressed`.
fn tab_accent_wash(colors: &design::ThemeColors, pressed: bool) -> gpui_kit::Hsla {
    let alpha = if pressed {
        TAB_PRESSED_WASH_ALPHA
    } else {
        TAB_HOVER_WASH_ALPHA
    };
    colors.text_accent.opacity(alpha)
}

/// A body label at the design system's size *and* its line height.
///
/// gpui-kit's `Label` hard-codes a 1.25rem line box, so a label that only carries
/// a size sits on a 20px line whatever the type scale says. The tokens pair the
/// two, so the app's labels set both and the scale stays the one the design owns.
fn text(text: impl Into<SharedString>) -> Label {
    Label::new(text)
        .text_size(design::text::BODY)
        .line_height(design::text::BODY_LINE_HEIGHT)
}

fn text_small(text: impl Into<SharedString>) -> Label {
    Label::new(text)
        .text_size(design::text::CAPTION)
        .line_height(design::text::CAPTION_LINE_HEIGHT)
}

fn section_text(text: impl Into<SharedString>) -> Label {
    Label::new(text)
        .text_size(design::text::TITLE)
        .line_height(design::text::TITLE_LINE_HEIGHT)
}

/// The one line under a connection failure's title.
///
/// `UI-SPEC.md` §4.15 asks a failure to name the step that failed, and §9.3's ten-second rule
/// only works if the reader can tell "still waiting" from "the API server refused" — which is
/// the difference between the two branches. The transport already knows which one it is in, so
/// the words come from there; the fallbacks exist because an empty detail is a fact the reader
/// must still be able to act on rather than a blank line under a title.
fn connection_failure_reason(connection: &ConnectionState) -> SharedString {
    let detail = connection.detail().unwrap_or_default().trim();
    if !detail.is_empty() {
        return SharedString::from(detail);
    }
    SharedString::from(match connection {
        ConnectionState::Reconnecting(_) => {
            "The connection was lost and the app is trying to reach the API server again."
        }
        ConnectionState::Failed(_) => {
            "The API server could not be reached. Check the context and the network, then retry."
        }
        ConnectionState::Connecting | ConnectionState::Live => "The cluster answered.",
    })
}
/// One footer hint: what the key does, then the key.
///
/// The verb is `text::MICRO` in the tertiary ink and the chord is [`palette_keycap`], so a hint
/// reads as a hint and the footer's chips are the same box as the ones in the rows above it.
/// gpui-kit's `Kbd` was both of those two things at one size instead: the footer sat in the
/// rows' own type directly under the results, and its chips were a different shape from the
/// chips the list had just taught the reader to see.
fn footer_hint(verb: &'static str, stroke: &Keystroke, cx: &App) -> AnyElement {
    let line = format!("{verb}: {}", Kbd::format(stroke));
    h_flex()
        .id(format!("palette-hint-{verb}"))
        .flex_none()
        .gap(design::space::XS)
        .items_center()
        // The verb and the chord are one accessible name, so a screen reader reads the hint as
        // one thing instead of stopping at the gap between them.
        .aria_label(line)
        .child(
            Label::new(verb)
                .text_size(design::text::MICRO)
                .line_height(design::text::MICRO_LINE_HEIGHT)
                .text_color(design::role::fg_tertiary(cx)),
        )
        .child(palette_keycap(&Kbd::format(stroke), cx))
        .into_any_element()
}

/// The wash the row the keyboard cursor is on wears, solved against a plane other than the table's.
///
/// `design.rs` exports `row_selected_bg_on` for exactly this problem — a row state landing on a
/// surface the shared tokens were not solved for — but only for the *selected* state. The cursor
/// is the third state between the hover and the selection, and its own tokens
/// (`ROW_FOCUS_ALPHA`, `ROW_STATE_MIN_CONTRAST`) are public, so this is the same three lines the
/// shared helper is built from rather than a second design of the same wash. Promote it beside
/// `row_selected_bg_on` when the token layer next owns the sidebar.
///
/// It has to exist at all: `design::row_focus_bg` is solved against the table's `surface_raised`,
/// and `design.rs` says in so many words that using a row token on the surface it does not belong
/// to leaves it "far too quiet on the surface it actually lands on".
fn tree_cursor_bg(surface: gpui_kit::Hsla, cx: &App) -> gpui_kit::Hsla {
    let wash = design::composite_surface(
        surface,
        design::role::accent(cx).opacity(design::ROW_FOCUS_ALPHA),
    );
    design::graphic_on_with_minimum(surface, wash, design::ROW_STATE_MIN_CONTRAST)
}

/// The keycap for one chord, or nothing when the chord does not parse.
fn keystroke(spec: &str) -> Option<Keystroke> {
    Keystroke::parse(spec).ok()
}

fn toolbar_separator(cx: &Context<Shell>) -> impl IntoElement {
    Separator::vertical()
        .flex_none()
        .h(design::space::LG)
        .color(chrome_hairline(
            design::role::surface_chrome(cx),
            cx.theme().colors.border,
        ))
}

/// A structural rule between two regions of the same window.
///
/// `colors.border` is solved for a control sitting on a panel, and a hairline is not a control: on
/// the chrome band it can land under the floor `design::border::MIN_RULE_CONTRAST` sets for a
/// structural divider, which is how a 1px rule between the rail and the sidebar reads as nothing at
/// all and the two lanes merge into one 284px block. Resolving it against the surface it is painted
/// on is the same move `status_bar.rs` makes for a popover's edge, and it is why the rule — not the
/// two near-identical greys — is what separates the lanes.
pub(super) fn chrome_hairline(
    surface: gpui_kit::Hsla,
    preferred: gpui_kit::Hsla,
) -> gpui_kit::Hsla {
    design::graphic_on_with_minimum(surface, preferred, design::border::MIN_RULE_CONTRAST)
}

/// The band the centre column spends between the title bar and the open-view strip.
///
/// It is mounted whether or not a resource header has anything to put in it, and that is the
/// whole point: the open-view strip is the one band in the centre a reader tracks across every
/// tab, so the moment the header is dropped for a non-resource tab — a Pod detail, the Overview,
/// Helm, a connection failure — the strip and the whole content plane jumped up 40px and jumped
/// back 40px on the way out. A jump of that size on a bar that never changes content is the
/// window telling the reader it is unstable, which is worse than the band that was dropped.
///
/// Reserved rather than left out on purpose: it is what this product already does everywhere
/// else it has a band with nothing in it — the summary strip, the subline slot and the meter slot
/// all keep their space — so an empty header reads as "this tab has no resource header", which is
/// true, and never as "the window just moved".
///
/// The header owns the edge between itself and the open-view strip below it. It used to draw that
/// rule here alone, which is why the two bands read as one 68px block with two jobs. The plane and
/// the rule live here so a tab with a header and a tab without one are the same shape at the same
/// weight, and the stack above the rows reads as one stack.
fn resource_header_band(cx: &App) -> Div {
    let content = design::role::surface_content(cx);
    h_flex()
        .flex_none()
        .w_full()
        .h(design::size::RESOURCE_HEADER)
        .bg(content)
        .border_b_1()
        .border_color(chrome_hairline(content, design::colors(cx).border))
}

/// The button box whose *derived* glyph is [`design::icon::IN_TOOLBAR`].
///
/// gpui-kit sets a mark's size from the button's own box (`size * 0.75`) and overwrites whatever
/// the caller named on the mark, so the box is the only lever there is and every icon control that
/// wants the same lane has to ask for the same box. Stated once here because the bar's two button
/// shapes disagree otherwise: at `size::ICON_BUTTON` the six icon actions drew an eighteen-pixel
/// glyph and at `size::CONTROL` the two context fields drew a twenty-one-pixel one, on a band the
/// design calls forty pixels tall, either side of the error chip's mark at the lane's sixteen.
/// Those boxes are still the two *targets*; they are no longer the two glyph sizes.
fn toolbar_glyph_box() -> Pixels {
    Pixels::from(f32::from(design::icon::IN_TOOLBAR) / 0.75)
}

/// The trigger both title-bar selectors are built from.
///
/// One builder, because the two controls are one sentence — *where am I* — and a reader who finds
/// one of them well made and the other not has to work out which is the real one. It states the
/// shape the bar's other controls already use, and both things that were wrong with them are
/// answered here rather than at each call site.
///
/// **The mark leads.** It identifies what is being switched, so it belongs in front of the name,
/// the way it does in every other control in the product. It used to trail, which put a folder
/// glyph *after* `All namespaces` and read as a second unnamed control parked beside the selector
/// rather than as part of it. It is also always present, so the name starts at the same x whether
/// or not the cluster is in trouble — a mark that appears and disappears is a lane that moves.
///
/// **The control is `size::CONTROL` tall, not the height of the band.** A 40px trigger in a 40px
/// bar has no edge of its own: its hover plane filled the whole band, so the name floated in the
/// middle of a chrome strip with two glyphs reacting to the pointer and nothing else on the band
/// reacting to anything. A 28px control in a 40px bar has a shape, and the shape is what tells a
/// reader it is clickable before the pointer arrives.
///
/// The caret is the affordance that says the control opens a list, and its open state is the
/// Shell's own flag reaching this button: `Popover::trigger` answers
/// `selected(selected || is_open)`, so the control stays visibly pressed for as long as the list
/// is on screen. Hover cannot explain a relationship between a trigger and its surface.
///
/// **The glyph's size is the component's, and the ink is the caller's.** `Button` derives its
/// glyph from the box (`size * 0.75`) and exposes no knob that separates the two, so the mark is
/// drawn from [`toolbar_glyph_box`] whatever size it names — which is how the two selectors and
/// the six icon actions come to share [`design::icon::IN_TOOLBAR`] instead of the twenty-one and
/// eighteen pixels their two different boxes produced. What a caller does own is the ink, and it
/// has to be stated on the mark: the button's own text colour reaches the glyph only in its
/// resting state, and a glyph that changes ink on hover reads as a different glyph.
/// The chord that opens a top-bar switcher, drawn from the keymap the action runs from.
///
/// **Both switchers carry a chord and neither said so.** The bindings were there
/// (`secondary-shift-c` / `secondary-shift-m`, and a dozen more in every preset) and
/// the two most-used controls in the window named only what they currently hold —
/// "Switch Context: prod · Live". A shortcut nobody can discover is not a
/// shortcut, and these two are how every session starts: the first thing a person
/// does is pick a context and a namespace.
///
/// So the chord is shown on the control, not hidden behind a hover, and it is read
/// from the keymap rather than written here — a preset that rebinds it must not
/// find a second, stale spelling on screen.
fn switcher_chord(action: &str, cx: &App) -> Option<gpui_kit::Keystroke> {
    crate::keymap::binding_for_context(action, "Shell", cx)
        .and_then(|chord| gpui_kit::Keystroke::parse(&chord).ok())
}

/// The same chord as the tooltip spells it, so the hover text and the keycap drawn
/// on the control can never disagree about which one it is.
fn with_chord(sentence: String, action: &str, cx: &App) -> String {
    match switcher_chord(action, cx) {
        Some(chord) => format!("{sentence}  {chord}"),
        None => sentence,
    }
}

fn top_bar_selector_button(
    id: &'static str,
    label: impl Into<SharedString>,
    mark: Icon,
    tooltip: impl Into<SharedString>,
    chord: Option<gpui_kit::Keystroke>,
) -> Button {
    let label: SharedString = label.into();
    Button::new(id)
        .icon(mark)
        // The name rides in as a child rather than through `Button::label`, because a button with
        // a custom `Size` takes its label's size from `button_text_size`, and that arm resolves
        // `Size::Size(_)` to gpui-kit's `text_base()` — 16px, a step above `text::TITLE` and three
        // above `text::BODY`. `content_style` is the only place `Button` exposes that choice and it
        // is `pub(crate)`, so a named button cannot be put back on the scale from outside the kit.
        // Measured on the title bar, `kind-k8s-gpui-3n` and `All namespaces` wore an ascender-to-
        // descender ink of 31 device pixels against 22 for the tab titles beside them, which are
        // `text::BODY`: the chrome's most-said name was a step louder than the view it names.
        .child(
            div()
                .min_w_0()
                .whitespace_nowrap()
                .text_ellipsis()
                // `text::LABEL`, not `text::BODY`.
                //
                // This is a VALUE IN A CONTROL on a 40px chrome band, not body
                // copy: at `BODY` it was the largest and brightest text in the
                // whole title bar, louder than the tab titles it sits above, so a
                // toolbar read as a sentence. It stepped down once already - the
                // component resolved it through `button_text_size` to
                // `text_base()`, 16px, three steps above `BODY` - and this is the
                // second step, onto the token the scale reserves for a label.
                //
                // The eyebrow beside it stays `CAPTION`, one step smaller, and the
                // two do not collapse into each other: the eyebrow is uppercase,
                // tracked and tertiary, the value is sentence case and
                // secondary. Size is not the only thing separating them and it
                // does not have to carry it alone.
                .text_size(design::text::LABEL)
                .line_height(design::text::LABEL_LINE_HEIGHT)
                .child(label.clone()),
        )
        // The chord, ON the control, between the name and the caret.
        //
        // **Both switchers were bound and neither said so.** Every preset carries
        // `secondary-shift-c` and `secondary-shift-m`, and these two are how a
        // session starts - pick a context, pick a namespace - so a reader who has
        // to find the shortcut in a file to learn it is being asked to do something
        // no other control in the window asks of them. Showing it here is cheaper
        // than the alternative and it is read from the keymap, so a preset that
        // rebinds the chord shows the new one.
        //
        // It sits inside the name's flex lane rather than after it, so a long
        // context name truncates BEFORE the keycap goes, never over it: the one
        // thing here that is not the current value must survive it.
        .when_some(chord, |this, chord| {
            this.child(
                div()
                    .flex_none()
                    .pl(design::space::SM)
                    .child(gpui_kit::component::kbd::Kbd::new(chord)),
            )
        })
        // The name the trigger is announced by, now that the name is not the button's label.
        .accessibility_label(label)
        .dropdown_caret(true)
        .ghost()
        // The box is the glyph, so it asks for the lane; the height is stated
        // beside it, because a 28px control in a 40px band is what gives this
        // row an edge of its own without the two answers becoming one number.
        .with_size(Size::Size(toolbar_glyph_box()))
        .h(design::size::CONTROL)
        .tooltip(tooltip)
        .role(RoleOverride::Presentational)
        .tab_stop(false)
}

/// The appearance every icon-only action in the bar is built from.
///
/// One builder for six controls, because they are one group: the same square target, the same
/// gap around them, and one place where the glyph's state ink is decided. They were each named a
/// `size::CONTROL` target — 28px inside a 40px band — which is a form field's height on a band
/// that is asking for a toolbar, and it is why five bare glyphs read as five unrelated controls
/// rather than as one row of actions. `size::ICON_BUTTON` is the token that means "a square icon
/// target", so the group is square, evenly pitched, and its members are the same size as each
/// other.
///
/// The ink goes on the mark, not on the button. gpui-kit's `Button` re-applies its variant's
/// hover and selected foregrounds over whatever the caller sets, so an ink stated on the control
/// describes the resting state and nothing else; on the mark it holds in every state, which is
/// what `design::icon` asks for — hover is a wash on the control, never a new ink on the glyph.
fn top_bar_icon_button(id: &'static str, icon: IconName, ink: gpui_kit::Hsla) -> Button {
    Button::new(id)
        .ghost()
        .icon(Icon::new(icon).text_color(ink))
        // The square a pointer aims at is `size::ICON_BUTTON` and is stated as
        // both axes: the box above is smaller, because gpui-kit reads the glyph
        // off it and `design::icon::IN_TOOLBAR` is what this lane's glyphs are.
        // Leaving the height to the box is how the target and the mark drifted
        // apart in the first place.
        .with_size(Size::Size(toolbar_glyph_box()))
        .w(design::size::ICON_BUTTON)
        .h(design::size::ICON_BUTTON)
}

/// The eyebrow that says which of the two context controls holds what.
///
/// `text::CAPTION` semibold in the tertiary ink, uppercased — the one treatment `UI-SPEC.md` §2.3
/// reserves for section heads and eyebrows, and nothing else in the bar wears it, so the two
/// words are the only thing on the band that is *about* the band.
///
/// They are here because a server glyph and a folder glyph a hairline apart in a 40px band do
/// not tell a reader which field is which. Both say "a place to point at", and the names inside
/// them are a context name and a namespace name — two different grammars with no shared cue.
/// The words cost 120px of a bar that has a flexible spacer in the middle, and they turn the
/// pair from two identical chevron buttons into two labelled fields.
///
/// The tertiary ink is kept, deliberately, while the mark beside it moves up to the resting
/// glyph ink: this is the one word on the band that is *about* the band rather than part of a
/// control, and `DESIGN.md` §1.4's placeholder tier is what a tracked uppercase label belongs
/// to. Two quiet things side by side is what stopped the pair reading as one field, so the step
/// between the label and the glyph it names is the point.
fn top_bar_eyebrow(word: &'static str, cx: &App) -> Label {
    Label::new(word.to_uppercase())
        .text_size(design::text::CAPTION)
        .line_height(design::text::CAPTION_LINE_HEIGHT)
        .font_weight(design::text::SEMIBOLD)
        // Stated, not inherited: gpui-kit's `Label` re-applies `theme().foreground` after it takes
        // the caller's style, so an ink set on the row around it never reaches the glyph.
        .text_color(design::role::fg_tertiary(cx))
}

///     /// A toolbar control the shell's own focus handle names.
///
/// gpui-kit's `Button` owns a focus handle keyed by its own id, so a shell handle that no
/// element tracks belongs to nothing: Tab walks past the control, and the shell's focus-lost
/// fallback takes the focus back the moment anything asks for that handle. The wrapper is the
/// control — the focus stop, the role, the name and the ring — and the button inside it is the
/// appearance and the pointer target, so one action is one control in the accessibility tree
/// and a click lands where the focus would.
///
/// The component's own ring is turned off for the same reason the ring is drawn here: it is a
/// 3px stroke at half alpha outside the border, which is the loudest thing a quiet chrome band
/// can carry, and the shell already has one answer to "what does focus look like".
fn top_bar_action(
    control: Stateful<Div>,
    focus: &FocusHandle,
    role: Role,
    label: impl Into<SharedString>,
    cx: &App,
    button: Button,
) -> AnyElement {
    control
        .track_focus(focus)
        .role(role)
        .aria_label(label)
        .rounded_md()
        .focus_visible(common::focus_ring(cx))
        .child(
            button
                .focus_ring(false)
                .role(RoleOverride::Presentational)
                .tab_stop(false),
        )
        .into_any_element()
}

fn namespace_picker_options(state: &NamespaceState, current: &SharedString) -> Vec<PickerOption> {
    let names = match state {
        NamespaceState::Ready(names) => names.as_slice(),
        NamespaceState::Loading | NamespaceState::Failed(_) => &[],
    };
    let mut all = PickerOption::new(super::ALL_NAMESPACES, super::ALL_NAMESPACES);
    all.icon = Some(design::glyph::object::namespace());
    all.current = current.as_ref() == super::ALL_NAMESPACES;
    all.debug_selector = Some(format!("MENU_ITEM-{}", super::ALL_NAMESPACES).into());
    match state {
        NamespaceState::Loading => {
            all.status = Some(design::Severity::Muted);
            all.status_label = Some("Loading…".into());
        }
        NamespaceState::Ready(names) if names.is_empty() => {
            all.status = Some(design::Severity::Muted);
            all.status_label = Some("No namespaces found".into());
        }
        NamespaceState::Failed(_) => {
            all.status = Some(design::Severity::Error);
            all.status_label = Some("Load failed".into());
        }
        NamespaceState::Ready(_) => {}
    }
    let mut options = vec![all];
    options.extend(names.iter().map(|name| {
        let mut option = PickerOption::new(name.clone(), name.clone());
        option.icon = Some(design::glyph::object::namespace());
        option.current = name.as_ref() == current.as_ref();
        option.debug_selector = Some(format!("MENU_ITEM-{name}").into());
        option
    }));
    disambiguate_options(&mut options);
    options
}

/// Render a tree status with an icon and message.
/// One recovery sentence for every "this resource is not listed" state.
fn resource_catalog_missing_message(title: &str) -> String {
    format!("{title} is not in the resource catalog. Refresh resources or show Pods.")
}

/// Strip-level summary of the failed kubeconfig sources, one line per source in `detail`.
fn kubeconfig_warning_message(detail: &str) -> String {
    let source_count = detail.lines().count();
    if source_count == 1 {
        "1 kubeconfig source failed to load. Fix the source, then reload kubeconfigs.".to_owned()
    } else {
        format!(
            "{source_count} kubeconfig sources failed to load. Fix the sources, then reload kubeconfigs."
        )
    }
}

/// The sidebar header's Kind count.
///
/// A catalog fact, not a UI state. It used to count the Kind rows the tree was showing, so the
/// number moved every time a reader expanded an API group while the `All API groups` count beside
/// it stayed put, and a filter that matched nothing took it to zero.
fn tree_kind_label(tree: &super::tree::ResourceTree) -> String {
    design::format::count_with_noun(tree.kind_count(), "Kind", "Kinds")
}

/// A status the tree reports instead of rows: what happened, in one line.
///
/// gpui-kit's `Marker` is the component for a full-width status row. It owns the spinner, the
/// content slot, and the `Role::Status` binding, so the surface no longer chooses a glyph size or
/// decides which of its two states turns.
fn tree_status(
    id: &'static str,
    icon: IconName,
    message: impl Into<SharedString>,
    cx: &App,
) -> AnyElement {
    let message = message.into();
    let colors = design::colors(cx);
    let loading = icon == IconName::LoaderCircle;
    let marker = Marker::new()
        .loading(loading)
        .items_center()
        .py(design::space::LG)
        .when(!loading, |this| {
            this.icon(
                // The marker glyph box is a fixed 16px, and the shared icon size is larger, so
                // the slot takes the icon's own measure rather than clipping the sweep.
                MarkerIcon::new().size_6().child(
                    Icon::new(icon)
                        .with_size(Size::Size(design::size::ICON_LARGE))
                        .text_color(colors.text_muted),
                ),
            )
        })
        .content(
            MarkerContent::new()
                .text_size(design::text::CAPTION)
                .line_height(design::text::CAPTION_LINE_HEIGHT)
                .text(message.clone()),
        );
    // `Marker` binds its role to its own element id, and the name has to travel with the padding
    // the surface adds, so the wrapper is the status node and the marker is the row inside it.
    div()
        .id(id)
        .w_full()
        .px(design::space::MD)
        .role(Role::Status)
        .aria_label(message)
        .child(marker)
        .into_any_element()
}

fn update_phase_icon(phase: UpdatePhase) -> IconName {
    match phase {
        UpdatePhase::Idle => IconName::Info,
        UpdatePhase::UpToDate => IconName::Check,
        UpdatePhase::Checking | UpdatePhase::Downloading => IconName::LoaderCircle,
        UpdatePhase::Ready => IconName::Check,
        UpdatePhase::Restarting => IconName::RotateCw,
        UpdatePhase::Failed => IconName::TriangleAlert,
        UpdatePhase::Unsupported => IconName::Dash,
    }
}

fn update_phase_severity(phase: UpdatePhase) -> design::Severity {
    match phase {
        UpdatePhase::Ready | UpdatePhase::UpToDate => design::Severity::Success,
        UpdatePhase::Failed => design::Severity::Error,
        UpdatePhase::Downloading | UpdatePhase::Checking | UpdatePhase::Restarting => {
            design::Severity::Info
        }
        UpdatePhase::Idle | UpdatePhase::Unsupported => design::Severity::Muted,
    }
}

/// The lane the sidebar's scrollbar occupies, reserved so nothing paints under it.
///
/// gpui-kit's own `Scrollbar::width()` is the source, restated because the width is
/// not reachable from a layout builder and a bare 16 here would be a fourth place
/// to keep in step with a component that owns it.
const SIDEBAR_SCROLL_GUTTER: gpui_kit::Pixels = px(16.);

impl Shell {
    pub(super) fn sync_helm_cluster(&self, view: &Entity<HelmView>, cx: &mut Context<Self>) {
        let context = self.helm_services().context;
        view.update(cx, |view, _| view.set_cluster_name(context));
    }

    fn center_owns_connection_failure(&self) -> bool {
        self.active_resource_view().is_some()
            || self.tabs.get(self.active_tab).is_some_and(|tab| {
                matches!(
                    tab.content,
                    TabContent::Resource
                        | TabContent::Overview
                        | TabContent::Helm
                        | TabContent::Preview
                )
            })
    }

    fn connection_failure_message(&self) -> &'static str {
        match self.connection {
            ConnectionState::Failed(_) => "Connection unavailable",
            ConnectionState::Reconnecting(_) => "Reconnecting…",
            ConnectionState::Connecting | ConnectionState::Live => "Connected",
        }
    }

    fn connection_failure_icon(&self) -> IconName {
        match self.connection {
            ConnectionState::Reconnecting(_) => IconName::LoaderCircle,
            ConnectionState::Failed(_) => IconName::TriangleAlert,
            ConnectionState::Connecting | ConnectionState::Live => IconName::Server,
        }
    }

    pub(super) fn connection_failure_is_primary(&self, cx: &App) -> bool {
        if !self.center_owns_connection_failure() {
            return false;
        }
        match self.connection {
            ConnectionState::Failed(_) => true,
            ConnectionState::Reconnecting(_) => {
                self.active_resource_view()
                    .is_some_and(|view| matches!(view.read(cx).status(cx), TableStatus::Failed(_)))
                    || matches!(&self.catalog_state, CatalogState::Failed(_))
            }
            ConnectionState::Connecting | ConnectionState::Live => false,
        }
    }

    fn render_connection_failure(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.connection_failure_is_primary(cx) {
            return None;
        }
        // `UI-SPEC.md` §4.15 wants the reason here, where the reader is looking, and named in
        // terms of the step that failed. The transport's own words are that sentence; the old
        // second line was not, because its subject — "check access to the context" — is not an
        // action anybody can take. So the reason replaces it rather than joining it.
        let reason = connection_failure_reason(&self.connection);
        let title = self.connection_failure_message();
        let role = if matches!(self.connection, ConnectionState::Failed(_)) {
            Role::Alert
        } else {
            Role::Status
        };
        // A reconnect is a real wait, so it gets the shared spinner: it rotates unless the
        // reader asked for less motion, which the inline rotation here never did.
        //
        // `UI-SPEC.md` §4.13 asks for a 24px `fg.tertiary` glyph and says not to colour it, and
        // D5 is why: this surface is the only thing on screen while the cluster is away, so a
        // 24px amber glyph is not a signal, it is the window's background colour. The state is
        // already in the shape (a spinner for a wait, a cross for a failure) and in the title.
        let icon_color = design::role::fg_tertiary(cx);
        let icon = if matches!(self.connection, ConnectionState::Reconnecting(_)) {
            crate::panels::common::spinner(
                self.connection_failure_icon(),
                icon_color,
                Size::Size(design::size::ICON_LARGE),
            )
        } else {
            Icon::new(self.connection_failure_icon())
                .with_size(Size::Size(design::size::ICON_LARGE))
                .text_color(icon_color)
                .into_any_element()
        };
        let reload = div()
            .debug_selector(|| "connection-reload-kubeconfigs".to_owned())
            .child(common::labelled(
                Button::new("connection-reload-kubeconfigs")
                    // Ghost, not outline: `PROMPT.md` §2 rule 5 keeps a visible 1px border to
                    // inputs, overlays, and the rule between two panels. An outlined button in
                    // the middle of an otherwise borderless surface is the fourth place.
                    .ghost()
                    .with_size(Size::Size(design::size::CONTROL))
                    .tab_index(0isize)
                    .tooltip("Reload kubeconfigs")
                    .on_click(cx.listener(|shell, _, window, cx| {
                        shell.dispatch(super::ReloadKubeconfigs, window, cx);
                    })),
                "Reload kubeconfigs",
            ));
        Some(
            v_flex()
                .id("connection-failure")
                .size_full()
                .role(role)
                .aria_label(title)
                .aria_description(reason.clone())
                .items_center()
                .justify_center()
                .gap(design::space::SM)
                .debug_selector(|| "connection-failure".to_owned())
                .child(icon)
                // §4.13: the title is `fg.primary` at 15/600, not a status colour.
                .child(section_text(title).text_color(design::role::fg_primary(cx)))
                .child(text(reason).text_color(design::role::fg_secondary(cx)))
                .child(
                    h_flex()
                        .id("connection-failure-actions")
                        .gap(design::space::SM)
                        .items_center()
                        .child(
                            div()
                                .debug_selector(|| "connection-retry".to_owned())
                                .child(
                                    common::labelled(
                                        Button::new("connection-retry")
                                            .primary()
                                            .with_size(Size::Size(design::size::CONTROL))
                                            .w(px(ACTION_BUTTON_WIDTH))
                                            .tab_index(0isize)
                                            .tooltip("Retry connection")
                                            .on_click(cx.listener(|shell, _, _, cx| {
                                                shell.retry_connection(cx)
                                            })),
                                        "Retry",
                                    )
                                    // The two words differ on purpose: a screen reader is told
                                    // what is being retried, because a panel that says only
                                    // "Retry" leaves the connection failure unnamed in the one
                                    // surface where the reason sits above it.
                                    .accessibility_label("Retry connection"),
                                ),
                        )
                        .child(reload),
                )
                .into_any_element(),
        )
    }

    fn render_update_progress(&self, progress: Option<f32>, cx: &Context<Self>) -> AnyElement {
        let colors = design::colors(cx);
        match progress {
            Some(progress) => {
                let progress = progress.clamp(0.0, 1.0);
                let percent = (progress * 100.0).round() as u32;
                h_flex()
                    .id("update-progress-determinate")
                    .w(design::size::UPDATE_PROGRESS)
                    .gap(design::space::SM)
                    .items_center()
                    .role(Role::Status)
                    .aria_label(format!("Download Progress: {percent}%"))
                    .child(
                        Progress::new("update-progress-bar")
                            // `Progress` reads a percentage, not a fraction.
                            .value(progress * 100.0)
                            .color(colors.text_accent)
                            .w(design::size::UPDATE_PROGRESS)
                            .accessibility_label(format!("Download Progress: {percent}%")),
                    )
                    .child(text_small(format!("{percent}%")).text_color(colors.text_muted))
                    .into_any_element()
            }
            None => h_flex()
                .id("update-progress-indeterminate")
                .gap(design::space::XS)
                .items_center()
                .role(Role::Status)
                .aria_label("Downloading update")
                .child(crate::panels::common::spinner(
                    IconName::LoaderCircle,
                    design::status_colors(cx).info,
                    Size::Size(design::size::HIT_MIN),
                ))
                .child(text_small("Downloading…").text_color(colors.text_muted))
                .into_any_element(),
        }
    }

    pub(super) fn render_update_strip(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.update_state.shows_strip() {
            return None;
        }
        let state = &self.update_state;
        let phase = state.phase;
        // The unsupported notice is a popover, not a strip. It renders in the
        // overlay layer so it stays above the status backdrop.
        if phase == UpdatePhase::Unsupported {
            return None;
        }
        let colors = design::colors(cx);
        let severity = update_phase_severity(phase);
        let status = state.status_text();
        let icon = if matches!(
            phase,
            UpdatePhase::Checking | UpdatePhase::Downloading | UpdatePhase::Restarting
        ) {
            crate::panels::common::spinner(
                update_phase_icon(phase),
                severity.marker(cx),
                Size::Size(design::size::HIT_MIN),
            )
        } else {
            Icon::new(update_phase_icon(phase))
                .xsmall()
                .text_color(severity.marker(cx))
                .into_any_element()
        };
        let progress = (phase == UpdatePhase::Downloading)
            .then(|| self.render_update_progress(state.progress, cx));
        let action: Option<AnyElement> = match phase {
            UpdatePhase::Ready => Some(
                common::labelled(
                    Button::new("update-restart")
                        .primary()
                        .with_size(Size::Size(design::size::CONTROL))
                        .w(px(ACTION_BUTTON_WIDTH))
                        .tab_index(1isize)
                        .disabled(self.update_actions.is_none())
                        .tooltip("Restart to update")
                        .on_click(cx.listener(|shell, _, _, cx| shell.run_update_restart(cx))),
                    "Restart to update",
                )
                .into_any_element(),
            ),
            UpdatePhase::Failed => Some(
                common::labelled(
                    Button::new("update-retry")
                        .primary()
                        .with_size(Size::Size(design::size::CONTROL))
                        .w(px(ACTION_BUTTON_WIDTH))
                        .tab_index(0isize)
                        .disabled(self.update_actions.is_none())
                        .tooltip("Retry update")
                        .on_click(cx.listener(|shell, _, _, cx| shell.run_update_retry(cx))),
                    "Retry",
                )
                // The two words differ on purpose: the visible word is the action and the
                // announced one is what is being retried, so a reader who cannot see the strip
                // still hears that this is the update and not the connection.
                .accessibility_label("Retry update")
                .into_any_element(),
            ),
            _ => None,
        };
        let role = if phase == UpdatePhase::Failed {
            Role::Alert
        } else {
            Role::Status
        };
        let failure = design::status_colors(cx).error;
        let mut strip =
            h_flex()
                .id("update-strip")
                .w_full()
                .flex_none()
                .h(design::size::UPDATE_STRIP)
                .px(design::space::SM)
                .py(design::space::XS)
                .gap(design::space::SM)
                .items_center()
                .tab_group()
                .tab_index(-2isize)
                .bg(colors.panel_background.alpha(1.0))
                .border_b_1()
                .border_color(colors.border)
                .role(role)
                .aria_label(status.clone())
                .when_some(state.error.clone(), |this, error| {
                    this.aria_description(error)
                })
                .child(icon)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap(design::space::XS)
                        .child(text(status).truncate().text_color(
                            if phase == UpdatePhase::Failed {
                                failure
                            } else {
                                colors.text
                            },
                        ))
                        .when_some(progress, |this, progress| this.child(progress)),
                )
                .when_some(action, |this, action| this.child(action));
        if let Some(error) = state.error.clone() {
            strip.interactivity().tooltip(common::hover_hint(error));
        }
        Some(strip.into_any_element())
    }

    /// Render the "updates are unavailable" card below the table's first row.
    ///
    /// It is a popover, not a strip, so it renders in the overlay layer where a press outside
    /// it dismisses it and the status backdrop cannot cover it. It opens once per run, and only
    /// for a build that can update itself, so neither a debug launch nor a repeated status
    /// covers the main table. The one-shot claim is spent in `set_update_state`, because render
    /// runs again for every frame and a claim spent here would drop the card on the second one.
    pub(super) fn render_update_overlay(
        &self,
        window: &mut Window,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !self.update_notice_opened {
            return None;
        }
        let state = &self.update_state;
        let phase = state.phase;
        let status = state.status_text();
        let colors = design::colors(cx);
        let severity = update_phase_severity(phase);
        let icon = Icon::new(update_phase_icon(phase))
            .xsmall()
            .text_color(severity.marker(cx));
        let viewport = window.viewport_size();
        // A narrow window must still show a usable popover, so the width keeps a
        // control-width floor instead of collapsing to nothing.
        let width = super::dialog_width(f32::from(viewport.width)).min(DIALOG_WIDTH);
        let top = update_overlay_top();
        // The card only gets the height left below its top, so it still fits at the smallest
        // supported window.
        let max_height =
            (f32::from(viewport.height) - f32::from(top) - f32::from(design::space::SM))
                .max(f32::from(design::size::ROW));

        let overlay_focused = self.update_overlay_focus.is_focused(window);
        // The overlay keeps focus itself and moves a ring between the actions, so
        // the actions are reported as the active descendant rather than tab stops.
        let action_active = |index: usize| overlay_focused && self.update_overlay_action == index;
        let header = h_flex()
            .flex_none()
            .w_full()
            .h(design::size::UPDATE_STRIP)
            .px(design::space::SM)
            .gap(design::space::SM)
            .items_center()
            .child(icon)
            .child(text(status.clone()).truncate().text_color(colors.text));
        let actions = v_flex()
            .id("update-strip-actions")
            .debug_selector(|| "update-strip-actions".to_owned())
            .w_full()
            .flex_none()
            .py(design::space::XS)
            .role(Role::ListBox)
            .aria_label("Update Actions")
            .child(
                div()
                    .id("update-action-check-slot")
                    .w_full()
                    .debug_selector(|| "update-action-check".to_owned())
                    .role(Role::ListBoxOption)
                    .accessibility_id("update-action-check")
                    .aria_position_in_set(1)
                    .aria_size_of_set(UPDATE_OVERLAY_ACTIONS)
                    .aria_selected(action_active(0))
                    .when(action_active(0), |this| this.aria_active_descendant())
                    .when(action_active(0), |this| {
                        this.border_1().border_color(colors.border_focused)
                    })
                    .child(
                        common::labelled(
                            Button::new("update-action-check")
                                .icon(IconName::RefreshCw)
                                .ghost()
                                .with_size(Size::Size(design::size::CONTROL))
                                .w(px(width))
                                .tooltip("Check for updates")
                                .disabled(self.update_actions.is_none())
                                .on_click(
                                    cx.listener(|shell, _, _, cx| shell.run_update_check(cx)),
                                ),
                            "Check for updates",
                        )
                        .into_any_element(),
                    ),
            )
            .child(
                div()
                    .id("update-action-retry-slot")
                    .w_full()
                    .debug_selector(|| "update-action-retry".to_owned())
                    .role(Role::ListBoxOption)
                    .accessibility_id("update-action-retry")
                    .aria_position_in_set(2)
                    .aria_size_of_set(UPDATE_OVERLAY_ACTIONS)
                    .aria_selected(action_active(1))
                    .when(action_active(1), |this| this.aria_active_descendant())
                    .when(action_active(1), |this| {
                        this.border_1().border_color(colors.border_focused)
                    })
                    .child(
                        common::labelled(
                            Button::new("update-action-retry")
                                .icon(IconName::RefreshCw)
                                .ghost()
                                .with_size(Size::Size(design::size::CONTROL))
                                .w(px(width))
                                .tooltip("Retry update")
                                .disabled(self.update_actions.is_none())
                                .on_click(
                                    cx.listener(|shell, _, _, cx| shell.run_update_retry(cx)),
                                ),
                            "Retry update",
                        )
                        .into_any_element(),
                    ),
            )
            .child(
                div()
                    .id("update-action-restart-slot")
                    .w_full()
                    .debug_selector(|| "update-action-restart".to_owned())
                    .role(Role::ListBoxOption)
                    .accessibility_id("update-action-restart")
                    .aria_position_in_set(3)
                    .aria_size_of_set(UPDATE_OVERLAY_ACTIONS)
                    .aria_selected(action_active(2))
                    .when(action_active(2), |this| this.aria_active_descendant())
                    .when(action_active(2), |this| {
                        this.border_1().border_color(colors.border_focused)
                    })
                    .child(
                        common::labelled(
                            Button::new("update-action-restart")
                                .icon(IconName::RotateCw)
                                .ghost()
                                .with_size(Size::Size(design::size::CONTROL))
                                .w(px(width))
                                .tooltip("Restart to update")
                                .disabled(self.update_actions.is_none())
                                .on_click(
                                    cx.listener(|shell, _, _, cx| shell.run_update_restart(cx)),
                                ),
                            "Restart to update",
                        )
                        .into_any_element(),
                    ),
            );
        let mut overlay = v_flex()
            .id("update-strip-overlay")
            .debug_selector(|| "update-strip-overlay".to_owned())
            .absolute()
            .top(top)
            .right(design::space::SM)
            .w(px(width))
            .max_h(px(max_height))
            .overflow_y_scroll()
            .rounded_lg()
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface_background.alpha(1.0))
            .shadow(cx.theme().shadow_tokens().lg)
            .track_focus(&self.update_overlay_focus)
            .tab_group()
            .tab_index(7isize)
            .focus_visible(|this| this.border_color(colors.border_focused))
            .key_context("UpdateOverlay")
            .aria_keyshortcuts("Escape")
            .on_key_down(cx.listener(|shell, event, window, cx| {
                shell.on_update_overlay_key_down(event, window, cx)
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|shell, _, window, cx| {
                    window.focus(&shell.update_overlay_focus, cx);
                    cx.stop_propagation();
                }),
            )
            .on_mouse_down_out(cx.listener(|shell, _, window, cx| {
                shell.close_update_overlay(window, cx);
            }))
            .role(Role::Dialog)
            .accessibility_id("update-overlay")
            .aria_label(status)
            .when_some(state.error.clone(), |this, error| {
                this.aria_description(error)
            })
            .child(
                v_flex()
                    .id("update-strip")
                    .debug_selector(|| "update-strip".to_owned())
                    .w_full()
                    .flex_none()
                    .child(header)
                    .child(actions),
            );
        if let Some(error) = state.error.clone() {
            overlay.interactivity().tooltip(common::hover_hint(error));
        }
        Some(overlay.into_any_element())
    }

    /// The "some sources could not be read" strip.
    ///
    /// gpui-kit's `Alert` draws the bar: the banner variant is the full-width form, so the strip
    /// is one component call and the app supplies the severity, the sentence, and the recovery.
    pub(super) fn render_kubeconfig_warning(&self) -> Option<AnyElement> {
        let detail = self.kubeconfig_warning.clone()?;
        let message = kubeconfig_warning_message(&detail);
        // `Alert` owns the bar's role, its accessible name, and its accessible description, so
        // the wrapper is only the tab-order placement the shell's own focus routing needs and the
        // selector a test reads the strip through.
        let mut strip = div()
            .debug_selector(|| "kubeconfig-source-warning".to_owned())
            .flex_none()
            .tab_group()
            .tab_index(-3isize)
            .child(
                Alert::warning("kubeconfig-source-warning", message)
                    .banner()
                    .with_size(Size::XSmall)
                    .icon(Icon::new(IconName::TriangleAlert).xsmall()),
            );
        strip.interactivity().tooltip(common::hover_hint(detail));
        Some(strip.into_any_element())
    }

    /// Render the top toolbar.
    ///
    /// Two clusters, one spacer, and both clusters aligned to the same `space::SM` the window's padding
    /// puts on the bar itself — so the first control's glyph and the last control's glyph sit the same
    /// distance from the window edge, which is the only thing that makes a bar read as a bar rather than
    /// as controls floating in a strip.
    ///
    /// The left cluster is *where am I*: the panel switch and the two switches that name the context.
    /// The right cluster is *what can I do here*. Both are `Role::Group` with a name, because a row of
    /// icon buttons with no group node is a row of unlabelled glyphs to an assistive technology — and
    /// the two icon buttons that used to sit on the left with nothing beside them (a bare copy glyph and
    /// a bare caret) are the shape that produces: a control is only legible relative to its neighbours.
    pub(super) fn render_top_bar(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let width = f32::from(window.viewport_size().width);
        let compact = !super::inspector_available(width);
        // One compact step, one number. The bar sheds its *secondary* clusters below
        // [`super::chrome_compact_width`], the same width at which the sidebar becomes the rail and
        // the status bar sheds its optional readouts. Every one of the clusters that go has a
        // palette command and a keymap behind it — `Toggle Inspector`, and the connection state the
        // status bar already prints — so nothing loses its last path.
        let narrow = width < super::chrome_compact_width();
        // The bar's own question is not "has the reader closed the tree" but "could this window
        // ever show it", asked with the flag forced on, because a control that takes the tree's
        // slot has to know the tree is not there.
        let tree_fits = super::responsive_panel_visibility(true, true, width).0;
        let colors = design::colors(cx);
        div()
            .relative()
            .flex_none()
            .w_full()
            .h(design::size::TITLE_BAR)
            .px(design::space::SM)
            .overflow_hidden()
            // `role::surface_chrome`, by name. This read `colors.title_bar_background` — the theme
            // *field* behind the role. The two hold the same value in both shipped themes, so the
            // title bar looked like the sidebar by coincidence rather than by decision, and a theme
            // that moved the chrome plane would have separated them.
            .bg(design::role::surface_chrome(cx).alpha(1.0))
            // The title bar's own bottom edge, and the only rule on it: it spans the window, so it
            // is the single owner of the boundary between chrome and whatever is under it — the
            // rail, the sidebar, the centre and the Inspector all start below this one line.
            //
            // Solved against the plane it lands on, like every other structural rule in the
            // window, so the band stack reads as one stack instead of four bands that happen to
            // share a colour name.
            .border_b_1()
            .border_color(chrome_hairline(
                design::role::surface_chrome(cx),
                colors.border,
            ))
            .child(
                h_flex()
                    .size_full()
                    .min_w(px(0.0))
                    .items_center()
                    .gap(design::space::XS)
                    .tab_group()
                    .tab_index(-4isize)
                    .overflow_hidden()
                    // `UI-SPEC.md` §4.1 puts the cluster and the namespace next to each other on
                    // the left: they are one sentence — where am I — and the reader changes either
                    // of them from the same place. The namespace used to sit on the far side of a
                    // `flex_1` spacer, pressed up against the search icon at the right edge, which
                    // read as a fourth control in the icon cluster and put the scope of every row
                    // on screen 1700px from the context it scopes.
                    .child(
                        h_flex()
                            .id("top-bar-context")
                            .debug_selector(|| "top-bar-context".to_owned())
                            .flex_none()
                            .min_w(px(0.0))
                            .h_full()
                            .gap(design::space::SM)
                            .items_center()
                            .role(Role::Group)
                            .aria_label("Context")
                            .child(if tree_fits {
                                self.render_sidebar_toggle(cx)
                            } else {
                                self.render_resource_switcher(cx)
                            })
                            // The sidebar switch is the tree's slot, not part of the sentence the
                            // other two make, so a rule separates the panel control from the pair
                            // that names the context. It is the same object the actions group is
                            // held off with, so one hairline answers one question in this bar:
                            // "these are different things".
                            .child(toolbar_separator(cx))
                            .child(self.render_cluster_selector(cx))
                            // The pair used to be two identical chevron buttons a `space::SM` apart,
                            // which is the gap between two items in one group and not the gap
                            // between two fields. A rule, plus the eyebrows each field now carries,
                            // is what says they are separate answers to separate questions.
                            .child(toolbar_separator(cx))
                            .child(self.render_namespace_selector(window, cx)),
                    )
                    .child(div().flex_1().min_w(px(0.0)))
                    .child(
                        h_flex()
                            .id("top-bar-actions")
                            .debug_selector(|| "top-bar-actions".to_owned())
                            .flex_none()
                            .h_full()
                            // The actions in one cluster are parts of one control, so they take
                            // `space::XS` between them; the cluster as a whole is held off the
                            // context cluster by a rule rather than by more space, which is what
                            // says "these are different things" without spending a `space::XXL`.
                            .gap(design::space::XS)
                            .items_center()
                            .role(Role::Group)
                            .aria_label("Window actions")
                            .child(toolbar_separator(cx))
                            // The connection point leads the group and the icon actions follow it,
                            // one `space::XS` apart and nothing between them: the chip carries its
                            // own boundary, so a second rule beside it would be the window saying
                            // the same thing twice. The bar's answer to "which of my four clusters
                            // is this" comes before "what can I do here", and the status bar prints
                            // the same state, so the compact band drops the chip rather than
                            // printing one fact twice in two strips 40px apart.
                            .when(!narrow, |this| this.child(self.render_connection_point(cx)))
                            .child(self.render_palette_button(cx))
                            .child(self.render_settings_button(cx))
                            .when(!narrow, |this| {
                                this.child(self.render_inspector_toggle(compact, cx))
                            })
                            .child(self.render_top_bar_notifications(cx)),
                    ),
            )
    }

    /// The fault count and the notification bell, for the top bar's right-hand group.
    ///
    /// `windows.md` asks for critical information and actions to stay off a window's bottom edge,
    /// because a window gets moved. Failures and the notification center therefore live up here,
    /// where the title bar keeps them on screen.
    ///
    /// This bar is permanent, so a count of zero is a permanent report of nothing. `UI-SPEC` §8's
    /// 克制 group asks the interface not to spend attention on what is not happening, and a title
    /// bar that always reads `0 errors · 0 active notifications` spends the most valuable strip in
    /// the window on the absence of two things, in the one place a fault count is allowed to
    /// appear. So each figure is drawn only when it has something to say, and the bell — a
    /// control, not a report — stays either way and carries no number of its own. The
    /// notification center's own header already takes this rule for its count and for its Clear
    /// action, so the two agree about what a zero is worth.
    ///
    /// The two numbers answer two different questions, and severity leads. Errors are what the
    /// cluster did wrong, and this is the one place a fault count appears in the app's chrome.
    /// Notifications are everything the app has to say, and four of the five Settings switches add
    /// an Info confirmation each time they succeed, so the raw total is padded with the reader's
    /// own actions; the bell counts the active incidents, which is the same figure the
    /// notification center puts first and the only one that means something on its own.
    fn render_top_bar_notifications(&self, cx: &Context<Self>) -> AnyElement {
        let colors = design::colors(cx);
        let summary = self.status_summary(cx);
        let errors = design::format::count_with_noun(summary.errors, "error", "errors");
        let active = summary.active_notifications;
        let notifications =
            design::format::count_with_noun(active, "active notification", "active notifications");
        let total =
            design::format::count_with_noun(summary.notifications, "notification", "notifications");
        let open = self.status_panel == super::StatusPanel::Notifications;
        let action = if open {
            "Close Notifications"
        } else {
            "Open Notifications"
        };
        // The name carries the figures the bar is drawing, in the order it draws them, and
        // nothing else. Announced as "Open Notifications: 0 active notifications, 0 errors"
        // it describes a bar holding two readouts — the report this group exists to delete,
        // made to the one reader who cannot see that the bar is empty. The hover adds the
        // history total, which is the notification center's own figure and earns a line
        // only once there is history to reach.
        let mut label = action.to_owned();
        if summary.errors > 0 {
            label.push_str(&format!(": {errors}"));
        }
        if active > 0 {
            label.push_str(&format!(": {notifications}"));
        }
        let hover = if summary.notifications > 0 {
            format!("{label}. {total} in total")
        } else {
            label.clone()
        };
        // A chip that is on screen is a fault that happened, so it has no quiet state left to
        // paint: with nothing to report it is not on the bar at all, and the icon takes the
        // error channel's ink for the one state it can be in.
        let count = (summary.errors > 0).then(|| {
            h_flex()
                .id("top-bar-errors")
                // A selector so a test can prove the two numbers are two groups.
                .debug_selector(|| "top-bar-errors".to_owned())
                .flex_none()
                .gap(design::space::XS)
                .items_center()
                .role(Role::Status)
                .aria_label(errors.clone())
                // The error shape is the health channel's, not the chrome's: the old
                // `IconName::X` was the same glyph as this row's own dismiss button, so
                // "3 errors" and "close this" were one shape.
                .child(
                    Icon::new(design::health_icon(design::Severity::Error))
                        // The bar's row lane beside a count, and not `xsmall()`'s 12px, which put
                        // the one status mark in the band a full step under the bell's glyph.
                        .with_size(Size::Size(design::icon::IN_ROW))
                        .text_color(design::icon::status(cx, design::Severity::Error)),
                )
                .child(text_small(errors).text_color(colors.text_muted))
        });
        let mut bell = top_bar_icon_button(
            "top-bar-notifications",
            IconName::Bell,
            // A control's resting ink, not the count's. The bell is a control and the number
            // beside it is a count, and the step between the two is what stops a toolbar glyph
            // from being read as a third figure on the band. It used to take the error count's
            // colour, which made it an unlabelled second copy of a number printed 20px away —
            // the alarm belongs to the error chip on its left.
            if open {
                design::icon::active(cx)
            } else {
                design::icon::resting(cx)
            },
        )
        .tab_index(7isize)
        // The notification centre is a panel attached to this control, so the control stays
        // visibly open while the panel is on screen. It is the same rule the two switchers
        // follow through `Popover::trigger(trigger.selected(open))`: hover cannot explain a
        // relationship between a trigger and a surface, and the bell's own tooltip is suppressed
        // while the panel is open precisely because the state is on the control instead.
        .toggled(open)
        .on_click(cx.listener(|shell, _: &ClickEvent, window, cx| {
            if shell.status_panel == super::StatusPanel::Notifications {
                shell.close_notifications(window, cx);
            } else {
                shell.open_notifications(window, cx);
            }
        }));
        if !open {
            bell = bell.tooltip(hover);
        }
        let control = top_bar_action(
            div().id("top-bar-notifications-control").flex_none(),
            &self.top_bar_notifications_focus,
            Role::Button,
            label,
            cx,
            bell,
        );
        h_flex()
            .id("top-bar-notifications-group")
            .flex_none()
            .gap(design::space::XS)
            .items_center()
            .when_some(count, |this, count| this.child(count))
            .child(
                h_flex()
                    .id("top-bar-notification-count")
                    .debug_selector(|| "top-bar-notification-count".to_owned())
                    .flex_none()
                    .gap(design::space::XS)
                    .items_center()
                    .child(control)
                    // The number is the live region and the bell is the control, so the count
                    // chip is its own element rather than a wrapper around a button. It is also
                    // the only count in here, so with nothing to count the bell stands alone.
                    .when(active > 0, |this| {
                        this.child(
                            div()
                                .id("top-bar-notification-count-value")
                                .flex_none()
                                .role(Role::Status)
                                .aria_label(notifications.clone())
                                .child(text_small(notifications).text_color(colors.text_muted)),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_sidebar_toggle(&self, cx: &Context<Self>) -> AnyElement {
        // The switch follows the panel, not the tab. Settings is a center tab, so the resource
        // tree is still there to be hidden and shown, and the toggle used to be disabled with a
        // label naming the way out instead.
        //
        // It names the panel rather than the state — "Hide sidebar" / "Show sidebar" — and it keeps
        // `.toggled(open)`, so the icon and the pressed state move together. It is also the tree's
        // slot in the bar in the compact band too: the sidebar collapses to the rail rather than
        // disappearing, so the toggle has a panel to act on at every width this window can be.
        let open = self.sidebar_open;
        top_bar_action(
            div().id("toggle-sidebar-control").flex_none(),
            &self.top_bar_focus,
            Role::Button,
            if open { "Hide sidebar" } else { "Show sidebar" },
            cx,
            top_bar_icon_button(
                "toggle-sidebar",
                if open {
                    IconName::PanelLeftOpen
                } else {
                    IconName::PanelLeftClose
                },
                if open {
                    design::icon::active(cx)
                } else {
                    design::icon::resting(cx)
                },
            )
            .tab_index(0isize)
            .toggled(open)
            .tooltip_with_action(
                if open { "Hide sidebar" } else { "Show sidebar" },
                &ToggleLeftPanel,
                Some("Shell"),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.dispatch(ToggleLeftPanel, window, cx);
            })),
        )
    }

    /// The navigation the resource tree would answer, for a window that cannot hold it.
    ///
    /// The layout draws the tree only when the *client* width clears `MIN_LAYOUT_WIDTH`, and
    /// `MIN_LAYOUT_WIDTH` is `design::size::WINDOW_MIN.0` — the *window* floor `main.rs` hands the
    /// window manager. A decorated window's client area is narrower than the window itself, so a
    /// window sitting at its own minimum arrives at the gate 19px short of it (941px on the
    /// machine this was found on) and the tree is not drawn at all, with a toggle beside it that
    /// reports "open" and changes nothing. The gate belongs to `shell::mod`, and a toggle cannot
    /// be satisfied by any width, so this takes the tree's slot in the bar — the first slot, and
    /// the shell's first tab stop, because it is the same job — and opens the same list of kinds
    /// as a palette, which is the `Choose Resource Kind` action the menu and `⌘⇧K` already name.
    ///
    /// The glyph is the tree's own, so the reader recognises the thing that is standing in for
    /// the panel, and the hover says why it is standing there.
    ///
    /// This is the answer for the narrow band, and it is a *list* rather than a rail on purpose: at
    /// this width a lane of glyphs would be a smaller tree rather than a readable one, and this
    /// band is the one place where a reader most wants to search for a kind by name instead.
    fn render_resource_switcher(&self, cx: &Context<Self>) -> AnyElement {
        let label = format!("{SIDEBAR_NARROW_LABEL}. {SIDEBAR_NARROW_HINT}");
        top_bar_action(
            div()
                .id("top-bar-resources-control")
                .debug_selector(|| "top-bar-resources".to_owned())
                .flex_none(),
            &self.top_bar_focus,
            Role::Button,
            label.clone(),
            cx,
            top_bar_icon_button(
                "top-bar-resources",
                IconName::ListTree,
                design::icon::resting(cx),
            )
            .tab_index(0isize)
            .tooltip_with_action(label, &super::OpenResourceKindSwitcher, Some("Shell"))
            .on_click(cx.listener(|this, _, window, cx| {
                this.dispatch(super::OpenResourceKindSwitcher, window, cx);
            })),
        )
    }

    /// The Settings trigger.
    ///
    /// `toolbars.md` › Actions asks for a symbol over a text label when the symbol is well
    /// recognized, and a gear is. Keeping the label and its shortcut chip would also cost more
    /// width than the cluster name, which is the one item that must never be ambiguous, so the
    /// shortcut stays in the tooltip and the accessible name is unchanged.
    fn render_settings_button(&self, cx: &Context<Self>) -> AnyElement {
        top_bar_action(
            div()
                .id("open-settings-control")
                .debug_selector(|| "open-settings".to_owned())
                .flex_none(),
            &self.top_bar_settings_focus,
            Role::Button,
            "Open settings…",
            cx,
            top_bar_icon_button(
                "open-settings",
                IconName::Settings,
                design::icon::resting(cx),
            )
            .tab_index(5isize)
            .tooltip_with_action("Open settings…", &crate::settings::OpenSettings, None)
            .on_click(cx.listener(|this, _, window, cx| {
                this.dispatch(crate::settings::OpenSettings, window, cx);
            })),
        )
    }

    fn render_inspector_toggle(&self, compact: bool, cx: &Context<Self>) -> AnyElement {
        // Only a window too narrow for the Inspector disables this, which is a fact about the
        // window. Settings is a tab, and the Inspector belongs to the resource views, so it keeps
        // its own switch while Settings is open.
        let open = self.inspector_open;
        let shown = open && !compact;
        let label = if compact {
            super::INSPECTOR_COMPACT_LABEL
        } else if open {
            "Hide Inspector"
        } else {
            "Show Inspector"
        };
        let name = if compact {
            super::INSPECTOR_COMPACT_LABEL
        } else {
            "Toggle Inspector"
        };
        let button = top_bar_icon_button(
            "toggle-inspector",
            if shown {
                IconName::PanelRightOpen
            } else {
                IconName::PanelRightClose
            },
            // A window too narrow for the Inspector cannot show it, so the glyph wears the
            // disabled ink rather than the resting one: the control's own state, not a status.
            if compact {
                design::icon::disabled(cx)
            } else if shown {
                design::icon::active(cx)
            } else {
                design::icon::resting(cx)
            },
        )
        .tab_index(6isize)
        .toggled(shown)
        .on_click(cx.listener(|this, _, window, cx| {
            this.dispatch(ToggleRightPanel, window, cx);
        }));
        let button = if compact {
            // A window too narrow for the Inspector cannot show it, so the hint is
            // the only place the reader learns why.
            button.tooltip(format!("{label}. {}", super::INSPECTOR_WIDTH_HINT))
        } else {
            button.tooltip_with_action(label, &ToggleRightPanel, Some("Shell"))
        };
        top_bar_action(
            div().id("toggle-inspector-control").flex_none(),
            &self.top_bar_inspector_focus,
            Role::Button,
            name,
            cx,
            button.disabled(compact),
        )
    }

    /// The Command Palette trigger.
    ///
    /// `toolbars.md` › Actions asks for a symbol over a text label when the symbol is well
    /// recognized, and the magnifier is the symbol for searching every command. Leaving the
    /// shortcut in the tooltip rather than on the surface also leaves the leading items of the
    /// bar free for the context they describe.
    fn render_palette_button(&self, cx: &Context<Self>) -> AnyElement {
        top_bar_action(
            div()
                .id("command-palette")
                .debug_selector(|| "command-palette".to_owned())
                .flex_none(),
            &self.top_bar_palette_focus,
            Role::Button,
            "Open command palette",
            cx,
            top_bar_icon_button(
                "command-palette",
                IconName::Search,
                design::icon::resting(cx),
            )
            .tab_index(4isize)
            .tooltip_with_action("Open command palette", &ToggleCommandPalette, Some("Shell"))
            .on_click(cx.listener(|this, _, window, cx| {
                this.dispatch(ToggleCommandPalette, window, cx);
            })),
        )
    }

    /// A `Popover` around the picker card, for the two toolbar switchers.
    ///
    /// The card is the popover's content and the open state belongs to the Shell
    /// flag, so the mouse and the keyboard reach the same list and the flag still
    /// answers "is the list open" for the trigger's own selected presentation.
    fn picker_menu(
        &self,
        id: &'static str,
        kind: PickerKind,
        options: Vec<PickerOption>,
        trigger: Button,
    ) -> Popover {
        let shell = self.shell_weak.clone();
        let on_select: PickerSelectHandler = Rc::new(move |value, window, cx| {
            let shell = shell.clone();
            window.defer(cx, move |_, cx| {
                shell
                    .update(cx, |shell, cx| match kind {
                        PickerKind::Cluster => {
                            if let Some(index) = shell
                                .clusters
                                .iter()
                                .position(|name| name.as_ref() == value.as_ref())
                            {
                                shell.switch_cluster(index, cx);
                            }
                        }
                        PickerKind::Namespace => shell.set_namespace(value, cx),
                    })
                    .ok();
            });
        });
        let open = match kind {
            PickerKind::Cluster => self.cluster_menu_open,
            PickerKind::Namespace => self.namespace_menu_open,
        };
        let shell_for_open = self.shell_weak.clone();
        let shell_for_dismiss = self.shell_weak.clone();
        Popover::new(id)
            .anchor(Anchor::BottomLeft)
            // The card draws its own plane, radius and shadow, so the popover must not
            // draw a second set around it. `Popover`'s appearance is `popover_style` plus
            // a `p_3` gutter, and with the card's own `CONTENT_INSET` inside that gutter
            // the switcher measured twenty-four pixels of gutter on all four sides around a
            // square-cornered card — one rounded object inside another, which is what
            // `picker_menu` below spent its comment on and what made the surface read as
            // unfinished. Turning it off leaves the card as the popover's whole surface.
            .appearance(false)
            .open(open)
            .on_open_change(move |open, _, cx| {
                // The callback runs while `Popover` is being rendered, which is inside
                // the Shell's own update lease, so the write is deferred to the end of
                // it rather than leasing Shell a second time.
                let open = *open;
                let shell = shell_for_open.clone();
                cx.defer(move |cx| {
                    if let Some(shell) = shell.upgrade() {
                        shell.update(cx, |shell, cx| shell.set_picker_menu_open(kind, open, cx));
                    }
                });
            })
            .trigger(trigger.selected(open))
            .content(move |_, window, cx| {
                // Choosing an option closes the list, and the list's open state is
                // the Shell's flag, so the picker clears it rather than trying to
                // take the popover down itself.
                let shell_for_dismiss = shell_for_dismiss.clone();
                let (picker, fresh) = cx.update_default_global::<PickerCards, _>(|cards, cx| {
                    let Some(picker) = cards.slot_mut(kind).clone() else {
                        let config = PickerConfig {
                            kind,
                            options: options.clone(),
                            on_select: on_select.clone(),
                            on_dismiss: Rc::new(move |_, cx| {
                                if let Some(shell) = shell_for_dismiss.upgrade() {
                                    shell.update(cx, |shell, cx| {
                                        shell.set_picker_menu_open(kind, false, cx)
                                    });
                                }
                            }),
                        };
                        let picker = cx.new(|cx| SearchablePicker::new(config, window, cx));
                        *cards.slot_mut(kind) = Some(picker.clone());
                        return (picker, true);
                    };
                    // A card that is already open keeps its search, so a list that
                    // has not changed leaves the query and the selection alone.
                    picker.update(cx, |picker, cx| picker.set_options(options.clone(), cx));
                    (picker, false)
                });
                // Opening the list is a request to search it, so the field takes
                // focus on the next frame, once the popover has laid out. Only the
                // frame that built the card asks for it: focusing a card that
                // already holds focus is a frame request with nothing behind it,
                // and the popover renders again the moment the focus lands.
                if fresh {
                    let focus_picker = picker.clone();
                    window.on_next_frame(move |window, cx| {
                        let focus = focus_picker.read(cx).input_focus_handle(cx);
                        window.focus(&focus, cx);
                    });
                }
                // The card IS the popover's body, and it is placed there directly.
                //
                // It used to be wrapped in a `PopupMenu`, which is itself a popover
                // surface: `Popover.content` paints one rounded plane and its own
                // padding, and `PopupMenu::build` paints a SECOND one with a second
                // padding inside the first. The card then painted a third, unrounded,
                // plane inside those - so the switcher, the surface the reader opens
                // most, was showing three frames of different sizes and radii nested
                // inside each other. It read as a card inside a card inside a card,
                // which is the exact thing the design guide forbids and the exact
                // thing that made this surface look unfinished.
                //
                // The wrapper is removed rather than restyled because it earned
                // nothing here: the card is one element, not a list of menu items, so
                // the menu's rows, separators and keyboard walking have nothing to
                // act on. Dismissal, focus and the open flag belong to the `Popover`
                // above, which is what raised `on_open_change` in the first place.
                //
                // `Popover::appearance(false)` above is the other half of the same fix:
                // the card now draws the plane, the radius and the shadow, so the plane
                // gpui-kit would have drawn around it is a fourth frame rather than the
                // one the card is made of.
                picker.clone().into_any_element()
            })
    }

    fn render_cluster_selector(&self, cx: &Context<Self>) -> impl IntoElement {
        let clusters = self.clusters.clone();
        let active = self.active_cluster;
        let current = clusters
            .get(active)
            .cloned()
            .unwrap_or_else(|| SharedString::from("No Context"));
        let status = self.connection.label();
        // `UI-SPEC.md` §4.1: the cluster name and a caret. The connection used to be appended to
        // it as `name · Live`, which put the same fact in the title bar and in the status bar
        // 1000px away and left the context name one word narrower than the space reserved for it.
        let label = current.clone();
        let tooltip = with_chord(
            match self.connection.detail() {
                Some(reason) => format!("Switch Context: {current} · {status}. {reason}"),
                None => format!("Switch Context: {current} · {status}"),
            },
            "k8s_shell::OpenContextSwitcher",
            cx,
        );
        let mut options = match self.session.as_ref().and_then(ClusterSession::registry) {
            Some(registry) => registry
                .clusters()
                .iter()
                .map(|cluster| {
                    let health = cluster.health_snapshot();
                    let (status_severity, status_label, detail) = match &health {
                        Health::Ready => (design::Severity::Success, "Live", None),
                        Health::NotReady(reason) => (
                            design::Severity::Warning,
                            "Not ready",
                            Some(SharedString::from(reason.clone())),
                        ),
                        Health::Unknown => (design::Severity::Muted, "Not checked", None),
                    };
                    let mut option = PickerOption::new(cluster.name(), cluster.name());
                    option.source = registry
                        .context_source(cluster.name())
                        .map(|path| path.to_string_lossy().into_owned().into());
                    option.status = Some(status_severity);
                    option.status_label = Some(status_label.into());
                    option.detail = detail;
                    option.icon = Some(IconName::Server);
                    option
                })
                .chain(registry.context_errors().iter().map(|error| {
                    let reason = SharedString::from(error.source.to_string());
                    let mut option = PickerOption::new(&error.context, &error.context);
                    option.source = registry
                        .context_source(&error.context)
                        .map(|path| path.to_string_lossy().into_owned().into())
                        .or(Some(reason.clone()));
                    option.detail = Some(reason);
                    option.status = Some(design::Severity::Error);
                    option.status_label = Some("Unavailable".into());
                    option.icon = Some(IconName::Server);
                    option
                }))
                .collect::<Vec<_>>(),
            None => Vec::new(),
        };
        if options.is_empty() {
            options = clusters
                .iter()
                .map(|name| {
                    let mut option = PickerOption::new(name.clone(), name.clone());
                    option.icon = Some(IconName::Server);
                    option
                })
                .collect();
        }
        for (index, option) in options.iter_mut().enumerate() {
            option.current = index == active;
            option.debug_selector = Some(format!("cluster-option-{index}").into());
        }
        disambiguate_options(&mut options);
        // The cluster name is the one piece of context that must never be ambiguous in a
        // multi-cluster tool, so the control sizes to the name and the cap only bounds a
        // pathological kubeconfig entry. The budget is the width the resource tree gives a name,
        // so the same identity string reads the same in both places. `layout.md` > Visual
        // hierarchy puts the leading, most important item where it gets the room; the toolbar
        // earns that room by spending it on chrome the reader already knows.
        let max_width = f32::from(design::size::SIDEBAR_MAX);
        // A reachable context is the state with nothing to show, so it shows no shape: a muted
        // dash in front of the context name reads as a typographic dash opening the title bar, and
        // D5 gives the amber channel to contexts that are actually in trouble.
        //
        // The mark is always drawn, and it is always in the *control's* state, never the
        // cluster's. It used to fade to `fg_tertiary` while the cluster was healthy, on the
        // theory that a healthy cluster should be quiet — which put the one glyph that says
        // *which context am I in* a step under every control beside it, so the context control
        // read as disabled next to a bright sidebar toggle. An ink is a state, not a mood: the
        // server glyph identifies the kind of thing, and severity has its own two places in this
        // bar — the connection dot and the status word on its right.
        let mark = Icon::new(IconName::Server).text_color(if self.cluster_menu_open {
            design::icon::active(cx)
        } else {
            design::icon::resting(cx)
        });
        let menu = self.picker_menu(
            "cluster-menu",
            PickerKind::Cluster,
            options,
            // The trigger is the popover's, so the control `top_bar_action` would have
            // wrapped is this box, and the button inside it is presentational.
            top_bar_selector_button(
                "cluster-selector",
                label,
                mark,
                tooltip.clone(),
                switcher_chord("k8s_shell::OpenContextSwitcher", cx),
            )
            .tab_index(1isize),
        );
        // The eyebrow and the trigger are one row, and the row is the control: the trigger is the
        // pointer target and this box is the focus stop, the role and the name.
        h_flex()
            .id("cluster-selector-control")
            .flex_none()
            .min_w(px(0.0))
            .max_w(px(max_width))
            .flex_shrink_1()
            .items_center()
            .gap(design::space::XS)
            .track_focus(&self.top_bar_cluster_focus)
            .role(Role::ComboBox)
            .aria_label(format!("Context: {current}, {status}"))
            .aria_description(tooltip)
            // The two switchers were the only controls in the bar that tracked a shell focus
            // handle and drew no ring for it, so Tab stopped on them and nothing said so. The
            // ring hangs on the box that owns the handle, which is the same answer
            // `top_bar_action` gives the six controls beside them.
            .rounded_md()
            .focus_visible(common::focus_ring(cx))
            .debug_selector(|| "cluster-selector".to_owned())
            .child(top_bar_eyebrow("Context", cx))
            .child(menu)
            .into_any_element()
    }

    /// `UI-SPEC.md` §4.1's connection point: a 6px dot and the state word, on the right.
    ///
    /// The title bar is the one strip a reader sees when the window is not focused, and §4.1
    /// keeps the connection there for that reason: an Alt-Tab switcher and a dock preview both
    /// show it, so "which of my four clusters is this" is answerable without focusing the window.
    ///
    /// **It is a chip, and it is the only status *word* in the bar.** It used to be a bare `Live`
    /// in the mark's own ink: the dot and the word both read `severity.marker(cx)`, which is a
    /// token solved for a 6px dot, so an 11px word wore the ink of the thing it was describing and
    /// a reader could not tell whether the grey meant *reaching the API server* or *nothing at
    /// all*. The mark takes `role::status_for` and the word takes `role::status_word_for`, which
    /// is the whole reason the product keeps two.
    ///
    /// `Live` is still drawn in the quiet ink, and that is D5 rather than an oversight: a healthy
    /// cluster is the absence of a status channel, and a green dot in the corner of every window
    /// is a status light that never changes, which is wallpaper. A reconnecting or failed
    /// connection is the case that earns the colour, and it earns all of it.
    ///
    /// The boundary is a hairline rather than a fill. A fill would have to be either the plane
    /// above it — a 1px bar on a title bar reads as an input — or the plane below it, and
    /// `role::surface_inset` is the editor's darkest step, which on this band is a hole rather
    /// than a pill. A hairline gives the group a silhouette at the bar's own weight and leaves
    /// the state legible in greyscale.
    fn render_connection_point(&self, cx: &Context<Self>) -> AnyElement {
        let severity = self.connection.severity();
        let word = self.connection.label();
        let reason = self.connection.detail().unwrap_or_default();
        let cluster = self
            .clusters
            .get(self.active_cluster)
            .map_or("No Context", |name| name.as_ref());
        let hint = if reason.is_empty() {
            format!("Connected to {cluster}")
        } else {
            format!("Connected to {cluster}. {reason}")
        };
        let plane = design::role::surface_chrome(cx);
        let mut chip = h_flex()
            .id("top-bar-connection")
            .debug_selector(|| "top-bar-connection".to_owned())
            .flex_none()
            // The same square target as the icon actions beside it, so the group reads as one row
            // of controls rather than as a pill floating among five unrelated glyphs.
            .h(design::size::ICON_BUTTON)
            .px(design::space::XS)
            .gap(design::space::XS)
            .items_center()
            .rounded(design::radius::SM)
            .border_1()
            .border_color(chrome_hairline(plane, design::colors(cx).border))
            .role(Role::Status)
            .aria_label(format!("Connection: {word}"))
            .child(
                div()
                    .flex_none()
                    .size(design::size::STATUS_DOT)
                    .rounded_full()
                    .bg(design::role::status_for(severity, cx)),
            )
            .child(
                Label::new(word)
                    // The line height is stated with the size: gpui-kit's `Label` hard-codes a
                    // 1.25rem line box, so a caption that carried only its size sat 20px tall in
                    // a 24px chip and the word drifted off the dot's centre line.
                    .text_size(design::text::CAPTION)
                    .line_height(design::text::CAPTION_LINE_HEIGHT)
                    .font_weight(design::text::MEDIUM)
                    .text_color(design::role::status_word_for(severity, cx)),
            );
        chip.interactivity().tooltip(common::hover_hint(hint));
        chip.into_any_element()
    }

    fn render_namespace_selector(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        let current = self.namespace.clone();
        let options = namespace_picker_options(&self.namespace_state, &current);
        let state_label = match &self.namespace_state {
            NamespaceState::Loading => "Loading namespaces…",
            NamespaceState::Ready(names) if names.is_empty() => "No namespaces found",
            NamespaceState::Ready(_) => "Loaded",
            NamespaceState::Failed(_) => "Namespace list unavailable",
        };
        let tooltip = with_chord(
            match &self.namespace_state {
                NamespaceState::Failed(_) => {
                    "Namespace List Unavailable. Refresh the list, then try again.".to_owned()
                }
                _ => format!("Switch Namespace: {current}"),
            },
            "k8s_shell::OpenNamespaceSwitcher",
            cx,
        );
        let aria_label = format!("Namespace: {current}, {state_label}");
        // The budget the name gets, and it is the BUDGET, not the control's width:
        // `SIDEBAR_MIN` plus a step was a number chosen before the control carried
        // an eyebrow, a mark, a caret and a keycap, and every one of those took its
        // share out of the name. `All namespaces` arrived as `All na…`, which is
        // the one value on this control a reader cannot guess - it is the literal
        // string in every kubeconfig, and the one whose truncation hides the fact
        // that no namespace is selected at all.
        //
        // So the budget now names what it has to hold: the widest namespace name a
        // cluster actually has, the eyebrow, the mark, the keycap and the caret.
        // The bar has a flexible spacer in the middle, so this costs nothing until
        // the window is too narrow to have it, which is the breakpoint below.
        let max_width =
            if f32::from(window.viewport_size().width) < super::INSPECTOR_LAYOUT_BREAKPOINT {
                f32::from(design::size::SIDEBAR_MIN) + f32::from(design::space::LG)
            } else {
                // What the control has to HOLD, on the normal window: the widest
                // namespace a cluster actually has, plus the eyebrow, the mark, the
                // keycap and the caret that share the row with it.
                //
                // `SIDEBAR_MIN + a step` was a number chosen when the control carried
                // a mark and a caret, and it was already tight; the eyebrow and the
                // keycap then took their shares out of the NAME, so `All namespaces`
                // arrived as `All na…` - which is the one value here a reader cannot
                // guess, because it is the literal string in every kubeconfig and
                // its truncation hides that no namespace is selected at all.
                //
                // The context control beside it already takes `SIDEBAR_MAX`, and the
                // two together fit: the bar has a flexible spacer between them and
                // its own cluster, so this only costs room on the narrow breakpoint,
                // where the name truncates - which is the correct place for it.
                f32::from(design::size::SIDEBAR_MAX)
            };
        let failed = matches!(self.namespace_state, NamespaceState::Failed(_));
        // The folder names the kind of thing and takes the control's own state ink, on the same
        // argument as the cluster's server mark: a healthy namespace faded to `fg_tertiary` beside
        // an `fg_secondary` name, which is the shape a disabled control makes. A failed namespace
        // list is a different statement, so the mark becomes the health channel's shape and
        // carries that channel's mark ink — severity on a mark, never on the glyph that names the
        // kind of thing.
        let icon = Icon::new(if failed {
            IconName::TriangleAlert
        } else {
            IconName::Folder
        })
        .text_color(if failed {
            design::icon::status(cx, design::Severity::Warning)
        } else if self.namespace_menu_open {
            design::icon::active(cx)
        } else {
            design::icon::resting(cx)
        });
        let menu = self.picker_menu(
            "namespace-menu",
            PickerKind::Namespace,
            options,
            // The trigger is the popover's, so the control `top_bar_action` would have
            // wrapped is this box, and the button inside it is presentational.
            //
            // The cap is a `max_w` the button can shrink inside rather than a `w` that forces
            // it wide. gpui-kit gives a button with a dropdown caret `justify_between` inside
            // its content row, so a button forced wider than its own content spends the slack
            // on the two gaps and opens both of them, while the same button at its natural
            // width gets only `gap_2`. Measured on the title bar, the context field's mark sat
            // 8px from its name and 11px from its caret; the namespace field's sat 18px and
            // 21px — the same builder, the same lane, laid out by two rules, which is what
            // made the pair read as two kinds of field rather than as one sentence.
            top_bar_selector_button(
                "namespace-selector",
                current,
                icon,
                tooltip.clone(),
                switcher_chord("k8s_shell::OpenNamespaceSwitcher", cx),
            )
            .max_w(px(max_width))
            .min_w(px(0.0))
            .flex_shrink_1()
            .tab_index(3isize),
        );
        // See `render_cluster_selector`: the eyebrow and the trigger are one row, and the row is
        // the control.
        h_flex()
            .id("namespace-selector-control")
            .flex_none()
            .min_w(px(0.0))
            .max_w(px(max_width))
            .flex_shrink_1()
            .items_center()
            .gap(design::space::XS)
            .track_focus(&self.top_bar_namespace_focus)
            .role(Role::ComboBox)
            .aria_label(aria_label)
            .aria_description(tooltip)
            // See `render_cluster_selector`: both switchers take the one focus treatment the
            // window has, and both wear it on the box that owns the handle.
            .rounded_md()
            .focus_visible(common::focus_ring(cx))
            .debug_selector(|| "namespace-selector".to_owned())
            .child(top_bar_eyebrow("Namespace", cx))
            .child(menu)
            .into_any_element()
    }

    /// Render the resource tree and its load states.
    pub(super) fn render_tree(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let colors = design::colors(cx);
        let focused = self.tree_focus_handle.is_focused(window);
        let rows = self.visible_tree_rows();
        let kind_label = tree_kind_label(&self.tree);
        let surface = design::role::surface_chrome(cx);
        // The API-group container is the boundary between the pinned core Kinds
        // and the rest of the catalog, so it is the one row that gets a rule.
        let api_groups_row = self.clusters.get(self.active_cluster).and_then(|cluster| {
            let container = super::tree::api_groups_id(cluster);
            rows.iter().position(|row| row.id == container)
        });
        let body: AnyElement = match &self.catalog_state {
            CatalogState::Ready => {
                let mut children: Vec<AnyElement> = Vec::with_capacity(rows.len() * 2);
                for (index, row) in rows.into_iter().enumerate() {
                    if api_groups_row == Some(index) {
                        // A container child that carries no icon of its own is
                        // the only reliable group boundary, so the rule is the
                        // structure and the container row stays quiet. Solved
                        // against the sidebar's own plane, like every other rule in
                        // the window: this one separates the pinned core Kinds from
                        // the 69 API groups below, and it has to be findable at a
                        // glance for that to be worth drawing at all.
                        children.push(
                            div()
                                .w_full()
                                .h(design::border::LINE)
                                .child(
                                    Separator::horizontal()
                                        .w_full()
                                        .color(chrome_hairline(surface, colors.border_variant)),
                                )
                                .into_any_element(),
                        );
                    }
                    children.push(
                        self.render_tree_row(index, row, focused, cx)
                            .into_any_element(),
                    );
                }
                div().children(children).into_any_element()
            }
            CatalogState::Loading => tree_status(
                "tree-loading",
                IconName::LoaderCircle,
                "Loading resources…",
                cx,
            ),
            CatalogState::Failed(_)
                if matches!(
                    &self.connection,
                    ConnectionState::Failed(_) | ConnectionState::Reconnecting(_)
                ) =>
            {
                tree_status(
                    "tree-connection-status",
                    self.connection_failure_icon(),
                    self.connection_failure_message(),
                    cx,
                )
            }
            CatalogState::Failed(reason) => self.render_tree_failure(reason, cx),
        };
        v_flex()
            .id("resource-tree")
            .role(Role::Tree)
            .accessibility_id("resource-tree")
            .aria_label("Kubernetes Resources")
            // The keys this tree actually answers, so the row menu, the disclosure and the two
            // ends of the list are all discoverable without trying them. `Home` and `End` are here
            // because the Dock strip and the Inspector tab strip both take them; a list that has
            // them in one place and not the next is the finding, not the feature.
            .aria_keyshortcuts(TREE_KEYSHORTCUTS)
            .flex_none()
            .w_full()
            .h_full()
            .min_w(px(0.0))
            // No box of its own. This used to draw a 1px frame on all four sides *and* sit inside
            // `render_tree_with_filter`, which draws the sidebar's plane — so the sidebar had a
            // border inside it, and on the trailing edge it had two: this frame and the shell's own
            // splitter. Two owners for one boundary is how a 1px rule ends up looking like a 2px
            // one on one side of the window and like nothing at all on the other.
            //
            // The one boundary this region does own is *inside* it: the rule under the group head.
            // That is a structural divider between two bands of the same panel, not a seam between
            // panels, so it is a different thing and it stays.
            .bg(surface.alpha(1.0))
            .track_focus(&self.tree_focus_handle)
            .key_context("Tree")
            .tab_group()
            .tab_index(0isize)
            .focus_visible(|style| style.border_color(colors.border_focused))
            .on_key_down(cx.listener(Self::on_tree_key_down))
            .child(
                h_flex()
                    .flex_none()
                    // `size::GROUP_HEAD`, not `size::ROW`. The token exists for exactly this — "a
                    // sticky group heading, the sidebar's regions, a list's sections" — and a heading
                    // sized by a table row's number is a heading whose rhythm belongs to something
                    // else. Four pixels of air above and below the head is also what separates it
                    // from the filter field's band, which is the only reason that seam stopped
                    // reading as one block.
                    .h(super::tree::SIDEBAR_GROUP_HEAD_HEIGHT)
                    // The head's *label* shares the rows' label column. A row spends its inset,
                    // its depth indent, a disclosure lane, a [`SIDEBAR_MARK_LANE`] and two
                    // [`SIDEBAR_LANE_GAP`]s before its text starts, and the head spent only the
                    // inset and its own mark — so "Resources" sat 24px left of every label in the
                    // column it heads, which is the reason the head and the rows never read as one
                    // list. The lane is reserved here rather than dropped, so the head keeps a mark
                    // of its own *and* lines up.
                    .px(super::tree::SIDEBAR_INSET)
                    .pl(super::tree::SIDEBAR_INSET
                        + super::tree::SIDEBAR_DISCLOSURE_LANE
                        + super::tree::SIDEBAR_LANE_GAP)
                    // The same lane gap the rows spend between their own fixed lanes: one number
                    // for the gap inside a sidebar band, so a head and the row under it cannot
                    // disagree about it.
                    .gap(super::tree::SIDEBAR_LANE_GAP)
                    .items_center()
                    // The rule under the head is solved against the plane it lands on, like every
                    // other structural rule in the window: `colors.border_variant` is solved for
                    // the table, and on the sidebar it could sit under the floor and leave the
                    // head welded to the first row.
                    .border_b_1()
                    .border_color(chrome_hairline(surface, colors.border_variant))
                    // The head's mark and its rows' marks share one lane and one optical size, so
                    // the head's label starts on the same vertical line as the column it heads.
                    .child(
                        Icon::new(IconName::ListTree)
                            .with_size(Size::Size(super::tree::SIDEBAR_MARK_LANE))
                            .flex_none()
                            .text_color(design::role::fg_tertiary(cx)),
                    )
                    // The group label and the `69 Kinds` count share one type step, which is what
                    // puts them on one baseline. They were `TITLE` 15px and `MICRO` 10px in a
                    // centre-aligned row, so their centres coincided and their baselines sat 2.5px
                    // apart — a visible drift on the one line that has to read as one line. Flex
                    // baseline alignment cannot fix it: Taffy derives a child's baseline from its
                    // height when the measure function reports none, so `items_baseline()` on text
                    // is bottom alignment wearing a baseline's name. The fix is structural — one
                    // line box for both — and the count stays quiet through ink, not through size.
                    //
                    // `text::LABEL` and not `text::TITLE`. At 15px semibold the head was the
                    // loudest line in the window's left third and outranked the resource header,
                    // which is the one band whose whole job is to name the object being looked at.
                    // The sidebar's head is a section label; the scale gives that role `LABEL`.
                    .child(
                        Label::new("Resources")
                            .text_size(design::text::LABEL)
                            .line_height(design::text::LABEL_LINE_HEIGHT)
                            .font_weight(design::text::MEDIUM)
                            .text_color(design::role::fg_secondary(cx)),
                    )
                    .child(div().flex_1().min_w(px(0.0)))
                    .when_some(self.catalog_stale, |this, stale| {
                        // Mark an expired cache as stale.
                        let severity = if stale {
                            design::Severity::Warning
                        } else {
                            design::Severity::Muted
                        };
                        let label = if stale { "Stale cache" } else { "Cached" };
                        let mut chip = h_flex()
                            .id("tree-cache")
                            .gap(design::space::XS)
                            .items_center();
                        chip.interactivity().tooltip(common::hover_hint(
                            "Showing cached resources while the list refreshes in the background.",
                        ));
                        this.child(
                            chip.child(Badge::new().dot().xsmall().color(severity.marker(cx)))
                                .child(
                                    Label::new(label)
                                        .text_size(design::text::LABEL)
                                        .line_height(design::text::LABEL_LINE_HEIGHT)
                                        .text_color(design::role::fg_tertiary(cx)),
                                ),
                        )
                    })
                    // The count rides the same line box as the label, and it is quiet because it is
                    // tertiary ink at regular weight rather than because it is smaller. It takes the
                    // same tabular figures the table's own number columns take, so a count that
                    // grows a digit does not reflow the head beside it.
                    .child(
                        div()
                            .flex_none()
                            .font_features(crate::settings::data_typography(cx).features)
                            .child(
                                Label::new(kind_label)
                                    .text_size(design::text::LABEL)
                                    .line_height(design::text::LABEL_LINE_HEIGHT)
                                    .text_color(design::role::fg_tertiary(cx)),
                            ),
                    ),
            )
            .child(
                // gpui-kit's `Scrollbar` owns the thumb, its track, and its idle
                // fade, so the tree carries no scroll arithmetic of its own. The
                // scrollbar layer is absolutely positioned against this box, so
                // the box has to be its containing block.
                div()
                    .id("resource-tree-scroll")
                    .relative()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .track_scroll(&self.tree_scroll)
                    .py(design::space::XS)
                    // The scrollbar's lane, RESERVED.
                    //
                    // gpui-kit draws the scrollbar as an absolutely-positioned
                    // overlay and renders it after the rows, so the thumb is on top
                    // and a row's wash cannot hide it outright. That is not the same
                    // as the scrollbar having a place: the hover and selection
                    // washes still ran to the scroller's trailing edge underneath
                    // the track, so the thumb sat on a stripe of row colour and the
                    // panel read as though the list extended past its own frame.
                    //
                    // Reserving the lane is the same answer the rest of this
                    // product already gives every other trailing element - the
                    // sidebar's count lane, the status bar's forward lane, a table's
                    // trailing numeric lane - and it is the only one that costs
                    // nothing when the list is short enough not to scroll.
                    .pr(SIDEBAR_SCROLL_GUTTER)
                    .vertical_scrollbar(&self.tree_scroll)
                    .child(body),
            )
    }

    /// Render the resource load failure and retry action.
    fn render_tree_failure(&self, reason: &str, cx: &Context<Self>) -> AnyElement {
        let colors = design::colors(cx);
        let mut panel = v_flex()
            .id("tree-failure")
            .debug_selector(|| "tree-failure".to_owned())
            .role(Role::Alert)
            .aria_label("Resource Load Failed")
            .aria_description(reason.to_owned())
            .w_full()
            .items_center()
            .gap(design::space::SM)
            .px(design::space::MD)
            .py(design::space::LG);
        panel
            .interactivity()
            .tooltip(common::hover_hint(reason.to_owned()));
        panel
            .child(
                Icon::new(IconName::TriangleAlert)
                    .small()
                    .text_color(design::status_colors(cx).error),
            )
            .child(text("Resource Load Failed"))
            .child(
                text_small(
                    "Load the resource tree. If loading fails, check the context connection.",
                )
                .text_color(colors.text_muted),
            )
            .child(
                common::labelled(
                    Button::new("tree-retry")
                        .primary()
                        .with_size(Size::Size(design::size::CONTROL))
                        .w(px(ACTION_BUTTON_WIDTH))
                        .tab_index(0isize)
                        .tooltip("Retry Loading Resources")
                        .on_click(cx.listener(|this, _, _, cx| this.retry_catalog(cx))),
                    "Retry",
                )
                // The two words differ on purpose: the panel above this control names the
                // failure as the *resource tree*, and the announced name carries that over
                // for a reader who is not looking at the panel.
                .accessibility_label("Retry Loading Resources"),
            )
            .into_any_element()
    }

    ///
    /// The row is a plain flex line rather than a `ListItem`. `ListItem` puts
    /// every child inside a `w_full` box nested in a `justify_between` row, so
    /// the icon block and the label were laid out as two independent items and
    /// the icon drifted to a constant x, landing on top of the label on every
    /// row; it also adds `py_1`, which made each row 8px taller than the 24px
    /// `UI-SPEC` §4.5 fixes and pushed every label onto the one below.
    ///
    /// The wrapper carries the ARIA tree item, the selection wash and the focus
    /// ring. The focus ring is what has to account for the indent, because a ring
    /// drawn around an inner box lands under the centre panel once the indent has
    /// pushed that box right.
    ///
    /// Three lanes run left to right at a fixed width — the depth indent, the disclosure triangle and
    /// the kind mark — so a row's label starts at the same x whether or not it has a triangle and
    /// whether or not it is selected. A lane that moved with its contents would put every label at a
    /// different x, which is the one thing a 71-row column cannot afford.
    fn render_tree_row(
        &self,
        index: usize,
        row: TreeRow,
        tree_focused: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        // The plane every wash on this row is read against. It is named, not inherited: the
        // sidebar paints `role::surface_chrome`, and the shared row tokens are solved against the
        // table's `role::surface_raised`, which is a perceptible step away. `design.rs` says so
        // itself about the pickers — "using it there put the selection in the same bind the
        // sidebar rail was in: legible on paper, far too quiet on the surface it actually lands
        // on" — so the selection goes through the `_on` form and the hover through `state`.
        let surface = design::role::surface_chrome(cx);
        let expandable = row.expandable();
        // The click handler owns a clone of the row, because the closure takes it by value and the
        // row is what the handler is about. The hint is a string rather than a second clone: it is
        // read once here and moved into the two consumers that want it, and `TreeRow::hint` is
        // where the model's own disambiguation rule now lives.
        let click_row = row.clone();
        let row_hint = row.hint();
        let TreeRow {
            id,
            label,
            detail,
            depth,
            kind,
            expanded,
            pos_in_set,
            set_size,
            ..
        } = row;
        let selected = self
            .selected_node
            .as_ref()
            .is_some_and(|selection| selection.id == id);
        let cursor = tree_focused && self.tree_cursor == Some(index);
        // Containers and Kinds share one icon language: a folder marks a group
        // that opens, and a Kind carries the glyph of its own kind. Overview is
        // neither, and it used to borrow the cluster's server glyph, so the two
        // rows that open the app and the rows that list machines were the same
        // picture; the command palette has called the overview `Screen` for a
        // while, so the sidebar uses the app's own glyph for the same task.
        //
        // Every mark in this column is the shared Lucide family at
        // `size::KIND_ICON`, in one ink. A Kind's own bespoke duotone asset is
        // what the *title* slots use — at 16px, in the accent, on a raised plane —
        // and at fourteen pixels on chrome that same asset rasterises as a
        // halftone or as stacked rules depending on the shape, which is four
        // optical treatments in one column. `tree::sidebar_kind_mark` keeps the
        // per-kind identity inside the one family; `design::kind_icon` is still
        // the fallback for the kinds with no member of their own.
        let icon = match kind {
            // The toolbar names the context, so the cluster row itself is filtered out of the
            // sidebar. The arm still answers for the tree's own vocabulary, and a server is the
            // cluster: `kind_icon` covers resources, and its fallback is a file.
            TreeRowKind::Cluster => IconName::Server,
            TreeRowKind::Overview => IconName::Monitor,
            // ONE shape for the group, at both states. The chevron beside it is
            // the disclosure and it already says open or closed; a folder that
            // also swapped said the same thing twice, in two shapes.
            TreeRowKind::Group => design::glyph::object::group(),
            TreeRowKind::Kind => click_row
                .resource_kind
                .as_deref()
                .map_or(IconName::File, super::tree::sidebar_kind_mark),
        };
        // One ink for the whole column, and never a third colour "to make it lively". Colour on
        // this column would encode something the row does not know — a kind's health is not in
        // `TreeRow`, and the twelve bespoke marks are one hue precisely so that a reader cannot
        // read a status into a shape.
        //
        // What the column *does* encode is hierarchy, and it used to encode none of it: every one
        // of the 71 rows drew `fg.primary`, so the 69 API groups and the Kinds under them were one
        // wall of identical weight and a reader had to read all of them to find `Deployments`.
        // Three steps instead of one, so the wall becomes a set:
        //
        // - the selected row steps up, whatever it is;
        // - a *leaf* — a Kind, and the pinned `Overview` row, which is also a destination rather
        //   than a heading — is `fg_primary`, the ink for the identity of a thing. There is no
        //   bolder ink, so nothing in the column can out-shout it. `Overview` is grouped with the
        //   Kinds deliberately: it opens the dashboard, and a row you click should not read as a
        //   folder.
        // - a *container* — the `All API groups` row and the 69 API groups under it — is
        //   `fg_secondary` and semibold. A container is a heading for a set, and weight is what
        //   separates a heading from the items it names in greyscale.
        let leaf = matches!(kind, TreeRowKind::Kind | TreeRowKind::Overview);
        let emphasis = if selected {
            design::text::SEMIBOLD
        } else if leaf {
            design::text::REGULAR
        } else {
            design::text::MEDIUM
        };
        let mark_ink = if selected {
            design::role::fg_primary(cx)
        } else if leaf {
            design::role::fg_secondary(cx)
        } else {
            design::role::fg_tertiary(cx)
        };
        let label_ink = if selected || leaf {
            design::role::fg_primary(cx)
        } else {
            design::role::fg_secondary(cx)
        };
        let detail_color = if selected {
            design::role::fg_secondary(cx)
        } else {
            design::role::fg_tertiary(cx)
        };
        // The indent is padding on the row, not a margin on the list item: a
        // margin would push a full-width row past the sidebar by the indent.
        //
        // The step is `tree::SIDEBAR_DEPTH_STEP`, the constant the sidebar's own model was sized
        // against, so the label budget in `tree.rs` and the indent on screen are the same number.
        // A three-level sidebar indented at 12 pushed its deepest labels 24px right of the group
        // head, and the two rows the design cares about most — `Validating Admission Policies` and
        // `Validating Admission Policy Bindings` under `admissionregistration.k8s.io` — were
        // already cut to the same `Validating Admission P…` at 236px. Eight buys those labels eight
        // more pixels at the level where the truncation happens, and the hierarchy still reads:
        // the depth step is half the label's own cap height and the disclosure lane sits between
        // the levels, so no two labels in the column collide.
        let indent = f32::from(super::tree::SIDEBAR_DEPTH_STEP) * f32::from(depth);
        // Every row reserves the disclosure column, and a leaf leaves it empty.
        let disclosure_label = if expanded {
            format!("Collapse {label}")
        } else {
            format!("Expand {label}")
        };
        let disclosure_id = id.clone();
        // The disclosure control's own plane, for the same reason the row names one: a control
        // that takes its hover from whatever it happens to be sitting on changes appearance when a
        // container moves. It is the same value as the row's — the control is on the row.
        let disclosure_surface = surface;
        let disclosure_control = div()
            .id(("tree-disclosure", index))
            .debug_selector(move || format!("tree-disclosure-{index}"))
            .size_full()
            .flex()
            .justify_center()
            .items_center()
            .rounded(design::radius::XS)
            // No hand cursor: `UI-SPEC` §9.3 and `PROMPT.md` §3 both
            // list it. A native disclosure keeps the arrow.
            .role(Role::Button)
            // The tree is one Tab stop with a roving cursor, so the triangle stays
            // out of the Tab order; Arrow keys, Enter and Space already reach it
            // through the row.
            .tab_index(-1isize)
            .aria_label(disclosure_label.clone())
            // The row below is also clickable, and it toggles the same row, so the
            // control has to keep its own click to itself or one press toggles twice.
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                cx.stop_propagation();
                if event.click_count() > 1 {
                    return;
                }
                this.toggle_row(disclosure_id.clone(), cx);
                cx.notify();
            }))
            // The control has a hover and a press of its own. It is 20px of the 24px row, so it is
            // the first thing a pointer lands on when a reader reaches for a group, and it answered
            // with nothing — the row's own hover ran under it, which reads as a dead strip.
            .hover(|this| this.bg(design::state::hover(cx, disclosure_surface)))
            .active(|this| this.bg(design::state::press(cx, disclosure_surface)))
            // The control names what it does to the row it belongs to, so a pointer resting on 20
            // pixels of glyph says so. It sits outside the Tab order and outside the row's own
            // name, so this is the one place the verb "Expand" is available at all.
            .tooltip(common::hover_hint(disclosure_label))
            // `disclosure-controls.md` asks the control to point inward while the
            // content is hidden and down while it is shown, so the shape carries the
            // state as well as the ink.
            .child(
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                // `design::icon::NAV` and not `Icon::xsmall()`'s 12px: this is a
                // mark in the sidebar's navigation column and the column has one
                // mark size. The top bar made the same call on its error chip —
                // a raw component preset is a third lane, and a 12px triangle
                // beside a 14px folder reads as a smaller kind of thing rather
                // than as a disclosure. The *ink* stays incidental, which is what
                // `design::icon::incidental` lists a disclosure triangle under.
                .with_size(Size::Size(design::icon::NAV))
                .text_color(design::role::fg_tertiary(cx)),
            );
        let disclosure = h_flex()
            .flex_none()
            .w(super::tree::SIDEBAR_DISCLOSURE_LANE)
            .h(super::tree::SIDEBAR_ROW_HEIGHT)
            .justify_center()
            .items_center()
            .when(expandable, |this| this.child(disclosure_control));
        div()
            .id(("tree-row", index))
            .w_full()
            .pl(px(indent))
            .relative()
            .h(super::tree::SIDEBAR_ROW_HEIGHT)
            .role(Role::TreeItem)
            .accessibility_id(format!("tree-row-{id}"))
            .aria_level(usize::from(depth) + 1)
            .aria_position_in_set(pos_in_set)
            .aria_size_of_set(set_size)
            .aria_selected(selected)
            .when(expandable, |this| this.aria_expanded(expanded))
            // Hover and selection live here too, and so do the click handlers:
            // the row is one hit target from the leading rail to the sidebar
            // edge, so a click on the label, the icon or the empty space past
            // the count is the same gesture. The inner line below only draws.
            //
            // Three states, solved against the sidebar's own plane and in increasing weight, so
            // the order survives with every colour removed: hover is a wash of the bar's own ink,
            // the keyboard cursor is the same wash one step further in the *accent* channel, and
            // the selection is the accent wash `design::row_selected_bg_on` has already lifted to
            // the floor a row state has to clear. All three used to come from tokens solved for
            // the table, which is one step away from this plane.
            //
            // The cursor sits between hover and selection because it *is* between them: the tree
            // only paints it while it holds the keyboard, and the panel's own focus border has
            // already said so. Before it existed, a keyboard reader had no way to tell which of
            // 71 rows the arrows were on.
            // Hover and press composite over the row's current state, so a
            // pointer arriving on the selected row strengthens its fill rather
            // than replacing it — the forwards list and the picker already do
            // this, and a selection that vanishes under the pointer reads as
            // flicker.
            .hover(|this| {
                this.bg(if selected {
                    design::state::hover_on(
                        design::row_selected_bg_on(cx, surface),
                        design::role::accent(cx),
                    )
                } else if cursor {
                    design::state::hover_on(
                        tree_cursor_bg(surface, cx),
                        design::role::fg_primary(cx),
                    )
                } else {
                    design::state::hover(cx, surface)
                })
            })
            .active(|this| {
                this.bg(if selected {
                    design::state::press_on(
                        design::row_selected_bg_on(cx, surface),
                        design::role::accent(cx),
                    )
                } else if cursor {
                    design::state::press_on(
                        tree_cursor_bg(surface, cx),
                        design::role::fg_primary(cx),
                    )
                } else {
                    design::state::press(cx, surface)
                })
            })
            .when(cursor, |this| this.bg(tree_cursor_bg(surface, cx)))
            .when(selected, |this| {
                this.bg(design::row_selected_bg_on(cx, surface))
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.click_count() > 1 {
                    return;
                }
                this.tree_cursor = Some(index);
                window.focus(&this.tree_focus_handle, cx);
                this.on_tree_click(click_row.clone(), cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.tree_cursor = Some(index);
                    window.focus(&this.tree_focus_handle, cx);
                    this.open_tree_context_menu(index, Some(event.position), window, cx);
                    cx.stop_propagation();
                }),
            )
            .tooltip(common::hover_hint(row_hint.clone()))
            .aria_description(row_hint)
            // No selection rail. The guide is explicit that a selected navigation
            // item shows itself through "a selected fill, stronger foreground, or
            // heavier weight" and that a leading-edge bar is a web template habit:
            // it breaks the item's own silhouette and adds a second, competing edge
            // to a column that already aligns on its text. The row already has the
            // fill (`.when(selected, …row_selected_bg)`) and the mark and detail
            // already step up to `fg_primary` / `fg_secondary` above, so the state
            // is carried twice over and the third signal was never information.
            // The keyboard cursor is a ring around the row, drawn by the tree's own
            // focus handling rather than by a mark inside the row.
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(0.0))
                    .h(super::tree::SIDEBAR_ROW_HEIGHT)
                    .px(super::tree::SIDEBAR_INSET)
                    .gap(super::tree::SIDEBAR_LANE_GAP)
                    .items_center()
                    .id(("tree-item", index))
                    .role(Role::ListItem)
                    .accessibility_id(format!("tree-item-{id}"))
                    // The row above owns the click, the hover and the selection. Both lanes are
                    // fixed, so the label starts at the same x on every row whether the row has a
                    // triangle and a kind mark or not — which is what lets a reader scan the column
                    // as a column.
                    .child(disclosure)
                    .child(
                        Icon::new(icon)
                            // `tree::SIDEBAR_MARK_LANE` and not `Icon::xsmall()`'s 12px: the lane is
                            // the size the kind marks are drawn and judged at, and every mark in
                            // this column is that size — including the group head's.
                            .with_size(Size::Size(super::tree::SIDEBAR_MARK_LANE))
                            .flex_none()
                            .text_color(mark_ink),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .text_ellipsis()
                            // Stated, not inherited, for the same reason the tab strip states its
                            // ink: gpui-kit's `Label` re-applies `theme().foreground` after it
                            // takes the caller's style, so an ink set on the row never reached the
                            // glyph — every row drew `fg.primary` whatever this said.
                            .child(
                                Label::new(label)
                                    .text_size(design::text::BODY)
                                    .line_height(design::text::BODY_LINE_HEIGHT)
                                    .font_weight(emphasis)
                                    .text_color(label_ink),
                            ),
                    )
                    .when_some(detail, |this, detail| {
                        // The count sits in its own lane at a fixed width, so a row that gains or
                        // loses a count moves nothing else on the column: the label's *start* is
                        // pinned by the three fixed lanes to the left of it, and the lane is
                        // reserved whether or not there is anything to print in it. It takes the
                        // same tabular figures the table's own number columns take, for the same
                        // reason — a proportional `1` is narrower than a proportional `8` and the
                        // counts staircase.
                        this.child(
                            div()
                                .flex_none()
                                .w(super::tree::SIDEBAR_COUNT_LANE)
                                .text_right()
                                .font_features(crate::settings::data_typography(cx).features)
                                .child(text_small(detail).text_color(detail_color)),
                        )
                    })
                    .when(cursor, |this| this.aria_active_descendant()),
            )
    }

    pub(super) fn render_center(
        &mut self,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        self.normalize_open_tabs();
        let colors = design::colors(cx);
        let tabs = self
            .open_tabs
            .iter()
            .filter_map(|index| {
                self.tabs
                    .get(*index)
                    .map(|tab| (*index, tab.title.clone(), tab.icon))
            })
            .collect::<Vec<_>>();
        let active = self.tabs.get(self.active_tab);
        let connection_failure = self.render_connection_failure(cx);
        // Index 0 always uses the shared Pods view.
        let active_view: Option<TabView> = if connection_failure.is_some() {
            None
        } else {
            match active {
                Some(tab) if tab.kind.as_ref() == "Pod" && self.active_tab == 0 => {
                    Some(TabView::Resource(self.pods.clone()))
                }
                Some(_) => self
                    .views
                    .get(self.active_tab)
                    .and_then(Option::as_ref)
                    .cloned(),
                None => None,
            }
        };
        // The resource header belongs to a resource table and to nothing else.
        // It is the one place the reader learns what they are looking at and how
        // much of it there is, so it is mounted with the table it describes and
        // not with the window.
        //
        // Its *slot* is not conditional, though. A tab that is not a resource table has no
        // header to draw, and the slot still spends `size::RESOURCE_HEADER` — see
        // [`resource_header_band`]. Dropping it moved the open-view strip, and every band below
        // it, up 40px and back down again.
        let header = match (connection_failure.is_some(), active, active_view.as_ref()) {
            (false, Some(_), Some(TabView::Resource(view))) => {
                let view = view.clone();
                self.render_resource_header(view, cx)
            }
            _ => resource_header_band(cx)
                .id("resource-header-reserved")
                .debug_selector(|| "resource-header-reserved".to_owned())
                .into_any_element(),
        };
        let content = if let Some(failure) = connection_failure {
            failure
        } else {
            match (active, active_view) {
                (Some(_), Some(TabView::Resource(view))) => view.into_any_element(),
                (Some(_), Some(TabView::Overview(view))) => view.into_any_element(),
                (Some(_), Some(TabView::Forwards(view))) => view.into_any_element(),
                (Some(_), Some(TabView::Helm(view))) => view.into_any_element(),
                (Some(_), Some(TabView::Settings(view))) => view.into_any_element(),
                (Some(_), Some(TabView::Preview(view))) => view.into_any_element(),
                (Some(tab), None) => self.render_resource_placeholder(tab, cx),
                (None, _) => div().into_any_element(),
            }
        };
        let search_for_bounds = self.search.clone();
        let panel_label = active
            .map(|tab| tab.title.clone())
            .unwrap_or_else(|| SharedString::from("No Open Tab"));
        v_flex()
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .debug_selector(|| "resource-center".to_owned())
            .relative()
            .overflow_hidden()
            .bg(colors.background.alpha(1.0))
            // `UI-REDESIGN.md` §3.1 and `UI-SPEC.md` §4.3 put the open-view bar between the
            // resource header and the table, not above the header. The header names the thing
            // being looked at and the bar names the other things it could be, so the reading
            // order is "Pods 18, then the other views, then the rows" — and putting the bar first
            // made the one view the reader is in the second line of the answer to a question
            // they had not asked yet. It was also why the bar's `surface.chrome` could not read as
            // one continuous chrome band with the title bar: the header sat in between.
            //
            // The header is a band of the centre column and not inside the tab panel. Putting it
            // inside would change what `h_full()` means for the table beneath — and the table's
            // rows are the thing a reader opened the window for.
            .child(header)
            .child(self.render_center_tab_bar(window, &tabs, cx))
            .child(
                div()
                    .id("center-tab-panel")
                    .debug_selector(|| "center-tab-panel".to_owned())
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .role(Role::TabPanel)
                    .accessibility_id(format!("center-tab-panel-{}", self.active_tab))
                    .aria_label(panel_label)
                    .child(content),
            )
            // The search overlay is a modal over the whole window, so the shell root mounts it
            // next to the palette and the dialog. Mounted here it resolved its `size_full()`
            // against the centre column, and the scrim began 230 logical pixels in: the resource
            // tree, its filter field, all sixteen Kind rows and the status bar stayed at full
            // value next to a white modal, which `DESIGN.md` §3.4 does not let a modal do. The
            // measurement below stays, because the panel still sizes its card from the centre
            // column it covers.
            .when(self.search_open, |this| {
                this.on_children_prepainted(move |children, window, cx| {
                    let bounds = children.into_iter().reduce(|current, bounds| {
                        let left = current.left().min(bounds.left());
                        let top = current.top().min(bounds.top());
                        let right = current.right().max(bounds.right());
                        let bottom = current.bottom().max(bounds.bottom());
                        Bounds::new(point(left, top), size(right - left, bottom - top))
                    });
                    let Some(bounds) = bounds else {
                        return;
                    };
                    if search_for_bounds.read(cx).container_bounds() == Some(bounds) {
                        return;
                    }
                    let search = search_for_bounds.clone();
                    window.defer(cx, move |_, cx| {
                        search.update(cx, |search, cx| search.set_container_bounds(bounds, cx));
                    });
                })
            })
    }

    /// Render a resource tab before its view is ready.
    fn render_resource_placeholder(&self, tab: &CenterTab, cx: &Context<Self>) -> AnyElement {
        if self.session.is_some() {
            match &self.catalog_state {
                CatalogState::Loading => {
                    return tree_status(
                        "resource-placeholder-loading",
                        IconName::LoaderCircle,
                        "Loading resources…",
                        cx,
                    );
                }
                CatalogState::Failed(_)
                    if matches!(
                        &self.connection,
                        ConnectionState::Failed(_) | ConnectionState::Reconnecting(_)
                    ) =>
                {
                    return tree_status(
                        "resource-connection-status",
                        self.connection_failure_icon(),
                        self.connection_failure_message(),
                        cx,
                    );
                }
                CatalogState::Failed(_) => {
                    return tree_status(
                        "resource-placeholder-failed",
                        IconName::TriangleAlert,
                        resource_catalog_missing_message(&tab.title),
                        cx,
                    );
                }
                CatalogState::Ready => {}
            }
        }
        // The shared empty state owns the media/title/description shape and the action slot, so
        // this surface is the same one every other empty surface in the app draws.
        crate::panels::common::empty_state_with_action(
            tab.icon,
            "Browse this view",
            resource_catalog_missing_message(&tab.title),
            Some(
                common::labelled(
                    Button::new("placeholder-show-pods")
                        .ghost()
                        .with_size(Size::Size(design::size::CONTROL))
                        .w(px(ACTION_BUTTON_WIDTH))
                        .tab_index(0isize)
                        .tooltip("Show Pods")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.activate_tab(0, cx);
                            cx.notify();
                        })),
                    "Show Pods",
                )
                .into_any_element(),
            ),
        )
    }

    fn render_center_tab_item(
        &self,
        slot: CenterTabSlot,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let CenterTabSlot {
            index,
            position,
            total,
            title,
            icon,
            separated,
        } = slot;
        let colors = design::colors(cx);
        // The plane the tab sits on. The strip paints `role::surface_chrome` and the pill paints
        // no plane of its own, so this is the one surface the tab's washes are read against —
        // `panels/dock.rs` names it once for the same reason.
        let tab_surface = design::role::surface_chrome(cx);
        let focused = self.center_tabs_focus.is_focused(window);
        let cursor = self.center_tab_cursor();
        let pinned = self.center_tab_is_pinned(index);
        let is_active = index == self.active_tab;
        let dragging = self
            .center_tab_drag
            .is_some_and(|drag| drag.source == index);
        let drop_before = self.center_tab_drag.and_then(|drag| drag.insertion) == Some(position);
        let drop_after = self.center_tab_drag.and_then(|drag| drag.insertion) == Some(position + 1);
        let group: SharedString = format!("center-tab-{index}").into();
        let close_label = format!("Close {title} Tab");
        let drag_shell = cx.weak_entity();
        h_flex()
            .id(("center-tab", index))
            .debug_selector(move || format!("center-tab-item-{index}"))
            .group(group)
            .relative()
            // The pill is 22px inside the 28px strip, three pixels above and below, so the
            // selected tint is a rounded shape the reader can see rather than a band that colours
            // the whole navigation row. This is the same pill `panels/dock.rs` draws, and the
            // same `radius::SM`.
            .h(CENTER_TAB_HEIGHT)
            .flex_none()
            .rounded(design::radius::SM)
            // Symmetric padding at `space::SM`, with the trailing edge one step wider so the close
            // control is a control with air around it rather than a glyph parked against the
            // edge of the strip. It used to be `space::MD` on the leading edge and `space::SM` on
            // the trailing one, and the only reason for that asymmetry was the 2px rail that used
            // to stand in the leading padding.
            .px(design::space::SM)
            .pr(design::space::MD)
            .gap(design::space::XS)
            .items_center()
            // `UI-SPEC.md` §4.3: a middot 12px from each neighbour. It sits inside this item's
            // own left padding so the row keeps one child per tab, and it scrolls with the tab it
            // introduces rather than staying behind at the strip's edge.
            //
            // Both sides are `space::MD`, and the left one used to be `px(0.0)`: measured on the
            // open-view strip the middot landed 2px from the selected tab's trailing edge and 18px
            // from the next tab's leading glyph, so the mark that exists to say "these are two
            // things" was touching one of them and floating away from the other.
            .when(separated, |this| {
                this.pl(design::space::MD).child(
                    div()
                        .flex_none()
                        .h_full()
                        .pr(design::space::MD)
                        .items_center()
                        .child(
                            Label::new("·")
                                .text_size(design::text::BODY)
                                .text_color(design::role::fg_disabled(cx)),
                        ),
                )
            })
            .cursor_grab()
            .role(Role::Tab)
            .accessibility_id(format!("center-tab-{index}"))
            .aria_label(if pinned {
                format!("{title}, Pinned").into()
            } else {
                title.clone()
            })
            .aria_selected(is_active)
            .aria_position_in_set(position + 1)
            .aria_size_of_set(total)
            // Keyboard focus gets §3.2's ring: a 2px accent edge all the way round the tab, drawn
            // by the product's own focus token so the window has one answer to what focus looks
            // like. It is applied to the tab's own box, so focusing a tab never resizes it and
            // never moves the name beside it.
            .when(focused && index == cursor, |this| {
                this.focus_visible(common::focus_ring(cx))
            })
            .opacity(if dragging { 0.55 } else { 1.0 })
            // *Selected*, read through the tab's own surface and nothing else — the same five
            // points `panels/dock.rs` sets for its own strip, so the two tab strips in the app
            // are one component:
            //
            // - plane: the strip's own `role::surface_chrome`, unchanged. The pill has no plane
            //   of its own, so a selected tab and the strip behind it are the same surface plus a
            //   tint. It used to take the active tab's own plane, a second surface, and the two
            //   strips then disagreed about what "selected" looks like in the same window.
            // - fill: `role::accent_wash` — the same 12% of the accent `panels/dock.rs` paints —
            //   on the whole pill at `design::radius::SM`, so the tint follows the rounded
            //   silhouette instead of filling the whole 28px band behind the tab.
            // - ink: `role::fg_primary` at `design::text::SEMIBOLD` when selected against
            //   `role::fg_secondary` at `design::text::REGULAR` when not. The weight is the
            //   channel that replaces the marker: a `13/600` word beside a `13/400` one separates
            //   in greyscale, where a 12%-accent wash does not.
            //
            // Nothing is reserved for a marker. The 2px accent bar that used to run along the
            // bottom of the open tab is gone with the bar above the item, for the reason the guide
            // gives: a one-sided border as the selection marker "breaks the item's rounded
            // silhouette and adds a second, competing edge to a column that already aligns on its
            // text" — and the two middot-separated tab strips in this window align on their
            // text. It was absolute, so it reserved nothing either way.
            .when(is_active, |this| {
                this.bg(design::role::accent_wash(cx))
                    // The active tab has no hover of its own, so a pointer resting on the open
                    // tab looked like a pointer resting on nothing. The wash is a low alpha
                    // accent over the tab's own surface: the pressed state used to be a full
                    // `element_selected` slab, which measured 1.462:1 dark and 1.229:1 light over
                    // a navigation strip, and the same slab was just removed from the table
                    // header for exactly that reason.
                    .hover(|this| this.bg(tab_accent_wash(colors, false)))
                    .active(|this| this.bg(tab_accent_wash(colors, true)))
            })
            // The inactive wash is the accent read off the strip it actually sits on, which is the
            // same answer `panels/dock.rs` gives its own unselected tab.
            .when(!is_active, |this| {
                this.hover(|this| this.bg(design::state::hover(cx, tab_surface)))
                    .active(|this| this.bg(design::state::press(cx, tab_surface)))
            })
            .when(focused && index == cursor, |this| {
                this.aria_active_descendant()
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if event.modifiers.control {
                        this.open_tab_context_menu(index, Some(event.position), window, cx);
                        window.prevent_default();
                        cx.stop_propagation();
                        return;
                    }
                    window.focus(&this.center_tabs_focus, cx);
                    this.activate_tab(index, cx);
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_tab_context_menu(index, Some(event.position), window, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                    window.focus(&this.center_tabs_focus, cx);
                    this.close_tab_index(index, window, cx);
                }),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.modifiers().control {
                    return;
                }
                window.focus(&this.center_tabs_focus, cx);
                this.activate_tab(index, cx);
                cx.notify();
            }))
            .on_drag(
                CenterTabDragPayload {
                    index,
                    title: title.clone(),
                    icon,
                },
                move |drag, _, _, cx| {
                    let source = drag.index;
                    let title = drag.title.clone();
                    let icon = drag.icon;
                    if let Some(shell) = drag_shell.upgrade() {
                        shell.update(cx, |shell, cx| {
                            shell.center_tab_drag = Some(CenterTabDragState {
                                source,
                                insertion: None,
                            });
                            cx.notify();
                        });
                    }
                    cx.new(|_| CenterTabDragPreview { title, icon })
                },
            )
            .on_drag_move(cx.listener(
                move |this: &mut Self, event: &DragMoveEvent<CenterTabDragPayload>, _, cx| {
                    if event.bounds.contains(&event.event.position) {
                        let after = event.event.position.x > event.bounds.center().x;
                        this.update_center_tab_drag(event.drag(cx).index, index, after, cx);
                    }
                },
            ))
            .on_drop(cx.listener(
                move |this: &mut Self, drag: &CenterTabDragPayload, window, cx| {
                    this.reorder_center_tab(drag.index, window, cx);
                },
            ))
            .child(
                Icon::new(icon)
                    // The kind glyph's ink is the tab's own state, for the same reason the
                    // tab title's is: the label below was lifted out of `fg_tertiary` because the
                    // strip read as disabled, and the glyph that names the tab was left behind in
                    // it — one identity mark a step under its own label, in the row that was
                    // already saying "this view is open".
                    .xsmall()
                    .text_color(if is_active {
                        design::icon::active(cx)
                    } else {
                        design::icon::resting(cx)
                    }),
            )
            // The pin mark is a *state*, so it is reserved on every tab whether or not it has
            // one — the same argument `panels/dock.rs` makes for its status dot, and the reason
            // an unpinned tab's title does not move when it is pinned. It is a mark about the
            // tab, not a selection marker, so it does not count against the "nothing reserved for
            // the selection" rule.
            .child(Icon::new(IconName::Pin).xsmall().text_color(if pinned {
                design::icon::active(cx)
            } else {
                design::icon::incidental(cx).alpha(0.0)
            }))
            .child(
                // The label's ink and its weight are the tab's whole selected treatment, and both
                // are stated rather than inherited: gpui-kit's `Label` re-applies
                // `theme().foreground` after it takes the caller's style, so a `fg.tertiary` set
                // on the pill two children up never reached the glyph. `panels/dock.rs` hit the
                // same wall and says so at length.
                //
                // An inactive tab is `fg_secondary`, not `fg_tertiary`. A tab title is a
                // destination the reader has to be able to *read* to do the job, and
                // `DESIGN.md` §1.4 reserves the quietest ink for placeholders, column headers and
                // help text. At `fg_tertiary` the strip read as disabled: a row of words a reader
                // could not make out, one of which was the view they were already in.
                //
                // The weight carries the selection where the wash cannot. `SEMIBOLD` against
                // `REGULAR` is two rungs of the scale at one size, so the open tab is
                // unmistakable with every colour removed, and nothing about it rests on the accent
                // being legible.
                div()
                    .debug_selector(move || format!("center-tab-label-{index}"))
                    .flex_none()
                    .child(
                        Label::new(title)
                            .text_size(design::text::BODY)
                            .line_height(design::text::BODY_LINE_HEIGHT)
                            .font_weight(if is_active {
                                design::text::SEMIBOLD
                            } else {
                                design::text::REGULAR
                            })
                            .text_color(if is_active {
                                design::role::fg_primary(cx)
                            } else {
                                design::role::fg_secondary(cx)
                            }),
                    ),
            )
            .child(
                div()
                    .debug_selector(move || format!("center-tab-close-{index}"))
                    .flex_none()
                    .on_mouse_down(MouseButton::Left, |event, _, cx| {
                        if !event.modifiers.control {
                            cx.stop_propagation();
                        }
                    })
                    .child(
                        Button::new(("center-tab-close-button", index))
                            .icon(IconName::X)
                            .ghost()
                            .with_size(Size::Size(design::size::HIT_MIN))
                            .w(design::size::HIT_MIN)
                            .tooltip(close_label.clone())
                            .accessibility_label(close_label)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                window.focus(&this.center_tabs_focus, cx);
                                cx.stop_propagation();
                                this.close_tab_index(index, window, cx);
                            })),
                    ),
            )
            .when(drop_before, |this| {
                this.child(
                    // A drop indicator is a *pending* edge, not the selection, so it stays: the
                    // reader is being told where a reordering will land, which nothing else on
                    // the strip says.
                    div()
                        .id(("center-tab-drop-before", index))
                        .debug_selector(move || format!("center-tab-drop-before-{index}"))
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(design::border::FOCUS_RAIL)
                        .bg(design::role::accent(cx)),
                )
            })
            .when(drop_after, |this| {
                this.child(
                    div()
                        .id(("center-tab-drop-after", index))
                        .debug_selector(move || format!("center-tab-drop-after-{index}"))
                        .absolute()
                        .right_0()
                        .top_0()
                        .bottom_0()
                        .w(design::border::FOCUS_RAIL)
                        .bg(design::role::accent(cx)),
                )
            })
            .into_any_element()
    }

    /// The `size::RESOURCE_HEADER` resource header, and the five pills it replaces.
    ///
    /// `UI-REDESIGN` D16 deleted a toolbar of eleven chrome elements — a status
    /// chip, a cache chip, a count chip, a sort chip, an RTT chip, a filtering
    /// chip, a retry button, a pause pill, a test-updates button and the filter
    /// field — and split them across three places: the title here, the query box
    /// here, and the connection dot in the title bar. RTT is the one that
    /// disappears rather than moves: a reader is not being asked to judge it.
    ///
    /// What the old toolbar got wrong was not that it had controls. It was that
    /// all eleven looked identical, so a reader scanning the window saw a strip
    /// of pills rather than a title, a count, a way to search, and a menu. Four
    /// things with four different shapes read as a structure.
    fn render_resource_header(
        &self,
        view: Entity<crate::table_view::PodsView>,
        cx: &Context<Self>,
    ) -> AnyElement {
        use design::{role, space, text};

        let spec = view.read(cx).resource_spec();
        let rows = view.read(cx).row_count(cx);
        let filtering = view.read(cx).has_active_filter(cx);
        let paused = view.read(cx).updates_paused(cx);
        let churn = view.read(cx).churn_enabled();

        // The kind icon is the header's one accent use, and it is spent on the
        // thing the header is about rather than on a decoration.
        //
        // This is a *title* slot — `size::KIND_ICON_TITLE`, the size the twelve bespoke shapes were
        // drawn for — so the duotone fill has room to read as a wash rather than as a halftone. The
        // same asset at the sidebar's `size::KIND_ICON` is what made Config Maps and Namespaces
        // render as dot textures, which is why the tree wears the shared family instead and this
        // slot keeps the bespoke one.
        let kind_icon = div().flex_none().child(
            gpui_kit::svg()
                .path(design::kind_icon_path(spec.kind.as_ref()))
                .size(design::size::KIND_ICON_TITLE)
                // An identity mark is quiet: accent is the product's scarce
                // resource, spent on selection and focus, not decoration.
                .text_color(design::icon::resting(cx)),
        );

        let title = div()
            .flex_none()
            .text_size(text::TITLE)
            .line_height(text::TITLE_LINE_HEIGHT)
            .font_weight(text::SEMIBOLD)
            .text_color(role::fg_primary(cx))
            .child(spec.label.clone());

        // A count, not a control, and not a chip. It answers "how much am I looking
        // at" — which the table body deliberately does not repeat on every row — and
        // the header's whole job is to make the reader's eye land on the *title*.
        //
        // It used to be `text::LABEL` at `MEDIUM` in `fg_secondary` on a 4%-ink wash
        // pill, which is a badge: the second-loudest thing in the band, sitting one
        // step to the right of the word it belongs to. A count next to a title is
        // `text::CAPTION` in `fg_tertiary` — the scale's own answer for a figure beside
        // a name — and it takes the same tabular figures the table's number columns
        // take, so a count that grows a digit does not reflow the title beside it.
        let count = div()
            .flex_none()
            .font_features(crate::settings::data_typography(cx).features)
            .text_size(text::CAPTION)
            .line_height(text::CAPTION_LINE_HEIGHT)
            .font_weight(text::REGULAR)
            .text_color(role::fg_tertiary(cx))
            .child(design::format::count(rows));

        // When a filter is narrowing the view the count says so, because a count
        // that silently stops matching the body is the fastest way to make a
        // reader distrust a table.
        let count = if filtering {
            h_flex()
                .flex_none()
                .gap(space::XS)
                .items_center()
                .child(count)
                .child(
                    div()
                        .text_size(text::CAPTION)
                        .line_height(text::CAPTION_LINE_HEIGHT)
                        .font_weight(text::SEMIBOLD)
                        // A narrowed view the reader asked for is not a warning:
                        // the same state in the forwards list wears tertiary ink.
                        .text_color(role::fg_tertiary(cx))
                        .child("filtered"),
                )
                .into_any_element()
        } else {
            count.into_any_element()
        };

        // The overflow menu, and the home of everything the toolbar used to keep
        // on the surface. Ghost, so it takes a hover and gives it back; and an
        // icon button at 24px, because the header is a fixed-height strip.
        let shell_for_menu = cx.entity();
        // The menu's two rows close over the view, so the last use of it moves
        // the handle. Cloning before the popover keeps both the menu and the
        // query box pointed at the same table.
        let menu_view = view.clone();
        let menu = Popover::new("resource-overflow")
            .anchor(Anchor::BottomRight)
            .open(self.resource_menu_open)
            .on_open_change(move |open, _, cx| {
                let open = *open;
                let shell = shell_for_menu.clone();
                cx.defer(move |cx| {
                    shell.update(cx, |shell, cx| {
                        shell.resource_menu_open = open;
                        cx.notify();
                    });
                });
            })
            .trigger(
                Button::new("resource-overflow-button")
                    .icon(Icon::new(IconName::Ellipsis))
                    .ghost()
                    .with_size(Size::Size(design::size::ICON_BUTTON))
                    .w(design::size::ICON_BUTTON)
                    .tooltip("Table options")
                    .selected(self.resource_menu_open)
                    .tab_stop(false),
            )
            .content(move |_, _window, _cx| {
                v_flex()
                    .p_1()
                    .gap(design::space::XXS)
                    .child(
                        Button::new("resource-toggle-updates")
                            .label(if paused {
                                "Resume live updates"
                            } else {
                                "Pause live updates"
                            })
                            .ghost()
                            .w_full()
                            .justify_start()
                            .on_click({
                                let view = menu_view.clone();
                                move |_, _, cx: &mut gpui_kit::App| {
                                    view.update(cx, |view, cx| view.toggle_updates(cx));
                                }
                            }),
                    )
                    .child(
                        Button::new("resource-toggle-churn")
                            .label(if churn {
                                "Test updates: on"
                            } else {
                                "Test updates: off"
                            })
                            .ghost()
                            .w_full()
                            .justify_start()
                            .on_click({
                                let view = menu_view.clone();
                                move |_, _, cx: &mut gpui_kit::App| {
                                    view.update(cx, |view, cx| view.toggle_churn(cx));
                                }
                            }),
                    )
            });

        let view_for_filter = view.clone();
        // The spine. `space::LG` put this band's content 8px right of every other band on the
        // window — the title bar, the open-view strip, the sidebar's filter box all start at
        // `space::SM` — so the band that names the object you are looking at was the one band
        // whose text did not line up with anything else. It keeps its hierarchy through what is
        // actually in it: the only accent mark in the centre column, a `TITLE` semibold label, and
        // a count. An inset is not hierarchy.
        resource_header_band(cx)
            .id("resource-header")
            .debug_selector(|| "resource-header".to_owned())
            .px(design::space::SM)
            .gap(space::SM)
            .items_center()
            .child(kind_icon)
            .child(title)
            .child(count)
            .child(div().flex_1())
            .child(view_for_filter.read_with(cx, |view, cx| view.filter_input(cx)))
            .child(menu)
            .into_any_element()
    }

    fn render_center_tab_bar(
        &self,
        window: &Window,
        tabs: &[(usize, SharedString, IconName)],
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let colors = design::colors(cx);
        let pinned_tabs = tabs
            .iter()
            .filter(|(index, _, _)| self.center_tab_is_pinned(*index))
            .cloned()
            .collect::<Vec<_>>();
        let ordinary_tabs = tabs
            .iter()
            .filter(|(index, _, _)| !self.center_tab_is_pinned(*index))
            .cloned()
            .collect::<Vec<_>>();
        let has_pinned = !pinned_tabs.is_empty();
        let pinned_last = pinned_tabs.last().map(|(index, _, _)| *index);
        let ordinary_last = ordinary_tabs.last().map(|(index, _, _)| *index);
        let total = pinned_tabs.len() + ordinary_tabs.len();
        let pinned_offset = 0;
        let pinned_section = h_flex()
            .id("center-tabs-pinned")
            .debug_selector(|| "center-tabs-pinned".to_owned())
            .flex_none()
            .max_w(design::size::CENTER_MIN)
            .min_w(px(0.0))
            .h_full()
            // Centre the pills in the band; see the root strip's `items_center`.
            .items_center()
            .overflow_x_scroll()
            .track_scroll(&self.pinned_tabs_scroll)
            .restrict_scroll_to_axis()
            .children(
                pinned_tabs
                    .iter()
                    .enumerate()
                    .map(|(position, (index, title, icon))| {
                        self.render_center_tab_item(
                            CenterTabSlot {
                                index: *index,
                                position: pinned_offset + position,
                                total,
                                title: title.clone(),
                                icon: *icon,
                                separated: position > 0,
                            },
                            window,
                            cx,
                        )
                    }),
            )
            .when_some(pinned_last, |this, target| {
                this.on_drag_move(cx.listener(
                    move |this: &mut Self, event: &DragMoveEvent<CenterTabDragPayload>, _, cx| {
                        if event.bounds.contains(&event.event.position) {
                            this.update_center_tab_drop_at_end(event.drag(cx).index, target, cx);
                        }
                    },
                ))
                .on_drop(cx.listener(
                    move |this: &mut Self, drag: &CenterTabDragPayload, window, cx| {
                        this.reorder_center_tab(drag.index, window, cx);
                    },
                ))
            });
        let ordinary_offset = pinned_tabs.len();
        let ordinary_section = h_flex()
            .id("center-tabs-ordinary")
            .debug_selector(|| "center-tabs-ordinary".to_owned())
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            // Centre the pills in the band; see the root strip's `items_center`.
            .items_center()
            .overflow_x_scroll()
            .track_scroll(&self.tabs_scroll)
            .restrict_scroll_to_axis()
            .children(
                ordinary_tabs
                    .iter()
                    .enumerate()
                    .map(|(position, (index, title, icon))| {
                        self.render_center_tab_item(
                            CenterTabSlot {
                                index: *index,
                                position: ordinary_offset + position,
                                total,
                                title: title.clone(),
                                icon: *icon,
                                separated: position > 0,
                            },
                            window,
                            cx,
                        )
                    }),
            )
            .when_some(ordinary_last, |this, target| {
                this.on_drag_move(cx.listener(
                    move |this: &mut Self, event: &DragMoveEvent<CenterTabDragPayload>, _, cx| {
                        if event.bounds.contains(&event.event.position) {
                            this.update_center_tab_drop_at_end(event.drag(cx).index, target, cx);
                        }
                    },
                ))
                .on_drop(cx.listener(
                    move |this: &mut Self, drag: &CenterTabDragPayload, window, cx| {
                        this.reorder_center_tab(drag.index, window, cx);
                    },
                ))
            });
        h_flex()
            .id("center-tabs-scroll")
            .debug_selector(|| "center-tabs-items".to_owned())
            .on_drag_move(cx.listener(
                |shell: &mut Self, event: &DragMoveEvent<CenterTabDragPayload>, _, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        shell.clear_center_tab_drag(cx);
                    }
                },
            ))
            .flex_none()
            .w_full()
            .h(design::size::TAB_BAR)
            .min_w(px(0.0))
            // The pills are 22px inside a 28px band, so the sections centre them and the band
            // keeps three pixels of strip above and below — the same proportion the Dock's strip
            // draws.
            .items_center()
            .overflow_hidden()
            // The strip's plane is `role::surface_chrome`, by name, for the reason the Dock's own
            // strip uses it: a navigation band in this window is chrome, and the selected tab is
            // that surface plus an accent wash rather than a second surface of its own. It used to
            // be the theme's `tab_bar.background` field, which is one step off chrome, so the
            // centre strip and the Dock strip under it were two different greys either side of one
            // rule.
            .bg(design::role::surface_chrome(cx).alpha(1.0))
            .role(Role::TabList)
            .accessibility_id("center-tab-list")
            .aria_label("Open Tabs")
            // The tab menu is reachable without a pointer.
            .aria_keyshortcuts("Shift+F10 ContextMenu")
            .tab_group()
            .track_focus(&self.center_tabs_focus)
            .on_key_down(cx.listener(Self::on_center_tabs_key_down))
            // The strip's own edge against the table below it, solved against the plane it lands
            // on. It was the theme's `border` field, which is a control boundary, so the one line
            // that says "the navigation ends here" was the line the window was least likely to
            // draw.
            .border_b_1()
            .border_color(chrome_hairline(
                design::role::surface_chrome(cx),
                colors.border,
            ))
            .when(has_pinned, |this| this.child(pinned_section))
            .child(ordinary_section)
    }

    pub(super) fn open_tab_context_menu(
        &mut self,
        index: usize,
        position: Option<gpui_kit::Point<Pixels>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.open_tabs.contains(&index) || self.tab_context_menu.is_some() {
            return;
        }
        let shell = self.shell_weak.clone();
        let pinned = self.center_tab_is_pinned(index);
        let menu = PopupMenu::build(window, cx, move |menu, _, _| {
            let close_shell = shell.clone();
            let others_shell = shell.clone();
            let all_shell = shell.clone();
            let pin_shell = shell.clone();
            menu.item(PopupMenuItem::new("Close").icon(IconName::X).on_click(
                move |_, window, cx| {
                    if let Some(shell) = close_shell.upgrade() {
                        shell.update(cx, |shell, cx| shell.close_tab_index(index, window, cx));
                    }
                },
            ))
            .item(
                PopupMenuItem::new("Close other tabs")
                    .icon(IconName::ListCollapse)
                    .on_click(move |_, window, cx| {
                        if let Some(shell) = others_shell.upgrade() {
                            shell.update(cx, |shell, cx| {
                                shell.close_other_center_tabs(index, window, cx)
                            });
                        }
                    }),
            )
            .item(
                PopupMenuItem::new("Close all tabs")
                    .icon(IconName::X)
                    .on_click(move |_, window, cx| {
                        if let Some(all_shell) = all_shell.upgrade() {
                            all_shell.update(cx, |shell, cx| {
                                shell.close_all_center_tabs(window, cx);
                            });
                        }
                    }),
            )
            .separator()
            .item(
                PopupMenuItem::new(if pinned { "Unpin" } else { "Pin" })
                    .icon(if pinned {
                        IconName::PinOff
                    } else {
                        IconName::Pin
                    })
                    .on_click(move |_, _, cx| {
                        if let Some(pin_shell) = pin_shell.upgrade() {
                            pin_shell
                                .update(cx, |shell, cx| shell.toggle_center_tab_pin(index, cx));
                        }
                    }),
            )
        });
        let focus = menu.read(cx).focus_handle(cx).clone();
        self.tab_context_menu_previous_focus = window.focused(cx);
        self.tab_context_menu_position = position.unwrap_or_else(|| window.mouse_position());
        self.tab_context_menu = Some(menu.clone());
        let shell = self.shell_weak.clone();
        window
            .subscribe(&menu, cx, move |_, _: &DismissEvent, window, cx| {
                if let Some(shell) = shell.upgrade() {
                    shell.update(cx, |shell, cx| {
                        shell.close_tab_context_menu(window, cx);
                    });
                }
            })
            .detach();
        // The menu owns the keyboard from the moment it is on screen, and the frame
        // that puts it there has to draw before a keystroke can reach it. A defer
        // runs after this update and before that frame, so the handle is set and the
        // draw puts it on the dispatch path; a frame callback would have to wait for
        // a second frame, and nothing asks for one.
        window.defer(cx, move |window, cx| window.focus(&focus, cx));
        cx.notify();
    }

    pub(super) fn close_tab_context_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let menu_focused = self
            .tab_context_menu
            .as_ref()
            .is_some_and(|menu| menu.read(cx).focus_handle(cx).contains_focused(window, cx));
        if self.tab_context_menu.take().is_none() {
            return;
        }
        let previous_focus = self.tab_context_menu_previous_focus.take();
        if menu_focused {
            let focus = previous_focus.unwrap_or_else(|| self.center_tabs_focus.clone());
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    pub(super) fn render_tab_context_menu(&self, _cx: &Context<Self>) -> Option<AnyElement> {
        let menu = self.tab_context_menu.as_ref()?.clone();
        let position = self.tab_context_menu_position;
        Some(
            gpui_kit::deferred(
                gpui_kit::anchored()
                    .position(position)
                    .snap_to_window_with_margin(design::space::SM)
                    .child(
                        div()
                            .id("center-tab-context-menu")
                            .debug_selector(|| "center-tab-context-menu".to_owned())
                            .occlude()
                            .child(menu),
                    ),
            )
            .with_priority(3)
            .into_any_element(),
        )
    }

    /// Open the resource tree row menu from the pointer or the keyboard.
    ///
    /// `Shift+F10`, the Menu key, and a right click are the same entry point, so a
    /// keyboard user reaches every action the pointer does.
    pub(super) fn open_tree_context_menu(
        &mut self,
        index: usize,
        position: Option<gpui_kit::Point<Pixels>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tree_context_menu.is_some() {
            return;
        }
        let Some(row) = self.visible_tree_rows().get(index).cloned() else {
            return;
        };
        let expandable = row.expandable();
        let expanded = row.expanded;
        let label = row.label.clone();
        let openable = !matches!(row.kind, TreeRowKind::Group | TreeRowKind::Cluster);
        let toggle_id = row.id.clone();
        let open_row = row;
        let shell = self.shell_weak.clone();
        let menu = PopupMenu::build(window, cx, move |menu, _, _| {
            let toggle_shell = shell.clone();
            let open_shell = shell.clone();
            let copy_shell = shell.clone();
            let copy_label = label.clone();
            let mut menu = menu;
            if expandable {
                menu = menu.item(
                    PopupMenuItem::new(if expanded { "Collapse" } else { "Expand" })
                        .icon(if expanded {
                            IconName::ChevronUp
                        } else {
                            IconName::ChevronDown
                        })
                        .on_click(move |_, _, cx| {
                            if let Some(shell) = toggle_shell.upgrade() {
                                shell.update(cx, |shell, cx| {
                                    shell.toggle_row(toggle_id.clone(), cx);
                                    cx.notify();
                                });
                            }
                        }),
                );
            }
            if openable {
                // The menu builder may run more than once, so each entry owns a row.
                let open_row = open_row.clone();
                menu = menu.item(
                    PopupMenuItem::new("Open")
                        .icon(IconName::ArrowRight)
                        .on_click(move |_, window, cx| {
                            if let Some(shell) = open_shell.upgrade() {
                                shell.update(cx, |shell, cx| {
                                    shell.tree_cursor = Some(index);
                                    shell.on_tree_click(open_row.clone(), cx);
                                    shell.focus_active_view_and_clear_pending(window, cx);
                                });
                            }
                        }),
                );
            }
            menu.separator().item(
                PopupMenuItem::new("Copy name")
                    .icon(IconName::Copy)
                    .on_click(move |_, _, cx| {
                        if let Some(shell) = copy_shell.upgrade() {
                            shell.update(cx, |shell, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    copy_label.to_string(),
                                ));
                                shell.toast(
                                    format!("Copied {copy_label} to the clipboard."),
                                    design::Severity::Success,
                                    cx,
                                );
                            });
                        }
                    }),
            )
        });
        let focus = menu.read(cx).focus_handle(cx).clone();
        self.tree_context_menu_previous_focus = window.focused(cx);
        self.tree_context_menu_position = position.unwrap_or_else(|| window.mouse_position());
        self.tree_context_menu = Some(menu.clone());
        let shell = self.shell_weak.clone();
        window
            .subscribe(&menu, cx, move |_, _: &DismissEvent, window, cx| {
                if let Some(shell) = shell.upgrade() {
                    shell.update(cx, |shell, cx| {
                        shell.close_tree_context_menu(window, cx);
                    });
                }
            })
            .detach();
        window.defer(cx, move |window, cx| window.focus(&focus, cx));
        cx.notify();
    }

    pub(super) fn close_tree_context_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let menu_focused = self
            .tree_context_menu
            .as_ref()
            .is_some_and(|menu| menu.read(cx).focus_handle(cx).contains_focused(window, cx));
        if self.tree_context_menu.take().is_none() {
            return;
        }
        let previous_focus = self.tree_context_menu_previous_focus.take();
        if menu_focused {
            let focus = previous_focus.unwrap_or_else(|| self.tree_focus_handle.clone());
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    pub(super) fn render_tree_context_menu(&self, _cx: &Context<Self>) -> Option<AnyElement> {
        let menu = self.tree_context_menu.as_ref()?.clone();
        let position = self.tree_context_menu_position;
        Some(
            gpui_kit::deferred(
                gpui_kit::anchored()
                    .position(position)
                    .snap_to_window_with_margin(design::space::SM)
                    .child(
                        div()
                            .id("tree-context-menu")
                            .debug_selector(|| "tree-context-menu".to_owned())
                            .occlude()
                            .child(menu),
                    ),
            )
            .with_priority(3)
            .into_any_element(),
        )
    }

    /// Render the Inspector panel container.
    pub(super) fn render_inspector(&self, cx: &Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_none()
            .w(px(self.right_width))
            .h_full()
            .min_w(px(0.0))
            // Chrome, like the sidebar it mirrors: it is a third plane beside the content, and the
            // content is the one that is `surface_content`. Its leading edge belongs to the shell's
            // Inspector splitter.
            .bg(design::role::surface_chrome(cx).alpha(1.0))
            // Inspector focus follows the resource table.
            .tab_group()
            .tab_index(4)
            .child(self.inspector.clone())
    }

    /// Render the Dock panel container.
    pub(super) fn render_dock(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        // The shell owns the Dock's height, and `DockPanel::collapsed_height` is the value its own
        // doc says to read while the body is folded. It was not read: the box kept the full
        // remembered height either way, so folding the body left 192px of empty panel above the
        // status bar, and `UI-SPEC.md` §11.3's automatic fold below 760px did the same to every
        // short window. The fold moved a surface out of the way and the hole stayed.
        let height = if self.dock_panel.read(cx).body_collapsed(window) {
            f32::from(crate::panels::dock::DockPanel::collapsed_height())
        } else {
            self.dock_height
        };
        div()
            .id("shell-dock")
            .debug_selector(|| "shell-dock".to_owned())
            .flex_none()
            .w_full()
            .overflow_hidden()
            // Dock focus follows the Inspector.
            .tab_group()
            .tab_index(5)
            .when(self.dock_open, |this| this.h(px(height)))
            // Chrome, by name, like the title bar above it and the status bar below it: the Dock's
            // tab strip is chrome that happens to sit under the content, and the three bands the
            // shell owns are one plane with rules between them. Its own top edge belongs to the
            // shell's Dock splitter, which draws it, so this draws no border of its own.
            .bg(design::role::surface_chrome(cx).alpha(1.0))
            .when(self.dock_open, |this| this.child(self.dock_panel.clone()))
    }

    /// The live-region politeness a toast of this severity is announced with.
    ///
    /// `DESIGN.md` §4 reserves the shape and word vocabularies for `design::health_icon` and
    /// `design::health_label`, and this `match` is neither of those: it picks an ARIA politeness level
    /// and draws nothing. A toast already wears `design::severity_icon` and the message text, so the
    /// reader is told twice which class of thing arrived.
    ///
    /// The rule it states is a partition, not a threshold. `Severity` carries no order — `Info` is
    /// declared after `Error`, and `Neutral` and `Muted` are quietness roles rather than steps on a
    /// scale — so "is `Error` the most severe thing that can arrive" is not a question this type can
    /// answer, and an `Error => Alert, _ => Status` arm would look like an answer while quietly
    /// sending a `Reconnecting` toast to the polite queue. What the partition decides is which
    /// notifications may interrupt: a failure, and a thing that is not ready yet, interrupt; a
    /// confirmation of something the reader asked for waits its turn. Every arm is written out, so a
    /// new severity is a compile error rather than a silent downgrade to `Status`.
    fn toast_live_role(toast: &Toast) -> Role {
        match toast.severity {
            design::Severity::Error | design::Severity::Warning => Role::Alert,
            design::Severity::Success
            | design::Severity::Info
            | design::Severity::Neutral
            | design::Severity::Muted => Role::Status,
        }
    }

    /// Render transient feedback.
    pub(super) fn render_toast(&mut self, toast: &Toast, cx: &Context<Self>) -> AnyElement {
        if let Some(detail) = self
            .dock_panel
            .read(cx)
            .pending_notice_detail(&toast.message, toast.severity)
            && let Some(notification) = self.notifications.iter_mut().rev().find(|notification| {
                notification.message == toast.message && notification.severity == toast.severity
            })
        {
            notification.detail = Some(detail);
        }
        let colors = design::colors(cx);
        let role = Self::toast_live_role(toast);
        let full_message = self
            .notifications
            .iter()
            .rev()
            .find(|notification| {
                notification.message == toast.message && notification.severity == toast.severity
            })
            .and_then(|notification| {
                notification
                    .detail
                    .as_ref()
                    .filter(|detail| !detail.trim().is_empty())
                    .cloned()
            })
            .unwrap_or_else(|| toast.message.to_string());
        let has_detail = full_message != toast.message.as_ref();
        // The card's own surface, so the border is solved against what the reader actually sees.
        // `colors.border` on `raised` measured 1.37:1 in dark and 2.67:1 in light, and the focus
        // ring was that border, so the toast wore a permanent accent ring and dropped to the
        // hairline the moment focus moved to the dismiss button inside it. The border is now an
        // interactive boundary in its own right, and the button keeps the focus ring.
        let border = design::graphic_on_with_minimum(
            design::role::surface_raised(cx),
            colors.border,
            design::border::INTERACTIVE_MIN_CONTRAST,
        );
        let message = div()
            .id("toast-message")
            .debug_selector(|| "shell-toast-message".to_owned())
            .flex_1()
            .min_w(px(0.0))
            .overflow_hidden()
            .aria_label(toast.message.clone())
            .when(has_detail, |this| {
                this.aria_description(full_message.clone())
            })
            .child(
                div()
                    .line_height(design::text::BODY_LINE_HEIGHT)
                    // Three lines, not two. `TOAST_MAX_HEIGHT` is 64 and the padding takes 16,
                    // so the content box fits three 16px lines; clamping to two threw away the
                    // line the height was paid for, and an error toast is the one case where the
                    // third line is the reason.
                    .line_clamp(3)
                    .text_ellipsis()
                    .child(text(toast.message.clone())),
            );
        let action = toast.action.as_ref().map(|action| {
            // The label is copied out because the handler has to outlive this borrow: it reads the
            // action off the shell's own toast, so a second press finds nothing to run twice.
            let label = action.label;
            h_flex()
                .id("toast-action")
                .debug_selector(|| "shell-toast-action".to_owned())
                .flex_none()
                .child(common::labelled(
                    Button::new("toast-action-button")
                        .ghost()
                        .with_size(Size::Size(design::size::CONTROL))
                        .tab_index(0isize)
                        .on_click(cx.listener(|shell, _, window, cx| {
                            // The toast goes first, so the recovery runs against a shell with no
                            // toast on it.
                            let action = shell.toast.take().and_then(|toast| toast.action);
                            if let Some(action) = action {
                                (action.run)(shell, window, cx);
                            }
                            cx.notify();
                        })),
                    label,
                ))
        });
        let mut card = h_flex()
            .id("shell-toast")
            .debug_selector(|| "shell-toast".to_owned())
            .role(role)
            .aria_label(toast.message.clone())
            .when(has_detail, |this| {
                this.aria_description(full_message.clone())
            })
            .w_full()
            .min_w(px(0.0))
            .max_w(px(TOAST_MAX_WIDTH))
            .max_h(px(TOAST_MAX_HEIGHT))
            .px(design::space::MD)
            .py(design::space::SM)
            .gap(design::space::SM)
            .items_center()
            .overflow_hidden()
            .rounded_md()
            .border_1()
            .border_color(border)
            .bg(colors.elevated_surface_background.alpha(1.0))
            .shadow(cx.theme().shadow_tokens().lg)
            .child(
                div()
                    .id("toast-icon")
                    .debug_selector(|| "shell-toast-icon".to_owned())
                    .flex_none()
                    .child(
                        Icon::new(design::severity_icon(toast.severity))
                            .xsmall()
                            .text_color(toast.severity.marker(cx)),
                    ),
            )
            .child(message)
            // `alerts.md` asks an alert for the action that resolves it, not only for what
            // happened, so a toast that knows its way out says so.
            .when_some(action, |this, action| this.child(action))
            .child(
                div()
                    .id("toast-dismiss")
                    .debug_selector(|| "shell-toast-dismiss".to_owned())
                    .flex_none()
                    .child(
                        Button::new("toast-dismiss-button")
                            .icon(IconName::X)
                            .ghost()
                            .with_size(Size::Size(design::size::CONTROL))
                            .w(design::size::CONTROL)
                            .tooltip("Dismiss Message (Esc)")
                            .accessibility_label("Dismiss Message")
                            .on_click(cx.listener(|shell, _, _, cx| {
                                shell.toast = None;
                                cx.notify();
                            })),
                    ),
            );
        card.interactivity()
            .tooltip(common::hover_hint(full_message));
        div()
            .id("shell-toast-layer")
            .absolute()
            .left_0()
            .right_0()
            .bottom(px(toast_bottom_inset(self.dock_open, self.dock_height)))
            .px(design::space::LG)
            .flex()
            // The leading end of the bottom row, not the trailing one. The notification centre
            // and the port-forward panel are both anchored to the trailing bottom corner, and
            // this row used to land on top of them: `popovers.md` says not to put another view
            // over a popover, and the overlap covered the centre's header row and its
            // `New Port Forward…` button. The card is capped at `TOAST_MAX_WIDTH` and the
            // popovers at their own width, so at the 960px floor there is still a gap between
            // them, which `the_toast_never_lands_on_an_open_status_panel` holds.
            .justify_start()
            .child(card)
            .into_any_element()
    }

    /// Render the modal command palette.
    pub(super) fn render_command_palette(&self, cx: &Context<Self>) -> impl IntoElement {
        let matches = super::commands::filter_commands_for_scope(
            &self.commands,
            &self.palette_query,
            self.palette_scope,
        );
        let selected = self.selected_palette_index().unwrap_or(0);
        let empty = matches.is_empty();
        let result_label = palette_result_label(matches.len());
        // The measured chrome replaces the prediction from the second frame on, so a wrong
        // constant cannot keep clipping the last row.
        let chrome = match self.palette_chrome.get() {
            0.0 => palette_chrome_height(),
            measured => measured,
        };
        let card_height = palette_card_height(&matches, chrome);
        let context = self
            .clusters
            .get(self.active_cluster)
            .map(|name| name.as_ref())
            .unwrap_or("No Context");
        let namespace = self.namespace.as_ref();
        // The scope names where the switcher acts. The kind is the tab title behind the dialog,
        // which `DESIGN.md` §3.1 says must not be restated, and for the kind switcher it is also
        // the row the card ticks, so the card contradicted itself.
        let scope = format!("{context} · {namespace}");
        // The title names the task; the scope gets its own line. A single
        // middle-dot chain repeated the toolbar directly behind the dialog and
        // truncated mid-word as soon as a cluster name grew.
        let title = self.palette_scope.title();
        let shell = cx.weak_entity();
        // The field's own handle, so the card's focus ring can hang on the box that wraps it.
        // The field is the control the keyboard is on the moment the card opens, and GPUI only
        // resolves `focus_visible` on an element that tracks a handle.
        let input_focus = self.palette_input.read(cx).focus_handle(cx);
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            // The scrim states the modality, and it is the shared token composed to half its ink
            // rather than a fixed alpha. At the token's own strength the app behind the card was
            // gone — the cluster tree, the table and the status bar all dropped to one flat value
            // and the card floated in a void — and the palette is dismissed straight back into the
            // session it came from. See `PALETTE_SCRIM_INK`, which is the same number
            // `panels::search` uses so the two overlays dim their context identically.
            .bg(palette_scrim(cx))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_palette(window, cx)),
            )
            .flex()
            .justify_center()
            .child(
                v_flex()
        .on_children_prepainted(move |children, window, cx| {
            let (Some(first), Some(list), Some(last)) = (
                children.first(),
                children.get(3),
                children.last(),
            ) else {
                return;
            };
            let measured = f32::from(list.top() - first.top())
                + f32::from(last.bottom() - list.bottom());
            // The listener may run again next frame, so the deferred update takes its
            // own handle rather than moving this one out of a shared capture.
            let entity = shell.clone();
            window.defer(cx, move |_, cx| {
                let Some(shell) = entity.upgrade() else {
                    return;
                };
                // A card that reports the same chrome again must not ask for another
                // frame, or the measurement would schedule a render per frame.
                if (shell.read(cx).palette_chrome.get() - measured).abs() <= 0.5 {
                    return;
                }
                shell.update(cx, |shell, _| shell.palette_chrome.set(measured));
            });
        })
                    .id("command-palette")
                    // Not the trigger's `command-palette`: two elements under one selector make
                    // `debug_bounds` answer with whichever was prepainted last, and the card is the
                    // one the palette-height test has to measure.
                    .debug_selector(|| "command-palette-card".to_owned())
                    .mt(design::space::XL + design::size::TOOLBAR + design::space::XL)
                    // The palette takes at most 60% of the window on each axis, and never
                    // grows past its own maximum.
                    .w(DefiniteLength::Fraction(PALETTE_VIEWPORT_SHARE))
                    .max_w(px(PALETTE_WIDTH))
                    .min_w(px(0.0))
                    .h(px(card_height))
                    .max_h(DefiniteLength::Fraction(PALETTE_VIEWPORT_SHARE))
                    .flex_none()
                    // `radius::XL`, not `radius::LG`. The shape rule gives a palette and a
                    // dialog the product's largest radius, and `panels::search` — the sibling
                    // window-wide overlay this file already claims to be one component family
                    // with — takes it. `radius::LG` is the popover's radius: this card owns the
                    // whole window, so it floats a step further from it than a popover over a
                    // control does, and a card on the same radius as a menu above the title bar
                    // is one card at two altitudes.
                    .rounded(design::radius::XL)
                    // One hairline, one surface, one shadow — the three values
                    // `panels::search` puts on its own card, so the two overlays cannot drift
                    // apart. It used to take the theme's `shadow_tokens().lg` and a theme
                    // background field, and the surface the rows' washes below are solved
                    // against was therefore named by something other than the role it is.
                    .border_1()
                    .border_color(design::role::border_base(cx))
                    .bg(design::role::surface_raised(cx).alpha(1.0))
                    .shadow(design::shadow::overlay(cx))
                    .overflow_hidden()
                    // The card is its chrome plus a flexible list, so the chrome is what the
                    // list does not get: everything above it and everything below it. Child order
                    // is declaration order, and the list frame is the third child. Measuring the
                    // gaps from the laid-out positions rather than from tokens is what keeps the
                    // two-line scope block, the field's own frame, and the footer's rule counted
                    // once each.
                    .role(Role::Dialog)
                    .accessibility_id("command-palette")
                    .aria_label(format!("Command palette. Scope: {scope}"))
                    .track_focus(&self.palette_focus_handle)
                    .tab_group()
                    .tab_index(-5isize)
                    .key_context("CommandPalette")
                    // Focus stays inside the palette; Escape is the documented way out.
                    .aria_keyshortcuts("Tab Shift+Tab Escape Up Down Enter")
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        v_flex()
                            .id("command-palette-scope")
                            .debug_selector(|| "command-palette-scope".to_owned())
                            .flex_none()
                            .w_full()
                            .min_w(px(0.0))
                            .px(PALETTE_LANE_PAD)
                            .pt(design::space::SM)
                            .gap(design::space::XS)
                            .child(
                                h_flex()
                                    .id("command-palette-title")
                                    .debug_selector(|| "command-palette-title".to_owned())
                                    .w_full()
                                    .min_w(px(0.0))
                                    .items_center()
                                    // A modal title is a title and nothing else. The count used to
                                    // sit in a trailing lane on this baseline, and at `text::LABEL`
                                    // in the tertiary ink beside a 15px name
                                    // `Command palette155 results` was one run-on string with no
                                    // hierarchy in it: the reader had to work out which half was
                                    // the name of the surface. It is on the scope line below
                                    // instead, which is the band that already says what the card
                                    // is pointed at — and the band the two switchers put their
                                    // own count on.
                                    .child(
                                        div().min_w(px(0.0)).flex_shrink_1().child(
                                            label_panel_title(title)
                                                .text_color(design::role::fg_primary(cx))
                                                .truncate(),
                                        ),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .id("command-palette-search")
                            .debug_selector(|| "command-palette-search".to_owned())
                            .flex_none()
                            .m(design::space::SM)
                            // Inset like the rows and the title, so the field lines up with
                            // them and takes the whole width between them: `space::SM` of margin
                            // plus `space::XS` of padding is the same twelve pixels the title and
                            // the scope line start at.
                            .mx(design::space::SM)
                            .px(design::space::XS)
                            .rounded(design::radius::MD)
                            .items_center()
                            .track_focus(&input_focus)
                            // The product's own focus treatment on the box that carries the
                            // field's focus handle: a `border::FOCUS_RAIL` accent stroke and the
                            // soft halo `design::state::glow_for` describes.
                            .focus_visible(common::focus_ring(cx))
                            // The field paints the product's focus treatment itself:
                            // `table_view::TextInput` turns off gpui-kit's 3px ring and puts
                            // the accent on the field's own border with `design::state`'s halo
                            // on the row around it, so there is nothing here to clip and no
                            // second language of ring on the card.
                            .child(self.palette_input.clone()),
                    )
                    // The scope gets its own line under the field, which is the shape
                    // `DESIGN.md` §4 asks for and the shape the resource search already has.
                    // Between the title and the field it read as part of the title, and for the
                    // context switcher it named the answer to the question the card asks.
                    //
                    // The count shares this band rather than the title's. Both are metadata
                    // about what the card is looking at — where it is pointed and how much is
                    // there — and the switchers put their count and their current value on one
                    // band for the same reason. The count sits in a trailing lane so the scope
                    // sentence truncates against the card's edge instead of pushing it out.
                    .child(
                        h_flex()
                            .id("command-palette-scope-line")
                            .debug_selector(|| "command-palette-scope-line".to_owned())
                            .flex_none()
                            .w_full()
                            .min_w(px(0.0))
                            // The same box the resource search gives its scope line, and the same
                            // one inset the rows, the group heads and the footer use, so the two
                            // modals put the same sentence on the same spine.
                            .px(PALETTE_LANE_PAD)
                            // Twelve pixels, not eight: the list adds `space::XS` above its first
                            // line and the first group head adds `space::SM` above itself, and at
                            // eight the context sentence sat on the group head's shoulder.
                            .pb(design::space::MD)
                            .gap(design::space::SM)
                            .items_center()
                            .aria_label(format!("Current scope: {scope}"))
                            .child(
                                h_flex().min_w(px(0.0)).flex_shrink_1().child(
                                    Label::new(scope.clone())
                                        .text_size(design::text::LABEL)
                                        .line_height(design::text::LABEL_LINE_HEIGHT)
                                        // Stated, not inherited: gpui-kit's `Label` re-applies
                                        // `theme().foreground` after it takes the caller's
                                        // style, so a wrapper's ink never reaches the glyph.
                                        .text_color(design::role::fg_tertiary(cx))
                                        .truncate(),
                                ),
                            )
                            .child(
                                h_flex()
                                    .id("command-palette-result-count")
                                    .debug_selector(|| {
                                        "command-palette-result-count".to_owned()
                                    })
                                    .flex_none()
                                    .justify_end()
                                    .role(Role::Status)
                                    .aria_label(result_label.clone())
                                    .child(
                                        Label::new(result_label)
                                            .text_size(design::text::LABEL)
                                            .line_height(design::text::LABEL_LINE_HEIGHT)
                                            .text_color(design::role::fg_tertiary(cx)),
                                    ),
                            ),
                    )
                    .child(
                        // The frame owns the list plus the two edges that say the results
                        // continue past the window. The edges stay out of the scrolling
                        // element, so they neither scroll with the rows nor take a hitbox
                        // away from them.
                        v_flex()
                            .flex_1()
                            .min_h(px(0.0))
                            .child(
                                div()
                                    .id("command-palette-list")
                                    .role(Role::ListBox)
                                    .aria_label("Commands")
                                    // gpui-kit's `Scrollbar` owns the thumb, the track, and
                                    // the idle fade, so the palette computes no scroll
                                    // arithmetic of its own. The layer is absolutely
                                    // positioned against this box, so the box is its
                                    // containing block.
                                    .relative()
                                    .flex_1()
                                    .min_h(px(0.0))
                                    .overflow_y_scroll()
                                    .track_scroll(&self.palette_scroll)
                                    .py(design::space::XS)
                                    .vertical_scrollbar(&self.palette_scroll)
                                    .when(empty, |this| {
                                        // The app's one empty state, asked for by name rather
                                        // than rebuilt. The palette was the last surface still
                                        // drawing its own sentence-plus-control, so two overlays
                                        // in the same window said the same thing two ways — and
                                        // the pair of them is how the resource search and both
                                        // switchers already say it. The wrapper carries the
                                        // card's inset and takes the list's height, because the
                                        // shared state is `flex_1` and a percentage height in a
                                        // scrolling column has nothing definite to resolve
                                        // against: without this box the state collapsed to
                                        // nothing under the field.
                                        this.child(
                                            div()
                                                .id("command-palette-empty")
                                                .debug_selector(|| {
                                                    "command-palette-empty".to_owned()
                                                })
                                                .w_full()
                                                .flex_1()
                                                .min_h(px(0.0))
                                                .px(PALETTE_LANE_PAD)
                                                .child(common::empty_state_with_action(
                                                    IconName::Search,
                                                    "No matching commands",
                                                    "Clear the search, or try another term.",
                                                    // `DESIGN.md` §9 promises a control here: a
                                                    // sentence that tells the reader to clear
                                                    // the search is not a way to clear it. Ghost
                                                    // rather than outlined, because that is the
                                                    // control the switchers' own empty states
                                                    // offer, and a ring round it would make the
                                                    // palette's clear the one bordered button in
                                                    // a window that has no other. The wrapper
                                                    // carries the selector because the shared
                                                    // `Button` does not expose one.
                                                    Some(
                                                        div()
                                                            .id("command-palette-clear-search")
                                                            .debug_selector(|| {
                                                                "command-palette-clear-button"
                                                                    .to_owned()
                                                            })
                                                            .child(common::labelled(
                                                                Button::new(
                                                                    "command-palette-clear-button",
                                                                )
                                                                .ghost()
                                                                .with_size(Size::Size(
                                                                    design::size::CONTROL,
                                                                ))
                                                                .tab_index(0isize)
                                                                .on_click(cx.listener(
                                                                    |shell, _, window, cx| {
                                                                        shell.palette_query.clear();
                                                                        shell.palette_input.update(
                                                                            cx,
                                                                            |input, cx| {
                                                                                input.set_text("", window, cx)
                                                                            },
                                                                        );
                                                                        shell.palette_note = None;
                                                                        shell.sync_palette_selection();
                                                                        shell.reveal_palette_selection();
                                                                        cx.notify();
                                                                    },
                                                                )),
                                                                "Clear search",
                                                            ))
                                                            .into_any_element(),
                                                    ),
                                                )),
                                        )
                                    })
                                    .children(self.render_command_rows(&matches, selected, cx)),
                            )
                            .children(self.render_palette_list_edges(cx)),
                    )
                    .child(
                        // The hints are help, not data, and this strip is the one place in the
                        // card that must not compete with the rows above it. Four things say so:
                        // one hairline instead of a plane (a second plane inside the card is a
                        // card inside the card), `text::MICRO` verbs in the tertiary ink rather
                        // than the body size the rows' labels use, a chip built by
                        // `palette_keycap` — the same box the rows' own shortcut chips are — and
                        // one short item per key with a gap between them, so the strip wraps
                        // rather than truncating a run-on line.
                        h_flex()
                            .id("command-palette-footer")
                            .debug_selector(|| "command-palette-footer".to_owned())
                            .flex_none()
                            .w_full()
                            .min_w(px(0.0))
                            .h(design::size::ROW)
                            .px(PALETTE_LANE_PAD)
                            .gap(design::space::LG)
                            .items_center()
                            .border_t_1()
                            .border_color(design::role::border_subtle(cx))
                            // Show why an unavailable command cannot run.
                            .when_some(self.palette_note, |this, note| {
                                this.child(
                                    h_flex()
                                        .flex_none()
                                        .gap(design::space::XS)
                                        .items_center()
                                        // `design::health_icon` gives every severity one shape,
                                        // and the note is an explanation, not a failure, so it
                                        // takes the info shape in the muted ink. Painting an info
                                        // glyph in the warning colour says two things at once.
                                        .child(
                                            Icon::new(design::health_icon(design::Severity::Info))
                                                .xsmall()
                                                .text_color(design::role::fg_tertiary(cx)),
                                        )
                                        .child(
                                            Label::new(note)
                                                .text_size(design::text::MICRO)
                                                .line_height(design::text::MICRO_LINE_HEIGHT)
                                                .text_color(design::role::fg_tertiary(cx)),
                                        ),
                                )
                            })
                            .when(self.palette_note.is_none(), |this| {
                                this.when_some(keystroke("up"), |this, kb| {
                                    this.child(footer_hint("Move selection", &kb, cx))
                                })
                                .when_some(keystroke("enter"), |this, kb| {
                                    this.child(footer_hint("Run command", &kb, cx))
                                })
                                .when_some(keystroke("escape"), |this, kb| {
                                    this.child(footer_hint("Dismiss", &kb, cx))
                                })
                            }),
                    ),
            )
    }

    /// Render a dialog button with a visible focus state.
    ///
    /// `variant` is a gpui-kit `ButtonVariant`; the dialog only ever asks for the
    /// filled, the outlined and the destructive filled button, so it names those
    /// three directly instead of carrying a style table of its own.
    fn dialog_button(
        &self,
        id: &'static str,
        label: &'static str,
        focus_index: usize,
        variant: ButtonVariant,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &Context<Self>,
    ) -> AnyElement {
        let colors = design::colors(cx);
        // `Button` owns a focus handle keyed by its own id, so the dialog's ring handle is
        // attached to the wrapper it drives. Without it the handle belongs to no element and
        // the shell's focus-lost fallback takes focus back the moment the dialog opens.
        let focus = self.dialog_button_focus(focus_index);
        // `ButtonVariant::Default` is the dialog's outlined button: gpui-kit keeps
        // the outline as a flag beside the variant, and the dialog's three actions
        // are filled, outlined and destructive, so the pair is the whole table.
        let outlined = variant == ButtonVariant::Default;
        let button = Button::new(id)
            .label(label)
            .with_variant(variant)
            .when(outlined, |button| button.outline())
            .with_size(Size::Size(design::size::CONTROL))
            .w(px(ACTION_BUTTON_WIDTH))
            .tab_index(focus_index as isize)
            .tooltip(label)
            .accessibility_label(label)
            .on_click(on_click);
        // The dialog moves a ring between its actions rather than letting focus
        // land on one and stay there, so the ring lives on the wrapper the dialog
        // focuses and the button keeps the shared control's own presentation.
        div()
            .rounded_md()
            .track_focus(&focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    this.dialog_focus = focus_index;
                    this.focus_dialog_index(focus_index, window, cx);
                }),
            )
            .border_1()
            .border_color(if self.dialog_focus == focus_index {
                colors.border_focused
            } else {
                colors.border_variant
            })
            .child(button)
            .into_any_element()
    }

    fn dialog_cancel_button(
        &self,
        id: &'static str,
        focus_index: usize,
        cx: &Context<Self>,
    ) -> AnyElement {
        self.dialog_button(
            id,
            "Cancel",
            focus_index,
            ButtonVariant::Default,
            cx.listener(|this, _, window, cx| this.cancel_dialog(window, cx)),
            cx,
        )
    }

    fn dialog_actions(cancel: AnyElement, confirm: AnyElement) -> gpui_kit::Div {
        h_flex()
            .justify_end()
            .gap(design::space::SM)
            .child(cancel)
            .child(confirm)
    }

    fn focus_dialog_index(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog_focus = index;
        self.focus_dialog_control(window, cx);
    }

    fn dialog_shell(
        &self,
        role: Role,
        title: impl Into<SharedString>,
        detail: Label,
        window: &Window,
        cx: &Context<Self>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        let colors = design::colors(cx);
        let title = title.into();
        v_flex()
            .id("dialog-card")
            .w(px(super::dialog_width(f32::from(
                window.viewport_size().width,
            ))))
            .max_h(px((f32::from(window.viewport_size().height)
                - f32::from(design::space::XXL))
            .max(0.0)))
            .overflow_y_scroll()
            // `radius::XL` for the reason `render_command_palette` gives it: this is a dialog,
            // the largest silhouette in the window, and the sibling resource search takes the
            // same one. `radius::LG` belongs to a popover over a control.
            .rounded(design::radius::XL)
            .border_1()
            // The same resolved boundary the toast and the popovers use. `colors.border` on
            // `raised` is 1.37:1 in dark and 2.67:1 in light, and the dialog carried it as its
            // focus ring, so a dialog that had never been focused looked focused and one that
            // had lost focus looked like a sheet of paper.
            .border_color(design::graphic_on_with_minimum(
                design::role::surface_raised(cx),
                colors.border,
                design::border::INTERACTIVE_MIN_CONTRAST,
            ))
            .bg(colors.elevated_surface_background.alpha(1.0))
            .shadow(cx.theme().shadow_tokens().lg)
            .track_focus(&self.dialog_focus_handle)
            .tab_group()
            .key_context("Dialog")
            .accessibility_id("shell-dialog")
            // Focus stays inside the dialog; Escape is the documented way out.
            .aria_keyshortcuts("Tab Shift+Tab Escape")
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .p(design::space::LG)
            .gap(design::space::MD)
            .role(role)
            .aria_label(title.clone())
            .child(section_text(title).text_color(colors.text))
            .child(detail.text_color(colors.text_muted))
    }

    fn render_dialog_input(
        &self,
        id: &'static str,
        input: &Entity<TextInput>,
        focus_index: usize,
        cx: &Context<Self>,
    ) -> AnyElement {
        let input = input.clone();
        let input_bounds = Rc::new(Cell::new(Bounds::<Pixels>::default()));
        let input_bounds_for_layout = input_bounds.clone();
        let input_for_click = input.clone();

        div()
            .w_full()
            .debug_selector(move || id.to_owned())
            .on_children_prepainted(move |children, _, _| {
                if let Some(bounds) = children.first() {
                    input_bounds_for_layout.set(*bounds);
                }
            })
            .id(id)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.focus_dialog_index(focus_index, window, cx);
                    let bounds = input_bounds.get();
                    if bounds.size.width > px(0.) {
                        let text = input_for_click.read(cx).text().to_owned();
                        let index =
                            super::dialog_input_index_at(&text, event.position, bounds, window, cx);
                        this.set_dialog_input_caret(&input_for_click, index, window, cx);
                    }
                    cx.notify();
                }),
            )
            .child(input)
            .into_any_element()
    }

    /// Render the active modal dialog.
    pub(super) fn render_dialog(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        let colors = design::colors(cx);
        let error_ink = design::status_colors(cx).error;
        // The plane every choice row inside a dialog card is read against. The card paints the
        // theme's elevated surface, so the two option lists below — a container to open a shell
        // in, a port to forward — take their selection from `design::row_selected_bg_on` solved
        // against it. They used to take `colors.element_selected`, a filled slab the theme solves
        // for a web list, which is the one web-shaped fill a desktop dialog must not have.
        let card_surface = colors.elevated_surface_background;
        let Some(dialog) = self.dialog.as_ref() else {
            return div().into_any_element();
        };
        let mut card;

        match dialog {
            Dialog::ConfirmTabClose { request } => {
                let (title, detail) = match request {
                    super::TabCloseRequest::One(_) => (
                        "Close tab?",
                        "Unsaved YAML changes in this tab will be permanently discarded.",
                    ),
                    super::TabCloseRequest::Others(_) => (
                        "Close other tabs?",
                        "Unsaved YAML changes in these tabs will be permanently discarded. You cannot undo this.",
                    ),
                    super::TabCloseRequest::All => (
                        "Close all tabs?",
                        "Unsaved YAML changes in every tab will be permanently discarded. You cannot undo this.",
                    ),
                };
                card = self
                    .dialog_shell(Role::AlertDialog, title, text(detail), window, cx)
                    .child(Self::dialog_actions(
                        self.dialog_cancel_button("dialog-close-tabs-cancel", 0, cx),
                        self.dialog_button(
                            "dialog-close-tabs-confirm",
                            "Close",
                            1,
                            ButtonVariant::Danger,
                            cx.listener(|this, _, window, cx| this.confirm_dialog(window, cx)),
                            cx,
                        ),
                    ));
            }
            Dialog::HelmConfirm {
                action,
                input,
                error,
                ..
            } => {
                let title = action.title();
                let detail = action.detail();
                let confirm_label = action.confirm_label();
                let destructive = matches!(action, crate::panels::HelmAction::Uninstall { .. });
                let upgrade_input = input.is_some();
                let cancel_focus: usize = if upgrade_input { 1 } else { 0 };
                let confirm_focus: usize = if upgrade_input { 2 } else { 1 };
                let enabled = input.as_ref().is_none_or(|input| {
                    super::parse_chart_reference(input.read(cx).text()).is_ok()
                });
                let field = input
                    .as_ref()
                    .map(|input| self.render_dialog_input("dialog-helm-chart-input", input, 0, cx));
                let confirm_style = if destructive {
                    ButtonVariant::Danger
                } else {
                    ButtonVariant::Primary
                };
                let confirm = if enabled {
                    self.dialog_button(
                        "dialog-helm-confirm",
                        confirm_label,
                        confirm_focus,
                        confirm_style,
                        cx.listener(|this, _, window, cx| this.confirm_helm(window, cx)),
                        cx,
                    )
                } else {
                    // The confirm is disabled until the chart reference parses, and
                    // it still has to sit in the same slot with the same focus ring,
                    // so it is the same button without the handler.
                    let confirm_focus_index = confirm_focus;
                    let button = Button::new("dialog-helm-confirm")
                        .label(confirm_label)
                        .with_variant(confirm_style)
                        .with_size(Size::Size(design::size::CONTROL))
                        .w(px(ACTION_BUTTON_WIDTH))
                        .tab_index(confirm_focus as isize)
                        .tooltip(confirm_label)
                        .accessibility_label(confirm_label)
                        .disabled(true);
                    div()
                        .rounded_md()
                        .track_focus(&self.dialog_button_focus(confirm_focus_index))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                this.dialog_focus = confirm_focus_index;
                                this.focus_dialog_index(confirm_focus_index, window, cx);
                            }),
                        )
                        .border_1()
                        .border_color(if self.dialog_focus == confirm_focus {
                            colors.border_focused
                        } else {
                            colors.border_variant
                        })
                        .child(button)
                        .into_any_element()
                };
                card = self.dialog_shell(Role::AlertDialog, title, text(detail), window, cx);
                if let Some(field) = field {
                    card = card
                        .child(text_small("Chart reference").text_color(colors.text_muted))
                        .child(field)
                        .when_some(error.clone(), |this, message| {
                            this.child(text_small(message).text_color(error_ink))
                        });
                }
                card = card.child(Self::dialog_actions(
                    self.dialog_cancel_button("dialog-helm-cancel", cancel_focus, cx),
                    confirm,
                ));
            }
            Dialog::ConfirmDelete {
                title,
                detail,
                count,
                ..
            } => {
                let title = if *count > 1 {
                    format!("Delete {} selected objects?", design::format::count(*count))
                } else {
                    title.to_string()
                };
                card = self
                    .dialog_shell(Role::AlertDialog, title, text(detail.clone()), window, cx)
                    .child(Self::dialog_actions(
                        self.dialog_cancel_button("dialog-delete-cancel", 0, cx),
                        self.dialog_button(
                            "dialog-delete-confirm",
                            "Delete",
                            1,
                            ButtonVariant::Danger,
                            cx.listener(|this, _, window, cx| this.confirm_delete(window, cx)),
                            cx,
                        ),
                    ));
            }
            Dialog::Exec { target, selected } => {
                let title = format!("Open shell in {}", target.name);
                let containers = target.containers.clone();
                card = self
                    .dialog_shell(
                        Role::Dialog,
                        title,
                        text("Choose a container for the shell."),
                        window,
                        cx,
                    )
                    .children(containers.iter().enumerate().map(|(index, container)| {
                        let active = index == *selected;
                        let focused = self.dialog_focus == 0;
                        let container = container.clone();
                        h_flex()
                            .id(("exec-container", index))
                            .relative()
                            .w_full()
                            .h(design::size::ROW)
                            .px(design::space::SM)
                            .gap(design::space::XS)
                            .items_center()
                            .rounded(design::radius::MD)
                            .role(Role::Button)
                            .aria_label(container.clone())
                            .aria_selected(active)
                            .when(active && focused, |this| this.aria_active_descendant())
                            .when(active, |this| {
                                this.bg(design::row_selected_bg_on(cx, card_surface))
                            })
                            // Hover and press strengthen the active row's fill
                            // instead of replacing it.
                            .hover(|this| {
                                this.bg(if active {
                                    design::state::hover_on(
                                        design::row_selected_bg_on(cx, card_surface),
                                        design::role::accent(cx),
                                    )
                                } else {
                                    design::state::hover(cx, card_surface)
                                })
                            })
                            .active(|this| {
                                this.bg(if active {
                                    design::state::press_on(
                                        design::row_selected_bg_on(cx, card_surface),
                                        design::role::accent(cx),
                                    )
                                } else {
                                    design::state::press(cx, card_surface)
                                })
                            })
                            .when(focused, |this| {
                                this.child(
                                    div()
                                        .absolute()
                                        .left_0()
                                        .top_0()
                                        .bottom_0()
                                        .w(design::border::FOCUS_RAIL)
                                        .bg(colors.text_accent),
                                )
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.dialog_focus = 0;
                                if let Some(Dialog::Exec { selected, .. }) = this.dialog.as_mut() {
                                    *selected = index;
                                }
                                cx.notify();
                            }))
                            .child(
                                Icon::new(if active {
                                    IconName::Check
                                } else {
                                    IconName::SquareTerminal
                                })
                                .xsmall()
                                .text_color(if active {
                                    colors.text
                                } else {
                                    colors.text_muted
                                }),
                            )
                            .child(text(container))
                    }))
                    .child(Self::dialog_actions(
                        self.dialog_cancel_button("dialog-exec-cancel", 1, cx),
                        self.dialog_button(
                            "dialog-exec-confirm",
                            "Open Shell",
                            2,
                            ButtonVariant::Primary,
                            cx.listener(|this, _, window, cx| this.confirm_exec_dialog(window, cx)),
                            cx,
                        ),
                    ));
            }
            Dialog::PortForward {
                target,
                input,
                local,
                selected,
                error,
            } => {
                let detail = match &target.namespace {
                    Some(namespace) => {
                        format!("In namespace {namespace}, choose or enter the container port.")
                    }
                    None => "For the context, choose or enter the container port.".to_owned(),
                };
                let title = format!("Port forward to {}", target.name);
                let field = self.render_dialog_input("dialog-port-forward-input", input, 0, cx);
                let local_field = self.render_dialog_input(
                    "dialog-port-forward-local-input",
                    local,
                    super::PORT_FORWARD_LOCAL_FOCUS,
                    cx,
                );
                // The Pod's own container ports are offered as choices, so the common case needs
                // no typing. A Pod that declares none keeps the free-text field.
                let choices: Vec<AnyElement> = target
                    .ports
                    .iter()
                    .enumerate()
                    .map(|(index, port)| {
                        let active = selected == &Some(port.port);
                        let port_number = port.port;
                        let label = port.label();
                        h_flex()
                            .id(("port-forward-port", index))
                            .h(design::size::ROW)
                            .px(design::space::SM)
                            .gap(design::space::XS)
                            .items_center()
                            .rounded(design::radius::MD)
                            .border_1()
                            .border_color(if active {
                                colors.border_focused
                            } else {
                                colors.border_variant
                            })
                            .role(Role::Button)
                            .aria_label(format!("Remote port {label}"))
                            .aria_selected(active)
                            .when(active, |this| {
                                this.bg(design::row_selected_bg_on(cx, card_surface))
                            })
                            .hover(|this| {
                                this.bg(if active {
                                    design::state::hover_on(
                                        design::row_selected_bg_on(cx, card_surface),
                                        design::role::accent(cx),
                                    )
                                } else {
                                    design::state::hover(cx, card_surface)
                                })
                            })
                            .active(|this| {
                                this.bg(if active {
                                    design::state::press_on(
                                        design::row_selected_bg_on(cx, card_surface),
                                        design::role::accent(cx),
                                    )
                                } else {
                                    design::state::press(cx, card_surface)
                                })
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_remote_port(port_number, window, cx);
                                this.focus_dialog_index(0, window, cx);
                            }))
                            .child(
                                Icon::new(if active {
                                    IconName::Check
                                } else {
                                    IconName::ArrowRightLeft
                                })
                                .xsmall()
                                .text_color(if active {
                                    colors.text
                                } else {
                                    colors.text_muted
                                }),
                            )
                            .child(text(label))
                            .into_any_element()
                    })
                    .collect();
                // An empty local field asks for no port, and the forwards layer picks a free one.
                let local_error =
                    crate::panels::terminal::parse_local_port(local.read(cx).text()).err();
                card = self
                    .dialog_shell(Role::Dialog, title, text(detail), window, cx)
                    .when(!choices.is_empty(), |this| {
                        this.child(
                            h_flex()
                                .id("port-forward-choices")
                                .w_full()
                                .gap(design::space::XS)
                                .flex_wrap()
                                .children(choices),
                        )
                    })
                    .child(text_small("Remote Port").text_color(colors.text_muted))
                    .child(field)
                    .when_some(error.clone(), |this, message| {
                        this.child(text_small(message).text_color(error_ink))
                    })
                    .child(text_small("Local Port").text_color(colors.text_muted))
                    .child(local_field)
                    .when_some(local_error, |this, message| {
                        this.child(text_small(message).text_color(error_ink))
                    })
                    .child(
                        text_small("Leave Local Port empty to assign a free local port.")
                            .text_color(colors.text_muted),
                    )
                    .child(Self::dialog_actions(
                        self.dialog_button(
                            "dialog-forward-cancel",
                            "Cancel",
                            super::PORT_FORWARD_CANCEL_FOCUS,
                            ButtonVariant::Default,
                            cx.listener(|this, _, window, cx| {
                                this.focus_dialog_index(
                                    super::PORT_FORWARD_CANCEL_FOCUS,
                                    window,
                                    cx,
                                );
                                this.cancel_dialog(window, cx)
                            }),
                            cx,
                        ),
                        self.dialog_button(
                            "dialog-forward-confirm",
                            "Start Port Forward",
                            super::PORT_FORWARD_CONFIRM_FOCUS,
                            ButtonVariant::Primary,
                            cx.listener(|this, _, window, cx| {
                                this.focus_dialog_index(
                                    super::PORT_FORWARD_CONFIRM_FOCUS,
                                    window,
                                    cx,
                                );
                                this.confirm_port_forward(window, cx)
                            }),
                            cx,
                        ),
                    ));
            }
            Dialog::Scale {
                target,
                input,
                error,
                ..
            } => {
                let detail = match &target.object.namespace {
                    Some(namespace) => {
                        format!("In namespace {namespace}, enter the new replica count.")
                    }
                    None => "For the context, enter the new replica count.".to_owned(),
                };
                let title = format!("Scale {}", target.object.name);
                let field = self.render_dialog_input("dialog-scale-input", input, 0, cx);
                card = self
                    .dialog_shell(Role::Dialog, title, text(detail), window, cx)
                    .child(text_small("Replica Count").text_color(colors.text_muted))
                    .child(field)
                    .when_some(*error, |this, message| {
                        this.child(text_small(message).text_color(error_ink))
                    })
                    .child(Self::dialog_actions(
                        self.dialog_button(
                            "dialog-scale-cancel",
                            "Cancel",
                            1,
                            ButtonVariant::Default,
                            cx.listener(|this, _, window, cx| {
                                this.focus_dialog_index(1, window, cx);
                                this.cancel_dialog(window, cx)
                            }),
                            cx,
                        ),
                        self.dialog_button(
                            "dialog-scale-confirm",
                            "Scale",
                            2,
                            ButtonVariant::Primary,
                            cx.listener(|this, _, window, cx| {
                                this.focus_dialog_index(2, window, cx);
                                this.confirm_scale(window, cx)
                            }),
                            cx,
                        ),
                    ));
            }
        }

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .bg(design::role::surface_backdrop(cx))
            // Clicking the backdrop cancels the dialog.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.cancel_dialog(window, cx)),
            )
            .flex()
            .justify_center()
            .items_center()
            .child(card)
            .into_any_element()
    }

    /// The edges of the scrolling list: the fades that say the results continue past the window.
    ///
    /// The thumb, its track, and its idle fade belong to gpui-kit's `Scrollbar`, which the list
    /// itself carries. The edge fades are not a scrollbar, so they stay here; they read the same
    /// scroll handle the component does, so the two cannot disagree about which end has more.
    fn render_palette_list_edges(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let scroll = &self.palette_scroll;
        let max_offset = f32::from(scroll.max_offset().y);
        if max_offset <= 0.0 {
            return Vec::new();
        }
        let scrolled = (-f32::from(scroll.offset().y) / max_offset).clamp(0.0, 1.0);
        // The card's own plane, by the same role the rows' washes are solved against. Naming
        // the theme field here instead put the fade on a value nothing else in the list reads.
        let surface = design::role::surface_raised(cx);
        let mut edges = Vec::new();
        if scrolled > 0.0 {
            edges.push(Self::palette_edge_fade(surface, true));
        }
        if scrolled < 1.0 {
            edges.push(Self::palette_edge_fade(surface, false));
        }
        edges
    }

    /// One edge fade: bands of the card surface that thin out away from the window edge, so a
    /// row the window cuts in half reads as more content instead of as a broken row. The bands
    /// are stacked in element order rather than angled, so the direction of the fade is stated
    /// in the layout instead of in a gradient angle.
    fn palette_edge_fade(surface: gpui_kit::Hsla, at_top: bool) -> AnyElement {
        let height = px(f32::from(design::space::XS) * PALETTE_FADE_BANDS as f32);
        let fade = v_flex().absolute().left_0().right_0().h(height);
        // The strongest band sits at the window edge, whichever edge that is.
        let fade = if at_top {
            fade.top_0()
        } else {
            fade.bottom_0().flex_col_reverse()
        };
        fade.children((1..=PALETTE_FADE_BANDS).map(|band| {
            div()
                .flex_1()
                .bg(surface.alpha(1.0 - band as f32 / (PALETTE_FADE_BANDS as f32 + 1.0)))
        }))
        .into_any_element()
    }

    fn render_command_rows(
        &self,
        matches: &[&super::commands::Command],
        selected: usize,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        // The card's own plane, and the one surface every row's wash is read against. The card
        // paints `role::surface_raised`, and a row wash solved against any other plane is a tint
        // that lands somewhere the reader cannot predict.
        let row_surface = design::role::surface_raised(cx);
        let selected_row = design::row_selected_bg_on(cx, row_surface);
        let mut rows = Vec::new();
        let mut group: Option<&str> = None;
        for (index, command) in matches.iter().enumerate() {
            if group != Some(command.group.as_ref()) {
                group = Some(command.group.as_ref());
                // A group head is a label for a set, not one more row of it. It used to take the
                // section role its rows shared in size, weight and ink, so the only thing that
                // said which kind of line it was belonged to a screen reader. It goes through the
                // same lane builder as its rows, so its label starts on their spine rather than
                // two columns to their left.
                //
                // The accessible name stays in the case the group is written in. The uppercase
                // is a visual treatment for a short set heading and shouting the name of the
                // set at a screen reader as well buys nothing.
                let heading = command.group.clone();
                rows.push(
                    h_flex()
                        .id(("command-group", index))
                        .w_full()
                        .h(design::size::GROUP_HEAD)
                        .flex_none()
                        // `space::SM` above every head and `space::XXS` below it, the first one
                        // included, so the groups read as groups and the first is not welded to the
                        // context line above it. `palette_group_head_height` is the same sum.
                        .mt(design::space::SM)
                        .mb(design::space::XXS)
                        .px(PALETTE_LANE_PAD)
                        .items_center()
                        .role(Role::Group)
                        .aria_label(heading)
                        .child(palette_lanes(
                            None,
                            None,
                            // `section_text` is the one heading label this file owns; the size is
                            // restated, because `panels::search` draws its group head at
                            // `text::CAPTION` and the two overlays have to read as one component
                            // family rather than as two palettes with different headings.
                            //
                            // Uppercase because this is the one line on the card that is allowed
                            // it: a short set heading, which is exactly what `DESIGN.md` §2.3
                            // reserves it for. Without it the head is an 11px semibold grey line
                            // sitting two pixels above a 13px row, and it reads as an ordinary
                            // row with nothing in it — the exact failure the eight pixels above
                            // it and the `Role::Group` were there to prevent. The `+0.06em`
                            // tracking §2.3 pairs with the uppercase has no API in this text
                            // stack, the same gap `table_view/view.rs` reports for column headers,
                            // so the case carries the separation by itself.
                            section_text(command.group.to_uppercase())
                                .text_size(design::text::CAPTION)
                                .line_height(design::text::CAPTION_LINE_HEIGHT)
                                .font_weight(design::text::SEMIBOLD)
                                // Stated, not inherited, for the reason the title row's
                                // label states it.
                                .text_color(design::role::fg_tertiary(cx))
                                .into_any_element(),
                            None,
                        ))
                        .into_any_element(),
                );
            }
            // The chip has to name a key the shell surface owns. Every business binding is
            // released inside the palette, so a palette-scoped lookup answers with nothing and
            // the row would draw an empty keycap where the shortcut belongs. A command with no
            // key in the shell gets the empty trailing lane instead.
            let trailing: Option<AnyElement> = command.binding.and_then(|binding| {
                let action = (binding.make_action)();
                keymap::binding_for_context(action.as_ref().name(), "Shell", cx)
                    .and_then(|chord| keystroke(&chord))
                    .map(|stroke| palette_keycap(&Kbd::format(&stroke), cx))
            });
            // The current row carries its check and nothing else. The badge lane stays for
            // commands that genuinely cannot run, where the reason is the point.
            let current = super::palette_command_is_current(command);
            let trailing = trailing.or_else(|| match &command.run {
                CommandRun::Unavailable { badge, .. } if !current => Some(
                    Label::new(*badge)
                        .text_size(design::text::CAPTION)
                        .line_height(design::text::CAPTION_LINE_HEIGHT)
                        .text_color(design::role::fg_tertiary(cx))
                        .into_any_element(),
                ),
                _ => None,
            });
            let command_id = command.id.clone();
            let label = if current {
                format!("{}, current", command.label)
            } else {
                command.label.to_string()
            };
            let is_selected = index == selected;
            let command_icon = command.icon;
            let command_label = command.label.to_string();
            rows.push(
                h_flex()
                    .id(("command", index))
                    .w_full()
                    .h(design::size::PALETTE_ROW)
                    .flex_none()
                    // The selected fill is inset by `space::XS` and carries the palette's own
                    // radius, so it stays one continuous row and never paints outside a rounded
                    // corner of the card.
                    .px(design::space::XS)
                    .rounded(design::radius::LG)
                    .role(Role::ListBoxOption)
                    .aria_label(label)
                    .aria_selected(is_selected)
                    .aria_position_in_set(index + 1)
                    .aria_size_of_set(matches.len())
                    // *Selected*, read through the row's own surface and nothing else: a fill on
                    // the whole row and `role::fg_primary` on its label and mark. There is no
                    // leading-edge rail and nothing is reserved for one — the guide is explicit
                    // that a one-sided border as the selection marker "breaks the item's rounded
                    // silhouette and adds a second, competing edge to a column that already
                    // aligns on its text", and the strip of rows above this one already aligns on
                    // its text. `panels::search` removed one from its rows for the same reason.
                    //
                    // The fill is the app's own selected-row wash over the card's own plane, so
                    // the row is the same accent channel the table, the tree and the two switchers
                    // select with, solved against the surface it is painted on rather than against
                    // the table's.
                    .when(is_selected, |this| this.bg(selected_row))
                    .hover(|this| {
                        this.bg(if is_selected {
                            design::state::hover_on(selected_row, design::role::accent(cx))
                        } else {
                            design::state::hover_on(row_surface, design::role::fg_primary(cx))
                        })
                    })
                    .active(|this| {
                        this.bg(if is_selected {
                            design::state::press_on(selected_row, design::role::accent(cx))
                        } else {
                            design::state::press_on(row_surface, design::role::fg_primary(cx))
                        })
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.run_command(command_id.clone(), window, cx)
                    }))
                    .child(
                        // The row's own `space::XS` inset plus this `space::SM` puts the lanes at
                        // `PALETTE_LANE_PAD`, the same twelve pixels the group headings, the empty
                        // state, the scope line and the footer start at. The lanes are one helper,
                        // but a helper cannot agree with itself about an inset that lives on the
                        // element *around* it — and eight pixels of drift between a heading and the
                        // rows under it is exactly what this helper exists to stop.
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .px(design::space::SM)
                            .child(palette_lanes(
                                // The check follows the row that is current, not the row the keyboard is
                                // on. Bound to the selection it ticked whatever the cursor happened to
                                // rest on, so the card could tick Pods while its own scope line said the
                                // view was Overview.
                                current.then(|| {
                                    Icon::new(IconName::Check)
                                        .xsmall()
                                        .text_color(design::role::fg_primary(cx))
                                        .into_any_element()
                                }),
                                Some(
                                    Icon::new(command_icon)
                                        .xsmall()
                                        // The resting ink, on every row, selected or not.
                                        //
                                        // It was `fg_tertiary`, argued as "a command's glyph is
                                        // its category, not its state". The observation is right
                                        // and the conclusion is not: `fg.tertiary` is the PLACEHOLDER
                                        // and count tier, so a mark at it beside an `fg.secondary`
                                        // label reads as a disabled control - which is the same
                                        // failure the title bar's cluster mark was, and the same one
                                        // the sibling switcher card had already been corrected for.
                                        // A category is not a reason to be quieter than the words
                                        // beside it; it is the reason the mark does not change with
                                        // selection, which is what this keeps.
                                        .text_color(design::icon::resting(cx))
                                        .into_any_element(),
                                ),
                                Label::new(command_label)
                                    .text_size(design::text::BODY)
                                    .line_height(design::text::BODY_LINE_HEIGHT)
                                    // The selected row's label is the other half of its selection; every
                                    // other label is `fg_secondary`, which is the ink the rows' copy uses
                                    // everywhere else in the app.
                                    .text_color(if is_selected {
                                        design::role::fg_primary(cx)
                                    } else {
                                        design::role::fg_secondary(cx)
                                    })
                                    .truncate()
                                    .into_any_element(),
                                // The chip keeps its own appearance on the selected row, so the one row
                                // the reader is on does not gain a second highlight beside the fill.
                                trailing,
                            )),
                    )
                    .into_any_element(),
            );
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;

    use crate::update::{UpdateActions, UpdateUiState};

    use super::*;

    /// The file under test, for the checks that have nothing else to read.
    ///
    /// A token swap has no bounds to measure and no event to simulate, so the only thing left is
    /// to read the line back. The comparisons below ignore whitespace, so a reformat cannot fail
    /// them, and a change of role fails them.
    const SOURCE: &str = include_str!("panels.rs");

    #[gpui_kit::test]
    fn toast_bounds_avoid_status_bar_and_open_dock_at_supported_sizes(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        shell.update(cx, |shell, cx| {
            shell.notify(
                "The app did not patch the Kubernetes workload. Inspect the full error, then try again."
                    .to_owned(),
                design::Severity::Error,
                Some(
                    "The Kubernetes API request failed. The app did not patch Pod \"api-orders-7d9f8b6c4d-h6x2q\" in namespace \"production-system\" after 3 attempts. The server returned an internal error."
                        .to_owned(),
                ),
                cx,
            );
        });

        for (width, height, dock_open) in [
            (px(960.0), px(640.0), false),
            (px(960.0), px(640.0), true),
            (px(1440.0), px(900.0), false),
            (px(1440.0), px(900.0), true),
        ] {
            cx.simulate_resize(gpui_kit::size(width, height));
            shell.update(cx, |shell, cx| {
                shell.dock_open = dock_open;
                cx.notify();
            });
            cx.run_until_parked();

            let toast = cx
                .debug_bounds("shell-toast")
                .expect("toast must be laid out");
            let message = cx
                .debug_bounds("shell-toast-message")
                .expect("toast message must be laid out");
            let dismiss = cx
                .debug_bounds("shell-toast-dismiss")
                .expect("toast dismiss action must be laid out");
            let dock = cx
                .debug_bounds("shell-dock")
                .expect("Dock layout must be laid out");
            let status_top = height - px(super::super::status_bar_height());

            assert!(toast.left() >= design::space::LG);
            assert!(toast.right() <= width - design::space::LG);
            assert!((dock.bottom() - status_top).abs() <= px(0.5));
            assert!(toast.bottom() <= status_top - design::space::LG);
            assert!(toast.right() - toast.left() <= px(TOAST_MAX_WIDTH));
            assert!(toast.bottom() - toast.top() <= px(TOAST_MAX_HEIGHT));
            assert!(message.right() <= dismiss.left());
            assert!(message.bottom() - message.top() <= design::text::BODY_LINE_HEIGHT * 2.0);
            assert!((dismiss.size.width - design::size::CONTROL).abs() <= px(0.5));
            assert!((dismiss.size.height - design::size::CONTROL).abs() <= px(0.5));
            if dock_open {
                assert!(toast.bottom() <= dock.top() - design::space::LG);
            }
        }
    }

    /// The search card is a modal over the window, so its scrim has to reach the sidebar and the
    /// status bar too.
    ///
    /// The overlay used to be mounted inside the centre column, so its `size_full()` resolved to
    /// that column: the scrim began 230 logical pixels in and the resource tree, its filter
    /// field, all sixteen Kind rows and the status bar stayed at full value next to a white
    /// modal. `modality.md` asks a modal to obscure the context it came from, and one that
    /// obscures part of it does not read as a modal.
    #[gpui_kit::test]
    fn resource_search_is_centred_on_the_window_not_the_centre_column(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        let width = px(1440.0);
        cx.simulate_resize(gpui_kit::size(width, px(900.0)));
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| assert!(shell.open_search(window, cx)));
        });
        cx.run_until_parked();

        let card = cx
            .debug_bounds("resource-search")
            .expect("resource search must be laid out");
        let centre = cx
            .debug_bounds("resource-center")
            .expect("resource center must be laid out");
        // Centred on the window, which is what a full-window scrim means. Mounted inside the
        // centre column the card was centred on that column, 200px to the right of here.
        assert!(
            (card.center().x - width / 2.0).abs() <= px(1.0),
            "the card is centred on the window, not on the column it used to be mounted in"
        );
        assert!(
            (card.center().x - centre.center().x).abs() > px(1.0),
            "the card must not still be centred on the centre column"
        );

        // The sidebar is behind the scrim now. A click on it reaches the scrim, which is the
        // point: the scrim used to stop at the centre column, so the same click landed on the
        // resource tree and opened a view behind an open modal. The overlay's own `occlude`
        // reaches its own bounds and the tree click had no `search_open` guard, so both halves
        // had to change.
        let tree = cx
            .debug_bounds("resource-tree-panel")
            .expect("the resource tree must be laid out");
        let tabs = shell.read_with(cx, |shell, _| shell.open_tabs.len());
        cx.simulate_click(tree.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            shell.read_with(cx, |shell, _| shell.open_tabs.len()),
            tabs,
            "a click behind the search overlay must not open a resource view"
        );
        assert!(
            !shell.read_with(cx, |shell, _| shell.search_open),
            "the click reached a window-wide scrim instead of the sidebar"
        );
    }

    /// The transient message must never land on an open status panel.
    ///
    /// `popovers.md` says not to show another view over a popover. The toast and the status
    /// panel are both anchored to the trailing bottom corner, so the toast used to cover the
    /// notification centre's header row and the toast's own dismiss button with it.
    #[gpui_kit::test]
    fn the_toast_never_lands_on_an_open_status_panel(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        let width = px(960.0);
        cx.simulate_resize(gpui_kit::size(width, px(640.0)));
        shell.update(cx, |shell, cx| {
            shell.toast(
                "The app could not start the port forward. Try again, then check the pod."
                    .to_owned(),
                design::Severity::Error,
                cx,
            );
        });
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| shell.open_notifications(window, cx));
        });
        cx.run_until_parked();

        let toast = cx
            .debug_bounds("shell-toast")
            .expect("the toast must be laid out");
        let panel = cx
            .debug_bounds("notification-center")
            .expect("the notification centre must be laid out");
        assert!(
            toast.right() <= panel.left(),
            "toast {:?} overlaps the panel {:?}",
            toast,
            panel
        );
    }

    /// An alert that knows its way out says so.
    ///
    /// `alerts.md` asks an alert for the action that resolves it, not only for what happened, and
    /// the full text of a failure used to be reachable only from `aria_description` and a hover
    /// tooltip, so a screen reader and a keyboard user were both told what went wrong and given
    /// nothing to do about it.
    #[gpui_kit::test]
    fn a_toast_with_a_recovery_offers_it(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        shell.update(cx, |shell, cx| {
            shell.toast_with_action(
                "Open Shell needs a context connection. Select a context, then try again."
                    .to_owned(),
                design::Severity::Warning,
                super::super::reload_kubeconfigs_action(),
                None,
                cx,
            );
        });
        cx.run_until_parked();

        let action = cx
            .debug_bounds("shell-toast-action")
            .expect("a warning with one recovery offers it");
        let message = cx
            .debug_bounds("shell-toast-message")
            .expect("the message must be laid out");
        assert!(
            action.left() >= message.right() - px(1.0),
            "the action sits beside the message, not under it"
        );
    }

    /// The bell shows the count it is named for.
    ///
    /// The top bar printed the error count and put a bell beside it whose colour came from that
    /// same count, so the notification count reached no surface at all. It is padded by the Info
    /// confirmation every successful Settings switch raises, so the number that means something
    /// is the active incidents, which is also what the notification centre puts first.
    #[gpui_kit::test]
    fn the_top_bar_counts_the_notifications_it_shows(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        shell.update(cx, |shell, cx| {
            shell.notify(
                "Port forward 8080 started.".to_owned(),
                design::Severity::Success,
                None,
                cx,
            );
            shell.notify(
                "The API server refused the patch. Nothing was changed.".to_owned(),
                design::Severity::Error,
                Some("The server returned 403 for pods/patch.".to_owned()),
                cx,
            );
        });
        cx.run_until_parked();

        let count = cx
            .debug_bounds("top-bar-notification-count")
            .expect("the bell carries its own number");
        let errors = cx
            .debug_bounds("top-bar-errors")
            .expect("the error count stays separate");
        assert!(
            errors.right() <= count.left() + px(1.0),
            "the two numbers are two groups"
        );
        let summary = shell.read_with(cx, |shell, cx| shell.status_summary(cx));
        assert_eq!(summary.notifications, 2);
        assert_eq!(
            summary.active_notifications, 1,
            "the success confirmation is not an incident"
        );
    }

    /// The disclosure has to be on screen, and inside the panel.
    ///
    /// `ListItem::toggle` puts the triangle at `left(rems(-1.))`, which is 16px left of the item
    /// box and therefore outside the sidebar for a depth-0 row, and hides the expanded one behind
    /// `visible_on_hover` on top of that. A full-width scan of the gutter found three device
    /// pixels of a collapsed triangle and none of an expanded one.
    #[gpui_kit::test]
    fn the_tree_disclosure_is_inside_the_sidebar(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (_shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.simulate_resize(gpui_kit::size(px(1440.0), px(900.0)));
        cx.run_until_parked();

        let tree = cx
            .debug_bounds("resource-tree-panel")
            .expect("the resource tree must be laid out");
        // `debug_bounds` keys on a `&'static str`, so the rows this sweep can see are named
        // here rather than formatted from the index.
        const DISCLOSURE_SELECTORS: [&str; 8] = [
            "tree-disclosure-0",
            "tree-disclosure-1",
            "tree-disclosure-2",
            "tree-disclosure-3",
            "tree-disclosure-4",
            "tree-disclosure-5",
            "tree-disclosure-6",
            "tree-disclosure-7",
        ];
        let mut found = 0;
        for (index, selector) in DISCLOSURE_SELECTORS.into_iter().enumerate() {
            let Some(disclosure) = cx.debug_bounds(selector) else {
                continue;
            };
            found += 1;
            assert!(
                disclosure.left() >= tree.left() && disclosure.right() <= tree.right(),
                "the disclosure {index} at {disclosure:?} is outside the sidebar {tree:?}"
            );
            assert!(
                disclosure.size.height >= design::size::HIT_MIN,
                "the disclosure {index} is shorter than the minimum hit area"
            );
        }
        assert!(
            found > 0,
            "no container row draws a disclosure control at all"
        );
    }

    /// The sidebar's icon column has to say something.
    ///
    /// `Overview` and `Nodes` were the same double-rack glyph, and the group rows' folder was the
    /// same closed folder, so the column stopped encoding hierarchy and 12px of indent was all
    /// the reader had.
    #[test]
    fn the_sidebar_icons_do_not_repeat() {
        assert_ne!(
            IconName::Monitor,
            design::kind_icon("Node"),
            "Overview must not look like a machine"
        );
        assert_ne!(
            design::kind_icon("Namespace"),
            IconName::Folder,
            "a namespace must not look like a collapsed group"
        );
    }

    /// The header number is a fact about the catalog, not about the rows on screen.
    ///
    /// It used to count the Kind rows the tree happened to be showing, so `16 Kinds` climbed
    /// every time a reader expanded an API group while the `All API groups 21` beside it stayed
    /// put: two numbers in the same 11px muted row disagreeing about the same list.
    #[test]
    fn the_tree_header_counts_kinds_and_not_rows() {
        let tree = super::super::tree::ResourceTree::demo();
        let closed = tree_kind_label(&tree);
        let opened = tree_kind_label(&tree);
        assert_eq!(closed, opened);
        assert_eq!(closed, format!("{} Kinds", tree.kind_count()));

        // The old count was the visible rows, and it moved with the expansion.
        let collapsed = tree.default_collapsed();
        let visible = tree
            .rows_for_cluster("kind-k8s-gpui-dev", &collapsed)
            .iter()
            .filter(|row| row.kind == TreeRowKind::Kind)
            .count();
        assert_ne!(
            visible,
            tree.kind_count(),
            "the fixture must actually hide Kinds, or this test proves nothing"
        );
    }

    #[gpui_kit::test]
    fn overflowing_center_tabs_keep_first_and_last_reachable(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.simulate_resize(gpui_kit::size(px(960.0), px(640.0)));
        shell.update(cx, |shell, cx| {
            for index in 0..12 {
                shell.open_resource_tab(
                    format!("Resource{index}").into(),
                    format!("Long resource name {index}").into(),
                    None,
                    None,
                    cx,
                );
            }
        });
        cx.run_until_parked();

        let center = cx
            .debug_bounds("center-tabs-items")
            .expect("center tab strip must be laid out");
        let scroll = shell.read_with(cx, |shell, _| shell.tabs_scroll.clone());
        assert!(scroll.max_offset().x > px(0.0));

        scroll.scroll_to_item(0);
        cx.update(|_, cx| cx.notify(shell.entity_id()));
        cx.run_until_parked();
        let first = cx
            .debug_bounds("center-tab-item-0")
            .expect("first tab must be laid out");
        assert!(first.left() >= center.left() - px(0.5));
        assert!(first.right() <= center.right() + px(0.5));

        let last_index = shell.read_with(cx, |shell, _| shell.open_tabs.len() - 1);
        assert_eq!(last_index, 12);
        scroll.scroll_to_item(last_index);
        cx.update(|_, cx| cx.notify(shell.entity_id()));
        cx.run_until_parked();
        let last = cx
            .debug_bounds("center-tab-item-12")
            .expect("last tab must be laid out");
        assert!(last.left() >= center.left() - px(0.5));
        assert!(last.right() <= center.right() + px(0.5));
    }

    /// Identity outranks chrome. `layout.md` > Visual hierarchy puts the most important items
    /// near the leading side, so the context name takes more room than the palette and Settings
    /// controls put together, and neither of them keeps a permanent shortcut chip on the surface.
    #[gpui_kit::test]
    fn the_cluster_name_outranks_the_toolbar_chrome(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        shell.update(cx, |shell, cx| {
            shell.clusters = vec!["kind-k8s-gpui-development-cluster-long-name".into()];
            shell.active_cluster = 0;
            cx.notify();
        });

        for width in [px(960.0), px(1440.0), px(1920.0)] {
            cx.simulate_resize(gpui_kit::size(width, px(900.0)));
            cx.run_until_parked();
            let cluster = cx
                .debug_bounds("cluster-selector")
                .expect("cluster selector is laid out");
            let palette = cx
                .debug_bounds("command-palette")
                .expect("command palette trigger is laid out");
            let settings = cx
                .debug_bounds("open-settings")
                .expect("settings trigger is laid out");
            assert!(
                cluster.size.width > palette.size.width + settings.size.width,
                "the context name needs more room than the chrome at {width} wide"
            );
            // The binding moved into the tooltip. A permanent chip spent toolbar width on a
            // shortcut the reader already knows, and drew it with the label token reserved for an
            // unavailable behavior.
            assert!(cx.debug_bounds("command-palette-keycap").is_none());
            assert!(cx.debug_bounds("open-settings-keycap").is_none());
        }
    }

    /// A failed connection must offer both ways back: Retry and Reload Kubeconfigs.
    #[gpui_kit::test]
    fn connection_failure_offers_retry_and_reload_kubeconfigs(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        shell.update(cx, |shell, cx| {
            shell.connection = ConnectionState::Failed("connection refused".to_owned());
            cx.notify();
        });
        cx.run_until_parked();

        assert!(cx.debug_bounds("connection-failure").is_some());
        assert!(
            cx.debug_bounds("connection-retry").is_some(),
            "Retry must stay available"
        );
        assert!(
            cx.debug_bounds("connection-reload-kubeconfigs").is_some(),
            "a dead connection must offer Reload Kubeconfigs"
        );
    }

    /// The kubeconfig warning must agree with the number of failed sources.
    #[gpui_kit::test]
    fn kubeconfig_warning_agrees_with_the_source_count(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        for (warning, expected) in [
            ("one source is broken", "1 kubeconfig source"),
            (
                "one source is broken\nanother source is broken",
                "2 kubeconfig sources",
            ),
        ] {
            shell.update(cx, |shell, cx| {
                shell.kubeconfig_warning = Some(warning.to_owned());
                cx.notify();
            });
            cx.run_until_parked();
            let message = kubeconfig_warning_message(warning);
            assert!(
                message.starts_with(expected),
                "message must name the failed sources: {message}"
            );
            assert!(
                message.contains("reload kubeconfigs"),
                "message must name the recovery action: {message}"
            );
        }
    }

    /// A debug build has no update actions, so it must not open a notice over the table.
    #[gpui_kit::test]
    fn a_build_that_cannot_update_itself_keeps_the_notice_off_the_table(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.simulate_resize(gpui_kit::size(px(960.0), px(640.0)));
        shell.update(cx, |shell, cx| {
            shell.set_update_state(UpdateUiState::new(UpdatePhase::Unsupported), cx);
        });
        cx.run_until_parked();

        // The shell still records the notice as open, so the update flow stays intact and
        // Settings keeps the status and the manual check.
        assert!(shell.read_with(cx, |shell, _| shell.update_strip_expanded));
        assert!(
            cx.debug_bounds("update-strip-overlay").is_none(),
            "a build that cannot update itself must not open a notice on every launch"
        );
        assert!(cx.debug_bounds("update-strip").is_none());
    }

    /// A managed build opens the notice once, and the card clears the table's first row.
    #[gpui_kit::test]
    fn the_update_notice_opens_once_and_clears_the_first_table_row(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        shell.update(cx, |shell, cx| {
            shell.set_update_actions(UpdateActions::new(|_| {}, |_| {}, |_| {}), cx);
            shell.set_update_state(UpdateUiState::new(UpdatePhase::Unsupported), cx);
        });
        cx.simulate_resize(gpui_kit::size(px(1440.0), px(800.0)));
        cx.run_until_parked();

        let card = cx
            .debug_bounds("update-strip")
            .expect("update card is laid out");
        let panel = cx
            .debug_bounds("center-tab-panel")
            .expect("center tab panel is laid out");
        // Measured against the first row rather than against a count of row
        // heights. The resource header now sits between the panel and the table,
        // so "the panel's top plus two rows" stopped being the first row — and
        // the assertion was passing or failing for a reason that had nothing to
        // do with the notice. The contract is the one the comment states: the
        // notice must not land on a row a reader is trying to read.
        let first_row = cx
            .debug_bounds("resource-row-0")
            .expect("the table's first row is laid out");
        assert!(
            card.top() >= first_row.top() || card.bottom() <= first_row.top(),
            "the notice must not cover the first table row: notice {card:?}, row {first_row:?}"
        );
        let _ = panel;
        assert!(card.right() <= px(1440.0) - design::space::SM);
        assert!(card.bottom() <= px(800.0 - super::super::status_bar_height()));
        for selector in [
            "update-action-check",
            "update-action-retry",
            "update-action-restart",
        ] {
            assert!(cx.debug_bounds(selector).is_some(), "{selector}");
        }

        // The updater reports the same phase again. The notice must not come back.
        shell.update(cx, |shell, cx| {
            shell.set_update_state(UpdateUiState::new(UpdatePhase::Unsupported), cx);
        });
        cx.run_until_parked();
        assert!(shell.read_with(cx, |shell, _| shell.update_strip_expanded));
        assert!(
            cx.debug_bounds("update-strip-overlay").is_none(),
            "the notice opens once per run"
        );
    }

    /// The resource tree is answerable at every width a window can be.
    ///
    /// `shell::mod` draws the tree when the *client* width reaches `design::size::WINDOW_MIN`,
    /// and that number is the *window* floor `main.rs` hands the window manager. A decorated
    /// window's client area is narrower than the window, so a window at its own minimum arrives
    /// short of the gate — 941px on the machine this was found on — and the tree is not drawn at
    /// all, with a toggle beside it that claims to be open and changes nothing. The gate is
    /// `shell::mod`'s to move, so what is held here is the answer rather than the gate: at every
    /// width the tree is either on screen or reachable from the top bar, and the control that
    /// takes the tree's slot in the narrow band opens the same list of kinds the tree holds.
    #[gpui_kit::test]
    fn the_resource_tree_is_reachable_at_the_narrowest_window(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.simulate_resize(gpui_kit::size(px(941.0), px(640.0)));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("resource-tree-panel").is_none(),
            "941px is below the layout gate, so this is the case under test"
        );

        let control = cx
            .debug_bounds("top-bar-resources")
            .expect("a window too narrow for the tree still answers for it in the top bar");
        cx.simulate_click(control.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        assert!(
            shell.read_with(cx, |shell, _| shell.palette_open),
            "the control has to reach the kind list, not merely be on screen"
        );
        assert_eq!(
            shell.read_with(cx, |shell, _| shell.palette_scope),
            crate::shell::PaletteScope::Kind,
            "it answers with the tree's own list of kinds"
        );

        // One more pixel of client area and the tree itself is drawn, with the toggle back in
        // the first slot. One band, two states — not a second feature.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.simulate_resize(gpui_kit::size(px(960.0), px(640.0)));
        cx.run_until_parked();
        assert!(cx.debug_bounds("resource-tree-panel").is_some());
        assert!(cx.debug_bounds("top-bar-resources").is_none());
    }

    /// A compact window must say what to do about the missing Inspector.
    #[test]
    fn compact_inspector_label_asks_for_a_wider_window() {
        assert_eq!(
            crate::shell::INSPECTOR_COMPACT_LABEL,
            "Inspector Unavailable. Widen the window to show it."
        );
        assert_eq!(
            crate::shell::INSPECTOR_WIDTH_HINT,
            "Widen the window to show the Inspector."
        );
    }

    /// The active tab's press is a wash, not a slab.
    ///
    /// `element_selected` over the tab strip measured 1.462:1 in dark and 1.229:1 in light, which
    /// is louder than the selected table row it sits above, and the same slab was just removed
    /// from the table header. The two washes stay inside the strip and they differ from each
    /// other, so rest, hover and press are three states.
    #[gpui_kit::test]
    fn the_active_tab_states_are_three_washes(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (_shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.update(|_, cx| {
            let colors = design::colors(cx);
            let rest = design::role::surface_content(cx);
            let hover = tab_accent_wash(colors, false);
            let pressed = tab_accent_wash(colors, true);
            assert_ne!(hover, pressed, "hover and press must be tellable apart");
            assert_ne!(rest, hover);
            assert_ne!(rest, pressed);
            // The wash is quieter than the slab it replaced and louder than nothing, measured
            // against the surface the active tab actually paints on.
            for (name, surface) in [
                ("tab bar", design::role::surface_chrome(cx)),
                ("tab active", design::role::surface_content(cx)),
            ] {
                for (state, wash) in [("hover", hover), ("press", pressed)] {
                    let ratio = design::calculate_contrast_ratio(
                        design::composite_surface(surface, wash),
                        surface,
                    );
                    assert!(
                        ratio < 1.45,
                        "{state} over the {name} surface reached {ratio}:1, which is the slab \
                         this replaced"
                    );
                    assert!(
                        ratio > 1.05,
                        "{state} over the {name} surface reached {ratio}:1, which is no state at \
                         all"
                    );
                }
            }
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("center-tab-item-0").is_some());
    }

    /// The state matrix's Tab row has a cell for `disabled`, and centre tabs have no such state.
    ///
    /// There is nothing to disable: a centre tab is either open or not there, and the strip
    /// carries no tab the reader cannot use. Rather than invent a flag that is never set, the
    /// decision is recorded where it can be checked: the tab item draws no disabled state, so
    /// `DESIGN.md` §5's cell is N/A and says so.
    #[test]
    fn centre_tabs_have_no_disabled_state() {
        let item = SOURCE
            .split("fn render_center_tab_item")
            .nth(1)
            .and_then(|rest| rest.split("fn render_center_tab_bar").next())
            .expect("the centre tab item renderer");
        assert!(
            !item.contains(".disabled("),
            "a centre tab is never disabled, so the tab item must not carry a disabled state"
        );
    }

    /// A group header is a section label, and the rows under it are body text.
    ///
    /// The header used to be body text in a lighter weight, which measured 21 device pixels of ink
    /// against its rows' 20: the label that names a group was quieter than the items it names.
    /// `DESIGN.md` §3.1 has no 13px role for a heading, so this checks the role it does name.
    #[test]
    fn the_palette_group_header_outranks_its_rows() {
        assert!(design::text::TITLE > design::text::BODY);
        let rows = SOURCE
            .split("fn render_command_rows")
            .nth(1)
            .expect("the command row renderer");
        // The needle is the group name reaching the heading helper, not the case it is written
        // in: the palette head wears the uppercase set-heading treatment `DESIGN.md` §2.3
        // reserves for section heads, and that is a decision about case, while the decision this
        // test holds is about role — a head built by `text_small` or `text_body` would satisfy
        // an uppercase check and still be body text.
        assert!(
            rows.contains("section_text(command.group.to_uppercase())"),
            "the group header takes the section role"
        );
    }

    /// A modal title is a title, not a count.
    ///
    /// The palette title, its result count and its scope line were all `metadata` at 11px, and
    /// two full-screen modals whose titles differed by 30% is what a shared token with its own
    /// subtitles looks like. `DESIGN.md` §3.1 gives `panel_title` to the names of surfaces, and
    /// the six call sites that were building that role themselves now go through the one helper.
    #[test]
    fn the_palette_title_uses_the_panel_title_role() {
        assert!(design::text::TITLE > design::text::CAPTION);
        // The scan stops at the test module, so neither assertion below can match itself, and the
        // needle is assembled so the negative one cannot match its own message.
        let code = SOURCE
            .split("mod tests {")
            .next()
            .expect("this module before its tests");
        assert!(
            code.contains("label_panel_title(title)"),
            "the modal title takes the shared panel-title role rather than naming the size again"
        );
        assert!(
            !code.contains(&["text::", "PANEL", "_TITLE"].concat()),
            "a surface title must be one helper; naming the size at a call site is how a toolbar \
             ended up naming itself in body text"
        );
    }

    /// The footer hint is one appearance, not two.
    ///
    /// `KeybindingHint::with_prefix` sets `FontStyle::Italic` and `text_disabled` on its own base
    /// and the keycap resets its face inside it, so the verb read italic and dim while the key
    /// beside it did not. `DESIGN-PROPOSAL` §4.2 item 4 is "kill the chip's italic", and a parent
    /// cannot do it because the shared component sets the style on itself.
    #[test]
    fn the_palette_footer_does_not_ask_for_an_italic_prefix() {
        // The needle is assembled rather than written out, so this assertion does not match
        // itself: the literal cannot appear anywhere in the file, including here.
        let needle = ["KeybindingHint", "::with_prefix("].concat();
        assert!(
            !SOURCE.contains(&needle),
            "the shared hint's prefix carries the disabled face; the verb is a label of its own"
        );
        assert!(SOURCE.contains("fn footer_hint("));
    }

    /// No results offers a way out of no results.
    ///
    /// `DESIGN.md` §9 claims the empty palette has a `Clear search` control, and the sibling
    /// picker has had one all along. A sentence that tells the reader to clear the search is not
    /// a way to clear it.
    #[gpui_kit::test]
    fn an_empty_palette_offers_the_clear_search_control(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.simulate_resize(gpui_kit::size(px(1440.0), px(900.0)));
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.open_palette_with_scope(
                    super::super::PaletteScope::Commands,
                    "zzz-no-such-command",
                    window,
                    cx,
                );
            });
        });
        cx.run_until_parked();
        let button = cx
            .debug_bounds("command-palette-clear-button")
            .expect("the empty palette offers Clear search");
        cx.simulate_click(button.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            shell.read_with(cx, |shell, _| shell.palette_query.clone()),
            "",
            "the control clears the search"
        );
        assert!(cx.debug_bounds("command-palette-clear-button").is_none());
    }

    /// `DESIGN.md` §5: a selected button takes the primary style, and this crate has no primary.
    ///
    /// `ButtonStyle` offers Filled, Tinted, Outlined, OutlinedGhost, OutlinedCustom, Subtle and
    /// Transparent, and a `toggle_state` with no `selected_style` keeps rendering the style it was
    /// handed. That makes the selected state of a button with an explicit `.style(...)` invisible:
    /// the popover switchers were `Subtle` open and `Subtle` closed, and the shared component's
    /// own default was never the thing on screen.
    ///
    /// A source read is the only way to see this, because a selected style is a colour and a colour
    /// is not a bounds. The needle is assembled so this assertion cannot match itself.
    /// A toast is a statement about the cluster's state, and only some of them may interrupt.
    ///
    /// The `match` over a toast's severity is the one `Severity` `match` in this file, so this
    /// states what it is for. It partitions the variants into interrupting and not; it is not a
    /// threshold, because `Severity` carries no order (`Info` is declared after `Error`, and
    /// `Neutral` and `Muted` are quietness roles). A test that said "the most severe severity
    /// interrupts" would be asserting a comparison the type cannot make.
    #[test]
    fn only_a_toast_about_an_unfinished_thing_interrupts() {
        let toast = |severity| Toast {
            message: "Something happened.".to_owned().into(),
            severity,
            action: None,
        };
        for severity in [design::Severity::Error, design::Severity::Warning] {
            assert_eq!(
                Shell::toast_live_role(&toast(severity)),
                Role::Alert,
                "{severity:?}"
            );
        }
        for severity in [
            design::Severity::Success,
            design::Severity::Info,
            design::Severity::Neutral,
            design::Severity::Muted,
        ] {
            assert_eq!(
                Shell::toast_live_role(&toast(severity)),
                Role::Status,
                "{severity:?}"
            );
        }
    }
}
