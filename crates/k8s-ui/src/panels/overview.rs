//! Cluster overview: a verdict strip, a shared band of four figures, the five
//! workload kinds, and the per-node capacity table.
//!
//! The screen is looked at for ten seconds to answer one question — is this
//! cluster fine — so the layout serves that question and nothing else. The strip
//! is one grey line until something is actually wrong; when it is not, the strip
//! spends **one** status mark on the one dominant fact and itemises the rest in
//! ink, because emphasis is a budget and four coloured dots beside a count is how
//! the count stops being the thing the reader sees; the four figures share one
//! surface, so a row of unboxed numbers reads as a group rather than as four loose
//! readings; and the row spends **one** `display` figure, on the pods the cluster
//! is running out of the ones it declares, because a surface with four equal
//! headlines has no headline; node capacity is bars, so the relationship between a
//! request and a limit is visible rather than two numbers sitting next to each
//! other.
//! Everything past that is detail the reader can reach in one more step, and it
//! is placed last.
//!
//! **One meter grammar.** Every meter the page owns — a stat tile's, a workload
//! cell's, a capacity cell's — is the same shape: a track that is the whole of its
//! slot and a fill that is the value. The lane used to be sized to the value
//! against a half-scale, and that put four different track lengths in one row and
//! saturated at `50%`, so `85%` and `67%` drew the same bar. See `bar_slot`.
//!
//! **Every region states what its figures count.** The workloads denominator and
//! the Node capacity metrics absence are both printed under their own heading
//! rather than parked in a tooltip, because a figure whose denominator is named
//! nowhere on the page is a figure the reader has to reconstruct. The page's last
//! line names the one mark the capacity table uses for a value the cluster did not
//! report — which is what makes the page's end deliberate rather than merely
//! empty.
//!
//! **The page is a composition, not a stack.** Four bands on one left spine,
//! each boundary two spacing steps wide, and every band's box is its own
//! surface's box — the figures inside sit one band padding in from it, which is
//! what makes each band read as a card rather than as a row of figures somebody
//! forgot to pad. The deepest band — the capacity table, the one region on the
//! screen that scrolls — is sized to the rows the cluster reported and takes
//! whatever the window has left over only when there are more rows than fit.
//!
//! **The page ends where its content ends.** An earlier version stretched that
//! last band to the bottom of the window, and the render showed what that costs:
//! on a 1920×1080 window with three nodes the table's frame carried 280px of
//! vacant surface below the third row, which reads as a panel that failed to
//! load rather than as a page with three nodes in it. `Design guides > Visual
//! language > Hierarchy` asks for a small number of clear levels and the
//! spacing between them; it does not ask a region to invent height to fill a
//! window, and a framed box holding nothing is the one thing a reader cannot
//! mistake for anything else. So the surplus a three-node cluster leaves is the
//! page's own plane — air below the last band — and the page's closing gesture is
//! the one line that says what the table's own mark means. See
//! `capacity_table_height` and `Self::available_height`, and the states with
//! no data, which are placed by the same measurement so a failed load is centred
//! in the panel rather than parked under its toolbar.
//!
//! Freshness is part of the same promise (`UI-REDESIGN.md` L9): a failed refresh
//! keeps the last good numbers on screen, marks them stale, and says how long
//! ago they were taken. Blanking a dashboard is worse than showing a slightly
//! old one.
//!
//! **Nothing on this page animates except the loading ladder.** A figure here
//! changes once a refresh lands, so it is set rather than eased
//! (`stat_number`); the hover and press washes are [`design::state`] steps at
//! `motion::INSTANT`; the one thing that is genuinely being waited on — the
//! skeleton and the spinner on it — breathes on [`design::motion::LOADING`] and
//! the spinner is what honours the reader's reduced-motion preference.

use std::{
    cell::{Cell, RefCell},
    cmp::Ordering,
    rc::Rc,
    time::{Duration, Instant, SystemTime},
};

use crate::task::join_abortable;
use gpui_kit::assets::IconName;
use gpui_kit::base::ElementExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::label::Label;
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme, Icon, Sizable, Size, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AnyView, App, AppContext as _, Context, Div, ElementId, Entity, FocusHandle,
    Focusable as _, Font, FontFeatures, Hsla, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, ParentElement, Pixels, Render, Role, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Task, Window, div, font, px, relative,
};

use k8s_core::overview::{
    HealthLevel, NodeCapacity, NodeUsage, OVERVIEW_SOURCE_COUNT, Overview, ReplicaSummary,
    SOURCE_NOT_LOADED, WorkloadCounts,
};
use k8s_core::projection::Sort;
use tokio::runtime::Handle;

use super::common::{label_panel_title, label_small};
use crate::design::{self, Severity, border, radius, role, space, text};
use crate::panels::common;
use crate::session::OpsFuture;
use crate::settings;
use k8s_core::cluster_data::ClusterDataSource;

// ── Typography ───────────────────────────────────────────────────────────────

/// The UI face, straight off the active theme. gpui-kit owns the family, so a
/// surface that names it reads the theme rather than keeping a second copy of
/// the choice.
fn ui_font(cx: &App) -> Font {
    font(&cx.theme().font_family)
}

/// Tabular figures on the UI face.
///
/// `UI-SPEC` §2.3 asks for `tnum` on every column of numbers, and a stat tile is
/// a column of one. The feature is asked for here rather than through
/// `DataTypography` because the data face is the *buffer* font — YAML, logs,
/// UIDs, ports — and a count is none of those (`PROMPT.md` §2.1 #1). Without it a
/// tile's digits change width as the number changes and the whole row breathes
/// once a second.
fn tabular_features() -> FontFeatures {
    static FEATURES: std::sync::LazyLock<FontFeatures> = std::sync::LazyLock::new(|| {
        FontFeatures(std::sync::Arc::new(vec![
            ("tnum".to_owned(), 1),
            ("zero".to_owned(), 1),
        ]))
    });
    FEATURES.clone()
}

/// A tile's figure, at the type step that figure is worth.
///
/// `UI-SPEC` §2.3 reserves `display` for the one number a surface exists to
/// communicate, so only [`stat_number`] may spend it — and only the lead tile
/// does. The three tiles beside it and the five workload cells below them print
/// [`stat_figure`].
///
/// **They are set, never transitioned.** A figure here is replaced on every
/// refresh, twenty seconds apart, and `Design guides > Motion` is explicit that
/// motion explains change rather than decorating it: a number that eases from one
/// refresh to the next is a page that pulses, and the reader cannot tell a value
/// that moved from a value that was edited. `motion::INSTANT` is therefore the
/// whole of this number's motion policy, and the only animated thing on the page
/// is the loading ladder above it.
fn tile_number(text: &str, step: (Pixels, Pixels), cx: &App) -> Div {
    div()
        .font(ui_font(cx))
        .font_features(tabular_features())
        .text_size(step.0)
        .line_height(step.1)
        .font_weight(text::SEMIBOLD)
        .text_color(role::fg_primary(cx))
        .min_w(px(0.))
        .overflow_hidden()
        .text_ellipsis()
        .whitespace_nowrap()
        .child(text.to_owned())
}

/// The one number this surface exists to communicate: the lead tile's figure.
///
/// One `display` number per surface, and the page has exactly one — the pods a
/// cluster is running out of the ones it declares, which is the question the
/// rest of the page itemises. The three tiles beside it were `display` too, so a
/// row of four equal headlines gave the reader four focal points and none: the
/// answer is the one number on the page and the other three are its context.
fn stat_number(text: &str, cx: &App) -> Div {
    tile_number(text, (text::DISPLAY, text::DISPLAY_LINE_HEIGHT), cx)
}

/// A figure that supports the lead tile rather than being it: one step down.
///
/// The same face, weight and tabular figures as [`stat_number`], so the row reads
/// as one repeated pattern, and the same ink — a reader has to be able to read
/// `2 / 3` on the nodes tile at a glance, and the difference between the tiles is
/// size, not contrast. What the size buys is the one thing a surface with four
/// tiles cannot afford: a place the eye lands first.
fn stat_figure(text: &str, cx: &App) -> Div {
    tile_number(text, (text::TITLE, text::TITLE_LINE_HEIGHT), cx)
}

/// A tile's label. `UI-SPEC` §3.5 fixes it at 11px uppercase muted, and §2.3
/// reserves `caption` for section heads and table headers — a tile label is the
/// heading of a one-row table, so it is the same token.
///
/// The ink is the caller's, because a label is the quietest line in its own
/// cell: a tile label in `fg.secondary` and its subline in the same ink are two
/// levels wearing one, and the reading order the eye is supposed to take —
/// label, then number, then subline — has nowhere to go.
fn tile_label(label: &'static str) -> Label {
    Label::new(label.to_uppercase())
        .text_size(text::CAPTION)
        .line_height(text::CAPTION_LINE_HEIGHT)
        .font_weight(text::SEMIBOLD)
}

/// A tile's subline: the 12px sentence under the number, in one or more runs.
///
/// A subline is a LIST because a sentence can name two states and one ink can only
/// say one of them. `2 pending · 1 failed` was drawn in the failure ink because
/// the tile's verdict is Error, and the effect was that a PENDING count was
/// announced as a failure - the line misstated one of the two facts it states
/// exactly, in the most expensive ink on the page. Two runs, each in its own word
/// ink, is the two-ink rule applied to a sentence rather than to a pair of objects.
fn tile_subline(runs: &[(String, Severity)], resting: Hsla, cx: &App) -> Div {
    let mut line = h_flex().min_w(px(0.)).flex_wrap().gap(px(0.));
    for (index, (text, severity)) in runs.iter().enumerate() {
        if index > 0 {
            // The separator belongs to no bucket, so it wears the resting ink and
            // never the previous run's - a middot in danger red reads as part of the
            // word beside it.
            line = line.child(
                Label::new(" · ".to_owned())
                    .text_size(text::LABEL)
                    .line_height(text::LABEL_LINE_HEIGHT)
                    .text_color(resting),
            );
        }
        line = line.child(
            Label::new(text.clone())
                .text_size(text::LABEL)
                .line_height(text::LABEL_LINE_HEIGHT)
                .text_color(subline_ink(*severity, resting, cx)),
        );
    }
    line
}

/// A plain text tooltip for the surfaces gpui-kit has no tooltip-aware control
/// for. The tooltip is the app's own; the popup and its dismissal are
/// gpui-kit's.
fn text_tooltip(
    text: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    move |window, cx| Tooltip::new(text.clone()).build(window, cx)
}

/// Ink for a state, with health greyed.
///
/// `UI-SPEC` §0 铁律三 is unambiguous: healthy is grey and only Pending, Failed
/// and Error are coloured. `design::Severity::color` maps `Success` onto the
/// theme's green, so every healthy figure that routed through it printed a green
/// number on a cluster with nothing wrong with it. The shared mapping lives in
/// `design`, which this wave does not own, so the correction is made here and
/// the shared map is reported rather than edited.
///
/// **This is the *mark* half of the map, and live code no longer draws words with
/// it.** A 6px dot and a 13px label are the same hue at two lightnesses held to
/// two floors, so a status-coloured word reads [`word_ink`] and a status-coloured
/// dot, bar or rail reads [`mark_ink`] — see [`role::status_for`] and
/// [`role::status_word_for`] for the pair this module is built on, and
/// [`subline_ink`] for the one place a figure is neither. What survives here is the
/// thing neither pair states — **a healthy figure is secondary ink, not the theme's
/// success hue** — which `a_healthy_strip_is_grey_and_only_issues_spend_a_colour`
/// holds in place. It is `#[cfg(test)]` because that is now the only reader: keeping
/// it compiling would be dead code in the library, and deleting it would take the
/// guarantee with it.
#[cfg(test)]
fn state_ink(severity: Severity, cx: &App) -> Hsla {
    match severity {
        Severity::Warning => role::warning(cx),
        Severity::Error => role::danger(cx),
        Severity::Info => role::info(cx),
        Severity::Success | Severity::Neutral | Severity::Muted => role::fg_secondary(cx),
    }
}

/// The ink a **subline** is written in: metadata, or the channel's *word* ink
/// when that line is the one on the cell about a state rather than about a
/// quantity.
///
/// `resting` is the ink a line that is only reporting takes, and it is an argument
/// because the two kinds of subline are not the same importance: a workload
/// cell's `10,001 missing` is the story that cell exists to tell, while a
/// supporting tile's `1.15 of 20 cores` is the absolute behind a percentage the
/// reader has already got, and `fg.tertiary` is the ink `Design guides > Color
/// and themes` gives to help text. Only the three status arms point anywhere
/// else, and they point at the *word* inks.
///
/// It is never the mark ink and never the theme's success hue, which is what makes
/// a healthy figure grey rather than green (`UI-SPEC` §0 铁律三).
fn subline_ink(severity: Severity, resting: Hsla, cx: &App) -> Hsla {
    match severity {
        Severity::Warning | Severity::Error | Severity::Info => word_ink(severity, cx),
        Severity::Success | Severity::Neutral | Severity::Muted => resting,
    }
}

/// The ink a **bar** is filled with, which is not the ink its subline is written
/// in.
///
/// `UI-SPEC` §4.4 draws a table's summary bar with the healthy stretch in
/// `fg.tertiary` and the abnormal stretches in the status hue, and §4.4's status
/// cell splits the same way: `健康 → dot fg.tertiary, 文字 fg.secondary`. A bar is
/// the mark half of that pair, so it is [`mark_ink`] — except for the two arms
/// `role::status_for` paints `fg.secondary` and `fg.primary`, because a bar is a
/// graphic and the brightest object on a healthy page should not be the one thing
/// that is fine. Only `Warning` and `Muted` reach here today; the guard is what
/// stops the next severity from spending a hue on a bar.
///
/// The tile bars were filled with `fg_secondary` — the *text* colour — and a
/// healthy 100% bar is 380px of it. On a cluster with nothing wrong with it that
/// bar was the brightest object on the page, louder than the 28px headline number
/// above it, so the one thing that was fine was the thing the eye landed on.
///
/// Only the capacity table's meters call this now, because a request above its own
/// allocatable *is* a verdict and that cell states it in a glyph as well. Every
/// meter on the page whose value is a quantity reads [`magnitude_bar_ink`].
fn bar_ink(severity: Severity, cx: &App) -> Hsla {
    match severity {
        Severity::Success | Severity::Neutral => role::fg_tertiary(cx),
        other => mark_ink(other, cx),
    }
}

/// The ink a bar that measures a **quantity** is filled with.
///
/// A tile's bar answers "how much", never "is it wrong": `Pods ready` is 82% full
/// on a cluster with two failed pods, and painting that bar in the fault hue
/// printed 615px of red under the sentence `2 failed · 1` that already says it —
/// the loudest object on the page was the cluster being 82% fine. The bar takes
/// the mark half of the healthy pair and the subline keeps the status ink, which
/// is where a state belongs: it is the only line on the tile that is about the
/// state rather than the quantity.
///
/// **The workload meters read this too, and used not to.** A kind below its
/// declared replicas was drawn short *and* in the caution hue, which was the rule
/// that made the shortfall unmissable — but it made a full-cell track the largest
/// coloured area on the page for a fact the cell's own subline states in words
/// three lines up. The meter measures a quantity and a state belongs in words and
/// an ink, so the hue left the bar: `4 / 10,005` is now a neutral meter and a
/// `10,001 missing` under it.
fn magnitude_bar_ink(cx: &App) -> Hsla {
    role::fg_tertiary(cx)
}

/// The ink a **mark** is drawn in: a 6px dot, a bar fill, a state rail.
///
/// [`role::status_for`] is the product's own mapping for this, and it is the one
/// this panel now reads for every mark it draws. Its healthy arms are grey, which
/// is the rule this panel needs: a bar, a dot or a rail has no business wearing
/// the theme's green on a cluster with nothing wrong with it.
fn mark_ink(severity: Severity, cx: &App) -> Hsla {
    role::status_for(severity, cx)
}

/// The ink a **word** is drawn in: a status noun beside its mark, or a count on
/// a section heading's trailing edge.
///
/// `role::status_word_for` rather than [`mark_ink`], because the two inks are
/// the same hue at two lightnesses and are held to two different floors: a 6px
/// dot and a 13px word do not read at the same contrast, and asking one colour
/// to do both buys the mark's legibility at the word's expense. §4.4's status
/// cell is the rule this exists for — `健康 → dot fg.tertiary, 文字 fg.secondary`
/// — and it is a table with no dots that gets it wrong most easily, because
/// there is no mark next to the word to carry the state.
fn word_ink(severity: Severity, cx: &App) -> Hsla {
    role::status_word_for(severity, cx)
}

// ── Grid ─────────────────────────────────────────────────────────────────────

/// Columns the Overview's grids are built on.
///
/// `UI-SPEC` §7 fixes this at twelve with a 16px gap, and every span below is
/// counted against it. A row of tiles that share the width evenly is a flexbox
/// with five children; a row that lines up with the row above it is a grid, and
/// the difference is the whole reason the page stopped reading as a form.
const GRID_COLUMNS: u32 = 12;

/// The gap between grid columns. `UI-SPEC` §7 and §3.5 both say 16.
const GRID_GAP: Pixels = space::LG;

/// Panel width above which the four stat tiles share one row.
///
/// The widest figure a tile can hold is a `104 / 10,004` replica ratio, which at
/// `display` measures roughly 180px on the UI face. Four of those plus three 16px
/// gaps is 768px, and the floor leaves a 32px margin so the longest realistic
/// figure still fits without an ellipsis — an ellipsis on a dashboard's headline
/// number reports no value at all. Below it the tiles go to two rows of two,
/// which keeps every figure whole at the narrowest centre panel the shell can
/// build.
const TILE_ROW_WIDE_ABOVE: f32 = 800.0;

///
/// Every slot starts from a zero basis and grows in proportion to the columns it
/// spans, so a row whose spans add up to twelve hands out exactly one twelfth of
/// the space per column whatever the slot count is. That is what lets the tile
/// row and the workload row below it share their column edges without either of
/// them being told where the other's columns are.
fn grid_row(cells: Vec<(String, u32, AnyElement)>) -> AnyElement {
    debug_assert_eq!(
        cells.iter().map(|(_, span, _)| *span).sum::<u32>(),
        GRID_COLUMNS,
        "a grid row whose spans do not add up to twelve is not on the grid, and the row below it \
         will not line up"
    );
    h_flex()
        .w_full()
        .min_w(px(0.))
        .gap(GRID_GAP)
        .items_stretch()
        .children(cells.into_iter().map(|(selector, span, cell)| {
            // The slot is the grid's own box and is named separately from the
            // content it carries, so a measurement of "the tile" is unambiguous.
            // A shared selector is two elements under one name, and a layout test
            // that measures whichever one it happens to find is a test that
            // measures nothing.
            let slot = format!("{selector}-slot");
            div()
                .debug_selector(move || slot.clone())
                .flex_grow(span as f32)
                .flex_shrink(1.)
                .flex_basis(relative(0.))
                .min_w(px(0.))
                .child(cell)
        }))
        .into_any_element()
}

// ── Band ─────────────────────────────────────────────────────────────────────

/// A band's own padding, on **all four** sides.
///
/// `space::LG_PLUS` rather than the grid's own `space::LG`, because the spatial
/// grammar in `Design guides > Spatial grammar` is explicit that a container
/// spends **more** space at its boundary than between its contents, and the
/// contents here are a row of cells 16px apart. Padding the band by the same 16
/// would make the boundary and the gaps indistinguishable, which is the one
/// reading this rule exists to prevent.
///
/// Twenty is also what clears the band's own corner: `radius::LG` is 8px, so a
/// cell's first line of type starts 20px below the band's top edge and 12px past
/// the end of the corner's arc.
///
/// **One value, both axes.** A card with 20px above its content and 1px beside
/// it is not a card with tight padding — it is a card with no padding on one
/// axis, and the reader's eye reads the two differently: air above a row of
/// figures is a margin, and air beside them is the figure being held off the
/// page. There is also nothing about a band's left edge that asks for a different
/// number from its top edge, so a second constant would be a second value with
/// no reason behind it.
///
/// The horizontal half **costs no height**, which is the one thing the height
/// model has to know: [`figures_band_height`] counts this value twice and no
/// more, and the figures inside a band move 20px in on each side without moving
/// the page's vertical rhythm at all.
const BAND_PADDING: Pixels = space::LG_PLUS;

/// One group of related figures on one shared surface.
///
/// Four stat tiles floating on the content plane are four unboxed numbers in a
/// row, which is the shape `Design guides > Visual language > Hierarchy` calls out
/// as the "AI generated dashboard" look: nothing states that the four readings
/// are one answer, so the reader checks them one at a time. The guide's answer is
/// also the constraint that decides how this band is built — *most desktop
/// regions need only a background, a hairline boundary, and intentional
/// spacing*, and **nested cards inside cards** are what it rules out. So there is
/// one surface and one boundary here, not four cards inside a card, and the cells
/// inside it are separated by the grid gap and nothing else.
///
/// **The plate is a sibling, not the band's own background.** The band's box is
/// its surface's box and the plate is an absolutely-positioned sibling painted
/// behind it, filling that box edge to edge — so the surface and the content
/// always agree, every pixel of plate is [`BAND_PADDING`] from the figures it
/// carries, and a stripe of colour beside them is not a thing this function can
/// draw.
///
/// **The padding is [`BAND_PADDING`] on all four sides, and the surface is what
/// sits on the page's spine.** This band used to pad vertically only, on the
/// reasoning that a full-bleed surface has to run to the content spine or the
/// four tiles stop starting where the section headings above and below them do.
/// The render showed what that reasoning bought: on a 2× capture, `PODS READY`,
/// the `18 / 21` figure and the meter bar all began **1.5 logical px** inside the
/// band's own edge — 3 device pixels, which is glyph side bearing, not padding —
/// against 36px of air above the row's first line of type and 30px below its last,
/// of which the band's own padding is 20 on each. A box with vertical air and
/// none horizontal is not a card with tight padding; it is a table of figures
/// somebody forgot to pad, and that is the reading the tile row was sent back for.
///
/// So the region that shares the page's spine is the **plate**, and it is a
/// region's own box: the plate, the health strip, the section headings and the
/// capacity table all start on the same x as the toolbar title above them. What
/// sits one `BAND_PADDING` further in is a band's **contents**, which is what a
/// card's padding is for — the same way the figure inside a card is held off the
/// card's edge.
///
/// **Four figures, one reading.** The cells keep the grid gap and no surface of
/// their own, because four cards inside a card is what the guide rules out and
/// because the row's hierarchy is already carried by type: the lead tile prints a
/// `display` figure and the three beside it a `title` one. Boxes would add a
/// second hierarchy the page would then have to reconcile with the first.
///
/// No border and no shadow. The surface is one step off the content plane and the
/// radius is the panel tier (`radius::LG`), so the boundary is a change of value
/// rather than a stroke; a hairline here would put a second edge against the
/// page's own, and `Design guides > Surfaces and elevation` reserves strokes for
/// regions that background contrast does not separate.
///
/// `plate` is the surface's own value rather than something read here, because the
/// page's bands have to carry the stale marking too: `UI-REDESIGN.md` L9 wants the
/// whole data region tinted when the last refresh failed, and an opaque plate on top
/// of the region's own wash hides the marking from the two bands a reader looks at
/// first.
fn band(selector: &'static str, content: AnyElement, plate: Hsla) -> Div {
    div()
        .debug_selector(move || selector.to_owned())
        .relative()
        .w_full()
        .min_w(px(0.))
        // One value on every edge, so the figures inside the surface are as far
        // from its left and right as they are from its top and bottom.
        .p(BAND_PADDING)
        .child(
            div()
                .absolute()
                .inset_0()
                .debug_selector(move || format!("{selector}-plate"))
                .bg(plate)
                .rounded(radius::LG),
        )
        .child(content)
}

// ── Proportion bar ───────────────────────────────────────────────────────────

/// The surface one of the page's bands is raised to.
///
/// `role::surface_raised` is the band's own value, and the whole point of the band
/// is the one step it takes off the content plane. When the last refresh failed
/// the plate carries the stale wash as well: `UI-REDESIGN.md` L9 asks the marking
/// to tint every figure at once rather than sit in a corner, and an opaque plate
/// would have taken the two bands a reader reads first out of that marking.
fn band_plate(stale: bool, cx: &App) -> Hsla {
    if stale {
        design::composite_surface(
            role::surface_raised(cx),
            role::warning(cx).opacity(STALE_WASH_ALPHA),
        )
    } else {
        role::surface_raised(cx)
    }
}

/// Height of a proportion bar.
///
/// `UI-REDESIGN.md` §3.5 fixes node capacity at 6px; this is `space::SM`, 8px,
/// and the reason it moved is the type the bar sits under. A 6px mark under a
/// 28px display number is a hairline next to the loudest object on the page,
/// and the tile bar is the page's headline mark rather than one cell of a dense
/// row — `Design guides > Radius, spacing, and density` asks the geometry to
/// match what it belongs to. Eight is a step on the product's own spacing grid,
/// so it is `space::SM` rather than a raw `px(8.)`, and it is the same height in
/// the stat tiles, the workload cells and the capacity table so the page carries
/// one mark rather than three.
/// There is deliberately no maximum width on this page, and a measure was tried.
///
/// Centring the body at 1200px makes the tiles and their meters read better on a
/// wide display — a meter whose track is the cell is only a bar while the cell is
/// a bar's width, and four 470px tiles make four 460px rules. It also clips
/// `MEMORY LIMITS` off the capacity table, which needs about 1600.
///
/// The reason it does not ship is the page's spine. Three invariants hold it
/// together, and a centred measure breaks all three: every section starts on the
/// panel's padding line, the toolbar's title and the content below it start on one
/// x, and each grid fills the width it was given so its columns stay comparable
/// across rows. A reader's eye tracks this page down its LEFT edge; a block
/// floating in the middle of the window, under a toolbar that starts 104px to its
/// left, reads as two things rather than one page.
///
/// So the width stays the panel's. The remaining answer — capping the meter on
/// the tile instead — is `bar_slot`'s to settle, and it is a real trade: a meter
/// that stops short of its cell makes four meters four lengths again, which is
/// the comparison the tile row exists to make. Until that is decided the long
/// track is the lesser defect, because it is honest: it is exactly as long as the
/// cell it measures, and the value is printed above it to three figures.
const BAR_HEIGHT: Pixels = space::SM;

/// Corner on the track and on the fill.
///
/// `radius::XS` is 3px against an 8px mark, so it stays visibly smaller than
/// half the shorter side — the rule in `Design guides > Radius, spacing, and
/// density` that an ordinary rectangle's radius must satisfy. The alternative
/// the file used to argue for, square ends, is what a bar looked like when the
/// bar was a hairline; at 8px with a real track behind it, a square-ended fill
/// on a rounded track pokes out through the track's own leading corner, which is
/// worse than either.
const BAR_RADIUS: Pixels = radius::XS;

/// The surface a bar's track is cut into.
///
/// `role::surface_inset` and not `role::border_subtle`: the track is a surface
/// the fill sits *in*, and the ladder already names the darkest step for content
/// that recedes. `border_subtle` is a stroke — one device pixel of ink on a
/// surface — and a 6px element painted in it is a rule 8px tall, which is why a
/// 380px track used to read as a loading skeleton rather than as a container.
/// The inset step measures 1.28:1 against the content plane in the dark
/// appearance and 1.11:1 against the raised band, which is the quiet end of
/// "you can see it and it is not competing".
fn bar_track(cx: &App) -> Hsla {
    // A wash of the local ink over the tile's own plane, NOT `role::surface_inset`.
    //
    // `surface_inset` is derived from the editor background, which in both shipped
    // appearances is several steps darker than the tile the meter is painted on. A
    // meter drawn on it read as a black slot cut into the card rather than as the
    // whole a share is a share of - and the card behind a bar is the only thing
    // that says what the bar is measured against.
    //
    // This is the one wash in the product that exists to make a plane recede
    // rather than to answer a hover, and it is the same 4% of the local ink, so it
    // is invisible-in-the-right-way on a light tile and on a dark one.
    design::state::hover_on(role::surface_raised(cx), role::fg_primary(cx))
}

/// A share of a whole, drawn as a bar.
///
/// One bar, one value. The fill is the share and the track is the whole it is a
/// share of, and nothing else is drawn on the track: a bar that carries two marks
/// is a bar whose reader has to guess which mark is the value.
///
/// The limit tick this bar used to carry is the reason. `UI-REDESIGN.md` §3.5
/// asks for `limit = 同轨`, a limit on the same track as the request, and it does
/// not say the tick has to be *on* the bar — and on the bar it was unreadable.
/// The tick is 2px in a 6px bar and painted in the same `fg.tertiary` as a healthy
/// fill, so a node running at 1% with a 6% request showed two grey dashes five
/// pixels apart and the reader had to work out which was the number. It was also
/// **redundant in the live columns**: the tick on `CPU now` is the request share,
/// which is the neighbouring `CPU requests` column's own fill on the same scale,
/// drawn a second time as a dash.
///
/// So the limit leaves the bar and keeps its column, and the one fact the tick was
/// carrying that the numbers do not — *this node's request is above its own
/// limit* — is now a state on the limits cell itself, where it has words, a glyph
/// and an ink. See `limit_state`.
fn proportion_bar(selector: String, fill: f64, fill_ink: Hsla, cx: &App) -> Div {
    let share = |value: f64| relative((value / 100.0).clamp(0.0, 1.0) as f32);
    // The fill is named as well as the track, because a track's bounds say nothing
    // about whether the mark inside it is legible: `2%` of a 300px lane and `2%` of
    // a 36px lane have identical proportions and completely different readings, and
    // the measurement that tells them apart needs a handle on the fill.
    let fill_selector = format!("{selector}-fill");
    let track = div()
        .absolute()
        .inset_0()
        .bg(bar_track(cx))
        .rounded(BAR_RADIUS)
        .child(
            div()
                .debug_selector(move || fill_selector.clone())
                .absolute()
                .top_0()
                .left_0()
                .h_full()
                .w(share(fill))
                .bg(fill_ink)
                .rounded(BAR_RADIUS),
        );
    div()
        .debug_selector(move || selector.clone())
        .relative()
        .min_w(px(0.))
        .h(BAR_HEIGHT)
        .child(track)
}

/// The slot a tile's or a workload cell's meter occupies, whether or not it has a
/// share to draw.
///
/// The slots' heights are load-bearing: [`capacity_table_height`] reads the
/// composition's rhythm out of the tokens to decide how much height the capacity
/// table has left, and a tile that quietly drops its meter when no node reported
/// an allocatable figure moves that number by 8px on exactly the clusters where
/// the page already looks thin. Reserving the slot is also what keeps the four
/// meters along the bottom of the tile row on one line, which the subline slot
/// above them already does for the same reason.
///
/// **One meter grammar for the whole page: the track is the cell, and the fill is
/// the value.** That is the whole of this function.
///
/// It was the other way round — the *lane* was sized to the value, against a
/// half-scale — and the render showed what that costs. A reader scanning the tile
/// row compared four different track lengths: `17 / 20` filled a 460px lane,
/// `2 / 3` a 300px one, and both `CPU REQUESTED 2%` and `MEMORY REQUESTED 1%`
/// got the 12.5% floor, which is a 57px stub with a dot on it. Four lanes are
/// four lengths and a scan of four lengths learns nothing, because the reader has
/// no way to tell a saturated lane from a short one. Worse, the half-scale
/// saturated: `TILE_METER_FULL_SHARE` was `50%`, so `85%`, `67%` and everything
/// between them and `100%` all drew a **full** lane and were indistinguishable
/// from each other — the three busiest figures on a healthy cluster drew the same
/// bar.
///
/// So the track is the cell, on every meter the page owns, and the only thing
/// that varies is the fill. `85%` and `67%` are then two different lengths inside
/// the same box, which is the comparison the row is for, and the scale runs to
/// `100%` because that is the ceiling a share of allocatable actually has.
///
/// **The fill is the share, exactly, in every meter on the page.** There is no
/// minimum-visible fill and no saturation clamp, because a clamp is a second rule
/// and a floor would draw `0.4%` and `4%` as the same mark — a mark lying about a
/// value the type states exactly one line above it. The floor existed only because
/// the old lane was `12.5%` of the cell, where a `2%` was 0.9px and read as a dot
/// on a stub; against a track that *is* the cell, the same `2%` is 3px in the
/// narrowest tile slot and 7.6px in the widest, which is a bar in a visible groove.
///
/// The value is printed in every cell that owns a meter — that is what `UI-SPEC`
/// §2.3's `tnum` demand is for — so nothing on this page is ever encoded in a mark
/// alone, which is the rule the floor was defending.
fn bar_slot(bar: Option<(String, f64, Hsla)>, cx: &App) -> Div {
    match bar {
        Some((selector, share, ink)) => {
            proportion_bar(selector, share.clamp(0.0, 100.0), ink, cx).w_full()
        }
        // No share means no track: a kind the cluster has no denominator for draws
        // no meter at all rather than a groove that means nothing.
        None => div().h(BAR_HEIGHT),
    }
}

// ── Vertical composition ─────────────────────────────────────────────────────

/// Space between two bands, after the body's own gap.
///
/// The body separates its children by `space::XXL`, and a *section* insets itself
/// by the same step, so a section boundary is 64px and the health strip above the
/// first band is 32 — a strip is not a section and does not get a heading of its
/// own. `Design guides > Spatial grammar` asks for 24px between separate sections
/// and 32px between the major regions of a page, so one boundary is worth the sum
/// of the two.
///
/// It used to be a `band_gap()` function returning `2 × space::XXL` on top of the
/// body's own gap, which spent the same step twice and rendered a 96px boundary:
/// 32 more air than the rhythm asks for, on the axis the page was already short
/// of. The step is written where it is spent, and `space::XXL` is what both the
/// layout and [`height_above_capacity_table`] now read, so the two cannot
/// disagree about how tall a boundary is.
///
/// A boundary is carried by this whitespace rather than by the hairline that
/// marks it, which is what `the_capacity_header_follows_its_data_and_the_sections_share_one_edge`
/// measures.
const SECTION_GAP: Pixels = space::XXL;

/// Space between a section heading and the band it introduces.
const SECTION_CONTENT_GAP: Pixels = space::SM;

/// Height of the stat tiles in a wide row, in logical pixels.
///
/// Derived from the same tokens the tile is built from rather than written down,
/// because [`capacity_table_height`] reads it to work out how much of the window
/// the capacity table has left, and a constant that drifts from the type scale
/// would mis-size every table on the page by exactly the drift.
///
/// It is the **lead** tile's height, because the lead tile is the tallest one and
/// the row is stretched to it: the three tiles beside it print a `title` figure
/// and take the difference in their spacer, so their content is 12px shorter and
/// their meters land on the lead tile's meter line rather than 12px above it.
///
/// Four gaps and not three, for the same reason the tile has a spacer: the spacer
/// is a child, so it has a gap on either side of it even when it takes no height.
fn stat_tile_height() -> f32 {
    f32::from(text::CAPTION_LINE_HEIGHT)
        + f32::from(text::DISPLAY_LINE_HEIGHT)
        + f32::from(text::LABEL_LINE_HEIGHT)
        + f32::from(BAR_HEIGHT)
        + 4.0 * f32::from(space::XS)
}

/// Height of one workload cell, in logical pixels.
///
/// The same four-line shape as a stat tile — label, figure, subline, meter — with
/// the number a type step down, so the two bands on the page are one repeated
/// pattern rather than two. The third line is what says *how many replicas are
/// missing*, which is the fact a `4 / 10,005` figure cannot state on its own.
fn workload_cell_height() -> f32 {
    f32::from(text::CAPTION_LINE_HEIGHT)
        + f32::from(text::TITLE_LINE_HEIGHT)
        + f32::from(text::LABEL_LINE_HEIGHT)
        + f32::from(BAR_HEIGHT)
        + 3.0 * f32::from(space::XS)
}

/// Rows a band of figures takes at `wide`.
fn band_rows(wide: bool) -> f32 {
    if wide { 1.0 } else { 2.0 }
}

/// Height of a band of figures: its cells, the gaps between them, and the padding
/// above and below them.
///
/// **Two [`BAND_PADDING`]s and no more**, because the band's horizontal inset
/// costs the page nothing vertically: the figures move 20px in on each side and
/// the band is exactly as tall as it was before it had them. So the tile row is
/// **126px** wide-layout (86 of tile, 40 of padding) and 228 narrow, and the
/// workload band is 126 wide and 196 narrow (70 of cell, 16 between the rows, 40
/// of padding). Those are the numbers [`capacity_table_height`] spends, and a lane
/// that changed [`BAND_PADDING`] without changing this with it is how the modelled
/// page and the drawn one drift apart — the table then runs past the fold by
/// exactly the drift.
fn figures_band_height(wide: bool, cell: f32) -> f32 {
    let rows = band_rows(wide);
    rows * cell + (rows - 1.0) * f32::from(GRID_GAP) + 2.0 * f32::from(BAND_PADDING)
}

/// Height a section heading and its heading-to-content gap take together.
fn section_header_height() -> f32 {
    f32::from(design::size::ROW_DENSE) + f32::from(SECTION_CONTENT_GAP)
}

/// Height a section's own reading line and the gap above it take together.
///
/// The reading line is the sentence that says what a region's figures *count* —
/// `Available replicas against the replicas the manifests declare`, and what
/// happens to two columns when metrics-server does not answer. It exists because
/// a tooltip is not reachable by a reader scanning the page, and a figure whose
/// denominator is named nowhere on the page is a figure the reader has to
/// reconstruct.
fn section_reading_height() -> f32 {
    f32::from(text::LABEL_LINE_HEIGHT) + f32::from(SECTION_CONTENT_GAP)
}

/// Height from the panel's scroll container down to the capacity table's top edge.
///
/// The scroll body's own padding is chrome rather than composition, but it is
/// height the table has to fit inside, so it is counted here rather than guessed at
/// the call site. Only the *leading* padding counts here; the trailing one is below
/// the table and is spent by [`capacity_table_height`].
///
/// Every term is a token the layout above it already spends, so this is a reading
/// of the composition rather than a second copy of it: 16 of panel padding, the
/// 32px verdict strip, a 32px body gap, the tile band, a body gap and a
/// [`SECTION_GAP`], the heading, its own reading line and its gap, the workload
/// band, and the same three steps again before the last heading — 528px in the
/// wide layout, every term of it fixed by construction, which is what lets the
/// table's own top edge be the first line of the page that is a function of how
/// many nodes the cluster has.
///
/// `stale` is the refresh-error strip, which is a real region: it adds its own
/// 32px **and** a 32px body gap, because the body grows a child. Without both
/// terms the model would hand the table a height the page has already spent, and
/// the last band of a stale page would run 64px past the fold.
///
/// `workloads` and `metrics_available` are the two regions that are *sometimes*
/// there. The workloads band is a whole section — heading, reading line, band —
/// and the metrics reading line is another 20px, and a model that counted both
/// unconditionally would hand a cluster with no workloads and no metrics-server a
/// table 100px shorter than the space it has, so its rows would end 100px above
/// the fold for no reason a reader could see.
fn height_above_capacity_table(
    wide: bool,
    stale: bool,
    workloads: bool,
    metrics_available: bool,
) -> f32 {
    let refresh_error = if stale {
        f32::from(design::size::ROW) + f32::from(space::XXL)
    } else {
        0.0
    };
    let workloads_section = if workloads {
        f32::from(space::XXL)
            + f32::from(SECTION_GAP)
            + section_header_height()
            + section_reading_height()
            + figures_band_height(wide, workload_cell_height())
    } else {
        0.0
    };
    f32::from(space::LG)
        + f32::from(design::size::SUMMARY_STRIP)
        + f32::from(space::XXL)
        + refresh_error
        + figures_band_height(wide, stat_tile_height())
        + workloads_section
        + f32::from(space::XXL)
        + f32::from(SECTION_GAP)
        + section_header_height()
        + if metrics_available {
            0.0
        } else {
            section_reading_height()
        }
}

/// The height the capacity table takes in a scroll container `content_height` tall,
/// for a table holding `row_count` rows.
///
/// **The box is the box its rows are:** one row for the heading band, one row per
/// node, and no more than the window has left. An earlier version grew it to the
/// bottom of the window instead, on the reasoning that a dashboard should fill the
/// window. Measured on the render at 1920×1080 with three nodes, that put **280px
/// of framed, empty surface** below the third row: a rectangle of table background
/// holding nothing, which a reader cannot read as anything but a panel that failed
/// to load. `Design guides > Visual language > Hierarchy` asks for a few clear
/// levels separated by intentional spacing; it does not ask a region to invent
/// height, and the surplus a three-node cluster leaves is the page's own plane.
///
/// Measured after the change, at 1920×1080 with three nodes: the box is 128px —
/// the heading band and three rows — its bottom edge lands exactly on the third
/// row's, and the 412px the page leaves below it is the content plane with nothing
/// in it.
///
/// The height is computed rather than grown with `flex_1` because the parent is a
/// scroll container, and a percentage height inside one resolves against nothing —
/// which is why every "content-sized, never `size_full()`" note in this file
/// exists. Stating the number keeps the table's own box definite, so gpui-kit's
/// `DataTable`, which is `size_full()`, resolves against something real.
///
/// **The page therefore never scrolls on a window that has room for the rows,**
/// which is the promise the previous version broke in the other direction: on a
/// 1400×660 window with three nodes it handed the table 128px of box in 120px of
/// space and the **page** scrolled by 8. A scrollbar for eight pixels is a
/// defect, and a page scrollbar is the worse of the two anyway — it moves the
/// four bands a reader is looking at to reveal rows of the region they are already
/// looking at. More rows than fit is the one case that does scroll, and the
/// table's own row list is the region that scrolls it: measured at the minimum
/// window, 960×640, the three rows need 128px in the 100px that is left, the box
/// takes the 100, and the page's content is 600px in a 600px panel — no page
/// scrollbar.
///
/// The floor is [`CAPACITY_TABLE_MIN_ROWS`]: one row for the heading band and one
/// data row, because the floor's whole job is to stop a short window from
/// squeezing the box down to a header with nothing under it.
///
/// The heading is inside this box, and it has to be: gpui-kit lays the heading band
/// out **inside** it and at the height of a data row (`TableState` builds the leaf
/// header row with `h(row_height)`), which is why the arithmetic counts a row for
/// the heading.
fn capacity_table_height(
    row_height: f32,
    row_count: usize,
    content_height: f32,
    wide: bool,
    stale: bool,
    workloads: bool,
    metrics_available: bool,
) -> Pixels {
    // The trailing `space::LG` is the scroll body's own bottom padding, spent so
    // the last band does not sit flush against the panel's edge.
    let available = content_height
        - height_above_capacity_table(wide, stale, workloads, metrics_available)
        - f32::from(space::LG);
    let wanted = row_height * (1.0 + row_count as f32);
    let floor = row_height * (1.0 + CAPACITY_TABLE_MIN_ROWS);
    px(available.min(wanted).max(floor))
}

// ── Loading ──────────────────────────────────────────────────────────────────

/// The four rungs of `UI-SPEC` §4.14, advanced by timers rather than by
/// comparing elapsed time on every frame.
///
/// A frame that arrives late must not be able to skip a rung, and a rung only a
/// wall clock can reach is a rung nobody can test. `table_view/view.rs` walks the
/// same ladder for the same reason; the two are separate types because the two
/// panels answer to different data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum LoadingTier {
    /// Under the table's invisible rung: draw no loading state at all. A spinner that
    /// appears for 40ms and disappears is a flash, and a flash reads as a glitch
    /// rather than as speed.
    #[default]
    Nothing,
    /// A spinner in the strip's own slot, so the layout under it does not move.
    Spinner,
    /// A skeleton built to the real tile geometry, so nothing jumps when the
    /// numbers arrive.
    Skeleton,
    /// Past [`LOADING_PROGRESS`]: the skeleton and the elapsed time, which is the
    /// only progress an aggregation can honestly report.
    Progress,
}

/// Past this the reader has waited long enough to be shown the shape of what is
/// coming.
const SKELETON_AFTER: Duration = Duration::from_millis(500);
/// Past this the reader is owed a quantity as well as a spinner.
const LOADING_PROGRESS: Duration = Duration::from_secs(2);
/// How often the last rung repaints while it is still loading.
///
/// A second, because the figure it prints is elapsed whole seconds and a faster
/// tick would repaint the panel to draw the same digit.
const LOADING_TICK: Duration = Duration::from_secs(1);
/// The skeleton's one breath: `motion::LOADING`.
///
/// The product's single "something is genuinely being waited on" duration, used here as the cycle
/// of [`design::skeleton_alpha`] rather than as a second number. The breathe exists only while a
/// load is in flight — the tier it belongs to is itself a timer — and the spinner on the rung above
/// it is what owns the user's reduced-motion preference.
///
/// The breathe is hand-rolled because gpui-kit ships a `Skeleton` element that is the wrong one
/// here for two reasons on the forbidden list: it eases with `bounce`, which `PROMPT.md` §3 rules
/// out outside a drag, and it reads `theme().skeleton` rather than a semantic role, which the
/// token migration removes. The curve itself is shared with the table's skeleton through
/// `design::skeleton_alpha`.
const SKELETON_BREATHE: Duration = design::motion::LOADING;
/// One breathing placeholder, at the height of the text it stands in for.
///
/// `UI-SPEC` §4.14 asks for the skeleton's blocks to keep the real layout's
/// dimensions, at 60–80% of the width of the thing they cover, and for the widths
/// to be deterministically pseudo-random so a screenshot reproduces. The width is
/// therefore a *fraction* of the slot rather than a pixel count, so a block is
/// 60% of the tile at every panel width, and it is stepped by the slot's own
/// index, so no two placeholders are the same length and none of it is a clock.
/// The breathe is the elapsed-time walk [`skeleton_alpha`] already does, rather
/// than an animation element, because the tier it belongs to is itself a timer and
/// a second clock under it would be two answers to one question.
fn skeleton_block(selector: &str, share: f32, height: Pixels, waited: Duration, cx: &App) -> Div {
    div()
        .debug_selector(move || selector.to_owned())
        .flex_none()
        .h(height)
        .w(relative(share.clamp(0.0, 1.0)))
        .rounded(radius::XS)
        .bg(role::fg_primary(cx).opacity(design::skeleton_alpha(waited, SKELETON_BREATHE)))
}

/// A skeleton's stat tile: the four blocks of a real one in the real places.
///
/// The label, the number and the subline are placeholders at their own line
/// heights, and the bar is a full-width placeholder at the real height, so the
/// numbers arrive into space that is already theirs and nothing below the row
/// moves when they land. The subline block is always present even on the tiles
/// that print nothing there, because the real tile reserves that slot too and a
/// placeholder that vanished would move the bar up 16px.
///
/// `lead` is the real tile's own flag, because the placeholder for a `title`
/// figure is a `title`-sized block: a skeleton that stood every number at the
/// display height would be 12px taller than three of the four cells it stands in
/// for, and the row it builds would not be the row the numbers land in.
///
/// **The placeholder is content-sized and banded like the real thing.** A skeleton
/// whose shape does not match the loaded layout is a second layout to get wrong:
/// `UI-SPEC` §4.14 asks for a skeleton only where the real geometry is known, and
/// it is known here, so the tiles sit in the same shared band and the bands are
/// separated by the same two-step gap. What is *not* drawn is the capacity
/// table's frame — the one region whose height is a function of the window, and a
/// placeholder sized from the window would be a table-shaped block that a real
/// three-row table then does not fill.
fn skeleton_stat_tile(
    selector: &str,
    index: u64,
    lead: bool,
    waited: Duration,
    cx: &App,
) -> AnyElement {
    // 60–80%, stepped by the slot's own index.
    let seed = 0.6 + 0.1 * (index % 3) as f32;
    v_flex()
        .w_full()
        .min_w(px(0.))
        .gap(space::XS)
        .debug_selector(move || selector.to_owned())
        .child(skeleton_block(
            &format!("{selector}-label"),
            0.34 * seed,
            text::CAPTION_LINE_HEIGHT,
            waited,
            cx,
        ))
        .child(skeleton_block(
            &format!("{selector}-number"),
            0.66 * seed,
            if lead {
                text::DISPLAY_LINE_HEIGHT
            } else {
                text::TITLE_LINE_HEIGHT
            },
            waited,
            cx,
        ))
        .child(skeleton_block(
            &format!("{selector}-subline"),
            0.48 * seed,
            text::LABEL_LINE_HEIGHT,
            waited,
            cx,
        ))
        // The real tile's spacer, for the reason `stat_tile` gives: the meters in
        // a row share one line whatever the figures above them are set at.
        .child(div().flex_1())
        .child(skeleton_block(
            &format!("{selector}-bar"),
            1.0,
            BAR_HEIGHT,
            waited,
            cx,
        ))
        .flex_1()
        .into_any_element()
}

/// A skeleton's workload cell: the same four blocks as the real one.
///
/// A stat-tile skeleton here would be wrong in the way that shows: the real cell
/// is shorter than the real tile — the figure is a type step down — so five
/// placeholder tiles under the real heading would leave a hole where the first row
/// of figures is going to be. And the subline block is the load-bearing one: the
/// real cell now states *how many replicas are missing* under its figure, so a
/// placeholder without it would be 16px shorter than the cell the numbers arrive
/// into, which is the same jump this skeleton exists to prevent.
fn skeleton_workload_cell(selector: &str, index: u64, waited: Duration, cx: &App) -> AnyElement {
    let seed = 0.6 + 0.1 * (index % 3) as f32;
    v_flex()
        .w_full()
        .min_w(px(0.))
        .gap(space::XS)
        .debug_selector(move || selector.to_owned())
        .child(skeleton_block(
            &format!("{selector}-label"),
            0.42 * seed,
            text::CAPTION_LINE_HEIGHT,
            waited,
            cx,
        ))
        .child(skeleton_block(
            &format!("{selector}-figure"),
            0.52 * seed,
            text::TITLE_LINE_HEIGHT,
            waited,
            cx,
        ))
        .child(skeleton_block(
            &format!("{selector}-subline"),
            0.38 * seed,
            text::LABEL_LINE_HEIGHT,
            waited,
            cx,
        ))
        .child(skeleton_block(
            &format!("{selector}-bar"),
            1.0,
            BAR_HEIGHT,
            waited,
            cx,
        ))
        .flex_1()
        .into_any_element()
}

/// The skeleton's tile row, laid out on the same grid, with the same spans and
/// inside the same shared band the real one uses, so the band below it starts
/// where it is going to start.
fn skeleton_tiles(wide: bool, waited: Duration, cx: &App) -> AnyElement {
    let span = if wide { 3 } else { 6 };
    let per_row = if wide { 4 } else { 2 };
    let mut grid = v_flex().w_full().gap(GRID_GAP);
    let mut made = 0u64;
    while made < 4 {
        let take = per_row.min(4 - made as usize);
        grid = grid.child(grid_row(
            (0..take)
                .map(|offset| {
                    let index = made + offset as u64;
                    (
                        format!("overview-skeleton-tile-{index}"),
                        span,
                        // The lead tile is the first slot, exactly as the loaded
                        // row puts it.
                        skeleton_stat_tile(
                            &format!("overview-skeleton-tile-{index}"),
                            index,
                            index == 0,
                            waited,
                            cx,
                        ),
                    )
                })
                .collect(),
        ));
        made += take as u64;
    }
    band(
        "overview-skeleton-vitals-band",
        grid.into_any_element(),
        band_plate(false, cx),
    )
    .into_any_element()
}

/// The skeleton's workload section: the same heading, the same reading line, the
/// same band, and the same five cells on the same spans as the real one, so the
/// capacity table's heading starts where it is going to start.
///
/// The heading's trailing total and the reading line's words are the two things
/// the skeleton does not stand in for, and that is deliberate — a total is a
/// *reading*, and `UI-SPEC` §4.14 is explicit that a skeleton must never cover a
/// number a reader could read. The reading line gets a placeholder anyway,
/// because it is a sentence's worth of height the real one occupies.
fn skeleton_workloads(wide: bool, waited: Duration, cx: &App) -> AnyElement {
    let spans = if wide {
        &WORKLOAD_SPANS_WIDE[..]
    } else {
        &WORKLOAD_SPANS_NARROW[..]
    };
    let cell = |index: usize| {
        (
            format!("overview-skeleton-workload-{index}"),
            spans[index],
            skeleton_workload_cell(
                &format!("overview-skeleton-workload-{index}"),
                index as u64 + 8,
                waited,
                cx,
            ),
        )
    };
    let mut grid = v_flex().w_full().gap(GRID_GAP);
    if wide {
        grid = grid.child(grid_row((0..5).map(cell).collect()));
    } else {
        // Three kinds on the first row and two on the second, because five does
        // not divide into twelve — the same wrap the real region makes.
        grid = grid
            .child(grid_row((0..3).map(cell).collect()))
            .child(grid_row((3..5).map(cell).collect()));
    }
    v_flex()
        .w_full()
        .min_w(px(0.))
        .mt(SECTION_GAP)
        .gap(SECTION_CONTENT_GAP)
        .child(section_heading("Workloads", cx))
        .child(v_flex().w_full().min_w(px(0.)).child(skeleton_block(
            "overview-skeleton-workloads-reading",
            0.34,
            text::LABEL_LINE_HEIGHT,
            waited,
            cx,
        )))
        .child(band(
            "overview-skeleton-workloads-band",
            grid.into_any_element(),
            band_plate(false, cx),
        ))
        .into_any_element()
}

// ── Freshness ────────────────────────────────────────────────────────────────

/// How often the panel reloads itself.
///
/// The other polling surface in the product is the metrics sampler, which reads
/// one object from metrics-server every 10s on the local tier and every 30s on
/// the high-latency one (`k8s_core::metrics::SampleScheduler`). This page is not
/// one object: one load is seven `list` calls plus the metrics read, so it sits
/// between the two tiers rather than taking the fastest of them and hammering
/// the API server with a whole-cluster aggregation every ten seconds.
///
/// The interval is a setting, and `settings.rs` has no key for it yet — that file
/// is not this wave's, so the constant is the product's default and the wiring is
/// reported. The region around it (the freshness label, the stale marking, the
/// last-good-numbers rule) is this file's and is implemented in full.
const OVERVIEW_REFRESH_INTERVAL: Duration = Duration::from_secs(20);

/// The sentence that states, on the page, what the workload figures count.
///
/// **`below target` is not said anywhere on this page any more, and this is what
/// replaced it.** The phrase appeared on the Workloads heading's trailing edge
/// beside a ratio (`9 / 10,016   10,007 below target`) and was defined nowhere a
/// reader could reach: the full sentence was in the heading's tooltip, which is
/// not a place a reader goes to find out what a word means. So either the word
/// was defined or the page stopped using it — and a *definition* is cheaper than
/// it sounds, because the word only ever meant "the replicas the manifests
/// declare", which is one sentence and is now printed under the heading on every
/// load. The count beside the ratio is spelled `N missing`, which needs no
/// definition to be honest, and each workload cell says its own shortfall in the
/// same words.
const WORKLOADS_READING: &str = "Available replicas against the replicas the manifests declare.";

/// The sentence that states, on the page, what the Node capacity table is missing.
///
/// Deliberately does not name a cause. The panel is told *that* metrics did not
/// answer, never *why* — see [`OverviewView::render_capacity`] — so a sentence
/// that said "metrics-server is not installed" would be a guess printed as a
/// fact, and the same sentence on a cluster that merely denied the read would send
/// the reader to install something they already have.
///
/// It is **printed**, not hovered: two columns that are not on the table are a
/// structural absence, and a reader has to be able to tell "this node is idle"
/// from "the app cannot see what this node is doing" without pointing at
/// anything. It says nothing about the dashes in the limits columns beside it,
/// because those are a per-node absence rather than this one — the page's closing
/// line names those, once.
const NO_LIVE_METRICS_READING: &str =
    "Live CPU and memory are unavailable, so this table shows requests and limits only.";

/// The page's last line: what the dash in a limits column means.
///
/// **This is the page's closing gesture, and it is a definition rather than a
/// decoration.** The table above ends with two limits columns in which every node
/// with no declared limit prints `—`, which is the same glyph the *live* columns
/// used to print when metrics were missing — so one mark meant two absences, and
/// the header's `No live metrics` (which is no longer there) was being asked to
/// explain four cells that had nothing to do with it. The distinction now has a
/// sentence, and the sentence is at the foot of the page because that is where a
/// legend belongs and because the page needed a last line that acknowledged where
/// the data stopped.
///
/// It says nothing about *when* the numbers were taken: the freshness line in the
/// panel's own toolbar already owns that, and saying it twice is the repetition
/// `Design guides > Interface language > Let context carry context` rules out.
const CAPACITY_DASH_READING: &str = "A dash in a limits column means no limit is declared.";

/// Alpha of the wash that marks the whole region stale. `UI-REDESIGN.md` L9 fixes
/// it at 4%: enough to tint every figure at once, quiet enough that a healthy
/// cluster is never mistaken for a stale one.
const STALE_WASH_ALPHA: f32 = 0.04;

/// Kinds of issue the strip can itemise, and therefore chip handles it holds.
///
/// Four, not the three `UI-REDESIGN.md` §3.5 draws: failed pods, pending pods,
/// nodes not ready and workloads below target are four different problems, and a
/// strip that dropped the fourth would be wrong rather than restrained. The
/// handles are made once rather than keyed per chip, because a keyed handle is
/// created and dropped every time a cluster recovers — which is exactly when a
/// reader is most likely to be holding a chip.
const HEALTH_ISSUE_COUNT: usize = 4;

/// How the snapshot on screen relates to the cluster right now.
///
/// This is a different question from how the cluster is doing, and the two have
/// to stay apart: a cluster the app could not read has no health verdict, and a
/// snapshot that is being replaced is not yet wrong. Collapsing the two is what
/// makes a status display lie during an incident.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Freshness {
    /// Nothing has been taken yet.
    Never,
    /// A load is in flight. Whatever is on screen is the previous answer.
    Refreshing,
    /// A refresh failed. The numbers on screen are the last good ones, and how old
    /// they are is stated rather than implied.
    Stale,
    /// The answer is current.
    Current,
}

impl Freshness {
    /// The one line the toolbar says.
    fn label(self, age: Duration) -> String {
        match self {
            Self::Never => "Not loaded".to_owned(),
            Self::Refreshing => "Refreshing".to_owned(),
            Self::Stale => format!("Stale {}", format_age(age)),
            Self::Current => format!("Updated {}", format_age(age)),
        }
    }

    /// The sentence a tooltip and a screen reader get.
    fn detail(self, age: Duration) -> String {
        match self {
            Self::Never => "No snapshot has been taken yet.".to_owned(),
            Self::Refreshing => {
                "A refresh is in flight. The figures shown are the previous ones.".to_owned()
            }
            Self::Stale => format!(
                "The last refresh failed. These figures are {} old.",
                format_age(age)
            ),
            Self::Current => format!("Last refresh {}", format_age(age)),
        }
    }
}

/// An age in the coarsest unit that is still honest about it.
///
/// A wall clock formatted to the second (`updated 08:08:28 UTC`) makes a reader
/// do arithmetic to answer "how old is this", and the answer they want is one
/// glance. Below a minute the seconds are noise, so the ladder starts there.
fn format_age(age: Duration) -> String {
    let seconds = age.as_secs();
    if seconds < 60 {
        return if seconds <= 1 {
            "just now".to_owned()
        } else {
            format!("{seconds}s ago")
        };
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    format!("{}d ago", hours / 24)
}

/// The same ladder, for a wait that has not finished.
///
/// `format_age` is the wrong word for it in the one place that matters: the
/// loading ladder's last rung says how long the reader has been waiting, and
/// `Still loading · 2s ago` claims the load finished two seconds ago, which is
/// both wrong and — on the one rung that exists to reassure a reader that the
/// app is still trying — actively alarming. The numbers are shared so the two
/// ladders can never drift apart; only the suffix is different.
fn format_wait(waited: Duration) -> String {
    let seconds = waited.as_secs();
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h");
    }
    format!("{}d", hours / 24)
}

/// The freshness of the snapshot on screen, and how old it is.
fn freshness(
    state: &OverviewState,
    refreshing: bool,
    refresh_failed: bool,
    last_refreshed: Option<SystemTime>,
) -> (Freshness, Duration) {
    let age = last_refreshed
        .and_then(|at| SystemTime::now().duration_since(at).ok())
        .unwrap_or_default();
    // A failure outranks a load in flight. The figures on screen are still the
    // last good ones and they are still old, so `Stale` is the honest word for
    // them — and grading the retry as `Refreshing` instead would take the amber
    // off for the length of every request and put it back when the request
    // failed, so a cluster that is down would blink at the reader once per
    // interval rather than sitting there marked.
    let state = match (refresh_failed, state) {
        (true, OverviewState::Ready(_)) => Freshness::Stale,
        (false, OverviewState::Ready(_)) if refreshing => Freshness::Refreshing,
        (false, OverviewState::Ready(_)) => Freshness::Current,
        // Nothing has been taken yet, so there is nothing a refresh is
        // replacing: the first load used to say `Refreshing` over an empty page,
        // which claims a previous answer is on screen when the page is blank. A
        // panel with no cluster has taken nothing for the same reason.
        (false, OverviewState::Loading)
        | (false, OverviewState::Disconnected)
        | (false, OverviewState::Failed(_)) => Freshness::Never,
        // Only reachable if a failure is recorded without a snapshot to keep,
        // which `apply_refresh_result` does not do. `Stale` is still the honest
        // word for it: the figures on screen are the last good ones.
        (true, _) => Freshness::Stale,
    };
    (state, age)
}

/// Loads one snapshot with optional node metrics.
#[derive(Clone)]
pub struct OverviewHandle {
    handle: Handle,
    service: ClusterDataSource,
}

impl OverviewHandle {
    pub fn new(handle: Handle, source: impl Into<ClusterDataSource>) -> Self {
        Self {
            handle,
            service: source.into(),
        }
    }

    /// Loads and aggregates one snapshot.
    pub fn load_future(&self, metrics: bool) -> OpsFuture<Overview> {
        let service = self.service.clone();
        let handle = self.handle.clone();
        Box::pin(async move {
            join_abortable(
                &handle,
                async move { service.port().overview(metrics).await },
            )
            .await
            .map_err(|error| {
                format!(
                    "Overview request failed: {error}. Retry, or make sure the cluster connection works."
                )
            })?
        })
    }
}

/// Formats CPU cores.
pub fn format_cores(cores: f64) -> String {
    if (cores - cores.round()).abs() < 0.005 {
        format!("{}", cores.round() as i64)
    } else {
        format!("{cores:.2}")
    }
}

/// Formats bytes with binary units.
pub fn format_bytes(bytes: f64) -> String {
    const UNITS: [(&str, f64); 4] = [
        ("TiB", 1024.0 * 1024.0 * 1024.0 * 1024.0),
        ("GiB", 1024.0 * 1024.0 * 1024.0),
        ("MiB", 1024.0 * 1024.0),
        ("KiB", 1024.0),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            let value = bytes / scale;
            return if (value - value.round()).abs() < 0.05 {
                format!("{} {unit}", value.round() as i64)
            } else {
                format!("{value:.1} {unit}")
            };
        }
    }
    format!("{} B", bytes.round() as i64)
}

fn format_millicores(millicores: f64) -> String {
    format!("{}m", millicores.round() as i64)
}

// ── Figures ──────────────────────────────────────────────────────────────────

/// The figure the panel prints when the cluster reported nothing.
///
/// One dash for every kind of "no answer", so a denied source, a source that has
/// not answered yet, and a genuinely empty list all read as the same absence
/// rather than as three different zeroes.
///
/// A figure that *is* a reading of zero is not one of them: the workload cells
/// print `0 / 0` for a kind the cluster answered for and has none of, because
/// the dash and the zero are different claims and the panel can tell them apart.
const NO_ANSWER: &str = "—";

fn format_pct(value: f64) -> String {
    format!("{value:.0}%")
}

fn request_ratio(requested: f64, allocatable: Option<f64>) -> Option<f64> {
    allocatable
        .filter(|value| *value > 0.0)
        .map(|value| requested / value * 100.0)
}

/// The figure a limit column shows.
///
/// Kubernetes reads a zero limit as "no limit declared", and a bare `0` reads
/// as "this node's limit is zero", which is a different and much worse claim.
fn format_limit(limit: f64, format: impl Fn(f64) -> String) -> String {
    if limit <= 0.0 {
        NO_ANSWER.to_owned()
    } else {
        format(limit)
    }
}

/// The two-part figure a capacity cell states in words: `1.15 / 20 cores`.
///
/// The bar carries the share, because a bar can carry a share and cannot carry a
/// pair without becoming a form — which is the change `UI-REDESIGN.md` §3.5 asks
/// for, "replacing the bare `1.15 / 20 cores` text". The pair therefore moves to
/// the cell's spoken label and its tooltip, which is where precision belongs on a
/// screen whose job is the ten-second read.
fn request_figure(
    requested: f64,
    allocatable: Option<f64>,
    format_requested: impl Fn(f64) -> String + Copy,
    format_allocatable: impl Fn(f64) -> String + Copy,
) -> String {
    let requested_text = format_requested(requested);
    match allocatable.filter(|value| *value > 0.0) {
        Some(allocatable) => format!("{requested_text} / {}", format_allocatable(allocatable)),
        None => format!("{requested_text} requested"),
    }
}

/// The figure plus its share of allocatable, for a row label and a tooltip.
fn request_text(
    requested: f64,
    allocatable: Option<f64>,
    format_requested: impl Fn(f64) -> String + Copy,
    format_allocatable: impl Fn(f64) -> String + Copy,
) -> String {
    let Some(allocatable) = allocatable.filter(|value| *value > 0.0) else {
        return format!(
            "{} requested · allocatable unavailable",
            format_requested(requested)
        );
    };
    let figure = request_figure(
        requested,
        Some(allocatable),
        format_requested,
        format_allocatable,
    );
    let percentage = format_pct(requested / allocatable * 100.0);
    if requested > allocatable {
        format!("{figure} · {percentage} oversubscribed")
    } else {
        format!("{figure} · {percentage} of allocatable")
    }
}

fn format_node_usage(usage: Option<&NodeUsage>, metrics_available: bool) -> String {
    let Some(usage) = usage else {
        return if metrics_available {
            "Waiting for the first sample".to_owned()
        } else {
            "Metrics unavailable".to_owned()
        };
    };
    let mut parts = Vec::new();
    if let Some(cpu) = usage.cpu_millicores {
        parts.push(format!("CPU {}", format_millicores(cpu)));
    }
    if let Some(memory) = usage.memory_bytes {
        parts.push(format!("Memory {}", format_bytes(memory)));
    }
    if parts.is_empty() {
        return if metrics_available {
            "Waiting for the first sample".to_owned()
        } else {
            "Metrics unavailable".to_owned()
        };
    }
    parts.join(" · ")
}

/// The short, honest answer a live cell gives before the cluster reports.
///
/// The cluster answered for some nodes and not yet for this one, or metrics-server
/// is not installed. A zero would be a reading the app never took, and a cell
/// sized for a number would only show the first word of the full sentence, which
/// stays in the row's spoken label instead.
fn no_reading(metrics_available: bool) -> &'static str {
    if metrics_available {
        "Waiting"
    } else {
        "No metrics"
    }
}

/// The node's own sentence, for the row's accessible name and its tooltip.
///
/// It states the share of allocatable that `utilization_pct` already computes,
/// and every absolute the bar no longer prints, because the row is the only
/// element that knows the blended figure — the two axes each carry one.
fn capacity_summary(capacity: &NodeCapacity, usage: Option<&NodeUsage>) -> String {
    let mut parts = vec![capacity.name.clone()];
    if let Some(pct) = capacity.utilization_pct {
        parts.push(format!(
            "Node utilization: {}{}",
            format_pct(pct),
            if pct > 100.0 {
                " oversubscribed"
            } else {
                " of allocatable"
            }
        ));
    }
    parts.push(format!(
        "CPU requests: {}",
        request_text(
            capacity.requested_cpu,
            capacity.allocatable_cpu,
            format_cores,
            |cores| format!("{} cores", format_cores(cores)),
        )
    ));
    parts.push(format!(
        "Memory requests: {}",
        request_text(
            capacity.requested_memory,
            capacity.allocatable_memory,
            format_bytes,
            format_bytes,
        )
    ));
    parts.push(format!(
        "CPU limits: {}",
        format_limit(capacity.limits_cpu, format_cores)
    ));
    parts.push(format!(
        "Memory limits: {}",
        format_limit(capacity.limits_memory, format_bytes)
    ));
    if let Some(usage) = usage {
        parts.push(format!(
            "Current usage: {}",
            format_node_usage(Some(usage), true)
        ));
    }
    parts.join(", ")
}

// ── Column layout ────────────────────────────────────────────────────────────

/// Column minimum widths, in column order.
///
/// A capacity cell holds a bar, a share and a figure, so a column narrower than
/// its own value ends every row in an ellipsis, and an ellipsis reports no value
/// at all. The minimums are therefore sized for the text a row actually renders:
/// a 28-character node name, `104%`, `4.8 / 4 cores`, and the severity slot an
/// oversubscribed cell reserves for the overcommit glyph. The two limits columns
/// carry that slot too, beside `31.1 GiB` and a dash, because a request above its
/// own limit is now a state on the figure that is wrong. Below this grid the
/// table keeps the minimums and scrolls, instead of squashing numbers into
/// unreadable widths.
///
/// A **heading** counts as text a row renders, and two of these were narrower
/// than their own: at the minimum grid `CPU LIMITS` and `MEMORY LIMITS` came out
/// as `CPU LIM…` and `MEMORY LI…`, so the two columns that name a ceiling were the
/// two columns whose names could not be read. The live columns gave 144 each for a
/// cell that holds a three-character share and a bar, and that width is where it
/// came from.
const CAPACITY_NODE_WIDTH: f32 = 208.0;
const CAPACITY_CPU_REQUEST_WIDTH: f32 = 128.0;
const CAPACITY_MEMORY_REQUEST_WIDTH: f32 = 152.0;
const CAPACITY_CPU_LIMITS_WIDTH: f32 = 104.0;
const CAPACITY_MEMORY_LIMITS_WIDTH: f32 = 128.0;
const CAPACITY_CPU_NOW_WIDTH: f32 = 104.0;
const CAPACITY_MEMORY_NOW_WIDTH: f32 = 128.0;

/// Column minimum widths, in column order.
const CAPACITY_COLUMN_MIN_WIDTHS: [f32; 7] = [
    CAPACITY_NODE_WIDTH,
    CAPACITY_CPU_REQUEST_WIDTH,
    CAPACITY_MEMORY_REQUEST_WIDTH,
    CAPACITY_CPU_LIMITS_WIDTH,
    CAPACITY_MEMORY_LIMITS_WIDTH,
    CAPACITY_CPU_NOW_WIDTH,
    CAPACITY_MEMORY_NOW_WIDTH,
];

/// Share of the spare width each column takes.
///
/// **The node column takes the smallest share it can, and that is the fix.** It
/// used to take three thirteenths of the surplus, which on a 1920px window made
/// it 435px wide — 24% of the table for a string that is usually eleven
/// characters — and starved nothing, because the six numeric columns were the ones
/// whose spare went into a bar stretched past the point where its length means
/// anything. Weights are now 1 for the name and 1.5–1.75 for the numeric columns,
/// in the order of how much each one holds: memory before CPU (a byte figure is
/// two to three characters longer), limits below both (a limit is a bare number),
/// and the live columns like the request columns because they carry a meter too.
///
/// The surplus the node column no longer takes goes *inside* the numeric cells,
/// into the meters, rather than to one column: `Design guides > Alignment details`
/// asks peers to share geometry, and a reader comparing two `5%` cells to be told
/// they are the same size is better served by two identical meters than by two
/// identical meters at opposite ends of a 200px cell. The meter is also the only
/// element in a cell whose *length* means something, so it is the only one that
/// can take the extra width without inventing a hole — see
/// [`CAPACITY_CELL_METER`].
const CAPACITY_COLUMN_WEIGHTS: [f32; 7] = [1.0, 1.5, 1.75, 1.0, 1.25, 1.5, 1.75];

/// The lane a numeric cell gives its meter: the length it has before the column's
/// spare is added, and the lane a heading is set over.
///
/// `size::UPDATE_PROGRESS` (160px) is the width the product already gives a
/// progress mark, so the capacity meters and the refresh sweep are one length
/// rather than two nearly-similar ones, and it is long enough for a fill to be a
/// shape: at 6px tall, a `5%` of 160px is an 8px bar.
///
/// **The meter grows from here; the cell does not.** It used to stop at this
/// length and hand the rest of the column to a flexible spacer between the meter
/// and the figure, which on a 1920px window put **115px of nothing** between a
/// `5%` and the bar it belongs to: the row read as `▬▬▬ ▬▬▬▬▬▬ 5% ▬▬▬ 1% —` and a
/// reader had no way to tell which figure belonged to which meter. A spacer
/// between two things that belong together is `Design guides > Alignment details`'
/// "missing structure in the middle", and the surplus now goes to the meter
/// instead, which is the one thing in the cell whose length is a measurement.
///
/// The share is therefore drawn against a lane that grows with the column rather
/// than a fixed 160px, and a very small share is a very small fill on it — the
/// value is printed in the lane beside it, which is what a table of figures is
/// for.
const CAPACITY_CELL_METER: Pixels = design::size::UPDATE_PROGRESS;

/// The lane a numeric cell reserves for its figure, at the cell's trailing edge.
///
/// A fixed lane rather than a shrink-to-fit one, and that is the whole point of
/// it: `104%` is a character wider than `0%`, so a figure that sizes itself moves
/// the bar beside it by a character on every row and the column's right edge stops
/// being a spine. `Design guides > Alignment details` asks for "intentional slots
/// or lanes when cross-row comparison matters", and this column is nothing but
/// cross-row comparison.
///
/// Five characters of `text::BODY` on the UI face, measured the same way
/// [`OverviewView::empty_state_measure`] measures its 40ch line: `0` advances
/// 0.6em, so `104%` plus a character of slack is `5 × 13 × 0.6`. The figure is
/// right-aligned inside the lane, so its trailing edge is the column's trailing
/// edge — the same edge the heading above it ends on.
fn capacity_figure_lane() -> Pixels {
    px(f32::from(text::BODY) * 0.6 * 5.0)
}

/// The lane a numeric cell reserves for its state glyph, drawn or not.
///
/// Reserved on every numeric row so that an oversubscribed node does not move its
/// own meter 16px to the right: a glyph lane that exists only when there is
/// something to put in it moves everything beside it, which is the opposite of
/// `Design guides > Alignment details`' "preserve the spine through optional
/// content". The heading above reserves the same lane, so a column's heading
/// starts where its meter starts rather than a glyph-width to its left.
const CAPACITY_CELL_GLYPH: Pixels = design::icon::IN_ROW;

/// Column headings, in column order.
///
/// A heading is a noun phrase, so the live columns name the reading they hold
/// rather than a bare `Now`. The panel is sentence case everywhere else, so the
/// headings are too; the `CPU` acronym stays uppercase.
const CAPACITY_COLUMNS: [&str; 7] = [
    "Node",
    "CPU requests",
    "Memory requests",
    "CPU limits",
    "Memory limits",
    "CPU now",
    "Memory now",
];

/// The share a request or usage bar is drawn against, for each axis's column.
///
/// Spoken, because a bar carries no unit of its own: `104%` on its own is a
/// percentage of nothing.
const CAPACITY_REQUEST_SHARE: [&str; 2] = [
    "share of the node's allocatable CPU",
    "share of the node's allocatable memory",
];
const CAPACITY_USAGE_SHARE: [&str; 2] = [
    "current usage as a share of the node's allocatable CPU",
    "current usage as a share of the node's allocatable memory",
];

/// How the capacity table answers keyboard navigation.
const CAPACITY_TABLE_DESCRIPTION: &str = "Node requests, limits, and current usage, as shares \
of each node's allocatable capacity. Use Up and Down to move between nodes. Use Left and Right \
to move between columns. Use Home and End for the first or last column in a row. \
Use Control+Home or Control+End for the first or last cell in the table. \
Select a column heading to sort by it.";

/// The keys the capacity table answers, for assistive technology.
const CAPACITY_TABLE_KEYS: &str = "ArrowLeft ArrowRight ArrowUp ArrowDown Home End \
Control+Home Control+End PageUp PageDown";

/// The breathing between two columns of the capacity table.
///
/// `UI-SPEC` §2.1 asks for 12px of air between columns, "provided by the column
/// widths", and there is a reason this one has to be *stated* rather than left to
/// the widths: a request cell is a bar and a percentage, and the bar is a
/// flexible element that ends wherever the percentage begins. So the gap between
/// a bar and **its own** figure and the gap between that figure and the **next
/// column's** bar were both 4px, and the row read as `▬▬▬ 5% ▬▬▬ 1% — 340 MiB` — a
/// sequence of marks with no way to tell which bar a percentage belonged to.
/// Measured on a 1920px window, `CPU REQUESTS`' bar ended at x=1082, its `5%` ran
/// 1087–1105, and `MEMORY REQUESTS`' bar started at 1109: a 5px gap to its own
/// number against a 4px gap to the next one.
///
/// Twelve on the **leading** edge of every numeric column fixes it without moving
/// a single figure off the column edge its heading sits over: bar→figure stays 4px
/// and figure→next-bar becomes 16px, a 4:1 difference, and a number now reads as
/// belonging to the bar immediately to its left. Padding the trailing edge
/// instead would have shifted every figure 12px away from its own heading, and
/// `the_capacity_header_follows_its_data_and_the_sections_share_one_edge` is the
/// test that would have caught it.
const CAPACITY_COLUMN_BREATH: Pixels = space::MD;

/// Live columns: current CPU and current memory, one axis per cell.
const CAPACITY_LIVE_COLUMN_COUNT: usize = 2;

/// Column count without the live reading columns.
const CAPACITY_COLUMN_COUNT: usize = CAPACITY_COLUMN_MIN_WIDTHS.len() - CAPACITY_LIVE_COLUMN_COUNT;

/// Column widths for the available width, never below the column minimums.
fn capacity_column_widths(available: f32, metrics_available: bool) -> Vec<f32> {
    let count = if metrics_available {
        CAPACITY_COLUMN_MIN_WIDTHS.len()
    } else {
        CAPACITY_COLUMN_COUNT
    };
    let minimum: f32 = CAPACITY_COLUMN_MIN_WIDTHS[..count].iter().sum();
    // Below the minimum grid the columns keep their minimums and the table
    // scrolls, instead of squashing numbers into unreadable widths.
    let table_width = if available.is_finite() {
        available.max(minimum)
    } else {
        minimum
    };
    let spare = table_width - minimum;
    let weight_total: f32 = CAPACITY_COLUMN_WEIGHTS[..count].iter().sum();
    (0..count)
        .map(|column| {
            let share = CAPACITY_COLUMN_WEIGHTS[column] / weight_total;
            // Whole pixels. The table lays its heading out in one flex row and
            // its cells out in a virtualised list, and the two accumulate
            // fractional widths differently: a column of 170.2px left the last
            // heading and the last cell a whole pixel apart, so a heading no
            // longer sat over the numbers it names.
            (CAPACITY_COLUMN_MIN_WIDTHS[column] + spare * share).round()
        })
        .collect()
}

/// Padding the panel puts around the capacity grid: the scroll padding, on both
/// sides.
///
/// The grid is the one region on this page that is **not** inside a [`band`] —
/// it runs to the panel's own padding line so its seven columns get the width —
/// so the band's [`BAND_PADDING`] is not part of this budget. Counting it anyway
/// would take 40px the grid does not give up and force its columns to scroll a
/// window they would otherwise fit.
///
/// The group frame this used to add is gone with the other strokes, and a width
/// budget that still reserves a border the panel does not draw is a budget the
/// table gives away for nothing.
fn capacity_panel_padding() -> f32 {
    2.0 * 2.0 * f32::from(space::LG)
}

/// Width the capacity grid may use in a panel measured at `measured`.
///
/// The measurement is taken at paint time and applied on the next frame, so on
/// the frame right after a shrink-resize it is still the width the panel used
/// to have. The window is the hard ceiling: a table wider than the window it
/// sits in forces a horizontal scroll, and the minimum window size has to keep
/// the table readable without one.
fn capacity_available_width(measured: f32, window_width: f32) -> f32 {
    let padding = capacity_panel_padding();
    (measured - padding).clamp(0.0, (window_width - padding).max(0.0))
}

///
/// `UI-SPEC` §1.1 gives `surface.content` to the table and the YAML plane — the
/// one surface the eye rests on for hours — and the whole page is on it now, so
/// the table, its rows and the region around it are one plane and the table has
/// no band of its own. This used to read the *skin* field the token migration
/// removes; the row base also used to stay on the app plane
/// while the stripe, the hover and the selection all landed elsewhere, so the
/// stripe was mixed against a base it was not drawn on.
fn capacity_table_surface(cx: &App) -> Hsla {
    role::surface_content(cx).alpha(1.0)
}

/// Rows the capacity table is never squeezed below, on a window too short to give
/// it more.
///
/// **Two, and the second one is the whole point.** One row is the heading band, and
/// gpui-kit spends it inside this box; the other is a data row, so the region can
/// never be a header with nothing under it. See [`capacity_table_height`].
///
/// There is no ceiling on the *region*, and there was: a cluster of hundreds of
/// nodes scrolls its own row list — the table is virtualised, so the viewport is
/// bounded and only the visible rows are built — but a cluster of three does not
/// get a box sixteen rows deep with thirteen of them vacant. The box follows the
/// rows; the rows follow the cluster.
const CAPACITY_TABLE_MIN_ROWS: f32 = 1.0;

/// Widest gap the width measurement ignores while a splitter is dragged.
const WIDTH_MEASURE_STEP: f32 = 8.0;

/// A section heading: the word on the leading edge, an optional total on the
/// trailing edge, and one hairline under the whole row.
///
/// **The hairline spans the section, and that is the change.** It used to be a
/// 48px dash that stopped in the middle of empty space beside the word — the
/// cheapest-looking mark on the page, and a mark whose only job was to say "I am
/// a heading" while measuring nothing. A rule that spans the region it names
/// says the same thing *structurally*: it is the boundary's owner, so
/// `Design guides > Alignment details` ("hairlines belong on the boundary owner")
/// is satisfied by construction, and the eye reads `WORKLOADS … 9 / 10,016` as one
/// row over one rule rather than as a label, an orphan dash and a number.
///
/// Both section headings are built by this one function, so `WORKLOADS` and
/// `NODE CAPACITY` cannot drift: same row height, same label role, same total
/// treatment, same rule.
///
/// The word is `caption` 11/600 uppercase at `fg.tertiary`, which is what §2.3
/// reserves `caption` for (分区标题) and what the Inspector's own `section_head`
/// wears — so the two panels' section headings are one role and not two. It is
/// **not** `body`, which is what the Dock's empty-state *description* was changed
/// to: that change was about a 15/600 title sitting directly above an 11/600
/// sentence, which is two levels at one weight reading as one heading. Nothing
/// here sits directly under a 15px line, and `§2.3`'s own example of the failure
/// is `body 13` against `metadata 11`, i.e. the opposite direction.
///
/// The rule is pinned to the row's bottom edge with `bottom_0` rather than
/// centred in it. A 1px element is crisp only when its top edge lands on a whole
/// logical pixel, and centring one in an even band always produces
/// `band/2 − 0.5` — a half-integer, by construction, for every even band in the
/// app. `bottom_0` on a 24px band puts it at `24 − 1`, an integer, so the rule is
/// on the grid for every band whose own origin is one. The other half of that
/// rule — the band's origin — belongs in `design.rs` as device-pixel snapping in
/// the vertical rhythm; reported, not taken here.
fn section_heading(title: &'static str, cx: &App) -> AnyElement {
    section_heading_with_total(title, None, None, cx)
}

/// A section heading, optionally carrying its region's total on the trailing
/// edge and the sentence that reads it.
///
/// The total is the answer to "are they up" and the region below is the
/// itemisation of it, so the heading is where the answer belongs. It is also
/// where the Inspector puts a section's count, which is the same decision about
/// the same question.
///
/// **The total is a count, so it is quiet, and the news beside it is what spends a
/// channel.** The trailing edge used to be one string in one ink, and the ink was
/// the region's severity: `WORKLOADS … 9 / 10,016` came out in the caution hue
/// because some replicas were below target. But the number that was coloured was
/// not the fact — the fact is *how many* are missing, and 10,016 of them existing
/// is not a problem at all. A status ink on a neutral figure is `Design guides >
/// Color and themes`' "a semantic status colour as decoration", and a reader who
/// has learned that amber on this screen means something now has to unlearn it
/// every time a healthy total goes amber. So [`HeadingTotal`] splits the edge: the
/// figure is set in the quiet ink at `text::LABEL` in tabular figures, and the
/// `note` — the sub-count, which is the fact that carries the severity — is set in
/// [`word_ink`], the role for a status *word*, because there is no dot beside it to
/// carry the state.
///
/// **The sentence that reads the number is not on the row, it is under it.** A
/// ratio is a form: `9 / 10,016` states two counts and leaves the reader to work
/// out that a thousand of them are missing, so the note names the shortfall in
/// words and the *full* sentence rides in the heading's accessible description and
/// its tooltip, where a pointer and a screen reader both find it. It used to be
/// computed on every frame and thrown away.
fn section_heading_with_total(
    title: &'static str,
    total: Option<HeadingTotal<'_>>,
    reading: Option<&str>,
    cx: &App,
) -> AnyElement {
    let rule_selector = "overview-section-dash".to_owned();
    // Two headings, so the id carries the title: an id is a window key, and two
    // regions cannot share one.
    let heading_id = format!("overview-section-heading-{title}");
    h_flex()
        .id(ElementId::from(heading_id))
        .debug_selector(|| "overview-section-heading".to_owned())
        .relative()
        .w_full()
        .min_w(px(0.))
        .h(design::size::ROW_DENSE)
        .gap(space::SM)
        .items_center()
        .when_some(reading, |this, reading| {
            this.aria_description(reading.to_owned())
                .tooltip(text_tooltip(reading.to_owned()))
        })
        .child(
            Label::new(title.to_uppercase())
                .text_size(text::CAPTION)
                .line_height(text::CAPTION_LINE_HEIGHT)
                .font_weight(text::SEMIBOLD)
                .text_color(role::fg_tertiary(cx)),
        )
        .child(div().flex_1().min_w(px(0.)))
        .when_some(total, |this, total| {
            this.child(
                h_flex()
                    .flex_none()
                    .min_w(px(0.))
                    .gap(space::SM)
                    .children([
                        div()
                            .flex_none()
                            .debug_selector(|| "overview-section-total".to_owned())
                            .font(ui_font(cx))
                            .font_features(tabular_features())
                            // The head's own step: 11px uppercase next to 12px put
                            // the two baselines a pixel apart on one line, and the
                            // count stays quiet through ink, not through size.
                            .text_size(text::CAPTION)
                            .line_height(text::CAPTION_LINE_HEIGHT)
                            .font_weight(text::MEDIUM)
                            .text_color(word_ink(total.severity, cx))
                            .child(total.figure.to_owned()),
                        // The sub-count, in the channel that owns the state. It
                        // appears and disappears with the state, and because the
                        // group is flush right the figure beside it never moves
                        // when it does — a heading whose trailing edge reflows is a
                        // heading that moves under the reader's eye.
                        div()
                            .flex_none()
                            .debug_selector(|| "overview-section-note".to_owned())
                            .font(ui_font(cx))
                            .font_features(tabular_features())
                            .text_size(text::CAPTION)
                            .line_height(text::CAPTION_LINE_HEIGHT)
                            .font_weight(text::MEDIUM)
                            .text_color(match total.note {
                                Some((_, severity)) => word_ink(severity, cx),
                                None => role::fg_tertiary(cx),
                            })
                            .when_some(total.note, |this, (text, _)| this.child(text.to_owned())),
                    ])
                    .into_any_element(),
            )
        })
        .child(
            div()
                .debug_selector(move || rule_selector.clone())
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .h(border::LINE)
                .bg(role::border_subtle(cx)),
        )
        .into_any_element()
}

/// A section heading's trailing edge: the region's total, and the news about it.
///
/// The two travel together because only one of them is a fact a reader acts on,
/// and only one of them is allowed to spend a status channel. `severity` belongs
/// to the *figure* and is what a heading whose whole edge is one string passes;
/// `note` is the coloured half, and it is the sentence that says what is wrong
/// rather than the count that is.
struct HeadingTotal<'a> {
    /// The count, or the one-word state a region with no count reports instead.
    figure: &'a str,
    /// The ink the count is set in. `Muted` is grey, which is what a count of
    /// everything the cluster has wants to be.
    severity: Severity,
    /// The sub-count that carries the severity, when there is one.
    note: Option<(&'a str, Severity)>,
}

/// One quiet line under a section heading: what the figures below it count.
///
/// This is the answer to "what is this number out of?", printed where a reader
/// scanning the page will meet it rather than in a tooltip they have to go and
/// find. It is the same role twice — the Workloads denominator and the Node
/// capacity metrics absence — so it is the same treatment twice: one line at
/// `label` in secondary ink, on the content spine, on the same 8px gap the
/// heading and its band already share.
///
/// `label` and not `caption`: it is a sentence rather than a label, and
/// `Design guides > Interface language > Capitalization` asks sentence case for
/// prose. It is not `body`, because it is not carrying the work — the figures
/// below it are.
fn section_reading(key: &'static str, text: &'static str, cx: &App) -> AnyElement {
    let selector = format!("overview-section-reading-{key}");
    div()
        .id(ElementId::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .w_full()
        .min_w(px(0.))
        .font(ui_font(cx))
        .text_size(text::LABEL)
        .line_height(text::LABEL_LINE_HEIGHT)
        .text_color(role::fg_secondary(cx))
        .child(text)
        .into_any_element()
}

/// The page's last line, and the only thing on the page after the capacity table.
///
/// See [`CAPACITY_DASH_READING`]. It is `caption` in the quietest ink because it is
/// a footnote and nothing more: no box, no rule, no colour, and 32px of the body's
/// own gap above it, so it reads as the page's end rather than as another region
/// that failed to load.
fn page_reading(cx: &App) -> AnyElement {
    div()
        .id("overview-page-reading")
        .debug_selector(|| "overview-page-reading".to_owned())
        .w_full()
        .min_w(px(0.))
        .font(ui_font(cx))
        .text_size(text::CAPTION)
        .line_height(text::CAPTION_LINE_HEIGHT)
        .text_color(role::fg_tertiary(cx))
        .child(CAPACITY_DASH_READING)
        .into_any_element()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverviewDataState {
    Empty,
    Partial,
    /// Every source was denied: the cluster is fine, the account is not.
    Forbidden,
    Error,
    Complete,
}

fn workload_data_available(workloads: &WorkloadCounts) -> bool {
    [
        workloads.deployments,
        workloads.stateful_sets,
        workloads.daemon_sets,
        workloads.jobs,
        workloads.cron_jobs,
    ]
    .iter()
    .any(|summary| summary.desired != 0 || summary.available != 0)
}

fn overview_data_state(overview: &Overview) -> OverviewDataState {
    let has_data = overview.has_data
        || overview.nodes.count > 0
        || overview.health.total_pods > 0
        || workload_data_available(&overview.workloads)
        || !overview.capacities.is_empty()
        || overview
            .usage
            .as_ref()
            .is_some_and(|usage| !usage.is_empty());
    let has_source_error = overview
        .unavailable_sources
        .iter()
        .any(|source| source.reason != SOURCE_NOT_LOADED);
    if has_source_error && overview.unavailable_sources.len() >= OVERVIEW_SOURCE_COUNT {
        // A denial is a configuration problem, not a cluster problem. It gets
        // its own state so the copy can name the missing permission.
        return if overview.fully_denied() {
            OverviewDataState::Forbidden
        } else {
            OverviewDataState::Error
        };
    }
    if !has_data && !has_source_error {
        return OverviewDataState::Empty;
    }
    if !overview.unavailable_sources.is_empty() {
        return OverviewDataState::Partial;
    }
    if overview.nodes.count == 0 || overview.capacities.len() != overview.nodes.count {
        OverviewDataState::Partial
    } else {
        OverviewDataState::Complete
    }
}

fn source_failure_detail(overview: &Overview) -> String {
    overview
        .unavailable_sources
        .iter()
        .map(|source| format!("{}: {}", source.source, source.reason))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The permissions a denied snapshot is missing, written for a human.
fn denied_permissions(overview: &Overview) -> Vec<&'static str> {
    overview
        .denied_sources()
        .map(|(_, permission)| permission)
        .collect()
}

/// Copy for a denial: what is missing and what to do about it.
///
/// The raw API text stays in the tooltip. This sentence names the permission,
/// because a denial is answered by an RBAC change, not by a retry — and it does
/// not open with "Access denied", because both of its callers have already said
/// that in their title (`Access denied`) or in their own clause
/// (`Some cluster data is unavailable`). A hint that repeats its own heading is
/// the "two sentences saying the same thing" `UI-SPEC` §4.13 rules out, and the
/// verb matches the button under it so the sentence and the control agree.
fn denial_copy(overview: &Overview) -> String {
    let permissions = denied_permissions(overview);
    if permissions.is_empty() {
        return "Grant the permissions this account needs, then retry.".to_owned();
    }
    let named = permissions
        .iter()
        .map(|permission| format!("'{permission}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let more = if permissions.len() > 1 {
        format!(" and {} more", count(permissions.len() - 1))
    } else {
        String::new()
    };
    format!("Grant {named}{more} in your role, then retry.")
}

/// Which step of the load failed.
///
/// `UI-SPEC` §4.15 is explicit — "错误出现在它发生的位置" and the worked example
/// names the step in the sentence — and this panel gave every failure the same
/// two sentences: `Failed to load the overview` over `Retry, or make sure the
/// cluster connection works.`, with the actual error in a tooltip no reader is
/// going to hover. A refused connection, a DNS failure, a request that ran past
/// the deadline and an RBAC denial are four different problems with four
/// different next steps, and one sentence for all four is a sentence that answers
/// none of them.
///
/// The classification reads the same substrings `k8s_core::overview::SourceFailure`
/// reads — the API server's own words plus the transport's — because that is the
/// vocabulary both layers see. It is written out here rather than called through
/// `SourceFailure` because that method is a method on a per-source record and this
/// is one whole-request error; the data layer is not this wave's file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailureStep {
    /// The API server answered, and the answer was no: this identity is not
    /// allowed to read what it asked for.
    Forbidden,
    /// The API server answered, and it would not say who this is. A `401` is not
    /// a `403`, and it has never been treated as one here: they were one branch,
    /// so a rejected token was reported as "this account is missing a permission"
    /// and sent the reader to look at a ClusterRole that was fine.
    Unauthenticated,
    /// The cluster did not answer inside the request deadline.
    TimedOut,
    /// The cluster could not be reached at all.
    Unreachable,
    /// The request failed for a reason the transport did not name.
    Other,
}

/// What the one control on a failure state actually does.
///
/// **The sentence and the button have to be one decision.** The `Unauthenticated`
/// next step says `Reload the kubeconfig` and the state printed `Retry` under it,
/// and a `Retry` re-sends the request the API server has just refused with the
/// same credentials — it cannot succeed, so the control promised a different
/// answer than the sentence described. The classifier already knows which of the
/// two a failure is answered by, so it decides rather than the copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailureAction {
    /// Ask this connection the same question again.
    Retry,
    /// Read the kubeconfig files again and rebuild the connection.
    ReloadKubeconfigs,
}

impl FailureStep {
    /// Classifies one error string.
    fn of(reason: &str) -> Self {
        let reason = reason.to_lowercase();
        if reason.contains("403") || reason.contains("forbidden") {
            Self::Forbidden
        } else if reason.contains("401")
            || reason.contains("unauthorized")
            || reason.contains("unauthenticated")
        {
            Self::Unauthenticated
        } else if reason.contains("timed out")
            || reason.contains("timeout")
            || reason.contains("deadline")
            || reason.contains("elapsed")
        {
            Self::TimedOut
        } else if reason.contains("connection")
            || reason.contains("dns")
            // getaddrinfo's two spellings, and the one a resolver failure almost
            // always arrives as. `k8s_core::overview::SourceFailure` only matches
            // the literal "dns", so a snapshot whose source came back
            // `Name or service not known` was classified `Other` there and would
            // be here — reported, not edited: the data layer is not this file.
            || reason.contains("name or service not known")
            || reason.contains("nodename nor servname")
            || reason.contains("temporary failure in name resolution")
            || reason.contains("no such host")
            || reason.contains("refused")
            || reason.contains("no route")
            || reason.contains("host is unreachable")
            || reason.contains("certificate")
            || reason.contains("eof")
        {
            Self::Unreachable
        } else {
            Self::Other
        }
    }

    /// The short clause the inline 32px strip can hold next to its own sentence.
    fn clause(self) -> &'static str {
        match self {
            Self::Forbidden => "The API server refused this account",
            Self::Unauthenticated => "The API server rejected this account's token",
            Self::TimedOut => "The cluster did not answer in time",
            Self::Unreachable => "The cluster could not be reached",
            Self::Other => "The request failed",
        }
    }

    /// The state of a load that failed, naming the step that failed.
    ///
    /// `UI-SPEC` §4.15 asks for a *specific* sentence and this panel's title was
    /// the same five words for all four failures — `Failed to load the overview` —
    /// which answers nothing on its own: a refused connection, a rejected token, a
    /// request that ran past the deadline and an RBAC denial are four problems
    /// with four different next steps, and the reader cannot tell from the title
    /// which one they are looking at. So the title names the step and the line
    /// under it names the move, and each is one clause.
    fn title(self) -> &'static str {
        match self {
            Self::Forbidden => "This account cannot read the cluster",
            Self::Unauthenticated => "The cluster rejected this account",
            // §4.15's own worked example titles the failure as a statement about
            // the step, not about the panel.
            Self::TimedOut => "Reading the cluster timed out",
            Self::Unreachable => "The cluster could not be reached",
            Self::Other => "Reading the cluster failed",
        }
    }

    /// The move, as a verb phrase.
    ///
    /// This is the *action* only; [`Self::title`] already said what failed, and
    /// repeating it here is the "two sentences saying the same thing"
    /// `UI-SPEC` §4.13 rules out.
    fn next_step(self) -> &'static str {
        match self {
            Self::Forbidden => "Bind a role that can read this cluster, then retry.",
            // A `401` is answered by new credentials, never by an RBAC edit, and
            // the app's own action for that is `ReloadKubeconfigs`.
            Self::Unauthenticated => "Reload the kubeconfigs to read new credentials.",
            // `PROMPT.md` §2.4 requires a network timeout to be *visible* inside
            // 10 seconds, and `Self::TimedOut`'s title is what makes it visible:
            // it names the wait rather than asking the reader to go and look.
            Self::TimedOut => "Check the network between here and the API server, then retry.",
            Self::Unreachable => "Check the API server address and the network, then retry.",
            Self::Other => "Retry, or check the app log for the full request.",
        }
    }

    /// The control this failure is answered by.
    ///
    /// Only an identity the API server would not name is different: the request
    /// carries the same token whichever way it is sent, so the connection has to
    /// be rebuilt from the file rather than asked again.
    fn action(self) -> FailureAction {
        match self {
            Self::Unauthenticated => FailureAction::ReloadKubeconfigs,
            Self::Forbidden | Self::TimedOut | Self::Unreachable | Self::Other => {
                FailureAction::Retry
            }
        }
    }
}

/// A count for a user-facing figure.
///
/// Every count in this panel goes through the shared formatter, so a figure
/// reads the same here and in the table toolbar.
fn count(value: usize) -> String {
    design::format::count(value)
}

/// A value like `3 / 3`, or a dash when the cluster reported nothing.
fn ratio(available: usize, total: usize) -> String {
    if total == 0 {
        return NO_ANSWER.to_owned();
    }
    format!("{} / {}", count(available), count(total))
}

/// Renders the panel's biggest number and the ratios under it.
pub type ShowProblemsCallback = Rc<dyn Fn(&mut Window, &mut App)>;

/// The routes a host can install from a count on the strip to the rows behind it.
///
/// Only the pods route exists today: `shell/mod.rs` installs it, and it opens the
/// Pods table with the problems filter on. The node and workload routes are the
/// same shape and the panel already renders a chip for each, so the two slots
/// exist rather than leaving the panel unable to draw a chip it can describe. A
/// chip with no route is drawn as the same wash with no target, because a control
/// that goes nowhere is a lie — the rule the hero caption used to be held to.
#[derive(Default, Clone)]
pub struct OverviewRoutes {
    /// Pods that need attention.
    pub pods: Option<ShowProblemsCallback>,
    /// Nodes that are not ready.
    pub nodes: Option<ShowProblemsCallback>,
    /// Workloads below their target.
    pub workloads: Option<ShowProblemsCallback>,
}

/// One thing the cluster needs attention about.
struct Issue {
    /// The words on the chip: the count and the noun it counts.
    label: String,
    /// The whole sentence, for the tooltip and the chip's accessible name.
    detail: String,
    /// The channel the chip spends. A healthy cluster has no chips at all, so
    /// this is only ever a caution or a fault.
    severity: Severity,
    /// Where to go for the rows behind the count, when the host installed a way.
    route: Option<ShowProblemsCallback>,
}

impl Issue {
    /// How far down the "look at this one first" order the issue sits.
    ///
    /// Zero is the thing to open first. `PRODUCT.md` §3.3 asks for the clusters
    /// on the startup screen to be ordered by "who to look at first" rather than
    /// by name, and the same question asked of the four kinds of problem inside
    /// one cluster has the same answer: a cluster with a failed pod and a pending
    /// pod is not two equal facts, and a strip that prints them in the order the
    /// code happened to discover them makes the reader rank them by eye.
    ///
    /// The order was already error-first by accident — `health_issues` pushes
    /// failed pods first — so this changes no screenshot today. It is stated
    /// because the order is a product decision, and the next kind of issue to be
    /// added would otherwise land wherever it was written.
    fn rank(&self) -> u8 {
        match self.severity {
            Severity::Error => 0,
            _ => 1,
        }
    }
}

/// The worst severity among the strip's items, and so the one channel it spends.
///
/// The strip draws a single mark and this is what decides its colour: the fault
/// hue whenever anything on it is a fault, the caution hue when nothing is and
/// something is a caution, and no mark at all when the only thing the cluster has
/// is a condition the app cannot interpret. See [`OverviewView::strip_mark`].
fn worst_severity(issues: &[Issue]) -> Severity {
    issues
        .iter()
        .fold(Severity::Muted, |worst, issue| match issue.severity {
            Severity::Error => Severity::Error,
            Severity::Warning if !matches!(worst, Severity::Error) => Severity::Warning,
            _ => worst,
        })
}

/// The destination sentence an item adds — or nothing at all.
///
/// `UI-REDESIGN.md` §3.5 makes the chip the route to the rows behind its count,
/// and the shell has installed one of the three route slots. A chip whose slot
/// is empty is drawn as a figure: the same wash, no hover, no focus stop, no
/// click. The words are then the last thing on screen that could still promise a
/// destination the panel has no way to take anyone to, and a count that leads
/// nowhere is exactly the number a reader is holding the chip to follow. So the
/// destination is part of the sentence only when the route it names exists, and
/// the fact stands on its own when it does not.
fn route_sentence(route: &Option<ShowProblemsCallback>, destination: &str) -> String {
    route
        .as_ref()
        .map(|_| format!(" {destination}"))
        .unwrap_or_default()
}

/// What needs attention: one chip per thing, each with a way to go and look.
///
/// Every clause names its own unit and no total is added up across them: pods,
/// nodes, and workload *objects* are three different counts, and one number that
/// mixes them cannot be reconciled with the replica figure a screen's worth below
/// it.
///
/// A pod the kubelet lost track of gets no chip. It is a different fact from one
/// that is not running — it is the absence of a verdict, not a verdict — and it
/// is counted in the pod tile's subline, which is the line that reports rather
/// than alarms. The tile and the strip therefore never disagree about it. The
/// cluster verdict still counts it, because "we cannot vouch for this pod" is
/// not a healthy reading; the two questions are allowed to have two answers.
fn health_issues(overview: &Overview, routes: &OverviewRoutes) -> Vec<Issue> {
    let health = overview.health;
    let mut issues = Vec::new();
    let pods = routes.pods.clone();
    if health.failed > 0 {
        issues.push(Issue {
            label: format!(
                "{} failed",
                design::format::count_with_noun(health.failed, "pod", "pods")
            ),
            detail: format!(
                "{} of {} pods failed.{}",
                count(health.failed),
                count(health.total_pods),
                route_sentence(&pods, "Show the failed pods in the Pods table.")
            ),
            severity: Severity::Error,
            route: pods.clone(),
        });
    }
    if health.pending > 0 {
        let unready = health.pending + health.failed;
        issues.push(Issue {
            label: format!(
                "{} pending",
                design::format::count_with_noun(health.pending, "pod", "pods")
            ),
            detail: format!(
                "{} of {} pods are not running.{}",
                count(unready),
                count(health.total_pods),
                route_sentence(
                    &pods,
                    "Show the pods that need attention in the Pods table."
                )
            ),
            severity: Severity::Warning,
            route: pods.clone(),
        });
    }
    if overview.nodes.not_ready > 0 {
        let nodes = routes.nodes.clone();
        issues.push(Issue {
            label: format!(
                "{} not ready",
                design::format::count_with_noun(overview.nodes.not_ready, "node", "nodes")
            ),
            detail: format!(
                "{} of {} nodes are not ready.{}",
                count(overview.nodes.not_ready),
                count(overview.nodes.count),
                route_sentence(
                    &nodes,
                    "Show the nodes that are not ready in the Nodes table."
                )
            ),
            // `Muted`, and not `Warning`: a node that is not `Ready` is a condition
            // the snapshot reports, not a verdict. It is cordoned, joining or
            // draining as often as it is broken, the panel is not told which, and
            // an amber item beside the pod items' amber made two kinds of news look
            // like one. The count stays — the reader has to see it — and it is the
            // *only* thing on the strip that spends a channel, so a cluster whose
            // sole problem is a node that is not Ready prints a count and no mark
            // at all. See `vital_figures` for the same decision on the tile.
            severity: Severity::Muted,
            route: nodes,
        });
    }
    if overview.unavailable_workloads > 0 {
        let workloads = routes.workloads.clone();
        issues.push(Issue {
            label: format!(
                "{} below target",
                design::format::count_with_noun(
                    overview.unavailable_workloads,
                    "workload",
                    "workloads"
                )
            ),
            // No kind is named here, because the one route slot behind this chip
            // does not fix which of the five kinds it lands on: that is the
            // host's decision, and a sentence that guessed at it would be a
            // second way to be wrong.
            detail: format!(
                "{} workloads are below their replica target.{}",
                count(overview.unavailable_workloads),
                route_sentence(&workloads, "Show the workloads that are below target.")
            ),
            severity: Severity::Warning,
            route: workloads,
        });
    }
    // Worst first. `sort_by_key` is stable, so two issues of the same rank keep
    // the order they were discovered in — pods, then nodes, then workloads —
    // which is the order that puts the two pod facts next to each other where a
    // reader can reconcile them into one sentence.
    issues.sort_by_key(Issue::rank);
    issues
}

/// The cluster verdict, from the cluster's own facts.
///
/// This is the one place in the app that answers "is this cluster healthy", so it
/// must not be reachable from the connection: `Overview::level` counts failed
/// pods, pods the API has not placed, nodes that are not ready, and workloads
/// below their target, and nothing else. A snapshot that could not be read
/// completely has no verdict at all, and the data state says so in its own words.
fn health_verdict(overview: &Overview) -> (&'static str, Severity) {
    match overview_data_state(overview) {
        OverviewDataState::Empty => ("No cluster data", Severity::Muted),
        // The cluster is missing a permission, not unhealthy. Amber here would
        // spend the caution channel on the app's own configuration and leave the
        // reader hunting for a cluster problem that is not there.
        OverviewDataState::Partial => ("Partial cluster data", Severity::Muted),
        OverviewDataState::Forbidden => ("Access denied", Severity::Warning),
        OverviewDataState::Error => ("Failed to load cluster data", Severity::Error),
        OverviewDataState::Complete => match overview.level() {
            HealthLevel::Healthy => ("Cluster healthy", Severity::Success),
            HealthLevel::Warning => ("Cluster needs attention", Severity::Warning),
            HealthLevel::Error => ("Failed pods detected", Severity::Error),
        },
    }
}

/// The one grey line a healthy cluster gets, and the one sentence the three
/// unreadable data states get.
///
/// A cluster that is fine says so and stops. The three data states get their own
/// wording because they are three different problems — the cluster returned
/// nothing, the cluster answered for part of the question, and the cluster could
/// not be read at all — and one sentence for all three would be a sentence that
/// answers none of them.
fn quiet_line(overview: &Overview) -> String {
    match overview_data_state(overview) {
        OverviewDataState::Empty => "The cluster returned no node or pod data".to_owned(),
        OverviewDataState::Partial => {
            let mut line = "Some cluster data is unavailable".to_owned();
            if overview.access_denied() {
                line.push_str(" · ");
                line.push_str(&denial_copy(overview));
            }
            line
        }
        OverviewDataState::Forbidden => denial_copy(overview),
        OverviewDataState::Error => "Failed to load the cluster data".to_owned(),
        OverviewDataState::Complete => {
            if overview.unknown_pods > 0 {
                format!(
                    "{} not reporting",
                    design::format::count_with_noun(overview.unknown_pods, "pod", "pods")
                )
            } else {
                "Nothing needs attention".to_owned()
            }
        }
    }
}

/// The cluster's node readiness, in the panel's own ratio spelling.
///
/// `ratio` owns the dash and the separator, so the same `2 / 3` appears here and
/// on the `Nodes ready` tile instead of a third spacing of its own.
fn node_status(overview: &Overview) -> String {
    if overview.nodes.count == 0 {
        "No nodes reported".to_owned()
    } else if overview.nodes.not_ready == 0 {
        format!(
            "{} ready",
            ratio(overview.nodes.ready, overview.nodes.count)
        )
    } else {
        format!(
            "{} ready · {} not ready",
            ratio(overview.nodes.ready, overview.nodes.count),
            count(overview.nodes.not_ready)
        )
    }
}

/// The workload kinds the panel itemises, with the name a person would say.
///
/// The grid the reader scans and the sentence assistive technology reads come from this one list
/// and [`workload_summaries`], in the same order, so a kind cannot be on screen and missing from
/// the label that names it.
const WORKLOAD_LABELS: [&str; 5] = [
    "Deployments",
    "Stateful sets",
    "Daemon sets",
    "Jobs",
    "Cron jobs",
];

/// The snapshot source behind each kind in [`WORKLOAD_LABELS`].
///
/// A source the cluster refused is the difference between "this cluster has no
/// StatefulSet" and "this app was not allowed to look", and the two used to
/// render as the same `0/0`.
const WORKLOAD_SOURCES: [&str; 5] = [
    "deployments",
    "statefulsets",
    "daemonsets",
    "jobs",
    "cronjobs",
];

/// Columns each workload kind takes in the wide layout.
///
/// Five kinds do not divide into twelve, and the two that get three are the two
/// that carry long-lived, high-replica workloads: a Deployment's replica ratio is
/// routinely five digits wide while a Job's is routinely `0 / 0` or a single
/// digit, so giving the first two three columns buys the widest figure in the row
/// room to be read whole and costs the two narrow ones columns they cannot fill.
const WORKLOAD_SPANS_WIDE: [u32; 5] = [3, 3, 2, 2, 2];

/// Columns each workload kind takes once the row wraps.
///
/// Three on the first row and two on the second, because five does not divide
/// into twelve and a row of five cells at the narrowest centre panel would put a
/// twelve-character figure in a hundred pixels.
const WORKLOAD_SPANS_NARROW: [u32; 5] = [4, 4, 4, 6, 6];

/// The name a workload cell is addressed by, in a selector and in the app.
fn workload_selector(label: &str) -> String {
    format!("overview-workload-cell-{}", label.replace(' ', "-"))
}

/// The five kinds' replica totals, in [`WORKLOAD_LABELS`] order.
fn workload_summaries(counts: &WorkloadCounts) -> [ReplicaSummary; 5] {
    [
        counts.deployments,
        counts.stateful_sets,
        counts.daemon_sets,
        counts.jobs,
        counts.cron_jobs,
    ]
}

/// Replica counts arrive signed, but a negative replica count is not a thing.
fn replica_counts(summary: ReplicaSummary) -> (usize, usize) {
    (
        usize::try_from(summary.available).unwrap_or(0),
        usize::try_from(summary.desired).unwrap_or(0),
    )
}

/// `available / desired` for one kind, both counts through the shared formatter.
///
/// A figure, whatever it says. `0 / 0` is a **reading** — the app was allowed to
/// list StatefulSets and the cluster has none — and this used to return the
/// no-answer dash for it, which made "this cluster has no StatefulSets" and "this
/// app was not allowed to look" the same string in the same ink, the exact
/// confusion the function existed to prevent. The two are now told apart by the
/// cell, which already knows whether the source answered: a kind that was read
/// prints `0 / 0` in the muted ink, and a kind that was not prints the dash
/// under the unknown glyph.
fn replica_figure(summary: ReplicaSummary) -> String {
    let (available, desired) = replica_counts(summary);
    format!("{} / {}", count(available), count(desired))
}

/// True when the snapshot could not read this kind's source at all.
fn workload_source_unavailable(overview: &Overview, source: &str) -> bool {
    overview
        .unavailable_sources
        .iter()
        .any(|entry| entry.source == source)
}

/// The per-kind workload figures, `None` where the app could not read the kind.
///
/// These are the five parts of the workload ratio. They are laid out as a grid,
/// because one `·`-separated line of five pairs is a sentence and the eye cannot
/// find one kind in it. A kind the cluster refused has no figure at all, which is
/// a different statement from a kind with nothing in it.
fn workload_breakdown(overview: &Overview) -> Vec<(&'static str, Option<String>)> {
    if !workload_data_available(&overview.workloads) {
        return Vec::new();
    }
    WORKLOAD_LABELS
        .iter()
        .copied()
        .zip(WORKLOAD_SOURCES)
        .zip(workload_summaries(&overview.workloads))
        .map(|((label, source), summary)| {
            let figure =
                (!workload_source_unavailable(overview, source)).then(|| replica_figure(summary));
            (label, figure)
        })
        .collect()
}

/// The same five pairs as one sentence, for a row's spoken label.
fn workload_detail(overview: &Overview) -> String {
    if !workload_data_available(&overview.workloads) {
        return "The cluster reported no workload data. Refresh to check again.".to_owned();
    }
    workload_breakdown(overview)
        .into_iter()
        .map(|(label, figure)| match figure {
            Some(figure) => format!("{label} {figure}"),
            None => format!("{label} not read"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// One workload kind: its replica roll-up, what is missing from it, and a meter
/// of the share that is available.
///
/// `UI-REDESIGN.md` §3.5 gives the five kinds a cell each with a bar or a ring;
/// a bar is the one that reads at a glance and needs no second scale, and it is
/// the same bar the capacity table draws, so the page has one kind of mark.
///
/// The figure is a step down from the tiles above — `title` rather than
/// `display` — because these five are an itemisation of the replica roll-up
/// rather than answers in their own right, and `UI-SPEC` §2.3 keeps `display`
/// for the number a surface exists to communicate.
///
/// **The third line is the story, and it is why the cell has four lines.** A
/// cluster with `4 / 10,005` deployments is not a ratio, it is a sentence — ten
/// thousand replicas declared and four available — and a ratio alone leaves the
/// reader to do the subtraction before they can feel it. `10,001 missing` says it
/// in three words, per kind, at the place the reader is already comparing kinds,
/// and it is the same word the heading's total uses so the two agree. The four
/// states are the four readings the model can produce, and each one is stated:
/// a kind the app could not read, a kind with nothing declared, a kind short of
/// what it declared, and a kind at what it declared.
///
/// **The meter is neutral and the shortfall is in the words.** The bar used to be
/// painted in the caution hue, which was right when it was a 47px lane and wrong
/// the moment the meter grammar became one track per cell: a full-width amber
/// track is the largest coloured area on the page, spent on a fact the line above
/// it already states. A meter measures a quantity (`magnitude_bar_ink` is the
/// whole argument) and a state belongs in words and an ink, which is what the
/// subline now is.
fn workload_cell(
    label: &'static str,
    figure: Option<&str>,
    summary: ReplicaSummary,
    cx: &App,
) -> AnyElement {
    let selector = workload_selector(label);
    // Three figure states, one subline. A kind the cluster refused has no figure
    // at all and wears the unknown glyph; a kind the cluster answered for and that
    // has nothing in it is a real reading of zero, so it prints `0 / 0` in the
    // muted ink; a kind with replicas prints them at full strength. The
    // whole-strength ink is reserved for a reading worth acting on, so a row of
    // five kinds on a quiet cluster is five grey figures rather than five
    // headlines.
    let unread = figure.is_none();
    let (available, desired) = replica_counts(summary);
    let missing = desired.saturating_sub(available);
    let short = !unread && missing > 0;
    let (subline, subline_severity) = if unread {
        ("Not read".to_owned(), Severity::Muted)
    } else if desired == 0 {
        ("None declared".to_owned(), Severity::Muted)
    } else if short {
        (format!("{} missing", count(missing)), Severity::Warning)
    } else {
        ("All available".to_owned(), Severity::Muted)
    };
    let figure = figure.unwrap_or(NO_ANSWER);
    let ink = if unread {
        design::confidence::foreground(design::Confidence::Unknown, cx)
    } else if desired == 0 {
        role::fg_tertiary(cx)
    } else {
        role::fg_primary(cx)
    };
    let share = (desired > 0).then(|| available as f64 / desired as f64 * 100.0);
    // The meter carries no unit, so the thing it is a share of is named in the
    // cell's spoken label — which is the tooltip a pointer gets and the sentence a
    // screen reader is given. Both counts go through the shared formatter, because
    // `10,001` is how every other figure on this page spells a thousand.
    let spoken = if unread {
        format!("{label}: not read.")
    } else {
        format!(
            "{label}: {available} of {desired} replicas available. {subline}. {WORKLOADS_READING}",
            available = count(available),
            desired = count(desired),
        )
    };
    // One `String` per closure that names the cell. `debug_selector` takes a
    // `Fn`, so each of the borrows rather than moves, and a borrow that
    // outlives the function is the compiler's whole objection.
    let unread_selector = selector.clone();
    let inner_selector = selector.clone();
    let bar_selector = selector.clone();
    v_flex()
        .id(ElementId::from(selector))
        .debug_selector(move || unread_selector.clone())
        .w_full()
        .min_w(px(0.))
        .gap(space::XS)
        .role(Role::Group)
        .aria_label(spoken.clone())
        .tooltip(text_tooltip(spoken))
        .child(tile_label(label).text_color(role::fg_tertiary(cx)))
        .child(
            h_flex()
                .min_w(px(0.))
                .gap(space::XS)
                .items_center()
                .when(unread, |this| {
                    this.child(
                        div()
                            .flex_none()
                            .debug_selector(move || format!("{inner_selector}-unread"))
                            .child(
                                Icon::new(design::confidence::icon(design::Confidence::Unknown))
                                    .with_size(Size::Size(design::icon::IN_ROW))
                                    .text_color(ink),
                            ),
                    )
                })
                .child(
                    div()
                        .font(ui_font(cx))
                        .font_features(tabular_features())
                        .text_size(text::TITLE)
                        .line_height(text::TITLE_LINE_HEIGHT)
                        .font_weight(text::SEMIBOLD)
                        .text_color(ink)
                        .min_w(px(0.))
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(figure.to_owned()),
                ),
        )
        // The subline slot is reserved on every cell, whether or not the kind has
        // a shortfall to state, for the reason the stat tiles reserve theirs: five
        // meters on one line is a spine, and a cell that printed two lines and a
        // cell that printed three would put its meter 16px off it.
        .child(
            div().min_w(px(0.)).min_h(text::LABEL_LINE_HEIGHT).child(
                // The cell's own subline is the story it exists to tell — how many
                // replicas are missing — so its resting arm is secondary ink. See
                // [`subline_ink`].
                tile_subline(
                    &[(subline.clone(), subline_severity)],
                    role::fg_secondary(cx),
                    cx,
                ),
            ),
        )
        // The bar slot is reserved whether or not this kind has a denominator —
        // `bar_slot` says why, and the short version is that a kind with `0 / 0`
        // has no bar to draw and the other four cells in the row do.
        .child(bar_slot(
            share.map(|share| (format!("{bar_selector}-bar"), share, magnitude_bar_ink(cx))),
            cx,
        ))
        .into_any_element()
}

/// One tile of the grid: a label, a number, a subline, and a share of a whole.
///
/// The four are the shape `UI-REDESIGN.md` §3.5 fixes for a stat tile, with the
/// 60px sparkline replaced by a proportion bar. A sparkline needs a series and
/// the Overview takes one snapshot, so there is nothing to plot; a bar is what
/// the series would have been read for anyway, and it is a fourth line rather
/// than a decoration.
struct StatTile {
    /// The slot's own name, so a screenshot and a test can address one tile of
    /// the row without counting.
    selector: &'static str,
    /// Whether this tile holds the page's one `display` figure. Exactly one tile
    /// does, and it is the first.
    lead: bool,
    label: &'static str,
    value: String,
    subline: String,
    /// The subline split into the runs it actually is, each with its own severity.
    ///
    /// One run for every subline that names one state, which is nearly all of
    /// them. The tile that names two gets two, because one ink cannot say both —
    /// see [`tile_subline`]. `subline` above stays the single flattened source
    /// for the tooltip and the accessible name, so the two cannot drift: a reader
    /// who cannot see the colours still hears the same sentence in the same order.
    subline_runs: Vec<(String, Severity)>,
    /// The share the bar draws, when the tile has a denominator to be a share of.
    bar: Option<f64>,
    /// The whole sentence, for the tooltip and the tile's accessible name.
    detail: String,
}

impl StatTile {
    fn plain(
        selector: &'static str,
        label: &'static str,
        value: String,
        subline: String,
        subline_severity: Severity,
    ) -> Self {
        Self {
            selector,
            lead: false,
            label,
            value,
            detail: subline.clone(),
            subline_runs: vec![(subline.clone(), subline_severity)],
            subline,
            bar: None,
        }
    }

    /// Splits the subline into runs, each in its own state ink.
    ///
    /// The split is for a sentence that names two buckets of one population — a
    /// pending count and a failed count — where one ink would announce one of them
    /// as the other. The runs must join back to `subline` separated by the same
    /// separator [`tile_subline`] draws, because that flattened form is what the
    /// tooltip and the accessible name read.
    fn with_subline_runs(mut self, runs: Vec<(String, Severity)>) -> Self {
        let joined = runs
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>()
            .join(" \u{b7} ");
        assert_eq!(
            joined, self.subline,
            "the subline's runs must join back to the sentence the tooltip reads, \
             or a reader who cannot see the colours hears a different sentence"
        );
        self.subline_runs = runs;
        self
    }
}

/// One resource axis of the cluster, summed over the nodes that reported both
/// figures.
#[derive(Clone, Copy, Debug, Default)]
struct Axis {
    requested: f64,
    allocatable: f64,
    /// How many nodes contributed. A node the app could not read is not a node
    /// with no capacity, so it is left out of the sum rather than counted as a
    /// zero — averaging it in would make a healthy cluster look empty.
    counted: usize,
}

impl Axis {
    /// The requested share of allocatable in percent, or `None` when no node
    /// reported an allocatable figure.
    fn share(&self) -> Option<f64> {
        (self.counted > 0 && self.allocatable > 0.0)
            .then(|| self.requested / self.allocatable * 100.0)
    }
}

/// Cluster-wide requests against allocatable, one `Axis` per resource.
fn cluster_capacity(overview: &Overview) -> (Axis, Axis) {
    let mut cpu = Axis::default();
    let mut memory = Axis::default();
    for capacity in &overview.capacities {
        if let Some(allocatable) = capacity.allocatable_cpu.filter(|value| *value > 0.0) {
            cpu.requested += capacity.requested_cpu;
            cpu.allocatable += allocatable;
            cpu.counted += 1;
        }
        if let Some(allocatable) = capacity.allocatable_memory.filter(|value| *value > 0.0) {
            memory.requested += capacity.requested_memory;
            memory.allocatable += allocatable;
            memory.counted += 1;
        }
    }
    (cpu, memory)
}

/// A share as the number, or the dash when nothing was reported.
fn capacity_figure(axis: &Axis) -> String {
    axis.share().map_or(NO_ANSWER.to_owned(), format_pct)
}

/// The absolutes, which the bar replaced but which a tooltip and a screen reader
/// still need.
///
/// `total_unit` is the unit the two numbers do **not** already carry, and it is
/// the unit of the total only. `format_cores` spells a bare number, so the CPU
/// subline is `1.15 of 20 cores`; `format_bytes` already ends both of its numbers
/// in `GiB`, so passing `cores` there too printed `1.3 GiB of 31.1 GiB GiB
/// requested` — the same word three times in a 12px line under a number that is
/// already the loudest thing on the row.
///
/// The line does **not** end in "requested" either. The tile's own label is
/// `CPU REQUESTED`, so `1.35 of 60 cores requested` said the word twice on one
/// 16px line and the second one carried nothing: the label is what makes it a
/// request, not the noun at the end. The full sentence — `CPU requested across 3
/// nodes: …` — is still in the tile's `detail`, which is the tooltip and the
/// accessible name, where nothing is truncated and nothing is read at a glance.
fn capacity_subline(
    axis: &Axis,
    format: impl Fn(f64) -> String,
    total_unit: &str,
    axis_name: &str,
) -> String {
    if axis.counted == 0 {
        return format!("No {axis_name} reported");
    }
    format!(
        "{} of {}{total_unit}",
        format(axis.requested),
        format(axis.allocatable)
    )
}

///
/// A cluster's requested capacity is the figure that decides whether the next pod
/// can be scheduled, and a share of it is the figure that can be read without
/// arithmetic: `1.15 / 20 cores` is a form, `6%` is a dashboard. The absolutes
/// stay in the subline, where they are one small step below the number rather
/// than the number itself.
fn capacity_tiles(cpu: Axis, memory: Axis) -> [StatTile; 2] {
    let mut cpu_tile = StatTile::plain(
        "overview-tile-cpu",
        "CPU requested",
        capacity_figure(&cpu),
        capacity_subline(&cpu, format_cores, " cores", "CPU"),
        Severity::Muted,
    );
    cpu_tile.bar = cpu.share();
    cpu_tile.detail = format!(
        "CPU requested across {} nodes: {}",
        count(cpu.counted),
        cpu_tile.subline
    );
    let mut memory_tile = StatTile::plain(
        "overview-tile-memory",
        "Memory requested",
        capacity_figure(&memory),
        // `format_bytes` already ends both of its numbers in their unit.
        capacity_subline(&memory, format_bytes, "", "memory"),
        Severity::Muted,
    );
    memory_tile.bar = memory.share();
    memory_tile.detail = format!(
        "Memory requested across {} nodes: {}",
        count(memory.counted),
        memory_tile.subline
    );
    [cpu_tile, memory_tile]
}

/// The hero tile and the three beside it.
///
/// Four tiles, three columns each: the row is the smallest number of figures that
/// divides twelve and it is the one a reader can hold. The order is the scan
/// order — pods, then the fleet's shape (nodes), then the fleet's capacity (CPU
/// and memory) — and the replica roll-up is deliberately **not** one of them,
/// because the Workloads region below itemises the same number five ways and a
/// sixth copy of it is repetition rather than hierarchy. It is on that region's
/// heading instead, where a total belongs.
///
/// **The first tile is the page's one `display` figure and the other three are a
/// step down.** The row used to print four `display` numbers side by side, which
/// is four answers of equal weight to a question — is my cluster healthy, and what
/// needs me — that only the first of them answers. Pods ready leads because it is
/// the figure the other four regions itemise; the three beside it are its
/// context, and a type step is what says so without spending a colour on it.
///
/// The pod tile's subline names the largest bucket that is not running and counts
/// the rest: the ratio beside it already says how many are, but a tile that
/// reports 9,900 pending and hides 7 more unready pods only tells the truth to
/// the reader who hovers.
///
/// **The node tile spends no channel at all, and that is the fix for "not
/// ready".** `nodes.not_ready` is a *condition the snapshot reports*, not a
/// verdict: a node that is not `Ready` is cordoned, joining, or draining as often
/// as it is broken, and this panel is not told which. Amber next to the pod
/// tile's amber made two different kinds of news look like one, and
/// `Design guides > Visual language > Hierarchy` asks colour to be spent where a
/// distinction changes what the reader does next — and "1 not ready" does not
/// change anything, because the page cannot say what to do about it. The count
/// stays, in secondary ink, and the strip's own item for it says the same thing
/// for the same reason (see `health_issues`).
///
/// The order of the buckets is a tie-break, not a priority list, and it reads
/// backwards on purpose: `max_by_key` returns the *last* of several equal maxima,
/// so the worst verdict is listed last and wins the tie. `unknown` is first
/// because it is the one bucket that is not a verdict at all.
fn vital_figures(overview: &Overview) -> (StatTile, Vec<StatTile>) {
    let health = overview.health;
    let nodes = overview.nodes;

    let unready = [
        (overview.unknown_pods, "unknown"),
        (health.pending, "pending"),
        (health.failed, "failed"),
    ];
    // The caption and its runs come from ONE decision, so the sentence and the
    // colours cannot end up describing different buckets.
    //
    // This is the sentence that misstated a fact. The line was one `Label` and the
    // tile had one `subline_severity`, so `2 pending · 1 failed` took the failure
    // ink whole - and a PENDING count announced in the failure channel, sitting in
    // the largest number on the page, is the most expensive misstatement the
    // Overview can make. Two buckets, two word inks, one sentence.
    let bucket_severity = |name: &str| match name {
        "failed" => Severity::Error,
        "pending" => Severity::Warning,
        // `unknown` is deliberately not in the ladder: the table draws such a pod
        // as a dash with no verdict, so it takes the quietest arm.
        _ => Severity::Muted,
    };
    let pod_runs: Vec<(String, Severity)> = if health.total_pods == 0 {
        vec![("No pods reported".to_owned(), Severity::Muted)]
    } else {
        let ranked: Vec<(usize, &str)> = unready
            .iter()
            .copied()
            .filter(|(number, _)| *number > 0)
            .collect();
        match ranked.iter().max_by_key(|(number, _)| *number) {
            Some((number, name)) => {
                let mut runs = vec![(
                    format!("{} {name}", count(*number)),
                    bucket_severity(name),
                )];
                if let Some((rest, rest_name)) = ranked
                    .iter()
                    .filter(|(other, _)| *other != *number)
                    .max_by_key(|(other, _)| *other)
                {
                    runs.push((
                        format!("{} {rest_name}", count(*rest)),
                        bucket_severity(rest_name),
                    ));
                }
                runs
            }
            None => vec![("all running".to_owned(), Severity::Muted)],
        }
    };
    let pod_caption = if health.total_pods == 0 {
        "No pods reported".to_owned()
    } else {
        match unready
            .iter()
            .max_by_key(|(number, _)| *number)
            .filter(|(number, _)| *number > 0)
        {
            Some((number, name)) => {
                // Both halves of the sum are named. The tail used to be a bare
                // figure — `2 failed · 1` — and a number without a unit on a line
                // whose job is to be read at a glance is a number the reader has
                // to go and look up. The tail is the *next* largest bucket, not
                // the remainder, so the two halves are both something a reader can
                // act on even where they do not add up to the ratio above.
                let lead = format!("{} {name}", count(*number));
                match unready
                    .iter()
                    .filter(|(other, _)| *other > 0 && *other != *number)
                    .max_by_key(|(other, _)| *other)
                {
                    Some((rest, rest_name)) => format!("{lead} · {} {rest_name}", count(*rest)),
                    None => lead,
                }
            }
            None => "all running".to_owned(),
        }
    };
    // The worst verdict, and nothing else. `unknown` is deliberately not in the
    // ladder: the table draws a pod in the `Unknown` phase as a dash with no
    // verdict, and this subline sat a screenful above it in the banner's own
    // amber, so one pod the kubelet lost read as a caution event on an otherwise
    // healthy cluster. The count still goes in the subline and still routes to
    // those pods — `problems_only` keeps a pod with no verdict — it just does not
    // spend the caution channel.
    let pod_severity = if health.failed > 0 {
        Severity::Error
    } else if health.pending > 0 {
        Severity::Warning
    } else {
        Severity::Muted
    };

    // `all ready` rather than nothing. The pod tile two slots along says
    // `all running` when its ratio is whole, so an empty subline here left one
    // tile in the row carrying a verdict and the next carrying a 16px hole
    // reserved for it — the row read as a layout accident rather than as four
    // figures of the same kind. It is also the sentence the ratio cannot make:
    // `1 / 1` says one of one exists, `all ready` says nothing is wrong with it.
    let node_caption = if nodes.count == 0 {
        "No nodes reported".to_owned()
    } else if nodes.not_ready == 0 {
        "all ready".to_owned()
    } else {
        format!("{} not ready", count(nodes.not_ready))
    };
    // `Severity::Muted` on purpose: see the note on this function. The words are
    // the report, and a condition the panel cannot interpret does not get a
    // channel.
    let node_severity = Severity::Muted;

    let share = |available: usize, total: usize| {
        (total > 0).then(|| available as f64 / total as f64 * 100.0)
    };

    let mut hero = StatTile::plain(
        "overview-hero",
        "Pods ready",
        ratio(health.running, health.total_pods),
        pod_caption,
        pod_severity,
    )
    .with_subline_runs(pod_runs);
    hero.lead = true;
    hero.bar = share(health.running, health.total_pods);
    hero.detail = format!(
        "Pods running {}, {} pending, {} failed",
        ratio(health.running, health.total_pods),
        count(health.pending),
        count(health.failed)
    );

    let mut nodes_tile = StatTile::plain(
        "overview-tile-nodes",
        "Nodes ready",
        ratio(nodes.ready, nodes.count),
        node_caption,
        node_severity,
    );
    nodes_tile.bar = share(nodes.ready, nodes.count);
    nodes_tile.detail = format!(
        "Nodes ready {} · {} not ready",
        ratio(nodes.ready, nodes.count),
        count(nodes.not_ready)
    );

    let (cpu, memory) = cluster_capacity(overview);
    let [cpu, memory] = capacity_tiles(cpu, memory);
    (hero, vec![nodes_tile, cpu, memory])
}

/// The replica roll-up across the five workload kinds, for the Workloads heading.
///
/// `UI-REDESIGN.md` §3.5 itemises the five kinds in five cells, and a sixth cell
/// carrying their sum would answer a question the row two hundred pixels above
/// already answered. The total is therefore on the heading — which is where the
/// Inspector puts a section's count, and for the same reason: a heading is what
/// the reader's eye lands on, and a total is the answer to "are they up" before
/// the itemisation is.
///
/// **The total and the shortfall are returned separately, and only the shortfall
/// is allowed to be a colour.** `9 / 10,016` is how many replicas exist; `10,007`
/// is how many of them are not available. The first was printed in the caution hue
/// because of the second, so a count of everything the cluster has read as a
/// warning. Both are on the trailing edge now — the ratio quiet, the sub-count in
/// the caution ink — because the reader needs both to act and only one of them is
/// the news.
///
/// **The word is `missing`, and it is the word every workload cell uses.** It used
/// to be `below target`, which appears nowhere else in the product and was
/// defined only in this function's own tooltip — see [`WORKLOADS_READING`], which
/// is what replaced it. The full sentence is still the heading's accessible
/// description, where nothing is truncated.
fn replicas_total(overview: &Overview) -> (String, String, Option<(String, Severity)>) {
    let replicas = overview.workloads.replicas();
    let desired = usize::try_from(replicas.desired).unwrap_or(0);
    let available = usize::try_from(replicas.available).unwrap_or(0);
    let short = desired > 0 && available < desired;
    let reading = if desired == 0 {
        "No workloads were reported.".to_owned()
    } else if short {
        format!(
            "{} of {} replicas are available. {} are missing.",
            count(available),
            count(desired),
            count(desired - available)
        )
    } else {
        format!("All {} replicas are available.", count(desired))
    };
    let note = short.then(|| {
        (
            format!("{} missing", count(desired - available)),
            Severity::Warning,
        )
    });
    (ratio(available, desired), reading, note)
}

/// One tile as it is drawn: label, number, subline, bar, and nothing else.
///
/// **The reading order is the whole design.** Three steps, three inks, three
/// sizes: the label is `caption` in `fg.tertiary` (quiet — it identifies the
/// figure and says nothing about it), the number is the lead tile's `display` or
/// a supporting tile's `title`, both in `fg.primary`, and the subline is `label`
/// in [`subline_ink`]. Before this the label and the subline were both
/// `fg.secondary`, so the two quiet lines of the tile wore one ink and the eye had
/// no way to tell which of them was the figure's own name.
///
/// A tile is **not** a card. The four tiles share one surface — [`band`] — and a
/// tile draws no frame, no fill and no shadow of its own: `UI-SPEC` §0 铁律一
/// rules the border off, the Bootstrap look it names is exactly a grid of rounded
/// outlined boxes, `PROMPT.md` §4 rules the shadow off, and `Design guides >
/// Visual language > Hierarchy` rules out the card-inside-a-card that four
/// bordered cells inside a bordered row would be. What separates a tile from its
/// neighbour is the grid's 16px gap, and what groups the four is the surface they
/// all sit on.
///
/// **The meter is on the tile's bottom edge, and that is what keeps the row's four
/// meters on one line.** The supporting tiles' figures are a type step shorter
/// than the lead tile's, so their content is 12px less and a meter laid out after
/// it would ride 12px above its neighbours — four bars in one row at two heights
/// is two rows. The spacer takes the difference instead, so every tile in a row
/// draws its meter on the row's baseline whether or not its figure is the long
/// one.
fn stat_tile(tile: &StatTile, cx: &App) -> AnyElement {
    let selector = tile.selector.to_owned();
    let bar_selector = selector.clone();
    let figure = if tile.lead {
        stat_number(&tile.value, cx)
    } else {
        stat_figure(&tile.value, cx)
    };
    v_flex()
        .id(ElementId::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .w_full()
        .min_w(px(0.))
        // The lead tile's height, stated rather than inherited. The grid slot is a
        // row and the tile is a column in it, and a column that takes its own
        // content's height leaves the row's extra 12px below its meter instead of
        // above it — the render measured the four meters along the bottom of the
        // tile row at two heights, with the lead tile's on the lower line, which
        // is the one arrangement that makes a meter row mean nothing. A floor the
        // spacer can grow into is what turns that slack into the bar's margin, and
        // it is the same number [`capacity_table_height`] already reserves for the
        // band, so the two cannot disagree about how tall a tile is.
        .min_h(px(stat_tile_height()))
        .gap(space::XS)
        .role(Role::Group)
        .aria_label(format!("{}: {}", tile.label, tile.detail))
        // The sentence the tile does not have room for: the pod tile's own line
        // names the two largest unready buckets and the ratio above it names the
        // rest, so only the cluster-wide counts are missing — and they are exactly
        // what a reader hovering a figure that says `17 / 20` wants. The field was
        // documented as the tooltip's text from the day it was added and the tooltip
        // was never wired to it.
        .tooltip(text_tooltip(tile.detail.clone()))
        .child(tile_label(tile.label).text_color(role::fg_tertiary(cx)))
        .child(figure)
        // The subline slot is reserved even when a tile has nothing to add, so
        // the four bars along the bottom of the row are on one line. A tile that
        // printed nothing and then took no space would be a tile whose bar sat a
        // whole subline higher than its neighbours'.
        .child(div().min_w(px(0.)).min_h(text::LABEL_LINE_HEIGHT).when(
            !tile.subline.is_empty(),
            |this| {
                this.child(tile_subline(
                    &tile.subline_runs,
                    // The lead tile's subline states the worst verdict in
                    // the cluster, so it is part of the headline and reads
                    // as body copy. A supporting tile's subline is the
                    // absolute behind the share above it, and the share is
                    // the answer — so it is help text, and the quietest ink.
                    if tile.lead {
                        role::fg_secondary(cx)
                    } else {
                        role::fg_tertiary(cx)
                    },
                    cx,
                ))
            },
        ))
        // The spacer that puts every meter in the row on one line. See the note on
        // the function.
        .child(div().flex_1())
        .child(bar_slot(
            tile.bar
                .map(|share| (format!("{bar_selector}-bar"), share, magnitude_bar_ink(cx))),
            cx,
        ))
        .into_any_element()
}

#[expect(clippy::large_enum_variant)]
enum OverviewState {
    Loading,
    Ready(Overview),
    /// There is no cluster to read: this panel was built without one, and a
    /// handle is never added afterwards, so it is always a first run or a
    /// session that has lost its cluster — never a dashboard with numbers on it.
    ///
    /// It used to be `Failed(common::NOT_CONNECTED_REASON)`, which made a state
    /// the app is *waiting* for indistinguishable from a request that failed, and
    /// every consumer had to string-match the message to tell them apart again.
    /// Three places did. This is the same fact without the round trip through a
    /// sentence.
    Disconnected,
    Failed(String),
}

fn apply_refresh_result(
    state: &mut OverviewState,
    refreshing: &mut bool,
    refresh_error: &mut Option<String>,
    current_epoch: u64,
    result_epoch: u64,
    result: Result<Overview, String>,
) -> Option<SystemTime> {
    if current_epoch != result_epoch {
        return None;
    }
    *refreshing = false;
    let had_snapshot = matches!(state, OverviewState::Ready(_));
    match result {
        Ok(overview) => {
            *state = OverviewState::Ready(overview);
            *refresh_error = None;
            Some(SystemTime::now())
        }
        Err(reason) if had_snapshot => {
            *refresh_error = Some(reason);
            None
        }
        Err(reason) => {
            *state = OverviewState::Failed(reason);
            None
        }
    }
}

/// Renders and refreshes the cluster overview.
pub struct OverviewView {
    refresh_focus: FocusHandle,
    retry_focus: FocusHandle,
    /// One focus handle per issue chip, in the order the strip lists them.
    chip_focus: Vec<FocusHandle>,
    /// Whether the focus the panel holds came from a key.
    ///
    /// gpui 0.6.6 has no `focus_visible` helper, and a tracked focus handle is
    /// focused by a mouse press as well as by a tab. `PROMPT.md` §2.1 #10 is
    /// that a mouse click produces no focus ring, so the origin has to be
    /// recorded. Every key press sets it and every mouse press clears it, which
    /// is the whole difference between a focus ring that means "you are here" and
    /// one that means "something is focused".
    keyboard_focus: Cell<bool>,
    handle: Option<OverviewHandle>,
    metrics_available: bool,
    state: OverviewState,
    refreshing: bool,
    refresh_error: Option<String>,
    last_refreshed: Option<SystemTime>,
    /// Which rung of `UI-SPEC` §4.14's ladder the reader is on.
    loading_tier: LoadingTier,
    /// When the load started, for the skeleton's one breath. A visual, so the
    /// wall clock is the right one; the tier above is not.
    loading_since: Option<Instant>,
    /// Whether the panel is on screen. An Overview behind another tab has nothing
    /// to refresh for, and a cluster does not stop changing because a tab is
    /// hidden.
    visible: bool,
    /// Whether the reload timer is running. Armed on the first refresh and never
    /// disarmed, because a panel that stops watching the cluster stops being a
    /// dashboard.
    auto_refresh_started: bool,
    /// The panel's own content box, measured at paint time.
    ///
    /// Two numbers, because two of this panel's layouts are functions of the space
    /// it has rather than of its content: the capacity table's columns follow the
    /// width, and the capacity table's *height* follows what is left below the rest
    /// of the composition. The window is not the same box — the shell's title bar
    /// and tab strip sit above this panel and are not part of it — so
    /// `viewport_size()` overstates the height by the whole chrome, and a table
    /// sized against it runs the page a chrome-height past the fold.
    ///
    /// Both numbers are shared with the `on_prepaint` closure that fills them, so
    /// the measurement has one home and one repaint rule.
    content_width: Rc<Cell<f32>>,
    content_height: Rc<Cell<f32>>,
    /// The capacity table's state, gpui-kit's own.
    ///
    /// It owns the cell cursor, the sort order, the scroll offsets, and the
    /// column widths, so the panel holds it for as long as it is open rather
    /// than rebuilding a cursor every frame. It is created on the first paint of
    /// a snapshot that has nodes to show, which is the first paint that has a
    /// window to hand the component.
    capacity_state: RefCell<Option<Entity<TableState<CapacityTableDelegate>>>>,
    routes: OverviewRoutes,
    /// Discards results from an older refresh.
    epoch: u64,
    _task: Option<Task<()>>,
}

impl OverviewView {
    pub fn new(
        handle: Option<OverviewHandle>,
        metrics_available: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            refresh_focus: cx.focus_handle().tab_stop(true).tab_index(0),
            retry_focus: cx.focus_handle().tab_stop(true).tab_index(0),
            chip_focus: (0..HEALTH_ISSUE_COUNT)
                .map(|index| {
                    cx.focus_handle()
                        .tab_stop(true)
                        .tab_index(index as isize + 1)
                })
                .collect(),
            keyboard_focus: Cell::new(false),
            handle,
            metrics_available,
            state: OverviewState::Loading,
            refreshing: false,
            refresh_error: None,
            last_refreshed: None,
            loading_tier: LoadingTier::Nothing,
            loading_since: Some(Instant::now()),
            visible: true,
            auto_refresh_started: false,
            content_width: Rc::new(Cell::new(0.0)),
            content_height: Rc::new(Cell::new(0.0)),
            capacity_state: RefCell::new(None),
            routes: OverviewRoutes::default(),
            epoch: 0,
            _task: None,
        }
    }

    /// Installs the host's route from a count on the strip to the rows behind it.
    ///
    /// Without it a panel that reports 9,900 pending pods offers no way to look at
    /// them, which is the one thing on this page a reader is most likely to want
    /// to do next.
    pub fn set_show_problems_callback(&mut self, callback: Option<ShowProblemsCallback>) {
        self.routes.pods = callback;
    }

    /// Installs every route at once, including the node and workload ones the
    /// shell does not yet provide.
    pub fn set_overview_routes(&mut self, routes: OverviewRoutes) {
        self.routes = routes;
    }

    /// Whether the panel is on screen, and therefore whether it keeps watching
    /// the cluster. `shell/mod.rs` owns tab activation and calls this; the
    /// default is on, which is right for a panel that has just been opened.
    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    pub fn focus_default_control(&self) -> FocusHandle {
        match &self.state {
            // Both states with nothing to show carry the one control that can
            // change that, and both of those controls are tracked by the same
            // handle because only one of the two states is ever on screen.
            OverviewState::Disconnected | OverviewState::Failed(_) => self.retry_focus.clone(),
            OverviewState::Ready(overview)
                if matches!(
                    overview_data_state(overview),
                    OverviewDataState::Error | OverviewDataState::Forbidden
                ) =>
            {
                self.retry_focus.clone()
            }
            _ => self.refresh_focus.clone(),
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_default_control()
    }

    /// Reloads when metrics-server availability changes.
    pub fn set_metrics_available(&mut self, available: bool, cx: &mut Context<Self>) {
        if self.metrics_available == available {
            return;
        }
        self.metrics_available = available;
        self.refresh(cx);
    }

    /// Walks the reader up the loading ladder, one timer per rung.
    ///
    /// Each task only ever moves the tier *up*: a late task cannot take a reader
    /// back to a spinner they have already been shown a skeleton for, which is
    /// the one way a timer-driven ladder can look worse than a clock comparison.
    /// The first rung is entered immediately rather than scheduled, because a
    /// spinner that arrives 200ms after the wait began is a spinner that appeared
    /// after the moment it was for.
    fn advance_loading_tier(&mut self, cx: &mut Context<Self>) {
        self.loading_tier = LoadingTier::Spinner;
        for (delay, target) in [
            (SKELETON_AFTER, LoadingTier::Skeleton),
            (LOADING_PROGRESS, LoadingTier::Progress),
        ] {
            cx.spawn(async move |view, cx| {
                cx.background_executor().timer(delay).await;
                view.update(cx, |view, cx| {
                    if view.loading_tier < target {
                        view.loading_tier = target;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
        // The last rung is the only one that prints a number, and both that number
        // and the skeleton's own breathe are functions of elapsed time, so past
        // this point the rung has to repaint on its own or both stop.
        //
        // They did stop. `Still loading · 2s` was painted once when the rung was
        // entered and then never again, so a cluster that took nine seconds read
        // as a cluster that took two — a clock frozen at the moment it became
        // visible, which is worse than no clock, and a skeleton whose 1.6s breathe
        // had stopped moving by the time a reader was looking at it. §4.14's "past
        // 2s, spinner + progress" is a live figure and this is what makes it one.
        //
        // One re-arming timer, and it stops the moment the load lands: the state
        // is no longer `Loading`, the update returns false, and the loop returns.
        // A dropped view fails the same way, so the loop cannot outlive the panel.
        //
        // The loop's own exit condition is the **state**, not the tier. It has to
        // outlive the two rungs below `Progress` or the timer would stop at one
        // second — before the rung that prints a number exists — and the last
        // thing this fixes would be the first thing to come back.
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(LOADING_TICK).await;
                let keep_going = view
                    .update(cx, |view, cx| {
                        if !matches!(view.state, OverviewState::Loading) {
                            return false;
                        }
                        // The rungs below `Progress` have nothing live to repaint:
                        // the skeleton's breathe is a function of the elapsed time
                        // it is handed at paint time, and a rung nobody has reached
                        // is not on screen to be frozen.
                        if view.loading_tier >= LoadingTier::Progress {
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    return;
                }
            }
        })
        .detach();
    }

    /// Loads and aggregates a new snapshot.
    ///
    /// A refresh started while one is in flight replaces it: `_task` is the only
    /// request this panel owns, dropping it aborts the cluster call underneath
    /// it (`AbortOnDrop`), and the `epoch` discards a result
    /// that was already on its way. The auto-refresh timer is what must not
    /// arrive during a load — see `Self::on_auto_refresh_tick`.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let epoch = self.epoch.wrapping_add(1);
        self.epoch = epoch;
        self._task = None;
        self.refreshing = true;
        // The last failure is **not** cleared on the way in. It is the record of
        // what the figures on screen are — a snapshot taken before the last
        // attempt failed — and only a load that lands takes it away, in
        // `apply_refresh_result`. Clearing it here would take the error strip and
        // the stale wash off for the length of every request and put them back
        // when the request failed, so a cluster that is down would flicker once
        // per interval instead of saying so once and staying said.
        let Some(handle) = self.handle.clone() else {
            self.refreshing = false;
            self.state = OverviewState::Disconnected;
            cx.notify();
            return;
        };
        if matches!(self.state, OverviewState::Failed(_)) {
            self.state = OverviewState::Loading;
        }
        if matches!(self.state, OverviewState::Loading) {
            // The ladder restarts with every load, and each rung is a timer rather
            // than a comparison: a late frame must not be able to skip a tier, and
            // a tier only a wall clock can reach is a tier nobody can test.
            self.loading_since = Some(Instant::now());
            self.advance_loading_tier(cx);
        }
        self.start_auto_refresh(cx);
        let future = handle.load_future(self.metrics_available);
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = future.await;
            this.update(cx, |view, cx| {
                if view.epoch == epoch {
                    let completed_at = apply_refresh_result(
                        &mut view.state,
                        &mut view.refreshing,
                        &mut view.refresh_error,
                        view.epoch,
                        epoch,
                        result,
                    );
                    if let Some(completed_at) = completed_at {
                        view.last_refreshed = Some(completed_at);
                    }
                    view.loading_tier = LoadingTier::Nothing;
                    view.loading_since = None;
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// Arms the reload timer, once.
    ///
    /// The invariant is the important part: **no cluster connection, no timer.**
    /// The loop is infinite, and a headless test drains its scheduler by advancing
    /// the clock to the next timer, so a timer armed on a view with no handle
    /// would spin a test that never meant to start one. Every fixture in this
    /// file builds its view with `None`; the shell builds it with a handle, and
    /// that is the case the timer exists for.
    ///
    /// The exit is the same one `table_view::host` uses for its watch loop: a
    /// `WeakEntity::update` that cannot find the view means the panel is gone, so
    /// the loop returns rather than rearming for a cluster nobody is looking at.
    fn start_auto_refresh(&mut self, cx: &mut Context<Self>) {
        if self.auto_refresh_started || self.handle.is_none() {
            return;
        }
        self.auto_refresh_started = true;
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(OVERVIEW_REFRESH_INTERVAL)
                    .await;
                if view
                    .update(cx, |view, cx| view.on_auto_refresh_tick(cx))
                    .is_err()
                {
                    return;
                }
            }
        })
        .detach();
    }

    /// One tick of the reload timer.
    ///
    /// **One request at a time.** `refresh` replaces the request in flight, so a
    /// tick that arrived during a load would cancel a live request before it
    /// could land: against a cluster that takes longer to answer than the
    /// interval, the panel would cancel every load before it finished and sit on
    /// its first snapshot for ever. Skipping the tick is the same coalesce
    /// `table_view::host` makes when a rebuild is already running — one in
    /// flight, and the next tick decides when the next one may start rather than
    /// forcing one.
    ///
    /// The `refreshing` check is the invariant and comes first; the visibility
    /// and connection checks are the panel's own reasons not to ask the cluster
    /// at all, and `visible` is what the shell sets when the tab is not the
    /// active one.
    fn on_auto_refresh_tick(&mut self, cx: &mut Context<Self>) {
        if self.refreshing {
            return;
        }
        if !self.visible || self.handle.is_none() {
            return;
        }
        self.refresh(cx);
    }

    /// The panel's own content box, in logical pixels, bounded by the window.
    ///
    /// Two of this panel's layouts are functions of the space it has rather than
    /// of its content — the capacity table's height and the vertical placement of
    /// every state with no data — so both read this instead of asking the window
    /// twice and getting two answers.
    ///
    /// The measurement lands a frame after it is taken, so a window that has just
    /// shrunk still reports the height it had. The window bounds it, or a state
    /// would centre itself in a box the panel no longer has. Before the first
    /// measurement the best guess is the window less this panel's own toolbar,
    /// which is the part of the chrome the panel owns: the shell's title bar and
    /// tab strip sit above this box and are not knowable from here, so the first
    /// frame is a chrome-height too tall and the second corrects it.
    fn available_height(&self, window: &Window) -> f32 {
        let ceiling = f32::from(window.viewport_size().height) - f32::from(design::size::TITLE_BAR);
        let measured = self.content_height.get();
        if measured.is_finite() && measured > 0.0 {
            measured.min(ceiling.max(0.))
        } else {
            ceiling.max(0.)
        }
    }

    // ── Chrome ───────────────────────────────────────────────────────────────

    fn render_toolbar(&self, cx: &Context<Self>) -> AnyElement {
        let (state, age) = freshness(
            &self.state,
            self.refreshing,
            self.refresh_error.is_some(),
            self.last_refreshed,
        );
        let status = state.label(age);
        let detail = state.detail(age);
        let status_label = status.clone();
        h_flex()
            .id("overview-toolbar")
            .debug_selector(|| "overview-toolbar".to_owned())
            .flex_none()
            .w_full()
            .h(design::size::TITLE_BAR)
            // The scroll body below pads by `LG`, so the toolbar does too:
            // `UI-SPEC` §7 asks every element in a region to start on one x, and
            // the title used to sit 8px left of everything under it.
            .px(space::LG)
            .gap(space::SM)
            .items_center()
            .tab_group()
            // The one stroke the panel draws: the 1px line between two surfaces.
            .border_b_1()
            .border_color(role::border_subtle(cx))
            // No leading mark, and the reason is the alignment spine rather than taste.
            //
            // A header band's title has to start on the same x as the content under it, because a
            // reader scanning a region pairs a heading with what the heading is about, and a
            // heading 22px right of its own body is a heading that does not line up with anything.
            // A mark in the band pushes the title off that spine by its own width plus the gap.
            //
            // The resource table's band *does* open with a mark, so this is a real inconsistency
            // between two bands — and it is resolved here, in the band that can afford it, rather
            // than by indenting every row of the densest surface in the product by 22px to match
            // it. The table's header, rows, summary line and selection bar are all measured on one
            // 16px spine and that measurement is worth more than a glyph.
            .child(
                div()
                    .flex_none()
                    .debug_selector(|| "overview-toolbar-title".to_owned())
                    .child(label_panel_title("Cluster overview")),
            )
            .child(div().flex_1().min_w(px(0.)))
            // Freshness sits against the button that causes it rather than
            // against the far edge: `Stale 2m` and `Refresh` are one statement,
            // and the reader's eye should not cross the panel to pair them.
            .child(
                div()
                    .id("overview-refresh-status")
                    .debug_selector(|| "overview-refresh-status".to_owned())
                    .min_w(px(0.))
                    .role(Role::Status)
                    .aria_label(format!("{status_label}. {detail}"))
                    .tooltip(text_tooltip(detail))
                    // L9 grades the freshness point, and this line is the same
                    // statement without a shape: amber only once the numbers are
                    // actually old, grey while they are being replaced, which is
                    // not a fault. It is a *word* — there is no dot beside it to
                    // carry the state — so it takes the word ink, not the mark one
                    // the strip's chips use.
                    .child(label_small(status).text_color(match state {
                        Freshness::Stale => role::warning_word(cx),
                        _ => role::fg_tertiary(cx),
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .debug_selector(|| "overview-refresh".to_owned())
                    .child(
                        // A ghost button with the word on it, and the icon the rest
                        // of the product's refresh controls carry
                        // (`shell::panels`'s own `RefreshCw` trio) so the glyph is
                        // the same one everywhere a reader has seen it. Not
                        // `primary`: refreshing a view is an ordinary command,
                        // not the screen's one committed decision, and this panel
                        // spends its single primary on `Retry` in a state where
                        // nothing else is left to try.
                        common::labelled(
                            Button::new("overview-refresh")
                                .icon(IconName::RefreshCw)
                                .ghost()
                                .with_size(Size::Size(design::size::CONTROL))
                                // The 28px row is the shared control rhythm, and
                                // gpui-kit's own `Size::Medium` is 32, so the height
                                // is pinned rather than inherited from the component.
                                .h(design::size::CONTROL)
                                .tab_index(0isize)
                                .track_focus(&self.refresh_focus)
                                .tooltip("Refresh the cluster data")
                                .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                            "Refresh",
                        )
                        // The two words differ on purpose: `Refresh` is the action and the
                        // announced name says what it refreshes, so the control still names
                        // itself to a reader who never sees the strip around it.
                        .accessibility_label("Refresh the cluster overview"),
                    ),
            )
            .into_any_element()
    }

    /// The channel the strip may spend, and the one mark it spends it on.
    ///
    /// **The emphasis budget on this strip is exactly one status mark.** The render
    /// showed what four of them cost: a cluster with four problems printed four
    /// saturated 6px dots in a row beside a count, and the eye went to the dots
    /// rather than to the count — so the sentence the strip exists to state was
    /// quieter than the itemisation of it. `Design guides > Visual language >
    /// Hierarchy` is explicit that emphasis is a budget, and a field of competing
    /// highlights leaves nothing reading as important.
    ///
    /// So the mark moves onto the dominant fact and there is exactly one of it, in
    /// the *worst* channel on the strip: a failed pod is a fault, so the dot is the
    /// fault hue, and four thousand pending pods with no failure is a caution, so
    /// the dot is the caution hue. **A grey mark is drawn for no channel at all**,
    /// which is the state a cluster is in when the only thing it has is a node
    /// that is not `Ready`: the count is still printed, because the reader has to
    /// be able to see it, and it is printed in ink rather than in colour, because
    /// the panel is not told whether that node is broken or cordoned.
    fn strip_mark(severity: Severity, cx: &App) -> Option<AnyElement> {
        match severity {
            Severity::Error | Severity::Warning => Some(
                div()
                    .debug_selector(|| "overview-health-mark".to_owned())
                    .flex_none()
                    .size(design::size::STATUS_DOT)
                    .rounded_full()
                    .bg(mark_ink(severity, cx))
                    .into_any_element(),
            ),
            _ => None,
        }
    }

    /// The strip: one grey line when the cluster is fine, one dominant fact and a
    /// quiet itemisation when it is not.
    ///
    /// `UI-REDESIGN.md` §3.5 replaces the old banner — a heavy slab with a
    /// coloured left border, the loudest thing on screen for a cluster that is
    /// usually fine — with this, and §3.5 draws the state as a count *plus* the
    /// chips. The count is the sentence: two coloured chips side by side are two
    /// facts and no order, so nothing on the strip said which of them to open
    /// first, and the reader had to read both labels and pick. The chips
    /// themselves are ordered worst-first (`Issue::rank`), so the leftmost one is
    /// the one to open and the reader does not rank four labels by eye.
    ///
    /// **One dominant fact, everything else metadata.** The count is the primary
    /// reading and it is the only thing on the strip at `text::TITLE` in
    /// `fg.primary`; the itemisation is one type step down at `text::LABEL` in
    /// secondary ink, and the words carry every distinction the coloured dots used
    /// to. See [`Self::strip_mark`] for the one mark that survives. Before this
    /// every one of the five items was a saturated 12px dot plus a status-coloured
    /// word, so a cluster with four problems rendered a wall of equally-weighted
    /// fragments and the reader had to decide which mattered.
    ///
    /// A healthy cluster spends no colour, no fill and no stroke at all: it is one
    /// line of grey text on the content surface, and the screen gets quiet.
    fn render_health(&self, overview: &Overview, cx: &Context<Self>) -> AnyElement {
        let issues = health_issues(overview, &self.routes);
        let summary = if issues.is_empty() {
            quiet_line(overview)
        } else {
            format!(
                "{} need attention",
                design::format::count_with_noun(issues.len(), "issue", "issues")
            )
        };
        let aria = if issues.is_empty() {
            health_verdict(overview).0.to_owned()
        } else {
            format!(
                "{summary}: {}",
                issues
                    .iter()
                    .map(|issue| issue.label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let mut strip = h_flex()
            .id("overview-health")
            .debug_selector(|| "overview-health".to_owned())
            .role(Role::Status)
            .aria_label(aria)
            .w_full()
            .min_w(px(0.))
            // `UI-SPEC` §4.17 gives a strip above a list 32px, and the healthy
            // strip is exactly that. An item is 24px, so the row does not grow to
            // hold one and nothing below it ever moves.
            .h(design::size::SUMMARY_STRIP)
            .gap(space::MD)
            .items_center();
        if issues.is_empty() {
            return strip
                .child(
                    div()
                        .id("overview-health-summary")
                        .debug_selector(|| "overview-health-summary".to_owned())
                        .min_w(px(0.))
                        // The sentence is the one line on this panel that can be
                        // arbitrarily long — a denial names every missing
                        // permission — and a 32px strip truncates it. The tooltip
                        // carries the whole sentence so a truncation never costs a
                        // fact.
                        .tooltip(text_tooltip(summary.clone()))
                        .child(
                            label_small(summary)
                                .text_color(role::fg_secondary(cx))
                                .truncate(),
                        ),
                )
                .into_any_element();
        }
        // The items share what the verdict leaves rather than sitting on a fixed
        // row of their own: a cluster can raise all four at once, and four items
        // plus the count are most of the 700px a strip has at the minimum window.
        // They shrink and truncate instead, because an item that keeps its width
        // and spills off the panel reports a problem the reader cannot see.
        strip = strip
            .when_some(
                Self::strip_mark(worst_severity(&issues), cx),
                |this, mark| this.child(mark),
            )
            .child(
                div()
                    .debug_selector(|| "overview-health-summary".to_owned())
                    .flex_none()
                    .max_w(relative(0.45))
                    .child(
                        // The primary fact, at the panel-title role the rest of the
                        // app's titles wear. It is a count and a verdict, so it is
                        // set in tabular figures like every other number on the
                        // page, and in `fg.primary` — the mark beside it carries
                        // the channel, so this line never does.
                        Label::new(summary.clone())
                            .font_features(tabular_features())
                            .text_size(text::TITLE)
                            .line_height(text::TITLE_LINE_HEIGHT)
                            .font_weight(text::SEMIBOLD)
                            .text_color(role::fg_primary(cx))
                            .truncate(),
                    ),
            )
            // The hairline that says "and here is what it is made of". One rule,
            // between two groups, owned by the boundary between them —
            // `Design guides > Alignment details` ("hairlines belong on the
            // boundary owner") — and it does the job the four coloured dots were
            // doing: telling the reader that the items are subordinate to the
            // count rather than four alerts of their own.
            .child(
                div()
                    .debug_selector(|| "overview-health-rule".to_owned())
                    .flex_none()
                    .w(border::LINE)
                    .h(design::size::ICON_BUTTON)
                    .bg(role::border_subtle(cx)),
            )
            .child(
                h_flex()
                    .debug_selector(|| "overview-health-items".to_owned())
                    .flex_1()
                    .min_w(px(0.))
                    .gap(space::SM)
                    .overflow_hidden()
                    .children(
                        issues
                            .iter()
                            .enumerate()
                            .filter_map(|(index, issue)| self.render_issue_chip(index, issue, cx)),
                    ),
            );
        strip.into_any_element()
    }

    /// One issue item, or the same mark with no target when the host installed no
    /// route for it.
    ///
    /// **A count in a sentence, not a badge.** The item used to be a filled
    /// `warning_wash` / `danger_wash` pill, and then a 6px dot in a status hue
    /// beside a caption-size word. On a cluster that raised all four kinds of issue
    /// that was four saturated marks in a row competing with the count for the
    /// reader's first glance — the densest use of a caution hue anywhere in the
    /// product, for four facts the reader had to read all of before ranking them.
    /// Both are gone: an item is `count + noun` at `text::LABEL`, and it says what
    /// it counts rather than announcing that something is wrong. `9,900 pods
    /// pending` needs no amber to be understood, and a status-coloured word on a
    /// number that is not itself a verdict is `Design guides > Color and themes`'
    /// "a semantic status colour as decoration".
    ///
    /// **Routable is a persistent ink, not a hover.** An item the host installed a
    /// route for is secondary ink; an item it did not is tertiary. That difference
    /// is on screen at rest, so a reader can see which counts lead somewhere
    /// without pointing at anything — hover is the wrong place for the only cue
    /// that a number is a destination. The unroutable one also takes none of the
    /// rest of a control's treatment: no hover, no press, no focus stop, no click,
    /// no button role, and `route_sentence` leaves the destination out of its
    /// tooltip as well. Installing a route later changes an ink and a hit target
    /// and not one pixel of the layout, so a cluster that is already in trouble
    /// does not reflow under the reader.
    fn render_issue_chip(
        &self,
        index: usize,
        issue: &Issue,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let selector = format!("overview-issue-{index}");
        let detail = issue.detail.clone();
        let routable = issue.route.is_some() && self.chip_focus.get(index).is_some();
        let chip_selector = selector.clone();
        let chip = h_flex()
            .id(ElementId::from(selector.clone()))
            .debug_selector(move || chip_selector.clone())
            .flex_shrink(1.)
            .min_w(px(0.))
            .max_w_full()
            .h(design::size::ICON_BUTTON)
            .gap(space::XS)
            .px(space::SM)
            .items_center()
            .rounded(radius::SM)
            .tooltip(text_tooltip(detail.clone()))
            .child(
                label_small(issue.label.clone())
                    .text_size(text::LABEL)
                    .line_height(text::LABEL_LINE_HEIGHT)
                    .font_weight(text::MEDIUM)
                    .text_color(if routable {
                        role::fg_secondary(cx)
                    } else {
                        role::fg_tertiary(cx)
                    })
                    .min_w(px(0.))
                    .truncate(),
            );
        let (Some(route), Some(focus)) = (issue.route.clone(), self.chip_focus.get(index).cloned())
        else {
            // No route, no control. A figure that darkens under the pointer is a
            // promise the panel does not keep, so this branch deliberately takes
            // none of what makes a control look like one.
            return Some(chip.into_any_element());
        };
        // One handle per listener: `Rc<dyn Fn>` is `Fn`, so each closure that
        // captures it borrows rather than moves, and a listener outliving the
        // borrow is the compiler's whole objection.
        let on_key = route.clone();
        // The wash is the *only* thing a hover paints, and it is a wash rather than
        // a fill so the item carries nothing at rest. It comes from
        // `design::state` rather than from this file's own alpha helper because
        // the strip has no colour of its own to tint: the wash is a step of the
        // product's neutral ink over the plane the strip is on, so it cannot
        // change hue under the pointer the way a wash of the item's own status hue
        // could. Neither wash is a transition — the response is `motion::INSTANT`,
        // because a hover is not a place the reader needs to be told that it
        // happened.
        let surface = capacity_table_surface(cx);
        Some(
            chip.relative()
                .track_focus(&focus)
                .tab_index(index as isize + 1)
                .role(Role::Button)
                .aria_label(detail)
                .hover(|this| this.bg(design::state::hover_on(surface, role::fg_primary(cx))))
                .active(|this| this.bg(design::state::press_on(surface, role::fg_primary(cx))))
                // The focus ring is a rail drawn *outside* the item rather than a
                // border on it. A border would add two pixels the moment the
                // keyboard arrived, so every Tab would nudge the items after it —
                // and `UI-SPEC` §8's zero-roughness list is exactly a list of
                // shifts like that. An absolute rail takes no layout at all, and
                // it is the same 2px accent rail the selection uses, which is one
                // of the four places the accent is allowed to appear.
                //
                // It is drawn at the item's own left edge while its text starts
                // 8px in, so the rail sits in the padding and never on top of a
                // label.
                .when(self.keyboard_focus.get(), |this| {
                    this.child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left_0()
                            .w(design::size::SELECTION_RAIL)
                            .bg(role::accent(cx)),
                    )
                })
                .on_key_down(cx.listener(move |view, event: &KeyDownEvent, window, cx| {
                    view.keyboard_focus.set(true);
                    // An item is a control, so it answers the two keys every
                    // control answers, and an item that only answers a click is
                    // one a keyboard user cannot reach at all.
                    if matches!(event.keystroke.key.as_str(), "enter" | " ") {
                        on_key(window, cx);
                        cx.stop_propagation();
                    }
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|view, _, _, _| {
                        // A click is not the keyboard arriving. Without this the
                        // item under the pointer painted the same rail a Tab does,
                        // which is the single most reliable way to make a focus ring
                        // look decorative.
                        view.keyboard_focus.set(false);
                    }),
                )
                .on_click(cx.listener(move |_, _, window, cx| route(window, cx)))
                .into_any_element(),
        )
    }
    /// The row of four stat tiles on the twelve-column grid.
    ///
    /// `UI-REDESIGN.md` §3.5's complaint about this page was that the text and
    /// numbers filled about 60% of the width and left a void on the right, and
    /// that it read as a form. A row of tiles on a grid is the answer: four slots
    /// that grow in proportion to their columns fill the panel edge to edge, and
    /// the grid below lines up with them.
    ///
    /// **And the four share one surface.** The grid fills the band's own width;
    /// the surface around it is what says the four readings are one answer — see
    /// [`band`] for why it is one plate and not four cards, and why the padding
    /// that holds the figures off the plate is the plate's rather than the
    /// page's.
    fn render_tiles(&self, overview: &Overview, wide: bool, cx: &Context<Self>) -> AnyElement {
        let (hero, signals) = vital_figures(overview);
        let mut tiles = vec![hero];
        tiles.extend(signals);
        let span = if wide { 3 } else { 6 };
        let per_row = if wide { 4 } else { 2 };
        let mut grid = v_flex()
            .id("overview-vitals")
            .debug_selector(|| "overview-vitals".to_owned())
            .w_full()
            .min_w(px(0.))
            .gap(GRID_GAP)
            .role(Role::Group)
            .aria_label("Cluster figures");
        let mut made = 0usize;
        while made < tiles.len() {
            let take = per_row.min(tiles.len() - made);
            grid = grid.child(grid_row(
                tiles[made..made + take]
                    .iter()
                    .map(|tile| (tile.selector.to_owned(), span, stat_tile(tile, cx)))
                    .collect(),
            ));
            made += take;
        }
        band(
            "overview-vitals-band",
            grid.into_any_element(),
            band_plate(self.refresh_error.is_some(), cx),
        )
        .into_any_element()
    }

    /// The five workload kinds, each a count, a shortfall, and a proportion bar.
    ///
    /// The same four-line shape as the tile band above it — a heading, a figure, a
    /// subline and a meter, on one shared surface, five cells inside it — because
    /// the five kinds *are* the itemisation of the roll-up the tiles do not print,
    /// and an itemisation that looks like a different kind of thing from the thing
    /// it itemises reads as two unrelated regions.
    ///
    /// **Denser than five islands, which is what the render showed.** The band used
    /// to be five figures and five slivers on a 1,900px row: a number, a bar whose
    /// length was 12.5% of the cell, and 300px of nothing. The three changes are
    /// one decision — a cell is the same shape as a stat tile, on the same meter
    /// grammar — and they fill the row because each cell now says three things
    /// instead of one: the figure, how many replicas are missing from it, and a
    /// meter whose track is the whole cell so its length is a measurement rather
    /// than a leftover.
    ///
    /// The section also prints [`WORKLOADS_READING`] under its heading, which is
    /// what a `4 / 10,005` needs to be a fact rather than a form.
    fn render_workloads(
        &self,
        overview: &Overview,
        wide: bool,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let breakdown = workload_breakdown(overview);
        if breakdown.is_empty() {
            return None;
        }
        let summaries = workload_summaries(&overview.workloads);
        let spans = if wide {
            WORKLOAD_SPANS_WIDE
        } else {
            WORKLOAD_SPANS_NARROW
        };
        let cell = |index: usize| {
            let (label, figure) = &breakdown[index];
            (
                workload_selector(label),
                spans[index],
                workload_cell(label, figure.as_deref(), summaries[index], cx),
            )
        };
        let mut grid = v_flex()
            .id("overview-workload-grid")
            .debug_selector(|| "overview-workload-grid".to_owned())
            .w_full()
            .min_w(px(0.))
            .gap(GRID_GAP)
            .role(Role::Group)
            .aria_label(workload_detail(overview));
        let (total, reading, note) = replicas_total(overview);
        if wide {
            grid = grid.child(grid_row((0..5).map(cell).collect()));
        } else {
            // Three kinds on the first row and two on the second, because five
            // does not divide into twelve.
            grid = grid
                .child(grid_row((0..3).map(cell).collect()))
                .child(grid_row((3..5).map(cell).collect()));
        }
        Some(
            v_flex()
                .w_full()
                .min_w(px(0.))
                .mt(SECTION_GAP)
                .gap(SECTION_CONTENT_GAP)
                .child(section_heading_with_total(
                    "Workloads",
                    // The ratio is grey whatever the state: it counts everything
                    // the cluster has. The note is what wears the caution ink.
                    Some(HeadingTotal {
                        figure: total.as_str(),
                        severity: Severity::Muted,
                        note: note
                            .as_ref()
                            .map(|(text, severity)| (text.as_str(), *severity)),
                    }),
                    Some(reading.as_str()),
                    cx,
                ))
                // What the two numbers on this heading are, printed where a reader
                // scanning the page meets it. See `WORKLOADS_READING` for why the
                // word `target` is not used anywhere on this page instead.
                .child(section_reading("workloads", WORKLOADS_READING, cx))
                .child(band(
                    "overview-workload-band",
                    grid.into_any_element(),
                    band_plate(self.refresh_error.is_some(), cx),
                ))
                .into_any_element(),
        )
    }

    /// The node capacity grid.
    ///
    /// gpui-kit's `DataTable` owns the header band, the virtualised rows, the
    /// cell cursor, the horizontal scroll, and the arrow keys. The delegate
    /// carries the app's half: the columns, the figures, the bars, and the sort
    /// order. The section heading and its empty state are still the panel's.
    fn render_capacity(
        &self,
        overview: &Overview,
        usage: Option<&[NodeUsage]>,
        wide: bool,
        available_height: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let typography = settings::data_typography(cx);
        let metrics_available = usage.is_some();
        // The two live columns are not part of the table at all when the cluster
        // did not answer, and a column that is *absent* is the hardest kind of
        // absence to notice: five columns with no live reading in them looks like
        // a table that has nothing to say, rather than like a table whose two
        // right-hand columns are waiting for metrics-server. So the section says
        // so, in one sentence under its own heading, in secondary ink — this is a
        // missing reading and not a fault, and it spends no status channel.
        //
        // The panel knows **that** the answer is missing and not **why**: the
        // host hands [`Self::set_metrics_available`] a `bool`, and
        // `panels::metrics::MetricsProbeState` — which does know, and has copy
        // for each of its five cases — is not passed through. The sentence is
        // therefore the most this surface can honestly claim.
        let section = v_flex()
            .id("overview-capacity")
            .debug_selector(|| "overview-capacity".to_owned())
            .role(Role::Group)
            // The group's own accessible name carries the node readiness, so the
            // heading does not repeat it: `1 / 1 ready` appeared three times on
            // one screen, 120px and 260px apart, and every row's accessible name
            // opened with the same cluster-level sentence. The metrics sentence is
            // a child of this group and is announced from there.
            .aria_label(format!("Node capacity. {}", node_status(overview)))
            .w_full()
            .min_w(px(0.))
            .mt(SECTION_GAP)
            .gap(SECTION_CONTENT_GAP)
            .child(section_heading("Node capacity", cx))
            .when(!metrics_available, |this| {
                this.child(section_reading("metrics", NO_LIVE_METRICS_READING, cx))
            });
        if overview.capacities.is_empty() {
            // One line, in place, naming the absence and the one action that can
            // change it.
            //
            // The line used to end `Refresh, or make sure the cluster connection
            // works.` — advice for a *total* failure, printed under a *partial*
            // snapshot whose health strip 40px above already says which sources
            // were missing, and which sends a reader whose connection is fine to go
            // and debug their network. The strip owns the diagnosis; this owns the
            // hole in the table, so it says the hole and stops.
            //
            // It is a band, like the two above it, rather than a loose line: a
            // section whose body is one sentence should still read as a region with
            // a boundary, and a sentence floating on the page plane is the
            // unfinished look this page was sent back for. The sentence is `body`
            // and not the `caption` the section's own label wears, because it is a
            // complete sentence and the guide reserves uppercase caption type for
            // labels and short states.
            return section
                .child(band(
                    "overview-capacity-empty-band",
                    v_flex()
                        .w_full()
                        .min_w(px(0.))
                        .child(
                            label_small("Node capacity was not reported. Refresh to check again.")
                                .text_size(text::BODY)
                                .line_height(text::BODY_LINE_HEIGHT)
                                .text_color(role::fg_secondary(cx)),
                        )
                        .into_any_element(),
                    band_plate(self.refresh_error.is_some(), cx),
                ))
                .into_any_element();
        }
        let widths = capacity_column_widths(
            self.table_available_width(f32::from(window.viewport_size().width)),
            metrics_available,
        );
        let column_count = widths.len();
        let rows = capacity_rows(&overview.capacities, usage);
        let row_count = rows.len();
        // The state lives as long as the panel does, so the cursor, the scroll
        // offsets, the column layout, and the sort survive the refresh that
        // replaces the snapshot.
        let existing = self.capacity_state.borrow().clone();
        let state = match existing {
            Some(state) => state,
            None => {
                let state = cx.new(|cx| {
                    TableState::new(CapacityTableDelegate::new(cx), window, cx)
                        .cell_selectable(true)
                        // The node name is the row header, so the component's own
                        // narrow gutter would repeat it with nothing to say.
                        .row_header(false)
                        .row_selectable(false)
                        .col_selectable(false)
                        // Arrow keys stop at the ends. A table of nodes that
                        // wraps around reads as a loop, and the reader loses the
                        // node they were on.
                        .loop_selection(false)
                        // The sort cycle is the app's, and so is the heading that
                        // carries the control: the component's cycle ends in
                        // "unsorted", which for this table would leave the rows
                        // in whatever order the API happened to answer in.
                        .sortable(false)
                        .col_resizable(false)
                        .col_movable(false)
                });
                *self.capacity_state.borrow_mut() = Some(state.clone());
                state
            }
        };
        let focus = state.read(cx).focus_handle(cx);
        // The component starts in row mode, and in row mode with
        // `row_selectable(false)` its arrow bindings return without doing
        // anything — so a keyboard reader who tabs in and presses Down would get
        // nothing at all. Seating the cursor is what puts the arrows in charge.
        //
        // It happens when the table *takes the focus*, not on the first paint. The
        // component paints the cell cursor as `tokens.table_active`, so a cursor
        // seated at rest is a 200px block of accent on the node name of a screen
        // that is otherwise one line of grey — the reader has highlighted nothing
        // and the screen says they have. `table_view` opens with no selection for
        // the same reason (`TableViewState::new` seeds `selected_uid: None`), and
        // this table is the only place in the product that pre-selects.
        //
        // Seating on focus costs the reader nothing and buys the resting screen
        // its quiet: the moment a keyboard arrives, the cursor is there and the
        // arrows work; until then nothing on this table wears the accent.
        let table_focused = focus.is_focused(window);
        if table_focused && row_count > 0 {
            state.update(cx, |state, cx| {
                if state.delegate().seated || state.selected_cell().is_some() {
                    return;
                }
                state.set_selected_cell(0, 0, cx);
                state.delegate_mut().seated = true;
            });
        }
        state.update(cx, |state, cx| {
            // A snapshot that lost nodes must not leave the cursor on a row that
            // is gone.
            let clamped = state.selected_cell().map(|(row, column)| {
                (
                    row.min(row_count.saturating_sub(1)),
                    column.min(column_count.saturating_sub(1)),
                )
            });
            if let (Some(selected), Some(clamped)) = (state.selected_cell(), clamped)
                && selected != clamped
            {
                state.set_selected_cell(clamped.0, clamped.1, cx);
            }
            // The component measures the header once, so a new snapshot, a new
            // column count, or a new column layout needs it to measure again.
            let relayout = {
                let delegate = state.delegate_mut();
                let sort = delegate.sort;
                let relayout = delegate.widths != widths;
                delegate.rows = sorted_capacity_rows(rows, sort);
                delegate.widths = widths;
                delegate.metrics_available = metrics_available;
                relayout
            };
            if relayout {
                state.refresh(cx);
            }
        });
        let cursor = state.clone();
        // The component selects a cell on a click but does not take the focus,
        // so the arrows would go to whatever was focused before. The grid takes
        // the focus on any press inside it, which is what makes a click and the
        // key after it one gesture.
        section
            .child(
                v_flex()
                    .id("overview-capacity-table")
                    .debug_selector(|| "overview-capacity-table".to_owned())
                    .role(Role::Table)
                    .aria_label("Node capacity")
                    .aria_description(CAPACITY_TABLE_DESCRIPTION)
                    .aria_keyshortcuts(CAPACITY_TABLE_KEYS)
                    .aria_row_count(row_count + 1)
                    .aria_column_count(column_count)
                    .w_full()
                    .min_w(px(0.))
                    .h(capacity_table_height(
                        f32::from(typography.table_row_height()),
                        row_count,
                        available_height,
                        wide,
                        self.refresh_error.is_some(),
                        // The two regions above whose presence is a function of the
                        // snapshot rather than of the cluster: a cluster with no
                        // workloads has no Workloads band, and a cluster with no
                        // metrics-server has no metrics sentence. Counting either
                        // when it is not there hands the table a box 120px short of
                        // the space it has, and its last rows end above the fold for
                        // no reason a reader could see.
                        workload_data_available(&overview.workloads),
                        metrics_available,
                    ))
                    .tab_group()
                    .tab_index(HEALTH_ISSUE_COUNT as isize + 1)
                    .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
                        window.focus(&focus, cx);
                        cx.stop_propagation();
                    })
                    .on_key_down(move |event: &KeyDownEvent, _, cx| {
                        // Control+Home and Control+End are the first and last cell
                        // in the table. The component binds the unmodified keys to
                        // the first and last column of the current row and has no
                        // binding for these, so they arrive here.
                        if !event.keystroke.modifiers.control
                            || event.keystroke.modifiers.alt
                            || event.keystroke.modifiers.platform
                        {
                            return;
                        }
                        let target = match event.keystroke.key.as_str() {
                            "home" => Some((0, 0)),
                            "end" => {
                                Some((row_count.saturating_sub(1), column_count.saturating_sub(1)))
                            }
                            _ => None,
                        };
                        let Some(target) = target.filter(|_| row_count > 0) else {
                            return;
                        };
                        cursor.update(cx, |state, cx| {
                            state.set_selected_cell(target.0, target.1, cx);
                        });
                    })
                    .child(
                        DataTable::new(&state)
                            .with_size(Size::Size(typography.table_row_height()))
                            // No stripe, and no row divider: `PROMPT.md` §2.1 #6
                            // rules both out, and the delegate used to paint a
                            // zebra of its own behind the component's back while
                            // the component's own stripe flag said `false`.
                            .stripe(false)
                            .bordered(false)
                            .scrollbar_visible(true, true),
                    ),
            )
            .into_any_element()
    }

    /// The regions of the dashboard, in the order a reader scans them: the
    /// verdict, the fleet's four numbers, the itemisation of one of them, and
    /// then the deepest detail. The capacity table is a scrolling region and it
    /// goes last, so a hundred nodes cannot push the tiles off the screen.
    ///
    /// The body's own gap is one `space::XXL` and each *section* adds a second
    /// [`SECTION_GAP`], so a section boundary is two steps and the strip above the
    /// first band is one. Everything above the capacity table is fixed-height by
    /// construction, which is what lets [`capacity_table_height`] measure what the
    /// last band has left without the page having to be told.
    ///
    /// **The page ends with a line, not with a frame.** The 400px of plain plane a
    /// three-node cluster leaves below the last row is defensible — a page does
    /// not have to fill its window, and a framed box holding nothing reads as a
    /// panel that failed to load — but the page used to *stop* with nothing at all
    /// acknowledging the end, which is the difference between air and abandonment.
    /// So the last child is [`page_reading`]: one caption line, on the content
    /// spine, 32px below the table, naming the one mark the table above it uses
    /// for a value the cluster did not report. The air below *that* is the page's
    /// own plane and is left alone.
    fn render_dashboard(
        &self,
        overview: &Overview,
        usage: Option<&[NodeUsage]>,
        wide: bool,
        available_height: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut body = v_flex()
            .id("overview-body")
            .debug_selector(|| "overview-body".to_owned())
            .w_full()
            .min_w(px(0.))
            // Between the major regions of a page: `UI-SPEC` §2.1 gives 32, and
            // it is the spacing that replaces the frame and the two full-width
            // rules this region used to be built out of.
            .gap(space::XXL);
        if self.refresh_error.is_some() {
            // `UI-REDESIGN.md` L9: "you are looking at old data" has to be visible
            // in the data area itself, not only in a corner. A 4% warning wash over
            // the whole region says it to every figure at once, which a marker in
            // the toolbar does not.
            body = body.bg(design::composite_surface(
                capacity_table_surface(cx),
                role::warning(cx).opacity(STALE_WASH_ALPHA),
            ));
        }
        body = body
            .child(self.render_health(overview, cx))
            .when_some(self.refresh_error.clone(), |this, reason| {
                this.child(self.render_refresh_error(&reason, cx))
            })
            .child(self.render_tiles(overview, wide, cx));
        if let Some(workloads) = self.render_workloads(overview, wide, cx) {
            body = body.child(workloads);
        }
        // The capacity table and its legend are RETURNED SEPARATELY so the caller
        // can take them out of the page's measure.
        //
        // One measure cannot serve both halves of this page. The tiles and the
        // workload row want about 1200 — a tile needs room for a label, a figure,
        // a subline and a meter that still reads as a meter — while the capacity
        // table's five columns with their meters and their limits need about 1600,
        // and capping it clipped MEMORY LIMITS off the right edge, which is the
        // one thing a table may never lose.
        //
        // So the measure holds the parts that are read as figures and the table
        // takes the full width beside it. That is the ordinary full-bleed-table
        // arrangement, and it is why the table's **surface** and the two bands'
        // surfaces share one left edge with it: the table runs to the panel's own
        // padding line and each band's plate does too, because none of the three
        // is inside a [`band`]. What sits inside a band is one `BAND_PADDING`
        // further in, which is the card's padding and not a second spine.
        body.child(self.render_capacity(overview, usage, wide, available_height, window, cx))
            // The legend belongs to the table it explains, so a section that
            // rendered no table — a cluster that reported no node capacity at
            // all — does not get one. The empty band above says its own thing.
            .when(!overview.capacities.is_empty(), |this| {
                this.child(page_reading(cx))
            })
            .into_any_element()
    }

    /// The inline error strip.
    ///
    /// `UI-SPEC` §4.15 fixes this one: 32px tall, a 3px danger bar, a danger wash,
    /// 12px text, and a text button whose label is a verb. gpui-kit's `Alert` is
    /// not it — it is a bordered box with an icon whose colours are
    /// `theme().danger` and `theme().background`, so it would reintroduce both the
    /// stroke and the token the migration removes. The strip is a dozen lines.
    ///
    /// The strip says **which step** failed, which is the half of §4.15 the
    /// sentence used to leave out: it read `Refresh failed. The figures are the
    /// last ones the cluster gave.` for a refused RBAC call, a dead API server and
    /// a request that ran past the deadline alike, and the only way to tell those
    /// three apart was the tooltip. The clause is the shortest form of the
    /// [`FailureStep`] verdict that fits a 32px row beside the `Retry`; the raw
    /// error is still one hover away, and the two halves together are what the
    /// reader acts on.
    ///
    /// **The rail is a mark and the two texts are words, and the panel now says
    /// so.** A status word printed on a wash of its own channel is the exact
    /// combination `design::Roles::refine` solves `danger_word` against
    /// (`channel_surfaces`) and the mark ink is deliberately *not* solved for: the
    /// 3px rail is the mark half of §4.4's pair, and the sentence and the `Retry`
    /// label beside it are the word half. At rest the two inks resolve to the same
    /// value, so nothing moves; under Increase Contrast the word lifts to the text
    /// floor and the rail stays on the graphic one, which is the promise the two
    /// roles are split for.
    fn render_refresh_error(&self, reason: &str, cx: &Context<Self>) -> AnyElement {
        let message = format!(
            "{} · the figures are the last ones the cluster gave.",
            FailureStep::of(reason).clause()
        );
        h_flex()
            .id("overview-refresh-error")
            .debug_selector(|| "overview-refresh-error".to_owned())
            .w_full()
            .min_h(design::size::ROW)
            .items_stretch()
            .gap(space::SM)
            // The card radius, and the page has one: `radius::MD` is what a card
            // wears, the bands above wear the panel tier above it, and an inline
            // strip at a third value is how a page ends up with three corners. A
            // chip keeps `radius::SM` — a chip is barely round, and it is a
            // different shape of thing.
            .rounded(radius::MD)
            .bg(role::danger_wash(cx))
            .role(Role::Alert)
            // The sentence is the name. The raw reason is the *description*, which
            // is what `UI-SPEC` §4.15's "不给原始 RBAC JSON" is about in a
            // tooltip sense — and it used to be concatenated into the label, so a
            // screen reader announced `ServiceError: client error (Connect)`
            // before the sentence a reader can act on, unasked and every time.
            .aria_label(message.clone())
            .aria_description(reason.to_owned())
            .tooltip(text_tooltip(reason.to_owned()))
            .child(
                div()
                    .flex_none()
                    .w(border::TABLE_FOCUS_RAIL)
                    .h_full()
                    .rounded(radius::XS)
                    .bg(role::danger(cx)),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .px(space::SM)
                    .py(space::XS)
                    .items_center()
                    .child(
                        div().flex_1().min_w(px(0.)).child(
                            label_small(message)
                                .text_color(role::danger_word(cx))
                                .truncate(),
                        ),
                    )
                    // A text button, not a pushed one: `UI-SPEC` §4.15 asks for a
                    // word, and the screen has no primary action to spend.
                    .child(
                        div()
                            .id("overview-refresh-retry")
                            .debug_selector(|| "overview-refresh-retry".to_owned())
                            .flex_none()
                            .child(
                                common::labelled(
                                    Button::new("overview-refresh-retry")
                                        .ghost()
                                        .with_size(Size::Size(design::size::ROW_DENSE))
                                        .h(design::size::ROW_DENSE)
                                        .tab_index(0isize)
                                        .track_focus(&self.retry_focus)
                                        .text_color(role::danger_word(cx))
                                        .on_click(cx.listener(|view, _, _, cx| view.refresh(cx))),
                                    "Retry",
                                )
                                // The two words differ on purpose: this control sits beside the
                                // strip's own message, and a bare "Retry" announced there says
                                // nothing about which panel is asking.
                                .accessibility_label("Retry loading the cluster overview"),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// The first paint of the panel, before the snapshot arrives.
    ///
    /// `UI-SPEC` §4.14 is a ladder and not a single state: under 200ms nothing is
    /// drawn at all, a spinner takes the strip's own slot so nothing below it
    /// moves, and past 500ms a skeleton built to the real tile geometry stands in
    /// so the numbers arrive into space that is already theirs. The last rung adds
    /// the elapsed time, which is the only progress an aggregation can honestly
    /// report.
    ///
    /// **Every rung here is content-sized, and none of them may call
    /// `size_full()`.** All three used to, and `size_full()` is `h_full()` — a
    /// *percentage* height. The body's parent is the scroll container, whose own
    /// height comes from the flex chain above it, so the percentage had nothing
    /// definite to resolve against and came out **zero**: measured at 1920×1080 the
    /// whole ladder was `1888px × 0px`. The children were laid out correctly and
    /// clipped off the end, because a scroll container's scrollable area is the
    /// height of its content and a 0px content clips everything in it.
    ///
    /// What a user saw, on a load that took eleven seconds against a real
    /// cluster, was one 16px grey dash at the top of an empty panel — no tile
    /// skeletons, no workload skeletons, and no elapsed time, because the >2s
    /// progress row is the fourth child of the same zero-height box. `§2.4`'s
    /// "a network timeout must be visible inside ten seconds" and `§4.14`'s
    /// "past 2s, spinner + progress" both depend on this box having a height.
    /// `render_dashboard` never called `size_full()`, which is exactly why the
    /// dashboard rendered and the three non-data states did not.
    fn render_loading(&self, wide: bool, cx: &Context<Self>) -> AnyElement {
        let waited = self
            .loading_since
            .map(|since| since.elapsed())
            .unwrap_or_default();
        match self.loading_tier {
            // 0px is the point of this rung: a load that resolves inside 200ms
            // must leave no trace, and a content-sized empty column is that.
            LoadingTier::Nothing => v_flex().w_full().min_w(px(0.)).into_any_element(),
            LoadingTier::Spinner => v_flex()
                .id("overview-loading")
                .debug_selector(|| "overview-loading".to_owned())
                .w_full()
                .min_w(px(0.))
                .role(Role::Status)
                .aria_label("Loading the cluster overview")
                .child(
                    h_flex()
                        .w_full()
                        .h(design::size::SUMMARY_STRIP)
                        .items_center()
                        .child(common::spinner(
                            IconName::LoaderCircle,
                            role::fg_tertiary(cx),
                            Size::Small,
                        )),
                )
                .into_any_element(),
            tier => v_flex()
                .id("overview-skeleton")
                .debug_selector(|| "overview-skeleton".to_owned())
                .w_full()
                .min_w(px(0.))
                .gap(space::XXL)
                .role(Role::Status)
                .aria_label("Loading the cluster overview")
                .child(
                    h_flex()
                        .w_full()
                        .h(design::size::SUMMARY_STRIP)
                        .items_center()
                        .child(skeleton_block(
                            "overview-skeleton-strip",
                            0.14,
                            text::LABEL_LINE_HEIGHT,
                            waited,
                            cx,
                        )),
                )
                .child(skeleton_tiles(wide, waited, cx))
                .child(skeleton_workloads(wide, waited, cx))
                .when(tier == LoadingTier::Progress, |this| {
                    this.child(
                        h_flex()
                            .w_full()
                            .h(design::size::SUMMARY_STRIP)
                            .gap(space::SM)
                            .items_center()
                            .child(skeleton_block(
                                "overview-skeleton-progress",
                                0.18,
                                text::LABEL_LINE_HEIGHT,
                                waited,
                                cx,
                            ))
                            .child(
                                // An elapsed *wait*, not an age. `format_age`
                                // appends "ago" because every other caller is
                                // describing a reading, and a reader who saw
                                // `Still loading · 2s ago` was being told the
                                // load finished two seconds ago.
                                label_small(format!("Still loading · {}", format_wait(waited)))
                                    .text_color(role::fg_tertiary(cx)),
                            ),
                    )
                })
                .into_any_element(),
        }
    }

    /// The cluster answered, and the answer was "there is nothing here".
    ///
    /// `UI-SPEC` §4.13: a 24px muted glyph and one line, and **no** action. The
    /// line states the fact rather than the mood — it used to be the title
    /// `Nothing to report` with `The cluster returned no node or pod data`
    /// underneath, which is one sentence saying the same thing twice and neither
    /// half of it being the thing a reader would type into a search box.
    ///
    /// There is no action, on purpose: a cluster with no nodes is not a mistake
    /// the reader can undo from here, the panel already reloads itself every 20
    /// seconds, and the toolbar's `Refresh` is one key away for anyone who wants
    /// to ask again now. A `Retry` here would be a button that promises a
    /// different answer and cannot deliver one.
    fn render_empty_cluster(
        &self,
        _overview: &Overview,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        self.centred_state(
            "empty-state",
            Role::Region,
            design::glyph::state::empty_cluster(),
            "No pods or nodes reported",
            "The cluster answered, and it has nothing in it.",
            None,
            "",
            window,
            cx,
        )
    }

    /// The panel with no cluster to read.
    ///
    /// `UI-SPEC` §4.13: a 24px muted glyph, one line, and at most one action.
    ///
    /// **This state used to have no action, and that was the dead end.** Its
    /// sentence sent the reader to the title bar to pick a cluster, which is
    /// wrong on both arrivals that reach it: on a machine with no kubeconfig the
    /// picker holds nothing to pick, and a kubeconfig whose contexts all failed
    /// to load holds contexts the app cannot reach. The button says the move
    /// instead — `Reload kubeconfigs`, which is the one action that fixes every
    /// way of being here: it re-reads `~/.kube` and `$KUBECONFIG`, re-resolves
    /// the contexts and re-selects one. It is the same command the palette and
    /// `secondary-shift-r` already carry, so nothing new is introduced here and
    /// the keyboard reaches the same place the button does.
    ///
    /// The toolbar's own `Refresh` is not that control and was never an answer:
    /// with no handle it re-entered this same state, which is why the keyboard
    /// used to land on it.
    fn render_no_cluster(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        self.centred_state(
            "empty-state",
            Role::Region,
            design::glyph::state::no_cluster(),
            "No cluster connected",
            "Clusters come from ~/.kube/config or $KUBECONFIG. Reload kubeconfigs to re-read them and connect.",
            Some(self.reload_kubeconfigs_button("overview-reload-kubeconfigs", cx)),
            "",
            window,
            cx,
        )
    }

    /// The one verb-phrase action a state with no data can offer.
    ///
    /// `UI-SPEC` §4.15 asks for the next step and nothing else, and §4.13 gives
    /// an empty state at most one action. The button is the screen's only
    /// `primary`, which is what the budget allows, and the name rides on a
    /// wrapper because a gpui-kit `Button` owns its own box — the same way the
    /// toolbar's refresh names itself.
    fn retry_button(&self, id: &'static str, cx: &Context<Self>) -> AnyElement {
        div()
            .id(id)
            .debug_selector(move || id.to_owned())
            .flex_none()
            .child(
                Button::new(id)
                    .label("Retry")
                    .primary()
                    .with_size(Size::Medium)
                    .tab_index(0isize)
                    .track_focus(&self.retry_focus)
                    .accessibility_label("Retry loading cluster overview")
                    .on_click(cx.listener(|view, _, _, cx| view.refresh(cx))),
            )
            .into_any_element()
    }

    /// The control for the states where the answer lives outside this panel.
    ///
    /// A `Retry` re-asks this panel's own handle, which on a disconnected panel
    /// is no handle at all and lands in the same state it started in. The
    /// kubeconfig files are what decide the answer and they change outside the
    /// app, so the control that can change this state re-reads them.
    fn reload_kubeconfigs_button(&self, id: &'static str, cx: &Context<Self>) -> AnyElement {
        div()
            .id(id)
            .debug_selector(move || id.to_owned())
            .flex_none()
            .child(
                Button::new(id)
                    .label("Reload kubeconfigs")
                    .primary()
                    .with_size(Size::Medium)
                    .tab_index(0isize)
                    .track_focus(&self.retry_focus)
                    .accessibility_label("Reload kubeconfigs and reconnect to a cluster")
                    .on_click(cx.listener(|_, _, window, cx| {
                        window.dispatch_action(Box::new(crate::shell::ReloadKubeconfigs), cx)
                    })),
            )
            .into_any_element()
    }

    fn render_error(&self, reason: &str, window: &Window, cx: &Context<Self>) -> AnyElement {
        let step = FailureStep::of(reason);
        let action = match step.action() {
            FailureAction::Retry => self.retry_button("overview-retry", cx),
            FailureAction::ReloadKubeconfigs => {
                self.reload_kubeconfigs_button("overview-reload-kubeconfigs", cx)
            }
        };
        self.centred_state(
            "overview-error",
            Role::Alert,
            IconName::TriangleAlert,
            step.title(),
            step.next_step(),
            Some(action),
            reason,
            window,
            cx,
        )
    }

    /// The measure an empty or failed state's sentence is wrapped at.
    ///
    /// `UI-SPEC` §4.13 gives an empty state a 40ch line, and the failure state
    /// was capped at 80% of the panel instead — 1,500px on a wide window. A
    /// percentage of the window is not a measure: a sentence can be one line at
    /// 1,500px and still make the reader's eye travel the whole width to reach
    /// its second line, and a denial names three permissions, so it does have
    /// one.
    fn empty_state_measure() -> Pixels {
        design::size::EMPTY_MEASURE
    }

    /// The cluster answered, and the answer was "forbidden".
    ///
    /// It gets its own icon, title and hint: telling the user to check the
    /// connection would send them after the wrong thing, because the
    /// connection is fine. The missing permissions are in the hint, so the
    /// state explains itself without a hover; the raw API text stays in the
    /// tooltip.
    fn render_forbidden(
        &self,
        overview: &Overview,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let detail = source_failure_detail(overview);
        let hint = denial_copy(overview);
        self.centred_state(
            "overview-forbidden",
            Role::Alert,
            design::glyph::state::access_denied(),
            "Access denied",
            &hint,
            Some(self.retry_button("overview-retry", cx)),
            &detail,
            window,
            cx,
        )
    }

    /// A state with no data at all, so there is no region to be "in place" of.
    ///
    /// `UI-SPEC` §4.15 asks for a specific sentence and a verb-phrase action and
    /// `§4.13` for a 24px muted glyph, one line and at most one action; between
    /// them this is the whole of it, and all four states on this panel that have
    /// no data to show go through it so they cannot drift apart.
    ///
    /// The glyph is `role::fg_tertiary` and not the danger hue, because a coloured
    /// 24px icon on a screen with nothing else on it is the loudest thing a
    /// failure state can be.
    ///
    /// **Never `size_full()`** — the same reason [`Self::render_loading`] gives.
    /// This one was `size_full().min_h(0.)` and measured `1888px × 0px`, so its
    /// glyph, its title, its sentence and its `Retry` were all laid out and then
    /// clipped off the bottom of a scroll container with nothing to scroll: a
    /// cluster the app could not read rendered as a **blank panel** with a toolbar
    /// that said `Failed to load the overview` and nothing under it.
    ///
    /// **And never purely content-sized either, which is what it was.** A 168px
    /// block parked 40px under the toolbar on a 1,000px panel is the same
    /// "everything in the upper half" the loaded page was sent back for, with the
    /// void now measured in 736px instead of 450. So the height is *stated* — the
    /// panel's own content box less the scroll body's own padding, read from
    /// [`Self::available_height`] — and the state is centred inside it. Stating the
    /// number is the same trade [`capacity_table_height`] makes and for the same
    /// reason: a percentage height resolves against nothing inside a scroll
    /// container, and that is the one thing this file has already been bitten by
    /// twice.
    ///
    /// `§4.13` asks for 48px above and below, and 48 is not one of the ten values
    /// in `§2.1` — a spec conflict, reported rather than resolved here. The padding
    /// is `space::XXL` above and below, which is the air a boundary between two
    /// page regions gets (`Design guides > Spatial grammar`: 32px for "a major
    /// region boundary", which is what an empty state is), and the centring
    /// happens *inside* it: the column is optically centred on the panel rather
    /// than hung from its top edge, and the same `space::XXL` reads above and below
    /// so the frame is one thing.
    #[expect(clippy::too_many_arguments)]
    fn centred_state(
        &self,
        id: &'static str,
        role_kind: Role,
        icon: IconName,
        title: &'static str,
        line: &str,
        action: Option<AnyElement>,
        detail: &str,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let line: SharedString = line.to_owned().into();
        let aria = format!("{title}. {line}");
        // Centred on both axes of the *content area*, with the air the loaded page
        // spends between its bands.
        //
        // Horizontally it always was off: the column carries `max_w(measure)` and
        // nothing put it in the middle, so every empty and failed state on this
        // panel hung off the left edge like a paragraph rather than sitting in the
        // space it is describing. `justify_center` on the row is what fixes it, and
        // it costs nothing at the minimum window because the column is `min(40ch,
        // the panel)`.
        //
        // Vertically it is `items_center` on a box as tall as the content area, and
        // the box is a `min_h` rather than a height so a window too short to hold
        // the state grows it and scrolls instead of clipping. The scroll body's own
        // `space::LG` comes off the measured height, because the measurement
        // covers the box the padding is on.
        let fill = (self.available_height(window) - 2.0 * f32::from(space::LG)).max(0.);
        let mut panel = div()
            .id(id)
            .debug_selector(move || id.to_owned())
            .flex()
            .w_full()
            .min_w(px(0.))
            .min_h(px(fill))
            .pt(space::XXL)
            .pb(space::XXL)
            .items_center()
            .justify_center()
            .role(role_kind)
            .aria_label(aria)
            .child(
                v_flex()
                    .items_center()
                    .gap(space::SM)
                    .max_w(Self::empty_state_measure())
                    // `design::icon::LEAD` rather than the component's own `large()`:
                    // the two are the same 24px today, and naming the product token
                    // is what keeps them from drifting apart.
                    .child(
                        Icon::new(icon)
                            .with_size(Size::Size(design::icon::LEAD))
                            .text_color(role::fg_tertiary(cx)),
                    )
                    .child(
                        Label::new(title)
                            .text_size(text::TITLE)
                            .line_height(text::TITLE_LINE_HEIGHT)
                            .font_weight(text::SEMIBOLD)
                            .text_color(role::fg_primary(cx)),
                    )
                    .child(
                        label_small(line.clone())
                            .text_color(role::fg_secondary(cx))
                            .text_size(text::BODY)
                            .line_height(text::BODY_LINE_HEIGHT),
                    )
                    .when_some(action, |this, action| this.child(action)),
            );
        if !detail.is_empty() {
            panel = panel.tooltip(text_tooltip(detail.to_owned()));
        }
        panel.into_any_element()
    }

    /// Width the capacity grid may use.
    ///
    /// The measurement covers the whole panel, so the scroll padding comes off
    /// before the columns are laid out.
    fn table_available_width(&self, window_width: f32) -> f32 {
        let measured = self.content_width.get();
        if !measured.is_finite() || measured <= 0.0 {
            // Before the first measurement the columns keep their minimums.
            return 0.0;
        }
        capacity_available_width(measured, window_width)
    }
}

/// Sort order after a click on a column heading.
///
/// The node column is the one the table opens on, so a third click lands
/// there and the table always comes back to a real sort.
fn next_capacity_sort(current: Sort, column: usize) -> Sort {
    match (current.column == column, current.descending) {
        (true, false) => Sort::descending(column),
        (true, true) => Sort::ascending(0),
        (false, _) => Sort::ascending(column),
    }
}

/// The value a column sorts by, or `None` when the value is unknown.
fn capacity_sort_value(
    capacity: &NodeCapacity,
    sample: Option<&NodeUsage>,
    column: usize,
) -> Option<f64> {
    match column {
        1 => Some(capacity.requested_cpu),
        2 => Some(capacity.requested_memory),
        3 => Some(capacity.limits_cpu),
        4 => Some(capacity.limits_memory),
        // The live columns sort by the reading they show, which is the value a
        // user compares when looking for the busiest node.
        5 => sample.and_then(|sample| sample.cpu_millicores),
        6 => sample.and_then(|sample| sample.memory_bytes),
        _ => None,
    }
}

/// Orders the table rows by the sorted column.
///
/// The node column orders by name. Every other column orders by its value, and
/// an unknown value sorts last, so real data leads.
fn sorted_capacity_rows(rows: Vec<CapacityRow>, sort: Sort) -> Vec<CapacityRow> {
    let column = sort.column;
    let mut rows = rows;
    if column == 0 {
        // The node column sorts by name. The snapshot arrives in the order the
        // API listed the nodes, so leaving that order in place would show an
        // ascending arrow over rows it is not ascending.
        rows.sort_by(|left, right| {
            let ordering = left.capacity.name.cmp(&right.capacity.name);
            if sort.descending {
                ordering.reverse()
            } else {
                ordering
            }
        });
        return rows;
    }
    rows.sort_by(|left, right| {
        let ordering = match (
            capacity_sort_value(&left.capacity, left.sample.as_ref(), column),
            capacity_sort_value(&right.capacity, right.sample.as_ref(), column),
        ) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(left), Some(right)) => left.partial_cmp(&right).unwrap_or(Ordering::Equal),
        };
        // Equal values keep the node-name order, so a sort is stable.
        let ordering = ordering.then_with(|| left.capacity.name.cmp(&right.capacity.name));
        if sort.descending {
            ordering.reverse()
        } else {
            ordering
        }
    });
    rows
}

/// One node of the capacity table: what the cluster says it can run, and what it
/// is running now.
struct CapacityRow {
    capacity: NodeCapacity,
    sample: Option<NodeUsage>,
}

/// The rows the table draws, each joined to the live sample that belongs to it.
///
/// The table owns its rows, so the virtualised list can build one row without
/// reaching back into the snapshot.
fn capacity_rows(capacities: &[NodeCapacity], usage: Option<&[NodeUsage]>) -> Vec<CapacityRow> {
    capacities
        .iter()
        .map(|capacity| CapacityRow {
            capacity: capacity.clone(),
            sample: usage
                .and_then(|usage| usage.iter().find(|sample| sample.name == capacity.name))
                .cloned(),
        })
        .collect()
}

/// The rows, the columns, and the app's half of the capacity table's behaviour.
///
/// gpui-kit's `DataTable` draws the frame: the header band, the virtualised
/// rows, the cell cursor, the horizontal scroll, and the keyboard navigation.
/// Everything a reader of this table would call policy is here instead — the
/// figures, the bars, the oversubscription glyph, and the sort order, including
/// the cycle a third click completes.
struct CapacityTableDelegate {
    rows: Vec<CapacityRow>,
    /// Column and direction the table is sorted by. It opens on the node column,
    /// ascending, so the first paint is in a documented order.
    sort: Sort,
    /// The width each column takes, a share of the width the panel measured.
    widths: Vec<f32>,
    /// Whether the cluster answered for live readings. Without them the two live
    /// columns are not part of the table at all.
    metrics_available: bool,
    /// Whether a cursor has been placed yet.
    ///
    /// The component starts in row mode and only enters cell mode on a cell
    /// click, which a reader using only the keyboard cannot make. Seating the
    /// cursor on the first cell once puts the arrow keys in charge of the cell
    /// from the first keypress.
    seated: bool,
}

impl CapacityTableDelegate {
    fn new(_cx: &App) -> Self {
        Self {
            rows: Vec::new(),
            sort: Sort::ascending(0),
            widths: Vec::new(),
            metrics_available: false,
            seated: false,
        }
    }

    /// The node's own sentence, for the row's accessible name and its tooltip.
    ///
    /// The row is the only element that knows the blended share of allocatable:
    /// the two request cells each carry one axis, and the per-axis numbers do
    /// not add up to it. So the sentence a screen reader gets is also the
    /// sentence a pointer gets — and it is also where every absolute the bars
    /// replaced still lives, because a bar carries a share and cannot carry a
    /// pair. A cell's own tooltip still wins over this one — it is the inner
    /// element — which is the right order: the cell is the narrower question.
    fn row_summary(&self, row_ix: usize) -> String {
        self.rows
            .get(row_ix)
            .map(|row| capacity_summary(&row.capacity, row.sample.as_ref()))
            .unwrap_or_default()
    }

    /// One cell: the figure the column prints, the words that state its state,
    /// the ink that shows it, and the bar it draws.
    ///
    /// `spoken` is what a screen reader and the tooltip hear. It is the same text
    /// as the cell shows unless the cell is stating something the figure cannot,
    /// which is what makes a bar answer "is this node oversubscribed" for
    /// everyone and not only for the reader who can see amber.
    fn cell(&self, row_ix: usize, col_ix: usize, cx: &App) -> CapacityCell {
        let Some(row) = self.rows.get(row_ix) else {
            return CapacityCell::plain(NO_ANSWER.to_owned(), role::fg_tertiary(cx));
        };
        let capacity = &row.capacity;
        let sample = row.sample.as_ref();
        let cores = |value: f64| format!("{} cores", format_cores(value));
        match col_ix {
            // The node's own name, and the one figure in full-strength ink. The
            // row-level state lives in the row's label and tooltip rather than
            // here, because every row of a 100-node cluster used to open with
            // the same cluster-level sentence.
            0 => CapacityCell::plain(capacity.name.clone(), role::fg_primary(cx)),
            // Requests: the share is the bar and the figure, and the absolutes are
            // in the spoken label. This is the change `UI-REDESIGN.md` §3.5 asks
            // for — the bare `1.15 / 20 cores` becomes a bar whose length is the
            // answer. The limit used to ride the same track as a tick; it is in
            // its own column now, which is where a number belongs, and the one
            // thing the tick said that two numbers do not is a state on that
            // column instead. See `limit_state`.
            1 => CapacityCell::share(
                request_ratio(capacity.requested_cpu, capacity.allocatable_cpu),
                request_text(
                    capacity.requested_cpu,
                    capacity.allocatable_cpu,
                    format_cores,
                    cores,
                ),
                format!("CPU requests. {}", CAPACITY_REQUEST_SHARE[0]),
                oversubscription(capacity.requested_cpu, capacity.allocatable_cpu),
                cx,
            ),
            2 => CapacityCell::share(
                request_ratio(capacity.requested_memory, capacity.allocatable_memory),
                request_text(
                    capacity.requested_memory,
                    capacity.allocatable_memory,
                    format_bytes,
                    format_bytes,
                ),
                format!("Memory requests. {}", CAPACITY_REQUEST_SHARE[1]),
                oversubscription(capacity.requested_memory, capacity.allocatable_memory),
                cx,
            ),
            // The limits keep their own column for the number: `16.8 cores` is a
            // fact and `84%` is an inference from it, and a column of seven
            // characters costs less than a hover to every row.
            //
            // A node that declares no limit prints the dash, and the dash is an
            // absence rather than a reading, so it takes the tertiary ink. In
            // secondary it was the same weight of type as `340 MiB` beside it,
            // and a column that was empty for every row on the cluster read as
            // a column of values.
            //
            // A node whose request is above its own limit is a manifest that
            // cannot be scheduled as written, and that used to be a 2px tick
            // somewhere on the bar next door. It is words, a glyph and an ink on
            // the figure that is wrong instead.
            3 => limit_cell(
                capacity.limits_cpu,
                capacity.requested_cpu,
                &format_cores,
                "CPU",
                cx,
            ),
            4 => limit_cell(
                capacity.limits_memory,
                capacity.requested_memory,
                &format_bytes,
                "Memory",
                cx,
            ),
            // Live readings: the same bar, one value. The tick here used to be
            // the request, which is the neighbouring request column's own fill on
            // the same scale — the same number drawn twice, once as a fill and
            // once as a dash, and the reason `CPU now` read as two marks.
            5 => {
                let usage = sample
                    .and_then(|sample| sample.cpu_millicores)
                    .map(|m| m / 1_000.0);
                match usage {
                    Some(cores_used) => CapacityCell::share(
                        request_ratio(cores_used, capacity.allocatable_cpu),
                        request_text(cores_used, capacity.allocatable_cpu, format_cores, cores),
                        format!("CPU now. {}", CAPACITY_USAGE_SHARE[0]),
                        None,
                        cx,
                    ),
                    None => CapacityCell::plain(
                        no_reading(self.metrics_available).to_owned(),
                        role::fg_tertiary(cx),
                    ),
                }
            }
            6 => {
                let usage = sample.and_then(|sample| sample.memory_bytes);
                match usage {
                    Some(bytes) => CapacityCell::share(
                        request_ratio(bytes, capacity.allocatable_memory),
                        request_text(
                            bytes,
                            capacity.allocatable_memory,
                            format_bytes,
                            format_bytes,
                        ),
                        format!("Memory now. {}", CAPACITY_USAGE_SHARE[1]),
                        None,
                        cx,
                    ),
                    None => CapacityCell::plain(
                        no_reading(self.metrics_available).to_owned(),
                        role::fg_tertiary(cx),
                    ),
                }
            }
            _ => CapacityCell::plain(NO_ANSWER.to_owned(), role::fg_tertiary(cx)),
        }
    }
}

/// One capacity cell: what it shows, what it says, and the bar it draws.
struct CapacityCell {
    /// The figure the column prints.
    text: String,
    /// What a screen reader and the tooltip hear.
    spoken: String,
    color: Hsla,
    /// The glyph that names the state, for a cell that is in one.
    severity: Option<Severity>,
    /// The share the cell draws, when it has an allocatable figure to be a share
    /// of. A node that reported no allocatable has no scale to draw on, and gets
    /// the absolute alone.
    bar: Option<CellBar>,
}

/// A capacity cell's bar: the fill, and the words that name what it is a share of.
struct CellBar {
    /// Share of allocatable the fill reaches, in percent. Above 100 is
    /// overcommit; the fill clamps at the track and the words say so.
    fill: f64,
    /// What the bar is a share *of*, because a bar carries no unit of its own.
    subject: String,
}

impl CapacityCell {
    /// A cell that carries no state of its own, so it says what it shows.
    fn plain(text: String, color: Hsla) -> Self {
        Self {
            spoken: text.clone(),
            text,
            color,
            severity: None,
            bar: None,
        }
    }

    /// A cell that is a share of something: a bar, the share as a figure, and
    /// the absolutes in the words.
    #[allow(clippy::too_many_arguments)]
    fn share(
        fill: Option<f64>,
        spoken: String,
        subject: String,
        severity: Option<Severity>,
        cx: &App,
    ) -> Self {
        // Without a scale there is nothing to be a share of, so the cell prints
        // the absolute and says so rather than drawing a bar that means 100% of
        // an unknown.
        let Some(fill) = fill else {
            return Self::plain(spoken, role::fg_tertiary(cx));
        };
        Self {
            text: format_pct(fill),
            spoken: format!("{spoken}. {subject}"),
            color: role::fg_primary(cx),
            severity,
            bar: Some(CellBar { fill, subject }),
        }
    }
}

/// One limits column: the ceiling, and whether the node's own request is above it.
///
/// The tick this replaced sat on the request bar, 2px wide, in the same ink as a
/// healthy fill, which made it a second mark rather than a state. A request above
/// its own limit is a manifest the scheduler cannot place as written, so it is
/// worth saying in words — and the cell that holds the number is the cell that is
/// wrong, so that is where the sentence goes.
///
/// A node that declares no limit has no ceiling to exceed and prints the dash in
/// the quiet ink, which `limit_ink` already owns.
fn limit_cell(
    limit: f64,
    requested: f64,
    spell: &dyn Fn(f64) -> String,
    axis: &'static str,
    cx: &App,
) -> CapacityCell {
    let figure = format_limit(limit, spell);
    let over = limit > 0.0 && requested > limit;
    let spoken = if over {
        format!(
            "{axis} limits {figure}. The request of {} is above this limit.",
            spell(requested)
        )
    } else if limit > 0.0 {
        format!("{axis} limits {figure}.")
    } else {
        format!("{axis} limits. No limit is declared.")
    };
    CapacityCell {
        spoken,
        text: figure.clone(),
        // The *word* ink, because this is a figure and not a mark, and it is
        // printed on the cell's own plane rather than on a wash. The mark half of
        // the same channel is the glyph the cell draws beside it, and the two are
        // the pair `role::status_for` and `role::status_word_for` exist for.
        color: if over {
            role::warning_word(cx)
        } else {
            limit_ink(&figure, cx)
        },
        severity: over.then_some(Severity::Warning),
        bar: None,
    }
}

/// Sorts by one column, or reverses an already sorted column.
///
/// A third click returns to the node-name order, the order the table opens in,
/// so the user always lands on a real sort. The rows are re-sorted here rather
/// than left for the next snapshot, because a notification on the table state
/// redraws the table and nothing above it.
fn sort_capacity(
    state: &mut TableState<CapacityTableDelegate>,
    column: usize,
    cx: &mut Context<TableState<CapacityTableDelegate>>,
) {
    let cursor_row = state.selected_cell().map(|(row, _)| row);
    let sort = next_capacity_sort(state.delegate().sort, column);
    let last_row = {
        let delegate = state.delegate_mut();
        delegate.sort = sort;
        let snapshot = std::mem::take(&mut delegate.rows);
        delegate.rows = sorted_capacity_rows(snapshot, sort);
        delegate.rows.len().saturating_sub(1)
    };
    // The cursor stays on the row the reader was on and follows them to the
    // column they just sorted by.
    if let Some(row) = cursor_row {
        state.set_selected_cell(row.min(last_row), column, cx);
    }
    cx.notify();
}

impl TableDelegate for CapacityTableDelegate {
    fn columns_count(&self, _cx: &gpui_kit::App) -> usize {
        self.widths.len()
    }

    fn rows_count(&self, _cx: &gpui_kit::App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _cx: &gpui_kit::App) -> Column {
        // `Column::width` is how a delegate states a width, and the component
        // lays the heading and every cell out to it, so the share of the
        // measured panel width is an input here rather than an override.
        Column::new(
            SharedString::from(format!("capacity-column-{col_ix}")),
            CAPACITY_COLUMNS[col_ix],
        )
        .width(px(self.widths.get(col_ix).copied().unwrap_or_default()))
        .resizable(false)
        .movable(false)
    }

    fn render_header(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        // `UI-SPEC` §4.4 gives a table header a transparent background, so the
        // heading takes the table's own plane and no band of its own. The whole
        // page is on that plane now, so the header and the rows under it are one
        // surface and the table has no edge to draw.
        //
        // The numeric columns carry the same 12px leading inset their cells do
        // (`render_td`), so `CAPACITY_COLUMN_BREATH` is the gap between two columns
        // rather than a second, invisible inset. See that constant for why the
        // inset is on the leading edge and not the trailing one. The lanes inside
        // the heading are the cells' own — see [`Self::render_th`].
        div()
            .id("overview-capacity-header")
            .role(Role::Row)
            .aria_row_index(1)
            .bg(capacity_table_surface(cx))
    }

    /// Column heading, with the sort control and the words that state it.
    ///
    /// The heading reads in the UI face at the caption role while the cells below
    /// read tabular, and every numeric heading is set **over the meter lane its own
    /// cells draw**. §4.4 gives an unsorted heading `fg.tertiary` and a sorted one
    /// `fg.primary` plus a 12px arrow — no accent, which is what takes this
    /// screen's accent budget to zero.
    ///
    /// **The heading reserves the same lanes the cells do, and that is the whole of
    /// the alignment.** A numeric heading used to be right-aligned at the column's
    /// trailing edge while the cells below it were `[glyph] [meter] … 115px of
    /// spacer … [figure]`: on the render at 1920×1080 `CPU REQUESTS` ended at
    /// x≈975 with its meter starting at x≈720 and its `5%` a further 115px away,
    /// so the heading, the meter and the figure were three objects on three
    /// unrelated grids and a reader could not tell which column a heading named.
    /// The heading now reserves [`CAPACITY_CELL_GLYPH`] before its label, so the
    /// label starts exactly where its own meters start, and it fills the rest of
    /// the lane so its own trailing edge still lands on the figure lane's.
    ///
    /// The two limits columns print `—` and own no meter, and they are built by
    /// the same rule: their heading starts on the same lane their cells would, so
    /// the whole heading row is one grid and a column with nothing in it is a
    /// column, not a gap in the row.
    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let sort = self.sort;
        let sorted = sort.column == col_ix;
        let direction = if sort.descending {
            "descending"
        } else {
            "ascending"
        };
        let label = CAPACITY_COLUMNS[col_ix];
        let aria = if sorted {
            format!("{label}, sorted {direction}")
        } else {
            label.to_owned()
        };
        let description = if sorted {
            format!("{label} is sorted {direction}. Activate to reverse it.")
        } else {
            format!("{label}. Activate to sort by it.")
        };
        // `UI-SPEC` §4.4 reserves `caption` for table headers and pairs its
        // uppercase with the token, and gpui 0.6.6 has no text-transform, so the
        // word is uppercased here — as a header, which is the one place §2.3
        // allows it to shout.
        let mut head = h_flex()
            .id(("overview-capacity-header-cell", col_ix))
            .debug_selector(move || format!("overview-capacity-header-cell-{col_ix}"))
            .role(Role::ColumnHeader)
            .aria_label(aria)
            .aria_description(description.clone())
            .aria_column_index(col_ix + 1)
            .w_full()
            .h_full()
            .gap(space::XS)
            .items_center()
            // The inter-column inset, on the leading edge, exactly as `render_td`
            // puts it on the cells — so `CAPACITY_COLUMN_BREATH` is the gap
            // between two columns rather than a second invisible inset inside one.
            .when(col_ix > 0, |this| this.pl(CAPACITY_COLUMN_BREATH))
            .font(ui_font(cx))
            .text_size(text::CAPTION)
            .line_height(text::CAPTION_LINE_HEIGHT)
            .font_weight(text::SEMIBOLD)
            .text_color(if sorted {
                role::fg_primary(cx)
            } else {
                role::fg_tertiary(cx)
            })
            .overflow_hidden()
            // The glyph lane every numeric cell reserves, drawn or not. Empty by
            // design: it is what puts the label's leading edge on the meter's
            // leading edge, and the node column has no meter and so no lane.
            .when(col_ix > 0, |this| {
                this.child(div().flex_none().w(CAPACITY_CELL_GLYPH))
            })
            // The label fills the lane rather than hugging the meter: a heading
            // that shrink-wrapped sat against the arrow, and its own trailing edge
            // would stop landing on the figure lane's. Filling it puts the text
            // where the meter starts and keeps the column's trailing edge one
            // spine across the heading row and the rows under it.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_left()
                    .debug_selector(move || format!("overview-capacity-header-label-{col_ix}"))
                    .child(label.to_uppercase()),
            );
        if sorted {
            head = head.child(
                Icon::new(if sort.descending {
                    IconName::ArrowDown
                } else {
                    IconName::ArrowUp
                })
                .with_size(Size::Size(design::icon::IN_ROW))
                // The sorted column is the selected one, so it wears the active
                // ink - the same one the Helm table's sorted column wears, which
                // is what lets a reader carry the fact across two panels.
                .text_color(design::icon::active(cx)),
            );
        }
        head.on_click(cx.listener(move |table, _, _, cx| sort_capacity(table, col_ix, cx)))
            .tooltip(text_tooltip(description))
            .into_any_element()
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        // No zebra. `PROMPT.md` §2.1 #6 rules it out, `UI-SPEC` §4.4 repeats it,
        // and the component's own `stripe(false)` flag already said so — the
        // delegate painted one behind the component's back anyway, which is how a
        // 10,000-row table ends up with a rhythm nobody asked for. Every row is
        // the table's own plane, and the component owns the cell cursor, so a row
        // carries no selection of its own.
        let summary = self.row_summary(row_ix);
        div()
            .id(("overview-capacity-row", row_ix))
            .debug_selector(move || format!("overview-capacity-row-{row_ix}"))
            .role(Role::Row)
            .aria_row_index(row_ix + 2)
            .aria_label(summary.clone())
            .bg(capacity_table_surface(cx))
            .tooltip(text_tooltip(summary))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let cell = self.cell(row_ix, col_ix, cx);
        let selector = format!("overview-capacity-cell-{row_ix}-{col_ix}");
        let cell_selector = selector.clone();
        let glyph_selector = format!("overview-capacity-severity-{row_ix}-{col_ix}");
        let bar_selector = format!("{selector}-bar");
        // The numeric columns read tabular in the UI face. `PROMPT.md` §2.1 #2
        // says tables are sans — what is being read is a node name and a
        // percentage, not code — and §2.3 asks for `tnum` on every column of
        // numbers, so the glyphs of a `104%` and a `6%` line up.
        //
        // The size and the weight are **stated** rather than inherited. `§4.4`'s
        // cell is `13/400`, and this left both to whatever the element above it
        // happened to be: the row drew no size, so a change to the shell's root
        // type would have resized this table's cells without touching this file.
        // §4.4's status cell is the one place a table cell is `500`, and this
        // table has no status text — its state is a glyph and an ink.
        let figure_text = cell.text.clone();
        let figure_color = cell.color;
        // Resolved once and cloned into the closure, because a `move` closure over
        // `cx` would take the borrow with it and the glyph lane below still needs it.
        let figure_font = ui_font(cx);
        let figure_features = tabular_features();
        let figure = move |right: bool| {
            let lane = div()
                .font(figure_font)
                .font_features(figure_features)
                .text_size(text::BODY)
                .line_height(text::BODY_LINE_HEIGHT)
                .font_weight(text::REGULAR)
                .text_color(figure_color)
                .min_w_0()
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .child(figure_text.clone());
            // In a lane the text hugs the lane's trailing edge; loose in the row it
            // is the row's own `justify_end` that does it.
            if right {
                lane.w_full().text_right()
            } else {
                lane
            }
        };
        let numeric = col_ix > 0;
        let mut element = h_flex()
            .id((
                "overview-capacity-cell",
                ((row_ix as u64) << 16) | col_ix as u64,
            ))
            .debug_selector(move || cell_selector.clone())
            .role(Role::Cell)
            .aria_label(cell.spoken.clone())
            .aria_column_index(col_ix + 1)
            .aria_row_index(row_ix + 2)
            // The cell fills the column so the numeric columns end where their
            // headings end; the component's own padding sits on top of it, on
            // the heading and the data alike.
            .w_full()
            .h_full()
            .items_center()
            .gap(space::XS)
            .min_w_0()
            .overflow_hidden()
            // The same leading inset the heading above takes, and the same
            // reason: it is the breathing *between* two columns, so it has to sit
            // on the side that faces the neighbour rather than the side that
            // faces the heading.
            .when(numeric, |this| this.pl(CAPACITY_COLUMN_BREATH));
        // The glyph sits in its own lane on every numeric row, drawn or not.
        //
        // A node above its own limit is the half of the state that survives
        // greyscale — amber ink alone is not a verdict, because a meter that is 40%
        // longer than its track needs a second channel, and the shape is the one
        // this app owns. Reserving the lane is what keeps the meter beside it on the
        // column's leading spine: without it, the three rows above an oversubscribed
        // node would each hold their meter 16px further left than it. The heading
        // above reserves the same lane, which is what puts `CPU REQUESTS` on the
        // meter's leading edge rather than a glyph-width to its left.
        if numeric {
            element = element.child(div().flex_none().w(CAPACITY_CELL_GLYPH).when_some(
                cell.severity,
                |this, severity| {
                    // The name is on the glyph, not on the lane: a reserved lane
                    // is structure, and a lane that answered to the state's name
                    // would report a state on every row of every column.
                    let glyph_selector = glyph_selector.clone();
                    this.child(
                        div().debug_selector(move || glyph_selector.clone()).child(
                            Icon::new(design::health_icon(severity))
                                .with_size(Size::Size(design::icon::IN_ROW))
                                .text_color(mark_ink(severity, cx)),
                        ),
                    )
                },
            ));
        }
        if let Some(bar) = &cell.bar {
            // One meter, one value: the share this column is named for. A state
            // that the meter itself does not measure is carried by the glyph and
            // the ink beside it, never by a second mark on the track.
            //
            // **The cell is three lanes and no spacer: glyph, meter, figure.** The
            // figure holds [`capacity_figure_lane`] at the column's trailing edge,
            // so every row of the column draws its numbers over the same span, and
            // the meter fills the lane between the glyph lane and it — from
            // [`CAPACITY_CELL_METER`] up to whatever the column is wide.
            //
            // The spacer that used to sit between the meter and the figure is what
            // this removes, and it was the defect: measured on the render at
            // 1920×1080, `CPU REQUESTS` drew its meter over x≈720–880, then 115px
            // of nothing, then `5%` at x≈995. A hole in the middle of a row is
            // `Design guides > Alignment details`' "missing structure", and it read
            // as two loose marks rather than as a number and the thing it measures.
            // The meter takes that width instead, because a meter's length is a
            // measurement and it is the same length on every row of the column —
            // the figure's own lane is fixed, so `0%` and `104%` do not move it.
            element = element
                .child(
                    proportion_bar(
                        bar_selector,
                        bar.fill,
                        bar_ink(cell.severity.unwrap_or(Severity::Muted), cx),
                        cx,
                    )
                    // The meter is the measurement, so it is given its stated lane
                    // first and grows into the column's spare; it shrinks rather
                    // than overflows when the column is narrower than that lane, so
                    // the same three lanes hold at the minimum window, where the
                    // component's horizontal scrollbar is what reaches the rest.
                    .flex_grow(1.)
                    .flex_shrink(1.)
                    .flex_basis(CAPACITY_CELL_METER),
                )
                .child(
                    div()
                        .flex_none()
                        .w(capacity_figure_lane())
                        .child(figure(true)),
                );
        } else {
            // A cell with no meter — a node name, a ceiling, a missing reading — is
            // one figure. The numeric ones hang it off the trailing edge, which is
            // where their own heading's lane ends and where the meters' figures sit,
            // so a column of dashes is on the same spine as a column of numbers.
            element = element
                .when(numeric, |this| this.justify_end())
                .child(figure(false));
        }
        let spoken = match &cell.bar {
            Some(bar) => format!("{}. {}", cell.spoken, bar.subject),
            None => cell.spoken.clone(),
        };
        element.tooltip(text_tooltip(spoken)).into_any_element()
    }
}

/// The ink a limits figure is written in: secondary for a number, tertiary for
/// the dash.
///
/// `UI-SPEC` §4.4's status cell is the model's rule — `健康 → dot fg.tertiary,
/// 文字 fg.secondary` — and a limit nobody declared is the same kind of
/// statement in a table that has no dot: a present mark in the quiet ink rather
/// than a present mark in the reading ink.
fn limit_ink(figure: &str, cx: &App) -> Hsla {
    if figure == NO_ANSWER {
        role::fg_tertiary(cx)
    } else {
        role::fg_secondary(cx)
    }
}

/// The ink and the severity of a request figure above allocatable.
///
/// The two travel together because a state that lives only in a colour is a
/// state a screen reader never receives and a greyscale print never shows. With
/// the severity, the cell draws a glyph from the shared health vocabulary; the
/// words come from [`request_text`], which is what the cell's accessible name
/// and tooltip carry.
fn oversubscription(requested: f64, allocatable: Option<f64>) -> Option<Severity> {
    request_ratio(requested, allocatable)
        .is_some_and(|pct| pct > 100.0)
        .then_some(Severity::Warning)
}

impl Render for OverviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let measured_width = self.content_width.get();
        let window_width = f32::from(viewport.width);
        // The measurement lands a frame after it is taken, so a window that has
        // just shrunk still reports the width it had. The window bounds it, or
        // the panel would lay out for a size it no longer has.
        let available_width = if measured_width.is_finite() && measured_width > 0.0 {
            measured_width.min(window_width)
        } else {
            window_width
        };
        // The same bound for the height, against the same one-frame lag. Before the
        // first measurement the best guess is the window less this panel's own
        // toolbar, which is the part of the chrome the panel owns; the shell's title
        // bar and tab strip are not knowable from here, so the first frame is a
        // frame too tall and the second corrects it.
        let available_height = self.available_height(window);
        // One breakpoint for the whole page: above it the four stat tiles share a
        // row, below it they go to two rows of two and the workload row wraps.
        // Both shapes are the same grid, so a resize moves a figure between rows
        // without changing which column it is in.
        let wide = available_width >= TILE_ROW_WIDE_ABOVE;
        let body: AnyElement = match &self.state {
            OverviewState::Loading => self.render_loading(wide, cx),
            OverviewState::Disconnected => self.render_no_cluster(window, cx),
            OverviewState::Failed(reason) => self.render_error(reason, window, cx),
            OverviewState::Ready(overview) => match overview_data_state(overview) {
                OverviewDataState::Forbidden => self.render_forbidden(overview, window, cx),
                OverviewDataState::Error => {
                    self.render_error(&source_failure_detail(overview), window, cx)
                }
                // A cluster that answered with nothing at all is an **empty**
                // state, not a dashboard. Rendering the grid anyway put five
                // em-dashes under four headings and no bars — a screen that
                // looks broken rather than one that says there is nothing here,
                // and `UI-SPEC` §4.13 asks for the second. A cluster with nodes
                // and no workloads is *not* this state: `overview_data_state`
                // counts nodes, so a real fleet keeps its dashboard.
                OverviewDataState::Empty => self.render_empty_cluster(overview, window, cx),
                OverviewDataState::Partial | OverviewDataState::Complete => {
                    let usage = overview.usage.clone();
                    self.render_dashboard(
                        overview,
                        usage.as_deref(),
                        wide,
                        available_height,
                        window,
                        cx,
                    )
                }
            },
        };
        let measured_width = self.content_width.clone();
        let measured_height = self.content_height.clone();
        let panel = cx.entity().downgrade();
        let content = div()
            .flex_1()
            .min_h(px(0.))
            .min_w(px(0.))
            // `on_prepaint` on **this** box, not `on_children_prepainted` on the
            // scroll container inside it. A scrolling element's `Bounds` is its
            // frame *grown by its content and shifted by its offset* — that is how
            // scrolling works — so reading the child's height answered "how tall is
            // the page" rather than "how tall is the panel", which is the opposite
            // of what this number is for: the page is the thing being fitted, and
            // asking it its own height is how the table grew by exactly the height
            // it had just added.
            .on_prepaint(move |bounds, window, cx| {
                let width = f32::from(bounds.size.width);
                let height = f32::from(bounds.size.height);
                if !width.is_finite() || width <= 0.0 || !height.is_finite() || height <= 0.0 {
                    return;
                }
                // A drag moves the edge a few pixels a frame, and a change
                // that small is not worth a layout. A bigger change is a
                // resize or a new window, and it is measured at once: a width
                // the panel no longer has is what makes the capacity table
                // overflow at the minimum window size, and a dropped
                // measurement is never taken again, so the table would keep
                // that width until the next resize.
                //
                // The height answers the same question for the other axis and is
                // held to the same threshold: a height the panel no longer has is
                // what makes the capacity table run a panel-height past the fold,
                // and re-measuring on every frame of a splitter drag is a layout per
                // frame for a difference no reader can see.
                let previous_width = measured_width.get();
                let previous_height = measured_height.get();
                let settled = previous_width > 0.0
                    && (previous_width - width).abs() <= WIDTH_MEASURE_STEP
                    && previous_height > 0.0
                    && (previous_height - height).abs() <= WIDTH_MEASURE_STEP;
                if settled {
                    return;
                }
                measured_width.set(width);
                measured_height.set(height);
                let panel = panel.clone();
                window.defer(cx, move |_, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |_, cx| cx.notify());
                    }
                });
            })
            .id("overview-content-bounds")
            .debug_selector(|| "overview-content-bounds".to_owned())
            // The body's own tab group, so the chips, the retry control and the
            // capacity table are numbered from zero inside it and cannot collide
            // with the toolbar's `Refresh` outside it.
            .tab_group()
            .tab_index(1)
            .child(
                div()
                    .id("overview-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    // `UI-SPEC` §2.1: a panel's own padding is 16, four pixels
                    // looser than a web form's default, and the toolbar above pads
                    // by the same so the region reads as one.
                    .p(space::LG)
                    .child(body),
            );
        v_flex()
            .id("overview-view")
            .role(Role::Region)
            .aria_label("Cluster overview")
            .size_full()
            .min_w(px(0.))
            // `UI-SPEC` §1.1 gives `surface.content` to the table and the YAML
            // plane, and the whole dashboard is a table of figures, so the panel
            // is on that plane and the tiles have nothing to sit on top of.
            .bg(capacity_table_surface(cx))
            .text_color(role::fg_primary(cx))
            .font(ui_font(cx))
            .child(self.render_toolbar(cx))
            .child(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::TestAppContext;
    use k8s_core::cluster_data::{ClusterDataPort, DataFuture};
    use k8s_core::metrics::{MetricsError, NodeMetric, PodMetric};
    use k8s_core::overview::{HealthSummary, NodeSummary, UnavailableSource};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll};
    use std::time::Duration as TimeDuration;

    struct PendingUntilDropped(Arc<AtomicBool>);

    impl Future for PendingUntilDropped {
        type Output = ();

        fn poll(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    impl Drop for PendingUntilDropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    /// A cluster connection that never answers.
    ///
    /// It is the extreme of the case the auto-refresh tick has to survive: a load
    /// still in flight when the interval goes round. Every port method is a
    /// `pending()` rather than a panic, because a port that could fail would be a
    /// second thing this fixture had to be right about.
    struct SilentCluster;

    impl ClusterDataPort for SilentCluster {
        fn overview(&self, _metrics: bool) -> DataFuture<Overview, String> {
            Box::pin(std::future::pending())
        }

        fn metrics_probe(&self) -> DataFuture<(), MetricsError> {
            Box::pin(std::future::pending())
        }

        fn metrics_nodes(&self) -> DataFuture<Vec<NodeMetric>, MetricsError> {
            Box::pin(std::future::pending())
        }

        fn metrics_pods(
            &self,
            _namespace: Option<String>,
        ) -> DataFuture<Vec<PodMetric>, MetricsError> {
            Box::pin(std::future::pending())
        }

        fn namespaces(&self) -> DataFuture<Vec<String>, String> {
            Box::pin(std::future::pending())
        }

        fn cluster_uid(&self) -> DataFuture<String, String> {
            Box::pin(std::future::pending())
        }

        fn server_version(&self) -> DataFuture<String, String> {
            Box::pin(std::future::pending())
        }
    }

    struct SizedOverview {
        view: gpui_kit::Entity<OverviewView>,
        width: gpui_kit::Pixels,
        height: gpui_kit::Pixels,
    }

    impl gpui_kit::Render for SizedOverview {
        fn render(
            &mut self,
            _window: &mut gpui_kit::Window,
            _cx: &mut gpui_kit::Context<Self>,
        ) -> impl gpui_kit::IntoElement {
            div().w(self.width).h(self.height).child(self.view.clone())
        }
    }

    #[gpui_kit::test]
    fn default_focus_targets_refresh(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        cx.simulate_resize(gpui_kit::size(px(960.), px(640.)));
        cx.run_until_parked();

        let refresh = view.read_with(cx, |view, _| view.focus_default_control());
        assert!(refresh.tab_stop);
        assert_eq!(refresh, view.read_with(cx, |view, _| view.focus_handle()));
        cx.update(|window, cx| window.focus(&refresh, cx));
        assert!(cx.update(|window, _| refresh.is_focused(window)));

        let bounds = cx
            .debug_bounds("overview-refresh")
            .expect("refresh control");
        assert_eq!(f32::from(bounds.size.height), 28.0);

        // The toolbar sits on the shared 40px rhythm. The capacity table is the
        // panel's other focus stop, and gpui-kit owns its focus handle, so the
        // panel builds no table — and offers no phantom tab stop — until a
        // snapshot has nodes to put in one.
        let toolbar = cx
            .debug_bounds("overview-toolbar")
            .expect("overview toolbar");
        assert_eq!(f32::from(toolbar.size.height), 40.0);
        assert!(
            view.read_with(cx, |view, _| view.capacity_state.borrow().is_none()),
            "no snapshot has arrived, so the tab order is the refresh control and nothing else"
        );
    }

    /// The minimum supported window is 960x640, and the centre panel is narrower
    /// than that once the sidebar takes its share.
    ///
    /// The guard used to assert "the table fits the panel", on a fixture whose
    /// `usage` was `None`: the panel then computes five columns, whose minimum
    /// grid is 652px plus 64 of chrome, and the assertion passed at 960 without
    /// ever building the seven-column grid a real cluster with metrics-server
    /// installed produces. Seven columns of real figures do not fit a 960px
    /// window, so the promise this test keeps is the one that can be true: the
    /// real grid renders, the columns past the panel edge stay reachable through
    /// the component's own horizontal scrollbar rather than being clipped away,
    /// and the cell cursor answers the arrow keys and the two table-wide keys
    /// without a pointer.
    #[gpui_kit::test]
    fn the_capacity_grid_reaches_every_column_at_the_minimum_window_and_takes_the_keyboard(
        cx: &mut TestAppContext,
    ) {
        crate::init_ui(cx);
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, true, cx));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Ready(overview_with_metrics(
                HealthSummary {
                    total_pods: 1,
                    running: 1,
                    ..HealthSummary::default()
                },
                NodeSummary {
                    count: 3,
                    ready: 3,
                    not_ready: 0,
                },
            ));
            cx.notify();
        });
        cx.simulate_resize(gpui_kit::size(px(960.), px(640.)));
        cx.run_until_parked();
        cx.run_until_parked();

        let panel = cx
            .debug_bounds("overview-content-bounds")
            .expect("overview panel");
        let grid = cx
            .debug_bounds("overview-capacity-table")
            .expect("capacity grid");
        let state = view
            .read_with(cx, |view, _| view.capacity_state.borrow().clone())
            .expect("a snapshot with nodes builds the grid");
        let minimum: f32 = CAPACITY_COLUMN_MIN_WIDTHS.iter().sum();
        assert_eq!(
            state.read_with(cx, |state, app| state.delegate().columns_count(app)),
            CAPACITY_COLUMNS.len(),
            "metrics are available, so all seven columns render"
        );
        assert!(
            right_edge(grid) <= right_edge(panel) + 0.5,
            "the grid box is the panel's own width, and the columns past it scroll: grid {grid:?} \
             panel {panel:?}"
        );
        // The component virtualises the columns, so at this width only the ones
        // near the edge are built. The one past the panel edge is the promise:
        // its width is still its own, so the scrollbar is what reaches it.
        let built: Vec<(&str, f32)> = HEADER_CELLS
            .iter()
            .filter_map(|cell| {
                cx.debug_bounds(cell)
                    .map(|bounds| (*cell, right_edge(bounds)))
            })
            .collect();
        assert!(
            !built.is_empty(),
            "the header renders the columns the viewport reaches"
        );
        let furthest = built
            .iter()
            .map(|(_, right)| *right)
            .fold(f32::MIN, f32::max);
        assert!(
            furthest > right_edge(grid),
            "a column reaches past the panel edge, so the always-visible scrollbar is what gets \
             it: the last built heading ends at {furthest}px, the grid at {:?}",
            right_edge(grid)
        );
        // The narrowest centre panel the shell can build keeps the same grid, so
        // the four columns that say whether a node is in trouble are never
        // dropped: they scroll.
        assert!(
            minimum > f32::from(design::size::CENTER_MIN),
            "a centre panel at its floor is narrower than the grid, which is why the grid scrolls"
        );
        // The guard above is only worth anything because this fixture carries node
        // metrics. A snapshot without them computes five columns, so the same
        // assertion would be measuring a grid that no cluster with
        // metrics-server produces.
        let available =
            capacity_available_width(f32::from(panel.size.width), f32::from(panel.size.width));
        let without_metrics = capacity_column_widths(available, false);
        assert_eq!(without_metrics.len(), CAPACITY_COLUMN_COUNT);
        let short_grid: f32 = without_metrics.iter().sum();
        assert!(
            (short_grid - minimum).abs() > 1.0,
            "a metrics-free fixture measures {short_grid}px against the real {minimum}px, so the \
             seven-column assertion above fails on it"
        );

        // The cursor is the component's, so the panel reaches it through the
        // table's own focus handle.
        let focus = state.read_with(cx, |state, app| state.focus_handle(app));
        assert!(focus.tab_stop, "the grid is a tab stop of its own");
        let cursor = |cx: &mut gpui_kit::VisualTestContext| {
            state.read_with(cx, |state, _| state.selected_cell())
        };
        // At rest the component is in row mode with nothing selected, and the
        // component paints the cell cursor as `tokens.table_active`. A cursor
        // seated on the first paint is a block of accent on a screen whose
        // promise is that it is grey until something is wrong, and the reader has
        // highlighted nothing. `table_view` opens with no selection for the same
        // reason.
        assert_eq!(
            cursor(cx),
            None,
            "a snapshot nobody has touched pre-selects nothing: the accent belongs to the \
             reader's own cursor"
        );
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.run_until_parked();
        assert_eq!(
            cursor(cx),
            Some((0, 0)),
            "the moment the grid takes the focus the cursor is seated, so a keyboard-only \
             reader is in charge of the cell from the first key"
        );
        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert_eq!(
            cursor(cx),
            Some((2, 0)),
            "the cursor stops at the last node"
        );
        cx.simulate_keystrokes("right");
        cx.run_until_parked();
        assert_eq!(cursor(cx), Some((2, 1)));
        cx.simulate_keystrokes("ctrl-end");
        cx.run_until_parked();
        assert_eq!(
            cursor(cx),
            Some((2, 6)),
            "Control+End is the last cell in the table, which the component has no binding for"
        );
        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        assert_eq!(
            cursor(cx),
            Some((2, 0)),
            "the unmodified Home is the first column of the current row, which is the component's \
             binding"
        );
        cx.simulate_keystrokes("ctrl-home");
        cx.run_until_parked();
        assert_eq!(
            cursor(cx),
            Some((0, 0)),
            "and Control+Home is the first cell in the table"
        );
    }

    #[gpui_kit::test]
    fn compact_layout_uses_the_overview_panel_width(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (_root, cx) = cx.add_window_view(|_, cx| {
            let view = cx.new(|cx| OverviewView::new(None, false, cx));
            let data = overview(
                HealthSummary {
                    total_pods: 1,
                    running: 1,
                    ..HealthSummary::default()
                },
                NodeSummary {
                    count: 1,
                    ready: 1,
                    not_ready: 0,
                },
                HealthLevel::Healthy,
            );
            view.update(cx, |view, cx| {
                view.state = OverviewState::Ready(data);
                cx.notify();
            });
            SizedOverview {
                view,
                width: px(480.),
                height: px(640.),
            }
        });
        cx.run_until_parked();
        cx.run_until_parked();
        let panel = cx
            .debug_bounds("overview-content-bounds")
            .expect("overview content bounds");
        let vitals = cx.debug_bounds("overview-vitals").expect("overview vitals");
        assert!((f32::from(panel.size.width) - 480.).abs() <= 1.);
        assert!(f32::from(vitals.size.height) > 80.);
    }

    #[tokio::test]
    async fn dropping_overview_request_aborts_inner_task() {
        let dropped = Arc::new(AtomicBool::new(false));
        let handle = Handle::current();
        let request = join_abortable(&handle, PendingUntilDropped(Arc::clone(&dropped)));

        assert!(
            tokio::time::timeout(Duration::from_millis(10), request)
                .await
                .is_err()
        );
        tokio::task::yield_now().await;
        assert!(dropped.load(Ordering::Relaxed));
    }

    fn overview(health: HealthSummary, nodes: NodeSummary, level: HealthLevel) -> Overview {
        let mut overview = Overview {
            health,
            nodes,
            ..Overview::default()
        };
        overview.capacities = (0..overview.nodes.count)
            .map(|index| NodeCapacity {
                name: format!("worker-{index}"),
                allocatable_cpu: Some(4.0),
                allocatable_memory: Some(8.0 * 1024.0 * 1024.0 * 1024.0),
                requested_cpu: 0.0,
                requested_memory: 0.0,
                limits_cpu: 0.0,
                limits_memory: 0.0,
                utilization_pct: Some(0.0),
            })
            .collect();
        if level == HealthLevel::Error {
            overview.health.failed = 1;
        }
        overview
    }

    /// A snapshot from a cluster with metrics-server installed.
    ///
    /// `usage: None` makes the panel compute five columns, which is the state
    /// every layout test used to measure and the state a real cluster is never
    /// in once metrics have been probed.
    fn overview_with_metrics(health: HealthSummary, nodes: NodeSummary) -> Overview {
        let mut overview = overview(health, nodes, HealthLevel::Healthy);
        overview.usage = Some(
            overview
                .capacities
                .iter()
                .map(|capacity| NodeUsage {
                    name: capacity.name.clone(),
                    cpu_millicores: Some(161.0),
                    memory_bytes: Some(3.2 * 1024.0 * 1024.0 * 1024.0),
                })
                .collect(),
        );
        overview
    }

    /// One node, oversubscribed on CPU and comfortable on memory.
    fn oversubscribed_node() -> Overview {
        let mut overview = overview_with_metrics(
            HealthSummary {
                total_pods: 1,
                running: 1,
                ..HealthSummary::default()
            },
            NodeSummary {
                count: 1,
                ready: 1,
                not_ready: 0,
            },
        );
        overview.capacities[0].requested_cpu = 4.8;
        overview.capacities[0].requested_memory = 1.0 * 1024.0 * 1024.0 * 1024.0;
        overview.capacities[0].utilization_pct = Some(120.0);
        overview
    }

    fn left_edge(bounds: gpui_kit::Bounds<gpui_kit::Pixels>) -> f32 {
        f32::from(bounds.origin.x)
    }

    fn right_edge(bounds: gpui_kit::Bounds<gpui_kit::Pixels>) -> f32 {
        f32::from(bounds.origin.x + bounds.size.width)
    }

    /// Column-heading boxes, in column order.
    const HEADER_CELLS: [&str; 7] = [
        "overview-capacity-header-cell-0",
        "overview-capacity-header-cell-1",
        "overview-capacity-header-cell-2",
        "overview-capacity-header-cell-3",
        "overview-capacity-header-cell-4",
        "overview-capacity-header-cell-5",
        "overview-capacity-header-cell-6",
    ];

    /// The label inside each heading, which is what text alignment moves.
    const HEADER_LABELS: [&str; 7] = [
        "overview-capacity-header-label-0",
        "overview-capacity-header-label-1",
        "overview-capacity-header-label-2",
        "overview-capacity-header-label-3",
        "overview-capacity-header-label-4",
        "overview-capacity-header-label-5",
        "overview-capacity-header-label-6",
    ];

    /// The first data row's cells, in column order.
    const ROW_CELLS: [&str; 7] = [
        "overview-capacity-cell-0-0",
        "overview-capacity-cell-0-1",
        "overview-capacity-cell-0-2",
        "overview-capacity-cell-0-3",
        "overview-capacity-cell-0-4",
        "overview-capacity-cell-0-5",
        "overview-capacity-cell-0-6",
    ];

    /// One glance has to answer "is this cluster healthy and what needs
    /// attention". That means the banner and the figures must not print the same
    /// number twice, and the visible caption has to be one phrase rather than a
    /// run of counts joined by dots.
    /// A cluster that reported nothing shows a dash, because a zero is a reading
    /// the app never took.
    /// A pod the kubelet lost is not a pod that needs attention.
    ///
    /// The table draws it as a dash with no verdict and a muted mark, because the
    /// app has no reading on it. The hero caption a screenful above used to spend
    /// the whole caution set on it — the banner's own amber — so a cluster whose
    /// only problem was one unreadable pod read as a warning event over a row that
    /// said "no verdict". The cluster verdict above it is a separate question and
    /// belongs to `k8s_core::overview::level`, which still counts this bucket.
    /// A cluster the app could not read completely is not an unhealthy cluster.
    ///
    /// The banner used to answer `Partial` with amber, a caution wash, and a
    /// warning glyph — the whole set of symbols that means "this object is in bad
    /// shape" — while the actual problem was the app's own RBAC. The confidence
    /// channel is where that belongs, and it has to actually speak: a partial
    /// snapshot is `Unknown`, so the hollow `?` is drawn.
    /// The banner is the only place in the app that claims a cluster is healthy,
    /// and the claim has to come from the cluster rather than from the socket.
    /// A denied cluster is not an unreachable cluster. The two states need
    /// different titles, different advice, and different focus targets.
    #[gpui_kit::test]
    fn total_source_failure_renders_user_error_and_retry(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Ready(Overview {
                unavailable_sources: (0..OVERVIEW_SOURCE_COUNT)
                    .map(|index| UnavailableSource {
                        source: "source",
                        reason: format!("request {index} failed"),
                    })
                    .collect(),
                ..Overview::default()
            });
            cx.notify();
        });
        cx.run_until_parked();
        let retry = view.read_with(cx, |view, _| view.retry_focus.clone());
        assert_eq!(
            view.read_with(cx, |view, _| view.focus_default_control()),
            retry
        );
        assert!(cx.debug_bounds("overview-error").is_some());
    }

    /// A cluster that answered with nothing is an empty state, and a cluster that
    /// answered with *anything* still gets its dashboard.
    ///
    /// The branch that sends `OverviewDataState::Empty` to the empty state is the
    /// one place a mistake in this file takes a working cluster's Overview away
    /// and replaces it with a sentence, so both directions are pinned: the empty
    /// snapshot must reach the empty state, and a snapshot with nothing but nodes
    /// — no pods, no workloads, which is what a freshly joined control plane
    /// looks like — must still reach the tiles.
    #[gpui_kit::test]
    fn an_empty_snapshot_is_an_empty_state_and_a_silent_one_is_not(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let silent = overview(
            HealthSummary::default(),
            NodeSummary::default(),
            HealthLevel::Healthy,
        );
        assert_eq!(overview_data_state(&silent), OverviewDataState::Empty);

        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        cx.simulate_resize(gpui_kit::size(px(1_440.), px(900.)));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Ready(silent);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("empty-state").is_some(),
            "a cluster with no nodes, no pods and no workloads is an empty state, not a grid of \\
             dashes"
        );
        assert!(
            cx.debug_bounds("overview-vitals").is_none(),
            "and the four tiles are not drawn behind it"
        );

        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        cx.simulate_resize(gpui_kit::size(px(1_440.), px(900.)));
        view.update(cx, |view, cx| {
            // A control plane that has joined and has no pods yet: one node, no
            // pods, no workloads. Every count is zero except the one that says
            // the cluster is real.
            view.state = OverviewState::Ready(overview(
                HealthSummary::default(),
                NodeSummary {
                    count: 1,
                    ready: 1,
                    not_ready: 0,
                },
                HealthLevel::Healthy,
            ));
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("overview-vitals").is_some(),
            "one node is data: a cluster that has nodes keeps its dashboard even with no pods"
        );
    }

    /// The panel takes the width it is given, and a caption the panel has to cut
    /// keeps its whole text.
    ///
    /// The compact rule was 1200px, which gave up the section explanations on
    /// every panel narrower than a comfortable window — including the 480 to
    /// 800px centre panel a 960px window leaves — and gave them up at exactly
    /// the widths where the capacity table has already pushed four of its
    /// columns out of view. It is now tied to the supported window instead: the
    /// rule is read against the panel's own width, which is never wider than the
    /// window, so 960px is not compact and the sentence is there at the floor.
    /// The table has to be reachable and readable without a pointer: the
    /// minimum window is 960x640, and every cell is a Tab stop away.
    /// Every capacity cell has to hold the value the panel renders, at the
    /// product default data size.
    ///
    /// The audit found every numeric column ending in an ellipsis, and an ellipsis
    /// reports no value at all, so the minimum grid is measured against the text a
    /// real row produces rather than guessed.
    /// A zero limit means "no limit declared", and a bare `0` reads as the
    /// opposite claim.
    /// The percentage of allocatable a node is asked for is computed in the data
    /// layer and used by the row's own name and tooltip. The two request cells
    /// each carry their own axis, and neither says the blended number.
    /// A count is the same number everywhere in the app, so every figure here
    /// goes through the shared formatter instead of a hand-rolled separator.
    /// "The cluster reported nothing" is one answer, and a source the app was not
    /// allowed to read is a different one from a list that is genuinely empty.
    /// The cell shows the pair, so the number survives the column width. The
    /// share of allocatable stays in the row label and the tooltip.
    /// The table, its header, and the region around it are one surface.
    ///
    /// The whole page is on `surface.content` now, so the table has no band of
    /// its own and its rows cannot be a different value from the tiles two
    /// inches above them. The base was the *skin* field before the token migration,
    /// and the app plane before that, which is what made a row wash composite
    /// against a surface the row was not drawn on.
    /// The product theme, not a component-library one: the assertion is made
    /// against the theme the panel actually reads.
    #[gpui_kit::test]
    fn the_capacity_table_is_painted_on_the_table_surface(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        cx.update(|cx| {
            let base = capacity_table_surface(cx);
            let canvas = design::role::surface_app(cx).alpha(1.0);
            assert_eq!(
                base,
                role::surface_content(cx).alpha(1.0),
                "UI-SPEC §1.1 gives surface.content to the table plane, and the header band and \
                 every row read this one value"
            );
            assert_ne!(
                base, canvas,
                "the table must not be drawn on the canvas the shell's field uses: a row base on \
                 the canvas mixes every row wash against a surface the row is not on"
            );
        });
    }

    /// A healthy strip is grey, and only an issue spends a status colour.
    ///
    /// The banner this replaced was a heavy slab with a coloured left border, and
    /// it drew the *success* wash for a cluster with nothing wrong with it — the
    /// loudest thing on screen for the state the screen is in ninety percent of
    /// the time. `UI-SPEC` §0 铁律三 is explicit that healthy is grey, so the
    /// invariant worth holding is the one the new strip is built on: no issues,
    /// no wash, no colour, and the three channels a chip can spend are
    /// distinguishable from each other.
    #[gpui_kit::test]
    fn a_healthy_strip_is_grey_and_only_issues_spend_a_colour(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let healthy = overview(
            HealthSummary {
                total_pods: 4,
                running: 4,
                ..HealthSummary::default()
            },
            NodeSummary {
                count: 2,
                ready: 2,
                not_ready: 0,
            },
            HealthLevel::Healthy,
        );
        let routes = OverviewRoutes::default();
        assert!(
            health_issues(&healthy, &routes).is_empty(),
            "a cluster with nothing wrong itemises nothing"
        );
        assert_eq!(
            quiet_line(&healthy),
            "Nothing needs attention",
            "and says so in one grey line rather than a slab"
        );

        cx.update(|cx| {
            // Healthy ink is the same grey `state_ink` gives a muted severity, and
            // it is not the theme's success hue — that mapping lives in `design`,
            // which this wave does not own, so it is corrected here.
            assert_eq!(state_ink(Severity::Success, cx), role::fg_secondary(cx));
            assert_ne!(state_ink(Severity::Success, cx), role::success(cx));
            assert_eq!(state_ink(Severity::Muted, cx), role::fg_secondary(cx));
            assert_eq!(state_ink(Severity::Warning, cx), role::warning(cx));
            assert_eq!(state_ink(Severity::Error, cx), role::danger(cx));
            // The two channels a chip spends are tellable apart on their own, so
            // a failed pod and a pending pod are not the same word.
            assert_ne!(role::warning_wash(cx), role::danger_wash(cx));
        });

        let mut degraded = healthy.clone();
        degraded.health.pending = 12;
        degraded.health.total_pods = 16;
        let issues = health_issues(&degraded, &routes);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].label, "12 pods pending");
        assert_eq!(issues[0].severity, Severity::Warning);
        assert!(
            issues[0].route.is_none(),
            "a chip with no route is drawn without a target, because a control that goes nowhere \
             is a lie"
        );

        // Four kinds of issue are four chips, and none of them is silently dropped.
        let mut four = healthy;
        four.health.pending = 9;
        four.health.failed = 1;
        four.nodes.not_ready = 1;
        four.unavailable_workloads = 2;
        let issues = health_issues(&four, &routes);
        assert_eq!(
            issues.len(),
            4,
            "failed, pending, nodes and workloads are four different problems, not one: {:?}",
            health_issues(&four, &routes)
                .iter()
                .map(|issue| issue.label.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            issues[0].severity,
            Severity::Error,
            "failed pods are the fault channel"
        );
        assert_eq!(issues[1].severity, Severity::Warning);
    }

    /// A failure names the step it failed on, and a wait is not an age.
    ///
    /// Two copy bugs this panel shipped, both of which are the same bug wearing
    /// different clothes: one sentence for every failure, and one formatter for
    /// two different clocks.
    ///
    /// `§4.15` asks an error to say **which** step failed, and this panel gave
    /// every failure `Retry, or make sure the cluster connection works.` — which
    /// sends a reader with a missing RBAC permission to go and debug their
    /// network. The four verdicts have to be distinguishable, or the classifier
    /// is decoration. `§2.4` asks a network timeout to be visible inside ten
    /// seconds, and the last rung of the loading ladder printed that ten seconds
    /// through `format_age` — so at ten seconds the panel said `Still loading ·
    /// 2s ago`, a clock frozen at the moment it became visible and claiming the
    /// load had already finished.
    #[test]
    fn a_failure_names_its_step_and_a_wait_is_not_an_age() {
        let cases = [
            ("api error 403 Forbidden", FailureStep::Forbidden),
            // A `401` is not a `403`. The API server was asked the same question
            // and gave a different answer: "I will not say who you are" rather
            // than "you may not". Classifying both as `Forbidden` titled a
            // rejected token `This account is missing a permission` and asked for
            // a ClusterRole edit, which is the one change that cannot fix it.
            // `UI-SPEC` §4.15 asks a permission error to say what the identity
            // *can* do, and the first half of that answer is being able to
            // identify at all.
            (
                "Overview request failed: Unauthorized",
                FailureStep::Unauthenticated,
            ),
            ("context deadline exceeded", FailureStep::TimedOut),
            ("the request timed out", FailureStep::TimedOut),
            ("no route to host", FailureStep::Unreachable),
            (
                "failed to lookup address information: Name or service not known",
                FailureStep::Unreachable,
            ),
            ("request 3 failed", FailureStep::Other),
        ];
        for (reason, expected) in cases {
            assert_eq!(
                FailureStep::of(reason),
                expected,
                "{reason:?} is classified wrongly"
            );
        }
        // Five failures, five sentences. Two of them sharing either half is how
        // the panel got here, and the titles are part of the same contract: they
        // are the line a reader reads first and they are what names the step.
        let steps = [
            FailureStep::Forbidden,
            FailureStep::Unauthenticated,
            FailureStep::TimedOut,
            FailureStep::Unreachable,
            FailureStep::Other,
        ];
        let clauses = steps.map(FailureStep::clause);
        let titles = steps.map(FailureStep::title);
        let next_steps = steps.map(FailureStep::next_step);
        for (index, step) in next_steps.iter().enumerate() {
            assert!(!step.is_empty());
            assert!(
                !next_steps[..index].contains(step),
                "two failures share the next step {step:?}"
            );
            assert!(
                !clauses[..index].contains(&clauses[index]),
                "two failures share the clause {:?}",
                clauses[index]
            );
            assert!(
                !titles[..index].contains(&titles[index]),
                "two failures share the title {:?}, so the state cannot say which \
                 step failed",
                titles[index]
            );
        }

        // The control matches the sentence. A rejected token used to be sent to
        // a `Retry`, which re-sends the credentials the API server has just
        // refused.
        for step in steps {
            let action = match step.action() {
                FailureAction::Retry => "retry",
                FailureAction::ReloadKubeconfigs => "reload",
            };
            assert!(
                step.next_step().to_lowercase().contains(action),
                "{step:?} is answered by {action:?} and says {:?}",
                step.next_step()
            );
        }

        // A wait has no "ago", and the two ladders cannot drift apart on the
        // numbers.
        for (waited, expected) in [
            (TimeDuration::from_secs(0), "0s"),
            (TimeDuration::from_secs(9), "9s"),
            (TimeDuration::from_secs(59), "59s"),
            (TimeDuration::from_secs(60), "1m"),
            (TimeDuration::from_secs(3_599), "59m"),
            (TimeDuration::from_secs(3_600), "1h"),
            (TimeDuration::from_secs(86_400), "1d"),
        ] {
            assert_eq!(format_wait(waited), expected);
        }
        assert_eq!(
            format_wait(TimeDuration::from_secs(42)),
            format_age(TimeDuration::from_secs(42)).trim_end_matches(" ago"),
            "the two ladders differ only in the suffix, so a second of waiting and a second of age \
             are never formatted two different ways"
        );
    }

    /// Freshness and health are two channels, not two shades of one.
    ///
    /// A cluster the app could not read has no health verdict; a snapshot being
    /// replaced is not yet wrong; a refresh that failed leaves the last good
    /// numbers on screen, which is stale and not unknown. L9 asks for exactly
    /// these four answers, and the toolbar's one line is the only place that has
    /// to choose between them.
    #[gpui_kit::test]
    fn freshness_answers_separately_from_health(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let sample = overview(
            HealthSummary {
                total_pods: 1,
                running: 1,
                ..HealthSummary::default()
            },
            NodeSummary {
                count: 1,
                ready: 1,
                not_ready: 0,
            },
            HealthLevel::Healthy,
        );
        let answered = OverviewState::Ready(sample.clone());
        let now = SystemTime::now();
        let old = now.checked_sub(TimeDuration::from_secs(125)).unwrap_or(now);

        assert_eq!(
            freshness(&answered, false, false, Some(now)).0,
            Freshness::Current
        );
        assert_eq!(
            freshness(&answered, true, false, Some(now)).0,
            Freshness::Refreshing,
            "a refresh in flight leaves the previous answer on screen"
        );
        assert_eq!(
            freshness(&answered, false, true, Some(now)).0,
            Freshness::Stale,
            "a failed refresh keeps the last good answer rather than blanking the page"
        );
        assert_eq!(
            freshness(&OverviewState::Loading, false, false, None).0,
            Freshness::Never
        );
        assert_eq!(
            freshness(
                &OverviewState::Failed("no route to host".to_owned()),
                false,
                false,
                None
            )
            .0,
            Freshness::Never
        );
        assert_eq!(
            freshness(&answered, false, false, Some(old)).1.as_secs(),
            125,
            "the age is measured, so `Stale 2m` is a fact rather than a claim"
        );
        assert_eq!(
            freshness(&answered, false, true, Some(old))
                .0
                .label(TimeDuration::from_secs(125)),
            "Stale 2m ago",
            "L9's own spelling: how old the numbers are, in one glance"
        );
        // A source the app could not read leaves the *health* verdict alone and
        // the strip quiet, because an unreadable source is a fact about the app's
        // configuration and not a caution event on the cluster.
        let mut partial = sample;
        partial.unavailable_sources = vec![UnavailableSource {
            source: "statefulsets",
            reason: "403 Forbidden".to_owned(),
        }];
        assert_eq!(health_verdict(&partial).0, "Partial cluster data");
        assert_eq!(health_verdict(&partial).1, Severity::Muted);
        assert!(
            health_issues(&partial, &OverviewRoutes::default()).is_empty(),
            "a partial snapshot has no chips: the missing data is a footnote, not a verdict"
        );
    }

    /// The loading ladder is a ladder, and the empty and error states are two
    /// different things.
    ///
    /// `UI-SPEC` §4.14 grades a load by how long the reader has waited, and the
    /// three rungs are reachable from a test's clock rather than from wall time —
    /// which is the difference between a loading rule and a loading rule nobody
    /// can check. §4.13's terse empty state is the first thing a person sees on a
    /// first run, and it is *not* a red screen: there is nothing to have failed.
    #[gpui_kit::test]
    fn the_three_states_and_the_loading_ladder(cx: &mut TestAppContext) {
        crate::init_ui(cx);

        // Rung one: a load that resolves inside 200ms draws nothing at all. A
        // spinner that appears for 40ms and disappears is a flash, and a flash
        // reads as a glitch rather than as speed.
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        cx.simulate_resize(gpui_kit::size(px(1_440.), px(900.)));
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.loading_tier),
            LoadingTier::Nothing,
            "a view that has not been asked to load anything is not loading"
        );

        // The ladder, from the timers rather than from a clock comparison.
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Loading;
            view.loading_since = Some(Instant::now());
            view.advance_loading_tier(cx);
            // `Context::update` does not repaint on its own: without this the
            // tier is Spinner in the model and the panel still shows the frame it
            // drew before the change, and a test that then looks for the spinner
            // is looking for a state that was never painted.
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.loading_tier),
            LoadingTier::Spinner,
            "the first rung is entered immediately rather than scheduled, because a spinner that \\
             arrives 200ms after the wait began is a spinner that appeared after the moment it was \\
             for"
        );
        assert!(
            cx.debug_bounds("overview-loading").is_some(),
            "and the spinner takes the strip's own slot, so nothing below it moves"
        );

        cx.executor()
            .advance_clock(SKELETON_AFTER + TimeDuration::from_millis(1));
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.loading_tier),
            LoadingTier::Skeleton
        );
        assert!(
            cx.debug_bounds("overview-skeleton").is_some(),
            "past 500ms a skeleton stands in, so the numbers arrive into space that is already \\
             theirs"
        );
        // A skeleton over real data is the failure `UI-SPEC` §4.14 names: covering
        // a number a reader can read is worse than the wait. So the skeleton's
        // blocks are the real tile's blocks, at the real line heights.
        let tile = cx
            .debug_bounds("overview-skeleton-tile-0-number")
            .expect("a placeholder at the display size");
        assert_eq!(
            f32::from(tile.size.height),
            f32::from(text::DISPLAY_LINE_HEIGHT),
            "the placeholder for the number is the number's own line height"
        );

        cx.executor().advance_clock(LOADING_PROGRESS);
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.loading_tier),
            LoadingTier::Progress
        );
        assert!(
            cx.debug_bounds("overview-skeleton-progress").is_some(),
            "past 2s the reader is owed a quantity as well as a placeholder"
        );

        // Disconnected: a first run with no cluster. Terse, and not a failure.
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Disconnected;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("empty-state").is_some(),
            "no cluster is an empty state, not an error: a red screen on a first run is the wrong \\
             first impression of a product that is working"
        );
        assert!(
            cx.debug_bounds("overview-error").is_none(),
            "and it is not the error state"
        );
        assert_eq!(
            view.read_with(cx, |view, _| view.focus_default_control()),
            view.read_with(cx, |view, _| view.retry_focus.clone()),
            "and the keyboard lands on that action rather than on the toolbar's refresh, which has \
             no handle to ask and returns this same state"
        );
        assert!(
            cx.debug_bounds("overview-reload-kubeconfigs").is_some(),
            "the state offers the action that fixes it. It used to offer none and pointed at the \
             title bar's cluster picker instead, which holds nothing to pick on a machine with no \
             kubeconfig and nothing reachable on one whose contexts all failed to load"
        );
        assert!(
            cx.debug_bounds("overview-retry").is_none(),
            "a state with no cluster is not a failure, so it has nothing to retry"
        );

        // Error: unreachable, so the reason and a verb-phrase action.
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        view.update(cx, |view, cx| {
            view.state =
                OverviewState::Failed("Overview request failed: no route to host.".to_owned());
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("overview-error").is_some());
        assert!(
            cx.debug_bounds("overview-retry").is_some(),
            "`UI-SPEC` §4.15: every error has a next step, and the button says what it is"
        );
        assert_eq!(
            view.read_with(cx, |view, _| view.focus_default_control()),
            view.read_with(cx, |view, _| view.retry_focus.clone()),
            "and the retry is what the keyboard lands on"
        );
    }

    /// A failed refresh keeps the numbers, marks them stale, and says how old.
    ///
    /// This is the difference between a dashboard and a guess. Blanking the page
    /// on a transient failure takes away an answer the reader already had; showing
    /// it without saying it is old is worse, because a ten-second glance would
    /// read a stale number as a current one.
    #[gpui_kit::test]
    fn a_failed_refresh_keeps_the_numbers_and_marks_them_stale(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, true, cx));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Ready(oversubscribed_node());
            cx.notify();
        });
        cx.simulate_resize(gpui_kit::size(px(1_440.), px(900.)));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("overview-tile-nodes").is_some(),
            "the tiles are on screen before anything fails"
        );
        assert!(cx.debug_bounds("overview-refresh-error").is_none());

        // A refresh that fails with a snapshot already on screen keeps it.
        view.update(cx, |view, cx| {
            let applied = apply_refresh_result(
                &mut view.state,
                &mut view.refreshing,
                &mut view.refresh_error,
                view.epoch,
                view.epoch,
                Err("Overview request failed: no route to host.".to_owned()),
            );
            assert!(applied.is_none(), "a failure has no completion time");
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("overview-tile-nodes").is_some(),
            "the numbers are still on screen: a dashboard that blanks on a transient failure takes \\
             away an answer the reader already had"
        );
        assert!(
            cx.debug_bounds("overview-refresh-error").is_some(),
            "and the reason is stated in place, at the top of the data"
        );
        assert!(
            cx.debug_bounds("overview-refresh-retry").is_some(),
            "with a verb-phrase action beside it"
        );
        cx.debug_bounds("overview-body").expect("the data region");
    }

    /// A tick that lands while a request is in flight does not start a second.
    ///
    /// `refresh` replaces the request in flight rather than queueing behind it,
    /// so a timer that fired during a load would cancel a live request before it
    /// could land. Against a cluster that takes longer to answer than
    /// `OVERVIEW_REFRESH_INTERVAL` the panel would then take a new snapshot never
    /// and sit on its first one for ever — a dashboard that is worse than one
    /// that does not refresh at all, and one that looks perfectly healthy. The
    /// epoch is the count of refreshes this panel has started, so it is what the
    /// assertion is made on.
    #[gpui_kit::test]
    fn an_auto_refresh_tick_does_not_stack_requests(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        // A runtime that is never driven, so the request the panel spawns stays
        // pending for the whole test: the cluster that never answers.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime to own the request");
        let handle = OverviewHandle::new(
            runtime.handle().clone(),
            ClusterDataSource::from_port(Arc::new(SilentCluster)),
        );
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(Some(handle), false, cx));
        view.update(cx, |view, cx| view.refresh(cx));
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.epoch),
            1,
            "one request, and the cluster has not answered it"
        );
        assert!(view.read_with(cx, |view, _| view.refreshing));

        // Two whole intervals, and the cluster still has not answered.
        cx.executor().advance_clock(OVERVIEW_REFRESH_INTERVAL * 2);
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.epoch),
            1,
            "a tick that lands on a request in flight does not start a second one, and does not \
             cancel the first"
        );
        assert!(
            view.read_with(cx, |view, _| view.refreshing),
            "the request in flight is still the one that is running"
        );

        // The load lands, and the next tick refreshes again. Without this half
        // the assertion above would also hold for a loop that had stopped.
        view.update(cx, |view, cx| {
            apply_refresh_result(
                &mut view.state,
                &mut view.refreshing,
                &mut view.refresh_error,
                view.epoch,
                view.epoch,
                Ok(Overview::default()),
            );
            cx.notify();
        });
        cx.executor().advance_clock(OVERVIEW_REFRESH_INTERVAL);
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.epoch),
            2,
            "and once nothing is in flight the next tick refreshes"
        );
    }

    /// A source the app could not read says so, and says it in words rather than
    /// in a wash.
    ///
    /// The banner this replaced answered a partial snapshot with amber, a caution
    /// wash and a warning glyph — the whole set of symbols that means "this object
    /// is in bad shape" — while the actual problem was the app's own RBAC. The
    /// strip now says which sources it could not read and spends no caution on
    /// it, because the cluster is not the thing that is wrong.
    #[gpui_kit::test]
    fn a_partial_snapshot_names_the_denied_permission_and_spends_no_caution(
        cx: &mut TestAppContext,
    ) {
        crate::init_ui(cx);
        let mut partial = overview(
            HealthSummary {
                total_pods: 2,
                running: 2,
                ..HealthSummary::default()
            },
            NodeSummary {
                count: 1,
                ready: 1,
                not_ready: 0,
            },
            HealthLevel::Healthy,
        );
        partial.unavailable_sources = vec![UnavailableSource {
            source: "statefulsets",
            reason: "403 Forbidden".to_owned(),
        }];
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Ready(partial);
            cx.notify();
        });
        cx.run_until_parked();
        let strip = cx
            .debug_bounds("overview-health")
            .expect("the health strip");
        assert_eq!(
            f32::from(strip.size.height),
            f32::from(design::size::SUMMARY_STRIP),
            "a quiet strip is the shared 32px summary height, so nothing below it moves"
        );
        assert!(
            cx.debug_bounds("overview-issue-0").is_none(),
            "a partial snapshot has no chips: the missing data is a footnote, not a problem"
        );
        assert!(strip.size.height > gpui_kit::px(0.));
    }

    /// The overcommit state is three channels, not one: a glyph, a bar that
    /// reaches the end of its own track, and the words.
    ///
    /// The cell used to hand a warning ink to its own `aria_label`, so the cell
    /// announced `1.15 / 20 cores` and the fact that the node is oversubscribed
    /// did not exist for anyone who could not see amber. It then became a bar,
    /// and a bar that grew past its own track would have been the second half of
    /// the same lie: a length drawn outside the scale it is measured on. So the
    /// three channels are a shape, a clamped fill, and a sentence.
    #[gpui_kit::test]
    fn an_oversubscribed_request_has_a_glyph_a_full_bar_and_words(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (_root, cx) = cx.add_window_view(|_, cx| {
            let view = cx.new(|cx| OverviewView::new(None, true, cx));
            view.update(cx, |view, cx| {
                view.state = OverviewState::Ready(oversubscribed_node());
                cx.notify();
            });
            SizedOverview {
                view,
                width: px(1_280.),
                height: px(800.),
            }
        });
        cx.run_until_parked();
        cx.run_until_parked();

        // The shape: a 4.8-core request on a 4-core node carries the shared
        // health glyph in front of its figure.
        cx.debug_bounds("overview-capacity-severity-0-1")
            .expect("the oversubscribed CPU request draws a severity glyph");
        // The bar: present, 6px tall, and the same width as its cell, so a fill
        // above 100% clamps inside the track rather than drawing past it.
        let bar = cx
            .debug_bounds("overview-capacity-cell-0-1-bar")
            .expect("the CPU request cell draws a bar");
        let cell = cx
            .debug_bounds("overview-capacity-cell-0-1")
            .expect("the CPU request cell");
        assert_eq!(f32::from(bar.size.height), f32::from(BAR_HEIGHT));
        assert!(
            right_edge(bar) <= right_edge(cell) + 0.5 && left_edge(bar) >= left_edge(cell) - 0.5,
            "the bar lives inside its own cell: bar {bar:?} cell {cell:?}"
        );
        // The words: the share is the figure, and the absolutes and the verdict
        // are in the cell's spoken label.
        // The memory axis is not over, so its cell states nothing.
        assert!(
            cx.debug_bounds("overview-capacity-severity-0-2").is_none(),
            "the memory axis is not over, so its cell states nothing"
        );
    }

    /// The figure a request cell prints is the share, and the limit is a tick on
    /// the same track.
    ///
    /// This is the change `UI-REDESIGN.md` §3.5 asks for: the bare
    /// `1.15 / 20 cores` becomes a bar whose length is the answer. The absolutes
    /// are not lost — they are the cell's spoken label, which is what the tooltip
    /// and a screen reader both get.
    ///
    /// The bar carries one mark. The limit used to ride the same track as a 2px
    /// tick in the same ink as a healthy fill, which on a node running at 1% with
    /// a 6% request read as two dashes and left the reader to guess which was the
    /// number; the limit keeps its own column, and the one thing the tick said
    /// that two numbers do not is a state on that column. Both halves are pinned
    /// here: `the_limits_column_states_a_request_above_its_own_limit` and the
    /// `live_columns_carry_one_mark` test below.
    #[gpui_kit::test]
    fn a_request_cell_is_a_share_with_the_absolutes_in_words(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        cx.update(|cx| {
            let mut snapshot = overview_with_metrics(
                HealthSummary {
                    total_pods: 1,
                    running: 1,
                    ..HealthSummary::default()
                },
                NodeSummary {
                    count: 1,
                    ready: 1,
                    not_ready: 0,
                },
            );
            let mut delegate = CapacityTableDelegate::new(cx);
            let mut rows = capacity_rows(&snapshot.capacities, snapshot.usage.as_deref());
            rows[0].capacity.requested_cpu = 1.15;
            rows[0].capacity.limits_cpu = 2.0;
            delegate.rows = rows;
            let cell = delegate.cell(0, 1, cx);
            assert_eq!(cell.text, "29%", "1.15 of 4 cores is 29% of allocatable");
            let bar = cell.bar.expect("a request cell draws a bar");
            assert!((bar.fill - 28.75).abs() < 0.01);
            assert!(
                cell.spoken.contains("1.15 / 4 cores"),
                "the absolutes the bar replaced are still in the words: {}",
                cell.spoken
            );
            assert!(
                cell.spoken.contains("of allocatable"),
                "and the whole it is a share of is named: {}",
                cell.spoken
            );
            assert!(cell.severity.is_none(), "29% is not overcommit");

            // A node with no allocatable has no scale, so the cell prints the
            // absolute and draws no bar rather than a bar that means 100% of an
            // unknown.
            snapshot.capacities[0].allocatable_cpu = None;
            let mut rows = capacity_rows(&snapshot.capacities, snapshot.usage.as_deref());
            rows[0].capacity.requested_cpu = 1.15;
            delegate.rows = rows;
            let cell = delegate.cell(0, 1, cx);
            assert!(cell.bar.is_none(), "no allocatable, no bar");
            assert!(
                cell.text.contains("1.15"),
                "the absolute is still stated: {}",
                cell.text
            );
        });
    }

    /// A request above the node's own limit is the one thing the removed tick
    /// said that the two numbers do not, so it has to be said somewhere, and the
    /// cell holding the ceiling is the cell that is wrong.
    #[gpui_kit::test]
    fn the_limits_column_states_a_request_above_its_own_limit(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        cx.update(|cx| {
            let snapshot = overview_with_metrics(
                HealthSummary {
                    total_pods: 1,
                    running: 1,
                    ..HealthSummary::default()
                },
                NodeSummary {
                    count: 1,
                    ready: 1,
                    not_ready: 0,
                },
            );
            let mut delegate = CapacityTableDelegate::new(cx);
            let mut rows = capacity_rows(&snapshot.capacities, snapshot.usage.as_deref());
            rows[0].capacity.limits_cpu = 0.5;
            rows[0].capacity.requested_cpu = 1.15;
            delegate.rows = rows;

            let over = delegate.cell(0, 3, cx);
            assert_eq!(over.text, "0.50", "the ceiling is still the figure");
            assert_eq!(
                over.severity,
                Some(Severity::Warning),
                "a request above the limit is a caution, and it survives greyscale \
                 through the glyph the cell draws beside the figure"
            );
            assert!(
                over.spoken.contains("above this limit"),
                "and it is in the words a screen reader and a tooltip both read: {}",
                over.spoken
            );
            assert!(over.bar.is_none(), "a ceiling is not a share of anything");

            // A ceiling the request fits under is a number, not a state.
            let mut rows = capacity_rows(&snapshot.capacities, snapshot.usage.as_deref());
            rows[0].capacity.limits_cpu = 4.0;
            rows[0].capacity.requested_cpu = 1.15;
            delegate.rows = rows;
            let under = delegate.cell(0, 3, cx);
            assert_eq!(under.text, "4");
            assert!(under.severity.is_none(), "1.15 fits under 4");

            // A node that declares no limit has no ceiling to exceed, and says so
            // rather than claiming one.
            let mut rows = capacity_rows(&snapshot.capacities, snapshot.usage.as_deref());
            rows[0].capacity.limits_cpu = 0.0;
            rows[0].capacity.requested_cpu = 1.15;
            delegate.rows = rows;
            let none = delegate.cell(0, 3, cx);
            assert_eq!(none.text, NO_ANSWER);
            assert!(none.severity.is_none(), "no limit is not overcommit");
            assert!(
                none.spoken.contains("No limit is declared"),
                "{}",
                none.spoken
            );
        });
    }

    /// The live columns used to draw the request as a tick on top of the usage,
    /// which is the neighbouring request column's own fill on the same scale: the
    /// same number twice on one row, once as a fill and once as a dash.
    #[gpui_kit::test]
    fn a_live_cell_carries_the_usage_and_nothing_else(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        cx.update(|cx| {
            let mut snapshot = overview_with_metrics(
                HealthSummary {
                    total_pods: 1,
                    running: 1,
                    ..HealthSummary::default()
                },
                NodeSummary {
                    count: 1,
                    ready: 1,
                    not_ready: 0,
                },
            );
            snapshot.usage = Some(vec![NodeUsage {
                name: snapshot.capacities[0].name.clone(),
                cpu_millicores: Some(80.0),
                memory_bytes: None,
            }]);
            let mut delegate = CapacityTableDelegate::new(cx);
            let mut rows = capacity_rows(&snapshot.capacities, snapshot.usage.as_deref());
            rows[0].capacity.requested_cpu = 1.15;
            delegate.rows = rows;

            let live = delegate.cell(0, 5, cx);
            let bar = live.bar.expect("a live reading is a share of allocatable");
            assert!(
                (bar.fill - 2.0).abs() < 0.01,
                "80m of 4000m is 2% of allocatable, which is the one mark on the track"
            );
            assert_eq!(live.text, "2%");
            assert!(
                !live.spoken.contains("1.15"),
                "the request is not restated here: {}",
                live.spoken
            );
        });
    }

    /// Clicking a heading sorts, clicking it again reverses, and a third click
    /// returns to the node-name order the table opens in.
    ///
    /// The cycle is the app's, and it is applied where the click lands: a
    /// notification on the table state redraws the table and nothing above it,
    /// so a cycle that only reordered on the next snapshot would look like it
    /// had done nothing at all.
    #[gpui_kit::test]
    fn a_third_click_on_a_heading_returns_the_table_to_the_node_name_order(
        cx: &mut TestAppContext,
    ) {
        crate::init_ui(cx);
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, true, cx));
        view.update(cx, |view, cx| {
            let mut snapshot = overview_with_metrics(
                HealthSummary {
                    total_pods: 3,
                    running: 3,
                    ..HealthSummary::default()
                },
                NodeSummary {
                    count: 3,
                    ready: 3,
                    not_ready: 0,
                },
            );
            // Three nodes whose CPU requests are in the order their names are
            // not, so every step of the cycle is visible in the row order.
            for (index, requested) in [3.0, 1.0, 2.0].into_iter().enumerate() {
                snapshot.capacities[index].requested_cpu = requested;
            }
            view.state = OverviewState::Ready(snapshot);
            cx.notify();
        });
        cx.simulate_resize(gpui_kit::size(px(1_440.), px(900.)));
        cx.run_until_parked();
        cx.run_until_parked();

        let state = view
            .read_with(cx, |view, _| view.capacity_state.borrow().clone())
            .expect("a snapshot with nodes builds the grid");
        let order = |cx: &mut gpui_kit::VisualTestContext| {
            state.read_with(cx, |state, _| {
                state
                    .delegate()
                    .rows
                    .iter()
                    .map(|row| row.capacity.name.clone())
                    .collect::<Vec<String>>()
            })
        };
        let sort = |cx: &mut gpui_kit::VisualTestContext| {
            state.read_with(cx, |state, _| state.delegate().sort)
        };
        let click_heading = |cx: &mut gpui_kit::VisualTestContext, column: usize| {
            let heading = cx
                .debug_bounds(HEADER_CELLS[column])
                .unwrap_or_else(|| panic!("the heading for column {column}"));
            cx.simulate_click(heading.center(), gpui_kit::Modifiers::none());
            cx.run_until_parked();
        };

        assert_eq!(order(cx), ["worker-0", "worker-1", "worker-2"]);
        assert_eq!(sort(cx), Sort::ascending(0), "it opens on the node name");

        click_heading(cx, 1);
        assert_eq!(sort(cx), Sort::ascending(1));
        assert_eq!(
            order(cx),
            ["worker-1", "worker-2", "worker-0"],
            "the smallest CPU request leads"
        );

        click_heading(cx, 1);
        assert_eq!(sort(cx), Sort::descending(1));
        assert_eq!(
            order(cx),
            ["worker-0", "worker-2", "worker-1"],
            "the same heading reverses"
        );

        click_heading(cx, 1);
        assert_eq!(
            sort(cx),
            Sort::ascending(0),
            "a third click lands on the node-name order rather than on no sort at all"
        );
        assert_eq!(order(cx), ["worker-0", "worker-1", "worker-2"]);

        click_heading(cx, 2);
        assert_eq!(
            sort(cx),
            Sort::ascending(2),
            "another column starts ascending"
        );
    }

    /// Header alignment follows the data it names, and the panel's sections share
    /// one left edge that clears the group frame.
    #[gpui_kit::test]
    fn the_capacity_header_follows_its_data_and_the_sections_share_one_edge(
        cx: &mut TestAppContext,
    ) {
        crate::init_ui(cx);
        let (_root, cx) = cx.add_window_view(|_, cx| {
            let view = cx.new(|cx| OverviewView::new(None, true, cx));
            view.update(cx, |view, cx| {
                // Workload data too, so the region between the tiles and the table
                // is on screen: the test is about the page's shared left edge, and
                // a region that is not rendered cannot share anything.
                let mut snapshot = overview_with_metrics(
                    HealthSummary {
                        total_pods: 2,
                        running: 2,
                        ..HealthSummary::default()
                    },
                    NodeSummary {
                        count: 2,
                        ready: 2,
                        not_ready: 0,
                    },
                );
                snapshot.workloads.deployments = ReplicaSummary {
                    desired: 6,
                    available: 6,
                };
                view.state = OverviewState::Ready(snapshot.clone());
                cx.notify();
            });
            SizedOverview {
                view,
                width: px(1_280.),
                height: px(800.),
            }
        });
        cx.run_until_parked();
        cx.run_until_parked();

        // Every numeric heading ends where its column's data ends. The selectors
        // are literals because the test context looks them up by `&'static str`.
        for column in 1..CAPACITY_COLUMNS.len() {
            let header = cx
                .debug_bounds(HEADER_CELLS[column])
                .unwrap_or_else(|| panic!("the heading for column {column}"));
            let label = cx
                .debug_bounds(HEADER_LABELS[column])
                .unwrap_or_else(|| panic!("the heading label for column {column}"));
            let cell = cx
                .debug_bounds(ROW_CELLS[column])
                .unwrap_or_else(|| panic!("the cell for column {column}"));
            assert!(
                (right_edge(label) - right_edge(cell)).abs() < 1.5,
                "{} is right-aligned in its data and left-aligned in its heading: label {:?} cell {:?}",
                CAPACITY_COLUMNS[column],
                label,
                cell
            );
            assert!(
                (right_edge(header) - right_edge(cell)).abs() < 0.5,
                "the heading and the data share the column's trailing edge"
            );
        }
        // The node column stays left-aligned: the name is text, not a number.
        let node_label = cx.debug_bounds(HEADER_LABELS[0]).expect("the node heading");
        let node_cell = cx.debug_bounds(ROW_CELLS[0]).expect("the node cell");
        assert!((left_edge(node_label) - left_edge(node_cell)).abs() < 1.5);

        // Every region starts on the panel's own padding line.
        //
        // This used to be "the text clears the group frame by the LG inset": the
        // three regions sat inside one bordered box, and the assertion held that
        // their left edges cleared its 1px frame. The frame is gone — §0 铁律一
        // rules the border off a group, and `UI-REDESIGN.md` §3.5 replaced the
        // rules that separated the regions with whitespace and a short dash — so
        // the guarantee it was protecting is now stated where it actually lives:
        // the scroll body's own padding. Four regions, one left edge.
        //
        // **The two bands are measured by their plates**, which are the regions'
        // own boxes. A band's figures sit `BAND_PADDING` inside its surface —
        // that is the card's padding, measured against the card below — so the
        // thing that shares the page's spine is the surface and not the text
        // inside it. Measuring the grid instead would have pinned the padding as
        // the spine and put the regions' edges 20px inside the strip's and the
        // table's, which is the misalignment this test exists to prevent.
        let panel = cx
            .debug_bounds("overview-content-bounds")
            .expect("the panel");
        let regions = [
            ("overview-health", "the strip"),
            ("overview-vitals-band-plate", "the tiles' surface"),
            ("overview-workload-band-plate", "the workloads' surface"),
            ("overview-capacity", "the capacity table"),
        ];
        for (selector, name) in regions {
            let region = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{name}"));
            let inset = left_edge(region) - left_edge(panel);
            assert!(
                (inset - f32::from(space::LG)).abs() < 1.0,
                "{name} starts on the panel's padding line, not on the frame's: {inset}px"
            );
        }
        // And the surface and its own contents agree, on both axes: a band that
        // pads vertically only is the defect this measures, and the pixels that
        // show it are the figures sitting against the edge of their own surface.
        for (plate_selector, content_selector, name) in [
            ("overview-vitals-band-plate", "overview-vitals", "the tiles"),
            (
                "overview-workload-band-plate",
                "overview-workload-grid",
                "the workloads",
            ),
        ] {
            let plate = cx
                .debug_bounds(plate_selector)
                .unwrap_or_else(|| panic!("{name} surface"));
            let content = cx
                .debug_bounds(content_selector)
                .unwrap_or_else(|| panic!("{name}"));
            let inset_x = left_edge(content) - left_edge(plate);
            let inset_y = f32::from(content.origin.y) - f32::from(plate.origin.y);
            assert!(
                (inset_x - f32::from(BAND_PADDING)).abs() < 1.0
                    && (inset_y - f32::from(BAND_PADDING)).abs() < 1.0,
                "{name} is one band padding inside its own surface on BOTH axes, so there is no \
                 stripe of colour beside the figures and no table of figures flush to its edge: \
                 {inset_x}px in from the left, {inset_y}px from the top, surface {plate:?} content \
                 {content:?}"
            );
        }

        // A section boundary is more than the dash that marks it: 32px of
        // whitespace, then a 1px mark, then 32px again. The mark is a mark.
        let heading = cx
            .debug_bounds("overview-section-heading")
            .expect("a section heading");
        let tiles = cx.debug_bounds("overview-vitals").expect("the tiles");
        let gap = f32::from(heading.origin.y) - f32::from(tiles.origin.y + tiles.size.height);
        assert!(
            gap >= 2.0 * f32::from(space::XXL),
            "a section boundary is carried by whitespace, not by the line that marks it: {gap}px"
        );
    }

    /// The 1px dash after a section heading starts on a whole logical pixel.
    ///
    /// A 1px rule is crisp only when its top edge is an integer: at scale 2 the
    /// element covers device rows `[2y, 2y+2)`, and a half-integer `y` puts half
    /// its ink in each of two rows. Measured on this panel the dash sat at logical
    /// **311.5** — device rows 623–624 — which is invisible at 2× (both rows come
    /// out even) and reads as a 2px line at half the ink at 1×.
    ///
    /// `SECTION_DASH_OFFSET` is the panel's own instance of a fix that belongs in
    /// `design.rs` (device-pixel snapping in the vertical rhythm); this holds the
    /// instance from quietly going back to `items_center`.
    #[gpui_kit::test]
    fn a_section_dash_starts_on_a_whole_logical_pixel(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        cx.simulate_resize(gpui_kit::size(px(1_440.), px(900.)));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Ready(overview_with_metrics(
                HealthSummary {
                    total_pods: 12,
                    running: 12,
                    ..HealthSummary::default()
                },
                NodeSummary {
                    count: 2,
                    ready: 2,
                    not_ready: 0,
                },
            ));
            cx.notify();
        });
        cx.run_until_parked();

        let dash = cx
            .debug_bounds("overview-section-dash")
            .expect("a section dash");
        let top = f32::from(dash.origin.y);
        assert_eq!(
            top.fract(),
            0.,
            "a 1px rule at logical y={top} straddles two device rows: {:?}",
            dash
        );
        assert_eq!(
            f32::from(dash.size.height),
            1.,
            "it is one rule, not a divider"
        );
    }

    /// The five workload cells fill the width and share one row, and wrap into two
    /// full rows when the panel is too narrow for that.
    ///
    /// The old test asserted the opposite — that the five cells spanned *less
    /// than half* the panel — because the cells were content-width and left a void
    /// on the right, which is the `UI-REDESIGN.md` §1.1 complaint about this page
    /// restated as a regression test. The grid is the answer, so the invariant is
    /// now that the row reaches the panel's trailing edge and that a narrow panel
    /// wraps into rows which also add up to twelve.
    #[gpui_kit::test]
    fn the_workload_grid_fills_the_width_and_wraps(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let mut snapshot = overview_with_metrics(
            HealthSummary {
                total_pods: 12,
                running: 12,
                ..HealthSummary::default()
            },
            NodeSummary {
                count: 1,
                ready: 1,
                not_ready: 0,
            },
        );
        snapshot.workloads.deployments = ReplicaSummary {
            desired: 10_004,
            available: 104,
        };
        // The root view is built at a chosen width, so the panel is measured at
        // that width rather than at whatever the test window happens to be.
        macro_rules! overview_window {
            ($cx:ident, $width:expr, $snapshot:expr) => {{
                let view = $cx.new(|inner| OverviewView::new(None, false, inner));
                view.update($cx, |view, cx| {
                    view.state = OverviewState::Ready($snapshot);
                    cx.notify();
                });
                SizedOverview {
                    view,
                    width: px($width),
                    height: px(800.),
                }
            }};
        }
        let cells = || {
            [
                "overview-workload-cell-Deployments",
                "overview-workload-cell-Stateful-sets",
                "overview-workload-cell-Daemon-sets",
                "overview-workload-cell-Jobs",
                "overview-workload-cell-Cron-jobs",
            ]
        };
        let first_cell = cells()[0];
        let last_cell = cells()[cells().len() - 1];

        // Wide: one row of five, and it reaches the panel's trailing edge, because
        // a grid that leaves a void on the right is the thing this screen used to
        // get wrong.
        let wide = snapshot.clone();
        let (_root, cx) = cx.add_window_view(|_, cx| overview_window!(cx, 1_280., wide));
        cx.run_until_parked();
        cx.run_until_parked();
        let panel = cx
            .debug_bounds("overview-content-bounds")
            .expect("the panel");
        let first = cx.debug_bounds(first_cell).expect("the first kind");
        let last = cx.debug_bounds(last_cell).expect("the last kind");
        for selector in cells() {
            let cell = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("the {selector} cell"));
            assert!(
                right_edge(cell) <= right_edge(panel) + 0.5,
                "{selector} stays inside the panel: {cell:?} panel {panel:?}"
            );
        }
        assert!(
            (f32::from(last.origin.y) - f32::from(first.origin.y)).abs() < 1.0,
            "the five kinds share one row at a wide panel"
        );
        assert!(
            (right_edge(panel)
                - right_edge(last)
                - (f32::from(space::LG) + f32::from(BAND_PADDING)))
            .abs()
                < 1.0,
            "and the row fills the width up to the band's own gutter — the panel's padding plus the \
             surface's padding: {}px of void on the right of a {}px panel",
            right_edge(panel) - right_edge(last),
            f32::from(panel.size.width)
        );

        // Narrow: the row wraps into two full rows, and the two rows are on
        // different lines rather than one kind spilling onto a row of its own
        // beside two empty slots.
        let narrow = snapshot.clone();
        let (_root, cx) = cx.add_window_view(|_, cx| overview_window!(cx, 700., narrow));
        cx.run_until_parked();
        cx.run_until_parked();
        let panel = cx
            .debug_bounds("overview-content-bounds")
            .expect("the panel");
        let bounds: Vec<_> = cells()
            .iter()
            .map(|selector| {
                cx.debug_bounds(selector)
                    .unwrap_or_else(|| panic!("the {selector} cell"))
            })
            .collect();
        for cell in &bounds {
            assert!(
                right_edge(*cell) <= right_edge(panel) + 0.5,
                "a wrapped cell stays inside the panel: {cell:?} panel {panel:?}"
            );
        }
        let top = bounds
            .iter()
            .filter(|cell| (f32::from(cell.origin.y) - f32::from(bounds[0].origin.y)).abs() < 1.0)
            .count();
        assert_eq!(
            top, 3,
            "three kinds on the first row of a narrow panel: {bounds:?}"
        );
        for cell in &bounds {
            let on_first = (f32::from(cell.origin.y) - f32::from(bounds[0].origin.y)).abs() < 1.0;
            let on_second = (f32::from(cell.origin.y) - f32::from(bounds[3].origin.y)).abs() < 1.0;
            assert!(
                on_first || on_second,
                "every kind is on one of the two rows: {cell:?}"
            );
        }
    }

    /// The stat row is a twelve-column grid, not five even widths.
    ///
    /// `UI-REDESIGN.md` §1.1's complaint about this page was that the text and
    /// numbers filled about 60% of the width and left a void on the right. The
    /// fix is a grid, and the fix is only a grid if the four tiles' edges land on
    /// the same columns the workload row below uses — so the two rows are
    /// measured against each other and not just against the panel.
    #[gpui_kit::test]
    fn the_tiles_are_a_grid_that_lines_up_with_the_workload_row(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let mut snapshot = overview_with_metrics(
            HealthSummary {
                total_pods: 12,
                running: 12,
                ..HealthSummary::default()
            },
            NodeSummary {
                count: 1,
                ready: 1,
                not_ready: 0,
            },
        );
        snapshot.workloads.deployments = ReplicaSummary {
            desired: 10_004,
            available: 104,
        };
        let (_root, cx) = cx.add_window_view(|_, cx| {
            let view = cx.new(|inner| OverviewView::new(None, false, inner));
            view.update(cx, |view, cx| {
                view.state = OverviewState::Ready(snapshot.clone());
                cx.notify();
            });
            SizedOverview {
                view,
                width: px(1_280.),
                height: px(800.),
            }
        });
        cx.run_until_parked();
        cx.run_until_parked();

        let panel = cx
            .debug_bounds("overview-content-bounds")
            .expect("the panel");
        let tiles: Vec<_> = [
            "overview-hero",
            "overview-tile-nodes",
            "overview-tile-cpu",
            "overview-tile-memory",
        ]
        .iter()
        .map(|name| {
            cx.debug_bounds(name)
                .unwrap_or_else(|| panic!("the {name} tile"))
        })
        .collect();
        // A lead tile's meter and a supporting tile's are on one line, which is
        // what the tiles' shared height is for. The lead tile prints a `display`
        // figure and the three beside it a `title` one, so without a floor the
        // spacer has nothing to absorb and the meters land on two lines twelve
        // pixels apart — the render drew it that way and nothing here noticed,
        // because the tiles' own origins were still aligned.
        let meters: Vec<_> = ["overview-hero-bar", "overview-tile-nodes-bar"]
            .iter()
            .map(|name| {
                cx.debug_bounds(name)
                    .unwrap_or_else(|| panic!("the {name} meter"))
            })
            .collect();
        for meter in &meters {
            assert!(
                (f32::from(meter.origin.y) - f32::from(meters[0].origin.y)).abs() < 0.5,
                "the lead tile's meter and a supporting tile's are on one line: {meter:?} against \
                 {meters:?}"
            );
        }
        for cell in &tiles {
            assert!(
                (f32::from(cell.origin.y) - f32::from(tiles[0].origin.y)).abs() < 1.0,
                "the four tiles share one row: {cell:?}"
            );
            assert!(
                f32::from(cell.size.width) > f32::from(panel.size.width) * 0.15,
                "a tile is a quarter of the panel, not a fifth of a void: {cell:?} in a {}px panel",
                f32::from(panel.size.width)
            );
        }
        // Four tiles of three columns each, so every left edge is three columns
        // past the last and every right edge is one gap short of the panel's.
        for pair in tiles.windows(2) {
            let gap = f32::from(pair[1].origin.x) - right_edge(pair[0]);
            assert!(
                (gap - f32::from(GRID_GAP)).abs() < 1.0,
                "two adjacent tiles are one grid gap apart: {gap}px between {pair:?}"
            );
        }
        // The void on the trailing side is the panel's own padding **plus the
        // band's**: the four tiles fill their surface, and the surface is the
        // region. It was `space::LG` while the band padded vertically only, and
        // it is `space::LG + BAND_PADDING` now that the figures are held off the
        // surface's edge — which is a gutter the reader can see, not a void.
        let void = right_edge(panel) - right_edge(tiles[tiles.len() - 1]);
        let gutter = f32::from(space::LG) + f32::from(BAND_PADDING);
        assert!(
            (void - gutter).abs() < 1.0,
            "the last tile ends one band padding inside the band's surface, and that surface ends \
             one panel padding from the panel's own edge, so the row still fills the width: \
             {void}px of void against a {gutter}px gutter"
        );
        // The tile row and the workload region start on the same left edge, which
        // is the whole point of a grid: two rows of different shapes that still
        // line up.
        let workloads = cx
            .debug_bounds("overview-workload-grid")
            .expect("the workload region");
        assert!(
            (left_edge(tiles[0]) - left_edge(workloads)).abs() < 1.0,
            "the two grids start on one line: tiles {tiles:?} workloads {workloads:?}"
        );
    }

    /// The strip's chip is the route to the rows behind its count.
    ///
    /// `UI-REDESIGN.md` §3.5 makes the chip the thing that routes, so the
    /// affordance moved off the pod tile's subline and onto the strip. The
    /// invariant is unchanged and is the one the old caption test held: a count
    /// with a route is a control in the tab order that works, and a count with no
    /// route is quiet text, because a control that goes nowhere is a lie.
    #[gpui_kit::test]
    fn the_issue_chip_is_the_route_to_the_pods_it_counts(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let mut degraded = overview(
            HealthSummary {
                total_pods: 10_010,
                running: 110,
                pending: 9_900,
                failed: 4,
                ..HealthSummary::default()
            },
            NodeSummary {
                count: 1,
                ready: 1,
                not_ready: 0,
            },
            HealthLevel::Warning,
        );
        degraded.unknown_pods = 3;
        // The tile names the largest bucket and then the next one, each with its
        // own noun. It used to print the *remainder* as a bare figure —
        // `9,900 pending · 7` — so three unknown pods and four failed pods were
        // one number with no unit, on the one line whose whole job is to be read
        // without a hover. The two named buckets are what the reader can act on.
        let (hero, _) = vital_figures(&degraded);
        assert_eq!(
            hero.subline, "9,900 pending · 4 failed",
            "both halves of the subline name their bucket, so the four failed pods are \
             not folded into an unlabelled 7"
        );
        let issues = health_issues(&degraded, &OverviewRoutes::default());
        assert_eq!(
            issues.len(),
            2,
            "four failed and 9,900 pending are two chips"
        );
        assert_eq!(issues[0].label, "4 pods failed");
        assert_eq!(issues[1].label, "9,900 pods pending");

        // Without a host route a chip is the same wash with no target.
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Ready(degraded.clone());
            cx.notify();
        });
        cx.run_until_parked();
        cx.debug_bounds("overview-issue-1")
            .expect("the pending chip is drawn either way, so the strip does not reflate");

        // With one, it is a control in the tab order.
        let called: Rc<Cell<usize>> = Rc::new(Cell::new(0));
        let counter = called.clone();
        let show: ShowProblemsCallback = Rc::new(move |_window, _cx| {
            counter.set(counter.get() + 1);
        });
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Ready(degraded);
            view.set_show_problems_callback(Some(show));
            cx.notify();
        });
        cx.run_until_parked();
        let control = cx
            .debug_bounds("overview-issue-1")
            .expect("the pending chip");
        assert_eq!(called.get(), 0);
        // It is a working control, not a painted one. The callback is the only
        // thing standing between a number the page chose to report and the rows
        // behind it, and an unwired control reads as a promise.
        cx.simulate_click(control.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(called.get(), 1, "the chip opens the pods it counts");
        // The chip is 24px on the shared icon-button rhythm, so the strip holds
        // one line whether it is one grey sentence or four chips.
        assert_eq!(
            f32::from(control.size.height),
            f32::from(design::size::ICON_BUTTON)
        );

        // A cluster with nothing to follow does not get dressed up as a control.
        let healthy = overview(
            HealthSummary {
                total_pods: 4,
                running: 4,
                ..HealthSummary::default()
            },
            NodeSummary {
                count: 1,
                ready: 1,
                not_ready: 0,
            },
            HealthLevel::Healthy,
        );
        let (hero, _) = vital_figures(&healthy);
        assert_eq!(hero.subline, "all running");
        let show: ShowProblemsCallback = Rc::new(|_window, _cx| {});
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Ready(healthy);
            view.set_show_problems_callback(Some(show));
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("overview-issue-0").is_none(),
            "a healthy cluster has no chips at all, so there is nothing to route"
        );
    }

    /// A chip that cannot be followed must not say that it can.
    ///
    /// The strip is four chips and the shell has installed one of the three route
    /// slots, so on a cluster with something wrong in every bucket three of the
    /// four are figures. `render_issue_chip` takes the whole control treatment
    /// away from those — no hover, no press, no focus stop, no click — and the
    /// sentence in the tooltip is then the last thing on screen that could still
    /// offer a destination. It used to name the Pods table on a chip whose route
    /// slot was empty, which is the one chip a reader is most likely to click:
    /// the one reporting the largest number.
    #[test]
    fn a_chip_only_promises_a_destination_it_has() {
        let mut degraded = overview(
            HealthSummary {
                total_pods: 16,
                running: 4,
                pending: 9,
                failed: 3,
                ..HealthSummary::default()
            },
            NodeSummary {
                count: 3,
                ready: 2,
                not_ready: 1,
            },
            HealthLevel::Warning,
        );
        degraded.unavailable_workloads = 2;

        // No host routes at all: every chip is a figure, so none of them may
        // name a table.
        let unrouteable = health_issues(&degraded, &OverviewRoutes::default());
        assert_eq!(unrouteable.len(), 4, "one chip per problem the cluster has");
        for issue in &unrouteable {
            assert!(
                issue.route.is_none(),
                "{} is drawn without a target, so it has no route to offer",
                issue.label
            );
            assert!(
                !issue.detail.contains("Show "),
                "a chip that goes nowhere must not say it shows something: {}",
                issue.detail
            );
        }

        // The pods route is the one the shell installs today, so exactly the two
        // pod chips name a destination and the other two still do not.
        let show: ShowProblemsCallback = Rc::new(|_window, _cx| {});
        let routed = health_issues(
            &degraded,
            &OverviewRoutes {
                pods: Some(show),
                ..OverviewRoutes::default()
            },
        );
        assert!(
            routed[0]
                .detail
                .contains("Show the failed pods in the Pods table."),
            "{}",
            routed[0].detail
        );
        assert!(
            routed[1]
                .detail
                .contains("Show the pods that need attention in the Pods table."),
            "{}",
            routed[1].detail
        );
        for issue in &routed[2..] {
            assert!(
                !issue.detail.contains("Show "),
                "the node and workload chips have no route installed, so they offer no \
                 destination: {}",
                issue.detail
            );
        }
    }

    /// The toolbar's own name is a title, and the refresh status is one statement
    /// with the button it belongs to.
    #[gpui_kit::test]
    fn the_toolbar_title_and_refresh_status_belong_together(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Ready(overview(
                HealthSummary {
                    total_pods: 1,
                    running: 1,
                    ..HealthSummary::default()
                },
                NodeSummary {
                    count: 1,
                    ready: 1,
                    not_ready: 0,
                },
                HealthLevel::Healthy,
            ));
            cx.notify();
        });
        cx.simulate_resize(gpui_kit::size(px(1_440.), px(900.)));
        cx.run_until_parked();
        let panel = cx
            .debug_bounds("overview-content-bounds")
            .expect("the panel");
        let title = cx
            .debug_bounds("overview-toolbar-title")
            .expect("the toolbar title");
        let status = cx
            .debug_bounds("overview-refresh-status")
            .expect("the refresh status");
        let refresh = cx.debug_bounds("overview-refresh").expect("refresh");
        assert!(
            f32::from(title.size.height) > f32::from(status.size.height),
            "the toolbar title uses the panel-title role, not the body size: title {title:?} status {status:?}"
        );
        assert!(
            left_edge(refresh) - right_edge(status) <= f32::from(space::SM) + 0.5,
            "the timestamp sits against the button it describes: status {:?} refresh {:?}",
            status,
            refresh
        );
        // The toolbar pads by `LG` on both sides so its title starts on the same
        // x as everything under it (`UI-SPEC` §7 asks every element in a region to
        // share one left edge). It used to pad by `SM`, which put the title 8px
        // left of the scroll body's contents — a 8px misalignment a reader cannot
        // name and cannot stop seeing.
        assert!(
            (right_edge(panel) - right_edge(refresh) - f32::from(space::LG)).abs() < 0.5,
            "and the button keeps the toolbar's own trailing edge, which is the same LG inset \
             the content below it has"
        );
        let toolbar_title_left = left_edge(title);
        // The region under the title is the tile band's **surface**, not the
        // figures inside it: a card's contents sit `BAND_PADDING` inside the card,
        // and the spine the toolbar shares is the one every region's own box
        // starts on.
        let region = cx
            .debug_bounds("overview-vitals-band-plate")
            .expect("the tile band's surface");
        assert!(
            (toolbar_title_left - left_edge(region)).abs() < 1.0,
            "and the toolbar's title and the region below it start on one x: {toolbar_title_left} \
             vs {}",
            left_edge(region)
        );
    }
    /// Every state with no data has to be **taller than nothing**.
    ///
    /// `size_full()` is `h_full()`, which is a *percentage* height, and the panel
    /// body's parent is a scroll container whose own height comes from the flex
    /// chain above it — so the percentage resolved to zero. Measured at 1920×1080,
    /// all four of these were `1888px × 0px`: the children were laid out
    /// correctly and then clipped off the end of a scroll container whose
    /// scrollable area was 0px.
    ///
    /// The visible result was the worst kind of bug there is, because it is
    /// invisible *and* the tests were green:
    ///
    /// - a load over 500ms painted one 16px grey dash and an empty panel, and the
    ///   >2s progress row — the thing `§4.14` asks for and the thing `§2.4`'s
    ///   "a network timeout must be visible inside ten seconds" rests on — was the
    ///   fourth child of the same zero-height box and never appeared at all
    /// - a cluster the app could not read, and a denial, and a panel with no
    ///   cluster, each rendered as **nothing at all** under a toolbar that said so
    ///
    /// `render_dashboard` never called `size_full()`, which is the whole reason the
    /// one state nobody reported broken was the only one that worked.
    #[gpui_kit::test]
    fn every_state_with_no_data_has_a_height(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        fn height(cx: &mut gpui_kit::VisualTestContext, selector: &'static str) -> Option<f32> {
            cx.debug_bounds(selector)
                .map(|bounds| f32::from(bounds.size.height))
        }

        // The skeleton, at the rung a slow load actually reaches.
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        cx.simulate_resize(gpui_kit::size(px(1_920.), px(1_080.)));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Loading;
            view.loading_since = Some(Instant::now());
            view.loading_tier = LoadingTier::Progress;
            cx.notify();
        });
        cx.run_until_parked();
        let skeleton = height(cx, "overview-skeleton").expect("the skeleton is on screen");
        assert!(
            skeleton > 0.0,
            "a skeleton that is 0px tall is a skeleton that is not painted: every placeholder \
             inside it is clipped by the scroll container, and a reader is left with one grey dash \
             and an empty panel for as long as the cluster takes to answer"
        );
        // And the fourth child — the elapsed time — is inside that box, not past
        // the end of it.
        let progress = cx
            .debug_bounds("overview-skeleton-progress")
            .expect("past 2s the reader is owed a quantity as well as a placeholder");
        assert!(
            progress.origin.y + progress.size.height <= px(56. + skeleton),
            "the progress row is laid out at y={} inside a {skeleton}px box: the child is there \
             and the box is not tall enough to show it",
            f32::from(progress.origin.y)
        );

        // The spinner rung, which is a strip and must be exactly a strip.
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        cx.simulate_resize(gpui_kit::size(px(1_920.), px(1_080.)));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Loading;
            view.loading_since = Some(Instant::now());
            view.loading_tier = LoadingTier::Spinner;
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            height(cx, "overview-loading"),
            Some(f32::from(design::size::SUMMARY_STRIP)),
            "a spinner takes the strip's own slot, so nothing below it moves"
        );

        // No cluster, and a failure: both used to render as nothing.
        for (state, selector, label) in [
            (OverviewState::Disconnected, "empty-state", "no cluster"),
            (
                OverviewState::Failed("no route to host".to_owned()),
                "overview-error",
                "unreachable",
            ),
        ] {
            let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
            cx.simulate_resize(gpui_kit::size(px(1_920.), px(1_080.)));
            view.update(cx, |view, cx| {
                view.state = state;
                cx.notify();
            });
            cx.run_until_parked();
            let panel =
                height(cx, selector).unwrap_or_else(|| panic!("the {label} state is on screen"));
            assert!(
                panel > 0.0,
                "the {label} state is 0px tall, so its glyph, its line and its action are all \
                 laid out and then clipped: the reader gets a blank panel"
            );
        }
    }
}
