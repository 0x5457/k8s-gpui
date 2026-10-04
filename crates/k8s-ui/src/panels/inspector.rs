//! Inspector panel for YAML, Describe, Events, and Metrics.
//! Describe and Events load on demand and cache results by UID.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::label::Label;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Disableable as _, Icon, Sizable, Size, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{Animation, AnimationExt as _, Radians};
use gpui_kit::{
    AnyElement, AnyView, App, AppContext as _, ClickEvent, ClipboardItem, Context, Div, ElementId,
    Entity, FocusHandle, FontFeatures, Hsla, InteractiveElement, IntoElement, KeyDownEvent,
    Keystroke, ParentElement, Pixels, Render, Role, ScrollHandle, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Subscription, Task, UniformListScrollHandle, WeakEntity,
    Window, div, point, px, uniform_list,
};
use k8s_core::metrics::{SampleDecision, SampleScheduler};
use kube_core::DynamicObject;
use serde_json::Value;

use crate::charts::{ChartTable, LineChartView, Unit};
use crate::design::{self, Severity, border, radius, role, space, text};
pub use crate::session::InspectorSelection;
use crate::session::{InspectorBinding, InspectorBindingInput, InspectorUpdate};
use crate::yaml_editor::{Diagnostic, YamlView};

use super::common;
use super::common::{
    TabSpec, buffer_font, label_body, label_small, label_text, spinner, status_message,
};
pub use super::inspector_data::{ApplyRequest, ApplyTarget, InspectorSession};
use super::metrics::{
    DEFAULT_RANGE_MS, METRICS_UNAVAILABLE, MetricsHandle, MetricsProbeState, MetricsSamples,
    MetricsTarget, RANGE_OPTIONS, SamplePayload, retry_delay_text, sampling_interval_text,
};
use crate::session::{
    ApplyOutcome, DescribeData, InspectorSource, ObjectRef, OpsFuture, ResolveOutcome,
};

const COPIED_FEEDBACK: Duration = Duration::from_millis(1500);
/// Event cache lifetime.
const EVENTS_TTL: Duration = Duration::from_secs(15);
/// Maximum number of cached UIDs.
const CACHE_CAPACITY: usize = 16;
/// Maximum visible scalar fields before expansion.
const MAX_FIELD_ROWS: usize = 14;
const LABEL_CHIP_LIMIT: usize = 8;
const INSPECTOR_RELOAD_TAB_INDEX: isize = 8;
const METRICS_RANGE_TAB_INDEX: isize = 20;
const METRICS_RETRY_TAB_INDEX: isize = 10;
const INSPECTOR_CONTENT_TAB_INDEX: isize = 11;
const LABELS_EXPAND_TAB_INDEX: isize = 12;
const STATUS_EXPAND_TAB_INDEX: isize = 13;
const SPEC_EXPAND_TAB_INDEX: isize = 14;
/// The tab stops of the sections that gained a disclosure in `UI-REDESIGN.md` §3.4. They sit
/// above the three scroll regions so a keyboard walks the sections in the order they are read.
const CONDITIONS_EXPAND_TAB_INDEX: isize = 19;
const CONTAINERS_EXPAND_TAB_INDEX: isize = 21;
/// The Events disclosure inside the Describe body. It sits after Spec because that is where the
/// body puts it: the fields say what the object is, and the events say what the cluster did to it.
const EVENTS_EXPAND_TAB_INDEX: isize = 22;
const RELATED_EXPAND_TAB_INDEX: isize = 23;
/// The Identity disclosure. It sits after Related because it is the last thing a reader reaches
/// for: everything above it either names the problem or names a place to go.
const IDENTITY_EXPAND_TAB_INDEX: isize = 24;
/// The Diff mode's scroll region. It takes a stop above the tab panels because the mode
/// *replaces* them, so a keyboard that walked in document order would otherwise land on a
/// control the reader cannot see.
const REVIEW_SCROLL_TAB_INDEX: isize = 25;
/// Focus order of the YAML toolbar actions.
const APPLY_TAB_INDEX: isize = 1;
const REVERT_TAB_INDEX: isize = 2;
const COPY_YAML_TAB_INDEX: isize = 3;
/// The problems list renders directly under the YAML toolbar, so it takes the tab stop right
/// after it. Tab order follows visual order: a list that draws above the review strip has to be
/// reached before the strip, not after the three scroll regions of the other tabs.
const INSPECTOR_PROBLEMS_TAB_INDEX: isize = 4;
/// The review strip is the only place a write can start, so its safe action comes first and
/// keeps the first tab stop. The destructive action never takes focus by itself, and the
/// ascending indices below are that claim: the walk meets the safe action before the write.
const REVIEW_KEEP_EDITING_TAB_INDEX: isize = 5;
const REVIEW_APPLY_TAB_INDEX: isize = 7;
/// The fold on a long diff, after the summary it sits under.
const REVIEW_EXPAND_TAB_INDEX: isize = 8;
const _: () = assert!(REVIEW_KEEP_EDITING_TAB_INDEX < REVIEW_APPLY_TAB_INDEX);
const INSPECTOR_DESCRIBE_RETRY_TAB_INDEX: isize = 9;
/// Focus order of the YAML error state's Retry. It replaces the whole tab body, so it cannot share
/// a handle with the toolbar's controls; it takes the stop after the three scroll regions so the
/// YAML tab's own order is unchanged.
const INSPECTOR_YAML_RETRY_TAB_INDEX: isize = 18;
/// Every scrollable region is its own tab stop, so the keyboard can reach the content.
const INSPECTOR_DESCRIBE_SCROLL_TAB_INDEX: isize = 15;
const INSPECTOR_EVENTS_SCROLL_TAB_INDEX: isize = 16;
const INSPECTOR_METRICS_SCROLL_TAB_INDEX: isize = 17;
/// The object action row, which sits under the title in the identity band and is therefore
/// reached after the back control and the Related rows rather than with the tab strip.
const OBJECT_ACTION_TAB_INDEX: isize = 29;
/// First tab index of the value-row focus pool. The pool is handed out in render order, so the
/// rows follow the toolbar controls in document order. It starts at 34 rather than at 30 so the
/// five object-action stops above it have a band of their own: a control that shares an index with
/// the row beside it is a control the keyboard walks past in an order nobody chose.
const VALUE_FOCUS_POOL_TAB_INDEX: isize = 34;
/// Object actions the panel reserves tab stops for. The shell wires two or three; a fourth would
/// be a row that overflows a 260px panel, and `no_tab_loses_a_control_at_any_width_the_panel_allows`
/// is where that would show up.
const OBJECT_ACTION_TAB_SLOTS: isize = 5;
/// Focus handles reserved for the Describe value rows. One Describe body rarely holds more.
const VALUE_FOCUS_POOL_SIZE: usize = 96;
/// ARIA label of the Inspector tab strip.
const INSPECTOR_TAB_LIST_LABEL: &str = "Inspector tabs";
/// The tab strip holds the first tab stop, so focus enters the Inspector on the tabs.
const INSPECTOR_TAB_STRIP_TAB_INDEX: isize = 0;
/// `UI-SPEC.md` §4.19: the chord that copies the address of whatever the panel is showing.
const LINK_CHORD: &str = "secondary-l";
/// The header's copy-link control, the first stop on the title row.
const INSPECTOR_LINK_TAB_INDEX: isize = 26;
/// The header's back control, which exists only while the reader is inside a followed
/// relationship.
const INSPECTOR_BACK_TAB_INDEX: isize = 27;
/// The Related block's followable rows. One stop for the whole block rather than one per row:
/// the rows are the same distance from each other as the field rows above them, and a Tab that
/// has to pass six of them to reach the next control is a Tab that stops being used.
const RELATED_ROW_TAB_INDEX: isize = 28;
/// The confirmation names the object a write would touch, so it is never read as a generic
/// "are you sure".
const APPLY_REVIEW_TITLE: &str = "Apply these changes to the cluster?";
const APPLY_IN_FLIGHT_REASON: &str =
    "An apply is in progress, so the document is read-only. Wait for the result, then continue.";
const APPLY_CLEAN_REASON: &str = "The document matches the text the cluster reported, so there is nothing to preview. Edit the YAML, then preview it.";
const APPLY_UNAVAILABLE_REASON: &str =
    "No cluster is connected, so Preview is unavailable. Connect to a cluster, then retry.";
const APPLY_UNKNOWN_REASON: &str = "The result of the apply is unknown. Refresh the object to see whether the server applied the change.";
const APPLY_INVALID_REASON: &str =
    "The YAML has a problem. Fix every problem in the list, then preview again.";
const APPLY_STALE_REVIEW_REASON: &str =
    "The YAML changed while the review was open. Review the new text, then apply again.";
/// Severity marker slot. Always present, so a marked row keeps the same key and value
/// columns as an unmarked one.
///
/// The slot is the icon lane itself, because the three marks it holds — a severity, a
/// managed-field lock and a disclosure chevron — are one size and would otherwise disagree
/// inside the same 16px reservation.
const DESCRIBE_MARKER_SLOT: gpui_kit::Pixels = design::icon::IN_ROW;
/// Narrowest value column a two-column describe row may leave beside its key.
///
/// A value is YAML, a port range or a condition message. Below this it stops being skimmable,
/// which is the whole reason the two-column layout exists: the stacked layout is what a narrow
/// Inspector gets, and it is already the answer for "there is not enough room".
const DESCRIBE_VALUE_MIN_WIDTH: f32 = 164.0;
/// Narrowest content width that still leaves a readable value column beside the key.
///
/// Measured from [`DESCRIBE_KEY_COLUMN`] — the width the key is *drawn* at — and not from
/// [`DESCRIBE_KEY_WIDTH`]. Deriving it from the 220px figure put the breakpoint at 384px, above
/// the 352px `UI-SPEC.md` §11.1 gives the Inspector, so the two-column grid `UI-REDESIGN.md` §3.4
/// specifies was unreachable at every width the panel ships at: the default width got the
/// stacked layout and the grid only appeared on a panel dragged to its maximum. A breakpoint no
/// shipped width can satisfy is not a breakpoint, it is a dead branch.
const DESCRIBE_TWO_COLUMN_MIN_WIDTH: f32 = DESCRIBE_KEY_COLUMN + DESCRIBE_VALUE_MIN_WIDTH;
/// Values longer than this move under their key and wrap instead of running off the row.
///
/// Eighty characters is roughly what fits beside a `DESCRIBE_KEY_COLUMN` key inside the widest
/// Inspector (480px - 132px key - the marker slot - gaps), so it is where the shared one-line
/// layout stops being readable. It counts characters, not bytes, because the limit is about the
/// width a reader sees. It is a character count rather than a width because the value is set in
/// the data role, whose size the reader can change: a pixel limit would silently mean something
/// different at every setting, and a character count is what the wrap decision is actually about.
const DESCRIBE_INLINE_VALUE_LIMIT: usize = 80;
/// Title of the YAML read failure, which is a different boundary from "no row selected".
const YAML_LOAD_FAILED_TITLE: &str = "Failed to load YAML";
/// Label of the control that asks for the document again.
const YAML_RETRY_LABEL: &str = "Retry loading YAML";
const OBJECT_REPLACED_REASON: &str =
    "The server returned a different object. Reload the resource details.";
/// Reason for a load that is still running after [`LOAD_DEADLINE`].
const LOAD_TIMEOUT_REASON: &str = "The request is taking longer than expected.";
/// How long a `Loading` entry may stay before the tab offers Retry again.
const LOAD_DEADLINE: Duration = Duration::from_secs(10);
/// Height of a metrics chart, and of the sample table under it.
const METRICS_CHART_HEIGHT: f32 = 132.0;
const METRICS_TABLE_HEIGHT: f32 = 168.0;
/// `WRITE-OPS.md` §10.4: a diff longer than this folds, and the fold says how big it is.
const APPLY_REVIEW_DIFF_FOLD: usize = 60;
/// Lines shown before the fold, which is what the design fixes rather than a proportion: twenty
/// rows of change is enough to recognise the shape of what went wrong, and the summary under it
/// carries the rest.
const APPLY_REVIEW_DIFF_PREVIEW: usize = 20;
/// Problems shown at once. A document with dozens of parse problems must not push the editor
/// out of the panel, so the list is capped and scrolls past the cap. Nothing is dropped: the
/// title keeps the real total and the arrow keys walk every problem, so a capped list never
/// hides that Apply is blocked.
///
/// The count is stated rather than measured because the row height is not variable: a problem
/// message is clamped to [`PROBLEM_MESSAGE_LINES`] lines, so a row is
/// [`problem_row_height`] whatever the parser says, and eight of them are the budget below. It
/// used to be twelve one-line rows, which is the same 384px — twelve rows would now be 600px and
/// would have pushed a two-line document out of its own panel.
const PROBLEMS_VISIBLE_ROWS: usize = 8;
/// Lines a problem's message may take before it is clamped.
///
/// Two is the point at which the message still reads as a sentence, and the point at which a
/// longer one has already said which line and column to look at — the clamp is a floor on how
/// much space one problem may take, not the information the reader needs. The full text stays in
/// the row's tooltip and in its accessible name.
const PROBLEM_MESSAGE_LINES: usize = 2;
/// Lines the cluster's own refusal message may take in the apply-failed strip.
///
/// Two, for the reason [`PROBLEM_MESSAGE_LINES`] gives and with the same number: a validation
/// message opens with the field and the offending value — `spec.replicas: Invalid value: -1` —
/// which is what the reader has to act on, and everything after it is the API server explaining
/// its own schema. The strip shares the clamp so the two error surfaces in this panel cannot
/// disagree about how much of a sentence fits.
const APPLY_REASON_LINES: usize = 2;
/// The measure an error state's explanation wraps at.
///
/// A sentence in a 260px Inspector has to break somewhere, and a broken sentence with no measure
/// wraps at the panel edge - which puts its last word against the border. Forty characters is
/// what `UI-SPEC.md` §4.13's empty state uses and what leaves four or five words per line here.
const ERROR_MEASURE: Pixels = px(320.);
/// Width of the short dash a section heading wears instead of a full-width rule.
///
/// A rule that means "section" everywhere else is a rule that means nothing, and a table is the
/// only thing in the product with rules. Forty-eight pixels is short enough that it cannot be
/// mistaken for a divider and long enough to read as a mark after the word.
const SECTION_DASH_WIDTH: Pixels = px(48.);
/// Where the dash sits inside the 24px section band, in logical pixels.
///
/// `(ROW_DENSE − LINE) / 2` is 11.5, and a half-pixel offset is the whole bug: the band itself is
/// laid out from integer offsets, so every element on screen starts on a whole logical pixel, but
/// the one centred child of that band did not. At 2× the two device rows underneath it come out
/// even and the dash looks crisp; at 1× it straddles two rows at half coverage each, which reads
/// as a 2px line at half the ink — a divider that is twice as heavy and half as strong as every
/// other rule in the product.
///
/// Rounding the offset down keeps the dash on the device grid and moves its optical centre half a
/// pixel, which nothing can see. The general fix belongs at the root — every centred 1px element
/// in the app has this — and is reported rather than taken here, because `design.rs` is the shared
/// token layer; this constant is the panel's own instance of it until then.
const SECTION_DASH_OFFSET: Pixels = px(11.);
/// Lines of an event message the virtualised Events list shows before it clips.
///
/// A Kubernetes event message is a sentence, not a sentence and a half: `FailedScheduling` on a
/// three-node cluster runs to five lines, and a uniform row that tried to hold all of them would
/// cost 80px per event in a list that can hold twenty. Three lines is where the cause usually
/// stops, the full text is on the row's accessible name and one hover away, and the Describe
/// block's inline timeline still shows events whole.
const EVENT_MESSAGE_LINES: usize = 3;
/// Key column of a Describe key/value grid, and the one the row actually reserves.
///
/// `UI-REDESIGN.md` §3.4 fixes the pair at `132px | 1fr`. The key is a field path such as
/// `spec.containers[0].imagePullPolicy`, and a truncated key is a problem in a way a truncated
/// value is not — the key is how the reader finds the field they are looking for — so the full
/// text is one hover away and the clip is the accepted cost of a 352px column.
///
/// It is a measure rather than a rhythm step, so it is not a spacing token: no value in the 4px
/// scale is a readable key column. It is stated here beside [`DESCRIBE_VALUE_MIN_WIDTH`], the
/// other half of the pair, and the two cannot drift apart because
/// [`DESCRIBE_TWO_COLUMN_MIN_WIDTH`] is their sum.
const DESCRIBE_KEY_COLUMN: f32 = 132.0;
/// Value column the Related block gives an object's name, in `UI-REDESIGN.md` L3.
const RELATED_NAME_COLUMN: f32 = 96.0;
/// Key column of the Identity block.
///
/// The Identity keys are words - `Node`, `IP`, `UID`, `QoS` - not field paths, so the grid does
/// not need the width a path needs. It takes the mockup's own 88px, which fits the longest key
/// (`Node`) with room to spare and hands the rest of a 352px panel to the values, which are UIDs
/// and node names and are the part that gets clipped.
const IDENTITY_KEY_COLUMN: f32 = 88.0;
/// Characters of a UID the Identity block shows before it truncates.
///
/// `§2.3` asks for a middle ellipsis on identifiers, and a UID is one. It is a character count
/// rather than a pixel limit for the reason [`DESCRIBE_INLINE_VALUE_LIMIT`] gives: the reader can
/// change the data font size, and "36 characters" means the same thing at every setting.
const UID_INLINE_CHARS: usize = 36;
/// Width below which the panel stops reading as a column even when it is docked.
///
/// This is the *panel's own* narrow end and nothing to do with the window: a 260px column with
/// square corners, no rule and no shadow is a column, and it is the right answer in a 1400px
/// window where the shell gave the Inspector the minimum.
///
/// It is not the same question as whether the shell has floated the panel. That one is about the
/// window — `design::size::INSPECTOR_FLOAT_BELOW` — and it is answered in [`Self::floating_frame`]
/// from the viewport, because the shell's own `inspector_layout` is a function of the window width
/// and nothing else. The two used to be one constant that answered neither: 320 is narrower than
/// any width the shell ever floats the panel at, so the overlay frame was a branch no shipped
/// width could reach.
const INSPECTOR_OVERLAY_BELOW: f32 = 320.0;
/// Height of one tab in the Inspector's tab strip: the shared tab pill, so the strip
/// reads as a band with a rounded shape in it rather than as four rectangles painted
/// edge to edge. The centre tab strip and the Dock strip draw the same pill inside
/// their own bars.
const INSPECTOR_TAB_HEIGHT: Pixels = design::size::TAB_PILL;
/// The one word this panel prints for a field that has no value.
///
/// `resources {}` and `finalizers []` are the API server's source format leaking through the
/// interface: a reader is not asking what the JSON looks like, they are asking whether the field
/// is set. One word, in `role::fg_disabled`, in the UI face, everywhere — so a row that says
/// nothing is recognisable at a glance instead of being a row that looks like data and is not.
const EMPTY_VALUE: &str = "None";

// ── Component state ───────────────────────────────────────────────────────────
//
// A state is a role plus a state, never a second colour, so every wash below is a
// `role::*` value at an alpha. Hover and press are the two the design fixes
// (`UI-SPEC.md` §3: 4% white / 3.5% black on hover, +8% alpha on press), and the
// selection is the accent wash, which is the one accent the Inspector is allowed.

/// The hover fill of a control that carries no border of its own.
///
/// `UI-SPEC.md` §3.4 gives hover as translucent ink, and the ink is the panel's own primary
/// role, so the same expression is a 4% wash in the dark appearance and a 3.5% wash in the
/// light one without a second token.
fn hover_wash(cx: &App) -> Hsla {
    let alpha = if design::appearance(cx) == design::Appearance::Light {
        0.035
    } else {
        0.04
    };
    role::fg_primary(cx).opacity(alpha)
}

/// The press fill: hover plus the `+8% alpha` of `UI-SPEC.md` §3.
fn press_wash(cx: &App) -> Hsla {
    let alpha = if design::appearance(cx) == design::Appearance::Light {
        0.115
    } else {
        0.12
    };
    role::fg_primary(cx).opacity(alpha)
}

/// The focus rail's ink. `UI-SPEC.md` §3 makes focus-visible an accent border.
fn focus_ink(cx: &App) -> Hsla {
    role::accent(cx)
}

/// The hover and press fills for a control that is *already* selected.
///
/// A selected item that has no hover of its own is a pointer resting on nothing, so the open tab
/// in the strip takes the accent over its own fill at the two alphas `design::state` fixes. It is
/// the accent rather than the panel's primary ink because the item is already carrying the
/// selection, and a second hue would be a second thing to read.
///
/// Composited rather than returned at an alpha, because the tab sits on `surface_raised` and a
/// translucent accent over the wrong plane is a colour nobody chose.
fn hover_on(surface: Hsla, ink: Hsla) -> Hsla {
    design::state::hover_on(surface, ink)
}

/// The press step of [`hover_on`]: the same tint, committed once.
fn press_on(surface: Hsla, ink: Hsla) -> Hsla {
    design::state::press_on(surface, ink)
}

/// The ink a field the cluster owns is drawn in.
///
/// `UI-SPEC.md` §13.3: a managed field is one the controller writes back, so editing it is
/// pointless and not knowing that costs a beginner an afternoon. The value is drawn quieter than
/// its neighbours, with a lock beside it.
///
/// It is `fg_tertiary` and *not* `fg_disabled`, and the reason is what `fg_disabled` *is* rather
/// than how dark it is. `UI-SPEC.md` §3 gives `fg_disabled` as a *control* state — "opacity .40,
/// no hover, no pointer" — and the Inspector has no disabled control: nothing here is switched off,
/// so a `fg_disabled` value would be borrowing the appearance of a control this panel does not have
/// to mean that the value is one this panel does not own.
///
/// The contrast is worth stating because it was the other half of the old argument and it is no
/// longer true. `fg_disabled` used to measure 1.99:1 on the light content surface, below the 3:1
/// floor `DISABLED_TEXT_MIN_CONTRAST` sets for it, which is why this function was written at all;
/// the theme's derivation layer now solves that role against its own floor, and it measures
/// **3.61:1** in the light appearance and 3.52:1 in the dark one — see
/// `every_role_the_inspector_draws_clears_its_floor_in_both_appearances`, which re-measures both
/// on this panel's own surface so neither number here is a memory. `fg_tertiary`, the ink actually
/// used, measures 4.14:1 light and 4.04:1 dark: above the same 3:1 floor, and still a clear step
/// quieter than the `fg_primary` the unmanaged values beside it use, which is the whole point of
/// the mark.
fn managed_ink(cx: &App) -> Hsla {
    role::fg_tertiary(cx)
}

// Toolbar commands. Each one is dispatchable so a keymap or the command palette can
// reach the same entry point as the toolbar buttons.
gpui_kit::actions!(
    k8s_inspector,
    [
        // Reloads the data of the active Describe, Events, or Metrics tab.
        ReloadActiveTab,
        // The six time ranges `UI-SPEC` §16.5 fixes: 1m / 15m / 1h / 6h / 24h /
        // 7d. The old ladder was 5m / 15m / 1h, which is three of the six, and
        // the segment control claimed to cover the choice a reader makes on a
        // dashboard while omitting the two they make most: the last hour, and
        // the day they are actually looking at.
        MetricsRange1m,
        MetricsRange15m,
        MetricsRange1h,
        MetricsRange6h,
        MetricsRange24h,
        MetricsRange7d,
        // Retries the metrics probe or takes the next sample now.
        RetryMetrics,
        // Confirms the reviewed change and writes it to the cluster.
        ConfirmApply,
        // Drops the review without writing anything.
        CancelApplyReview,
        // Restores the text the cluster last reported.
        RevertYaml,
        // Copies the YAML in the editor.
        CopyYaml,
        // Expands the focused Describe value to its full text, or collapses it again.
        ToggleValueExpansion,
        // Copies the focused Describe value.
        CopyValue,
        // Moves the keyboard cursor to the next YAML problem and reveals it.
        NextProblem,
    ]
);

/// The four tabs, and the four glyphs, measured rather than chosen by eye.
///
/// Ink coverage normalised by each mark's own peak, off a 4x capture
/// (`size::TAB_PILL`'s 28px measures 112 capture px, which pins the scale):
/// `FileCode` 0.229, `Bell` 0.144, `TextQuote` 0.123, `SignalHigh` 0.078.
///
/// So the heavy mark in this strip is the **document**, on the tab whose whole
/// subject is a document, and it is heavy because that glyph fills more of its
/// box — not because it is drawn larger. All four are one size, the strip is
/// shared with the centre and Dock tabs, and selection changes no size, so the
/// only lever on this number is the shape itself.
///
/// **It stays.** Swapping a correct glyph for a lighter one buys 0.08 of ink and
/// costs the tab the mark that names it; a reader who has learned that the
/// document tab carries a document does not have to be told what it is. The
/// number is recorded here so the next person to look at this strip and think
/// the file mark is too heavy can read what it was measured against instead of
/// looking again.
const TABS: [TabSpec; 3] = [
    TabSpec {
        label: "YAML",
        icon: IconName::FileCode,
    },
    TabSpec {
        label: "Describe",
        icon: IconName::TextQuote,
    },
    TabSpec {
        label: "Events",
        icon: IconName::Bell,
    },
];

const METRICS_TAB: TabSpec = TabSpec {
    label: "Metrics",
    icon: IconName::SignalHigh,
};

/// The banner copy for an object the cluster no longer has.
///
/// `UI-SPEC.md` §4.15: the state, then the next step, and never the API server's own sentence. The
/// object is named because a reader who deleted one Pod out of forty has to know it was that one —
/// the table row is already gone, so nothing else on screen identifies it.
fn object_gone_reason(kind: &str, name: &str) -> String {
    // The namespace is deliberately not repeated here. The identity line sits directly above this
    // strip and already reads "Pod \u{b7} <namespace> \u{b7} <age>", so saying it again here made
    // the strip wrap to five lines inside a 420px panel -- which is the duplication R1 exists to
    // remove. Kind and name stay, because they are what the alert has to name for a screen reader,
    // which reads this strip and not the header above it.
    format!("This {kind} {name} was deleted.")
}

/// The gone strip's second line: why the values underneath cannot be trusted.
///
/// Separate from [`object_gone_reason`] because staleness is a property of showing a stale copy,
/// not of the deletion, so any future banner with the same caveat does not have to invent it.
const GONE_STALE_NOTE: &str = "The values below are its last known state.";

/// The next step every connection-shaped failure offers, in one sentence.
///
/// It was written out three times — once here and once per metrics failure state — and the two
/// metrics copies said "make sure the cluster connection works", which is a thing a reader has to
/// interpret rather than a thing they can do. One string, so the three states cannot drift, and
/// a verb the reader can act on: check the thing, then retry.
const CHECK_CONNECTION_AND_RETRY: &str = "Check the cluster connection, then retry.";

/// User-facing next step for a load failure.
fn load_failure_hint(reason: &str) -> &'static str {
    if reason == common::NOT_CONNECTED_REASON {
        "Connect to a cluster, then retry."
    } else if reason == OBJECT_REPLACED_REASON {
        "The object was replaced. Select the object again, then reload."
    } else if reason == LOAD_TIMEOUT_REASON {
        "The cluster is slow to answer. Retry, or check the cluster connection."
    } else {
        CHECK_CONNECTION_AND_RETRY
    }
}

/// Inspector tabs used by commands and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectorTab {
    Yaml = 0,
    Describe = 1,
    Events = 2,
    Metrics = 3,
}

/// The tab a reader lands on when they select an object.
///
/// `UI-REDESIGN.md` §3.4 puts Status first "because it is the only thing you open the panel
/// for", and D27 / `UI-SPEC.md` §13.1 make the document the fallback rather than the entrance:
/// `replicas`, `image`, `labels` and `resources` have controls, and only CRDs, patches and
/// schemaless fields belong in a text buffer.
///
/// Landing on the document instead meant that selecting a Pod opened line 221 of a 400-line
/// manifest. The answer to "what is wrong with this thing" was one tab away and invisible, and
/// `UI-SPEC.md` §13.6's own complaint — that 300 lines of YAML does not fit in a 352px column —
/// was on screen by default. YAML is still one click away, and it is still the only way to reach
/// a CRD; it is just no longer the first thing on the panel.
const DEFAULT_TAB: usize = InspectorTab::Describe as usize;
#[cfg(test)]
const DEFAULT_TAB_ENUM: InspectorTab = InspectorTab::Describe;

#[derive(Clone, Debug)]
enum LoadState<T> {
    /// The request is outstanding. It carries when it started because `UI-SPEC.md` §4.14 grades
    /// the *wait* — under 200ms shows nothing, 200ms to 2s shows a spinner — and a state that
    /// cannot say how long it has been waiting cannot honour that.
    Loading {
        since: Instant,
    },
    Ready(T),
    Failed(String),
}

impl<T> LoadState<T> {
    /// How long this state has been the one on screen.
    fn elapsed(&self) -> Duration {
        match self {
            Self::Loading { since } => since.elapsed(),
            // A settled state is not waiting, so it reports the floor rather than an unbounded
            // age: the number is only read while a fetch is outstanding.
            _ => Duration::ZERO,
        }
    }
}

#[derive(Clone, Debug)]
struct EventsEntry {
    fetched_at: Instant,
    state: LoadState<Arc<Vec<DynamicObject>>>,
}

/// A selection deferred while YAML has unsaved changes.
#[derive(Clone, Debug)]
enum PendingLoad {
    Yaml(Option<String>),
    Selection(Option<InspectorSelection>),
}

/// One lazily loaded tab, so a stuck request can be tracked separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoadKind {
    Describe,
    Events,
}

type ApplyHandler = Box<dyn Fn(ApplyRequest, &mut App)>;
#[cfg(test)]
type ApplyCallback = Box<dyn Fn(ApplyRequest)>;
type CheckHandler = std::rc::Rc<dyn Fn(ApplyRequest, &mut App)>;

/// What the server concluded, reduced to what the review needs to say.
pub enum ApplyVerdict {
    Valid,
    Conflict { owners: Vec<String> },
}
type MetricsProbeRetry = Rc<dyn Fn(&mut App)>;
/// Asks whoever owns the document to fetch it again. See [`InspectorPanel::yaml_reload`].
type YamlReload = Rc<dyn Fn(&mut App)>;

/// Which Describe sections the reader has expanded past their cap.
///
/// This is the "show 40 more fields" state, which is separate from whether a section is open at
/// all: `UI-REDESIGN.md` §3.4 puts Status first and always open, and every other section is a
/// disclosure, so "collapsed" and "showing its first fourteen rows" are two different questions.
#[derive(Clone, Copy, Debug, Default)]
struct ExpandedDetails {
    labels: bool,
    spec: bool,
}

/// One collapsible section of the Describe body.
///
/// `UI-REDESIGN.md` §3.4: `▸ LABELS 3`, `▸ CONDITIONS 1`, `▸ CONTAINERS 1`, `▸ SPEC`. Status is
/// the one exception - it is first and always open, because it is the only block the reader
/// opened the panel for - so it is spelled here rather than being a special case at three call
/// sites.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum DetailSection {
    Labels,
    Conditions,
    Containers,
    Status,
    Spec,
    Events,
    Related,
    Identity,
}

impl DetailSection {
    const fn title(self) -> &'static str {
        match self {
            Self::Labels => "Labels",
            Self::Conditions => "Conditions",
            Self::Containers => "Containers",
            Self::Status => "Status",
            Self::Spec => "Spec",
            Self::Events => "Events",
            Self::Related => "Related",
            Self::Identity => "Identity",
        }
    }

    /// The tab stop the section's disclosure takes, in document order.
    const fn tab_index(self) -> isize {
        match self {
            Self::Labels => LABELS_EXPAND_TAB_INDEX,
            Self::Conditions => CONDITIONS_EXPAND_TAB_INDEX,
            Self::Containers => CONTAINERS_EXPAND_TAB_INDEX,
            // Status is never collapsed, so it takes no tab stop. A stop that opens nothing is
            // a stop the keyboard walks through for no reason.
            Self::Status => STATUS_EXPAND_TAB_INDEX,
            Self::Spec => SPEC_EXPAND_TAB_INDEX,
            Self::Events => EVENTS_EXPAND_TAB_INDEX,
            Self::Related => RELATED_EXPAND_TAB_INDEX,
            Self::Identity => IDENTITY_EXPAND_TAB_INDEX,
        }
    }
}

/// A change that passed every guard and waits for a review.
///
/// The panel never writes from the editor keystroke: it captures the text, the target, and the
/// identity that target names, and the review strip is the only way to turn this into a request.
#[derive(Clone, Debug)]
struct PendingApply {
    request_id: u64,
    target: ApplyTarget,
    yaml: String,
}

/// What the API server said when asked to validate the document under review, without storing it.
///
/// A local parse cannot see a schema violation, an immutable field, an unknown enum value, or a
/// missing required field. Those are the failures that surface after an apply has already
/// half-succeeded, which is the worst moment to learn about them, so the review asks the server
/// first and states its answer before anything is written.
///
/// The real apply does not ask for strict field validation, so nothing short of this check tells
/// a reader that their document has a field the schema rejects. An answer the reader has to go
/// and ask for is an answer most of them do not have, and on the last screen before a write that
/// is the same as having none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum ApplyCheckState {
    /// No verdict for the document on screen. Opening a review starts the request, so this is
    /// what the panel carries between reviews.
    #[default]
    Running,
    /// The server accepted the document and returned the object it would store.
    Valid,
    /// Another field manager owns a field this apply would change.
    Conflict { owners: Vec<String> },
    /// The check could not complete. Applying is still allowed; the reason is shown instead.
    Failed { reason: String },
}

/// One flattened field: its path, the text it shows, and whether the JSON value is a string.
type FieldRow = (String, String, bool);

/// How a field value is drawn.
///
/// The size follows the section, so one column cannot change size halfway down, and the font
/// follows the value's type, so an address or a digest reads as code while a count does not.
#[derive(Clone, Copy)]
struct ValueStyle {
    data: bool,
    mono: bool,
}

impl ValueStyle {
    /// A value in a data column, drawn in the buffer font when it is a string.
    fn data(is_string: bool) -> Self {
        Self {
            data: true,
            mono: is_string,
        }
    }

    /// A value in a prose column, drawn in the UI font.
    fn prose() -> Self {
        Self {
            data: false,
            mono: false,
        }
    }
}

/// Focus handles for the Describe value rows.
///
/// The handles are created once and handed out in render order, so Tab walks the rows in the
/// order they are read, and a row keeps its handle between renders. A body with more rows than
/// the pool holds gets no handle for the overflow, which costs those rows their keyboard
/// affordance instead of stealing another row's focus.
struct ValueFocus {
    pool: Vec<FocusHandle>,
    next: usize,
    by_selector: HashMap<String, FocusHandle>,
    texts: HashMap<String, String>,
}

impl ValueFocus {
    fn new(pool: Vec<FocusHandle>) -> Self {
        Self {
            pool,
            next: 0,
            by_selector: HashMap::new(),
            texts: HashMap::new(),
        }
    }

    /// Starts a new render pass, so the selector map describes the rows on screen now.
    fn begin_render(&mut self) {
        self.next = 0;
        self.by_selector.clear();
        self.texts.clear();
    }

    /// Reserves the next handle for a row, and remembers the text a copy would take.
    fn take(&mut self, selector: &str, value: &str) -> Option<FocusHandle> {
        // The text is recorded even when the pool is exhausted, so a copy still takes the full
        // value of a row that lost its tab stop.
        self.texts.insert(selector.to_owned(), value.to_owned());
        let handle = self.pool.get(self.next)?.clone();
        self.next += 1;
        self.by_selector.insert(selector.to_owned(), handle.clone());
        Some(handle)
    }

    #[allow(dead_code)]
    fn get(&self, selector: &str) -> Option<FocusHandle> {
        self.by_selector.get(selector).cloned()
    }

    fn text(&self, selector: &str) -> Option<String> {
        self.texts.get(selector).cloned()
    }
}

/// What every value row needs while the Describe body renders.
#[derive(Clone)]
struct ValueRows {
    focus: Rc<RefCell<ValueFocus>>,
    expanded: Rc<BTreeSet<String>>,
    /// Rows whose copy feedback is still on screen.
    copied: Rc<BTreeSet<String>>,
    /// The field paths a controller owns, so a managed row can say it is not the reader's.
    managed: Rc<ManagedFields>,
    panel: WeakEntity<InspectorPanel>,
}

impl ValueRows {
    fn take_focus(&self, selector: &str, value: &str) -> Option<FocusHandle> {
        self.focus.borrow_mut().take(selector, value)
    }

    fn is_expanded(&self, selector: &str) -> bool {
        self.expanded.contains(selector)
    }

    /// Whether the copy feedback of a row is still showing, so the row can say so.
    fn copied(&self, selector: &str) -> bool {
        self.copied.contains(selector)
    }

    /// Whether a Describe field path belongs to a field manager.
    fn managed(&self, path: &str) -> bool {
        self.managed.owns(path)
    }
}

/// The set of field paths the API server reports as owned by a field manager.
///
/// `UI-SPEC.md` §13.3 and `WRITE-OPS.md` §3.1 both rest on the same fact: a field in
/// `metadata.managedFields` is written back by a controller, so editing it is not merely
/// discouraged, it is undone. Not knowing that is a large share of why a beginner loses an
/// afternoon to a `replicas` value that keeps coming back.
///
/// The set is built from the object once per Describe render rather than per row, because a
/// Pod's `managedFields` carries hundreds of paths and a linear scan per row would make the
/// panel quadratic in the number of fields it draws.
#[derive(Clone, Default)]
struct ManagedFields {
    paths: BTreeSet<String>,
    /// True when the object reports no managers at all, in which case nothing is locked. An
    /// object from a cluster that has server-side apply off simply has no list, and marking
    /// every field would be a worse lie than marking none.
    known: bool,
}

impl ManagedFields {
    /// Reads `metadata.managedFields[].fieldsV1` into a set of dotted paths.
    ///
    /// `fieldsV1` is a JSON object whose keys are the field paths in the server's own spelling:
    /// `f:spec`, `f:containers`, `k:{"name":"api"}`, `v:...`, `i:0`. Only the `f:` keys are
    /// fields; `k:` is a map key, `v:` a value and `i:` an index, and treating those as fields
    /// would lock half the object for no reason.
    fn from_object(object: &DynamicObject) -> Self {
        let Some(entries) = object.metadata.managed_fields.as_ref() else {
            return Self::default();
        };
        let mut fields = Self {
            paths: BTreeSet::new(),
            known: !entries.is_empty(),
        };
        for entry in entries {
            let Some(fields_v1) = entry.fields_v1.as_ref() else {
                continue;
            };
            // `FieldsV1` is a newtype over the JSON the server sent, so the walk reads through
            // it rather than re-serialising the whole object.
            collect_managed_paths(&fields_v1.0, "", &mut fields.paths);
        }
        fields
    }

    /// Whether any manager owns `path`, or a prefix of it.
    ///
    /// The prefix case matters: a manager that owns `spec` owns every field beneath it, and a
    /// row for `spec.containers[0].image` has to answer to the `spec` entry rather than to
    /// nothing.
    fn owns(&self, path: &str) -> bool {
        self.known
            && self.paths.iter().any(|owned| {
                path == owned
                    || path.starts_with(&format!("{owned}."))
                    || path.starts_with(&format!("{owned}["))
            })
    }
}

/// Walks a `fieldsV1` object, collecting the `f:` keys as dotted paths.
fn collect_managed_paths(value: &Value, prefix: &str, out: &mut BTreeSet<String>) {
    let Value::Object(map) = value else {
        return;
    };
    for (key, child) in map {
        let Some(field) = key.strip_prefix("f:") else {
            // `k:`, `v:` and `i:` are not fields, but a `k:` entry still nests fields under it,
            // so the walk continues with the same prefix.
            if key.starts_with("k:") {
                collect_managed_paths(child, prefix, out);
            }
            continue;
        };
        let path = if prefix.is_empty() {
            field.to_owned()
        } else {
            format!("{prefix}.{field}")
        };
        out.insert(path.clone());
        collect_managed_paths(child, &path, out);
    }
}

/// One line of a local diff, shown in the review strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffLine<'a> {
    Context(&'a str),
    Added(&'a str),
    Removed(&'a str),
}

impl DiffLine<'_> {
    fn text(&self) -> &str {
        match self {
            Self::Context(text) | Self::Added(text) | Self::Removed(text) => text,
        }
    }

    fn marker(&self) -> &'static str {
        match self {
            Self::Context(_) => " ",
            Self::Added(_) => "+",
            Self::Removed(_) => "-",
        }
    }

    fn severity(&self) -> Severity {
        match self {
            Self::Context(_) => Severity::Muted,
            Self::Added(_) => Severity::Success,
            Self::Removed(_) => Severity::Error,
        }
    }
}

/// One field-level change, in the shape `WRITE-OPS.md` §3.1 asks for.
///
/// A path and what it was and what it is. Forty lines of red and green is a diff of the *text*;
/// this is a diff of the *object*, which is the thing the reader is about to change.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FieldChange {
    path: String,
    before: Option<String>,
    after: Option<String>,
}

/// How big a reviewed diff is, and whether this reader has opened it.
///
/// One value because it is one decision, made once where the diff's size is known and drawn once
/// where it is shown: a fold that claims a different count from the summary under it is the kind
/// of small untruth that makes a reader stop trusting the number next to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DiffShape {
    foldable: bool,
    expanded: bool,
    added: usize,
    removed: usize,
}

impl FieldChange {
    /// `2 changes` / `1 change`, the count the summary line leads with.
    fn summary(count: usize) -> String {
        format!("{count} change{}", if count == 1 { "" } else { "s" })
    }
}

/// The kinds whose diff is computed field by field.
///
/// `WRITE-OPS.md` §3.1 splits the diff two ways: the built-in kinds get a semantic diff, and
/// anything else falls back to a line diff of the YAML. The line is not a list of icons — it is
/// the list of kinds whose *shape* this product knows, which is the same list `§13.1` uses to
/// decide which fields get a control instead of an editor. Secret is not one of the twelve
/// bespoke kinds but it is here because §10.1 requires its values to be masked, and a mask is
/// something the semantic diff is where you put it.
const SEMANTIC_DIFF_KINDS: [&str; 13] = [
    "Pod",
    "Deployment",
    "StatefulSet",
    "ReplicaSet",
    "DaemonSet",
    "Job",
    "CronJob",
    "Node",
    "Service",
    "Ingress",
    "ConfigMap",
    "Namespace",
    "Secret",
];

/// Whether a kind's diff is computed field by field rather than line by line.
fn semantic_diff_kind(kind: &str) -> bool {
    SEMANTIC_DIFF_KINDS
        .iter()
        .any(|known| kind.eq_ignore_ascii_case(known))
}

/// The fields a diff has to leave out, or every diff is hundreds of lines of nothing.
///
/// `WRITE-OPS.md` §3.1 names them: `metadata.resourceVersion`, `managedFields`, `status.*` and
/// `creationTimestamp`. The last two are the same idea — the server writes them and a person
/// never does — and the two annotations are here for the same reason: both are written by other
/// controllers, so a diff that keeps them reports a change the reader did not make.
const DIFF_NOISE: [&str; 4] = [
    "metadata.resourceVersion",
    "metadata.managedFields",
    "metadata.creationTimestamp",
    "status",
];

/// Annotations no one edits by hand, and which change on every write.
const DIFF_NOISE_ANNOTATIONS: [&str; 2] = [
    "kubectl.kubernetes.io/last-applied-configuration",
    "deployment.kubernetes.io/revision",
];

/// Flattens a parsed object into `path → scalar` leaves.
///
/// A list of named objects is keyed by its `name` rather than by its position, so a Deployment
/// whose containers were reordered reports no change at all instead of six. That is the whole
/// difference between a diff of the object and a diff of the text, and it is why the path reads
/// `containers[name=api].image`: the reader is looking for the container, not for row 0.
///
/// A list with no names to match on is indexed, because there is nothing else to pair its
/// elements by and a positional path is still a true one.
fn flatten_fields(
    value: &Value,
    prefix: &str,
    out: &mut BTreeMap<String, String>,
    mask: Option<&[&str]>,
) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                if DIFF_NOISE.contains(&path.as_str()) {
                    continue;
                }
                if prefix == "metadata.annotations"
                    && DIFF_NOISE_ANNOTATIONS.contains(&key.as_str())
                {
                    continue;
                }
                flatten_fields(child, &path, out, mask);
            }
        }
        Value::Array(items) if !items.is_empty() => {
            let named = items
                .iter()
                .all(|item| item.get("name").and_then(Value::as_str).is_some());
            for (index, item) in items.iter().enumerate() {
                let segment = if named {
                    format!(
                        "[name={}]",
                        item.get("name").and_then(Value::as_str).unwrap_or_default()
                    )
                } else {
                    format!("[{index}]")
                };
                flatten_fields(item, &format!("{prefix}{segment}"), out, mask);
            }
        }
        other => {
            // `WRITE-OPS.md` §10.1: a Secret's values are masked and its keys are not, because
            // knowing *which* key changed is the point and reading the new password off a
            // shoulder is not. The test is on the leaf's own parent, so `metadata.name` stays
            // legible and only the two payload roots go dark.
            let text = match mask {
                Some(roots)
                    if prefix
                        .rsplit_once('.')
                        .is_some_and(|(parent, _)| roots.contains(&parent)) =>
                {
                    DIFF_MASK.to_owned()
                }
                _ => canonical_scalar(other),
            };
            out.insert(prefix.to_owned(), text);
        }
    }
}

fn canonical_scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        // The same word the Describe body prints for a field with nothing in it, so a reader who
        // has just read `None` in the field list meets the same word in the review that says what
        // would change it.
        Value::Null => EMPTY_VALUE.to_owned(),
        other => other.to_string(),
    }
}

/// A field diff and how much of the object it left alone.
///
/// The unchanged count is part of the answer, not a footnote: "40 lines of red and green" is
/// unreadable partly because nothing says the other 60 fields did not move, and "2 changes" on
/// its own reads like the tool only looked at two fields.
struct SemanticDiff {
    changes: Vec<FieldChange>,
    unchanged: usize,
}

/// The semantic diff between the text the cluster reported and the text to apply.
///
/// `None` when either side will not parse, which is the one thing a field diff cannot survive
/// and the reason the line diff exists at all.
fn semantic_diff(old: &str, new: &str, mask_values: bool) -> Option<SemanticDiff> {
    let before = flatten_object(old, None)?;
    let mask: Option<&[&str]> = mask_values.then_some(DIFF_MASKED_ROOTS.as_slice());
    let after = flatten_object(new, mask)?;
    let unchanged = before
        .iter()
        .filter(|(path, value)| after.get(*path) == Some(*value))
        .count();
    let mut changes: Vec<FieldChange> = before
        .iter()
        .filter(|(path, _)| !after.contains_key(*path))
        .map(|(path, value)| FieldChange {
            path: (*path).clone(),
            before: Some(value.clone()),
            after: None,
        })
        .chain(
            after
                .iter()
                .filter(|(path, _)| !before.contains_key(*path))
                .map(|(path, value)| FieldChange {
                    path: (*path).clone(),
                    before: None,
                    after: Some(value.clone()),
                }),
        )
        .collect();
    for (path, value) in &after {
        if let Some(previous) = before.get(path)
            && previous != value
        {
            changes.push(FieldChange {
                path: path.clone(),
                before: Some(previous.clone()),
                after: Some(value.clone()),
            });
        }
    }
    // Document order is the order a reader checks a manifest in, so the diff follows it, and the
    // rank is only a tie-break between two changes to the same field.
    changes.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| change_rank(a).cmp(&change_rank(b)))
    });
    Some(SemanticDiff { changes, unchanged })
}

/// Parses a manifest and flattens it, or `None` when it will not parse.
fn flatten_object(text: &str, mask: Option<&[&str]>) -> Option<BTreeMap<String, String>> {
    let parsed = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text).ok()?;
    let value = serde_json::to_value(parsed).ok()?;
    let mut out = BTreeMap::new();
    flatten_fields(&value, "", &mut out, mask);
    Some(out)
}

/// `Removed` before `Changed` before `Added`, so a rewritten field reads as one edit rather than
/// as a deletion followed by an unrelated insertion.
fn change_rank(change: &FieldChange) -> u8 {
    match (&change.before, &change.after) {
        (Some(_), None) => 0,
        (Some(_), Some(_)) => 1,
        _ => 2,
    }
}

/// The roots whose scalar values the semantic diff masks, for a Secret.
///
/// `WRITE-OPS.md` §10.1: the diff of a Secret would otherwise lay the base64 out on the screen,
/// and a screen is somewhere a recording happens. Only these two roots — a Secret's `kind`,
/// `apiVersion` and `metadata.name` are not secrets, and masking them would leave a diff in
/// which nothing is legible, which is the same as no diff.
const DIFF_MASKED_ROOTS: [&str; 2] = ["data", "stringData"];

/// How many characters of a masked value the mask stands in for.
///
/// The mask is a fixed run of dots, so the length change §10.1 asks for has nowhere to live. The
/// count is enough: "32 → 48" is the only thing about a secret's length anybody acts on, and it
/// is far safer than the length itself.
const DIFF_MASK: &str = "••••••";

pub struct InspectorPanel {
    yaml_view: Entity<YamlView>,
    yaml_available: bool,
    /// Why the document could not be read, when it could not be read.
    ///
    /// This is the "读不到 ≠ 健康" channel the YAML tab was missing. The document arrives from
    /// whoever selected the row, and a failure used to arrive as the same `None` as "nothing is
    /// selected", so an RBAC denial, a broken watch, or a timeout rendered as `Select a row to
    /// inspect its YAML.` - a sentence that sends the reader to click the table again instead of
    /// naming the state they are in.
    yaml_error: Option<String>,
    /// Asks the owner to fetch the document again.
    ///
    /// The Inspector does not read YAML itself, so Retry has to leave the panel. Without a
    /// handler the control can only clear the error, which is why the shell installs one.
    yaml_reload: Option<YamlReload>,
    original: Option<String>,
    pending: Option<PendingLoad>,
    editable: bool,
    validation_error: Option<String>,
    applied_at: Option<Instant>,
    /// The channel this file's tests answer on; production uses `apply_handler`.
    #[cfg(test)]
    on_apply: Option<ApplyCallback>,
    apply_handler: Option<ApplyHandler>,
    apply_request: Option<ApplyRequest>,
    next_apply_id: u64,
    applying: bool,
    apply_error: Option<String>,
    conflict_owners: Option<Vec<String>>,
    /// A change that passed every guard and waits for a review before it can be written.
    pending_apply: Option<PendingApply>,
    /// The text the cluster last accepted. `original` stays the text the cluster reported, so
    /// Revert can always go back to it.
    applied_text: Option<String>,
    expanded_details: ExpandedDetails,
    /// Which Describe sections are open.
    ///
    /// `UI-REDESIGN.md` §3.4 makes Status the one block that is first and always open, and every
    /// other block a disclosure, so this is a set rather than a flag per section. It starts
    /// holding only Status: an SRE opens the Inspector to answer "is this thing healthy", and a
    /// panel that greets them with four collapsed sections answers a question they did not ask.
    open_sections: BTreeSet<DetailSection>,
    active_tab: usize,
    focused_tab: usize,
    tab_focus: FocusHandle,
    tabs_scroll: ScrollHandle,
    action_scroll: ScrollHandle,
    /// The Diff mode's own scroll region, so a long review is reachable without the keyboard
    /// having to leave the mode.
    review_scroll: ScrollHandle,
    /// The Diff mode's focus handle, so the mode is a real tab stop and its arrow keys scroll
    /// the review rather than falling through to the tab behind it.
    review_focus: FocusHandle,
    /// The tab panel's own handle: it holds every action the Inspector answers to, so a key that
    /// names a command reaches this panel whether or not a control currently holds focus.
    focus_handle: FocusHandle,
    /// The server's verdict on the document under review, if it was asked.
    apply_check: ApplyCheckState,
    /// Sends the document to the server for validation without storing it.
    check_handler: Option<CheckHandler>,
    problems_focus: FocusHandle,
    /// Scroll position of the capped problems list.
    problems_scroll: ScrollHandle,
    problem_cursor: usize,
    /// How many problems the editor reported in the last render.
    problem_count: usize,
    /// Watches the editor, so a parse that lands after the typing pause still reaches the panel.
    yaml_observation: Option<Subscription>,
    /// The problems the panel last rendered, so only a change asks for a repaint.
    rendered_problems: Vec<Diagnostic>,

    source: Option<Arc<dyn InspectorSource>>,
    session: InspectorSession,
    selection: Option<ObjectRef>,
    selection_target: Option<ApplyTarget>,
    /// How many rows the table had selected when it pushed [`Self::selection`].
    ///
    /// The table owns the selection; this panel owns what to say about it. It shows one object, so
    /// with several rows selected its header is about to describe one of them and say nothing
    /// about the rest. Zero and one are the same fact here — one object on screen — so only two
    /// and up change anything.
    selected_rows: usize,
    load_epoch: u64,
    describe_states: HashMap<String, LoadState<DescribeData>>,
    events_states: HashMap<String, EventsEntry>,
    describe_task: Option<Task<()>>,
    events_task: Option<Task<()>>,
    /// Watchdogs that turn a load that never answers into a retryable failure.
    describe_deadline: Option<Task<()>>,
    events_deadline: Option<Task<()>>,
    /// Bumped per request, so a late watchdog cannot expire a newer load.
    describe_token: u64,
    events_token: u64,
    describe_scroll: ScrollHandle,
    describe_focus: FocusHandle,
    describe_width: Rc<Cell<f32>>,
    /// What `InspectorSource::resolve` said about each ConfigMap and Secret the described object
    /// names, keyed by the selection's [`cache_key`] and then by `(kind, name)`.
    ///
    /// The cache is keyed rather than replaced on every selection because the Related block is
    /// re-rendered constantly — scrolling, hovering, opening a value — and one `GET` per reference
    /// per frame would be the most expensive thing in the panel. Nothing is ever refetched within a
    /// session for the same object: an absent ConfigMap does not come back without the Pod
    /// changing, and a Pod that changes is a different uid.
    reference_verdicts: HashMap<String, ReferenceVerdicts>,
    /// The in-flight resolve of the object's own references, so a second render does not start a
    /// second read.
    ///
    /// Separate from [`Self::presence_task`] because dropping a `Task` cancels it: sharing one
    /// field means whichever check ran last silently kills the other, and the panel quietly stops
    /// asking about anything.
    reference_task: Option<Task<()>>,
    /// The in-flight check of whether the selected object still exists.
    presence_task: Option<Task<()>>,
    /// The timer that asks again, so the check is a cadence rather than a per-render read.
    presence_recheck: Option<Task<()>>,
    /// Why the object on screen is gone, once the cluster has said so.
    ///
    /// `kubectl delete pod` makes the row vanish from the table and leaves the Inspector holding a
    /// complete, confident, wrong copy of it — the worst state a tool can be in, because the reader
    /// has no way to tell. `OBJECT_REPLACED_REASON` covers the other half of that failure (the name
    /// was taken by a *different* object); this covers "there is nothing there", which no amount of
    /// Reload will fix and which the panel used to keep showing as if it were fine.
    gone_reason: Option<String>,
    /// When the object was last checked, so the check is a cadence rather than a per-frame read.
    /// Whether this selection has already been asked about since it was loaded or since the last
    /// cadence tick.
    ///
    /// A flag rather than a timestamp on purpose. An earlier version gated on `Instant::elapsed`,
    /// which meant the gate ran on the wall clock while the driver that re-asks ran on the
    /// executor's clock -- two clocks for one interval, so the two disagreed and the cadence could
    /// never fire under a test clock at all. One flag, cleared by the driver, is the whole policy.
    presence_asked: bool,
    /// The width the shell actually gave the panel, measured once per render.
    ///
    /// `UI-SPEC.md` §3.4 makes a sub-320px Inspector an overlay, and the shell owns the docking
    /// decision, so the panel has to know what it was handed rather than assume. Reading it here
    /// rather than in the shell is what keeps the frame and the layout from disagreeing about the
    /// same breakpoint.
    panel_width: Rc<Cell<f32>>,
    events_scroll: UniformListScrollHandle,
    events_focus: FocusHandle,
    /// Focus handles and expansion state for the Describe value rows.
    value_focus: Rc<RefCell<ValueFocus>>,
    value_cursor: Option<String>,
    expanded_values: BTreeSet<String>,
    value_copied_at: Option<Instant>,
    /// The field managers of the object currently on screen, read once per Describe render.
    managed: Rc<ManagedFields>,

    // Metrics sampling
    metrics_source: Option<MetricsHandle>,
    metrics_probe: MetricsProbeState,
    metrics_probe_retry: Option<MetricsProbeRetry>,
    metrics_probe_task: Option<Task<()>>,
    /// Whether the Inspector is visible in the window.
    metrics_visible: bool,
    metrics_target: Option<MetricsTarget>,
    metrics: MetricsSamples,
    metrics_range_ms: i64,
    metrics_scheduler: SampleScheduler,
    metrics_task: Option<Task<()>>,
    metrics_epoch: u64,
    /// When the last sample was taken, so a restarted loop keeps the sample rate.
    metrics_last_sample: Option<Instant>,
    cpu_chart: Entity<LineChartView>,
    memory_chart: Entity<LineChartView>,
    metrics_scroll: ScrollHandle,
    metrics_focus: FocusHandle,
    /// Why the Metrics tab is missing, shown instead of a silent fallback to YAML.
    metrics_notice: Option<String>,

    copied_at: Option<Instant>,
    /// The object actions the shell has wired to this panel.
    ///
    /// `UI-REDESIGN.md` §3.4 puts an icon row in the header - logs, terminal, forward, edit,
    /// delete - and every one of those except Edit is a shell capability: the Dock owns the
    /// stream, the Forwards panel owns the sessions, and the table owns the delete with its T0-T4
    /// confirmation. The panel cannot reach any of them, so it asks.
    ///
    /// A button with no handler behind it is the worst thing a tool can draw, so an action with
    /// no handler is *not drawn at all* rather than drawn dead. The shell installs them in the
    /// assembly pass; until then the row is empty and the header is one line shorter, which is
    /// the correct thing for a control that cannot act.
    object_actions: Vec<ObjectAction>,
    /// Whether a long review diff is showing every line.
    ///
    /// `WRITE-OPS.md` §10.4 requires the folded state *not* to be remembered, so this lives in
    /// the panel rather than in a setting and is cleared when the review opens — the next change
    /// starts folded, and a reader who expanded the last one did not ask to expand this one.
    diff_expanded: bool,
    /// The objects the reader came through on the way to this one.
    ///
    /// `UI-REDESIGN.md` L3: a relationship is a way out of the object you are on, and a way out
    /// with no way back is a trap. The trail is what makes "back" mean something, and `Esc` is
    /// the chord — §9.3 says `Esc` always does something, and one level back is the smallest
    /// something that is true.
    ///
    /// It is local to the panel on purpose. The table's own selection belongs to the shell, and a
    /// panel that quietly moved it would leave the table showing one object and the panel showing
    /// another, with nothing on screen saying so.
    related_trail: Vec<ObjectRef>,
    /// How many rows the table had selected at each step of [`Self::related_trail`], so going back
    /// restores the count instead of quietly dropping it.
    ///
    /// Pushed and popped in the same two places as the trail itself, and nowhere else.
    related_rows: Vec<usize>,
    /// When the deep link was last copied, so the control can say so for as long as the other
    /// copy controls do.
    linked_at: Option<Instant>,
    /// The name a copied link carries for the cluster, when the shell has told us one.
    link_cluster: Option<String>,
}

/// One action the header's icon row can offer.
#[derive(Clone)]
pub struct ObjectAction {
    /// Stable identity, used as the element id and in the test selectors.
    pub id: &'static str,
    pub label: &'static str,
    pub icon: IconName,
    /// The verb, for the tooltip: `Open logs`, not `Logs`.
    pub tooltip: &'static str,
    /// The chord that reaches the same action, when the keymap binds one.
    pub chord: Option<Keystroke>,
    /// Whether this is the destructive action, which the design sets in `danger` ink.
    pub destructive: bool,
    run: Rc<dyn Fn(&mut App)>,
}

impl ObjectAction {
    /// Builds an action. `run` is the shell's, so the panel never has to know what opening a
    /// terminal or deleting a Pod actually involves.
    pub fn new(
        id: &'static str,
        label: &'static str,
        icon: IconName,
        tooltip: &'static str,
        chord: Option<Keystroke>,
        destructive: bool,
        run: impl Fn(&mut App) + 'static,
    ) -> Self {
        Self {
            id,
            label,
            icon,
            tooltip,
            chord,
            destructive,
            run: Rc::new(run),
        }
    }
}

impl InspectorBindingInput for Option<Entity<InspectorPanel>> {
    fn into_binding(self) -> Option<InspectorBinding> {
        self.map(|inspector| {
            InspectorBinding::new(move |update, cx| {
                inspector.update(cx, |panel, cx| match update {
                    InspectorUpdate::Selection(selection, rows) => {
                        panel.set_selected_rows(rows, cx);
                        panel.set_selection(selection, cx);
                    }
                    InspectorUpdate::Yaml(yaml) => panel.set_yaml(yaml, cx),
                });
            })
        })
    }
}

impl InspectorPanel {
    pub fn new(cx: &mut App) -> Self {
        let value_pool = (0..VALUE_FOCUS_POOL_SIZE)
            .map(|index| {
                cx.focus_handle()
                    .tab_stop(true)
                    .tab_index(VALUE_FOCUS_POOL_TAB_INDEX + index as isize)
            })
            .collect();
        Self {
            yaml_view: cx.new(YamlView::new),
            yaml_available: false,
            yaml_error: None,
            yaml_reload: None,
            original: None,
            pending: None,
            editable: false,
            validation_error: None,
            applied_at: None,
            #[cfg(test)]
            on_apply: None,
            apply_handler: None,
            apply_request: None,
            next_apply_id: 1,
            applying: false,
            apply_error: None,
            conflict_owners: None,
            pending_apply: None,
            diff_expanded: false,
            related_trail: Vec::new(),
            related_rows: Vec::new(),
            linked_at: None,
            link_cluster: None,
            applied_text: None,
            expanded_details: ExpandedDetails::default(),
            open_sections: BTreeSet::from([DetailSection::Status]),
            active_tab: DEFAULT_TAB,
            focused_tab: DEFAULT_TAB,
            tab_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(INSPECTOR_TAB_STRIP_TAB_INDEX),
            tabs_scroll: ScrollHandle::new(),
            action_scroll: ScrollHandle::new(),
            review_scroll: ScrollHandle::new(),
            review_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(REVIEW_SCROLL_TAB_INDEX),
            focus_handle: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(INSPECTOR_CONTENT_TAB_INDEX),
            apply_check: ApplyCheckState::default(),
            check_handler: None,
            problems_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(INSPECTOR_PROBLEMS_TAB_INDEX),
            problems_scroll: ScrollHandle::new(),
            problem_cursor: 0,
            problem_count: 0,
            yaml_observation: None,
            rendered_problems: Vec::new(),
            source: None,
            session: InspectorSession::default(),
            selection: None,
            selection_target: None,
            selected_rows: 1,
            load_epoch: 0,
            describe_states: HashMap::new(),
            events_states: HashMap::new(),
            describe_task: None,
            events_task: None,
            describe_deadline: None,
            events_deadline: None,
            describe_token: 0,
            events_token: 0,
            describe_scroll: ScrollHandle::new(),
            describe_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(INSPECTOR_DESCRIBE_SCROLL_TAB_INDEX),
            describe_width: Rc::new(Cell::new(0.0)),
            reference_verdicts: HashMap::new(),
            reference_task: None,
            presence_task: None,
            presence_recheck: None,
            gone_reason: None,
            presence_asked: false,
            panel_width: Rc::new(Cell::new(0.)),
            events_scroll: UniformListScrollHandle::new(),
            events_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(INSPECTOR_EVENTS_SCROLL_TAB_INDEX),
            value_focus: Rc::new(RefCell::new(ValueFocus::new(value_pool))),
            value_cursor: None,
            expanded_values: BTreeSet::new(),
            value_copied_at: None,
            managed: Rc::new(ManagedFields::default()),
            metrics_source: None,
            metrics_probe: MetricsProbeState::default(),
            metrics_probe_retry: None,
            metrics_probe_task: None,
            metrics_visible: false,
            metrics_target: None,
            metrics: MetricsSamples::default(),
            metrics_range_ms: DEFAULT_RANGE_MS,
            metrics_scheduler: SampleScheduler::default(),
            metrics_task: None,
            metrics_epoch: 0,
            metrics_last_sample: None,
            cpu_chart: cx.new(|_| LineChartView::new()),
            memory_chart: cx.new(|_| LineChartView::new()),
            metrics_scroll: ScrollHandle::new(),
            metrics_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(INSPECTOR_METRICS_SCROLL_TAB_INDEX),
            metrics_notice: None,
            copied_at: None,
            object_actions: Vec::new(),
        }
    }

    fn tab_count(&self) -> usize {
        TABS.len() + usize::from(self.metrics_tab_visible())
    }

    fn activate_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.tab_count();
        if count == 0 || index >= count {
            return;
        }
        self.focused_tab = index;
        self.tabs_scroll.scroll_to_item(tab_scroll_index(index));
        // The reason a Metrics tab could not open has been read the moment the reader picks
        // another tab. It used to survive until some unrelated command cleared it, and it lives
        // in the one status strip the panel draws — the YAML toolbar's — where it sat above
        // "Unsaved changes" on the tab a reader lands on after every object.
        self.metrics_notice = None;
        if index != InspectorTab::Metrics as usize || self.metrics_tab_visible() {
            self.active_tab = index;
        }
        self.ensure_tab_data(cx);
        self.update_metrics_sampling(cx);
        window.focus(&self.tab_focus, cx);
        cx.notify();
    }

    /// Switches tabs and loads data for the selected tab.
    pub fn show_tab(&mut self, tab: InspectorTab, cx: &mut Context<Self>) {
        let requested = tab as usize;
        if requested == InspectorTab::Metrics as usize && !self.metrics_tab_visible() {
            // The tab cannot exist for this object, so the request falls back to the reading
            // order's first tab and says why instead of appearing to do nothing.
            let reason = self.metrics_unavailable_reason();
            self.metrics_notice = Some(reason);
            self.active_tab = DEFAULT_TAB;
            self.focused_tab = DEFAULT_TAB;
            self.tabs_scroll
                .scroll_to_item(tab_scroll_index(DEFAULT_TAB));
            cx.notify();
            return;
        }
        self.metrics_notice = None;
        let index = requested;
        self.active_tab = index.min(self.tab_count().saturating_sub(1));
        self.focused_tab = self.active_tab;
        self.tabs_scroll
            .scroll_to_item(tab_scroll_index(self.active_tab));
        self.ensure_tab_data(cx);
        self.update_metrics_sampling(cx);
        cx.notify();
    }

    /// Why the Metrics tab is missing, so the fallback to YAML is never silent.
    fn metrics_unavailable_reason(&self) -> String {
        if self.metrics_source.is_none() {
            return "No cluster connection owns the metrics API, so this Inspector has no Metrics tab."
                .to_owned();
        }
        let Some(selection) = self.selection.as_ref() else {
            return "Select a Node or a Pod to see CPU and memory usage.".to_owned();
        };
        if self.metrics_target.is_none() {
            let kind = if selection.resource.kind.is_empty() {
                "This resource"
            } else {
                selection.resource.kind.as_str()
            };
            return format!("{kind} does not publish metrics. Only Nodes and Pods do.");
        }
        match &self.metrics_probe {
            MetricsProbeState::Missing => {
                "metrics-server is not installed on this cluster, so the Metrics tab is hidden."
                    .to_owned()
            }
            MetricsProbeState::Forbidden { reason } => format!(
                "The cluster denied the metrics request, so the Metrics tab is hidden: {reason}"
            ),
            MetricsProbeState::Error { reason } => {
                format!("The metrics check failed, so the Metrics tab is hidden: {reason}")
            }
            MetricsProbeState::Checking => {
                "The metrics check is still running, so the Metrics tab is hidden for now."
                    .to_owned()
            }
            MetricsProbeState::Available => {
                "The Metrics tab is hidden until the Inspector is visible.".to_owned()
            }
        }
    }

    /// Installs the object actions the header's icon row offers.
    ///
    /// The panel draws a button per action and nothing else, so an action the shell has not
    /// wired simply does not appear. See [`ObjectAction`].
    pub fn set_object_actions(&mut self, actions: Vec<ObjectAction>, cx: &mut Context<Self>) {
        self.object_actions = actions;
        cx.notify();
    }

    /// The actions currently wired, for the shell's own tests.
    pub fn object_action_ids(&self) -> Vec<&'static str> {
        self.object_actions.iter().map(|action| action.id).collect()
    }

    pub fn set_source(&mut self, source: Arc<dyn InspectorSource>) {
        let changed = self
            .source
            .as_ref()
            .is_none_or(|current| !Arc::ptr_eq(current, &source));
        if changed {
            self.session.id = self.session.id.wrapping_add(1);
            self.invalidate_session_state();
        }
        self.source = Some(source);
    }

    pub fn set_session_identity(&mut self, session: InspectorSession) {
        if self.session != session {
            self.session = session;
            self.invalidate_session_state();
        }
    }

    pub fn set_cluster_id(&mut self, cluster_id: Option<k8s_core::cluster::ClusterId>) {
        self.set_session_identity(InspectorSession {
            id: self.session.id,
            cluster_id,
        });
    }

    pub fn set_session_epoch(&mut self, session_epoch: u64) {
        self.set_session_identity(InspectorSession {
            id: session_epoch,
            cluster_id: self.session.cluster_id,
        });
    }

    pub fn session_identity(&self) -> InspectorSession {
        self.session
    }

    // Metrics

    /// Replaces the Metrics source when the session changes.
    pub fn set_metrics_source(
        &mut self,
        source: Option<MetricsHandle>,
        state: MetricsProbeState,
        cx: &mut Context<Self>,
    ) {
        let tier = source
            .as_ref()
            .map_or(k8s_core::latency::LatencyTier::Local, MetricsHandle::tier);
        self.metrics_source = source;
        self.metrics = MetricsSamples::default();
        self.metrics_scheduler = SampleScheduler::new(tier);
        self.metrics_epoch = self.metrics_epoch.wrapping_add(1);
        self.metrics_task = None;
        self.metrics_probe_task = None;
        self.metrics_last_sample = None;
        self.set_metrics_probe_state(state, cx);
        self.update_chart_data(cx);
    }

    pub fn set_metrics_probe_retry_handler(&mut self, handler: impl Fn(&mut App) + 'static) {
        self.metrics_probe_retry = Some(Rc::new(handler));
    }

    pub fn set_metrics_probe_state(&mut self, state: MetricsProbeState, cx: &mut Context<Self>) {
        self.metrics_probe = state;
        self.metrics.last_error = match &self.metrics_probe {
            MetricsProbeState::Available | MetricsProbeState::Checking => None,
            MetricsProbeState::Missing => Some(METRICS_UNAVAILABLE.to_owned()),
            MetricsProbeState::Forbidden { reason } | MetricsProbeState::Error { reason } => {
                Some(reason.clone())
            }
        };
        if !self.metrics_tab_visible() && self.active_tab == InspectorTab::Metrics as usize {
            let reason = self.metrics_unavailable_reason();
            self.active_tab = DEFAULT_TAB;
            self.focused_tab = DEFAULT_TAB;
            self.metrics_notice = Some(reason);
        }
        self.update_metrics_sampling(cx);
        cx.notify();
    }

    fn set_metrics_available(&mut self, available: bool, cx: &mut Context<Self>) {
        if !available && matches!(self.metrics_probe, MetricsProbeState::Error { .. }) {
            self.update_metrics_sampling(cx);
            return;
        }
        let state = if available {
            MetricsProbeState::Available
        } else {
            MetricsProbeState::Missing
        };
        self.set_metrics_probe_state(state, cx);
    }

    fn retry_metrics_sample(&mut self, cx: &mut Context<Self>) {
        self.metrics_epoch = self.metrics_epoch.wrapping_add(1);
        self.metrics_task = None;
        self.reset_metrics_scheduler();
        self.metrics_last_sample = None;
        self.metrics.last_error = None;
        self.update_metrics_sampling(cx);
        cx.notify();
    }

    fn retry_metrics(&mut self, cx: &mut Context<Self>) {
        if self.metrics_probe.is_available() {
            self.retry_metrics_sample(cx);
            return;
        }
        self.metrics_epoch = self.metrics_epoch.wrapping_add(1);
        self.metrics_task = None;
        self.reset_metrics_scheduler();
        self.metrics_probe = MetricsProbeState::Checking;
        self.metrics.last_error = None;
        match self.metrics_probe_retry.clone() {
            Some(retry) => retry(cx),
            // No owner for the probe, so run it here. Otherwise the panel would wait in
            // "Checking metrics availability" with no way forward.
            None => self.probe_metrics(cx),
        }
        self.update_metrics_sampling(cx);
        cx.notify();
    }

    /// Runs a one-shot metrics probe for a panel without a probe owner.
    fn probe_metrics(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.metrics_source.clone() else {
            self.set_metrics_probe_state(MetricsProbeState::Missing, cx);
            return;
        };
        let epoch = self.metrics_epoch;
        self.metrics_probe_task = Some(cx.spawn(async move |this, cx| {
            let result = source.probe_future().await;
            this.update(cx, |panel, cx| {
                if panel.metrics_epoch != epoch {
                    return;
                }
                panel.set_metrics_probe_state(MetricsProbeState::from_result(result), cx);
            })
            .ok();
        }));
    }

    /// Reloads the data behind the active tab.
    fn reload_active_tab(&mut self, cx: &mut Context<Self>) {
        match self.active_tab {
            0 => self.reload_yaml(cx),
            1 => self.ensure_describe(true, cx),
            2 => self.ensure_events(true, cx),
            3 => self.retry_metrics(cx),
            _ => {}
        }
    }

    fn reload_action(&mut self, _: &ReloadActiveTab, _window: &mut Window, cx: &mut Context<Self>) {
        self.reload_active_tab(cx);
    }

    fn metrics_retry_action(
        &mut self,
        _: &RetryMetrics,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.retry_metrics(cx);
    }

    fn set_metrics_range(&mut self, range_ms: i64, cx: &mut Context<Self>) {
        if self.metrics_range_ms == range_ms {
            return;
        }
        self.metrics_range_ms = range_ms;
        self.update_chart_data(cx);
        cx.notify();
    }

    fn range_1m(&mut self, _: &MetricsRange1m, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_metrics_range(60 * 1000, cx);
    }

    fn range_15m(&mut self, _: &MetricsRange15m, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_metrics_range(15 * 60 * 1000, cx);
    }

    fn range_1h(&mut self, _: &MetricsRange1h, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_metrics_range(60 * 60 * 1000, cx);
    }

    fn range_6h(&mut self, _: &MetricsRange6h, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_metrics_range(6 * 60 * 60 * 1000, cx);
    }

    fn range_24h(&mut self, _: &MetricsRange24h, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_metrics_range(24 * 60 * 60 * 1000, cx);
    }

    fn range_7d(&mut self, _: &MetricsRange7d, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_metrics_range(7 * 24 * 60 * 60 * 1000, cx);
    }

    /// Updates sampling when Inspector visibility changes.
    pub fn set_metrics_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.metrics_visible == visible {
            return;
        }
        self.metrics_visible = visible;
        self.update_metrics_sampling(cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn metrics_visible(&self) -> bool {
        self.metrics_visible
    }

    #[cfg(test)]
    pub(crate) fn metrics_sampling(&self) -> bool {
        self.metrics_task.is_some()
    }

    fn metrics_tab_visible(&self) -> bool {
        self.metrics_source.is_some() && self.metrics_target.is_some()
    }

    fn metrics_should_sample(&self) -> bool {
        self.metrics_tab_visible()
            && self.metrics_probe.is_available()
            && self.metrics_visible
            && self.active_tab == InspectorTab::Metrics as usize
    }

    fn reset_metrics_scheduler(&mut self) {
        let tier = self
            .metrics_source
            .as_ref()
            .map_or(k8s_core::latency::LatencyTier::Local, MetricsHandle::tier);
        self.metrics_scheduler = SampleScheduler::new(tier);
    }

    fn sync_metrics_scheduler_tier(
        &mut self,
        tier: k8s_core::latency::LatencyTier,
        cx: &mut Context<Self>,
    ) {
        if self.metrics_scheduler.interval() != SampleScheduler::interval_for(tier) {
            self.metrics_scheduler.set_tier(tier);
            self.update_chart_data(cx);
            cx.notify();
        }
    }

    fn update_metrics_sampling(&mut self, cx: &mut Context<Self>) {
        let should_sample = self.metrics_should_sample();
        self.metrics_scheduler.set_visible(should_sample);
        if !should_sample {
            self.metrics_task = None;
            return;
        }
        // Becoming visible forces one immediate sample. Drop that request when the last
        // sample is still fresh, so flipping tabs cannot flood the API server.
        if self
            .metrics_last_sample
            .is_some_and(|last| last.elapsed() < self.metrics_scheduler.interval())
        {
            let _ = self.metrics_scheduler.decide(Duration::ZERO);
        }
        if self.metrics_task.is_some() {
            return;
        }
        let (Some(source), Some(target)) =
            (self.metrics_source.clone(), self.metrics_target.clone())
        else {
            return;
        };
        let epoch = self.metrics_epoch;
        let initial_tier = source.tier();
        self.sync_metrics_scheduler_tier(initial_tier, cx);
        let mut latency = source.latency_receiver();
        self.metrics_task = Some(cx.spawn(async move |this, cx| {
            let mut last = this
                .update(cx, |panel, _| panel.metrics_last_sample)
                .ok()
                .flatten()
                .unwrap_or_else(Instant::now);
            loop {
                let current_tier = match latency.as_mut() {
                    Some(receiver) => receiver.borrow_and_update().tier(),
                    None => initial_tier,
                };
                let decision = this
                    .update(cx, |panel, cx| {
                        if panel.metrics_epoch != epoch || !panel.metrics_should_sample() {
                            return SampleDecision::Stopped;
                        }
                        panel.sync_metrics_scheduler_tier(current_tier, cx);
                        panel.metrics_scheduler.decide(last.elapsed())
                    })
                    .unwrap_or(SampleDecision::Stopped);
                match decision {
                    SampleDecision::Stopped => break,
                    SampleDecision::Wait(delay) => {
                        if let Some(receiver) = latency.as_mut() {
                            tokio::select! {
                                _ = cx.background_executor().timer(delay) => {}
                                result = receiver.changed() => {
                                    if result.is_err() {
                                        break;
                                    }
                                }
                            }
                        } else {
                            cx.background_executor().timer(delay).await;
                        }
                    }
                    SampleDecision::SampleNow => {
                        last = Instant::now();
                        let future = match &target {
                            MetricsTarget::Node { name, .. } => source.node_future(name),
                            MetricsTarget::Pod {
                                namespace, name, ..
                            } => source.pod_future(namespace, name),
                        };
                        let result = future.await;
                        let unavailable = result
                            .as_ref()
                            .err()
                            .is_some_and(|reason| reason == METRICS_UNAVAILABLE);
                        let updated = this
                            .update(cx, |panel, cx| {
                                if panel.metrics_epoch != epoch {
                                    return false;
                                }
                                panel.metrics_last_sample = Some(Instant::now());
                                panel.record_metrics_sample(result, cx);
                                if unavailable {
                                    panel.set_metrics_available(false, cx);
                                }
                                true
                            })
                            .unwrap_or(false);
                        if !updated {
                            break;
                        }
                    }
                }
            }
        }));
    }

    fn record_metrics_sample(
        &mut self,
        result: Result<SamplePayload, String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(payload) => {
                self.metrics.record(payload);
                self.metrics_scheduler.on_success();
            }
            Err(reason) => {
                self.metrics.last_error = Some(reason);
                self.metrics_scheduler.on_failure();
            }
        }
        self.update_chart_data(cx);
        cx.notify();
    }

    fn update_chart_data(&mut self, cx: &mut Context<Self>) {
        let interval_ms = MetricsSamples::interval_ms(&self.metrics_scheduler);
        let cpu = self
            .metrics
            .cpu_chart_data(self.metrics_range_ms, interval_ms);
        let memory = self
            .metrics
            .memory_chart_data(self.metrics_range_ms, interval_ms);
        let title = self.metrics_target.as_ref().map_or_else(
            || "Metrics".to_owned(),
            |target| target.kind_label().to_owned(),
        );
        self.cpu_chart.update(cx, |chart, cx| {
            chart.set_title(format!("{title} CPU"), cx);
            chart.set_data(Rc::new(cpu), cx);
        });
        self.memory_chart.update(cx, |chart, cx| {
            chart.set_title(format!("{title} memory"), cx);
            chart.set_data(Rc::new(memory), cx);
        });
    }

    pub fn set_selection(&mut self, selection: Option<InspectorSelection>, cx: &mut Context<Self>) {
        if self.yaml_view.read(cx).is_dirty() {
            self.pending = Some(PendingLoad::Selection(selection));
            self.validation_error = None;
            cx.notify();
            return;
        }
        self.load_selection(selection, cx);
    }

    /// States how many rows the table had selected when it pushed the selection on screen.
    ///
    /// The caller owns the number because the table owns the selection, and this panel owns the
    /// sentence because it is the only surface that shows one object out of many. The table's own
    /// selection bar says how many rows are selected, and the four actions that hit exactly one
    /// object refuse the selection by naming the count; without this the Inspector was the one
    /// surface that answered a different question — it showed the anchor and said nothing, so a
    /// reader with eight rows selected read a header that looked like it described all of them.
    ///
    /// Push it on every selection, not only when it grows: a stale count outlives the selection
    /// that justified it, and a banner that names rows nobody has selected any more is worse than
    /// no banner.
    pub fn set_selected_rows(&mut self, rows: usize, cx: &mut Context<Self>) {
        if self.selected_rows == rows {
            return;
        }
        self.selected_rows = rows;
        cx.notify();
    }

    pub fn set_yaml(&mut self, yaml: Option<String>, cx: &mut Context<Self>) {
        if self.yaml_view.read(cx).is_dirty() {
            self.pending = Some(PendingLoad::Yaml(yaml));
            self.validation_error = None;
            cx.notify();
            return;
        }
        self.invalidate_apply();
        self.clear_selection_state();
        self.load_yaml(yaml, cx);
        self.sync_metrics_target(cx);
    }

    /// Records that the document could not be read, and why.
    ///
    /// This is the state `InspectorUpdate::Yaml(None)` used to swallow. A failed fetch and an
    /// empty selection are different facts: one is an anomaly the reader has to know about, the
    /// other is an instruction. Rendering them the same way is what made an RBAC denial during an
    /// incident read as "you have not clicked anything yet".
    ///
    /// `object` is the object whose document failed, when the caller knows it. The selection moves
    /// to it, because the object *is* still selected: Describe, Events, and Metrics can read it,
    /// and the identity bar has to keep naming it. Only the document is missing, so the editor is
    /// emptied rather than left holding the previous object's text next to the new object's name.
    pub fn set_yaml_error(
        &mut self,
        object: Option<ObjectRef>,
        reason: String,
        cx: &mut Context<Self>,
    ) {
        if self.yaml_view.read(cx).is_dirty() {
            // The buffer holds edits that were never written, so a late failure must not replace
            // them. The next Apply or Revert clears the path back to a load.
            return;
        }
        let target = object
            .as_ref()
            .map(|object| ApplyTarget::from_object(object.clone(), Some(self.session)));
        let changed = self.selection != object || self.selection_target != target;
        if self.yaml_error.as_deref() == Some(reason.as_str()) && !changed {
            return;
        }
        if changed {
            self.load_yaml(None, cx);
            self.selection = object;
            self.selection_target = target;
            self.reset_selection_loads(cx);
            self.sync_metrics_target(cx);
        }
        self.invalidate_apply();
        // After the load, because `load_yaml` resolves every previous failure.
        self.yaml_error = Some(reason);
        cx.notify();
    }

    /// Installs the channel Retry uses to fetch the document again.
    ///
    /// The Inspector never reads YAML itself; the table hands it the text it already read, so the
    /// request has to go back out. The shell calls this with whatever re-reads the selected row.
    pub fn set_yaml_reload(&mut self, reload: impl Fn(&mut App) + 'static) {
        self.yaml_reload = Some(Rc::new(reload));
    }

    /// The reason the document could not be read, for tests and for the shell's own state.
    pub fn yaml_error(&self) -> Option<&str> {
        self.yaml_error.as_deref()
    }

    /// Asks for the document again and leaves the error state.
    ///
    /// The error clears either way: a Retry that leaves the reader in the same state is a control
    /// that does nothing. With no channel installed the panel simply returns to the empty
    /// selection, which is why the shell installs one.
    fn reload_yaml(&mut self, cx: &mut Context<Self>) {
        self.yaml_error = None;
        if let Some(reload) = self.yaml_reload.clone() {
            reload(cx);
        }
        cx.notify();
    }

    pub(crate) fn has_apply_handler(&self) -> bool {
        #[cfg(test)]
        if self.on_apply.is_some() {
            return true;
        }
        self.apply_handler.is_some()
    }

    fn apply_error_is_unknown(reason: &str) -> bool {
        let reason = reason.to_ascii_lowercase();
        reason.contains("timeout")
            || reason.contains("timed out")
            || reason.contains("gateway")
            || reason.contains("service unavailable")
            || reason.contains("hypererror")
            || reason.contains("serviceerror")
            || reason.contains("connection reset")
            || reason.contains("connection refused")
            || reason.contains("connection closed")
            || reason.contains("connection lost")
            || reason.contains("connection interrupted")
            || reason.contains("connection error")
            || reason.contains("network is unreachable")
            || reason.contains("broken pipe")
            || reason.contains("unexpected eof")
    }

    fn clear_unavailable_error(&mut self) {
        if self.apply_error.as_deref() == Some(APPLY_UNAVAILABLE_REASON) {
            self.apply_error = None;
        }
    }

    #[cfg(test)]
    pub fn set_on_apply(&mut self, callback: impl Fn(ApplyRequest) + 'static) {
        self.on_apply = Some(Box::new(callback));
        self.clear_unavailable_error();
    }
    pub fn set_apply_handler(&mut self, handler: impl Fn(ApplyRequest, &mut App) + 'static) {
        self.apply_handler = Some(Box::new(handler));
        self.clear_unavailable_error();
    }

    pub fn set_targeted_apply_handler(
        &mut self,
        handler: impl Fn(ApplyRequest, &mut App) + 'static,
    ) {
        self.set_apply_handler(handler);
    }

    pub fn set_editable(&mut self, editable: bool, window: &mut Window, cx: &mut Context<Self>) {
        let editable = editable && self.yaml_available && !self.applying;
        if self.editable != editable {
            self.editable = editable;
            self.validation_error = None;
            self.apply_error = None;
            self.conflict_owners = None;
            self.action_scroll.scroll_to_item(0);
        }
        // `Focus YAML` is the only caller that asks for an editable panel, so this is where the
        // caret goes. The command opens the Inspector and switches to this tab, and a command
        // called "Focus YAML" that leaves the caret in the table is a lie the reader only finds
        // out about when `⌘↵` previews an untouched document: every chord in §13.5 is scoped to
        // the editor, so the keys go to the row list and the document stays untyped and
        // un-previewable from the keyboard. Focusing here rather than in the command also covers
        // the paths that reach the panel without a window of their own.
        if editable {
            self.yaml_view.update(cx, |view, cx| view.focus(window, cx));
        }
        let weak = cx.weak_entity();
        let edit_weak = weak.clone();
        let apply_now_weak = weak.clone();
        self.yaml_view.update(cx, |view, cx| {
            view.set_editable(editable, cx);
            view.set_on_apply_requested(move |text, _window, cx| {
                if let Some(panel) = weak.upgrade() {
                    cx.defer(move |cx| {
                        panel.update(cx, |panel, cx| panel.apply_text(text, cx));
                    });
                }
            });
            // `WRITE-OPS.md` §7: the shifted chord writes, and only a change somebody has read.
            // The review is this panel's state, so this panel is what answers the question — an
            // open review writes, and anything else previews first, so there is no path from the
            // keyboard to a write that skipped the review.
            view.set_on_apply_now_requested(move |cx| {
                if let Some(panel) = apply_now_weak.upgrade() {
                    panel.update(cx, |panel, cx| {
                        if panel.pending_apply.is_some() {
                            panel.confirm_pending_apply(cx);
                        } else {
                            panel.apply(cx);
                        }
                    });
                }
            });
            view.set_on_edit(move |cx| {
                if let Some(panel) = edit_weak.upgrade() {
                    cx.defer(move |cx| {
                        panel.update(cx, |panel, cx| panel.clear_edit_feedback(cx));
                    });
                }
            });
            if editable {
                view.focus(window, cx);
            }
        });
        cx.notify();
    }

    pub fn is_dirty(&self, cx: &App) -> bool {
        self.yaml_view.read(cx).is_dirty()
    }

    pub fn is_applying(&self) -> bool {
        self.applying
    }

    pub fn has_yaml(&self) -> bool {
        self.yaml_available
    }

    /// Current tab for commands and tests.
    pub fn active_tab(&self) -> InspectorTab {
        match self.active_tab {
            1 => InspectorTab::Describe,
            2 => InspectorTab::Events,
            3 => InspectorTab::Metrics,
            _ => InspectorTab::Yaml,
        }
    }

    /// YAML edit state for commands and tests.
    pub fn is_editing(&self, cx: &App) -> bool {
        self.yaml_view.read(cx).is_editable()
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub fn yaml_focus_handle(&self, cx: &App) -> FocusHandle {
        self.yaml_view.read(cx).focus_handle().clone()
    }

    pub fn current_selection(&self, cx: &App) -> Option<InspectorSelection> {
        Some(InspectorSelection {
            object: self.selection.clone()?,
            yaml: self.yaml_view.read(cx).text()?,
        })
    }

    pub fn selection(&self) -> Option<&ObjectRef> {
        self.selection.as_ref()
    }

    pub fn apply_target(&self) -> Option<ApplyTarget> {
        self.current_apply_target().cloned()
    }

    /// The request this panel is acting on, or is about to.
    ///
    /// The shell drops any reply that does not belong to the request it is holding, which is the
    /// right guard for an apply and the wrong one for a check: a check belongs to a review that
    /// has not been confirmed, so before the write there was no in-flight request to name and
    /// every verdict was discarded on arrival. The review's own request answers while it is open.
    pub fn current_apply_request(&self) -> Option<ApplyRequest> {
        self.apply_request.clone().or_else(|| {
            self.pending_apply.as_ref().map(|pending| {
                ApplyRequest::new(
                    pending.request_id,
                    pending.target.clone(),
                    pending.yaml.clone(),
                )
            })
        })
    }

    pub fn pending_selection(&self) -> Option<InspectorSelection> {
        match &self.pending {
            Some(PendingLoad::Selection(selection)) => selection.clone(),
            _ => None,
        }
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
    /// Explains why Apply cannot run while a selection is deferred.
    ///
    /// The visible YAML still belongs to the previous object, so an Apply would write to
    /// an object the user already navigated away from.
    fn apply_blocked_by_pending(&self) -> Option<(&'static str, String)> {
        self.pending_selection()?;
        let current = self
            .selection
            .as_ref()
            .map_or_else(|| "the previous object".to_owned(), object_display_identity);
        Some((
            "Apply paused",
            format!(
                "The YAML still shows {current}. Cancel the changes to load the selected object."
            ),
        ))
    }

    /// What the panel is holding back, and why, for whichever of the two things it is.
    ///
    /// One sentence used to cover both, and it named the wrong one for half of them: a document
    /// the table refreshed for the object *already selected* is not a selection change, and
    /// telling a reader to "cancel before the next selection loads" about a live update of the
    /// object in front of them sent them looking for a navigation that was never going to happen.
    fn pending_reason(&self) -> (&'static str, String) {
        self.apply_blocked_by_pending().unwrap_or_else(|| {
            (
                "Unsaved changes",
                "The cluster sent a newer document for this object. Apply or cancel to see it."
                    .to_owned(),
            )
        })
    }

    /// Why Apply cannot run right now, or `None` when it can. This is what the toolbar shows as
    /// the button's tooltip, and it names the same set of states the button is disabled for: a
    /// control that is off while explaining a state the reader is not in is the defect this
    /// ordering exists to prevent.
    ///
    /// A deferred selection outranks an apply in flight, because it is the state that outlives
    /// the request and names the object the reader has to come back to. An apply in flight
    /// outranks the three states below it, because each of those asserts something false while
    /// a request is on the wire and names an action the reader cannot take: there is something
    /// to apply, the document is locked against the edit it asks for, and a request in flight
    /// means a cluster answered. "No cluster" still outranks "nothing to apply" - with nothing
    /// connected, editing is not the step that unblocks this.
    fn apply_block_reason(&self, problem_count: usize, cx: &App) -> Option<String> {
        if let Some((_, reason)) = self.apply_blocked_by_pending() {
            return Some(reason);
        }
        if self.applying {
            return Some(APPLY_IN_FLIGHT_REASON.to_owned());
        }
        if problem_count > 0 {
            return Some(APPLY_INVALID_REASON.to_owned());
        }
        if !self.has_apply_handler() {
            return Some(APPLY_UNAVAILABLE_REASON.to_owned());
        }
        if !self.yaml_view.read(cx).is_dirty() {
            return Some(APPLY_CLEAN_REASON.to_owned());
        }
        None
    }

    fn current_apply_target(&self) -> Option<&ApplyTarget> {
        self.selection_target
            .as_ref()
            .filter(|target| target.session == Some(self.session))
    }

    fn clear_selection_state(&mut self) {
        self.load_epoch = self.load_epoch.wrapping_add(1);
        self.abandon_stale_loads();
        self.describe_task = None;
        self.events_task = None;
        self.describe_deadline = None;
        self.events_deadline = None;
        self.selection = None;
        self.selection_target = None;
    }

    /// Drops cached `Loading` entries whose request can no longer complete.
    ///
    /// A load started for another selection never returns, so leaving the entry behind
    /// would block every later visit to that object.
    fn abandon_stale_loads(&mut self) {
        self.describe_states
            .retain(|_, state| !matches!(state, LoadState::Loading { .. }));
        self.events_states
            .retain(|_, entry| !matches!(entry.state, LoadState::Loading { .. }));
    }

    fn invalidate_apply(&mut self) {
        self.applying = false;
        self.apply_request = None;
        self.pending_apply = None;
    }

    fn clear_edit_feedback(&mut self, cx: &mut Context<Self>) {
        self.validation_error = None;
        self.applied_at = None;
        self.apply_error = None;
        self.conflict_owners = None;
        // Undoing back to the saved text leaves nothing to apply, so the deferred
        // selection can load right away.
        if self.has_pending() && !self.yaml_view.read(cx).is_dirty() && self.load_pending(cx) {
            return;
        }
        cx.notify();
    }

    fn invalidate_session_state(&mut self) {
        self.invalidate_apply();
        self.load_epoch = self.load_epoch.wrapping_add(1);
        self.describe_task = None;
        self.events_task = None;
        self.describe_deadline = None;
        self.events_deadline = None;
        self.describe_states.clear();
        self.events_states.clear();
        // A verdict is a fact about one cluster at one moment. Carrying it across a context switch
        // would let the new cluster's Related block inherit the old one's answers.
        self.reference_verdicts.clear();
        self.reference_task = None;
        self.presence_task = None;
        self.presence_recheck = None;
        self.gone_reason = None;
        self.presence_asked = false;
        self.pending = None;
        self.selection = None;
        self.selection_target = None;
        self.metrics_target = None;
        self.metrics = MetricsSamples::default();
        self.reset_metrics_scheduler();
        self.metrics_task = None;
        self.metrics_epoch = self.metrics_epoch.wrapping_add(1);
    }

    /// Drops the loads and the disclosure state that belonged to the previous object.
    ///
    /// Shared by the two ways the selection moves: a document arriving, and a document failing to
    /// arrive. A load started for another object never returns, so leaving it behind would block
    /// every later visit to that object.
    fn reset_selection_loads(&mut self, cx: &mut Context<Self>) {
        self.load_epoch = self.load_epoch.wrapping_add(1);
        self.abandon_stale_loads();
        self.describe_task = None;
        self.events_task = None;
        self.expanded_details = ExpandedDetails::default();
        // A new object gets the default reading order, not the last object's: a reader who
        // collapsed Spec on one Pod did not ask to see every Spec collapsed for the rest of the
        // session. `Status` is always in the set, so the block the panel exists for is never
        // the one that is missing.
        self.open_sections = BTreeSet::from([DetailSection::Status]);
        self.managed = Rc::new(ManagedFields::default());
        // A "this object was deleted" banner is a fact about one object. Carrying it onto the next
        // one would tell a reader that the Pod they just selected is gone when it is on screen.
        self.gone_reason = None;
        self.presence_asked = false;
        self.presence_task = None;
        self.presence_recheck = None;
        self.events_scroll
            .0
            .borrow_mut()
            .base_handle
            .set_offset(point(px(0.), px(0.)));
        self.ensure_tab_data(cx);
    }

    fn load_selection(&mut self, selection: Option<InspectorSelection>, cx: &mut Context<Self>) {
        let (yaml, object) = match selection {
            Some(selection) => (Some(selection.yaml), Some(selection.object)),
            None => (None, None),
        };
        let target = object
            .as_ref()
            .map(|object| ApplyTarget::from_object(object.clone(), Some(self.session)));
        let changed = self.selection != object || self.selection_target != target;
        if changed {
            self.load_yaml(yaml, cx);
            // A selection from outside ends the chain: the reader is somewhere else now, and a
            // "back" that returns to an object they left two clicks ago is a surprise, not a
            // shortcut.
            self.related_trail.clear();
            self.related_rows.clear();
        } else if self.yaml_view.read(cx).text().as_deref() != yaml.as_deref() {
            // Same object, new content: keep the caret, selection, scroll, and IME state
            // so a live update does not interrupt typing or a composition.
            self.load_yaml_content(yaml, cx);
        }
        self.selection = object;
        self.selection_target = target;
        if changed {
            self.reset_selection_loads(cx);
            // The liveness watch belongs to the selection: a new object is a new object to ask
            // about, and the first question is whether it is there at all.
            self.arm_presence_recheck(cx);
        }
        self.sync_metrics_target(cx);
        cx.notify();
    }

    /// Updates the Metrics target when the selection changes.
    fn sync_metrics_target(&mut self, cx: &mut Context<Self>) {
        let next = self.selection.as_ref().and_then(|object| {
            MetricsTarget::from_object(
                object.resource.kind.as_str(),
                object.namespace.as_deref(),
                &object.name,
                &object.uid,
            )
        });
        if self.metrics_target != next {
            self.metrics_target = next;
            self.metrics = MetricsSamples::default();
            self.reset_metrics_scheduler();
            self.metrics_epoch = self.metrics_epoch.wrapping_add(1);
            self.metrics_task = None;
            self.metrics_last_sample = None;
            self.update_chart_data(cx);
        }
        if !self.metrics_tab_visible() && self.active_tab == InspectorTab::Metrics as usize {
            let reason = self.metrics_unavailable_reason();
            self.active_tab = DEFAULT_TAB;
            self.focused_tab = DEFAULT_TAB;
            self.metrics_notice = Some(reason);
        }
        self.update_metrics_sampling(cx);
    }

    fn load_yaml(&mut self, yaml: Option<String>, cx: &mut Context<Self>) {
        self.load_yaml_content(yaml, cx);
        self.yaml_view
            .update(cx, |view, cx| view.reset_view_state(cx));
    }

    /// Replaces the document text and the panel state around it.
    fn load_yaml_content(&mut self, yaml: Option<String>, cx: &mut Context<Self>) {
        self.invalidate_apply();
        self.yaml_available = yaml.is_some();
        // A document that arrived resolves the last failure: the two states are exclusive, and
        // keeping a stale reason would put an anomaly banner over a document that is on screen.
        self.yaml_error = None;
        self.original = yaml.clone();
        self.editable = self.yaml_available;
        self.action_scroll.scroll_to_item(0);
        self.validation_error = None;
        self.applied_at = None;
        self.applied_text = None;
        self.apply_error = None;
        self.conflict_owners = None;
        self.expanded_values.clear();
        self.value_cursor = None;
        let editable = self.yaml_available;
        let weak = cx.weak_entity();
        let edit_weak = weak.clone();
        self.yaml_view.update(cx, |view, cx| {
            view.set_text(yaml, cx);
            view.set_editable(editable, cx);
            view.set_on_apply_requested(move |text, _window, cx| {
                if let Some(panel) = weak.upgrade() {
                    cx.defer(move |cx| {
                        panel.update(cx, |panel, cx| panel.apply_text(text, cx));
                    });
                }
            });
            view.set_on_edit(move |cx| {
                if let Some(panel) = edit_weak.upgrade() {
                    cx.defer(move |cx| {
                        panel.update(cx, |panel, cx| panel.clear_edit_feedback(cx));
                    });
                }
            });
        });
        cx.notify();
    }

    fn load_pending(&mut self, cx: &mut Context<Self>) -> bool {
        match self.pending.take() {
            Some(PendingLoad::Yaml(yaml)) => {
                self.clear_selection_state();
                self.load_yaml(yaml, cx);
                self.sync_metrics_target(cx);
                true
            }
            Some(PendingLoad::Selection(selection)) => {
                self.load_selection(selection, cx);
                true
            }
            None => false,
        }
    }

    pub fn discard_dirty(&mut self, cx: &mut Context<Self>) {
        self.revert(cx);
    }
    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.pending = None;
        self.invalidate_apply();
        self.clear_selection_state();
        self.load_yaml(None, cx);
        self.sync_metrics_target(cx);
        cx.notify();
    }
    // YAML apply

    pub fn apply(&mut self, cx: &mut Context<Self>) {
        if self.applying || !self.yaml_view.read(cx).is_dirty() {
            return;
        }
        let Some(text) = self.yaml_view.read(cx).text() else {
            return;
        };
        self.apply_text(text, cx);
    }

    /// Requests a review of the current text. Nothing is written from this call: the request it
    /// captures waits for [`Self::confirm_pending_apply`].
    fn apply_text(&mut self, text: String, cx: &mut Context<Self>) {
        if self.applying || !self.yaml_view.read(cx).is_dirty() {
            return;
        }
        if self.apply_blocked_by_pending().is_some() {
            // The status strip already explains the paused target.
            cx.notify();
            return;
        }
        match serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&text) {
            Ok(_) => {
                self.validation_error = None;
                self.apply_error = None;
                self.conflict_owners = None;
                self.yaml_view
                    .update(cx, |view, cx| view.clear_diagnostics(cx));
                if !self.has_apply_handler() {
                    self.apply_error = Some(APPLY_UNAVAILABLE_REASON.to_owned());
                    cx.notify();
                    return;
                }
                let Some(target) = self.current_apply_target().cloned() else {
                    self.apply_error = Some("Select a row to apply YAML.".to_owned());
                    cx.notify();
                    return;
                };
                if !target.is_complete() {
                    self.apply_error = Some(
                        "The selected object is incomplete. Select a complete object, then apply YAML."
                            .to_owned(),
                    );
                    cx.notify();
                    return;
                }
                if !yaml_matches_target(&target, &text) {
                    self.apply_error = Some(
                        "The YAML name, namespace, UID, or resource type does not match the selected object. Update the YAML or select the matching object."
                            .to_owned(),
                    );
                    cx.notify();
                    return;
                }
                let request_id = self.next_apply_id;
                self.next_apply_id = self.next_apply_id.wrapping_add(1);
                self.pending_apply = Some(PendingApply {
                    request_id,
                    target,
                    yaml: text,
                });
                // `WRITE-OPS.md` §10.4: the fold is per review, never remembered.
                self.diff_expanded = false;
                self.problem_cursor = 0;
                // The review asks the server itself. It used to offer a button, which made the
                // only check that can see a schema violation an optional step beside the write —
                // and a review of an object about to be changed, answered by a diff of text, is
                // not an answer.
                self.check_pending_apply(cx);
                cx.notify();
            }
            Err(error) => {
                let diagnostic = Diagnostic::from_yaml_error(&error);
                self.validation_error = Some(diagnostic.message.clone());
                self.applied_at = None;
                self.yaml_view
                    .update(cx, |view, cx| view.set_diagnostics(vec![diagnostic], cx));
                cx.notify();
            }
        }
    }

    /// A reviewed change waiting for the user.
    #[allow(dead_code)]
    fn reviewed_change(&self) -> Option<&PendingApply> {
        self.pending_apply.as_ref()
    }

    /// The text the cluster last accepted, which stays readable after the editor is marked saved.
    pub fn applied_text(&self) -> Option<&str> {
        self.applied_text.as_deref()
    }

    /// Writes the reviewed change. Every guard runs again here, because the text, the target, or
    /// the session can all have moved while the review was open.
    pub fn confirm_pending_apply(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_apply.take() else {
            return;
        };
        if self.applying {
            return;
        }
        if self.yaml_view.read(cx).text().as_deref() != Some(pending.yaml.as_str()) {
            self.apply_error = Some(APPLY_STALE_REVIEW_REASON.to_owned());
            cx.notify();
            return;
        }
        let Some(target) = self.current_apply_target().cloned() else {
            self.apply_error = Some("Select a row to apply YAML.".to_owned());
            cx.notify();
            return;
        };
        if target != pending.target {
            self.apply_error = Some(
                "The selected object changed. Select the object again, then apply the YAML."
                    .to_owned(),
            );
            cx.notify();
            return;
        }
        if !yaml_matches_target(&target, &pending.yaml) {
            self.apply_error = Some(
                "The YAML name, namespace, UID, or resource type does not match the selected object. Update the YAML or select the matching object."
                    .to_owned(),
            );
            cx.notify();
            return;
        }
        let request = ApplyRequest::new(pending.request_id, target, pending.yaml);
        self.apply_request = Some(request.clone());
        self.applying = true;
        self.editable = false;
        self.yaml_view
            .update(cx, |view, cx| view.set_editable(false, cx));
        let Some(handler) = &self.apply_handler else {
            // Only this file's tests leave `apply_handler` unset.
            #[cfg(test)]
            if let Some(on_apply) = &self.on_apply {
                on_apply(request.clone());
                self.mark_applied(request, cx);
            }
            return;
        };
        handler(request, cx);
        cx.notify();
    }

    /// Drops a review without writing anything.
    pub fn cancel_pending_apply(&mut self, cx: &mut Context<Self>) {
        if self.pending_apply.take().is_none() {
            return;
        }
        // A verdict belongs to the document it checked, so it does not outlive the review.
        self.apply_check = ApplyCheckState::default();
        cx.notify();
    }

    /// Asks the API server to validate the document under review without storing it.
    ///
    /// The review runs this itself when it opens; it is a separate entry point only because the
    /// shell owns the transport. It never blocks the apply and it never writes, so it answers a
    /// question the local diff cannot — would the server accept this document at all — and the
    /// reader does not have to know it is there to get it.
    ///
    /// The shell owns the request, exactly as it owns the apply, and reports the outcome back
    /// through [`Self::apply_check_finished`].
    pub fn check_pending_apply(&mut self, cx: &mut Context<Self>) {
        self.apply_check = ApplyCheckState::Running;
        let Some(pending) = self.pending_apply.clone() else {
            return;
        };
        let Some(handler) = self.check_handler.clone() else {
            // No session to ask. Saying so beats a line that claims a verdict nobody produced.
            self.apply_check = ApplyCheckState::Failed {
                reason: "No cluster connection to validate this document against.".to_owned(),
            };
            cx.notify();
            return;
        };
        let request = ApplyRequest::new(pending.request_id, pending.target, pending.yaml);
        cx.notify();
        handler(request, cx);
    }

    /// Installs the transport that sends a document to the server for validation.
    pub fn set_targeted_check_handler(
        &mut self,
        handler: impl Fn(ApplyRequest, &mut App) + 'static,
    ) {
        self.check_handler = Some(std::rc::Rc::new(handler));
    }

    /// Records the server's verdict.
    pub fn apply_check_finished(
        &mut self,
        result: Result<ApplyVerdict, String>,
        cx: &mut Context<Self>,
    ) {
        self.apply_check = match result {
            Ok(ApplyVerdict::Valid) => ApplyCheckState::Valid,
            Ok(ApplyVerdict::Conflict { owners }) => ApplyCheckState::Conflict { owners },
            Err(reason) => ApplyCheckState::Failed { reason },
        };
        cx.notify();
    }

    fn confirm_action(&mut self, _: &ConfirmApply, _window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_pending_apply(cx);
    }

    /// Keys the panel itself answers, on the frame rather than on the content.
    ///
    /// `Esc` is *not* here: it is bound to [`CancelApplyReview`] in the `Inspector` context, and
    /// a keystroke's bindings are dispatched before any element's `on_key_down`, so a raw handler
    /// on this frame would never see it. `UI-SPEC.md` §9.3's "Esc always does something" is met
    /// in the action, which is the one place `Esc` actually arrives.
    fn frame_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if !keystroke.modifiers.secondary()
            || keystroke.modifiers.control
            || keystroke.modifiers.alt
        {
            return;
        }
        // `UI-SPEC.md` §4.19: the link. It is the one chord that only means one thing, so it has no
        // shift form to be confused with.
        if keystroke.key == "l" && !keystroke.modifiers.shift && self.selection.is_some() {
            self.copy_link(cx);
            cx.stop_propagation();
            return;
        }
        // `WRITE-OPS.md` §3: `⌘↵` previews and `⌘⇧↵` writes. The editor answers both itself, but
        // only while the caret is in the document — and a reader who has just clicked a tab or
        // the header is *not* in the document, so the two chords used to go dead exactly when the
        // reader was looking at the review button they would have used instead. Reaching them
        // from anywhere inside the panel is the same action, so the frame answers them too.
        if keystroke.key == "enter" && self.active_tab == 0 {
            let shift = keystroke.modifiers.shift;
            if self.pending_apply.is_some() {
                if shift {
                    self.confirm_pending_apply(cx);
                    cx.stop_propagation();
                }
            } else {
                self.apply(cx);
                cx.stop_propagation();
            }
        }
    }

    /// `Esc`, which the keymap binds to this action in the `Inspector` context.
    ///
    /// It is an action rather than a key handler for a reason that is easy to get wrong: a
    /// keystroke's *bindings* are dispatched before any `on_key_down` on the element tree, so a
    /// handler that only listens for the raw key never sees `Esc` at all on a panel whose context
    /// already binds it. `UI-SPEC.md` §9.3 says `Esc` always does something, and the way to
    /// guarantee that on a panel the app already binds is to answer the action.
    ///
    /// Three states, nearest first: a review open, then the relationship trail, then nothing —
    /// where the editor's own find panel and the shell's overlays get their turn.
    fn cancel_review_action(
        &mut self,
        _: &CancelApplyReview,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pending_apply.is_some() {
            self.cancel_pending_apply(cx);
            return;
        }
        if self.go_back(cx) {
            return;
        }
        cx.notify();
    }

    fn revert_action(&mut self, _: &RevertYaml, _window: &mut Window, cx: &mut Context<Self>) {
        self.revert(cx);
    }

    fn copy_yaml_action(&mut self, _: &CopyYaml, _window: &mut Window, cx: &mut Context<Self>) {
        self.copy_yaml(cx);
    }

    fn toggle_value_action(
        &mut self,
        _: &ToggleValueExpansion,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selector) = self.value_cursor.clone() else {
            return;
        };
        self.toggle_value_expansion(&selector, cx);
    }

    fn copy_value_action(&mut self, _: &CopyValue, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(selector) = self.value_cursor.clone() else {
            return;
        };
        self.copy_value(&selector, cx);
    }

    fn next_problem_action(
        &mut self,
        _: &NextProblem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_problem_cursor(1);
        // The problems list is capped, so the next problem follows the cursor into view.
        self.problems_scroll.scroll_to_item(self.problem_cursor);
        self.jump_to_problem(window, cx);
    }

    fn release_apply_lock(&mut self, cx: &mut Context<Self>) {
        self.applying = false;
        let editable = self.yaml_available;
        self.editable = editable;
        self.yaml_view
            .update(cx, |view, cx| view.set_editable(editable, cx));
    }

    fn mark_applied(&mut self, request: ApplyRequest, cx: &mut Context<Self>) {
        if self.current_apply_target() != Some(&request.target) {
            self.apply_request = None;
            self.release_apply_lock(cx);
            self.apply_error = Some(
                "The selected object changed. Select the object again, then apply the YAML."
                    .to_owned(),
            );
            cx.notify();
            return;
        }
        if self.yaml_view.read(cx).text().as_deref() != Some(request.yaml.as_str()) {
            self.apply_request = None;
            self.release_apply_lock(cx);
            self.apply_error = Some(
                "The YAML changed during the apply operation. Review the YAML, then apply again."
                    .to_owned(),
            );
            cx.notify();
            return;
        }
        self.apply_request = None;
        self.yaml_view
            .update(cx, |view, cx| view.clear_diagnostics(cx));
        self.yaml_view.update(cx, |view, cx| view.mark_saved(cx));
        // `original` stays the text the cluster reported, so Revert can still go back to it. The
        // text that was applied is kept beside it instead of overwriting it.
        self.applied_text = Some(request.yaml);
        self.validation_error = None;
        self.apply_error = None;
        self.conflict_owners = None;
        self.applied_at = Some(Instant::now());
        self.release_apply_lock(cx);
        self.action_scroll.scroll_to_item(0);
        if !self.load_pending(cx) {
            cx.notify();
        }
    }

    pub fn apply_finished_for(
        &mut self,
        request: ApplyRequest,
        result: Result<ApplyOutcome, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.apply_request.as_ref() else {
            return;
        };
        if active != &request || self.current_apply_target() != Some(&request.target) {
            return;
        }
        match result {
            Ok(ApplyOutcome::Applied(object))
                if !&request.target.matches_object(object.as_ref()) =>
            {
                self.apply_request = None;
                self.release_apply_lock(cx);
                self.apply_error = Some(
                    "The apply result does not match the selected object. Refresh the object, then apply again."
                        .to_owned(),
                );
                cx.notify();
            }
            Ok(ApplyOutcome::Applied(_)) => self.mark_applied(request, cx),
            Ok(ApplyOutcome::Conflict { owners }) => {
                self.apply_request = None;
                self.release_apply_lock(cx);
                self.conflict_owners = Some(owners);
                self.apply_error = None;
                cx.notify();
            }
            Ok(ApplyOutcome::Unknown { .. }) => {
                self.apply_request = None;
                self.release_apply_lock(cx);
                self.apply_error = Some(APPLY_UNKNOWN_REASON.to_owned());
                self.conflict_owners = None;
                cx.notify();
            }
            Err(reason) => {
                self.apply_request = None;
                self.release_apply_lock(cx);
                self.apply_error = Some(if Self::apply_error_is_unknown(&reason) {
                    APPLY_UNKNOWN_REASON.to_owned()
                } else {
                    reason
                });
                self.conflict_owners = None;
                cx.notify();
            }
        }
    }
    /// Restores the text the cluster last reported.
    ///
    /// This is the only way back after an apply, so it stays available while the editor holds
    /// applied text rather than the text the server sent.
    pub fn revert(&mut self, cx: &mut Context<Self>) {
        self.invalidate_apply();
        if self.load_pending(cx) {
            return;
        }
        let original = self.original.clone();
        self.load_yaml(original, cx);
    }

    // Describe and Events loading

    fn ensure_tab_data(&mut self, cx: &mut Context<Self>) {
        match self.active_tab {
            1 => self.ensure_describe(false, cx),
            2 => self.ensure_events(false, cx),
            3 => self.update_metrics_sampling(cx),
            _ => {}
        }
    }

    fn selection_uid(&self) -> Option<String> {
        self.selection.as_ref().map(cache_key)
    }

    fn ensure_describe(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.clone() else {
            return;
        };
        // The cache is keyed by [`cache_key`] and the identity check is keyed by the uid, and
        // they differ only for an object the panel reached by name: the key has to tell two
        // Nodes apart, and the check has nothing to compare against.
        let key = cache_key(&selection);
        let expected = selection.uid.clone();
        if !force
            && matches!(
                self.describe_states.get(&key),
                Some(LoadState::Loading { .. } | LoadState::Ready(_))
            )
        {
            // The object is already here or on its way. If it is here, its references and its own
            // existence can be settled now; if it is on its way, the completion below re-runs both.
            self.ensure_references(cx);
            self.ensure_presence(force, cx);
            return;
        }
        // Describe shows the newest events, so it reads the same cache the Events tab reads. One
        // request serves both, and the "+N more" count can no longer disagree with that tab.
        self.ensure_events(false, cx);
        // The Related block's ConfigMap and Secret rows need the object before they can be
        // resolved, and it arrives with the Describe read — so both this and the completion below
        // have to ask. Neither can be the only one: this runs before the read has an entry to be
        // Ready in, and the completion runs only when the read succeeds.
        self.ensure_presence(force, cx);
        let Some(source) = self.source.clone() else {
            self.describe_states.insert(
                key,
                LoadState::Failed(common::NOT_CONNECTED_REASON.to_owned()),
            );
            cx.notify();
            return;
        };
        let task: OpsFuture<DescribeData> = source.describe(&selection);
        self.describe_states.insert(
            key.clone(),
            LoadState::Loading {
                since: Instant::now(),
            },
        );
        self.arm_load_deadline(LoadKind::Describe, key.clone(), cx);
        let epoch = self.load_epoch;
        self.describe_task = Some(cx.spawn(async move |this, cx| {
            let result = task
                .await
                .and_then(|data| prepare_describe_data(data, &expected));
            this.update(cx, |panel, cx| {
                panel.on_loaded(&epoch, result, &mut |panel, result| {
                    panel.describe_states.insert(key.clone(), result);
                });
                // The object has arrived, so its references can be resolved and its own existence
                // checked. A failed read reaches here too, and both no-op on it: there is no object
                // to resolve references of, and whether a read that failed says the object is gone
                // is not a question this can answer.
                panel.ensure_references(cx);
                // Deliberately no presence check here. This read just answered whether the object
                // exists -- a success proves it does, and a failure is not evidence that it is
                // gone -- so asking again would spend a second `GET` per selection to learn nothing
                // the first one had not already said. The cadence in `arm_presence_recheck` is what
                // notices a deletion that happens *after* this read, which is the case that matters.
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn ensure_events(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.clone() else {
            return;
        };
        let uid = cache_key(&selection);
        if !force {
            match self.events_states.get(&uid) {
                // A request that has not answered is not a reason to send another one, and a
                // request that *failed* is not a reason to send another one either. The tab has a
                // `Retry` for that (`UI-SPEC.md` §4.15: the next step is offered, not taken), and a
                // panel that re-asked every time the reader came back would hammer a broken
                // endpoint and make `Retry` indistinguishable from simply looking again. A
                // different object has a different key, so arriving somewhere new still loads.
                Some(entry)
                    if matches!(
                        entry.state,
                        LoadState::Loading { .. } | LoadState::Failed(_)
                    ) =>
                {
                    return;
                }
                Some(entry)
                    if matches!(entry.state, LoadState::Ready(_))
                        && entry.fetched_at.elapsed() < EVENTS_TTL =>
                {
                    return;
                }
                _ => {}
            }
        }
        let Some(source) = self.source.clone() else {
            self.events_states.insert(
                uid,
                EventsEntry {
                    fetched_at: Instant::now(),
                    state: LoadState::Failed(common::NOT_CONNECTED_REASON.to_owned()),
                },
            );
            cx.notify();
            return;
        };
        let task: OpsFuture<Vec<DynamicObject>> = source.events(&selection);
        self.events_states.insert(
            uid.clone(),
            EventsEntry {
                fetched_at: Instant::now(),
                state: LoadState::Loading {
                    since: Instant::now(),
                },
            },
        );
        self.arm_load_deadline(LoadKind::Events, uid.clone(), cx);
        let epoch = self.load_epoch;
        self.events_task = Some(cx.spawn(async move |this, cx| {
            let result = task
                .await
                .map(|events| Arc::new(events_newest_first(events)));
            this.update(cx, |panel, cx| {
                panel.on_loaded(&epoch, result, &mut |panel, result| {
                    panel.events_states.insert(
                        uid.clone(),
                        EventsEntry {
                            fetched_at: Instant::now(),
                            state: result,
                        },
                    );
                });
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Asks the cluster whether each ConfigMap and Secret the current object names is there.
    ///
    /// One `GET` per reference, issued once per selection, and only for the ones still unanswered.
    /// A source that does not implement [`InspectorSource::resolve`] answers `Unknown` for all of
    /// them, which lands on the event-text path — so an offline session and a source that never
    /// learned the query both draw exactly what they drew before this existed.
    fn ensure_references(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.clone() else {
            return;
        };
        let key = cache_key(&selection);
        let Some(LoadState::Ready(data)) = self.describe_states.get(&key) else {
            // Nothing to ask about until the object itself has arrived, and the object arrives
            // with the candidates on it.
            return;
        };
        let Some(source) = self.source.clone() else {
            return;
        };
        let namespace = selection.namespace.clone();
        let candidates = referenced_objects(&data.object);
        let asked = self.reference_verdicts.entry(key.clone()).or_default();
        let pending: Vec<ObjectRef> = candidates
            .into_iter()
            .filter(|(kind, name)| !asked.contains_key(&(kind.clone(), name.clone())))
            // A kind outside the relationship table has no `ApiResource`, so there is no `GET` to
            // issue for it. It keeps falling back to the event path, which is the right answer for
            // a kind the panel cannot name.
            .filter_map(|(kind, name)| relationship_ref(&kind, &name, namespace.as_deref(), ""))
            .collect();
        if pending.is_empty() {
            return;
        }
        // Marked asked before the task runs, so a second render in the same frame does not start a
        // second read for the same reference.
        for object in &pending {
            asked.insert(
                (object.resource.kind.clone(), object.name.clone()),
                ResolveOutcome::Unknown,
            );
        }
        let epoch = self.load_epoch;
        self.reference_task = Some(cx.spawn(async move |this, cx| {
            for object in pending {
                let verdict = source
                    .resolve(&object)
                    .await
                    // A failed read is not an answer about the object. Mapping it to `Unknown`
                    // rather than to an error keeps the fallback on the event path instead of
                    // replacing a quiet row with a loud one.
                    .unwrap_or(ResolveOutcome::Unknown);
                this.update(cx, |panel, cx| {
                    if panel.load_epoch != epoch {
                        return;
                    }
                    if let Some(asked) = panel.reference_verdicts.get_mut(&key) {
                        asked.insert((object.resource.kind.clone(), object.name.clone()), verdict);
                    }
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    /// Asks whether the object on screen is still there, at most once per [`PRESENCE_TTL`].
    ///
    /// This is the same capability the Related block uses, pointed at the selection instead of at
    /// its references, and it is the only signal available to a panel whose row has just vanished
    /// from the table behind it. Three states, and the discipline of `ResolveOutcome` is what keeps
    /// it honest:
    ///
    /// - `Missing` is the only answer that raises the banner. The API server answered, so this is a
    ///   fact about the cluster.
    /// - `Found` clears it — including when the uid differs from the one the panel holds, which is
    ///   `OBJECT_REPLACED_REASON`'s case and is already reported by `prepare_describe_data` on the
    ///   next read, so the banner does not need a second vocabulary for it.
    /// - `Unknown` changes nothing. A dropped connection, a refused verb, or a source that does
    ///   not implement the query all land here, and none of them is evidence that the object is
    ///   gone. A banner that appeared because the network hiccuped would be worse than none: it
    ///   teaches the reader that the strip is noise.
    ///
    /// The cadence is armed by [`Self::arm_presence_recheck`] rather than by a render: nothing else
    /// in the panel runs on a timer while the reader just sits on a healthy object, so a check that
    /// only ran on tab switches would leave the panel lying about a deleted object until the reader
    /// happened to switch tabs — which is the defect this exists to fix.
    ///
    /// `force` is what the cadence tick passes, because asking again is the entire reason it woke
    /// up. Every other caller leaves it alone, so switching to a tab the reader has already looked
    /// at costs no request.
    fn ensure_presence(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.clone() else {
            return;
        };
        if self.presence_asked && !force {
            return;
        }
        let Some(source) = self.source.clone() else {
            return;
        };
        self.presence_asked = true;
        let epoch = self.load_epoch;
        self.presence_task = Some(cx.spawn(async move |this, cx| {
            let outcome = source.resolve(&selection).await;
            this.update(cx, |panel, cx| {
                if panel.load_epoch != epoch {
                    return;
                }
                panel.gone_reason = match outcome {
                    Ok(ResolveOutcome::Missing) => Some(object_gone_reason(
                        selection.resource.kind.as_str(),
                        &selection.name,
                    )),
                    // A read that failed, and a source that cannot answer, are both silence.
                    _ => None,
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Fails a load that never answers, so the tab offers Retry again.
    ///
    /// A `Loading` entry is cached so a second visit does not start a duplicate request,
    /// which means a request that never resolves would pin the tab in a spinner for the
    /// whole session. The token check keeps a late watchdog from expiring a newer request.
    fn arm_load_deadline(&mut self, kind: LoadKind, uid: String, cx: &mut Context<Self>) {
        let epoch = self.load_epoch;
        let token = match kind {
            LoadKind::Describe => {
                self.describe_token = self.describe_token.wrapping_add(1);
                self.describe_token
            }
            LoadKind::Events => {
                self.events_token = self.events_token.wrapping_add(1);
                self.events_token
            }
        };
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LOAD_DEADLINE).await;
            this.update(cx, |panel, cx| {
                if panel.load_epoch != epoch || panel.load_token(kind) != token {
                    return;
                }
                if !panel.still_loading(kind, &uid) {
                    return;
                }
                // The request is dropped, so Retry starts a new one.
                match kind {
                    LoadKind::Describe => {
                        panel.describe_task = None;
                        panel.describe_states.insert(
                            uid.clone(),
                            LoadState::Failed(LOAD_TIMEOUT_REASON.to_owned()),
                        );
                    }
                    LoadKind::Events => {
                        panel.events_task = None;
                        panel.events_states.insert(
                            uid.clone(),
                            EventsEntry {
                                fetched_at: Instant::now(),
                                state: LoadState::Failed(LOAD_TIMEOUT_REASON.to_owned()),
                            },
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        });
        match kind {
            LoadKind::Describe => self.describe_deadline = Some(task),
            LoadKind::Events => self.events_deadline = Some(task),
        }
    }

    fn load_token(&self, kind: LoadKind) -> u64 {
        match kind {
            LoadKind::Describe => self.describe_token,
            LoadKind::Events => self.events_token,
        }
    }

    fn still_loading(&self, kind: LoadKind, uid: &str) -> bool {
        match kind {
            LoadKind::Describe => matches!(
                self.describe_states.get(uid),
                Some(LoadState::Loading { .. })
            ),
            LoadKind::Events => self
                .events_states
                .get(uid)
                .is_some_and(|entry| matches!(entry.state, LoadState::Loading { .. })),
        }
    }

    /// Discards stale results and stores current data.
    fn on_loaded<T>(
        &mut self,
        epoch: &u64,
        result: Result<T, String>,
        store: &mut dyn FnMut(&mut Self, LoadState<T>),
    ) {
        if *epoch != self.load_epoch {
            return;
        }
        let state = match result {
            Ok(value) => LoadState::Ready(value),
            Err(reason) => LoadState::Failed(reason),
        };
        store(self, state);
        self.prune_caches();
    }

    fn prune_caches(&mut self) {
        let current = self.selection_uid();
        if self.describe_states.len() > CACHE_CAPACITY {
            self.describe_states
                .retain(|uid, _| current.as_deref() == Some(uid.as_str()));
        }
        if self.events_states.len() > CACHE_CAPACITY {
            self.events_states
                .retain(|uid, _| current.as_deref() == Some(uid.as_str()));
        }
        if self.reference_verdicts.len() > CACHE_CAPACITY {
            self.reference_verdicts
                .retain(|uid, _| current.as_deref() == Some(uid.as_str()));
        }
    }

    fn copied(&self) -> bool {
        self.copied_at
            .is_some_and(|at| at.elapsed() < COPIED_FEEDBACK)
    }

    // Keyboard scrolling

    /// Scrolls a scroll container by a distance, and reports whether it moved.
    ///
    /// The offset is clamped to the content, so a key at either end changes nothing and stays
    /// available to the rest of the app instead of being swallowed.
    ///
    /// A GPUI scroll offset is the distance from the top of the content to the top of the
    /// viewport, so it grows more negative as the view moves down: a scroll towards the end of
    /// the content passes a negative distance.
    fn scroll_by(&self, handle: &ScrollHandle, distance: f32) -> bool {
        let offset = handle.offset();
        let limit = f32::from(handle.max_offset().y);
        if limit <= 0.0 {
            return false;
        }
        let next = px((f32::from(offset.y) + distance).clamp(-limit, 0.0));
        if (next - offset.y).abs() < px(0.5) {
            return false;
        }
        handle.set_offset(point(offset.x, next));
        true
    }

    /// Scrolls a uniform list by a distance, with the same clamping and sign as
    /// [`Self::scroll_by`].
    fn scroll_list_by(&self, handle: &UniformListScrollHandle, distance: f32) -> bool {
        let mut state = handle.0.borrow_mut();
        // A pending scroll_to_item would win over the offset on the next prepaint.
        state.deferred_scroll_to_item = None;
        let offset = state.base_handle.offset();
        let limit = f32::from(state.base_handle.max_offset().y);
        if limit <= 0.0 {
            return false;
        }
        let next = px((f32::from(offset.y) + distance).clamp(-limit, 0.0));
        if (next - offset.y).abs() < px(0.5) {
            return false;
        }
        state.base_handle.set_offset(point(offset.x, next));
        true
    }

    /// Height of a scroll viewport, so a page key moves a page instead of a guess.
    fn viewport_height(handle: &ScrollHandle) -> f32 {
        let height = f32::from(handle.bounds().size.height);
        if height.is_finite() && height > 0.0 {
            height
        } else {
            f32::from(design::size::ROW) * 8.
        }
    }

    /// Height of the events viewport, taken from the handle the list renders through.
    fn viewport_height_events(handle: &UniformListScrollHandle) -> f32 {
        let height = f32::from(handle.0.borrow().base_handle.bounds().size.height);
        if height.is_finite() && height > 0.0 {
            height
        } else {
            f32::from(design::size::ROW) * 8.
        }
    }

    /// Distance one arrow key moves: one row, so a line of text follows the key.
    fn scroll_step() -> f32 {
        f32::from(design::size::ROW)
    }

    fn scroll_handle_to_top(handle: &ScrollHandle) -> bool {
        let offset = handle.offset();
        if offset.y == px(0.) {
            return false;
        }
        handle.set_offset(point(offset.x, px(0.)));
        true
    }

    fn scroll_handle_to_bottom(handle: &ScrollHandle) -> bool {
        if handle.max_offset().y <= px(0.) {
            return false;
        }
        handle.scroll_to_bottom();
        true
    }

    fn scroll_list_to_top(handle: &UniformListScrollHandle) -> bool {
        let mut state = handle.0.borrow_mut();
        state.deferred_scroll_to_item = None;
        let offset = state.base_handle.offset();
        if offset.y == px(0.) {
            return false;
        }
        state.base_handle.set_offset(point(offset.x, px(0.)));
        true
    }

    fn scroll_list_to_bottom(handle: &UniformListScrollHandle) -> bool {
        let mut state = handle.0.borrow_mut();
        state.deferred_scroll_to_item = None;
        if state.base_handle.max_offset().y <= px(0.) {
            return false;
        }
        state.base_handle.scroll_to_bottom();
        true
    }

    fn on_describe_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform
        {
            return;
        }
        let step = Self::scroll_step();
        let page = Self::viewport_height(&self.describe_scroll);
        let handle = self.describe_scroll.clone();
        let moved = match event.keystroke.key.as_str() {
            "up" => self.scroll_by(&handle, step),
            "down" => self.scroll_by(&handle, -step),
            "pageup" => self.scroll_by(&handle, page),
            "pagedown" => self.scroll_by(&handle, -page),
            "home" => Self::scroll_handle_to_top(&handle),
            "end" => Self::scroll_handle_to_bottom(&handle),
            _ => return,
        };
        if moved {
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn on_events_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform
        {
            return;
        }
        let step = Self::scroll_step();
        let page = Self::viewport_height_events(&self.events_scroll);
        let handle = self.events_scroll.clone();
        let moved = match event.keystroke.key.as_str() {
            "up" => self.scroll_list_by(&handle, step),
            "down" => self.scroll_list_by(&handle, -step),
            "pageup" => self.scroll_list_by(&handle, page),
            "pagedown" => self.scroll_list_by(&handle, -page),
            "home" => Self::scroll_list_to_top(&handle),
            "end" => Self::scroll_list_to_bottom(&handle),
            _ => return,
        };
        if moved {
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn on_metrics_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform
        {
            return;
        }
        let step = Self::scroll_step();
        let page = Self::viewport_height(&self.metrics_scroll);
        let handle = self.metrics_scroll.clone();
        let moved = match event.keystroke.key.as_str() {
            "up" => self.scroll_by(&handle, step),
            "down" => self.scroll_by(&handle, -step),
            "pageup" => self.scroll_by(&handle, page),
            "pagedown" => self.scroll_by(&handle, -page),
            "home" => Self::scroll_handle_to_top(&handle),
            "end" => Self::scroll_handle_to_bottom(&handle),
            _ => return,
        };
        if moved {
            cx.stop_propagation();
            cx.notify();
        }
    }

    // Value rows

    /// Whether a section may be closed at all. Status may not.
    fn section_is_collapsible(&self, detail: DetailSection) -> bool {
        detail != DetailSection::Status
    }

    /// Whether the panel is so narrow that it cannot read as a column.
    ///
    /// The panel's *own* width, and only its own width. An unmeasured panel reports zero, which is
    /// *not* narrow: the first frame has no width yet, and painting a docked column as an overlay
    /// for one frame would flash a shadow and a corner radius on every open.
    fn overlay_frame(&self) -> bool {
        let width = self.panel_width.get();
        width > 0.0 && width < INSPECTOR_OVERLAY_BELOW
    }

    /// Whether the shell has floated this panel over the centre.
    ///
    /// `shell/mod.rs` decides that from the window width alone — below
    /// [`design::size::INSPECTOR_FLOAT_BELOW`] the Inspector is laid over the centre's right edge
    /// instead of beside it — and the frame it lays over paints nothing at all: no surface, no
    /// radius, no rule, no shadow. So the surface is this panel's to own, and it can only be owned
    /// by reading the same input the shell read. A panel-width rule could not do it: the shell
    /// gives a floating Inspector its resting width, which is wider than any threshold that also
    /// leaves a docked panel looking like a card.
    ///
    /// A viewport that has not been measured yet reports zero and is *not* narrow, so the first
    /// frame after a resize is the docked look for one paint rather than a card over the table.
    fn floating_frame(&self, window_width: f32) -> bool {
        window_width > 0.0 && window_width < design::size::INSPECTOR_FLOAT_BELOW
    }

    /// How long the Describe fetch for the current selection has been outstanding.
    ///
    /// The load state records when the entry was written, and `UI-SPEC.md` §4.14 grades the
    /// *waiting* on it, so the panel has to know the elapsed time rather than only the state.
    /// A load that has not started reports zero, which is the tier that says a fast operation is
    /// not slower than it is.
    fn describe_wait(&self) -> Duration {
        self.selection_uid()
            .and_then(|uid| self.describe_states.get(&uid))
            .map(LoadState::elapsed)
            .unwrap_or_default()
    }

    /// How long the Events fetch for the current selection has been outstanding.
    fn events_wait(&self) -> Duration {
        self.selection_uid()
            .and_then(|uid| self.events_states.get(&uid))
            .map(|entry| match &entry.state {
                LoadState::Loading { since } => since.elapsed(),
                _ => Duration::ZERO,
            })
            .unwrap_or_default()
    }

    /// Whether a value row is expanded, for commands and tests.
    #[allow(dead_code)]
    fn value_is_expanded(&self, selector: &str) -> bool {
        self.expanded_values.contains(selector)
    }

    /// The focus handle a value row was given in the last render.
    #[allow(dead_code)]
    fn value_focus_handle(&self, selector: &str) -> Option<FocusHandle> {
        self.value_focus.borrow().get(selector)
    }

    fn toggle_value_expansion(&mut self, selector: &str, cx: &mut Context<Self>) {
        self.value_cursor = Some(selector.to_owned());
        if !self.expanded_values.remove(selector) {
            self.expanded_values.insert(selector.to_owned());
        }
        cx.notify();
    }

    /// Copies a value row in full, which is what a truncated row hides.
    fn copy_value(&mut self, selector: &str, cx: &mut Context<Self>) {
        self.value_cursor = Some(selector.to_owned());
        let Some(value) = self.value_text(selector) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(value));
        self.value_copied_at = Some(Instant::now());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FEEDBACK).await;
            this.update(cx, |panel, cx| {
                panel.value_copied_at = None;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The full text of a value row, recorded while the body rendered it.
    fn value_text(&self, selector: &str) -> Option<String> {
        self.value_focus.borrow().text(selector)
    }

    /// Whether the copy feedback of a value row is still showing.
    #[allow(dead_code)]
    fn value_copied(&self, selector: &str) -> bool {
        self.value_cursor.as_deref() == Some(selector)
            && self
                .value_copied_at
                .is_some_and(|at| at.elapsed() < COPIED_FEEDBACK)
    }

    // YAML problems

    /// The parse problems the editor reports, which gate Apply.
    fn diagnostics(&self, cx: &App) -> Vec<Diagnostic> {
        self.yaml_view.read(cx).diagnostics().to_vec()
    }

    /// Whether the editor's problems differ from the ones the panel last rendered.
    ///
    /// The editor validates after a typing pause, so the problems arrive without an edit. The
    /// panel owns the problems list and the disabled Apply button, so it has to hear about that
    /// parse; the comparison keeps a caret blink or a scroll from repainting the whole panel.
    fn problems_changed(&mut self, current: &[Diagnostic]) -> bool {
        if self.rendered_problems == current {
            return false;
        }
        self.rendered_problems = current.to_vec();
        true
    }

    fn move_problem_cursor(&mut self, delta: isize) {
        let count = self.problem_count;
        if count == 0 {
            return;
        }
        let last = count - 1;
        let next = (self.problem_cursor as isize + delta).clamp(0, last as isize);
        self.problem_cursor = next as usize;
    }

    /// Reveals the problem under the cursor and puts the caret in the editor.
    fn jump_to_problem(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let index = self
            .problem_cursor
            .min(self.problem_count.saturating_sub(1));
        let Some(diagnostic) = self.yaml_view.read(cx).diagnostics().get(index).cloned() else {
            return;
        };
        self.yaml_view
            .update(cx, |view, cx| view.scroll_to_line(diagnostic.line, cx));
        self.yaml_view.update(cx, |view, cx| view.focus(window, cx));
    }

    fn on_problems_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let handled = match key {
            "up" => {
                self.move_problem_cursor(-1);
                true
            }
            "down" => {
                self.move_problem_cursor(1);
                true
            }
            "pageup" => {
                self.move_problem_cursor(-self.problem_page());
                true
            }
            "pagedown" => {
                self.move_problem_cursor(self.problem_page());
                true
            }
            "home" => {
                self.problem_cursor = 0;
                true
            }
            "end" => {
                self.problem_cursor = self.problem_count.saturating_sub(1);
                true
            }
            "enter" | "return" | "space" => {
                self.jump_to_problem(window, cx);
                true
            }
            _ => false,
        };
        if handled {
            // The list is capped, so the cursor row has to come into view: a highlighted problem
            // the reader cannot see is the same as no highlight.
            self.problems_scroll.scroll_to_item(self.problem_cursor);
            cx.stop_propagation();
            cx.notify();
        }
    }

    /// Rows one page key moves in the problems list: the rows the capped viewport shows.
    fn problem_page(&self) -> isize {
        let page = Self::viewport_height(&self.problems_scroll) / f32::from(design::size::ROW);
        page.floor().max(1.) as isize
    }

    fn copy_yaml(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self.yaml_view.read(cx).text() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.copied_at = Some(Instant::now());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FEEDBACK).await;
            this.update(cx, |panel, cx| {
                panel.copied_at = None;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
    /// `UI-SPEC.md` §4.19: copies the address of what is on screen.
    ///
    /// The link is the one thing about an object that survives leaving the window: a second
    /// window, a colleague's chat, an alert that names a cluster. `⌘L` reaches it, and the
    /// control that carries it is a 12px chain in the header's trailing edge.
    fn copy_link(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.clone() else {
            return;
        };
        let cluster = self
            .link_cluster
            .clone()
            .or_else(|| self.session.cluster_id.map(|id| id.to_string()))
            .unwrap_or_else(|| "cluster".to_owned());
        cx.write_to_clipboard(ClipboardItem::new_string(deep_link(&cluster, &selection)));
        self.linked_at = Some(Instant::now());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FEEDBACK).await;
            this.update(cx, |panel, cx| {
                panel.linked_at = None;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn linked(&self) -> bool {
        self.linked_at
            .is_some_and(|at| at.elapsed() < COPIED_FEEDBACK)
    }

    /// Follows a relationship, remembering where the reader came from.
    ///
    /// The trail is the whole design: a chain with a way back is navigation, and a chain without
    /// one is a trap. The YAML tab is emptied on the way, because the document the editor holds
    /// belongs to the *selected* row and this object is not it — showing the previous object's
    /// YAML under this object's name is the worst thing the panel could do.
    fn follow_related(&mut self, target: ObjectRef, cx: &mut Context<Self>) {
        let Some(current) = self.selection.clone() else {
            return;
        };
        if self.has_pending() {
            // A selection is still waiting to load. Following now would drop it, and the pending
            // state exists so that an unsaved document is not thrown away by a click.
            return;
        }
        self.related_trail.push(current);
        self.related_rows.push(self.selected_rows);
        self.select_related(target, 1, cx);
    }

    /// Carries out a Related row's action, whichever kind of way out it is.
    fn run_related_action(&mut self, action: RelatedAction, cx: &mut Context<Self>) {
        match action {
            RelatedAction::Follow(target) => self.follow_related(target, cx),
            RelatedAction::Tab(tab) => self.show_tab(tab, cx),
            RelatedAction::Fact => {}
        }
    }

    /// Goes back one level along the trail, and reports whether there was anywhere to go.
    fn go_back(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(previous) = self.related_trail.pop() else {
            return false;
        };
        let rows = self.related_rows.pop().unwrap_or(1);
        self.select_related(previous, rows, cx);
        true
    }

    /// Puts a followed object on screen without going through the shell.
    fn select_related(&mut self, target: ObjectRef, selected_rows: usize, cx: &mut Context<Self>) {
        self.load_yaml(None, cx);
        self.selection = Some(target.clone());
        self.selection_target = Some(ApplyTarget::from_object(target, Some(self.session)));
        // The count the table pushed described *its* selection, and a followed object is not a row
        // in it: carrying the number over would have the banner claim that a ConfigMap is one of
        // eight selected Pods. `go_back` hands the count back with the object it belongs to.
        self.selected_rows = selected_rows;
        self.invalidate_apply();
        self.reset_selection_loads(cx);
        self.sync_metrics_target(cx);
        cx.notify();
    }

    // Common rendering

    /// Label of the active tab, for the tab panel and the review copy.
    fn tab_label(&self, index: usize) -> &'static str {
        match index {
            1 => "Describe",
            2 => "Events",
            3 => "Metrics",
            _ => "YAML",
        }
    }

    /// The object this Inspector is showing, on every tab.
    ///
    /// The toolbar titles are 14px muted text that a 240px Inspector clips, and the Metrics tab
    /// used to show a bare name. A persistent header names the kind, the name, and the age, so no
    /// tab can be read without knowing which object it belongs to. It is also the *only* place
    /// object identity is written down: the YAML band kept a second copy of it that survived on
    /// about 27 of its 53 characters, so the header is the single source and the bands name their
    /// document instead.
    ///
    /// `floating` is the overlay frame's own corner, and it is passed in rather than asked for
    /// because the header and the frame have to agree: the frame rounds its own corners and the
    /// band that touches them rounds with it, or the radius is drawn and then painted over.
    fn render_identity(&self, cx: &Context<Self>, floating: bool) -> AnyElement {
        let rail_ghost = border_rail(cx);
        let Some(selection) = self.selection.as_ref() else {
            // Nothing selected: the band keeps its geometry and says nothing.
            //
            // It used to print `No resource selected` here while the Describe body printed
            // `Select a row to see its fields` — the same sentence on two faces, in the same
            // 15/600, which `UI-SPEC.md` §4.13 names as the thing to reorganize ("标题说没有详情,
            // 动作说去选一行"). The content region owns it, for three reasons that are also why the
            // band could never own it: the four tabs have four different sentences down there and
            // the band has one, so a band-level sentence is either vaguer than all of them or
            // wrong on three of the four; the band is chrome, and `§2.4` keeps content out of
            // chrome; and the band's sentence sat at the same weight as the region's, so the
            // reader could not tell which one was the panel's title.
            //
            // The band is not removed. Its height, its rule, and its rail are what hold the tab
            // strip in place, and an Inspector whose title bar collapses the moment you clear the
            // selection is a panel that reflows on every click.
            //
            // It held `design::size::ROW` here and nothing at all in the branch below, which is
            // the same collapse by another name: 32px of chrome with nothing on it against the
            // 50px the object's own two lines need, so clearing a selection pulled the tab strip,
            // the three toolbars and the body 18px up the panel and selecting a row pushed them
            // all back down. Measured at 2x on this product's own cluster, the band with a Pod
            // selected runs from device y=80 to y=179 and the tab strip's top edge is at y=182;
            // an empty band 18px shorter puts that edge at y=146. Both branches now answer
            // [`identity_band_height`].
            return h_flex()
                .id("inspector-identity")
                .debug_selector(|| "inspector-identity".to_owned())
                .flex_none()
                .w_full()
                .min_w(px(0.))
                .min_h(identity_band_height())
                .px(space::SM)
                .gap(space::SM)
                .items_center()
                .bg(if floating {
                    role::surface_overlay(cx)
                } else {
                    role::surface_raised(cx)
                })
                .border_l_2()
                .border_color(rail_ghost)
                .border_b_1()
                .border_color(role::border_subtle(cx))
                .when(floating, |this| {
                    this.rounded_tl(radius::LG).rounded_tr(radius::LG)
                })
                // The band's own name, for a screen reader that has just arrived in this region.
                // The visible sentence lives in the content region below.
                .role(Role::Region)
                .aria_label("Inspector. No resource selected.")
                .into_any_element();
        };
        let kind = if selection.resource.kind.is_empty() {
            "Resource"
        } else {
            selection.resource.kind.as_str()
        };
        // The age comes from the fetched object, so it appears once Describe or Metrics has read
        // it rather than guessing from a selection that carries no timestamp.
        let age = self
            .selection
            .as_ref()
            .and_then(|object| self.describe_states.get(&cache_key(object)))
            .and_then(|state| match state {
                LoadState::Ready(data) => data.object.metadata.creation_timestamp.as_ref(),
                _ => None,
            })
            .map(|created| format!("{} old", format_age(created.0.as_second())));
        // The name truncates in the *middle*. A Kubernetes name is `prefix-hash-suffix`, and the
        // hash is the half that identifies it: `…-59d59bb66d-gxgrw` is a pod you can find, while
        // `perf-129-59d5…` is not. `UI-SPEC.md` §7 fixes this for object names everywhere.
        let name = div()
            .id("inspector-identity-name")
            // The selection banner is measured against this row, and an `id` alone is not
            // addressable from the harness — only a debug selector is.
            .debug_selector(|| "inspector-identity-name".to_owned())
            .flex_1()
            .min_w(px(0.))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis_middle()
            .tooltip(common::hover_hint(object_accessible_identity(selection)))
            .child(
                common::label_panel_title(selection.name.clone()).text_color(role::fg_primary(cx)),
            );
        // The scope line: namespace, then kind, then age — three lanes, one word each, on one
        // decided spine.
        //
        // It used to be one string, `Pod · team-platform-data-ingestion-staging-eu-west-2 ·
        // 1d old`, drawn in one ink at one size, which is a way of saying three things while
        // giving the reader no way to tell that it is three. A reader in Kubernetes navigates by
        // namespace — it is the argument on every `kubectl` they type and the segment every scope
        // picker is built around — so the namespace leads and is the only lane a step above the
        // others in ink. The kind and the age follow in the quiet ink.
        //
        // Three lanes and two middots rather than one ellipsised sentence, because the sentence
        // is what clipped the namespace: a 44-character namespace in `team-platform-…` ate the row
        // and the age fell off the end, and the reader lost the one fact that tells them which
        // copy of a Deployment they are looking at. Each lane is its own element now, so the
        // namespace can give way on its own. The middot is `space::XS` from each neighbour for
        // the reason `UI-SPEC.md` §4.3 fixes it for the centre tab strip.
        //
        // The age ends in the trailing lane rather than beside the kind, and that is the fix for
        // four values fighting over one line: the metadata a reader reads to *identify* the
        // object runs left and is allowed to give way, while the age — a fixed-width fact they
        // compare across objects, which is worth nothing if it moves — sits against the trailing
        // edge where the copy control above it and the action row below it already are. Three
        // bands, one trailing spine.
        //
        // Truncation is decided per lane rather than per row, and the policy is the same one the
        // rest of the panel holds: an object's name truncates in the *middle* because the hash is
        // the half that identifies it, a namespace truncates at the *end* because its tail is the
        // part that tells one namespace from its sibling, and a fixed-width fact — the kind, the
        // age — is never truncated at all. Every clipped lane carries its full value as a tooltip.
        let namespace = selection
            .namespace
            .as_deref()
            .filter(|namespace| !namespace.is_empty());
        let age = age.filter(|age| !age.is_empty());
        let scope = h_flex()
            .id("inspector-identity-scope")
            .debug_selector(|| "inspector-identity-scope".to_owned())
            .w_full()
            .min_w(px(0.))
            .gap(space::XS)
            .items_center();
        let scope = scope.when_some(namespace, |this, namespace| {
            this.child(identity_lane(namespace, IdentityLane::Namespace, cx))
                .child(identity_separator(cx))
        });
        let scope = scope
            .child(identity_lane(kind, IdentityLane::Quiet, cx))
            .when_some(age, |this, age| {
                // The trailing lane is its own element rather than two more children of the
                // metadata row: an auto margin on the row itself would push the whole line
                // right, and the age has to be a column of its own for the spine to mean
                // anything.
                //
                // It takes the separator with it, because the rule on this line is one
                // separator between every pair of lanes rather than one separator where the
                // line happens to begin. The kind and the age were the only pair with nothing
                // between them, and a line that reads `namespace · Pod 3d old` is two joined
                // phrases where it was built to be three facts. The free space goes to the
                // LEFT of the separator so the dot keeps one `space::XS` gap to the age, the
                // same gap it keeps to the kind, rather than drifting against it.
                this.child(
                    h_flex()
                        .ml_auto()
                        .flex_none()
                        .items_center()
                        .gap(space::XS)
                        .child(identity_separator(cx))
                        .child(identity_lane(&age, IdentityLane::Quiet, cx)),
                )
            });
        v_flex()
            .id("inspector-identity")
            .debug_selector(|| "inspector-identity".to_owned())
            .flex_none()
            .w_full()
            .min_w(px(0.))
            // The same floor the empty band holds, so selecting a row cannot move anything under
            // it. A *minimum* rather than a fixed height, because the two states that add a row
            // of their own — a multi-row selection's banner, and a host that wires header actions
            // — have to keep growing the band rather than have their row clipped by it.
            .min_h(identity_band_height())
            .py(space::XS)
            .px(space::SM)
            .gap(space::XXS)
            // The one band in this panel that names an object, so it is the one band that gets
            // its own plane. It is `surface_raised` — a step above the chrome the navigation
            // bands below it share, and a step above the content plane the body sits on — which
            // is what turns four stacked bands into a ladder: title, navigation, work. It was
            // chrome like the tab strip and the toolbar, so the three read as one tall block and
            // the panel looked like a list that happened to have a title glued to the top of it.
            // The derivation layer solves every ink against `surface_raised` as well as against
            // the other five, so the name, the scope and the two controls keep their floors here.
            //
            // On the floating path the band is the *card's own* plane instead, because
            // `surface_raised` is a step below `surface_overlay` — a raised band on a floating
            // card would put the darkest region of the panel where its title is, and the ladder
            // would read upside down. There the card is the top of the stack, the navigation
            // block below it is recessed, and the body is the same plane as the card.
            .bg(if floating {
                role::surface_overlay(cx)
            } else {
                role::surface_raised(cx)
            })
            // The rail is reserved on every band, not only on the content region. It used to be
            // the content's alone, which put the name and the tab words 4px left of the section
            // headings underneath them — a 4px misalignment between the panel's title and its
            // first section, on every object, at every width. Reserving it uniformly is the whole
            // fix and it costs nothing but a transparent border.
            .border_l_2()
            .border_color(rail_ghost)
            .border_b_1()
            .border_color(role::border_subtle(cx))
            .when(floating, |this| {
                this.rounded_tl(radius::LG).rounded_tr(radius::LG)
            })
            .role(Role::Region)
            .aria_label(object_accessible_identity(selection))
            .when_some(self.render_selection_banner(cx), |band, banner| {
                band.child(banner)
            })
            .child(
                h_flex()
                    .w_full()
                    .min_w(px(0.))
                    .gap(space::SM)
                    .items_center()
                    // The way back exists only while there is somewhere to go. A back control
                    // that is present and greyed is one more thing to read on every object.
                    .when_some(self.related_trail.last().cloned(), |this, previous| {
                        this.child(self.back_control(&previous, cx))
                    })
                    .child(
                        kind_glyph(kind, cx).child(
                            Icon::default()
                                .path(design::kind_icon_path(kind))
                                .with_size(Size::Size(design::icon::IN_ROW))
                                // The resting ink of a control's glyph, and not
                                // `fg_tertiary`: this sits beside the object's name at
                                // full strength, so one step quieter than the name reads
                                // as a kind of object the panel cannot open rather than
                                // as the panel's quietest word.
                                .text_color(design::icon::resting(cx)),
                        ),
                    )
                    .child(name)
                    .child(self.link_control(cx)),
            )
            .child(scope)
            .when_some(self.render_object_actions(cx), |this, row| this.child(row))
            .into_any_element()
    }

    /// The line that says this panel is showing one object out of a multi-row selection.
    ///
    /// It is the one place that fact is written down. The table's selection bar counts the rows and
    /// the four actions that hit exactly one object refuse the selection by naming the count, so
    /// the header was the only surface left answering a third question: it named one of eight
    /// rows as if that were the selection.
    ///
    /// It sits above the name rather than under it, because it qualifies the name, and a
    /// qualification the reader meets after the name is one they have already acted on. It costs a
    /// single line and only when more than one row is selected: with one row selected the header
    /// already describes exactly what is on screen, and a caveat there would be read on every
    /// object the panel ever shows.
    fn render_selection_banner(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let (selected, shown) = self.selection_banner()?;
        Some(
            h_flex()
                .id("inspector-selection-banner")
                .debug_selector(|| "inspector-selection-banner".to_owned())
                .w_full()
                .min_w(px(0.))
                .gap(space::XS)
                .items_center()
                .role(Role::Status)
                .aria_label(format!("{selected} · showing {shown}"))
                // The count is a fixed-width fact and never gives way; the name gives way the way
                // every object name in this panel gives way, in the middle, because the hash in
                // `prefix-hash-suffix` is the half that identifies it.
                .child(
                    div()
                        .flex_none()
                        .child(label_small(selected).text_color(role::fg_secondary(cx))),
                )
                .child(identity_separator(cx))
                .child(
                    div()
                        .id("inspector-selection-banner-shown")
                        .debug_selector(|| "inspector-selection-banner-shown".to_owned())
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis_middle()
                        .child(
                            label_small(format!("showing {shown}"))
                                .text_color(role::fg_secondary(cx)),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The banner's two facts: how many rows the table has selected, and which one is on screen.
    ///
    /// `None` with no selection and `None` with a single row, because those are the two cases where
    /// the header already describes the whole selection.
    fn selection_banner(&self) -> Option<(String, String)> {
        let selection = self.selection.as_ref()?;
        if self.selected_rows < 2 {
            return None;
        }
        // A uid stands in for a missing name, the same fallback every other name of this object
        // takes, so the banner cannot name the panel's object as nothing.
        let shown = if selection.name.is_empty() {
            selection.uid.clone()
        } else {
            selection.name.clone()
        };
        Some((
            // The table's own selection bar phrases the count with this helper, so the two
            // surfaces that answer "how many" answer it in the same words.
            design::format::count_with_noun(self.selected_rows, "row selected", "rows selected"),
            shown,
        ))
    }

    /// The 24px back control that appears once the reader has followed a relationship.
    ///
    /// The tooltip names the object it goes back to rather than saying "back", because "back" is
    /// ambiguous the moment there is a table behind the panel: this goes back to *that* object.
    fn back_control(&self, previous: &ObjectRef, cx: &Context<Self>) -> AnyElement {
        let panel = cx.weak_entity();
        let label = format!("Back to {}", object_display_identity(previous));
        div()
            .id("inspector-related-back")
            .debug_selector(|| "inspector-related-back".to_owned())
            .flex_none()
            .tooltip(common::hover_hint(format!("{label} (Esc)")))
            .child(
                Button::new("inspector-related-back-button")
                    .icon(IconName::ArrowLeft)
                    .ghost()
                    // The glyph box is what gpui-kit derives the ICON from; the
                    // target is restated so the pointer still aims at a full
                    // `size::ICON_BUTTON`. Shrinking both together takes the hit
                    // area down with the glyph, which is how three of these left
                    // the toolbar's own vertical centre.
                    .with_size(Size::Size(icon_control_box()))
                    .w(design::size::ICON_BUTTON)
                    .h(design::size::ICON_BUTTON)
                    .text_color(role::fg_secondary(cx))
                    .accessibility_label(label)
                    .tab_index(INSPECTOR_BACK_TAB_INDEX)
                    .on_click(move |_, _, cx| {
                        if let Some(panel) = panel.upgrade() {
                            panel.update(cx, |panel, cx| {
                                panel.go_back(cx);
                            });
                        }
                    }),
            )
            .into_any_element()
    }

    /// The 12px chain that copies this object's address.
    ///
    /// `UI-SPEC.md` §4.19 puts it in the header's trailing edge and gives it `⌘L`. It is a ghost
    /// icon button rather than a labelled one because the panel has a name, a kind, a namespace
    /// and an age on the same two lines, and a text button here is the thing that pushes the
    /// name into a middle ellipsis.
    fn link_control(&self, cx: &Context<Self>) -> AnyElement {
        let linked = self.linked();
        // §4.19 gives the control `⌘L` and this surface binds it, so the hint is a fact about
        // the control rather than a lookup that could come back empty and leave the reader with
        // a control and no way to reach it from the keyboard.
        let chord = Keystroke::parse(LINK_CHORD).ok();
        let label = if linked {
            "Link copied"
        } else {
            "Copy link to this object"
        };
        div()
            .id("inspector-copy-link")
            .debug_selector(|| "inspector-copy-link".to_owned())
            .flex_none()
            .tooltip(inspector_control_tooltip(label, chord))
            .child(
                Button::new("inspector-copy-link-button")
                    .icon(if linked {
                        IconName::Check
                    } else {
                        IconName::Link
                    })
                    .ghost()
                    // The glyph box is what gpui-kit derives the ICON from; the
                    // target is restated so the pointer still aims at a full
                    // `size::ICON_BUTTON`. Shrinking both together takes the hit
                    // area down with the glyph, which is how three of these left
                    // the toolbar's own vertical centre.
                    .with_size(Size::Size(icon_control_box()))
                    .w(design::size::ICON_BUTTON)
                    .h(design::size::ICON_BUTTON)
                    // The copied state changes the *shape* — a tick instead of a chain — and
                    // deliberately not the colour. `UI-SPEC.md` §0's third rule reserves status
                    // ink for things that are wrong, and a copied link is the most ordinary
                    // outcome there is; a green check for "the thing you asked for happened" is
                    // the web's way of saying it, not a platform's.
                    //
                    // At rest it is the resting ink and not `fg_tertiary`, which is the
                    // placeholder and count tier: a chain in the quietest ink beside the
                    // back arrow at full strength reads as a control that cannot be used.
                    .text_color(if linked {
                        design::icon::active(cx)
                    } else {
                        design::icon::resting(cx)
                    })
                    .accessibility_label(label)
                    .tab_index(INSPECTOR_LINK_TAB_INDEX)
                    .on_click(cx.listener(|this, _, _, cx| this.copy_link(cx))),
            )
            .into_any_element()
    }

    /// The header's icon row: one 28px ghost button per wired action, each with a tooltip naming
    /// the object it acts on and whatever chord the keymap binds to it.
    ///
    /// `UI-REDESIGN.md` §3.4 puts this on its own band between the title and the scope line. It is
    /// `design::size::ROW` and the buttons are `design::size::CONTROL`, which is the pair every
    /// other row of controls in this panel uses — the three toolbars are the same 28px control on
    /// a 40px band. It was the 24px `ICON_BUTTON` in a 28px band, chosen because a 28px control
    /// plus the pixel a focus ring reserves does not fit a 28px row; the row is 32px now, so the
    /// arithmetic no longer applies and two sizes for one kind of control is one size too many.
    ///
    /// An icon-only control is only allowed with a name and a reason, and each of these has
    /// three: the `accessibility_label` a screen reader reads, the tooltip that spells out the
    /// scope ("Delete (Pod nginx-7d8f9c-x2k9p in namespace default)", not "Delete"), and the
    /// chord the keymap binds. They are in the tab order on stops of their own, they take a ghost
    /// button's hover, press and focus states, and there is no disabled state because an action
    /// that cannot run is not offered at all rather than offered greyed — a dead or permanently
    /// greyed control is worse than an absent one.
    ///
    /// The one thing this row does *not* do is own a context menu for the same commands. The
    /// object context menu belongs to the row the reader right-clicked, which is the table's and
    /// the tree's to render, and adding a second surface here would give one command two homes
    /// with different wording.
    fn render_object_actions(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.object_actions.is_empty() {
            return None;
        }
        // What each control acts on, in the words the header above already established. It is
        // composed here rather than written into `ObjectAction::tooltip` because the caller is the
        // shell, and the shell does not know which object the panel will be showing when it wires
        // an action — so a tooltip that said "Delete the Pod nginx-7d8f9c-x2k9p" would have been
        // a caption on one selection and a lie on the next.
        let scope = self
            .selection
            .as_ref()
            .map_or_else(String::new, object_display_identity);
        let buttons = self
            .object_actions
            .iter()
            .enumerate()
            .map(|(index, action)| {
                let run = action.run.clone();
                let label = format!("{}: {scope}", action.label);
                let hint = if scope.is_empty() {
                    action.tooltip.to_owned()
                } else {
                    format!("{} ({scope})", action.tooltip)
                };
                div()
                    .id(format!("inspector-action-{}", action.id))
                    .debug_selector(move || format!("inspector-action-{}", action.id))
                    .flex_none()
                    .tooltip(inspector_control_tooltip(hint, action.chord.clone()))
                    .child(
                        Button::new(format!("inspector-action-button-{}", action.id))
                            .icon(action.icon)
                            .ghost()
                            // One box, from [`icon_control_box`], so the mark is the design's
                            // sixteen pixels here as it is on every other icon control in this
                            // panel. It asked for `design::size::CONTROL`, which the component
                            // reads as 28px of box and answers with a twenty-one-pixel glyph.
                            // The glyph box is what gpui-kit derives the ICON from; the
                            // target is restated so the pointer still aims at a full
                            // `size::ICON_BUTTON`. Shrinking both together takes the hit
                            // area down with the glyph, which is how three of these left
                            // the toolbar's own vertical centre.
                            .with_size(Size::Size(icon_control_box()))
                            .w(design::size::ICON_BUTTON)
                            .h(design::size::ICON_BUTTON)
                            // A destructive action is the one place in the header that may carry
                            // status ink, and it carries it as its *rest* colour rather than on
                            // hover, so the reader sees what they are about to press before they
                            // press it.
                            .text_color(if action.destructive {
                                role::danger(cx)
                            } else {
                                role::fg_secondary(cx)
                            })
                            .accessibility_label(label)
                            // The row's own band of tab stops rather than the Button's default of 0,
                            // which the tab strip already owns: a control that shares an index with the
                            // navigation above it is walked in DOM order, not in the order the panel
                            // is read. Clamped to the band so a fourth action cannot take a stop the
                            // value pool owns.
                            .tab_index(
                                OBJECT_ACTION_TAB_INDEX
                                    + (index as isize).min(OBJECT_ACTION_TAB_SLOTS - 1),
                            )
                            .on_click(move |_, _, cx| run(cx)),
                    )
            });
        Some(
            h_flex()
                .id("inspector-actions")
                .debug_selector(|| "inspector-actions".to_owned())
                .flex_none()
                .w_full()
                .min_w(px(0.))
                .h(design::size::ROW)
                .mt(space::XXS)
                // `space::XS`, not `XXS`: the gap between two controls is the scale's "parts of
                // one thing" and it is also the space a pointer has to travel from one button to
                // the next. Two pixels put the hit targets of a Delete and a Scale against each
                // other, and the guide asks for targets that are comfortably sized even in a dense
                // layout.
                .gap(space::XS)
                .items_center()
                .role(Role::Group)
                .aria_label(format!("Actions for {scope}"))
                .children(buttons)
                .into_any_element(),
        )
    }

    fn render_tabs(&self, cx: &Context<Self>) -> AnyElement {
        let tab_count = self.tab_count();
        let active_tab = self.active_tab.min(tab_count.saturating_sub(1));
        let focused_tab = self.focused_tab.min(tab_count.saturating_sub(1));
        let rail_ghost = border_rail(cx);
        let mut tabs = TABS.to_vec();
        if self.metrics_tab_visible() {
            tabs.push(METRICS_TAB);
        }
        let visible_tab_count = tabs.len();
        let focus = self.tab_focus.clone();
        // The plane the pills sit on. The strip paints `role::surface_chrome` and a pill paints no
        // plane of its own, so this is the surface every wash below is read against — the same
        // one line `shell/panels.rs` and `panels/dock.rs` each state for their own strips.
        let strip_surface = role::surface_chrome(cx);
        let items = tabs.iter().copied().enumerate().map(|(index, tab)| {
            let selected = index == active_tab;
            let focused = index == focused_tab;
            // The selection is the item's own surface, and it is the same five points the centre
            // tab strip and the Dock strip set, so the three tab strips in one window are one
            // component:
            //
            // - plane: the strip's own `role::surface_chrome`, unchanged. A selected tab and the
            //   strip behind it are one surface plus a tint.
            // - fill: `role::accent_wash` — the same 12% of the accent — over the whole pill, at
            //   `design::radius::SM`, so the tint follows the rounded silhouette. It was
            //   `role::surface_raised`, a second surface, and the strip then disagreed with the
            //   other two about what "selected" looks like in the same window.
            // - ink: `role::fg_primary` at `design::text::MEDIUM` when selected, against
            //   `role::fg_secondary` at `design::text::REGULAR` when not. The weight is the channel
            //   that separates in greyscale, where a 12% accent wash over a 1.02:1 chrome step
            //   does not.
            //
            // An inactive tab is `fg_secondary` and not `fg_tertiary`, which is the tier measured
            // off this window's own `22` beside the resource header: a count. A tab title is a
            // destination the reader has to read to do the job, and three of the four are not the
            // tab they are on. `common::label_text` already answers `fg_secondary` and this
            // overrode it, which is how the strip ended up quieter than the helper it was handed.
            // `shell/panels.rs` reaches the same answer on the centre strip and gives the reason:
            // at `fg_tertiary` the strip read as disabled.
            //
            // Nothing is reserved for a marker. The 2px accent rail that used to run along the
            // bottom of the open tab is gone, for the reason the guide gives by name: "Do not add
            // a leading-edge bar or one-sided border as the selection marker… it breaks the item's
            // rounded silhouette and adds a second, competing edge to a column that already aligns
            // on its text."
            let ink = if selected {
                role::fg_primary(cx)
            } else {
                role::fg_secondary(cx)
            };
            // The glyph carries the tab's state too, and `icon::active` is the role that says so.
            //
            // It did not, and the strip then drew a mark one full tier above its own word in both
            // directions — `icon::resting` against a `fg_primary` label on the open tab, and
            // `icon::resting` against a `fg_tertiary` label on the three closed ones. The other
            // two strips in the window put the mark in the tab's ink, and the capture agrees: on
            // the centre strip the open tab's glyph and its word measure the same, and the closed
            // tab's glyph and its word measure the same. A mark that ignores the state of the thing
            // it marks is the one channel on the pill that a greyscale reader cannot read, because
            // a 12% wash over a 1.02:1 step is not a difference and the word's weight is one they
            // have to already know about.
            let glyph_ink = if selected {
                design::icon::active(cx)
            } else {
                design::icon::resting(cx)
            };
            h_flex()
                .id(("inspector-tab", index))
                .debug_selector(move || format!("inspector-tab-{index}"))
                .relative()
                // The 22px pill inside the 28px strip, centred — the same pill height and the
                // same `radius::SM` as `CENTER_TAB_HEIGHT` and `DOCK_TAB_HEIGHT`, so a tab in this
                // strip is the same shape as a tab in the other two. It fills the strip, so the
                // open tab's tint was a 28px band with a radius that governed nothing.
                .h(INSPECTOR_TAB_HEIGHT)
                .flex_none()
                .rounded(radius::SM)
                .px(space::SM)
                // `space::XS` between the mark and its word, not the looser
                // icon-to-label gap: four pills at the icon lane's width do not fit
                // the 260px minimum width the product supports, and the two strips
                // this one is a third of set this gap beside words of their own.
                .gap(space::XS)
                .items_center()
                // The focus ring is reserved and unpainted, as in `panels/dock.rs`, so taking and
                // leaving focus cannot resize a pill or move the word beside it.
                .border_1()
                .border_color(border_rail(cx))
                .when(focused, |this| this.track_focus(&focus))
                .role(Role::Tab)
                .aria_label(tab.label)
                .aria_selected(selected)
                .aria_position_in_set(index + 1)
                .aria_size_of_set(visible_tab_count)
                .aria_keyshortcuts("Enter Space ArrowLeft ArrowRight Home End")
                .accessibility_id(format!("inspector-tab-{index}"))
                // The two states are one pair each rather than a base pair with a `when` on top:
                // `hover` and `active` each hold a single style, so a second call replaces the
                // first and the selected tab would silently get the inactive wash. The selected
                // branch is written last so it is the one that lands on a selected tab.
                .when(!selected, |this| {
                    this.hover(|this| this.bg(design::state::hover(cx, strip_surface)))
                        .active(|this| this.bg(design::state::press(cx, strip_surface)))
                })
                .when(selected, |this| {
                    this.bg(role::accent_wash(cx))
                        // The open tab still answers the pointer. Removing the rail took the last
                        // state the selected item had, and a pointer resting on the tab a reader is
                        // already on used to look like a pointer resting on nothing; the wash is the
                        // accent over the pill's *own* surface, the same two steps
                        // `shell/panels.rs` gives the centre tab.
                        .hover(|this| this.bg(hover_on(strip_surface, role::accent(cx))))
                        .active(|this| this.bg(press_on(strip_surface, role::accent(cx))))
                })
                .focus_visible(|style| style.border_color(design::focus::border(cx)))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.activate_tab(index, window, cx);
                }))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    if event.keystroke.modifiers.control
                        || event.keystroke.modifiers.alt
                        || event.keystroke.modifiers.platform
                    {
                        return;
                    }
                    if matches!(event.keystroke.key.as_str(), "enter" | "return" | "space") {
                        this.activate_tab(index, window, cx);
                    } else if let Some(target) =
                        tab_focus_target(index, visible_tab_count, event.keystroke.key.as_str())
                    {
                        this.focused_tab = target;
                        this.tabs_scroll.scroll_to_item(tab_scroll_index(target));
                        cx.notify();
                    } else {
                        return;
                    }
                    cx.stop_propagation();
                }))
                // The glyph takes the strip's own size rather than
                // `design::icon::IN_ROW`. The centre tabs and the Dock tabs draw
                // theirs at the component's tab size, and three strips at one
                // height are one control: a sixteen-pixel mark in this strip beside
                // a twelve-pixel mark in the other two would make the Inspector read
                // as a different kind of tab. One tab lane, one number, and it is
                // a number the two sibling strips state as well - so it is a
                // cross-lane decision if it is ever to change.
                .child(Icon::new(tab.icon).xsmall().text_color(glyph_ink))
                // The label's ink *and* its weight are stated rather than inherited: gpui-kit's
                // `Label` re-applies `theme().foreground` after it takes the caller's style, so an
                // ink set on the pill two children up never reaches the glyph. `panels/dock.rs`
                // hit the same wall and says so at length. One icon slot, one label lane, one
                // baseline: the inactive words are the same size as the active one so the strip
                // does not reflow when the reader changes tab.
                .child(
                    label_text(tab.label)
                        .font_weight(if selected {
                            text::MEDIUM
                        } else {
                            text::REGULAR
                        })
                        .text_color(ink),
                )
                .into_any_element()
        });
        h_flex()
            .id("inspector-tabs")
            .debug_selector(|| "inspector-tabs".to_owned())
            .flex_none()
            .w_full()
            .h(design::size::TAB_BAR)
            .bg(strip_surface)
            // The rail is reserved here for the same reason it is reserved on the identity band
            // and the content region: four bands, one left edge.
            .border_l_2()
            .border_color(rail_ghost)
            .border_b_1()
            .border_color(role::border_subtle(cx))
            .tab_group()
            .child(
                h_flex()
                    .id("inspector-tabs-scroll")
                    .role(Role::TabList)
                    .aria_label(INSPECTOR_TAB_LIST_LABEL)
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    // The pills are centred in the 28px strip, so a reader scanning the words
                    // across the three strips in this window finds them on one line rather than on
                    // the strip's own top edge.
                    .items_center()
                    // `space::XXS` between pills, the number `panels/dock.rs` states for the strip
                    // this one is paired with. Two 22px pills at `radius::SM` with no gap touch, and
                    // what the notch between their corners leaves is one shape with a waist rather
                    // than two tabs: at magnification the Dock and the Inspector read as two
                    // different controls for the same verb. The centre strip answers the same
                    // question with a middot because §4.3 gives it one; this strip has none, so the
                    // gap is the whole of its separation.
                    .gap(space::XXS)
                    .overflow_x_scroll()
                    .restrict_scroll_to_axis()
                    .track_scroll(&self.tabs_scroll)
                    .children(items),
            )
            .into_any_element()
    }

    fn status(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if let Some(error) = &self.validation_error {
            return Some(status_message(
                Severity::Error,
                "YAML is invalid. Fix the syntax, then apply.",
                Some(error.clone()),
                cx,
            ));
        }
        if let Some(owners) = &self.conflict_owners {
            // The owners are the fact; the way out is a field they own, and the panel knows which
            // fields those are — the editor draws them quieter and the rail locks them. The old
            // line said "review your changes, then apply again", which is the request that had
            // just been refused, and a reader following it learns the same thing twice.
            return Some(status_message(
                Severity::Warning,
                "The cluster refused this: another tool owns a field it changes.",
                Some(format!(
                    "Owned by {}. Take that field out of your change, or revert to the text the cluster reported.",
                    owners.join(", ")
                )),
                cx,
            ));
        }
        if let Some(error) = &self.apply_error {
            if error == APPLY_UNAVAILABLE_REASON {
                return Some(status_message(
                    Severity::Warning,
                    "Apply unavailable",
                    Some(error.clone()),
                    cx,
                ));
            }
            if error == APPLY_UNKNOWN_REASON {
                return Some(status_message(
                    Severity::Warning,
                    "Apply result is unknown",
                    Some(error.clone()),
                    cx,
                ));
            }
            return Some(self.apply_failure_message(error, cx));
        }
        if self.applying {
            return Some(
                h_flex()
                    .id("yaml-applying-status")
                    .flex_none()
                    .gap(space::XS)
                    .items_center()
                    .role(Role::Status)
                    .aria_label("Applying changes")
                    .child(spinner(
                        IconName::LoaderCircle,
                        design::role::accent(cx),
                        Size::Size(design::icon::IN_ROW),
                    ))
                    .child(label_small("Applying changes…").text_color(role::fg_primary(cx)))
                    .into_any_element(),
            );
        }
        if let Some(reason) = &self.metrics_notice {
            return Some(status_message(
                Severity::Warning,
                "Metrics unavailable",
                Some(reason.clone()),
                cx,
            ));
        }
        if self.has_pending() {
            let (title, reason) = self.pending_reason();
            return Some(status_message(Severity::Warning, title, Some(reason), cx));
        }
        if self.yaml_available && !self.has_apply_handler() {
            let title = if self.yaml_view.read(cx).is_dirty() {
                "Unsaved changes. Apply is unavailable."
            } else {
                "Apply unavailable"
            };
            return Some(status_message(
                Severity::Warning,
                title,
                Some(APPLY_UNAVAILABLE_REASON.to_owned()),
                cx,
            ));
        }
        if self.yaml_view.read(cx).is_dirty() {
            // The only state in this function with no reason under it. Every other one answers
            // "what do I do", because an alert that describes a state without offering a verb
            // leaves the reader to find the way out themselves — and the two ways out of an edit
            // are the two controls already sitting in this band.
            return Some(status_message(
                Severity::Warning,
                "Unsaved changes",
                Some(
                    "Preview to see what the cluster would take, or Cancel to discard them."
                        .to_owned(),
                ),
                cx,
            ));
        }
        if self
            .applied_at
            .is_some_and(|at| at.elapsed() < COPIED_FEEDBACK)
        {
            return Some(status_message(Severity::Success, "Applied", None, cx));
        }
        None
    }

    /// The state after a write the server refused.
    ///
    /// `UI-SPEC.md` §4.15: an error appears where it happened, and it comes with a next step whose
    /// label is a verb. The previous version of this row said "Cancel the changes or try again"
    /// and offered nothing to do either with — a sentence describing two options the reader had to
    /// find themselves. `WRITE-OPS.md` §5.2 says the offer is `[View diff] [Revert] [Dismiss]`;
    /// the diff is the tab they are already on and the history is the shell's, so what the panel
    /// can do honestly is put the cluster's own text back and hand over the message it refused
    /// with.
    ///
    /// Two rows, and the second one is the cluster's own words. It used to be one row of
    /// `[icon] The cluster refused this change. [Revert] [Copy the reason]`, which failed twice:
    ///
    /// - **It said nothing useful.** "The cluster refused this change" is a category, and the
    ///   sentence an SRE needs is `spec.replicas: Invalid value: -1: must be greater than or
    ///   equal to 0`. That was in a tooltip, so the one piece of the failure that names a field
    ///   to fix was one hover away, and a failed apply is exactly when nobody is hovering.
    /// - **It did not fit.** `Copy the reason` is a 164px text button; with the icon, the
    ///   headline, `Revert` and the two gaps the row wanted 299px, so at the 260px the range
    ///   allows and every width up to 296 it ran off the panel and the copy control was half out
    ///   of the frame. A verb-phrase button is the right form for the one action that decides
    ///   what happens next, so `Revert` keeps its label; copying the reason is a courtesy, and a
    ///   courtesy gets the 28px icon control the shared status row already uses for it.
    fn apply_failure_message(&self, reason: &str, cx: &Context<Self>) -> AnyElement {
        let copy_reason = reason.to_owned();
        let copy: Rc<dyn Fn(&mut App)> = Rc::new(move |cx: &mut App| {
            cx.write_to_clipboard(ClipboardItem::new_string(copy_reason.clone()));
        });
        v_flex()
            .id("yaml-apply-failed")
            .debug_selector(|| "yaml-apply-failed".to_owned())
            .w_full()
            .min_w(px(0.))
            .gap(space::XXS)
            .py(space::XS)
            .px(space::SM)
            .bg(role::danger_wash(cx))
            .border_l(design::size::SELECTION_RAIL)
            .border_color(role::danger(cx))
            .role(Role::Alert)
            .aria_label(format!("The apply failed. {reason}"))
            .child(
                h_flex()
                    .w_full()
                    .min_w(px(0.))
                    .gap(space::SM)
                    .items_center()
                    .child(
                        Icon::new(design::health_icon(Severity::Error))
                            .flex_none()
                            .with_size(Size::Size(design::icon::IN_ROW))
                            .text_color(role::danger(cx)),
                    )
                    .child(
                        div()
                            .id("yaml-apply-failed-reason")
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            // A sentence, so the channel's *word* ink. The glyph beside it and
                            // the rail down the side keep the mark ink: one colour asked to be a
                            // 12px icon and a 12px label is a colour tuned for the icon.
                            .child(
                                label_small("The cluster refused this change.")
                                    .text_color(role::danger_word(cx)),
                            ),
                    )
                    .child(
                        div()
                            .id("yaml-apply-failed-revert")
                            .debug_selector(|| "yaml-apply-failed-revert".to_owned())
                            .flex_none()
                            .tooltip(common::hover_hint(
                                "Put the text the cluster reported back into the editor",
                            ))
                            .child(
                                Button::new("yaml-apply-failed-revert-button")
                                    .label("Revert")
                                    .ghost()
                                    .tab_index(REVERT_TAB_INDEX)
                                    .on_click(
                                        cx.listener(|this, _: &ClickEvent, _, cx| this.revert(cx)),
                                    ),
                            ),
                    ),
            )
            // The cluster's sentence, on its own line, wrapped to the panel's measure and clamped
            // at the same two lines the problem list uses. The clamp is the reason the tooltip is
            // still here: a validation message can be six lines, and the strip is inside a 336px
            // column that also has to hold the editor.
            .child(
                h_flex()
                    .w_full()
                    .min_w(px(0.))
                    .gap(space::SM)
                    .items_start()
                    .child(
                        div()
                            .id("yaml-apply-failed-detail")
                            .flex_1()
                            .min_w(px(0.))
                            .line_clamp(APPLY_REASON_LINES)
                            .tooltip(common::hover_hint(reason.to_owned()))
                            .child(
                                label_small(reason.to_owned()).text_color(role::danger_word(cx)),
                            ),
                    )
                    .child(
                        div()
                            .id("yaml-apply-failed-copy")
                            .debug_selector(|| "yaml-apply-failed-copy".to_owned())
                            .flex_none()
                            .tooltip(common::hover_hint(
                                "Copy the cluster's own message, for a ticket",
                            ))
                            .child(
                                common::reusable_icon_button(
                                    "yaml-apply-failed-copy-button",
                                    IconName::Copy,
                                    "Copy the reason",
                                )
                                .tab_index(REVERT_TAB_INDEX + 1)
                                .on_click(move |_, _, cx| copy(cx)),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Keeps asking, on the [`PRESENCE_TTL`] cadence, for as long as there is something to ask
    /// about.
    ///
    /// One `GET` every fifteen seconds while a reader sits on a healthy object is the price of
    /// being able to say "this was deleted" at all, and it is the same order as the events read the
    /// panel already performs on the same clock. The loop stops on the first of: no selection, no
    /// source, a session change (the epoch moves), or the panel going away with the task.
    ///
    /// A banner is also cleared once, here rather than by re-reading: a reader who has been told
    /// the object is gone and then watched the check come back `Found` has been told the opposite
    /// thing, and the honest resolution is to stop claiming anything.
    fn arm_presence_recheck(&mut self, cx: &mut Context<Self>) {
        // Dropping the old timer is what makes this idempotent: a second arm cancels the first
        // rather than leaving two loops doubling the request rate.
        self.presence_recheck = None;
        if self.selection.is_none() || self.source.is_none() {
            return;
        }
        let epoch = self.load_epoch;
        self.presence_recheck = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(PRESENCE_TTL).await;
                let keep_going = this
                    .update(cx, |panel, cx| {
                        if panel.load_epoch != epoch {
                            return false;
                        }
                        panel.ensure_presence(true, cx);
                        if panel.gone_reason.is_some() {
                            panel.gone_reason = None;
                        }
                        panel.selection.is_some() && panel.source.is_some()
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        }));
    }

    /// The strip that says the object on screen is gone, or `None` when it is not.
    ///
    /// The shape is the apply-failure strip's — `warning` ink, a left rail, one sentence — with the
    /// severity changed for a reason that is worth stating: a deleted object is a fact about the
    /// cluster, not a failure of the app, and `PROMPT.md` §2.1 #7 reverses the palette so that only
    /// Pending/Failed/Error wear colour. Painting this `danger` would put it in the same channel as
    /// "your apply was refused", and a reader who has learned to act on red would learn to ignore it.
    ///
    /// It does carry a control, because `§4.15` asks every anomaly for its next step and here the step
    /// is real: Reload is the only thing that can clear a stale copy, and it is the same control the
    /// reader already reaches with `⌘R`.
    fn render_gone_banner(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let reason = self.gone_reason.clone()?;
        // The mark and the word, kept apart. The rail down the leading edge and the glyph are
        // marks and take the mark ink; the sentence is a word and takes the channel's word ink,
        // which is the same hue solved for reading rather than for a 12px icon. An exceptional
        // state is allowed to be strong — this is the one band on the panel that interrupts the
        // hierarchy on purpose — and the word ink is what "strong" means for text.
        let ink = role::warning(cx);
        Some(
            h_flex()
                .id("inspector-object-gone")
                .debug_selector(|| "inspector-object-gone".to_owned())
                .w_full()
                .min_w(px(0.))
                .py(space::XS)
                .px(space::SM)
                .gap(space::SM)
                .items_start()
                .bg(role::warning_wash(cx))
                .border_l(design::size::SELECTION_RAIL)
                .border_color(ink)
                .role(Role::Alert)
                .aria_label(format!("The selected object is gone. {reason}"))
                .child(
                    Icon::new(design::health_icon(Severity::Warning))
                        .flex_none()
                        .mt(space::XXS)
                        .with_size(Size::Size(design::icon::IN_ROW))
                        .text_color(ink),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .child(
                            label_body(reason)
                                .text_color(role::warning_word(cx))
                                .font_weight(text::SEMIBOLD),
                        )
                        .child(label_small(GONE_STALE_NOTE).text_color(role::fg_tertiary(cx))),
                )
                .child(
                    div()
                        .id("inspector-object-gone-reload-slot")
                        .debug_selector(|| "inspector-object-gone-reload-slot".to_owned())
                        .flex_none()
                        .tooltip(common::hover_hint("Read the object again"))
                        .child(
                            Button::new("inspector-object-gone-reload")
                                .label("Reload")
                                .ghost()
                                .tab_index(INSPECTOR_DESCRIBE_RETRY_TAB_INDEX)
                                .accessibility_label("Reload the object that was deleted")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    // Forced, because the cached read is exactly what is stale — and
                                    // the forced read is also what finds the object again if it has
                                    // come back, which clears the banner.
                                    this.presence_asked = false;
                                    this.gone_reason = None;
                                    this.ensure_describe(true, cx);
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_yaml_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let copied = self.copied();
        let applying = self.applying;
        let dirty = self.yaml_view.read(cx).is_dirty();
        // A parse problem is a hard stop: the editor reports every problem it finds, and a
        // request built from broken YAML would only be rejected by the API server.
        let problems = self.diagnostics(cx);
        let problem_count = problems.len();
        // Decided once, here. The button is disabled by whether there is a reason and the
        // tooltip is that reason, so both read off this one value: a control cannot end up off
        // with nothing to say, or on while its tooltip explains why it is off. The flag is
        // taken before the reason moves into the tooltip, so nothing is cloned to read it twice.
        let apply_block = self.apply_block_reason(problem_count, cx);
        let apply_blocked = apply_block.is_some();
        let apply_chord = control_chord("k8s_shell::ApplyYaml", cx);
        let mut actions = vec![
            div()
                .id("yaml-action-apply")
                .debug_selector(|| "yaml-action-apply".to_owned())
                .tooltip(inspector_control_tooltip(
                    apply_block.unwrap_or_else(|| {
                        "Show what this change does before anything is written".to_owned()
                    }),
                    apply_chord,
                ))
                .child(
                    // `WRITE-OPS.md` §3: the write is the *second* step, so the first control says
                    // Preview. This button used to read Apply while doing nothing but opening the
                    // review, which is the one label in the flow that is simply untrue — and an
                    // untrue label next to a real write is how a person stops believing the
                    // confirmation too.
                    Button::new("yaml-apply")
                        .label("Preview")
                        .primary()
                        // The width follows the word. It used to be a fixed 72px, sized for
                        // "Apply", and "Preview" is two characters longer — so the primary
                        // control of the whole write flow rendered as `Prev…`, which is the one
                        // place on the panel where a truncated label changes what the reader
                        // thinks they are about to do. `design::size::HIT_MIN` is §4.6's floor.
                        .min_w(design::size::HIT_MIN)
                        .tab_index(APPLY_TAB_INDEX)
                        .disabled(apply_blocked)
                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                            this.action_scroll.scroll_to_item(0);
                            this.apply(cx);
                        })),
                )
                .into_any_element(),
        ];

        // The control stays while the editor holds text the cluster did not send, so the way back
        // survives an apply. Before an apply it reads Cancel, afterwards Revert.
        let diverged = matches!(
            (self.original.as_deref(), self.yaml_view.read(cx).text()),
            (Some(saved), Some(text)) if saved != text
        );
        if diverged {
            let label = if dirty { "Cancel" } else { "Revert" };
            let tooltip = if dirty {
                "Discard the local changes and restore the text the cluster reported"
            } else {
                "Restore the text the cluster reported before this change"
            };
            let revert_chord = control_chord("k8s_inspector::RevertYaml", cx);
            actions.push(
                div()
                    .id("yaml-action-cancel")
                    .debug_selector(|| "yaml-action-cancel".to_owned())
                    .tooltip(inspector_control_tooltip(tooltip, revert_chord))
                    .child(
                        Button::new("yaml-cancel")
                            .label(label)
                            .ghost()
                            .min_w(design::size::HIT_MIN)
                            .tab_index(REVERT_TAB_INDEX)
                            .disabled(applying)
                            .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                this.action_scroll.scroll_to_item(0);
                                this.revert(cx);
                            })),
                    )
                    .into_any_element(),
            );
        }
        let copy_chord = control_chord("k8s_inspector::CopyYaml", cx);
        let copy_label = if copied { "Copied" } else { "Copy YAML" };
        actions.push(
            div()
                .id("yaml-action-copy")
                .debug_selector(|| "yaml-action-copy".to_owned())
                .tooltip(inspector_control_tooltip(copy_label, copy_chord))
                .child(
                    Button::new("copy-yaml")
                        .icon(if copied {
                            IconName::Check
                        } else {
                            IconName::Copy
                        })
                        .ghost()
                        // The glyph box is what gpui-kit derives the ICON from; the
                        // target is restated so the pointer still aims at a full
                        // `size::ICON_BUTTON`. Shrinking both together takes the hit
                        // area down with the glyph, which is how three of these left
                        // the toolbar's own vertical centre.
                        .with_size(Size::Size(icon_control_box()))
                        .w(design::size::ICON_BUTTON)
                        .h(design::size::ICON_BUTTON)
                        // The copied state changes the *shape* — a tick instead of a copy glyph —
                        // and deliberately not the colour. This button wore `role::success`, so a
                        // routine confirmation wore the same green the panel reserves for "this is
                        // broken": a reader who has learned to act on that channel has to learn to
                        // ignore it again here, and the header's own copy-link control had already
                        // made the opposite call. `fg_primary` says "this is now the strongest
                        // thing in the row" and stops there; at rest it is the glyph's resting
                        // ink, because `fg_tertiary` here read as a control that cannot be used.
                        .text_color(if copied {
                            design::icon::active(cx)
                        } else {
                            design::icon::resting(cx)
                        })
                        .accessibility_label(copy_label)
                        .tab_index(COPY_YAML_TAB_INDEX)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.action_scroll.scroll_to_item(0);
                            this.copy_yaml(cx);
                        })),
                )
                .into_any_element(),
        );
        // No title here. The identity bar forty pixels above already names the object at
        // `label_panel_title` and full contrast, and this band had room for about 27 of the 53
        // characters `YAML · Pod nginx-7d8f9c-x2k9p in namespace default` needs at the 336px
        // default width - so it kept the kind and the start of the name and dropped the
        // namespace. Once the buffer is dirty the `Cancel` control takes its place and the title
        // falls to about 16 characters, which is the moment the reader is about to apply.
        // `render_identity` is the single source of object identity; the band's own icon says
        // which document this is.
        //
        // A bare glyph is still a mark that names nothing, so it carries the document's identity
        // the way the other three bands' leading slots do — a tooltip and an accessible name that
        // say what is in the editor and which object it belongs to — without a line of text that
        // would push the Preview control off the edge of a 260px panel.
        let document = self.selection.as_ref().map_or_else(
            || "YAML document. No resource selected.".to_owned(),
            |selection| {
                format!(
                    "YAML document for {}",
                    object_accessible_identity(selection)
                )
            },
        );
        // The `metadata` closure below moves `document` in, so the band's own name takes a copy
        // first rather than borrowing a value the closure owns.
        let document_name = document.clone();
        let metadata = self.status(cx).unwrap_or_else(|| {
            h_flex()
                .id("yaml-clean-metadata")
                .debug_selector(|| "yaml-clean-metadata".to_owned())
                .flex_none()
                .gap(space::XS)
                .items_center()
                .role(Role::Group)
                .aria_label(document.clone())
                .tooltip(common::hover_hint(document))
                .child(
                    // The document mark, and the band's own size rather than the row
                    // lane's: this slot holds nothing else, so the mark's own height
                    // is the only thing that says whether it is a mark or the line of
                    // words it stands in for, and a mark as tall as the smallest line
                    // of type the app prints has stopped being one.
                    Icon::new(IconName::FileCode)
                        .xsmall()
                        .text_color(design::icon::resting(cx)),
                )
                .into_any_element()
        });
        // The YAML band is the one that keeps a leading element, and it is the right exception.
        // The other three bands' leading slots were removed as identity restatements — the tab
        // strip already names the view and the identity band above already names the object — but
        // this one's content is not a restatement of either. It is the document's own state, clean
        // or carrying N problems, which is a fact about the work in front of the reader and the
        // thing the Preview control beside it acts on, and it changes as the reader edits.
        inspector_toolbar(
            "yaml-action-toolbar",
            document_name,
            Some(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .h(design::size::ROW)
                    .items_center()
                    .overflow_hidden()
                    .child(metadata)
                    .into_any_element(),
            ),
            h_flex()
                .id("yaml-action-scroll")
                .flex_none()
                .min_w(px(0.))
                .h(design::size::ROW)
                .gap(space::XS)
                .items_center()
                .overflow_x_scroll()
                .restrict_scroll_to_axis()
                .track_scroll(&self.action_scroll)
                .children(actions)
                .into_any_element(),
            cx,
        )
    }

    fn render_yaml(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // A fetch that failed is an anomaly, so it gets the same treatment Describe and Events
        // already give theirs: a severity, `Role::Alert`, the reason, and a Retry. The empty
        // selection below stays reserved for an actual empty selection, where `Select a row` is
        // the correct instruction rather than a guess.
        if let Some(reason) = self.yaml_error.clone() {
            return load_error(
                LoadErrorParts {
                    title: YAML_LOAD_FAILED_TITLE,
                    hint: load_failure_hint(&reason),
                    retry_label: YAML_RETRY_LABEL,
                    reason,
                    retry_tab_index: INSPECTOR_YAML_RETRY_TAB_INDEX,
                },
                cx,
                cx.listener(|this, _, _, cx| this.reload_yaml(cx)),
            );
        }
        if !self.yaml_available {
            // A followed relationship has no document, and the document the editor holds belongs
            // to the selected row rather than to this object. Showing it here would put the
            // previous object's YAML under this object's name, which is the one thing an editor
            // must never do. So the tab says so, and offers the one way out that does work.
            if !self.related_trail.is_empty() {
                let weak = cx.weak_entity();
                return inspector_empty_with(
                    IconName::FileCode,
                    "No YAML for a followed object",
                    Some(
                        "The document belongs to the selected row. Copy the link to open this one in its own view.",
                    ),
                    Some((
                        "Copy link",
                        Rc::new(move |cx: &mut App| {
                            if let Some(panel) = weak.upgrade() {
                                panel.update(cx, |panel, cx| panel.copy_link(cx));
                            }
                        }),
                    )),
                    cx,
                );
            }
            // The same sentence the editor shows for the same state, from one definition, and the
            // same *state* the other three tabs use: `inspector_empty_with`, not the shared
            // `empty_state`. The shared component lays its glyph, title and sentence out as one
            // horizontal header, and this panel's other three tabs stack them, so the YAML tab was
            // the one place in the Inspector where "nothing to show" was a different shape from
            // every other "nothing to show" on the same panel.
            return inspector_empty_with(
                IconName::FileCode,
                crate::yaml_editor::EMPTY_TITLE,
                Some(crate::yaml_editor::EMPTY_HINT),
                None,
                cx,
            );
        }
        let source_label = self
            .selection
            .as_ref()
            .map(|selection| format!("YAML for {}", object_accessible_identity(selection)))
            .unwrap_or_else(|| "YAML editor. No resource selected.".to_owned());
        v_flex()
            .size_full()
            .min_h(px(0.))
            .min_w(px(0.))
            .overflow_hidden()
            .child(self.render_yaml_toolbar(cx))
            .when_some(self.render_problems(cx), |this, problems| {
                this.child(problems)
            })
            .child(
                div()
                    .id("yaml-source-panel")
                    .role(Role::Region)
                    .aria_label(source_label)
                    .flex_1()
                    .min_h(px(0.))
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(self.yaml_view.clone()),
            )
            .into_any_element()
    }

    /// The parse problems that block Apply, as a list the keyboard can walk.
    fn render_problems(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let problems = self.diagnostics(cx);
        self.problem_count = problems.len();
        if problems.is_empty() {
            return None;
        }
        // The key handler reads the count from the panel, because it has no context to read the
        // editor with.
        let count = problems.len();
        let cursor = self.problem_cursor.min(count - 1);
        let focus_border = focus_ink(cx);
        let rail_ghost = border_rail(cx);
        let muted = role::fg_secondary(cx);
        // The mark ink and the word ink, kept apart on purpose.
        //
        // `role::danger` is solved for a 6px dot and a 2px bar; `role::danger_word` is the same
        // hue solved for a sentence. The band's headline is a sentence — `2 problems block Apply`
        // at `title 15/600` — so it takes the word ink, and only the 6px mark beside it takes the
        // other. They are two roles because a 6px mark and a 12px label do not read at the same
        // contrast, and one colour asked to do both buys the mark's legibility at the word's
        // expense.
        let marker = role::danger(cx);
        let weak = cx.weak_entity();
        // A problem position is data, and the reader can set the data font size. A row drawn at
        // the default size next to a row drawn at the configured one puts two sizes in one list.
        let data = crate::settings::data_typography(cx);
        let rows = problems.iter().enumerate().map(|(index, diagnostic)| {
            let focused = index == cursor;
            let position = format!(
                "Line {}, column {}",
                diagnostic.line + 1,
                diagnostic.column + 1
            );
            let message = diagnostic.short_message().to_owned();
            // Two lines, address above message, rather than one line with the message beside it.
            //
            // The message is the only thing that tells a reader what to do about the problem, and
            // beside the address there is no room for it: at the 336px this panel ships at, and at
            // the 260px its range allows, `Line 448, column 10` leaves about 150px, which is
            // twenty-three characters of an eleven-pixel face. `found unexpected end of stream` is
            // thirty-four, so the one sentence the feature exists to deliver was being cut in the
            // middle with the rest behind a tooltip nobody opens while staring at a red underline.
            //
            // Stacking them fixes the measure without costing the thing a list is for: the
            // addresses are still left-aligned in one column, so a document with twenty problems
            // still reads as twenty line numbers down the left edge, and the messages still wrap
            // under their own address instead of running into the next row's. The cap keeps a
            // long `found unexpected end of stream while scanning a quoted scalar` from turning
            // the list into a paragraph — two lines, then an ellipsis, and the whole text stays
            // one hover away and in the accessible name.
            let detail = v_flex()
                .id(("yaml-problem-detail", index))
                .flex_1()
                .min_w(px(0.))
                .gap(space::XXS)
                .child(
                    Label::new(position.clone())
                        .text_size(px(f32::from(data.size)))
                        .text_color(role::fg_tertiary(cx)),
                )
                .child(
                    div()
                        .id(("yaml-problem-message", index))
                        // A static selector as well as the per-row id: the id has to be unique per
                        // row and cannot be a literal, and a test wants to ask "is a message laid
                        // out at all" without knowing which problem it belongs to.
                        .debug_selector(|| "yaml-problem-message".to_owned())
                        .min_w(px(0.))
                        .line_clamp(PROBLEM_MESSAGE_LINES)
                        .tooltip(common::hover_hint(diagnostic.message.clone()))
                        .child(label_small(message.clone()).text_color(muted)),
                );
            let aria = format!("{position}. {message}");
            let panel = weak.clone();
            h_flex()
                .id(("yaml-problem", index))
                .debug_selector(move || format!("yaml-problem-{index}"))
                .w_full()
                .min_w(px(0.))
                // The row grows with its message instead of clipping it, so the minimum is the
                // one-line height and the two-line case is the one that got the review.
                .min_h(problem_row_height(cx))
                .py(space::XXS)
                .gap(space::SM)
                .items_start()
                .role(Role::ListItem)
                .aria_label(aria)
                .aria_selected(focused)
                .when(focused, |this| this.bg(role::accent_wash(cx)))
                // One mark, at mark size, in a fixed lane.
                //
                // This was a 12px health glyph on every row of a list that is itself one alert
                // band with a count in its headline: eight rows meant eight badges saying the
                // same thing the headline had already said, and the guide is explicit that a
                // Badge is not for every row of a list. The row keeps a mark because the mark is
                // what the reader scans down, and it is the same 2px bar the band's own headline
                // wears — `UI-SPEC.md` §4.15's "a bar of the danger channel beside the text" — in
                // the same lane and the same gap, so the messages below and the headline above
                // start on one leading edge. The row's *own* state is the accent wash, which is a
                // different channel: this one says the row is a problem, that one says it is the
                // one the cursor is on.
                .child(
                    // The lane is the address line's own height and the mark is centred inside
                    // it, rather than the mark carrying a hand-picked top margin: a half-pixel
                    // optical offset that only looks right at one row height is the kind of
                    // correction the alignment guide says has to be expressed as a relationship.
                    h_flex()
                        .flex_none()
                        .w(design::size::SELECTION_RAIL)
                        .h(text::CAPTION_LINE_HEIGHT)
                        .items_center()
                        .child(
                            div()
                                .w_full()
                                .h(design::size::STATUS_DOT)
                                .rounded_full()
                                .bg(marker),
                        ),
                )
                .child(detail)
                .on_click(move |_: &ClickEvent, window, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |panel, cx| {
                            panel.problem_cursor = index;
                            panel.jump_to_problem(window, cx);
                        });
                    }
                })
                .into_any_element()
        });
        // "1 problem block Apply" was missing the verb's `s` and read as one run-on sentence. The
        // count is a subject and Apply is what it acts on, so the sentence says so.
        let title = format!(
            "{count} problem{} blocks Apply",
            if count == 1 { "" } else { "s" }
        );
        Some(
            v_flex()
                .id("yaml-problems")
                .debug_selector(|| "yaml-problems".to_owned())
                .flex_none()
                .w_full()
                .min_w(px(0.))
                .gap(space::XS)
                .py(space::XS)
                .px(space::SM)
                .bg(role::danger_wash(cx))
                .border_b_1()
                .border_color(role::border_subtle(cx))
                .role(Role::List)
                .aria_label(format!("YAML problems, {count} found"))
                .aria_keyshortcuts("ArrowUp ArrowDown PageUp PageDown Home End Enter")
                .track_focus(&self.problems_focus)
                .tab_index(INSPECTOR_PROBLEMS_TAB_INDEX)
                // The rail is always reserved, so taking and leaving focus cannot slide the
                // list sideways.
                .border_l_2()
                .border_color(rail_ghost)
                .focus_visible(move |style| style.border_color(focus_border))
                .on_key_down(cx.listener(Self::on_problems_key_down))
                // `UI-SPEC.md` §4.15: an error is a 3px bar of the danger channel beside the
                // text, not a coloured card. The bar is the one mark that says "this is where
                // the failure is" without moving anything, and it is the *set* that gets it — the
                // count beside it is the sentence, and it is drawn in the channel's word ink
                // because it is a sentence.
                .child(
                    h_flex()
                        .w_full()
                        .gap(space::SM)
                        .items_center()
                        .child(
                            div()
                                .flex_none()
                                .w(design::size::SELECTION_RAIL)
                                .h(design::size::STATUS_DOT)
                                .rounded_full()
                                .bg(marker),
                        )
                        .child(common::label_panel_title(title).text_color(role::danger_word(cx))),
                )
                .child(
                    div()
                        .id("yaml-problem-list")
                        .debug_selector(|| "yaml-problem-list".to_owned())
                        .w_full()
                        .min_w(px(0.))
                        // The cap is what keeps a document with dozens of problems from pushing
                        // the editor out of the panel. Every problem stays in the list; the
                        // focus handle below scrolls the rest into reach.
                        .max_h(problems_list_max_height(cx))
                        .overflow_y_scroll()
                        .track_scroll(&self.problems_scroll)
                        .children(rows),
                )
                // A capped list says so, because this is the list that blocks the write. The
                // title keeps the real total, so the reader never mistakes the cap for the whole
                // list.
                .when(count > PROBLEMS_VISIBLE_ROWS, |this| {
                    let count_text = format!("Showing {PROBLEMS_VISIBLE_ROWS} of {count}.");
                    this.child(
                        label_small(format!("{count_text} The arrow keys reach the rest."))
                            .text_color(muted),
                    )
                })
                .into_any_element(),
        )
    }

    /// The review stands between a parsed change and a write.
    ///
    /// It names the object a request would touch, shows the local change against the text the
    /// cluster last reported, states what the API server made of the document, keeps the safe
    /// action first in the tab order. Nothing here writes: only "Apply to cluster" does, and it
    /// is never the default focus.
    ///
    /// The line that says whether the server would take this document, which is the one question
    /// a diff cannot answer and the reason the review exists rather than a plain editor.
    fn apply_check_line(&self, cx: &Context<Self>) -> AnyElement {
        let (title, color, severity, detail) = match &self.apply_check {
            ApplyCheckState::Running => (
                "Asking the API server whether it would take this document.".to_owned(),
                role::fg_secondary(cx),
                Severity::Neutral,
                None,
            ),
            ApplyCheckState::Valid => (
                "The API server would take this document. Nothing has been written yet.".to_owned(),
                // Words, so the word inks: this line is the answer a reader acts on, and a
                // sentence drawn in the mark ink is a sentence read at whatever contrast a 6px
                // dot happens to clear.
                role::success_word(cx),
                Severity::Success,
                None,
            ),
            ApplyCheckState::Conflict { owners } => {
                let owners = if owners.is_empty() {
                    "another field manager".to_owned()
                } else {
                    owners.join(", ")
                };
                (
                    format!(
                        "The API server would refuse this: another tool owns a field it changes ({owners})."
                    ),
                    role::warning_word(cx),
                    Severity::Warning,
                    // A conflict has one exit, and it is in the editor rather than on this
                    // screen: take the field they own out of your change. Saying "review your
                    // changes and apply again" sent the reader round the same request that had
                    // just refused it.
                    Some(
                        "Remove the field they own from your change, then apply again. Revert puts the cluster's text back.".to_owned(),
                    ),
                )
            }
            ApplyCheckState::Failed { reason } => (
                "The API server refused this document. Nothing has been written.".to_owned(),
                role::danger_word(cx),
                Severity::Error,
                // The server's own sentence, and the whole point of asking before the write: the
                // apply itself does not validate strictly, so this is the only place a field the
                // schema rejects gets named before the change reaches the cluster. It wraps
                // rather than being clamped, because on this screen the space it takes is the
                // space the reader needs to see it in.
                Some(reason.clone()),
            ),
        };
        let busy = matches!(self.apply_check, ApplyCheckState::Running);
        h_flex()
            .w_full()
            .min_w(px(0.))
            .gap(space::XS)
            .items_start()
            .debug_selector(|| "yaml-review-check-status".to_owned())
            .child(
                Icon::new(if busy {
                    IconName::LoaderCircle
                } else {
                    design::health_icon(severity)
                })
                .with_size(Size::Size(design::icon::IN_ROW))
                // The mark is 16px and the column beside it opens two lines of `label`, so the
                // mark sits on the first of them rather than centred between both.
                .mt(space::XXS)
                // The mark channel when the line is carrying a verdict, and the
                // resting ink when it is only carrying a wait: a severity glyph in
                // the quietest ink says nothing, and the answer this line gives is
                // the one the reader is about to act on.
                .text_color(if busy {
                    design::icon::resting(cx)
                } else {
                    design::icon::status(cx, severity)
                }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(space::XXS)
                    .child(label_small(title).text_color(color))
                    .when_some(detail, |this, detail| {
                        this.child(label_small(detail).text_color(color))
                    }),
            )
            .into_any_element()
    }

    /// The Diff mode's header: the mode's name, the object it would touch, and the way out.
    ///
    /// The tab strip is *replaced* rather than kept alongside, because a tab strip that still
    /// shows YAML while the panel is showing a diff tells the reader two things at once. The
    /// `Esc` hint is on the band rather than only in a tooltip because Esc is the chord that
    /// gets a reader back to their unsaved edits, and losing them would be the worst thing this
    /// mode could do.
    fn render_diff_header(&self, cx: &Context<Self>) -> AnyElement {
        let identity = self
            .pending_apply
            .as_ref()
            .map_or_else(String::new, |pending| {
                object_display_identity(&pending.target.object_ref())
            });
        h_flex()
            .id("inspector-diff-header")
            .debug_selector(|| "inspector-diff-header".to_owned())
            .flex_none()
            .w_full()
            .min_w(px(0.))
            .h(design::size::OPEN_VIEWS)
            .px(space::SM)
            .gap(space::SM)
            .items_center()
            .bg(role::surface_chrome(cx))
            .border_l_2()
            .border_color(border_rail(cx))
            .border_b_1()
            .border_color(role::border_subtle(cx))
            .role(Role::Banner)
            .aria_label(format!("Diff mode. Reviewing a change to {identity}"))
            .child(
                div()
                    .flex_none()
                    .child(section_label("Diff").text_color(role::fg_tertiary(cx))),
            )
            .child(
                div()
                    .id("inspector-diff-identity")
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .tooltip(common::hover_hint(identity.clone()))
                    .child(label_small(identity).text_color(role::fg_tertiary(cx))),
            )
            .child(
                div()
                    .flex_none()
                    .child(label_small("Esc to keep editing").text_color(role::fg_tertiary(cx))),
            )
            .into_any_element()
    }

    /// The review body: the changes, the check, and the three ways out.
    ///
    /// `WRITE-OPS.md` §3.1 splits the diff two ways and this is where the split happens. A kind
    /// the product knows the shape of gets a field diff — `spec.replicas  3 → 5` — because a
    /// reader is asking "what will this do to the object", not "which bytes moved". Anything
    /// else, which in practice means a CRD, gets the line diff, because inventing field names
    /// for a schema this product has never read is how a diff tool lies.
    fn review_body(&self, kind: &str, cx: &Context<Self>) -> AnyElement {
        let baseline = self.original.clone().unwrap_or_default();
        let yaml = self
            .pending_apply
            .as_ref()
            .map(|pending| pending.yaml.as_str())
            .unwrap_or_default();
        match semantic_diff(&baseline, yaml, kind.eq_ignore_ascii_case("Secret")) {
            Some(diff) if semantic_diff_kind(kind) => self.semantic_diff_body(&diff, cx),
            _ => self.line_diff_body(&baseline, yaml, cx),
        }
    }

    /// The field diff, with the summary line `WRITE-OPS.md` §3.1 draws under it.
    fn semantic_diff_body(&self, diff: &SemanticDiff, cx: &Context<Self>) -> AnyElement {
        let data = crate::settings::data_typography(cx);
        let row_height = data.line_height;
        let expanded = self.diff_expanded;
        // `WRITE-OPS.md` §10.4: past sixty lines a diff is folded, and the folded state says how
        // big it is — `+8 / -412` — because a reader who cannot see the scale cannot tell
        // whether they meant it.
        let foldable = diff.changes.len() > APPLY_REVIEW_DIFF_FOLD;
        let shown = if foldable && !expanded {
            APPLY_REVIEW_DIFF_PREVIEW
        } else {
            diff.changes.len()
        };
        let added = diff
            .changes
            .iter()
            .filter(|change| change.before.is_none())
            .count();
        let removed = diff
            .changes
            .iter()
            .filter(|change| change.after.is_none())
            .count();
        let rows = diff
            .changes
            .iter()
            .take(shown)
            .map(|change| {
                // A rewritten field is a warning rather than a success: the value is still there
                // and the reader is changing it, which is a different thing from adding a field
                // that was not there. The status colours are a separate budget from the accent, so
                // one small dot per row does not spend the screen's accent. The dot is a MARK, so
                // it takes the mark ink; the two values below it are WORDS, and take the word
                // inks of the same two channels.
                let dot_ink = match change_rank(change) {
                    2 => role::success(cx),
                    _ => role::warning(cx),
                };
                let path = h_flex()
                    .w_full()
                    .min_w(px(0.))
                    .h(row_height)
                    .items_center()
                    .gap(space::XS)
                    .child(
                        div()
                            .flex_none()
                            .w(design::size::STATUS_DOT)
                            .h(design::size::STATUS_DOT)
                            .rounded_full()
                            .bg(dot_ink),
                    )
                    .child(
                        div()
                            .id(format!("yaml-diff-path-{}", change.path))
                            // A static selector as well as the per-path id: the id has to be
                            // unique per row and cannot be a literal, and a test wants to ask
                            // "is there a field-path row at all" without knowing the path.
                            .debug_selector(|| "yaml-diff-path".to_owned())
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font(data.font.clone())
                            .font_features(data.features.clone())
                            .text_size(px(f32::from(data.size)))
                            .line_height(row_height)
                            .text_color(role::fg_secondary(cx))
                            .tooltip(common::hover_hint(change.path.clone()))
                            .child(SharedString::from(change.path.clone())),
                    );
                // `WRITE-OPS.md` §3.1 draws the values under the path rather than beside it, and
                // in a 336px column beside is not available: `containers[name=api].image` leaves
                // about 140px, which is a truncated value on one side and a truncated value on
                // the other. Underneath, the path keeps its whole name and the values keep
                // theirs, which is the trade the design is making.
                let values = h_flex()
                    .w_full()
                    .min_w(px(0.))
                    .h(row_height)
                    .gap(space::XS)
                    .items_center()
                    .child(div().w(design::size::STATUS_DOT + space::XS))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font(data.font.clone())
                            .font_features(data.features.clone())
                            .text_size(px(f32::from(data.size)))
                            .line_height(row_height)
                            .text_color(if change.before.is_some() {
                                role::danger_word(cx)
                            } else {
                                role::fg_tertiary(cx)
                            })
                            .child(SharedString::from(
                                change.before.clone().unwrap_or_else(|| "—".to_owned()),
                            )),
                    )
                    .child(
                        div()
                            .flex_none()
                            .child(SharedString::from("→"))
                            .text_color(role::fg_tertiary(cx)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font(data.font.clone())
                            .font_features(data.features.clone())
                            .text_size(px(f32::from(data.size)))
                            .line_height(row_height)
                            .text_color(if change.after.is_some() {
                                role::success_word(cx)
                            } else {
                                role::fg_tertiary(cx)
                            })
                            .child(SharedString::from(
                                change.after.clone().unwrap_or_else(|| "—".to_owned()),
                            )),
                    );
                v_flex()
                    .w_full()
                    .min_w(px(0.))
                    .gap(space::XXS)
                    .child(path)
                    .child(values)
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let summary = format!(
            "{} · {} fields unchanged",
            FieldChange::summary(diff.changes.len()),
            design::format::count(diff.unchanged)
        );
        self.diff_frame(
            rows,
            Some(summary),
            DiffShape {
                foldable,
                expanded,
                added,
                removed,
            },
            cx,
        )
    }

    /// The line diff, for a kind whose fields this product has not read.
    fn line_diff_body(&self, baseline: &str, yaml: &str, cx: &Context<Self>) -> AnyElement {
        let data = crate::settings::data_typography(cx);
        let row_height = data.line_height;
        let diff = yaml_diff(baseline, yaml);
        let added = diff
            .iter()
            .filter(|line| matches!(line, DiffLine::Added(_)))
            .count();
        let removed = diff
            .iter()
            .filter(|line| matches!(line, DiffLine::Removed(_)))
            .count();
        let changed = diff
            .iter()
            .filter(|line| matches!(line, DiffLine::Added(_) | DiffLine::Removed(_)))
            .count();
        let expanded = self.diff_expanded;
        let foldable = diff.len() > APPLY_REVIEW_DIFF_FOLD;
        let shown = if foldable && !expanded {
            APPLY_REVIEW_DIFF_PREVIEW
        } else {
            diff.len()
        };
        let rows = diff
            .iter()
            .take(shown)
            .map(|line| {
                let text = format!("{} {}", line.marker(), line.text());
                h_flex()
                    .w_full()
                    .min_w(px(0.))
                    .h(row_height)
                    .gap(space::XS)
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font(data.font.clone())
                            .font_features(data.features.clone())
                            .text_size(px(f32::from(data.size)))
                            .line_height(row_height)
                            // A whole line of a diff is a sentence, so it is drawn in the
                            // channel's *word* ink. `marker` is the 6px role and this is
                            // `data.size` monospace at whatever the reader configured — the one
                            // place in the panel where the two were being asked to be the same
                            // ink, and the reason a `+` line at 24px could fall under the
                            // contrast a dot has to clear.
                            .text_color(line.severity().word(cx))
                            .child(SharedString::from(text)),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let summary = format!(
            "{} · {} fields unchanged",
            FieldChange::summary(changed),
            design::format::count(
                diff.iter()
                    .filter(|line| matches!(line, DiffLine::Context(_)))
                    .count()
            )
        );
        self.diff_frame(
            rows,
            Some(summary),
            DiffShape {
                foldable,
                expanded,
                added,
                removed,
            },
            cx,
        )
    }

    /// The shared frame around either diff: the inset well, the summary rule, and the fold.
    ///
    /// The shape travels as one value because it is one idea: how big the change is, and whether
    /// this reader has opened it. Four loose booleans and counters read as four unrelated facts
    /// at the call site, and the fold is the one thing that must not disagree with the count it
    /// sits under.
    fn diff_frame(
        &self,
        rows: Vec<AnyElement>,
        summary: Option<String>,
        shape: DiffShape,
        cx: &Context<Self>,
    ) -> AnyElement {
        let DiffShape {
            foldable,
            expanded,
            added,
            removed,
        } = shape;
        let panel = cx.weak_entity();
        let hidden = rows.len().saturating_sub(APPLY_REVIEW_DIFF_PREVIEW);
        v_flex()
            .w_full()
            .min_w(px(0.))
            .gap(space::XS)
            .px(space::XS)
            .py(space::XS)
            // The diff is content, so it recedes: the inset surface, not a card
            // with a border. A stroke here would be a decorative one, and the design
            // allows exactly three places for those.
            .rounded(radius::SM)
            .bg(role::surface_inset(cx))
            .id("yaml-apply-review-diff")
            .debug_selector(|| "yaml-apply-review-diff".to_owned())
            .role(Role::Region)
            .aria_label("Change against the text the cluster reported")
            .children(rows)
            .when_some(summary, |this, summary| {
                this.child(
                    h_flex()
                        .w_full()
                        .min_w(px(0.))
                        .gap(space::XS)
                        .items_center()
                        .child(
                            div()
                                .flex_none()
                                .w(SECTION_DASH_WIDTH)
                                .h(border::LINE)
                                .bg(role::border_subtle(cx)),
                        )
                        .child(label_small(summary).text_color(role::fg_tertiary(cx))),
                )
            })
            // The count of a folded diff is on the fold itself, so a reader who does not expand it
            // still learns the scale of what they are about to write.
            .when(foldable, |this| {
                this.child(
                    h_flex()
                        .w_full()
                        .gap(space::SM)
                        .items_center()
                        .child(
                            label_small(format!("+{added} / −{removed}"))
                                .text_color(role::fg_tertiary(cx)),
                        )
                        .child(
                            Button::new("yaml-diff-expand")
                                .label(if expanded {
                                    "Collapse the change list".to_owned()
                                } else if hidden > 0 {
                                    format!("Show {hidden} more")
                                } else {
                                    "Show the whole change list".to_owned()
                                })
                                .ghost()
                                .tab_index(REVIEW_EXPAND_TAB_INDEX)
                                .on_click(move |_: &ClickEvent, _, cx| {
                                    if let Some(panel) = panel.upgrade() {
                                        panel.update(cx, |panel, cx| {
                                            panel.diff_expanded = !panel.diff_expanded;
                                            cx.notify();
                                        });
                                    }
                                }),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_apply_review(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pending = self.pending_apply.as_ref()?;
        let identity = object_display_identity(&pending.target.object_ref());
        let muted = role::fg_secondary(cx);
        let kind = pending.target.resource.kind.as_str();
        let body = self.review_body(kind, cx);
        Some(
            v_flex()
                .id("yaml-apply-review")
                .debug_selector(|| "yaml-apply-review".to_owned())
                .size_full()
                .min_h(px(0.))
                .min_w(px(0.))
                .overflow_y_scroll()
                .track_scroll(&self.review_scroll)
                .role(Role::Region)
                .aria_label("Change against the text the cluster reported")
                .aria_keyshortcuts("ArrowUp ArrowDown PageUp PageDown Home End")
                .track_focus(&self.review_focus)
                .tab_index(REVIEW_SCROLL_TAB_INDEX)
                .gap(space::SM)
                .py(space::MD)
                .px(space::LG)
                .bg(role::surface_content(cx))
                .role(Role::AlertDialog)
                .aria_label(format!("{APPLY_REVIEW_TITLE} {identity}"))
                .on_key_down({
                    let panel = cx.weak_entity();
                    move |event: &KeyDownEvent, window: &mut Window, cx: &mut App| {
                        if event.keystroke.key.as_str() == "escape" {
                            if let Some(panel) = panel.upgrade() {
                                panel.update(cx, |panel, cx| panel.cancel_pending_apply(cx));
                            }
                            cx.stop_propagation();
                            return;
                        }
                        // `WRITE-OPS.md` §7: the shifted chord writes, and the bare one leaves.
                        // Both are here rather than only on the buttons, because a person who has
                        // read a diff wants to act on it without going looking for a button.
                        let write = event.keystroke.key.as_str() == "enter"
                            && event.keystroke.modifiers.secondary()
                            && event.keystroke.modifiers.shift;
                        if !write {
                            return;
                        }
                        if let Some(panel) = panel.upgrade() {
                            panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
                        }
                        cx.stop_propagation();
                        let _ = window;
                    }
                })
                .child(
                    v_flex()
                        .w_full()
                        .min_w(px(0.))
                        .gap(space::XS)
                        .child(common::label_panel_title(APPLY_REVIEW_TITLE))
                        .child(
                            h_flex()
                                .w_full()
                                .min_w(px(0.))
                                .gap(space::XS)
                                .items_center()
                                .child(
                                    div()
                                        .min_w(px(0.))
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(label_body(identity.clone()).text_color(muted)),
                                )
                                .child(
                                    div()
                                        .min_w(px(0.))
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(
                                            Label::new(format!("UID {}", pending.target.uid))
                                                .text_size(design::text::CAPTION)
                                                .line_height(design::text::CAPTION_LINE_HEIGHT),
                                        ),
                                ),
                        )
                        .child(self.apply_check_line(cx)),
                )
                .child(body)
                .child(
                    h_flex()
                        .w_full()
                        .min_w(px(0.))
                        .gap(space::SM)
                        .justify_end()
                        // Wrapping is what keeps the *safe* action on screen at the panel's own
                        // narrow end. The two decisions measure 318px together — "Keep editing"
                        // 136 and "Apply to cluster" 174 — against 224px of content at the 260px
                        // floor, so a single row put `justify_end` on a box wider than its
                        // parent and pushed the left-hand button to x = −75: the one control that
                        // throws the change away, gone. Two rows of one decision each is a
                        // narrower dialog; a clipped "Keep editing" is a dialog that can only
                        // write.
                        .flex_wrap()
                        .child(
                            div()
                                .id("yaml-review-keep-editing")
                                .debug_selector(|| "yaml-review-keep-editing".to_owned())
                                .tooltip(common::hover_hint(
                                    "Close the review without writing anything",
                                ))
                                .child(
                                    Button::new("yaml-review-keep-editing")
                                        .label("Keep editing")
                                        .ghost()
                                        .min_w(design::size::HIT_MIN)
                                        .tab_index(REVIEW_KEEP_EDITING_TAB_INDEX)
                                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                            this.cancel_pending_apply(cx)
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .id("yaml-review-apply")
                                .debug_selector(|| "yaml-review-apply".to_owned())
                                .tooltip(common::hover_hint(format!(
                                    "Send this change to {identity}"
                                )))
                                .child(
                                    Button::new("yaml-review-apply")
                                        .label("Apply to cluster")
                                        .primary()
                                        .min_w(design::size::HIT_MIN)
                                        .tab_index(REVIEW_APPLY_TAB_INDEX)
                                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                            this.confirm_pending_apply(cx)
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    // Describe rendering

    fn render_describe(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(selection) = self.selection.clone() else {
            return inspector_empty(IconName::TextQuote, "Select a row to see its fields", cx);
        };
        let state = self.describe_states.get(&cache_key(&selection)).cloned();
        let body: AnyElement = match state {
            Some(LoadState::Ready(data)) => self.describe_body(&data, cx),
            Some(LoadState::Failed(reason)) => load_error(
                LoadErrorParts {
                    title: "Failed to load resource details",
                    hint: load_failure_hint(&reason),
                    retry_label: "Retry loading resource details",
                    reason,
                    retry_tab_index: INSPECTOR_DESCRIBE_RETRY_TAB_INDEX,
                },
                cx,
                cx.listener(|this, _, _, cx| this.ensure_describe(true, cx)),
            ),
            _ => loading_state(
                cx,
                "Loading resource details",
                "Fetching fields and conditions",
                self.describe_wait(),
            ),
        };
        let focus_border = focus_ink(cx);
        let rail_ghost = border_rail(cx);
        v_flex()
            .size_full()
            .min_h(px(0.))
            .min_w(px(0.))
            .child(self.render_inspector_toolbar(cx, "Describe"))
            .child(
                div()
                    .id("describe-scroll")
                    .debug_selector(|| "describe-scroll".to_owned())
                    .role(Role::Region)
                    .aria_label("Resource details. Use the arrow keys to scroll.")
                    .aria_keyshortcuts("ArrowUp ArrowDown PageUp PageDown Home End")
                    .track_focus(&self.describe_focus)
                    .tab_index(INSPECTOR_DESCRIBE_SCROLL_TAB_INDEX)
                    // The rail is always reserved, so taking and leaving focus cannot slide the
                    // details sideways.
                    .border_l_2()
                    .border_color(rail_ghost)
                    .focus_visible(move |style| style.border_color(focus_border))
                    .on_key_down(cx.listener(Self::on_describe_key_down))
                    .flex_1()
                    .min_h(px(0.))
                    .min_w(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.describe_scroll)
                    .child(body),
            )
            .into_any_element()
    }

    fn render_inspector_toolbar(&self, cx: &mut Context<Self>, label: &'static str) -> AnyElement {
        // The band names the *content*, not the object and not the tab.
        //
        // It used to read `Describe · Pod kindnet-z558r in namespace kube-system`, which is the
        // sentence forty pixels above it, spelled again and clipped at about half — and the YAML
        // band had already been fixed for exactly this, so the panel contradicted its own decision
        // depending on which tab was open. Then it was fixed the other way, to a bare `list`
        // glyph, and a mark that names nothing is worse than a sentence that names it twice: the
        // reader was left with a refresh control and an ornament.
        //
        // So the left slot says what the region holds — `Resource details`, `Object events` — in
        // the quiet ink, and the object's full identity is the slot's tooltip and accessible name
        // rather than a truncated line on the surface. That is the same division of labour the
        // Dock's own toolbar band makes: the band reports a fact the reader does not already have
        // in the title above, and the scope lives in the control that acts on it.
        let words = match label {
            "Events" => "Object events",
            _ => "Resource details",
        };
        let object = self
            .selection
            .as_ref()
            .map_or_else(String::new, object_display_identity);
        let title = if object.is_empty() {
            label.to_owned()
        } else {
            format!("{label} · {object}")
        };
        let reload_label = format!("Reload {title}");
        // The band's accessible name is the sentence its leading slot used to print, so the fact
        // survives the slot's removal instead of going with it.
        let aria = if object.is_empty() {
            words.to_owned()
        } else {
            format!("{words} for {object}")
        };
        let reload_chord = control_chord("k8s_inspector::ReloadActiveTab", cx);
        let actions = h_flex()
            .id("inspector-context-actions")
            .flex_none()
            .h(design::size::ROW)
            .gap(space::XS)
            .items_center()
            .child(
                div()
                    .id("inspector-action-reload")
                    .debug_selector(|| "inspector-action-reload".to_owned())
                    .tooltip(inspector_control_tooltip(
                        reload_label.clone(),
                        reload_chord,
                    ))
                    .child(
                        Button::new("inspector-reload")
                            .icon(design::glyph::action::reload())
                            .ghost()
                            // The glyph box is what gpui-kit derives the ICON from; the
                            // target is restated so the pointer still aims at a full
                            // `size::ICON_BUTTON`. Shrinking both together takes the hit
                            // area down with the glyph, which is how three of these left
                            // the toolbar's own vertical centre.
                            .with_size(Size::Size(icon_control_box()))
                            .w(design::size::ICON_BUTTON)
                            .h(design::size::ICON_BUTTON)
                            // Stated here because nothing above this button states it.
                            // `Icon` resolves an unset colour against the window's own
                            // foreground, and a capture of this band measured the reload
                            // glyph at 243 against 162 for every other mark in the panel:
                            // the brightest thing on the band was a control that only
                            // re-reads what the reader is already looking at.
                            .text_color(design::icon::resting(cx))
                            .accessibility_label(reload_label)
                            .tab_index(INSPECTOR_RELOAD_TAB_INDEX)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.reload_active_tab(cx)),
                            ),
                    )
                    .into_any_element(),
            )
            .into_any_element();
        inspector_toolbar("inspector-context-toolbar", aria, None, actions, cx)
    }

    fn describe_stacked(&self) -> bool {
        let width = self.describe_width.get();
        !width.is_finite() || width < DESCRIBE_TWO_COLUMN_MIN_WIDTH
    }

    fn describe_body(&mut self, data: &DescribeData, cx: &mut Context<Self>) -> AnyElement {
        let object = &data.object;
        let kind = object
            .types
            .as_ref()
            .map(|types| types.kind.clone())
            .unwrap_or_default();
        let stacked = self.describe_stacked();
        let events_state = self
            .selection
            .as_ref()
            .and_then(|selection| self.events_states.get(&cache_key(selection)))
            .map(|entry| entry.state.clone());

        // The managed-field set is read once here rather than per row: a Pod's
        // `managedFields` carries hundreds of paths, and a linear scan for every row the body
        // draws would make the panel quadratic in its own content.
        self.managed = Rc::new(ManagedFields::from_object(object));
        let values = {
            self.value_focus.borrow_mut().begin_render();
            ValueRows {
                focus: self.value_focus.clone(),
                expanded: Rc::new(self.expanded_values.clone()),
                copied: Rc::new(BTreeSet::new()),
                managed: self.managed.clone(),
                panel: cx.weak_entity(),
            }
        };

        // Status first, always open. `UI-REDESIGN.md` §3.4 puts it there for the reason the
        // panel exists: an SRE opens the Inspector to answer "is this thing healthy", and
        // everything below that answer is on request.
        let mut sections: Vec<AnyElement> = Vec::new();
        // The severity is read off the headline before the headline is rendered, because the
        // block consumes it and the heading's leading lane needs the same answer. One source, so
        // the dot in the lane and the word under it can never disagree.
        let headline = status_headline(object, &kind);
        // The severity is read off the headline before the headline is rendered, because the
        // block consumes it and the heading's leading lane needs the same answer.
        let lead = headline.map(|headline| StatusLead {
            headline: Some(vec![status_headline_block(headline.clone(), cx)]),
            mark: Some(headline.severity),
        });
        let status_rows = describe_status_rows(object, &kind);
        if !status_rows.is_empty() || lead.is_some() {
            sections.push(self.detail_section(
                DetailSection::Status,
                status_rows,
                stacked,
                &values,
                cx,
                lead,
            ));
            // Status is not a disclosure. If it were, the panel's one job would be one click away
            // from being invisible, and a reader who collapsed it would have no way to know the
            // answer was still there.
        }

        let labels = label_rows(object);
        if !labels.is_empty() {
            sections.push(self.detail_section(
                DetailSection::Labels,
                labels,
                stacked,
                &values,
                cx,
                None,
            ));
        }

        let conditions = condition_rows(object, stacked, &values, cx);
        if !conditions.is_empty() {
            sections.push(self.plain_section(DetailSection::Conditions, conditions, None, cx));
        }

        if kind == "Pod" {
            let containers = container_rows(object, stacked, &values, cx);
            if !containers.is_empty() {
                sections.push(self.plain_section(DetailSection::Containers, containers, None, cx));
            }
        }

        // The spec stays complete, and stays out of the Containers block's way: a container's
        // name and image are already two rows above in readable form, so those exact paths are
        // the ones left out. Everything else under a container — resources, ports, mounts,
        // probes — exists only here, and dropping the whole array to avoid two duplicate rows
        // would throw those away too.
        let mut spec_rows = Vec::new();
        if let Some(spec) = object.data.get("spec") {
            let repeated = container_identity_paths(spec, kind == "Pod");
            flatten_scalars_except(spec, "", &repeated, &mut spec_rows);
        }
        if !spec_rows.is_empty() {
            sections.push(self.detail_section(
                DetailSection::Spec,
                spec_rows,
                stacked,
                &values,
                cx,
                None,
            ));
        }

        // The newest events, right above the block that talks about them. The Events tab holds
        // the whole list and this holds the three that explain a state, so a reader who arrives
        // at a Pending Pod reads why before they decide to go anywhere.
        if let Some(LoadState::Ready(events)) = events_state.as_ref()
            && !events.is_empty()
        {
            sections.push(self.events_section(events, cx));
        }

        // Related then Identity. `UI-REDESIGN.md` L3: Related is the navigation out of this
        // object, and navigation is what a reader reaches for once they know what they are
        // looking at. Identity is the last word because it is a lookup table for a reader who
        // already knows the answer and wants the exact string.
        sections.push(self.related_section(object, data, events_state.as_ref(), cx));
        sections.extend(self.identity_section(object, &kind, cx));

        let measured_width = self.describe_width.clone();
        let panel = cx.entity().downgrade();
        div()
            .flex()
            .flex_col()
            .on_children_prepainted(move |children, window, cx| {
                let Some(bounds) = children.first() else {
                    return;
                };
                let width = f32::from(bounds.size.width);
                if width.is_finite() && width > 0.0 && (measured_width.get() - width).abs() > 0.5 {
                    measured_width.set(width);
                    let panel = panel.clone();
                    window.defer(cx, move |_, cx| {
                        if let Some(panel) = panel.upgrade() {
                            panel.update(cx, |_, cx| cx.notify());
                        }
                    });
                }
            })
            .id("inspector-describe-body")
            .debug_selector(|| "inspector-describe-body".to_owned())
            .w_full()
            .min_w(px(0.))
            .px(space::SM)
            .py(space::SM)
            .gap(space::MD)
            .children(sections)
            .into_any_element()
    }

    /// A field section: the cap, the disclosure, and the "show N more" footer.
    ///
    /// `Status` is the one section that is not a disclosure, so `detail_section` builds its
    /// `SectionSpec` from the panel's open set and only offers the chevron when the section can
    /// actually close. Everything else in the body goes through here or [`Self::plain_section`],
    /// so a new section cannot accidentally arrive with a full-width rule under it.
    ///
    /// `lead` is the block that opens the section above its rows, which today only Status has. It
    /// is a parameter rather than a second function because a section with a lead is the same
    /// disclosure with a different first child, and two functions would let the two drift apart on
    /// padding and on the count the heading shows. The sentence and the heading's mark travel
    /// together in one value because they are one fact read twice — [`StatusLead`] is what says so.
    fn detail_section(
        &mut self,
        detail: DetailSection,
        rows: Vec<FieldRow>,
        stacked: bool,
        values: &ValueRows,
        cx: &mut Context<Self>,
        lead: Option<StatusLead>,
    ) -> AnyElement {
        let StatusLead { headline, mark } = lead.unwrap_or_default();
        // The cap and the type treatment are properties of the section, not of the call: Labels
        // is a short prose list, Status and Spec are long data lists, and a caller that could
        // pass either would eventually pass the wrong one.
        let (limit, data_column) = match detail {
            DetailSection::Labels => (LABEL_CHIP_LIMIT, false),
            _ => (MAX_FIELD_ROWS, true),
        };
        let title = detail.title();
        let expanded = match detail {
            DetailSection::Labels => self.expanded_details.labels,
            DetailSection::Spec => self.expanded_details.spec,
            // Status is always open, so "expanded" for it means "past the cap", which is the
            // only expansion left to it.
            _ => self.expanded_details.spec,
        };
        let total = rows.len();
        let open = detail == DetailSection::Status || self.open_sections.contains(&detail);
        let spec = SectionSpec {
            title,
            count: total,
            collapsible: self.section_is_collapsible(detail),
            open,
            mark,
            tab_index: detail.tab_index(),
        };
        let (visible, hidden) = visible_detail_rows(rows, limit, expanded);
        let fields = visible
            .into_iter()
            .map(|(key, value, is_string)| {
                let style = if data_column {
                    ValueStyle::data(is_string)
                } else {
                    ValueStyle::prose()
                };
                field_row(&key, &value, style, stacked, values, cx)
            })
            .collect::<Vec<_>>();
        // The headline is the section's *sentence* and the rows are its *evidence*, so they are
        // two blocks rather than one list. Prepending it to the rows put it 2px above the first
        // field, the same gap as between two fields, and the answer the panel exists to give read
        // as one more line of the list. The gap under it is `space::MD` — the scale's "one content
        // group" — and the gap above it is the section's own `space::XXS`, so the sentence is the
        // block's first paragraph and the first field is a new group.
        let body = match headline {
            Some(headline) => v_flex()
                .id(format!("inspector-describe-lead-{}", detail.title()))
                .w_full()
                .min_w(px(0.))
                .gap(space::XXS)
                .children(headline)
                .child(
                    v_flex()
                        .w_full()
                        .min_w(px(0.))
                        .mt(space::MD)
                        .gap(space::XXS)
                        .children(fields),
                )
                .into_any_element(),
            None => v_flex()
                .w_full()
                .min_w(px(0.))
                .gap(space::XXS)
                .children(fields)
                .into_any_element(),
        };
        let footer = self.section_cap_footer(detail, hidden, expanded, total, cx);
        if self.section_is_collapsible(detail) {
            collapsible_section(&spec, vec![body], footer, detail, cx)
        } else {
            open_section(&spec, vec![body], footer, cx)
        }
    }

    /// A section of rows the panel already built, behind a disclosure.
    ///
    /// Conditions, Containers, Owners and Events are read as prose rather than as a field
    /// list, so they have no cap to offer and no `FieldRow` to flatten; this is the same
    /// disclosure the field sections get, with nothing else on it.
    fn plain_section(
        &mut self,
        detail: DetailSection,
        rows: Vec<AnyElement>,
        footer: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let spec = SectionSpec {
            title: detail.title(),
            count: rows.len(),
            collapsible: true,
            open: self.open_sections.contains(&detail),
            mark: None,
            tab_index: detail.tab_index(),
        };
        collapsible_section(&spec, rows, footer, detail, cx)
    }

    /// The "show the rest of this section" control, when the cap hid something.
    fn section_cap_footer(
        &mut self,
        detail: DetailSection,
        hidden: usize,
        expanded: bool,
        total: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        (hidden > 0).then(|| {
            let label = if expanded {
                "Show fewer".to_owned()
            } else {
                format!("Show {hidden} more")
            };
            let toggle_id = format!("inspector-detail-toggle-{}", detail.title());
            let aria_label = if expanded {
                format!("Collapse {}, {total} fields", detail.title())
            } else {
                format!("Show all {}, {total} fields", detail.title())
            };
            h_flex()
                .w_full()
                .justify_end()
                .child(
                    Button::new(toggle_id)
                        .label(label)
                        .ghost()
                        .tab_index(detail.tab_index())
                        .icon(Icon::new(if expanded {
                            IconName::ChevronUp
                        } else {
                            IconName::ChevronDown
                        }))
                        .toggled(expanded)
                        .accessibility_label(aria_label)
                        .on_click(cx.listener(move |panel, _, _, cx| {
                            match detail {
                                DetailSection::Labels => {
                                    panel.expanded_details.labels = !panel.expanded_details.labels
                                }
                                DetailSection::Spec => {
                                    panel.expanded_details.spec = !panel.expanded_details.spec
                                }
                                _ => {}
                            }
                            cx.notify();
                        })),
                )
                .into_any_element()
        })
    }

    /// `UI-REDESIGN.md` L3: the block that turns a dead end into a chain.
    ///
    /// Every source is already in the `DynamicObject` the Describe call returned —
    /// `metadata.ownerReferences`, `spec.nodeName`, and the event list's `involvedObject` — so
    /// this needs no new dependency and no new request. What it now also does is *follow*: a row
    /// the panel can name is a control, and following it pushes the object onto the panel's own
    /// trail so `Esc` comes back. A row the panel cannot name — a missing ConfigMap, a node the
    /// panel has no uid for — wears no arrow, because an arrow that does nothing is worse than
    /// no arrow.
    fn related_section(
        &mut self,
        object: &DynamicObject,
        data: &DescribeData,
        events_state: Option<&LoadState<Arc<Vec<DynamicObject>>>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut rows: Vec<AnyElement> = Vec::new();
        let namespace = self
            .selection
            .as_ref()
            .and_then(|selection| selection.namespace.clone());

        // Owners, from `metadata.ownerReferences`. `NotFound` is the case that matters: an owner
        // the API can no longer resolve is usually why the object is in the state it is in, and
        // it is the whole reason a person came to the Inspector.
        //
        // The list of `(kind, name)` pairs and the list of references are the same data read
        // twice, and only the references carry uids. A uid is what makes following safe: without
        // one a read could land on a different object that happens to share the name, which is
        // how a panel ends up describing the wrong ReplicaSet. So the names come from the first
        // and the uids from the second, and an owner whose reference had no uid gets a row
        // without an arrow rather than no row at all.
        let uids: BTreeMap<(&str, &str), &str> = object
            .metadata
            .owner_references
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|owner| {
                (
                    (owner.kind.as_str(), owner.name.as_str()),
                    owner.uid.as_str(),
                )
            })
            .collect();
        for (kind, name) in &data.owners {
            let uid = uids
                .get(&(kind.as_str(), name.as_str()))
                .copied()
                .unwrap_or_default();
            let action = followable(kind, name, namespace.as_deref(), uid)
                .map(RelatedAction::Follow)
                .unwrap_or(RelatedAction::Fact);
            rows.push(self.related_row(kind, name, RelatedHealth::Ok, action, cx));
        }

        // The node, from `spec.nodeName`. A Pod that has not been scheduled has no node, and
        // saying so in the slot is the answer to "why is this Pending".
        //
        // It is followable by name, which [`followable_by_name`] argues for: a Pod's node is the
        // one relationship every reader wants to walk, and the row that is always one dead end is
        // the row that teaches people the arrow is decoration.
        if let Some(node) = object
            .data
            .pointer("/spec/nodeName")
            .and_then(Value::as_str)
            .filter(|node| !node.is_empty())
        {
            let action = followable_by_name("Node", node, namespace.as_deref())
                .map(RelatedAction::Follow)
                .unwrap_or(RelatedAction::Fact);
            rows.push(self.related_row("Node", node, RelatedHealth::Ok, action, cx));
        }

        // A reference the cluster answered about: missing, live, or unknown. The third is the
        // case that matters — `UI-REDESIGN.md` L3 says a `NotFound` relationship is usually the
        // root cause and it is the whole reason a person came to the Inspector, so a row that
        // cries wolf costs more than a row that stays quiet.
        //
        // A `Missing` row is drawn in `danger` and with no arrow: the object does not exist, so
        // there is nothing to read and a link to it would be a dead control. A `Found` row is a
        // normal hop — the uid `resolve` returned is what makes following it safe, since a name
        // alone can be freed and taken by a different object.
        // Read under the *described* object's key rather than the selection's: the
        // verdicts were stored when the Describe read landed, and `prepare_describe_data` has
        // already proved the two objects are the same one. An empty map is the honest answer for
        // an object nobody asked about — every reference falls back to the event path.
        let verdicts = self
            .selection
            .as_ref()
            .and_then(|selection| self.reference_verdicts.get(&cache_key(selection)))
            .cloned()
            .unwrap_or_default();
        for row in reference_rows(object, events_state, &verdicts, namespace.as_deref()) {
            rows.push(
                self.related_row(
                    &row.kind,
                    &row.name,
                    row.health,
                    row.follow
                        .map(RelatedAction::Follow)
                        .unwrap_or(RelatedAction::Fact),
                    cx,
                ),
            );
        }

        // Events, from the same cache the Events tab reads, so the count here and the list there
        // can never disagree.
        //
        // The row is a jump rather than a hop, because the events belong to *this* object: taking
        // it opens the Events tab on the object already selected. It is the most-taken row in the
        // block in every tool of this shape, and a row that says `4 events` and then does nothing
        // when clicked is the definition of a dead control.
        if let Some(LoadState::Ready(events)) = events_state {
            let warnings = events
                .iter()
                .filter(|event| event_type(event) == "Warning")
                .count();
            let health = if warnings > 0 {
                RelatedHealth::Warning
            } else {
                RelatedHealth::Ok
            };
            let count = design::format::count_with_noun(events.len(), "event", "events");
            rows.push(self.related_row_with_count(
                "Events",
                &count,
                warnings,
                health,
                RelatedAction::Tab(InspectorTab::Events),
                cx,
            ));
        }

        if rows.is_empty() {
            rows.push(
                h_flex()
                    .w_full()
                    .min_w(px(0.))
                    .h(design::size::ROW)
                    .items_center()
                    .child(
                        label_small("Nothing references this object.")
                            .text_color(role::fg_tertiary(cx)),
                    )
                    .into_any_element(),
            );
        }
        self.plain_section(DetailSection::Related, rows, None, cx)
    }

    /// The Events block inside the Describe body: the newest few events as a timeline, and a
    /// control that opens the whole list.
    ///
    /// `UI-SPEC.md` §10.3 says an Event is not tabular — it is a story in time — so this reuses
    /// the same spine-and-dot row the Events tab draws rather than inventing a second shape for
    /// the same data. The split is deliberate: a Pod that is Pending usually has one event that
    /// says why, and a reader who has to switch tabs to read it will not. The rest of the list
    /// stays on the tab, where the uniform list virtualises a busy namespace's two hundred events.
    fn events_section(
        &mut self,
        events: &Arc<Vec<DynamicObject>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        const INLINE_EVENTS: usize = 3;
        let total = events.len();
        // Unclamped: this block exists so a reader does not have to leave the panel to read the
        // event that says why, and it holds three of them in a column that scrolls. The
        // virtualised list is the one with a fixed row, and it is the one that has to clamp.
        let rows = events
            .iter()
            .take(INLINE_EVENTS)
            .enumerate()
            .map(|(index, event)| {
                event_timeline_row(event, index + 1 == total.min(INLINE_EVENTS), None, cx)
            })
            .collect();
        let spec = SectionSpec {
            title: DetailSection::Events.title(),
            count: total,
            collapsible: true,
            open: self.open_sections.contains(&DetailSection::Events),
            mark: None,
            tab_index: DetailSection::Events.tab_index(),
        };
        // The control exists only when there is more here than the block shows. Ten events behind
        // a header that already says `10` is a jump the reader can predict; the same control
        // under `2` is a button that does nothing useful.
        let hidden = total.saturating_sub(INLINE_EVENTS);
        let footer = (hidden > 0).then(|| {
            h_flex()
                .w_full()
                .justify_end()
                .child(
                    Button::new("inspector-events-open")
                        .label(format!("See all {total} events"))
                        .ghost()
                        .icon(Icon::new(IconName::ChevronRight))
                        .tab_index(DetailSection::Events.tab_index())
                        .accessibility_label(format!("Open the Events tab, {total} events"))
                        .on_click(
                            cx.listener(|panel, _, _, cx| panel.show_tab(InspectorTab::Events, cx)),
                        ),
                )
                .into_any_element()
        });
        collapsible_section(&spec, rows, footer, DetailSection::Events, cx)
    }

    /// `UI-SPEC.md` §2.3's second table and the mockup's last block: the facts that say *where
    /// this object is* — the address, the identity the API server knows it by, and the scheduling
    /// class it was admitted under.
    ///
    /// These are not fields, so they are a grid of `IDENTITY_KEY_COLUMN` keys and mono values
    /// rather than the Describe key/value rows: none of these is a field path, and every one is a
    /// value a reader pastes into a `kubectl` command or a bug report, which is exactly the set
    /// `copyable_value` already recognises. The section is a disclosure for the same reason every
    /// other section below Status is — the reader who opens it is the one who already knows the
    /// answer, and the reader who does not is the one who should be reading Status.
    ///
    /// **The node used to be here too, and it is the one fact on this list that is not one.** It
    /// is already a Related row with an arrow, so this grid repeated it in a place where the
    /// reader could not follow it, and the table's own `NODE` column says it a third time. What is
    /// left here is what exists nowhere else: an address, a uid, and a QoS class.
    fn identity_section(
        &self,
        object: &DynamicObject,
        kind: &str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let mut rows: Vec<(&'static str, String, bool)> = Vec::new();
        // A Node's own address is its status address, not a pod address, so the key says which.
        let (ip_path, ip_label) = if kind == "Node" {
            ("/status/addresses/0/address", "Internal IP")
        } else {
            ("/status/podIP", "IP")
        };
        if let Some(ip) = identity_scalar(object, ip_path) {
            rows.push((ip_label, ip, true));
        }
        if let Some(uid) = object.metadata.uid.as_deref().filter(|uid| !uid.is_empty()) {
            rows.push(("UID", uid.to_owned(), true));
        }
        if let Some(qos) = identity_scalar(object, "/status/qosClass") {
            rows.push(("QoS", qos, false));
        }
        if rows.is_empty() {
            return None;
        }
        // The count says how many rows are in here. It used to be a hardcoded zero, so a Pod with
        // four facts about where it lives announced `IDENTITY 0` — which reads as "this object has
        // no identity" on the one section whose entire content is that it does.
        let count = rows.len();
        let grid = rows
            .into_iter()
            .map(|(key, value, mono)| identity_field(key, &value, mono, cx));
        let spec = SectionSpec {
            title: DetailSection::Identity.title(),
            count,
            collapsible: true,
            open: self.open_sections.contains(&DetailSection::Identity),
            mark: None,
            tab_index: DetailSection::Identity.tab_index(),
        };
        Some(collapsible_section(
            &spec,
            grid.collect(),
            None,
            DetailSection::Identity,
            cx,
        ))
    }
    // Events rendering

    fn render_events(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(selection) = self.selection.clone() else {
            return inspector_empty(IconName::Bell, "Select a row to see its events", cx);
        };
        let state = self
            .events_states
            .get(&cache_key(&selection))
            .map(|e| e.state.clone());
        let body: AnyElement = match state {
            Some(LoadState::Ready(events)) if events.is_empty() => {
                inspector_empty(IconName::Bell, "No events for this object", cx)
            }
            Some(LoadState::Ready(events)) => self.events_list(events),
            Some(LoadState::Failed(reason)) => load_error(
                LoadErrorParts {
                    title: "Failed to load events",
                    hint: load_failure_hint(&reason),
                    retry_label: "Retry loading events",
                    reason,
                    retry_tab_index: INSPECTOR_DESCRIBE_RETRY_TAB_INDEX,
                },
                cx,
                cx.listener(|this, _, _, cx| this.ensure_events(true, cx)),
            ),
            _ => loading_state(
                cx,
                "Loading events",
                "Waiting for Kubernetes events",
                self.events_wait(),
            ),
        };
        let focus_border = focus_ink(cx);
        let rail_ghost = border_rail(cx);
        v_flex()
            .size_full()
            .min_h(px(0.))
            .child(self.render_inspector_toolbar(cx, "Events"))
            .child(
                div()
                    .id("events-scroll")
                    .debug_selector(|| "events-scroll".to_owned())
                    .role(Role::Region)
                    .aria_label("Events, newest first. Use the arrow keys to scroll.")
                    .aria_keyshortcuts("ArrowUp ArrowDown PageUp PageDown Home End")
                    .track_focus(&self.events_focus)
                    .tab_index(INSPECTOR_EVENTS_SCROLL_TAB_INDEX)
                    // The rail is always reserved, so taking and leaving focus cannot slide the
                    // list sideways.
                    .border_l_2()
                    .border_color(rail_ghost)
                    .focus_visible(move |style| style.border_color(focus_border))
                    .on_key_down(cx.listener(Self::on_events_key_down))
                    .flex_1()
                    .min_h(px(0.))
                    .min_w(px(0.))
                    .child(body),
            )
            .into_any_element()
    }

    fn events_list(&self, events: Arc<Vec<DynamicObject>>) -> AnyElement {
        let count = events.len();
        let aria_label = self.selection.as_ref().map_or_else(
            || "Events, newest first".to_owned(),
            |selection| {
                format!(
                    "Events for {}, newest first",
                    object_accessible_identity(selection)
                )
            },
        );
        // `UI-SPEC.md` §10.3: an Event is not tabular, it is a story in time. The timeline keeps
        // the server's own order, which is the order the reader has to reason about, and the
        // uniform list still virtualises it so a busy namespace's 200 events cost a screen.
        let list = uniform_list("inspector-events-list", count, move |range, _, cx| {
            range
                .filter_map(|index| {
                    events.get(index).map(|event| {
                        event_timeline_row(event, index + 1 == count, Some(EVENT_MESSAGE_LINES), cx)
                    })
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.events_scroll)
        .debug_selector(|| "inspector-events-list".to_owned())
        .size_full();
        div()
            .id("inspector-events-list")
            .size_full()
            .role(Role::List)
            .aria_label(aria_label)
            .child(list)
            .into_any_element()
    }

    // Metrics rendering

    fn render_metrics_toolbar(&self, target: &MetricsTarget, cx: &Context<Self>) -> AnyElement {
        let full_title = metrics_identity(target);
        // No object name and no leading sentence in this band, and that is the whole change.
        //
        // The identity header is directly above and already prints the kind, the name and the
        // namespace at `title 15/600` in the primary ink, so a second copy here was the same
        // sentence twice on one screen — and the YAML and Describe bands had already been fixed for
        // exactly this, which left the panel contradicting its own decision on whichever tab was
        // open.
        //
        // The full identity has not gone anywhere: it is the Reload control's tooltip and its
        // accessible name, and the region's accessible name below, which is where a name fits and
        // where a reader who is looking for it actually reads it.
        //
        // The sample *window* used to be a visible note beside the range buttons. It is not
        // missed: the window is the range the reader has selected, and the selected segment says
        // so in the same place they are looking. It is still named for assistive technology,
        // where "which of these six is current" is a question with no visual answer.
        let window_note =
            (!self.metrics.window.is_empty()).then(|| format!(" Window {}.", self.metrics.window));
        let words = match self.metrics.series.len() {
            0 => "Metrics samples".to_owned(),
            1 => "Metrics sample".to_owned(),
            count => format!("Metrics samples · {}", design::format::count(count)),
        };
        // The band's accessible name, carrying the object's full identity — which is the one
        // fact the identity band above it does not repeat on the same screen — and the sample
        // count, which the chart reports in its own readout.
        let aria = format!(
            "{words} for {full_title}.{}",
            window_note.unwrap_or_default()
        );
        let reload_label = format!("Reload metrics for {full_title}");
        // The range is one object, not six buttons. It was a row of independent ghost controls
        // separated by 1px gaps, so the group had no outside edge, every segment read as its own
        // control, and the selected fill was a pill floating between two neighbours — the stock
        // segmented web control the design language rules out by name.
        //
        // One plane and one silhouette instead: the group is a single inset field with a
        // hairline boundary, the two end segments carry the container's radius and the four
        // between them stay square, and the boundary between neighbours is one 1px rule of the
        // same weight throughout rather than a gap whose width is the *only* thing telling the
        // reader where one segment stops and the next begins.
        let group_plane = role::surface_inset(cx);
        let group_edge = role::border_subtle(cx);
        let group_hover = design::state::hover(cx, group_plane);
        let group_press = design::state::press(cx, group_plane);
        // The selected fill is the accent over the group's own plane, so it is the accent at the
        // strength the rest of the app tints with rather than a second, stronger accent.
        let selected_plane = design::composite_surface(group_plane, role::accent_wash(cx));
        let features = range_features(cx);
        let range = RANGE_OPTIONS
            .iter()
            .enumerate()
            .map(|(index, (millis, label))| {
                let selected = self.metrics_range_ms == *millis;
                let first = index == 0;
                let last = index + 1 == RANGE_OPTIONS.len();
                let selector = format!("metrics-range-action-{label}");
                let tooltip = format!("Show the last {label}");
                let control_id = format!("metrics-range-{}", *millis as usize);
                // Weight is the channel that still separates the open range when the accent is
                // hidden, which is the test every selection in this app is held to.
                let ink = if selected {
                    role::fg_primary(cx)
                } else {
                    role::fg_secondary(cx)
                };
                // The button paints no plane of its own: a custom variant is transparent in every
                // state until it is told otherwise, so the wrapper below is the only thing that
                // wears a fill. That is deliberate — `ButtonRounded` is uniform, so a fill on the
                // button would round an end segment's *inner* corners as well as its outer ones
                // and notch the group's plane into the selection. The geometry has to live on a
                // wrapper that can round per corner, and the button keeps what only it can do:
                // the hit target, the tab stop, the toggle state and the click.
                let button = common::labelled(
                    Button::new(control_id.clone())
                        .custom(ButtonCustomVariant::new(cx).foreground(ink))
                        .text_color(ink)
                        // The same 28px control the reload button beside it uses: one band's
                        // controls are one height, and a 32px segment inside a 28px row would push
                        // the group's own boundary past the band. The height is stated because a
                        // labelled button at an explicit size only takes its padding from it, and a
                        // control whose height depends on the font is not one control.
                        .with_size(Size::Size(design::size::CONTROL))
                        .h(design::size::CONTROL)
                        .font_weight(if selected {
                            text::MEDIUM
                        } else {
                            text::REGULAR
                        })
                        .tab_index(METRICS_RANGE_TAB_INDEX + index as isize)
                        .toggled(selected)
                        .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                            panel.set_metrics_range(*millis, cx);
                        })),
                    *label,
                )
                // The two words differ on purpose: a segment reads `1m`, which is a number with
                // nothing to hang a sentence on, so the announced name says what the segment
                // actually shows.
                .accessibility_label(format!("Show the last {label}"));
                div()
                    .id(control_id)
                    .debug_selector(move || selector.clone())
                    .tooltip(common::hover_hint(tooltip))
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    // `1m 15m 1h 6h 24h` share a baseline only if every digit has the same
                    // advance, so the segments divide the group's width evenly instead of
                    // shifting as the reader changes range.
                    .font_features(features.clone())
                    .when(first, |segment| segment.rounded_l(radius::SM))
                    .when(last, |segment| segment.rounded_r(radius::SM))
                    // The last segment carries no rule: the group's own boundary is there.
                    .when(!last, |segment| {
                        segment.border_r_1().border_color(group_edge)
                    })
                    .when(selected, |segment| {
                        segment
                            .bg(selected_plane)
                            .hover(|segment| segment.bg(hover_on(selected_plane, role::accent(cx))))
                            .active(|segment| {
                                segment.bg(press_on(selected_plane, role::accent(cx)))
                            })
                    })
                    .when(!selected, |segment| {
                        segment
                            .hover(|segment| segment.bg(group_hover))
                            .active(|segment| segment.bg(group_press))
                    })
                    .child(button)
            })
            .collect::<Vec<_>>();
        // `flex_1`, not `flex_none`: the band right-aligns its contents, and a row that is wider
        // than the band under `justify_end` overflows *backwards* — the reload control's left edge
        // ends up outside the toolbar at 260px, which is what
        // `metrics_toolbar_actions_stay_inside_a_narrow_inspector` is there to catch. A row that
        // takes the band's width cannot overflow it, and the range group inside divides what is
        // left after the reload control.
        let actions = h_flex()
            .id("metrics-context-actions")
            .flex_1()
            .min_w(px(0.))
            .h(design::size::ROW)
            .gap(space::XS)
            .items_center()
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&self.action_scroll)
            .child(
                div()
                    .id("metrics-action-reload")
                    .debug_selector(|| "metrics-action-reload".to_owned())
                    .flex_none()
                    .tooltip(inspector_control_tooltip(
                        reload_label.clone(),
                        control_chord("k8s_inspector::RetryMetrics", cx),
                    ))
                    .child(
                        Button::new("metrics-reload")
                            .icon(design::glyph::action::reload())
                            .ghost()
                            // The glyph box is what gpui-kit derives the ICON from; the
                            // target is restated so the pointer still aims at a full
                            // `size::ICON_BUTTON`. Shrinking both together takes the hit
                            // area down with the glyph, which is how three of these left
                            // the toolbar's own vertical centre.
                            .with_size(Size::Size(icon_control_box()))
                            .w(design::size::ICON_BUTTON)
                            .h(design::size::ICON_BUTTON)
                            // The same ink the Describe band's reload wears, for the reason
                            // stated there: an unset glyph colour resolves against the
                            // window foreground and put this control a whole tier above
                            // every other mark on a band whose job is to re-read.
                            .text_color(design::icon::resting(cx))
                            .accessibility_label(reload_label)
                            .tab_index(INSPECTOR_RELOAD_TAB_INDEX)
                            .on_click(cx.listener(|this, _, _, cx| this.retry_metrics(cx))),
                    )
                    .into_any_element(),
            )
            // No text label: the durations are self-describing, and a label would push the last
            // one out of a 260px panel.
            //
            // The group takes what is left rather than asking for its own width, so the six
            // segments divide one row instead of overflowing it. The boundary is the group's own
            // hairline rather than a gap between the segments: a gap is negative space and reads
            // as separation, while a rule reads as the edge of one object, and the whole point of
            // the control is that `1m … 7d` is one choice with six answers.
            .child(
                h_flex()
                    .id("metrics-range-group")
                    .flex_1()
                    .min_w(px(0.))
                    .rounded(radius::SM)
                    .bg(group_plane)
                    .border_1()
                    .border_color(group_edge)
                    .role(Role::Group)
                    .aria_label("Chart range")
                    .children(range),
            )
            .into_any_element();
        inspector_toolbar("metrics-context-toolbar", aria, None, actions, cx)
    }

    fn render_metrics(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(target) = self.metrics_target.clone() else {
            return inspector_empty(IconName::Cpu, "Select a node or pod to see its usage", cx);
        };
        let focus_border = focus_ink(cx);
        let rail_ghost = border_rail(cx);
        let mut body = v_flex()
            .id("metrics-scroll")
            .debug_selector(|| "metrics-scroll".to_owned())
            .role(Role::Region)
            .aria_label("Metrics. Use the arrow keys to scroll.")
            .aria_keyshortcuts("ArrowUp ArrowDown PageUp PageDown Home End")
            .track_focus(&self.metrics_focus)
            .tab_index(INSPECTOR_METRICS_SCROLL_TAB_INDEX)
            // The rail is always reserved, so taking and leaving focus cannot slide the charts
            // sideways.
            .border_l_2()
            .border_color(rail_ghost)
            .focus_visible(move |style| style.border_color(focus_border))
            .on_key_down(cx.listener(Self::on_metrics_key_down))
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.metrics_scroll)
            .p(space::SM)
            .gap(space::MD);
        match self.metrics_probe.clone() {
            MetricsProbeState::Checking => {
                body = body.child(self.render_metrics_checking(cx));
            }
            MetricsProbeState::Missing => {
                body = body.child(self.render_metrics_failure(
                    "Metrics unavailable",
                    "Install or enable metrics-server, then retry.",
                    METRICS_UNAVAILABLE.to_owned(),
                    None,
                    Severity::Warning,
                    cx,
                ));
            }
            MetricsProbeState::Forbidden { reason } => {
                body = body.child(self.render_metrics_failure(
                    "Not allowed to read metrics",
                    "Grant read access to metrics.k8s.io, then retry.",
                    reason,
                    None,
                    Severity::Warning,
                    cx,
                ));
            }
            MetricsProbeState::Error { reason } => {
                body = body.child(self.render_metrics_failure(
                    "Failed to check metrics",
                    CHECK_CONNECTION_AND_RETRY,
                    reason,
                    None,
                    Severity::Error,
                    cx,
                ));
            }
            MetricsProbeState::Available => {
                if self.metrics.is_empty() {
                    if let Some(error) = &self.metrics.last_error {
                        body = body.child(self.render_metrics_failure(
                            "Failed to sample metrics",
                            CHECK_CONNECTION_AND_RETRY,
                            error.clone(),
                            retry_delay_text(&self.metrics_scheduler),
                            Severity::Error,
                            cx,
                        ));
                    } else {
                        body = body.child(self.render_metrics_waiting(cx));
                    }
                } else {
                    if let Some(error) = &self.metrics.last_error {
                        body = body.child(self.render_metrics_status(error.clone(), cx));
                    }
                    body = body
                        .child(self.render_metric_section(
                            "CPU",
                            Unit::Cpu,
                            &self.cpu_chart,
                            "metrics-cpu-table",
                            cx,
                        ))
                        .child(self.render_metric_section(
                            "Memory",
                            Unit::Memory,
                            &self.memory_chart,
                            "metrics-memory-table",
                            cx,
                        ));
                }
            }
        }
        v_flex()
            .size_full()
            .min_h(px(0.))
            .bg(role::surface_content(cx))
            .child(self.render_metrics_toolbar(&target, cx))
            .child(body)
            .into_any_element()
    }

    /// The metrics empty state's one control.
    ///
    /// `outline`, for the reason the Describe failure state's retry is: primary is reserved for
    /// the explicit default *commit* in a decision area, and asking a read to run again is not a
    /// commit. It was primary, and it made a filled accent button the loudest thing in a 352px
    /// panel in a state whose own warning band has already said the read failed. The accent is
    /// worth more spent on Apply and on a destructive confirmation.
    ///
    /// It is also the same width as the other three Retry controls on this panel, which is what
    /// `design::size::HIT_MIN` as a floor over the label's own width gives for free. The 72px it
    /// used to ask for was a private literal for the same word, so the Metrics tab drew one verb
    /// fifteen pixels wider than the Describe tab does.
    fn metrics_retry_button(&self, cx: &Context<Self>) -> AnyElement {
        Button::new("metrics-retry")
            .label("Retry")
            .outline()
            .min_w(design::size::HIT_MIN)
            .tab_index(METRICS_RETRY_TAB_INDEX)
            .accessibility_label("Retry metrics")
            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| panel.retry_metrics(cx)))
            .into_any_element()
    }

    /// The frame every state that owns the whole body shares.
    ///
    /// The Describe and Events tabs already have one — `inspector_empty` for nothing to show,
    /// `loading_state` for a read in flight, `load_error` for a read that failed — and the Metrics
    /// tab had three of its own, sitting at the top of the body in `body 13/400` with 24px of air.
    /// Five states on one panel that each place their title at a different height and a different
    /// size is a panel that jumps every time a tab is switched.
    ///
    /// Centred rather than top-aligned because the state *is* the body: these are the branches
    /// where no chart is drawn, so there is nothing above them and nothing below. `space::XXL` is
    /// the "empty-state breathing room" step, and the reason it is not `XXXL` like
    /// `inspector_empty` is that this body is a scroll region with its own 8px padding, so the
    /// two states would otherwise disagree by 16px on the same panel.
    fn metrics_state_frame(id: &'static str) -> Stateful<Div> {
        v_flex()
            .id(id)
            .flex_1()
            .min_h(px(0.))
            .items_center()
            .justify_center()
            .gap(space::SM)
            .py(space::XXL)
            .px(space::LG)
    }

    fn render_metrics_checking(&self, cx: &Context<Self>) -> AnyElement {
        Self::metrics_state_frame("metrics-checking")
            .role(Role::Status)
            .aria_label("Checking metrics availability")
            .child(waiting_glyph(cx))
            .child(common::label_panel_title("Checking metrics availability"))
            .child(label_body("Looking for metrics-server"))
            .into_any_element()
    }

    fn render_metrics_waiting(&self, cx: &Context<Self>) -> AnyElement {
        Self::metrics_state_frame("metrics-waiting")
            .role(Role::Status)
            .aria_label("Waiting for the first sample")
            .child(waiting_glyph(cx))
            .child(common::label_panel_title("Waiting for the first sample"))
            .child(label_body(sampling_interval_text(
                self.metrics_scheduler.interval(),
            )))
            .into_any_element()
    }

    fn render_metrics_status(&self, reason: String, cx: &Context<Self>) -> AnyElement {
        // The countdown is a word, so it takes the word ink of the channel rather than the mark
        // ink: `Severity::Info::marker` is solved for a 6px dot and this is a sentence.
        let info_word = Severity::Info.word(cx);
        let status = h_flex()
            .id("metrics-sample-status")
            .w_full()
            .gap(space::SM)
            .items_center()
            .role(Role::Alert)
            .aria_label("Metrics sampling failed")
            .aria_description("Retry metrics.")
            .tooltip(common::hover_hint(reason))
            .child(status_message(
                Severity::Error,
                "Failed to sample metrics",
                None,
                cx,
            ))
            .child(self.metrics_retry_button(cx));
        v_flex()
            .w_full()
            .min_w(px(0.))
            .gap(space::XS)
            .child(status)
            // The countdown answers "how long do I wait", so it gets its own line and
            // the info colour instead of sharing the hint's weight.
            .when_some(
                retry_delay_text(&self.metrics_scheduler),
                |this, backoff| this.child(label_small(backoff).text_color(info_word)),
            )
            .into_any_element()
    }

    fn render_metrics_failure(
        &self,
        title: &'static str,
        hint: &'static str,
        reason: String,
        backoff: Option<String>,
        severity: Severity,
        cx: &Context<Self>,
    ) -> AnyElement {
        // A word, so the word ink: the mark ink is solved for a 6px dot and the countdown is a
        // sentence a reader waits on.
        let info = Severity::Info.word(cx);
        Self::metrics_state_frame("metrics-failure")
            .role(Role::Alert)
            .aria_label(format!("{title}. {hint}"))
            .tooltip(common::hover_hint(reason))
            .child(
                Icon::new(IconName::TriangleAlert)
                    .with_size(state_icon_size())
                    .text_color(severity.marker(cx)),
            )
            .child(common::label_panel_title(title))
            .child(label_body(hint))
            // The countdown answers "how long do I wait": its own line, info colour.
            .when_some(backoff, |this, backoff| {
                this.child(label_small(backoff).text_color(info))
            })
            .child(self.metrics_retry_button(cx))
            .into_any_element()
    }

    fn render_metric_section(
        &self,
        title: &'static str,
        unit: Unit,
        chart: &Entity<LineChartView>,
        table_id: &'static str,
        cx: &Context<Self>,
    ) -> AnyElement {
        let muted = role::fg_tertiary(cx);
        let data = chart.read(cx).data_rc();
        // The current value is the one number on this row, and it is a sample, so it is set in
        // the data role at the size the reader configured. A sample table and a chart caption
        // that disagree about the data font size are two readings of the same measurement.
        let data_typography = crate::settings::data_typography(cx);
        // The series name trails the block name as a metadata label, separated by a
        // middle dot: `CPU · etcd` cannot read as one missing space.
        let series_names = h_flex()
            .flex_1()
            .min_w(px(0.))
            .overflow_hidden()
            .gap(space::XS)
            .items_center()
            .child(div().flex_none().child(label_small("·").text_color(muted)))
            .children(self.metrics.series.iter().map(|series| {
                div()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(
                        label_small(series.name.clone())
                            .text_color(muted)
                            .truncate(),
                    )
            }));
        // The current value is the only number on this row, so it sits on the trailing
        // edge of the plot area instead of floating in the middle. It shrinks before the
        // series names do, so a Pod with many containers cannot push the row wide.
        let current = h_flex()
            .flex_shrink_1()
            .min_w(px(0.))
            .gap(space::SM)
            .items_center()
            .children(self.metrics.series.iter().map(|series| {
                let latest = match unit {
                    Unit::Cpu => series.latest_cpu(),
                    _ => series.latest_memory(),
                };
                let text = latest.map_or_else(|| "—".to_owned(), |value| unit.format(value));
                div()
                    .id((ElementId::from("metrics-current"), series.name.clone()))
                    .flex_shrink_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .tooltip(common::hover_hint(format!("{}: {text}", series.name)))
                    .child(
                        Label::new(text)
                            .text_size(px(f32::from(data_typography.size)))
                            .truncate(),
                    )
            }));
        let latest = h_flex()
            .w_full()
            .min_w(px(0.))
            .gap(space::XS)
            .items_center()
            // The same section head the Describe body wears: `CAPTION` uppercase semibold in the
            // tertiary ink. It was the title role — `title 15/600` in the primary ink — which is
            // the size and weight the *object's name* carries in the header 40px above, so the
            // words `CPU` and `Memory` were competing with the name of the thing they measure.
            // One hierarchy across the tab: the name first, the block second, the value third.
            .child(div().flex_none().child(section_label(title)))
            .child(series_names)
            .child(current);
        v_flex()
            .w_full()
            .min_w(px(0.))
            .gap(space::XS)
            .child(latest)
            .child(
                // The plot's own plane, and the reason it is the inset step and not a card with a
                // border. `charts/element.rs` says out loud that "the surface around the chart owns
                // the visible frame" — and this one did not: it carried a `radius::SM` and nothing
                // behind it, so the radius governed nothing and the grid, the axis labels and the
                // legend all sat on the same plane as the rest of the body with no edge to end on.
                // `diff_frame` sets the same inset well around the diff for the same reason, and
                // the sample table below keeps the content plane because it paints its own header
                // band and its own rows in that colour — an inset well around it would show a
                // content-coloured block sitting on an inset one.
                div()
                    .w_full()
                    .min_w(px(0.))
                    .h(px(METRICS_CHART_HEIGHT))
                    .rounded(radius::SM)
                    .bg(role::surface_inset(cx))
                    .child(chart.clone()),
            )
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .h(px(METRICS_TABLE_HEIGHT))
                    .rounded(radius::SM)
                    .overflow_hidden()
                    .child(ChartTable::new(table_id, data)),
            )
            .into_any_element()
    }
}

fn tab_scroll_index(index: usize) -> usize {
    index
}

fn tab_focus_target(current: usize, count: usize, key: &str) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let current = current.min(count - 1);
    match key {
        "left" | "up" => Some((current + count - 1) % count),
        "right" | "down" => Some((current + 1) % count),
        "home" => Some(0),
        "end" => Some(count - 1),
        _ => None,
    }
}

fn object_display_identity(object: &ObjectRef) -> String {
    let kind = if object.resource.kind.is_empty() {
        "Resource"
    } else {
        object.resource.kind.as_str()
    };
    let name = if object.name.is_empty() {
        object.uid.as_str()
    } else {
        object.name.as_str()
    };
    match object
        .namespace
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        Some(namespace) => format!("{kind} {name} in namespace {namespace}"),
        None => format!("{kind} {name}"),
    }
}

/// The key a read is cached under.
///
/// It is the uid when the object has one, because a uid is the only identifier that cannot be
/// reused: a name can be freed and taken by a different object, and a Describe that answered for
/// the new occupant would be worse than no answer. It falls back to the object's full identity
/// only for the objects the panel *reached* by name — following a Pod's `spec.nodeName` has to
/// name a Node whose uid this request never carried — and that fallback is why the caches cannot
/// be keyed on the uid alone: two Nodes followed in a row would otherwise share one entry and the
/// second would be answered with the first's fields.
fn cache_key(object: &ObjectRef) -> String {
    if !object.uid.is_empty() {
        return object.uid.clone();
    }
    format!(
        "{}/{}/{}",
        object.resource.kind,
        object.namespace.as_deref().unwrap_or("-"),
        object.name
    )
}

fn object_accessible_identity(object: &ObjectRef) -> String {
    let display = object_display_identity(object);
    if !object.uid.is_empty() {
        format!("{display}. UID {}", object.uid)
    } else {
        display
    }
}

fn yaml_matches_target(target: &ApplyTarget, yaml: &str) -> bool {
    let Ok(object) = serde_yaml_ng::from_str::<DynamicObject>(yaml) else {
        return false;
    };
    if object
        .metadata
        .name
        .as_deref()
        .is_some_and(|name| name != target.name)
    {
        return false;
    }
    match (
        target.namespace.as_deref(),
        object.metadata.namespace.as_deref(),
    ) {
        (Some(expected), Some(actual)) if expected != actual => return false,
        (None, Some(actual)) if !actual.is_empty() => return false,
        _ => {}
    }
    if object.metadata.uid.as_deref() != Some(target.uid.as_str()) {
        return false;
    }
    object.types.as_ref().is_none_or(|types| {
        types.api_version == target.resource.api_version && types.kind == target.resource.kind
    })
}
impl Render for InspectorPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.yaml_observation.is_none() {
            let editor = self.yaml_view.clone();
            self.yaml_observation = Some(cx.observe(&editor, |panel, editor, cx| {
                if panel.problems_changed(editor.read(cx).diagnostics()) {
                    cx.notify();
                }
            }));
        }
        let active_tab = self.active_tab.min(self.tab_count().saturating_sub(1));
        // `WRITE-OPS.md` §3.3: the apply diff is a *mode* of the Inspector, not a strip inside
        // the YAML tab and not a popover. The reason is context — the table the change is about
        // stays visible on the left, and the reader's eyes are already in this column — so while
        // a review is open the tab strip is replaced by the Diff header and the whole content
        // region is the review. `Esc` drops back to the tab it came from with the unsaved edits
        // still in the editor, which is what `cancel_pending_apply` already guarantees.
        let diff_mode = self.pending_apply.is_some();
        let (header, content): (AnyElement, AnyElement) = if diff_mode {
            (
                self.render_diff_header(cx),
                self.render_apply_review(cx)
                    .unwrap_or_else(|| v_flex().size_full().into_any_element()),
            )
        } else {
            (
                self.render_tabs(cx),
                match active_tab {
                    0 => self.render_yaml(cx),
                    1 => self.render_describe(cx),
                    2 => self.render_events(cx),
                    _ => self.render_metrics(cx),
                },
            )
        };
        let rail_ghost = border_rail(cx);
        let focus_border = focus_ink(cx);
        let tab_label = self.tab_label(active_tab);
        // The panel is a tab panel, so the label names both the tab and the object it shows.
        let inspector_label = self.selection.as_ref().map_or_else(
            || format!("{tab_label}. Inspector. No resource selected."),
            |selection| {
                format!(
                    "{tab_label}. Inspector for {}",
                    object_accessible_identity(selection)
                )
            },
        );
        // Two questions, two answers, and the panel needs both. Has the shell floated this panel
        // over the centre? That is a function of the window width — the same function the shell
        // runs — and the frame it floats over paints nothing, so the surface is this panel's to
        // own. Is the panel itself too narrow to read as a column? That is a function of the
        // width the shell gave it, and it is the same question at the panel's own narrow end.
        let overlay =
            self.floating_frame(f32::from(window.viewport_size().width)) || self.overlay_frame();
        let surface = if overlay {
            role::surface_overlay(cx)
        } else {
            role::surface_content(cx)
        };
        let measured_width = self.panel_width.clone();
        let panel = cx.entity().downgrade();
        // The panel's own width, measured on the frame itself. `describe_width` measures the
        // Describe *body*; this one measures the column, because the overlay decision is about
        // the space the shell gave the whole panel.
        div()
            .flex()
            .flex_col()
            .on_children_prepainted(move |children, window, cx| {
                let Some(bounds) = children.first() else {
                    return;
                };
                let width = f32::from(bounds.size.width);
                if width.is_finite() && width > 0.0 && (measured_width.get() - width).abs() > 0.5 {
                    measured_width.set(width);
                    let panel = panel.clone();
                    window.defer(cx, move |_, cx| {
                        if let Some(panel) = panel.upgrade() {
                            panel.update(cx, |_, cx| cx.notify());
                        }
                    });
                }
            })
            .id("inspector-frame")
            .debug_selector(|| "inspector-frame".to_owned())
            // `⌘L` is the panel's, not the content region's: §4.19 puts the chord next to the
            // control and the control is in the header, so a reader who has just tabbed through
            // the tab strip has to reach the same action. Nothing else in the tree claims the
            // chord, and a handler that only answers while the content region is focused would
            // make the shortcut's reach depend on which tab happened to be open.
            .on_key_down(cx.listener(Self::frame_key_down))
            .size_full()
            .min_w(px(0.))
            .child(
                v_flex()
                    .size_full()
                    .min_w(px(0.))
                    .bg(surface)
                    .text_color(role::fg_primary(cx))
                    .key_context("Inspector")
                    .when(overlay, |this| {
                        // A floating panel is one of the three places a shadow is allowed
                        // (`PROMPT.md` §4) and one of the three places a border is. Docked, the
                        // panel is a column: no border, no radius, no shadow, because a shadow
                        // on a column is the single loudest signal that a product was decorated
                        // rather than designed.
                        //
                        // `shadow::overlay` rather than `shadow::popover`: this is not a menu
                        // anchored to something, it is a third of the window laid over the table
                        // the reader is looking at, and the popover step is too small a
                        // separation for that much surface. `radius::LG` is the panel tier, the
                        // same corner a floating card in the dock uses.
                        this.rounded(radius::LG)
                            .border_1()
                            .border_color(role::border_base(cx))
                            .shadow(design::shadow::overlay(cx))
                    })
                    .child(self.render_identity(cx, overlay))
                    .child(header)
                    // Between the tab strip and the content, so it is on screen whichever tab is
                    // open — a stale YAML is as wrong as a stale field list, and the reader who
                    // deleted the object does not know which tab they will notice on.
                    .when_some(self.render_gone_banner(cx), |this, banner| {
                        this.child(banner)
                    })
                    .child(
                        div()
                            .id("inspector-content")
                            .role(Role::TabPanel)
                            .aria_label(inspector_label)
                            .accessibility_id(format!("inspector-panel-{active_tab}"))
                            .flex_1()
                            .min_h(px(0.))
                            .min_w(px(0.))
                            .overflow_hidden()
                            // The radius has to govern the whole visible surface, and the two
                            // bands that reach the frame's edges are what a rounded parent does
                            // not clip: the identity band paints `surface.chrome` across the top
                            // and the content region paints the body plane across the bottom, so
                            // without these the floating panel's corners are square fills sitting
                            // on top of a rounded frame — the radius drawn and then covered.
                            .when(overlay, |this| {
                                this.rounded_bl(radius::LG).rounded_br(radius::LG)
                            })
                            .track_focus(&self.focus_handle)
                            .tab_index(INSPECTOR_CONTENT_TAB_INDEX)
                            // The rail is always reserved, so taking and leaving focus cannot
                            // slide the tab panel sideways.
                            .border_l_2()
                            .border_color(rail_ghost)
                            .focus_visible(move |style| style.border_color(focus_border))
                            .on_action(cx.listener(Self::reload_action))
                            .on_action(cx.listener(Self::metrics_retry_action))
                            .on_action(cx.listener(Self::range_1m))
                            .on_action(cx.listener(Self::range_15m))
                            .on_action(cx.listener(Self::range_1h))
                            .on_action(cx.listener(Self::range_6h))
                            .on_action(cx.listener(Self::range_24h))
                            .on_action(cx.listener(Self::range_7d))
                            .on_action(cx.listener(Self::confirm_action))
                            .on_action(cx.listener(Self::cancel_review_action))
                            .on_action(cx.listener(Self::revert_action))
                            .on_action(cx.listener(Self::copy_yaml_action))
                            .on_action(cx.listener(Self::toggle_value_action))
                            .on_action(cx.listener(Self::copy_value_action))
                            .on_action(cx.listener(Self::next_problem_action))
                            .child(content),
                    ),
            )
            .into_any_element()
    }
}

/// A toolbar band shared by the YAML, context, and metrics toolbars.
///
/// The three bars were written out three times, so a change to the band reached one of them. The
/// contents stay with their tab; only the frame, the height, and the hairline live here.
///
/// The band is `design::size::TOOLBAR` and not the dock's 28px `DOCK_TOOLBAR`, and the arithmetic
/// is the same one the shared icon button already makes: the controls in here are
/// `design::size::CONTROL` (28px) and a 28px control plus the pixel a focus ring reserves will not
/// fit a 28px row. So this is the smallest step in the scale that holds its own contents, and it
/// is the step every other band in this panel uses — the identity band and the tab strip are the
/// same 28px `TAB_BAR`, and the panel's other bands are this one. The three toolbars are
/// therefore one shape by construction rather than by agreement: same surface, same height, same
/// inset, same hairline, and the same leading slot — a 24px glyph, then what the tab is showing,
/// which the identity header above cannot say because it names the object and not the document.
/// One toolbar band: the region's own name for assistive technology, and its commands.
///
/// **No leading slot.** It carried one through three revisions — first a bare `list` glyph that
/// named nothing, then a glyph plus the sentence `Resource details` / `Object events` /
/// `Metrics samples · 5`, then a glyph alone again — and every version was wrong for the same
/// reason. The kind mark is already on the object's name in the identity band directly above, so
/// a second one is the same picture twice; and the words are the tab's own label, which the tab
/// strip 28px higher is already saying. What is left is a non-interactive glyph sitting where a
/// control would be, on a row whose only other child is a button — so the band read as two
/// ornaments and one of them looked like something you could press.
///
/// The commands take the trailing edge, which is where a document-level control belongs, and the
/// region's name — the one fact a toolbar has that the identity band deliberately does not
/// repeat on the same screen — is the band's accessible name.
fn inspector_toolbar(
    id: &'static str,
    aria: String,
    leading: Option<AnyElement>,
    actions: AnyElement,
    cx: &Context<InspectorPanel>,
) -> AnyElement {
    h_flex()
        .id(id)
        .debug_selector(move || id.to_owned())
        .flex_none()
        .w_full()
        .min_w(px(0.))
        .h(design::size::TOOLBAR)
        .px(space::SM)
        .gap(space::SM)
        .items_center()
        .justify_end()
        .bg(role::surface_chrome(cx))
        // One hairline per boundary, owned by the region above it. This is the boundary between
        // the navigation block and the work, and the toolbar is the last band of that block, so it
        // is the band that owns it.
        .border_b_1()
        .border_color(role::border_subtle(cx))
        .role(Role::Toolbar)
        .aria_label(aria)
        .when_some(leading, |band, slot| band.child(slot))
        .child(actions)
        .into_any_element()
}

/// Tooltip for a control that also has a key: the label, then the chord that reaches the same
/// action, so a shortcut is discoverable from the surface that owns it.
///
/// The chord comes from the keymap, so a control with no binding yet shows its label alone rather
/// than advertising a key that does nothing.
fn inspector_control_tooltip(
    label: impl Into<SharedString>,
    chord: Option<Keystroke>,
) -> impl Fn(&mut Window, &mut App) -> AnyView {
    let label = label.into();
    move |window, cx| {
        Tooltip::new(label.clone())
            .key_binding(chord.clone().map(Kbd::new))
            .build(window, cx)
    }
}

/// The chord a control advertises, read from the keymap so the hint cannot drift from the key.
///
/// The keymap is asked directly rather than through the rendered frame: the hint is drawn on a
/// surface that may not hold focus, and a chord that only resolves for the focused surface would
/// come and go with the pointer.
fn control_chord(action: &str, cx: &App) -> Option<Keystroke> {
    crate::keymap::binding_for_context(action, "Inspector", cx)
        .and_then(|chord| Keystroke::parse(&chord).ok())
}

/// The keycap for one of a row's own chords, drawn from the same keymap the command runs from.
fn chord_keycap(action: &str, cx: &App) -> Option<Kbd> {
    control_chord(action, cx).map(Kbd::new)
}
/// The border that reserves a focus rail without painting it.
///
/// The rail is always in the layout, so taking and leaving focus cannot slide the content
/// sideways; only the focused state gives it ink.
fn border_rail(cx: &App) -> Hsla {
    role::border_subtle(cx).alpha(0.)
}

/// Tabular figures, taken from the reader's configured data face.
///
/// The range values are the one place the panel prints values that sit side by side and have to
/// line up, and `design::text` carries no feature token — the same reason `panels/dock.rs` reaches
/// for the setting rather than carrying a private list of feature tags that could disagree with it.
fn range_features(cx: &App) -> FontFeatures {
    crate::settings::data_typography(cx).features
}

/// The size an Inspector empty or waiting state leads with.
///
/// The shared empty state leads with the same twenty-four pixels, and this state sits on the same
/// screen, so it takes the same number: two empty states at different sizes are two designs, and
/// `DESIGN.md` §3.3 lists the icon sizes as fixed dimensions.
fn state_icon_size() -> Size {
    Size::Size(design::icon::LEAD)
}

/// The box of an icon-only control, chosen so the *glyph* lands on the design's row-and-toolbar
/// lane.
///
/// gpui-kit reads `Size::Size(px)` on a button that carries no label as the whole box, and takes
/// the mark at 0.75 of it, overwriting whatever the caller named on the mark — so the box is the
/// only lever there is, and a button that asks for `design::size::CONTROL` does not get a 28px
/// glyph, it gets a 21px one. This panel had two of those boxes: `design::size::ICON_BUTTON`,
/// which draws an eighteen-pixel glyph, and `design::size::CONTROL`, which draws a
/// twenty-one-pixel one, four controls at the second and three at the first, in one column.
///
/// Neither number is the lane. `design::icon::IN_TOOLBAR` and `design::icon::IN_ROW` are both
/// sixteen, because the toolbar marks and the marks beside body text are meant to be one weight
/// on this panel — so sixteen is the glyph, and the box that draws it is this.
///
/// `shell/panels.rs::toolbar_glyph_box` is the same arithmetic stated for the title bar; it is a
/// function in both places because `Pixels` division is not a `const` operation, and it belongs
/// beside the size tokens as a named constant rather than in either panel when either panel can
/// reach it. It is not a fourth size: pass the result of this to `with_size` and nowhere else.
fn icon_control_box() -> Pixels {
    Pixels::from(f32::from(design::icon::IN_TOOLBAR) / 0.75)
}

/// A waiting marker, at the size an Inspector state uses.
///
/// The sweep belongs to the shared `spinner`, which also honors reduce motion, so nothing here
/// reads that setting.
fn waiting_glyph(cx: &App) -> AnyElement {
    spinner(
        IconName::LoaderCircle,
        design::role::accent(cx),
        state_icon_size(),
    )
}

/// A kind's own glyph, in the twelve bespoke shapes.
///
/// `design::kind_icon_path` is the asset for a kind that has a shape and the same stand-in for
/// every kind that does not, which is what stops a set of seventy-one kinds from looking like
/// seventy-one separate decisions. `design::kind_icon` answers "which chrome category", not
/// "which kind", so it is not the one that belongs beside an object's name.
fn kind_glyph(kind: &str, cx: &App) -> Div {
    div()
        .flex_none()
        .text_color(design::icon::resting(cx))
        .debug_selector(move || format!("kind-glyph-{kind}"))
}

/// Which ink one lane of the identity band's scope line wears.
///
/// An enum rather than an `Hsla` argument because the two lanes are a *hierarchy* and a hierarchy
/// is what a reader is meant to see: the namespace is the one the reader navigates by, so it is a
/// step above the kind and the age, and a caller that passed a colour could make the two equal
/// without anything in the code changing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentityLane {
    /// The namespace: `text::LABEL` at `text::MEDIUM` in the secondary ink.
    Namespace,
    /// The kind and the age: the same size in the tertiary ink.
    Quiet,
}

/// The height the identity band holds whether or not an object is selected.
///
/// The band is chrome, and chrome sized by what it happens to hold moves everything under it when
/// the holding changes. This one did: `design::size::ROW` with nothing selected and its own
/// contents with an object, so the tab strip, the three toolbars and the body sat 18px lower with
/// a row selected than with none. Both branches of `render_identity` answer this.
///
/// It is the band's own arithmetic in the tokens it is built from rather than a fourth number:
/// `space::XS` of padding above and below, the `design::size::ICON_BUTTON` the link control is —
/// which is what sets the name row's height, the name's own `TITLE_LINE_HEIGHT` being shorter —
/// one `space::XXS` gap, and the `design::text::LABEL_LINE_HEIGHT` the scope line draws at. Fifty,
/// and it measures fifty: the band's top rule is at device y=180 in a 2x capture with a Pod
/// selected and the tab strip's at y=182. `Pixels` arithmetic is not `const`, so the sum is a
/// function.
fn identity_band_height() -> Pixels {
    space::XS * 2. + design::size::ICON_BUTTON + space::XXS + design::text::LABEL_LINE_HEIGHT
}

/// One lane of the scope line under the object's name.
///
/// **Only the namespace may give way.** All three lanes were `flex_shrink_1`, so a long namespace
/// took its pressure out on the two lanes beside it and a 352px panel rendered the scope line as
/// `team-platform-data-ingestion-staging-eu…  ·  P…  ·  2d…` — three truncated fragments, the
/// worst of which is a kind cut to one letter, because `Pod` and `2d old` are fixed-width facts
/// and only the namespace is a variable-length one. Shrinking is therefore a property of the
/// lane, not of the box: the kind and the age are `flex_none` and render whole whatever the
/// namespace is doing, and the namespace absorbs all of it.
///
/// `flex_shrink_1` with `min_w(0)` rather than `flex_1`, because the lanes are one group that
/// reads left to right: a short namespace must sit against its kind and its age. `flex_1` would
/// spend the whole row on the namespace and push the age to the trailing edge of the panel, which
/// is the run-on this line was built to stop.
///
/// The tooltip is on the lane rather than on the row, so a namespace that clips says what it was
/// without the reader having to hover the age to find out.
fn identity_lane(value: &str, lane: IdentityLane, cx: &App) -> AnyElement {
    let prominent = lane == IdentityLane::Namespace;
    let ink = if prominent {
        role::fg_secondary(cx)
    } else {
        role::fg_tertiary(cx)
    };
    // The `id` is what makes the lane stateful, and stateful is what a tooltip attaches to: GPUI
    // hangs the hint off an element that owns an identity, which is the rule every clipped value
    // in this panel already depends on.
    div()
        .id(format!("inspector-identity-lane-{value}"))
        .when(prominent, |lane| {
            lane.flex_shrink_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
        })
        .when(!prominent, |lane| lane.flex_none())
        .tooltip(common::hover_hint(value.to_owned()))
        .child(
            Label::new(value.to_owned())
                .text_size(text::LABEL)
                .line_height(text::LABEL_LINE_HEIGHT)
                .font_weight(if prominent {
                    text::MEDIUM
                } else {
                    text::REGULAR
                })
                .text_color(ink),
        )
        .into_any_element()
}

/// The middot between two lanes of the scope line.
///
/// `role::fg_disabled`, which is the role for a separator dot — the product's own section head
/// puts one between a label and a count in the same ink. It is drawn as a `div` rather than as a
/// `Label` so it is `space::ICON` square in the band whatever the lanes around it do.
fn identity_separator(cx: &App) -> AnyElement {
    div()
        .flex_none()
        .text_size(text::LABEL)
        .line_height(text::LABEL_LINE_HEIGHT)
        .text_color(role::fg_disabled(cx))
        .child("·")
        .into_any_element()
}

/// A waiting state, at the tier the wait has actually reached.
///
/// `UI-SPEC.md` §4.14 grades the three, and the grading is the whole point: a skeleton over a
/// fetch that takes 180ms makes a fast operation feel slower than it is, and a spinner over a
/// fetch that takes ten seconds is a lie. `elapsed` is how long the request has been outstanding
/// when this renders, so the panel has to repaint as the wait crosses each boundary rather than
/// deciding once.
fn loading_state(
    cx: &App,
    title: &'static str,
    hint: &'static str,
    elapsed: Duration,
) -> AnyElement {
    // `UI-SPEC.md` §4.14 grades the wait, and the grading is the point: a skeleton over a fetch
    // that takes 180ms makes a fast operation feel slower than it is, and a bare spinner over a
    // fetch that takes ten seconds is a lie. There is no skeleton here at all — the panel has no
    // stale rows to keep on screen while it refetches, so the only honest thing is a spinner that
    // gets more specific the longer the reader waits.
    let slow = elapsed >= design::motion::SLOW * 20;
    v_flex()
        .id("inspector-loading")
        .debug_selector(|| "inspector-loading".to_owned())
        .size_full()
        .min_h(px(0.))
        .min_w(px(0.))
        .items_center()
        .justify_center()
        // The same air the empty and error states use — `space::XXL`, the scale's
        // "empty-state breathing room" step — so the three do not jump when one becomes another:
        // a spinner that sits 40px from the top and a title that sits 32px from it are two
        // different layouts for one idea. `metrics_state_frame` already used `XXL`; the other
        // three were `XXXL` on the argument that the region carried the state's whole height.
        .py(space::XXL)
        .gap(space::SM)
        .px(space::XL)
        .role(Role::Status)
        .aria_label(title)
        .aria_description(hint)
        .child(waiting_glyph(cx))
        .child(common::label_panel_title(title).text_color(role::fg_primary(cx)))
        .child(label_small(hint).text_color(role::fg_tertiary(cx)))
        // Past two seconds the wait stops being a moment and starts being a question, and §4.14
        // asks for progress at that point. The panel knows the deadline it armed, so it can say
        // when the request will be offered for retry rather than spinning indefinitely.
        .when(slow, |this| {
            this.child(
                label_small("Still waiting on the cluster. Retry becomes available at 10s.")
                    .text_color(role::fg_tertiary(cx)),
            )
        })
        .into_any_element()
}

/// The empty state for a panel with nothing selected.
///
/// `UI-SPEC.md` §4.13: an empty state is a 24px muted icon and one line, and today this region
/// prints `No resource details to show` and `Select a row to see its fields, owners, and
/// conditions.` at the same size as a field value, so the instruction and the data compete and
/// neither reads. Terse: the icon says "nothing here", the line says what would put something
/// here, and there is no second line.
fn inspector_empty(icon: IconName, title: &'static str, cx: &App) -> AnyElement {
    inspector_empty_with(icon, title, None, None, cx)
}

/// What one of those actions does when the reader takes it.
type StateHandler = Rc<dyn Fn(&mut App)>;

/// The one action an empty or failed state offers: a label and what it does.
///
/// A state that has something to offer says so in words *and* offers the thing. Naming the
/// action in a tuple rather than in a type of its own would leave every call site spelling the
/// same shape out again, and the shape is the part that has to agree.
type StateAction = (&'static str, StateHandler);

/// [`inspector_empty`] with the one optional line and the one optional action `UI-SPEC.md` §4.13
/// allows.
///
/// The description is off by default on purpose. An empty state that explains itself is a
/// paragraph competing with the table behind it, and the reason a panel is empty is almost always
/// one sentence: pick a row. The one case that earns a second line is a *failure* — a limit, a
/// permission, a filter — where the reader cannot act until they know which.
fn inspector_empty_with(
    icon: IconName,
    title: &'static str,
    hint: Option<&'static str>,
    action: Option<StateAction>,
    cx: &App,
) -> AnyElement {
    v_flex()
        .id("inspector-empty")
        .debug_selector(|| "inspector-empty".to_owned())
        .size_full()
        .min_h(px(0.))
        .min_w(px(0.))
        .items_center()
        .justify_center()
        // `space::XXL` above and below, the scale's empty-state step and the same one
        // `metrics_state_frame`, `loading_state` and `load_error` use, so the state sits in the
        // middle of the panel instead of clinging to the top of it and does not move when a read
        // turns into a failure. It was `XXXL` here and `XXL` on the Metrics tab, so the two
        // disagreed by 8px on the same panel.
        .py(space::XXL)
        .gap(space::MD)
        .px(space::LG)
        .role(Role::Region)
        .aria_label(title)
        .child(
            Icon::new(icon)
                .with_size(state_icon_size())
                // 24px in the tertiary ink. A large coloured glyph is the web empty state, and
                // this one has to read as "nothing here yet" rather than as a state of its own.
                .text_color(role::fg_tertiary(cx)),
        )
        // The title is the primary ink: it is the sentence the reader acts on, and §4.13 puts it
        // there. It was tertiary, which made the one line that mattered the quietest thing on
        // the panel.
        .child(common::label_panel_title(title).text_color(role::fg_primary(cx)))
        .when_some(hint, |this, hint| {
            this.child(
                div()
                    .max_w(ERROR_MEASURE)
                    .child(label_small(hint).text_color(role::fg_tertiary(cx))),
            )
        })
        .when_some(action, |this, (label, run)| {
            this.child(
                div()
                    .debug_selector(|| "inspector-empty-action".to_owned())
                    .child(
                        Button::new("inspector-empty-action-button")
                            .label(label)
                            .secondary()
                            .on_click(move |_, _, cx| run(cx)),
                    ),
            )
        })
        .into_any_element()
}

/// Identity of a metrics target in the same words [`object_display_identity`] uses.
fn metrics_identity(target: &MetricsTarget) -> String {
    match target {
        // A Node is cluster scoped, so it says so rather than leaving the reader to infer it
        // from an absent namespace.
        MetricsTarget::Node { name, .. } => format!("Node {name}"),
        MetricsTarget::Pod {
            namespace, name, ..
        } => {
            format!("Pod {name} in namespace {namespace}")
        }
    }
}

/// A local line diff between the text the cluster reported and the text to apply.
///
/// It runs on the two documents, not on the server, so it answers "what did I change" and never
/// "would the server accept this". A document pair too large to compare line by line falls back to
/// a count instead of an expensive table.
fn yaml_diff<'a>(old: &'a str, new: &'a str) -> Vec<DiffLine<'a>> {
    const MAX_CELLS: usize = 250_000;
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    if old_lines.len().saturating_mul(new_lines.len()) > MAX_CELLS {
        let mut summary = Vec::new();
        if !old_lines.is_empty() {
            summary.push(DiffLine::Removed("the previous document"));
        }
        if !new_lines.is_empty() {
            summary.push(DiffLine::Added("the edited document"));
        }
        return summary;
    }
    // Longest common subsequence over lines, walked forward into a unified list.
    let rows = old_lines.len() + 1;
    let columns = new_lines.len() + 1;
    let mut table = vec![0u32; rows * columns];
    for row in (0..old_lines.len()).rev() {
        for column in (0..new_lines.len()).rev() {
            table[row * columns + column] = if old_lines[row] == new_lines[column] {
                table[(row + 1) * columns + column + 1] + 1
            } else {
                table[(row + 1) * columns + column].max(table[row * columns + column + 1])
            };
        }
    }
    let mut diff = Vec::new();
    let (mut row, mut column) = (0usize, 0usize);
    while row < old_lines.len() && column < new_lines.len() {
        if old_lines[row] == new_lines[column] {
            diff.push(DiffLine::Context(old_lines[row]));
            row += 1;
            column += 1;
        } else if table[(row + 1) * columns + column] >= table[row * columns + column + 1] {
            diff.push(DiffLine::Removed(old_lines[row]));
            row += 1;
        } else {
            diff.push(DiffLine::Added(new_lines[column]));
            column += 1;
        }
    }
    while row < old_lines.len() {
        diff.push(DiffLine::Removed(old_lines[row]));
        row += 1;
    }
    while column < new_lines.len() {
        diff.push(DiffLine::Added(new_lines[column]));
        column += 1;
    }
    diff
}

/// What a read failure says, and the control that tries again.
///
/// One struct rather than six arguments: the copy, the reason and the retry's place in the tab
/// order travel together, and the only thing that differs between Describe, Events and the YAML
/// tab is which sentences are shown and where Retry sits.
struct LoadErrorParts {
    title: &'static str,
    hint: &'static str,
    retry_label: &'static str,
    reason: String,
    retry_tab_index: isize,
}

/// The shared read-failure state: a severity, an alert role, the reason, and a way to try again.
///
/// `retry_tab_index` is part of the state because each caller owns its own control, and two
/// controls that answered to the same tab index would be reachable by the same Tab press. The
/// YAML failure is the newest caller: it replaces the whole tab body, so it cannot take the
/// Describe or Events stop.
fn load_error(
    parts: LoadErrorParts,
    cx: &App,
    on_retry: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let LoadErrorParts {
        title,
        hint,
        retry_label,
        reason,
        retry_tab_index,
    } = parts;
    // `UI-SPEC.md` §4.15: the error appears where the read failed, and a permission failure says
    // what the identity *can* do rather than dumping the RBAC JSON. The reason itself is a
    // tooltip, because a cluster's message is a sentence for a cluster administrator, not the
    // first line an SRE reads at 3am.
    let permission = permission_hint(&reason);
    let copy_reason = permission_copy_action(&reason);
    v_flex()
        .id("inspector-load-error")
        .debug_selector(|| "inspector-load-error".to_owned())
        .w_full()
        .flex_1()
        .min_h(px(0.))
        .items_center()
        .justify_center()
        // The same air the empty and loading states use: `space::XXL`, one number for all four
        // states on this panel. Three states that each place their text at a different height is
        // one panel that jumps when a read fails.
        .py(space::XXL)
        .gap(space::SM)
        .px(space::LG)
        .role(Role::Alert)
        .aria_label(format!("{title}. {hint}"))
        .tooltip(common::hover_hint(reason))
        .child(
            Icon::new(design::health_icon(Severity::Error))
                .with_size(state_icon_size())
                .text_color(role::danger(cx)),
        )
        .child(common::label_panel_title(title).text_color(role::fg_primary(cx)))
        .child(
            div()
                .max_w(ERROR_MEASURE)
                .child(label_body(hint).text_color(role::fg_secondary(cx))),
        )
        .when_some(permission, |this, permission| {
            this.child(
                div()
                    .max_w(ERROR_MEASURE)
                    .child(label_small(permission).text_color(role::fg_tertiary(cx))),
            )
        })
        .child(
            h_flex()
                .gap(space::SM)
                .items_center()
                .child(
                    div().debug_selector(|| "inspector-retry".to_owned()).child(
                        // `Retry` is the verb phrase the design asks for. It is **not** primary,
                        // and it used to be, on the argument that being the only control on an
                        // error screen earns it. It does not: primary is reserved for "the
                        // explicit default commit in a decision area", and asking a read to run
                        // again is not a commit — nothing is at stake and nothing is decided. A
                        // filled accent button made the loudest thing in a 352px panel a request
                        // to try once more, in a state whose own band already says the read
                        // failed. `outline` gives it a boundary to be found by without spending
                        // the accent on it, and leaves the accent free for the two places that
                        // are genuinely a commit: Apply, and a destructive confirmation.
                        Button::new("inspector-retry")
                            .label("Retry")
                            .outline()
                            .min_w(design::size::HIT_MIN)
                            .tab_index(retry_tab_index)
                            .accessibility_label(retry_label)
                            .on_click(on_retry),
                    ),
                )
                // A permission failure gets a second control, and only that failure: an error a
                // reader can fix by trying again does not need a way to hand the message on.
                .when_some(copy_reason, |this, copy| {
                    this.child(
                        div()
                            .id("inspector-copy-reason")
                            .debug_selector(|| "inspector-copy-reason".to_owned())
                            .tooltip(common::hover_hint(
                                "Copy the cluster's own message, for a ticket or a role binding",
                            ))
                            .child(
                                Button::new("inspector-copy-reason-button")
                                    .label("Copy the reason")
                                    .ghost()
                                    .tab_index(retry_tab_index + 1)
                                    .accessibility_label("Copy the cluster's own error message")
                                    .on_click(move |_, _, cx| copy(cx)),
                            ),
                    )
                }),
        )
        .into_any_element()
}

/// What a permission failure says about what the identity *can* do.
///
/// `UI-SPEC.md` §4.15 asks for "what can / cannot / why" in words. The cluster's own message is
/// the why and it is already one tooltip away; what the panel can add on its own is the *can*,
/// because the Describe read succeeded or failed independently of the list read that put the
/// row on screen, so a denial here means this identity reads the object but not this part of
/// it. That is the sentence an SRE needs and the one no raw RBAC dump gives them.
fn permission_hint(reason: &str) -> Option<&'static str> {
    let lowered = reason.to_ascii_lowercase();
    let denied = lowered.contains("forbidden")
        || lowered.contains("unauthorized")
        || lowered.contains("cannot ")
        || lowered.contains("permission");
    denied.then_some(
        "This identity can read the object, but not this part of it. Check the role binding for \
         the verb, then retry.",
    )
}

/// The second control a permission failure offers.
///
/// `UI-SPEC.md` §4.15 wants a permission error to say what the identity can do, what it cannot,
/// and why, and §4.13's own table pairs that state with an `Open RBAC` beside the Retry. The
/// panel cannot open the cluster's RBAC objects — it has no way to change the table's resource —
/// so what it can do is put the *reason* on the clipboard, which is the thing a reader pastes
/// into the ticket. A button that says `OK` teaches nothing; a button that hands over the
/// sentence does.
fn permission_copy_action(reason: &str) -> Option<StateHandler> {
    permission_hint(reason)?;
    let reason = reason.to_owned();
    Some(Rc::new(move |cx: &mut App| {
        cx.write_to_clipboard(ClipboardItem::new_string(reason.clone()));
    }))
}

/// One collapsible block of the Describe body.
///
/// `UI-REDESIGN.md` §3.4 replaces the full-width rule under every section heading with 8px of
/// whitespace and a 48px dash after the title, and `UI-SPEC.md` §4.5 fixes the heading itself:
/// `CAPTION` 11/600 uppercase in `fg.tertiary`, a 12px chevron that turns a quarter turn, and a
/// right-aligned count. The count is not decoration — it is the whole reason a collapsed
/// section is worth collapsing, because it is what tells the reader what they would get.
/// The block that opens a section: the sentence, and the mark its heading wears.
///
/// One value for the two because they are the same fact drawn twice — once as a word a reader acts
/// on and once as a point they can find without reading. Passing them separately is how a heading
/// ends up wearing a dot that disagrees with the sentence under it.
#[derive(Default)]
struct StatusLead {
    headline: Option<Vec<AnyElement>>,
    mark: Option<Severity>,
}

struct SectionSpec {
    title: &'static str,
    /// How many rows the section holds, shown right-aligned on the heading.
    count: usize,
    /// Whether the section may be collapsed at all. Status may not: it is the one block the
    /// reader opened the panel for, and a section you can close is a section you can lose.
    collapsible: bool,
    open: bool,
    /// The severity a section that cannot be closed wears in its heading's leading lane.
    ///
    /// It exists so the lane is *reserved* on a heading that has no chevron. The lane was drawn
    /// only for a collapsible section, which put the `STATUS` caption 20px left of every caption
    /// below it and every dash after it 20px out of line — a heading column that is not a column.
    /// Status wears its mark in the same lane a chevron would take, so the one section that is
    /// never collapsed is also the one a reader can tell apart from the ones that are.
    mark: Option<Severity>,
    /// The tab stop the disclosure takes, so a keyboard reaches every section in document order.
    tab_index: isize,
}

/// The leading lane of a section heading: a chevron for a section that opens, a status mark for
/// the one that does not, and an empty slot for neither.
///
/// Twenty pixels, which is [`design::icon::IN_ROW`] plus `space::XS`, is the lane the
/// Dock's status dot and the centre tab's pin mark reserve for the same reason: "a row that
/// reserves no lane moves its label when its glyph's intrinsic width differs, which is a
/// different defect from a row without a glyph". It is stated here rather than taken from the
/// chevron's width so the caption's spine does not depend on which icon happens to be in the slot.
/// `Pixels` addition is not a `const` operation, so the lane is a function rather
/// than a constant; there is exactly one call site and it is in a render path.
fn section_mark_lane() -> Pixels {
    design::icon::IN_ROW + space::XS
}

/// The mark a section that cannot be collapsed wears in its heading's leading lane.
///
/// A 6px dot in the mark ink, not a glyph in the word ink: the sentence under the heading already
/// spells the state out in `role::status_word_for`, and §1.5 reserves the four status channels
/// for points and small areas. The dot is the small area, and it is what lets a reader scanning
/// the collapsed headings of a Describe body find the one section that has an answer in it.
fn section_mark(severity: Severity, cx: &App) -> AnyElement {
    div()
        .flex_none()
        .w(design::size::STATUS_DOT)
        .h(design::size::STATUS_DOT)
        .rounded_full()
        .bg(severity.marker_on(cx, role::surface_content(cx)))
        .into_any_element()
}

/// The 12px chevron on a section heading, turned a quarter turn when the section is open.
///
/// The turn is a rotation over `design::motion::FAST`, which is the product's state-change
/// duration; a spring here would be the single loudest "this is a web page" signal in the
/// panel, and `PROMPT.md` §2.1 reserves springs for a drag the user is holding.
fn section_chevron(section: &'static str, open: bool, cx: &App) -> AnyElement {
    Icon::new(IconName::ChevronRight)
        .with_size(Size::Size(design::icon::IN_ROW))
        // Incidental, and this is the narrow use the role allows: a disclosure
        // triangle whose meaning the heading beside it already states.
        .text_color(design::icon::incidental(cx))
        .rotate(quarter_turn(open))
        .with_animation(
            // One id per section. A shared id would hand every chevron on screen one animation
            // state, so opening one section would turn every other chevron with it.
            format!("inspector-section-chevron-{section}"),
            Animation::new(design::motion::FAST),
            move |icon: Icon, progress: f32| {
                // A oneshot animation restarts from 0 on the render that flips the state, so
                // the turn is `progress` forwards when opening and `1 - progress` backwards
                // when closing, and it is a real quarter turn rather than a snap at one end.
                let travelled = if open { progress } else { 1.0 - progress };
                icon.rotate(quarter_turn_fraction(travelled))
            },
        )
        .into_any_element()
}

/// The two states a section chevron rests at: closed at 0°, open at 90°.
fn quarter_turn(open: bool) -> Radians {
    quarter_turn_fraction(if open { 1.0 } else { 0.0 })
}

/// A quarter turn at any point along it, so the chevron rotates rather than jumping.
fn quarter_turn_fraction(fraction: f32) -> Radians {
    gpui_kit::radians(fraction.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2)
}

/// The 48px dash a section heading wears instead of a rule.
///
/// A rule under every heading reads as a bug: a table is the only thing in the product with
/// rules, and a mark that means "section" everywhere else is a mark that means nothing. Forty-
/// eight pixels is short enough that it cannot be read as a divider, so the group is carried by
/// whitespace and the dash is only there to say "a title just ended here".
///
/// It is aligned by offset rather than by centring. `items_center` on the band puts a 1px child of
/// a 24px band at `11.5`, which lands between two device rows; [`SECTION_DASH_OFFSET`] says why
/// and what the number is.
fn section_dash(cx: &App) -> AnyElement {
    div()
        .debug_selector(|| "inspector-section-dash".to_owned())
        .flex_none()
        .self_start()
        .mt(SECTION_DASH_OFFSET)
        .w(SECTION_DASH_WIDTH)
        .h(border::LINE)
        .bg(role::border_subtle(cx))
        .into_any_element()
}

/// A section heading: chevron, uppercase caption, dash, and the count on the trailing edge.
///
/// The block is `design::size::ROW_DENSE` tall and the word inside it is `CAPTION`. That is the
/// mockup's `.insp-sec-h` verbatim: a 24px band carrying a chevron, an 11px caption, a dash and a
/// count. The band is the block a reader scans for, and the caption is the word inside it — so
/// the two are separately addressable rather than the title *being* a piece of 11px text.
fn section_head(spec: &SectionSpec, cx: &App) -> AnyElement {
    let label_selector = format!("inspector-section-label-{}", spec.title);
    let head_selector = format!("inspector-section-head-{}", spec.title);
    let count = design::format::count(spec.count);
    h_flex()
        .id(format!("inspector-section-head-{}", spec.title))
        .debug_selector(move || head_selector.clone())
        .w_full()
        .min_w(px(0.))
        .h(design::size::ROW_DENSE)
        .mt(space::SM)
        .gap(space::XS)
        .items_center()
        // The lane is always there, for every section, whether or not it has a mark to put in it.
        // The heading is a row of four things — lane, caption, dash, count — and a row whose
        // first element comes and goes is a row whose other three move.
        .child(
            div()
                .flex_none()
                .w(section_mark_lane())
                .h_full()
                .items_center()
                .when(spec.collapsible, |this| {
                    this.child(section_chevron(spec.title, spec.open, cx))
                })
                .when_some(spec.mark, |this, severity| {
                    this.child(section_mark(severity, cx))
                }),
        )
        .child(
            div()
                .flex_none()
                .debug_selector(move || label_selector.clone())
                .child(section_label(spec.title).text_color(role::fg_tertiary(cx))),
        )
        .child(section_dash(cx))
        .child(div().flex_1().min_w(px(0.)))
        .child(
            div()
                .flex_none()
                .child(label_small(count).text_color(role::fg_tertiary(cx))),
        )
        .into_any_element()
}

/// A section heading's own text: `CAPTION`, semibold, uppercase.
///
/// Uppercase and tracked out is a role, not a decoration: `design::text::CAPTION` is the one
/// token the type scale reserves for section heads, and the only place it is allowed to shout.
fn section_label(text: &'static str) -> Label {
    Label::new(text.to_uppercase())
        .text_size(text::CAPTION)
        .line_height(text::CAPTION_LINE_HEIGHT)
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
}

/// A collapsible section, with the heading doubling as its disclosure control.
///
/// The heading is a button rather than a control beside a heading because a 352px panel cannot
/// spend a chevron *and* a separate hit target on a line that already carries a word, a dash and
/// a count. A section that is closed renders as its heading and nothing else, so a Describe body
/// for a healthy Pod is one screen of section titles with counts, and the reader opens the one
/// they need.
fn collapsible_section(
    spec: &SectionSpec,
    rows: Vec<AnyElement>,
    footer: Option<AnyElement>,
    detail: DetailSection,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    let section_selector = format!("inspector-describe-section-{}", spec.title);
    v_flex()
        .debug_selector(move || section_selector.clone())
        .w_full()
        .min_w(px(0.))
        .gap(space::XXS)
        .child(
            div()
                .id(format!("inspector-section-toggle-{}", spec.title))
                .debug_selector(move || format!("inspector-describe-section-title-{}", spec.title))
                .role(Role::Button)
                .aria_expanded(spec.open)
                .aria_label(if spec.open {
                    format!("Collapse {}", spec.title)
                } else {
                    format!("Expand {}", spec.title)
                })
                .tab_index(spec.tab_index)
                // The heading is a control, so it carries the control states. Only a ghost
                // affordance gets a hover fill: `UI-SPEC.md` §4.6 reserves fills on press for
                // the pushed variants and gives a ghost control its own, which is what this is.
                .hover(|this| this.bg(hover_wash(cx)))
                .active(|this| this.bg(press_wash(cx)))
                .on_click(cx.listener(move |panel, _, _, cx| {
                    if !panel.open_sections.contains(&detail) {
                        panel.open_sections.insert(detail);
                    } else {
                        panel.open_sections.remove(&detail);
                    }
                    cx.notify();
                }))
                .child(section_head(spec, cx)),
        )
        .when(spec.open, |this| {
            this.children(rows)
                .when_some(footer, |this, footer| this.child(footer))
        })
        .into_any_element()
}

/// A section that cannot be collapsed. Status uses this: it is always open.
fn open_section(
    spec: &SectionSpec,
    rows: Vec<AnyElement>,
    footer: Option<AnyElement>,
    cx: &App,
) -> AnyElement {
    let section_selector = format!("inspector-describe-section-{}", spec.title);
    let title_selector = format!("inspector-describe-section-title-{}", spec.title);
    v_flex()
        .debug_selector(move || section_selector.clone())
        .w_full()
        .min_w(px(0.))
        .gap(space::XXS)
        .child(
            div()
                .id(format!("inspector-section-head-{}", spec.title))
                .debug_selector(move || title_selector.clone())
                .w_full()
                .child(section_head(spec, cx)),
        )
        .children(rows)
        .when_some(footer, |this, footer| this.child(footer))
        .into_any_element()
}

fn field_row(
    label: &str,
    value: &str,
    style: ValueStyle,
    stacked: bool,
    values: &ValueRows,
    cx: &App,
) -> AnyElement {
    field_row_with_icon(
        label,
        value,
        style,
        stacked,
        None,
        format!("inspector-describe-field-{label}"),
        values,
        cx,
    )
}

/// A field row that carries a severity marker, so the marked and unmarked rows share one layout.
#[expect(clippy::too_many_arguments)]
fn status_field_row(
    label: &str,
    value: &str,
    style: ValueStyle,
    stacked: bool,
    severity: Severity,
    selector: String,
    values: &ValueRows,
    cx: &App,
) -> AnyElement {
    field_row_with_icon(
        label,
        value,
        style,
        stacked,
        Some(severity),
        selector,
        values,
        cx,
    )
}

/// One field row, with the keyboard affordances that make a long value reachable.
///
/// A row is a tab stop: Enter expands it to its full wrapped value, Ctrl or Cmd+C copies the
/// whole value, and every other key falls through to the scroll container. Truncation is a
/// display decision, so the text is always one keystroke or one shortcut away.
#[expect(clippy::too_many_arguments)]
fn field_row_with_icon(
    label: &str,
    value: &str,
    style: ValueStyle,
    stacked: bool,
    severity: Option<Severity>,
    selector: String,
    values: &ValueRows,
    cx: &App,
) -> AnyElement {
    let key_selector = format!("{selector}-key");
    let value_selector = format!("{selector}-value");
    let expanded = values.is_expanded(&selector);
    let focus = values.take_focus(&selector, value);
    let managed = values.managed(label);
    // A value the cluster sent is data, and the reader can set the data font size. The key role
    // beside it is not scaled by that setting, so both sizes have to come from their own source
    // or a raised data font leaves two sizes inside one row. The row is as tall as the configured
    // data line, for the reason the setting's own help text gives: a taller glyph in a shorter
    // box is cropped.
    let data_typography = style.data.then(|| crate::settings::data_typography(cx));
    let value_size = data_typography
        .as_ref()
        .map_or(text::MONO_SM, |data| data.size);
    let value_line_height = data_typography
        .as_ref()
        .map_or(text::MONO_SM_LINE_HEIGHT, |data| data.line_height);
    let row_height = data_typography.as_ref().map_or_else(
        || design::size::ROW,
        |data| design::row_height(data.line_height),
    );
    // A field with nothing in it is one word, in the quietest ink, in the UI face — never the
    // source format's braces, and never the mono face, which is what made `resources {}` read as
    // a fragment of JSON rather than as a Pod with no limits. `role::fg_disabled` is the role the
    // scale gives content that is permanently unavailable, which is what an unset field is; see
    // `managed_ink` for why this panel uses that role for nothing else. The row keeps its height,
    // its key and its position: a field that exists and is unset is information.
    let empty = value == EMPTY_VALUE;
    // The value's ink. A managed field is the one case where the value is deliberately quieter
    // than its neighbours: it is not wrong, it is not the reader's to change, and `fg_disabled`
    // is the role that says so without shouting about it.
    let value_ink = if empty {
        role::fg_disabled(cx)
    } else if managed {
        managed_ink(cx)
    } else if style.mono {
        role::fg_primary(cx)
    } else {
        role::fg_secondary(cx)
    };
    let key_ink = if managed {
        managed_ink(cx)
    } else {
        role::fg_tertiary(cx)
    };
    // Nothing to copy out of a field that holds nothing, and the chord would put the word `None`
    // on the clipboard.
    let copyable = !empty && copyable_value(label, value);
    // The slot is always present, so a marked row and an unmarked row share one key
    // column and one value start.
    let marker = h_flex()
        .w(DESCRIBE_MARKER_SLOT)
        .flex_none()
        .h(row_height)
        .items_center()
        .when_some(severity, |this, severity| {
            this.child(describe_severity_icon(severity, cx))
        })
        // A managed field wears a lock in the same slot, because a field the controller writes
        // back is information, not an error: the reader has to know it is not theirs to change,
        // and a lock is the one glyph that says so without spending a status colour.
        .when(managed && severity.is_none(), |this| {
            this.child(
                Icon::new(IconName::Lock)
                    .with_size(Size::Size(design::icon::IN_ROW))
                    .text_color(managed_ink(cx)),
            )
        })
        // An expanded row says it can be collapsed, in the slot that never moves anything.
        .when(expanded && severity.is_none() && !managed, |this| {
            this.child(
                Icon::new(IconName::ChevronUp)
                    .with_size(Size::Size(design::icon::IN_ROW))
                    .text_color(design::icon::incidental(cx)),
            )
        });
    // The key column is `UI-REDESIGN.md` §3.4's `132px`, fixed, so every value in the section
    // starts at the same x. That is the entire reason the grid exists: before it, the values
    // floated in the middle of whatever was left over and a long one wrapped out of alignment
    // with every other value in the list.
    let key = h_flex()
        .id(key_selector.clone())
        .h(row_height)
        .min_w(px(0.))
        .flex_none()
        .w(px(DESCRIBE_KEY_COLUMN))
        .gap(space::XXS)
        .items_center()
        .child(
            div()
                .id(format!("{key_selector}-text"))
                .debug_selector({
                    let selector = format!("{key_selector}-text");
                    move || selector.clone()
                })
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(text::LABEL)
                .line_height(text::LABEL_LINE_HEIGHT)
                .text_color(key_ink)
                .child(SharedString::from(label.to_owned())),
        )
        .when(managed, |this| {
            this.child(
                div()
                    .flex_none()
                    .child(label_small("managed").text_color(managed_ink(cx))),
            )
        })
        .debug_selector({
            let key_selector = key_selector.clone();
            move || key_selector.clone()
        });
    let value_text = SharedString::from(value.to_owned());
    let mut inline_value = h_flex()
        .id(value_selector.clone())
        .h(row_height)
        .flex_1()
        .min_w(px(0.))
        .gap(space::XS)
        .items_center()
        .child(
            // The *text* starts inside the value, after the row's own padding, so the value
            // element's bounds are not where a reader's eye starts. The text carries its own
            // selector so "the values line up" is a measurement rather than a hope — and so the
            // two layouts can be compared against each other at all.
            div()
                .id(format!("{value_selector}-text"))
                .debug_selector({
                    let selector = format!("{value_selector}-text");
                    move || selector.clone()
                })
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(f32::from(value_size)))
                .line_height(px(f32::from(value_line_height)))
                .text_color(value_ink)
                .child(value_text.clone()),
        )
        .debug_selector({
            let value_selector = value_selector.clone();
            move || value_selector.clone()
        });
    // The full text stays one hover away on the value itself, whether or not the row is a
    // tab stop, so a clipped value is never only reachable from the keyboard.
    inline_value = inline_value.tooltip(common::hover_hint(value.to_owned()));

    if let Some(copy) = copy_value_button(&selector, label, value, values, copyable, cx) {
        inline_value = inline_value.child(copy);
    }
    if style.mono && !empty {
        inline_value = inline_value.font(buffer_font(cx));
    }
    // A long value shares one line with its key badly, so it moves under the key and
    // wraps inside the readable measure. Nothing is dropped, and the tooltip still holds
    // the full text when a wrapped value has to clip.
    let mut wrapped_value = v_flex()
        .id(format!("{value_selector}-wrapped"))
        .w_full()
        .min_w(px(0.))
        .min_h(row_height)
        .pl(DESCRIBE_MARKER_SLOT + px(DESCRIBE_KEY_COLUMN) + space::XS * 2.)
        .gap(space::XXS)
        .overflow_hidden()
        .whitespace_normal()
        .tooltip(common::hover_hint(value.to_owned()))
        .text_size(px(f32::from(value_size)))
        .line_height(px(f32::from(value_line_height)))
        .text_color(value_ink)
        .debug_selector({
            // The same name the one-line layout uses, because only one of the two is ever in
            // the tree: a caller asking for this row's value gets whichever layout drew it.
            let value_selector = value_selector.clone();
            move || value_selector.clone()
        })
        .child(
            div()
                .id(format!("{value_selector}-wrapped-text"))
                .debug_selector({
                    let selector = format!("{value_selector}-wrapped-text");
                    move || selector.clone()
                })
                .child(value_text),
        );
    if let Some(copy) = copy_value_button(&selector, label, value, values, copyable, cx) {
        wrapped_value = wrapped_value.child(copy);
    }
    if style.mono && !empty {
        wrapped_value = wrapped_value.font(buffer_font(cx));
    }

    let row_selector = selector.clone();
    // An expanded row always takes the wrapped layout, because the point of expanding is to see
    // the whole value.
    let wrap = stacked || expanded || value.chars().count() > DESCRIBE_INLINE_VALUE_LIMIT;
    let layout: Div = if wrap {
        let key_line = h_flex()
            .id(format!("{key_selector}-line"))
            .w_full()
            .min_w(px(0.))
            .h(row_height)
            .gap(space::XS)
            .items_center()
            .child(marker)
            .child(div().flex_1().min_w(px(0.)).child(key));
        v_flex()
            .debug_selector(move || row_selector.clone())
            .w_full()
            .min_w(px(0.))
            .children([key_line, wrapped_value])
    } else {
        h_flex()
            .debug_selector(move || row_selector.clone())
            .w_full()
            .min_w(px(0.))
            .h(row_height)
            .gap(space::XS)
            .items_center()
            .child(marker)
            .child(key)
            .child(inline_value)
    };
    let mut row = layout
        .id((SharedString::from(selector.clone()), 0))
        // The row is the positioning context for the hover keycaps, so they can sit on its
        // trailing edge without taking a single pixel from the value column.
        .relative()
        .role(Role::Group)
        .aria_label(format!("{label}: {value}"))
        .aria_expanded(expanded)
        .aria_keyshortcuts("Enter Control+c Meta+c")
        .when(expanded, |this| this.bg(hover_wash(cx)));
    if let Some(focus) = focus {
        let panel = values.panel.clone();
        let row_selector = selector.clone();
        let copied = values.copied(&selector);
        // The two chords this row answers to, revealed while the row is hovered.
        //
        // A Describe view holds hundreds of rows, so painting the chords on every row would
        // turn the field list into a wall of keycaps and bury the values, which are the reason
        // the panel exists. Revealing them on approach teaches the shortcut to whoever is using
        // a pointer, and leaves the resting surface quiet for everyone else; keyboard users
        // meet the same chords in the Settings shortcuts list and the row's aria. The
        // chords themselves come from the keymap, so a rebinding shows up here for free.
        let group: SharedString = format!("{selector}-row").into();
        // Out of flow, and this is load-bearing rather than cosmetic. The chips used to be an
        // ordinary flex child at `opacity(0)`, which does not remove it from the layout: two
        // keycaps took 132px out of every value column, so a Describe value was left 20px wide
        // in a 336px panel — a third of the column given to two glyphs nobody could see. They sit
        // on the row's trailing edge instead, which is also the arrangement `UI-SPEC.md` §4.3
        // already uses for a hover-only close button: absolute, so revealing it moves nothing.
        let chords = h_flex()
            .absolute()
            .right_0()
            .top_0()
            .bottom_0()
            .flex_none()
            .gap(space::XS)
            .items_center()
            .opacity(0.)
            .group_hover(group.clone(), |style| style.opacity(1.))
            .debug_selector({
                let selector = format!("{selector}-chords");
                move || selector.clone()
            })
            .when_some(
                chord_keycap("k8s_inspector::ToggleValueExpansion", cx),
                |this, keycap| this.child(keycap),
            )
            .when_some(
                chord_keycap("k8s_inspector::CopyValue", cx),
                |this, keycap| this.child(keycap),
            );
        row = row
            .group(group)
            .track_focus(&focus)
            .tab_stop(true)
            .focus_visible(move |style| style.bg(role::accent_wash(cx)))
            .child(chords)
            .when(copied, |this| this.bg(hover_wash(cx)))
            .on_key_down(
                move |event: &KeyDownEvent, _window: &mut Window, cx: &mut App| {
                    let key = event.keystroke.key.as_str();
                    let modifiers = event.keystroke.modifiers;
                    let Some(panel) = panel.upgrade() else {
                        return;
                    };
                    if matches!(key, "enter" | "return" | "space") {
                        panel.update(cx, |panel, cx| {
                            panel.toggle_value_expansion(&row_selector, cx)
                        });
                    } else if key == "c" && (modifiers.control || modifiers.platform) {
                        panel.update(cx, |panel, cx| panel.copy_value(&row_selector, cx));
                    } else {
                        return;
                    }
                    cx.stop_propagation();
                },
            );
    }
    row.into_any_element()
}

/// Whether a value is one a reader copies rather than reads: a UID, an IP, a port.
///
/// `UI-REDESIGN.md` §3.4 asks for a copy button on exactly these three, and the reason is that
/// they are the values an SRE pastes into a `kubectl` command or a bug report. Everything else on
/// the row is prose or a count, and a copy button on a sentence is a control that does nothing
/// useful. The test is on the value's *shape* where the value has one, so a prose field that
/// happens to be called "Node" does not grow a button.
fn copyable_value(label: &str, value: &str) -> bool {
    let label = label.to_ascii_lowercase();
    let wanted = ["uid", "ip", "port"];
    if !wanted.iter().any(|token| label.contains(token)) {
        return false;
    }
    // A field path counts: `spec.containers[0].ports[0].containerPort` is a port, and
    // `status.podIP` is an IP. A field that merely mentions the word without carrying a
    // machine-shaped value does not get a control.
    !value.is_empty() && !value.contains(' ')
}

/// The copy button a UID, IP or port row wears, revealed on approach.
///
/// It is a button rather than a chord because copying a UID is a pointer-shaped task - nobody
/// memorises a chord per row - and it is *revealed* rather than always drawn because a Describe
/// body holds hundreds of rows and a permanent button on each would be a wall of glyphs. The
/// keyboard path is the existing `CopyValue` chord on the row, which stays.
fn copy_value_button(
    selector: &str,
    label: &str,
    value: &str,
    values: &ValueRows,
    copyable: bool,
    cx: &App,
) -> Option<AnyElement> {
    if !copyable {
        return None;
    }
    let panel = values.panel.clone();
    // The row registers this group, so the button reveals exactly where a pointer
    // can see the row it belongs to.
    let group: SharedString = format!("{selector}-row").into();
    let text = SharedString::from(value.to_owned());
    let id = format!("inspector-copy-{label}");
    let button_id = id.clone();
    let hover_id = id.clone();
    Some(
        div()
            .id(hover_id)
            .debug_selector(move || button_id.clone())
            .flex_none()
            .opacity(0.)
            .group_hover(group, |style| style.opacity(1.))
            .child(
                Button::new(id)
                    .icon(IconName::Copy)
                    .ghost()
                    // The glyph box is what gpui-kit derives the ICON from; the
                    // target is restated so the pointer still aims at a full
                    // `size::ICON_BUTTON`. Shrinking both together takes the hit
                    // area down with the glyph, which is how three of these left
                    // the toolbar's own vertical centre.
                    .with_size(Size::Size(icon_control_box()))
                    .w(design::size::ICON_BUTTON)
                    .h(design::size::ICON_BUTTON)
                    .text_color(design::icon::resting(cx))
                    // Out of the tab order on purpose, like the log row's copy
                    // button: the row's own chord is the keyboard path, and a
                    // stop here would put focus on a button that is invisible
                    // until the pointer arrives.
                    .tab_index(-1isize)
                    .tab_stop(false)
                    .accessibility_label(format!("Copy {label}"))
                    .on_click(move |_, _, cx| {
                        if let Some(panel) = panel.upgrade() {
                            panel.update(cx, |panel, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
                                panel.value_copied_at = Some(Instant::now());
                                cx.notify();
                            });
                        }
                    }),
            )
            .into_any_element(),
    )
}

/// A ConfigMap or Secret this object names and the cluster cannot resolve.
///
/// A k8s object rather than a log line: the block's rows are relationships between objects, and an
/// image reference is not one — it has no uid, no namespace, and nothing to open.
#[derive(Debug, PartialEq, Eq)]
struct MissingReference {
    kind: String,
    name: String,
}

/// The kinds a Related row can actually be followed to, and the resource that names them.
///
/// `UI-REDESIGN.md` L3 makes the Related block the way a dead end becomes a chain, and a chain
/// that stops at the first hop is a list. Following a name needs three things: the kind has to
/// name a group and a plural, the object needs a uid so the read cannot land on a different
/// object, and the namespace has to be right — including *no* namespace for the two kinds that
/// are cluster scoped, which is the case a hardcoded table gets wrong most easily.
///
/// The list is the set of kinds that own something or are owned. It is a table rather than a
/// discovery request because a relationship has to be followable from the panel that drew it, and
/// a discovery round trip on hover is a request per row.
const RELATIONSHIP_RESOURCES: [(&str, &str, &str, &str); 15] = [
    ("Pod", "", "v1", "pods"),
    ("Service", "", "v1", "services"),
    ("ConfigMap", "", "v1", "configmaps"),
    ("Secret", "", "v1", "secrets"),
    ("ServiceAccount", "", "v1", "serviceaccounts"),
    ("PersistentVolumeClaim", "", "v1", "persistentvolumeclaims"),
    ("Node", "", "v1", "nodes"),
    ("Deployment", "apps", "v1", "deployments"),
    ("StatefulSet", "apps", "v1", "statefulsets"),
    ("ReplicaSet", "apps", "v1", "replicasets"),
    ("DaemonSet", "apps", "v1", "daemonsets"),
    ("Job", "batch", "v1", "jobs"),
    ("CronJob", "batch", "v1", "cronjobs"),
    (
        "HorizontalPodAutoscaler",
        "autoscaling",
        "v2",
        "horizontalpodautoscalers",
    ),
    ("Ingress", "networking.k8s.io", "v1", "ingresses"),
];

/// The resource that names `kind`, or `None` for a kind nothing here can follow.
fn relationship_resource(kind: &str) -> Option<kube_core::ApiResource> {
    RELATIONSHIP_RESOURCES
        .iter()
        .find(|(name, ..)| *name == kind)
        .map(|(name, group, version, plural)| kube_core::ApiResource {
            group: (*group).to_owned(),
            version: (*version).to_owned(),
            api_version: if group.is_empty() {
                (*version).to_owned()
            } else {
                format!("{group}/{version}")
            },
            kind: (*name).to_owned(),
            plural: (*plural).to_owned(),
        })
}

/// The object a relationship row points at, when the panel can name it.
///
/// `None` for a kind outside the table, and `None` for an object with no uid — an owner
/// reference that names an object the API can no longer resolve is exactly the row that matters
/// most, and it is the one row there is nothing to follow.
fn followable(kind: &str, name: &str, namespace: Option<&str>, uid: &str) -> Option<ObjectRef> {
    if uid.is_empty() {
        return None;
    }
    relationship_ref(kind, name, namespace, uid)
}

/// The object a relationship row points at when only a name is known.
///
/// This is deliberately narrower than [`followable`], and the difference is the whole safety
/// argument: a name can be freed and taken by a different object, so a read answered for the new
/// occupant is worse than no read. A `Node` and a `Namespace` are the two kinds where that cannot
/// happen — both are named by the machine or the operator that owns them, both are registered once
/// and removed rather than replaced, and a cluster will not admit a second object under a Node name
/// while the first exists. So those two may be followed by name, and every other kind may not.
fn followable_by_name(kind: &str, name: &str, namespace: Option<&str>) -> Option<ObjectRef> {
    if !matches!(kind, "Node" | "Namespace") {
        return None;
    }
    relationship_ref(kind, name, namespace, "")
}

fn relationship_ref(
    kind: &str,
    name: &str,
    namespace: Option<&str>,
    uid: &str,
) -> Option<ObjectRef> {
    let resource = relationship_resource(kind)?;
    let namespaced = !matches!(kind, "Node" | "Namespace");
    Some(ObjectRef {
        resource,
        namespace: namespaced.then(|| namespace.unwrap_or_default().to_owned()),
        name: name.to_owned(),
        uid: uid.to_owned(),
    })
}

/// The deep link for an object, in the spelling `UI-SPEC.md` §4.19 fixes.
///
/// The kind goes in as the alias a person would type, not as the kind: the design's own example
/// is `k8s-gpui://<cluster>/deploy/<ns>/<name>`, and a link that reads `ReplicaSet` where the
/// specification says `rs` is a link that only this build opens.
fn deep_link(cluster: &str, target: &ObjectRef) -> String {
    let kind = kind_link_alias(target.resource.kind.as_str());
    let namespace = target.namespace.as_deref().unwrap_or("-");
    format!("k8s-gpui://{cluster}/{kind}/{namespace}/{}", target.name)
}

/// The short spelling of a kind in a link.
///
/// `UI-SPEC.md` §4.19 lists the aliases, and only those: a kind with no listed alias goes in as
/// itself, which is still a link this app can open.
fn kind_link_alias(kind: &str) -> &str {
    match kind {
        "Deployment" => "deploy",
        "ReplicaSet" => "rs",
        "Pod" => "po",
        "Service" => "svc",
        "ConfigMap" => "cm",
        "Namespace" => "ns",
        "Ingress" => "ing",
        other => other,
    }
}

/// How long the panel waits before asking whether the object it is showing still exists.
///
/// The same cadence as [`EVENTS_TTL`], and for the same reason: it is the interval at which this
/// panel already accepts that the cluster has moved on. Asking more often than the events it sits
/// beside would be a `GET` per second for a banner; asking less often leaves the panel lying for
/// longer than the events next to it do.
const PRESENCE_TTL: Duration = Duration::from_secs(15);

/// A ConfigMap or Secret this object names, and whether the cluster can resolve each one.
///
/// Keyed by the selection's [`cache_key`] so a reader who follows three objects in a row pays
/// for three lookups rather than nine, and cleared with the rest of the session state. A reference
/// that has not been asked about yet is absent rather than `Unknown`, and the two are treated
/// identically: neither has taught the panel anything.
type ReferenceVerdicts = HashMap<(String, String), ResolveOutcome>;

/// One ConfigMap or Secret this object names, and what the panel draws for it.
///
/// `follow` is `Some` only where there is a uid to follow, which is the whole difference between
/// a live reference and a fact: a `ConfigMap` the cluster confirmed is somewhere to go, and one it
/// denied is not.
#[derive(Debug, PartialEq, Eq)]
struct ReferenceRow {
    kind: String,
    name: String,
    health: RelatedHealth,
    follow: Option<ObjectRef>,
}

/// One row of the Related block for every ConfigMap and Secret this object names.
///
/// The candidate set and the event evidence are [`missing_references`]' job and are unchanged: the
/// names come from the object's own `spec`, and a Warning that mentions one is what says it is
/// missing. This adds the third source of truth on top, and the reason it can be trusted over the
/// event text is that it is a `GET` rather than a sentence — a controller that words the same
/// failure `FailedMount` cannot change the answer, and an event that says "not found" for a
/// ConfigMap that has since been recreated cannot either.
///
/// The three answers have to look three different ways, and the third is the one that is easy to
/// get backwards:
///
/// - `Missing` is the only answer that may paint `danger`. It is also the only one that may not
///   draw an arrow, because there is nothing on the other end of it.
/// - `Found { uid }` is a live reference: the primary ink, a follow action, the arrow.
/// - `Unknown` — which is also what a source that does not implement `resolve` returns, and what
///   an offline session gets for every reference at once — changes nothing. It falls back to the
///   event evidence, which is what the panel drew before `resolve` existed. A two-valued answer
///   would have to pick between "absent" and "not asked", and picking "absent" paints every
///   reference in the panel red the moment the connection drops.
fn reference_rows(
    object: &DynamicObject,
    events_state: Option<&LoadState<Arc<Vec<DynamicObject>>>>,
    resolved: &ReferenceVerdicts,
    namespace: Option<&str>,
) -> Vec<ReferenceRow> {
    // Read once rather than per candidate: a Pod with twenty references would otherwise re-read
    // the whole event list twenty times on every render.
    let warned: BTreeSet<(String, String)> = missing_references(object, events_state)
        .into_iter()
        .map(|missing| (missing.kind, missing.name))
        .collect();
    referenced_objects(object)
        .into_iter()
        .filter_map(|(kind, name)| {
            let key = (kind.clone(), name.clone());
            match resolved.get(&key) {
                Some(ResolveOutcome::Found { uid }) => Some(ReferenceRow {
                    health: RelatedHealth::Ok,
                    follow: followable(&kind, &name, namespace, uid),
                    kind,
                    name,
                }),
                Some(ResolveOutcome::Missing) => Some(ReferenceRow {
                    health: RelatedHealth::NotFound,
                    follow: None,
                    kind,
                    name,
                }),
                // `Unknown`, and a reference nobody has asked about yet. Both fall back to the
                // Warning events, which is the appearance the panel had before `resolve` existed.
                _ => warned.contains(&key).then_some(ReferenceRow {
                    health: RelatedHealth::NotFound,
                    follow: None,
                    kind,
                    name,
                }),
            }
        })
        .collect()
}

/// The ConfigMaps and Secrets this object names, and whether the cluster says they are missing.
///
/// Two changes of shape, both about where the claim comes from.
///
/// **The candidate set is read off the spec, not out of a sentence.** `spec.volumes`,
/// `spec.volumes[].projected.sources`, `envFrom` and `env[].valueFrom` are where a Pod says
/// which ConfigMaps and Secrets it cannot start without, and they are structured, so reading them
/// needs no vocabulary. The old version learned the names from the event text instead, which meant
/// the panel's whole notion of "which objects does this Pod reference" was a phrase matcher: a
/// controller that words the same failure `FailedMount` rather than `configmap "x" not found` made
/// the row vanish, and nothing said so.
///
/// **Absence is then confirmed by the name, not by the wording.** A Warning that mentions the
/// candidate is what says it is missing, so any phrasing works. An `optional: true` reference is
/// skipped outright: a missing optional ConfigMap is a configuration choice, and a panel that
/// paints it `danger` teaches the reader to stop believing the row.
///
/// This is the fallback for every reference the cluster cannot answer about — see
/// [`reference_rows`], which prefers a `resolve` answer and lands here otherwise.
fn missing_references(
    object: &DynamicObject,
    events_state: Option<&LoadState<Arc<Vec<DynamicObject>>>>,
) -> Vec<MissingReference> {
    let Some(LoadState::Ready(events)) = events_state else {
        return Vec::new();
    };
    let warnings: Vec<String> = events
        .iter()
        .filter(|event| event_type(event) == "Warning")
        .map(event_message)
        .collect();
    referenced_objects(object)
        .into_iter()
        .filter(|(_, name)| {
            warnings
                .iter()
                .any(|message| names_reference(message, name))
        })
        .map(|(kind, name)| MissingReference { kind, name })
        .collect()
}

/// The named ConfigMaps and Secrets an object cannot run without, deduplicated and in a stable
/// order so the block does not reshuffle between renders.
fn referenced_objects(object: &DynamicObject) -> Vec<(String, String)> {
    let mut found: BTreeSet<(String, String)> = BTreeSet::new();
    let mut take = |kind: &str, name: Option<&str>, optional: bool| {
        if optional {
            return;
        }
        if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
            found.insert((kind.to_owned(), name.to_owned()));
        }
    };
    let spec = &object.data;
    for volume in spec
        .pointer("/spec/volumes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let optional = volume
            .get("configMap")
            .and_then(|config| config.get("optional"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        take(
            "ConfigMap",
            volume.pointer("/configMap/name").and_then(Value::as_str),
            optional,
        );
        take(
            "Secret",
            volume.pointer("/secret/secretName").and_then(Value::as_str),
            volume
                .pointer("/secret/optional")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        );
        for source in volume
            .pointer("/projected/sources")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let optional = source
                .get("configMap")
                .and_then(|config| config.get("optional"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            take(
                "ConfigMap",
                source.pointer("/configMap/name").and_then(Value::as_str),
                optional,
            );
            take(
                "Secret",
                source.pointer("/secret/name").and_then(Value::as_str),
                source
                    .pointer("/secret/optional")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            );
        }
    }
    for container in containers(spec) {
        for source in container
            .get("envFrom")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let optional = source
                .get("configMapRef")
                .and_then(|reference| reference.get("optional"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            take(
                "ConfigMap",
                source.pointer("/configMapRef/name").and_then(Value::as_str),
                optional,
            );
            take(
                "Secret",
                source.pointer("/secretRef/name").and_then(Value::as_str),
                source
                    .pointer("/secretRef/optional")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            );
        }
        for entry in container
            .get("env")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            take(
                "ConfigMap",
                entry
                    .pointer("/valueFrom/configMapKeyRef/name")
                    .and_then(Value::as_str),
                entry
                    .pointer("/valueFrom/configMapKeyRef/optional")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            );
            take(
                "Secret",
                entry
                    .pointer("/valueFrom/secretKeyRef/name")
                    .and_then(Value::as_str),
                entry
                    .pointer("/valueFrom/secretKeyRef/optional")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            );
        }
    }
    found.into_iter().collect()
}

/// Every container list a Pod spec carries, including the ones a reader does not usually think of.
fn containers(spec: &Value) -> impl Iterator<Item = &Value> {
    ["containers", "initContainers", "ephemeralContainers"]
        .into_iter()
        .filter_map(|key| spec.get(key))
        .filter_map(Value::as_array)
        .flatten()
        .filter(|container| container.is_object())
}

/// Whether a message names `name` as a whole token.
///
/// Whole token, because a ConfigMap called `api` must not match a message about `api-gateway`.
/// The boundaries are anything that cannot be inside a Kubernetes name, which is why the test is
/// on characters rather than on a regular expression over a name grammar.
fn names_reference(message: &str, name: &str) -> bool {
    let bytes = message.as_bytes();
    let mut from = 0;
    while let Some(found) = message[from..].find(name) {
        let start = from + found;
        let end = start + name.len();
        let before = bytes[..start]
            .last()
            .is_none_or(|byte| !is_name_byte(*byte));
        let after = bytes[end..].first().is_none_or(|byte| !is_name_byte(*byte));
        if before && after {
            return true;
        }
        from = start + 1;
    }
    false
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

/// What a Related row says about the object it points at.
///
/// `UI-REDESIGN.md` L3 gives the block one rule that matters: a `NotFound` relationship is
/// usually the root cause, and it is the whole reason a person came to the Inspector. So the
/// failure wears `danger` and the healthy rows wear nothing at all — the same reversal the
/// status column follows, for the same reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RelatedHealth {
    Ok,
    Warning,
    NotFound,
}

impl RelatedHealth {
    /// The ink of the *word* on the row's trailing edge.
    ///
    /// The word ink, not the mark ink: `Warning` and `NotFound` are spelled out in text here, and
    /// the mark role is solved for a 6px dot. `Ok` has no word at all — the row wears nothing,
    /// which is the reversal — so it answers with the content ink and the caller prints nothing.
    fn word_ink(self, cx: &App) -> Hsla {
        match self {
            Self::Ok => role::fg_primary(cx),
            Self::Warning => role::warning_word(cx),
            Self::NotFound => role::danger_word(cx),
        }
    }
}

/// What a Related row does when it is taken.
///
/// `UI-REDESIGN.md` L3's block is the way a dead end becomes a chain, and there are two kinds of
/// way out: another object, and another view of this one. Splitting them into one option would
/// force the second to be spelled as the first — a fake `ObjectRef` pointing at a tab — and the
/// arrow would then be a promise the row cannot keep. The third case, a fact with no way out, is
/// the one that earns the absence of the arrow: a `ConfigMap` the API cannot resolve is the root
/// cause of a Pod, and there is nothing to open.
#[derive(Clone)]
enum RelatedAction {
    Follow(ObjectRef),
    /// Another tab of this panel, which is where the same object's own timeline lives.
    Tab(InspectorTab),
    /// A fact. Not a control, and the row says so by having no arrow.
    Fact,
}

impl RelatedAction {
    fn is_control(&self) -> bool {
        !matches!(self, Self::Fact)
    }
}

impl InspectorPanel {
    /// One row of the Related block.
    ///
    /// The relationship name is `LABEL` in the tertiary ink in a fixed 96px, the object's name takes
    /// what is left at `SUBTITLE`, and the trailing edge carries the status and the count. The fixed
    /// column is the whole reason the block reads as a table of relationships rather than as a list
    /// of sentences.
    ///
    /// `action` is what a click on this row does, and the row is a control exactly when it is
    /// something: hover, a focus stop, `Enter`, and the arrow that promises a way out. A row with
    /// no action is text, and it does not pretend to be hoverable — the arrow *is* the promise,
    /// so a row that cannot act has none.
    fn related_row(
        &self,
        kind: &str,
        name: &str,
        health: RelatedHealth,
        action: RelatedAction,
        cx: &Context<Self>,
    ) -> AnyElement {
        self.related_row_with_count(kind, name, 0, health, action, cx)
    }

    /// [`Self::related_row`] with a count and a status word on the trailing edge.
    fn related_row_with_count(
        &self,
        kind: &str,
        name: &str,
        count: usize,
        health: RelatedHealth,
        action: RelatedAction,
        cx: &Context<Self>,
    ) -> AnyElement {
        // Two inks, because the row carries two different kinds of thing. The *name* is content —
        // the object the row points at — and it is the part a reader acts on, so it stays in the
        // content ink whether the reference resolved or not. The *status* is a word, and it is the
        // only thing on the row that wears the channel. The row used to paint the name in the
        // channel's mark ink as well, which meant a missing ConfigMap was a red sentence and a
        // healthy Node was a near-white one: two different type colours for the same kind of text,
        // decided by something the name itself has nothing to do with.
        let word_ink = health.word_ink(cx);
        let name_ink = role::fg_primary(cx);
        let status = match health {
            RelatedHealth::Ok => "",
            RelatedHealth::Warning => "Warning",
            RelatedHealth::NotFound => "NotFound",
        };
        let panel = cx.weak_entity();
        let label = if status.is_empty() {
            format!("{kind} {name}")
        } else {
            format!("{kind} {name}, {status}")
        };
        // The focus handle is a control, so a row that cannot act does not take one: a Tab that
        // lands on a row and then does nothing is a keyboard dead end.
        let control = action.is_control();
        let focus = control.then(|| cx.focus_handle());
        h_flex()
            .id(format!("inspector-related-{kind}-{name}"))
            .debug_selector(move || format!("inspector-related-{kind}-{name}"))
            .w_full()
            .min_w(px(0.))
            .h(design::size::ROW)
            .gap(space::SM)
            .items_center()
            .role(Role::ListItem)
            .aria_label(label.clone())
            .when_some(focus.clone(), |this, focus| {
                this.track_focus(&focus)
                    .tab_index(RELATED_ROW_TAB_INDEX)
                    // §9.3: a click does not produce a focus ring. The reader's pointer is not a
                    // keyboard, and a ring on every row they sweep across makes the panel look
                    // like a form.
                    .focus_visible(|style| style.border_color(focus_ink(cx)))
            })
            .when(control, |this| {
                // One shared handle for both input paths: the pointer and the keyboard are the
                // same action, and two copies of the target would be two things to keep in step.
                let action = Rc::new(action);
                let click_action = action.clone();
                let key_action = action;
                let click_panel = panel.clone();
                let key_panel = panel;
                this.hover(|this| this.bg(hover_wash(cx)))
                    .active(|this| this.bg(press_wash(cx)))
                    .on_click(move |_: &ClickEvent, _, cx| {
                        if let Some(panel) = click_panel.upgrade() {
                            let action = (*click_action).clone();
                            panel.update(cx, |panel, cx| panel.run_related_action(action, cx));
                        }
                    })
                    .on_key_down(move |event: &KeyDownEvent, _, cx| {
                        if !matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            return;
                        }
                        if let Some(panel) = key_panel.upgrade() {
                            let action = (*key_action).clone();
                            panel.update(cx, |panel, cx| panel.run_related_action(action, cx));
                        }
                        cx.stop_propagation();
                    })
            })
            .child(
                div()
                    .flex_none()
                    .w(px(RELATED_NAME_COLUMN))
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(text::LABEL)
                    .line_height(text::LABEL_LINE_HEIGHT)
                    .text_color(role::fg_tertiary(cx))
                    .child(SharedString::from(kind.to_owned())),
            )
            .child(
                div()
                    .id(format!("inspector-related-name-{kind}-{name}"))
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .tooltip(common::hover_hint(name.to_owned()))
                    .text_size(text::SUBTITLE)
                    .line_height(text::SUBTITLE_LINE_HEIGHT)
                    .text_color(name_ink)
                    .child(SharedString::from(name.to_owned())),
            )
            .when(count > 0, |this| {
                this.child(div().flex_none().child(
                    label_small(design::format::count(count)).text_color(role::fg_tertiary(cx)),
                ))
            })
            .when(!status.is_empty(), |this| {
                this.child(
                    div()
                        .flex_none()
                        .child(label_small(status).text_color(word_ink)),
                )
            })
            .when(control, |this| {
                this.child(
                    // The row's own affordance, so the resting ink of a control's
                    // glyph and not the count tier. `fg_disabled` would be wrong
                    // for the reason `managed_ink` gives — there is no disabled
                    // control on this surface — and `fg_tertiary` is one step too
                    // quiet beside a name at full strength, which reads as a row
                    // the reader cannot open.
                    div().flex_none().child(
                        Icon::new(IconName::ArrowRight)
                            .with_size(Size::Size(design::icon::IN_ROW))
                            .text_color(design::icon::resting(cx)),
                    ),
                )
            })
            .into_any_element()
    }
}

/// A scalar under a JSON pointer, trimmed, or `None` when it is absent or empty.
///
/// The trim is the point: Kubernetes fills `nodeName: ""` and `podIP: ""` on an object that has
//  neither, and a row that says `Node` with an empty value is a question the reader has to notice
//  and dismiss.
fn identity_scalar(object: &DynamicObject, path: &str) -> Option<String> {
    object
        .data
        .pointer(path)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// One Identity row: a fixed key column and a mono value that clips in the middle.
///
/// The value clips in the middle rather than at the end because all four of these values end in
/// the half that identifies them — a UID's tail, a digest's tail, a node's `-worker2` — and
/// `UI-SPEC.md` §7 fixes middle ellipsis for exactly that case.
fn identity_field(key: &'static str, value: &str, mono: bool, cx: &App) -> AnyElement {
    let shown = if value.chars().count() > UID_INLINE_CHARS {
        let head: String = value.chars().take(UID_INLINE_CHARS / 2).collect();
        let tail: String = value
            .chars()
            .skip(value.chars().count() - UID_INLINE_CHARS / 2)
            .collect();
        format!("{head}…{tail}")
    } else {
        value.to_owned()
    };
    h_flex()
        .id(format!("inspector-identity-field-{key}"))
        .debug_selector(move || format!("inspector-identity-field-{key}"))
        .w_full()
        .min_w(px(0.))
        .h(design::size::ROW)
        .gap(space::SM)
        .items_center()
        .child(
            div()
                .flex_none()
                .w(px(IDENTITY_KEY_COLUMN))
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(text::LABEL)
                .line_height(text::LABEL_LINE_HEIGHT)
                .text_color(role::fg_tertiary(cx))
                .child(SharedString::from(key.to_owned())),
        )
        .child(
            div()
                .id(format!("inspector-identity-value-{key}"))
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis_middle()
                .tooltip(common::hover_hint(value.to_owned()))
                .text_size(if mono { text::MONO_SM } else { text::LABEL })
                .line_height(text::LABEL_LINE_HEIGHT)
                .text_color(if mono {
                    role::fg_secondary(cx)
                } else {
                    role::fg_primary(cx)
                })
                .when(mono, |this| this.font(buffer_font(cx)))
                .child(SharedString::from(shown)),
        )
        .into_any_element()
}

/// One event in the vertical timeline `UI-SPEC.md` §10.3 asks for.
///
/// Events are not tabular — they are a story in time, and a table makes the reader sort by a
/// column to recover the order the API server already sent them in. The spine is 2px, the dot
/// sits on it, and the time and reason sit beside it.
fn event_timeline_row(
    event: &DynamicObject,
    last: bool,
    message_lines: Option<usize>,
    cx: &App,
) -> AnyElement {
    let kind = event_type(event);
    let severity = event_severity(&kind);
    let spine_ink = severity
        .map(|severity| match severity {
            Severity::Error => role::danger(cx),
            Severity::Warning => role::warning(cx),
            _ => role::fg_disabled(cx),
        })
        .unwrap_or_else(|| role::fg_disabled(cx));
    let reason = event_reason(event);
    let age = event_age(event);
    let message = event_message(event);
    let selector = format!("event-timeline-{}", event_identity(event));
    // A virtualised list gives every row the height of one measured row, and it measures with
    // `MaxContent` width — so a message that wraps on screen is one line tall when measured. The
    // two then disagree and the rows print on top of each other, which is what two events on a
    // real Pod did: `FailedMount`'s second line landed on `Scheduled`'s first.
    //
    // So the row states its own height. `message_lines` is the clamp, and the height follows from
    // it rather than from the text, so the measurement and the paint are the same number. The
    // whole message stays on the element's accessible name and one hover away.
    let height = message_lines
        .map(|lines| text::LABEL_LINE_HEIGHT * lines as f32 + text::LABEL_LINE_HEIGHT + space::XXS);
    h_flex()
        .id(selector.clone())
        .debug_selector(move || selector.clone())
        .w_full()
        .min_w(px(0.))
        .gap(space::SM)
        .items_start()
        .when_some(height, |this, height| this.h(height).overflow_hidden())
        .role(Role::ListItem)
        .aria_label(format!("{kind} event: {reason}, {age}. {message}"))
        // The spine runs the height of the row and stops at the last one, so the timeline reads
        // as a line that ended rather than as a line that was cut off.
        .child(
            v_flex()
                .flex_none()
                .w(design::size::SELECTION_RAIL)
                .items_center()
                .child(
                    div()
                        .w(design::size::SELECTION_RAIL)
                        .h(design::size::STATUS_DOT)
                        .rounded_full()
                        .bg(spine_ink),
                )
                .when(!last, |this| {
                    this.child(div().w(design::size::SELECTION_RAIL).flex_1().bg(spine_ink))
                }),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w(px(0.))
                .gap(space::XXS)
                .child(
                    h_flex()
                        .w_full()
                        .min_w(px(0.))
                        .gap(space::XS)
                        .items_center()
                        .child(
                            div()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(text::LABEL)
                                .line_height(text::LABEL_LINE_HEIGHT)
                                .text_color(role::fg_primary(cx))
                                .child(SharedString::from(reason.clone())),
                        )
                        // The spine's amber says "warning" in colour; the word
                        // says it too, because a status carried by colour alone
                        // is unreadable to anyone who cannot see the colour.
                        .when(severity == Some(Severity::Warning), |this| {
                            this.child(
                                div().flex_none().child(
                                    label_small("Warning").text_color(role::warning_word(cx)),
                                ),
                            )
                        })
                        .child(
                            div()
                                .flex_none()
                                .child(label_small(age).text_color(role::fg_tertiary(cx))),
                        ),
                )
                .when(!message.is_empty(), |this| {
                    this.child(
                        div()
                            .id(format!("inspector-event-message-{}", event_identity(event)))
                            .min_w(px(0.))
                            .whitespace_normal()
                            .text_size(text::LABEL)
                            .line_height(text::LABEL_LINE_HEIGHT)
                            .text_color(role::fg_secondary(cx))
                            // A clipped message is only honest if the rest of it is reachable, and
                            // GPUI hangs a tooltip off an element that owns an identity.
                            .when_some(message_lines, |this, lines| {
                                this.line_clamp(lines)
                                    .tooltip(common::hover_hint(message.clone()))
                            })
                            .child(SharedString::from(message.clone())),
                    )
                }),
        )
        .into_any_element()
}

/// The severity an event's type carries, or `None` for the types that are neither.
fn event_severity(kind: &str) -> Option<Severity> {
    match kind {
        "Warning" => Some(Severity::Warning),
        _ => None,
    }
}

fn describe_severity_icon(severity: Severity, cx: &App) -> AnyElement {
    // The marker sits on the panel background, so it is solved against that surface and not
    // against the canvas the default marker colour assumes.
    //
    // It is `design::size::STATUS_MARKER`, not the 12px it used to be, and the slot
    // around it is that same width - a marked row and an unmarked one still share
    // one key column and one value start, and the glyph got bigger without the
    // layout moving.
    Icon::new(design::severity_icon(severity))
        .with_size(Size::Size(design::size::STATUS_MARKER))
        .text_color(severity.marker_on(cx, role::surface_content(cx)))
        .into_any_element()
}

/// Height of the problems list at the cap: the rows a reader sees before scrolling.
fn problems_list_max_height(cx: &App) -> Pixels {
    problem_row_height(cx) * PROBLEMS_VISIBLE_ROWS as f32
}

/// Height of one problem row: an address line, a message of up to [`PROBLEM_MESSAGE_LINES`]
/// lines, and the vertical padding around them.
///
/// Composed from the tokens it is made of rather than written out, so raising the message clamp
/// or the reader's data font size moves the budget with it instead of quietly overflowing it —
/// the cap is the only thing standing between a dozen syntax errors and an editor with no height.
fn problem_row_height(cx: &App) -> Pixels {
    let address = crate::settings::data_typography(cx).line_height;
    let message = design::text::CAPTION_LINE_HEIGHT * PROBLEM_MESSAGE_LINES as f32;
    address + message + space::XXS * 3.
}

fn visible_detail_rows(
    mut rows: Vec<FieldRow>,
    limit: usize,
    expanded: bool,
) -> (Vec<FieldRow>, usize) {
    let total = rows.len();
    let hidden = total.saturating_sub(limit);
    if !expanded {
        rows.truncate(limit);
    }
    (rows, hidden)
}

fn label_rows(object: &DynamicObject) -> Vec<FieldRow> {
    let mut rows: Vec<FieldRow> = object
        .metadata
        .labels
        .as_ref()
        .into_iter()
        .flatten()
        // A label value is a string, so the type flag is true and the font decision follows it.
        .map(|(key, value)| (key.clone(), value.clone(), true))
        .collect();
    rows.sort();
    rows
}

/// Flattens every scalar under a prefix, tagging each row with the JSON type of its value.
///
/// The type is what decides the font, so a column cannot change size halfway down and an address
/// reads as code without a string-matching heuristic.
fn flatten_scalars(value: &Value, prefix: &str, out: &mut Vec<FieldRow>) {
    flatten_scalars_except(value, prefix, &[], out)
}

/// [`flatten_scalars`] with the full paths to leave out.
///
/// The skip is by whole path rather than by key name because the block that already shows a
/// container's name and image still has to show its resources, ports and mounts, and those live at
/// `containers[0].resources…` — one level below the two fields that would be duplicates. A
/// name-keyed skip would have to drop the whole array to drop two fields, and the block would lose
/// the only part of the spec a reader cannot get from Containers.
fn flatten_scalars_except(value: &Value, prefix: &str, skip: &[String], out: &mut Vec<FieldRow>) {
    match value {
        Value::Object(map) => {
            // An empty object is a field the cluster sent with nothing in it. It used to be
            // printed as the two characters `{}`, which is the source format rather than the
            // answer: a reader looking at `resources {}` is asking whether the Pod has limits,
            // and `{}` answers in a language the interface is not written in. One word, in the
            // quietest ink — see [`EMPTY_VALUE`] — and the field still gets its row, because a
            // field that exists and is unset is information and a field that vanished is a
            // question.
            if map.is_empty() && !prefix.is_empty() {
                push_field(prefix, EMPTY_VALUE, false, skip, out);
                return;
            }
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_scalars_except(child, &path, skip, out);
            }
        }
        Value::Array(items) => {
            if items.is_empty() && !prefix.is_empty() {
                push_field(prefix, EMPTY_VALUE, false, skip, out);
                return;
            }
            for (index, child) in items.iter().enumerate() {
                flatten_scalars_except(child, &format!("{prefix}[{index}]"), skip, out);
            }
        }
        _ => {
            if let Some(text) = scalar_text(value) {
                push_field(prefix, &text, matches!(value, Value::String(_)), skip, out);
            }
        }
    }
}

/// The Spec paths the Containers block already prints: each container's `name` and `image`.
///
/// Only the `spec` subtree a Pod carries is scanned, because Containers is a Pod-only block; a
/// Deployment's `spec.template.spec.containers` is not printed anywhere else, so skipping those
/// paths would delete information rather than remove a duplicate.
fn container_identity_paths(spec: &Value, pod: bool) -> Vec<String> {
    const ARRAYS: [&str; 3] = ["containers", "initContainers", "ephemeralContainers"];
    if !pod {
        return Vec::new();
    }
    let mut paths = Vec::new();
    for array in ARRAYS {
        let Some(items) = spec.get(array).and_then(Value::as_array) else {
            continue;
        };
        for (index, _) in items.iter().enumerate() {
            for key in ["name", "image"] {
                paths.push(format!("{array}[{index}].{key}"));
            }
        }
    }
    paths
}

/// Adds one row unless its path is one of the skipped ones, or the path is the empty root.
fn push_field(path: &str, text: &str, is_string: bool, skip: &[String], out: &mut Vec<FieldRow>) {
    if path.is_empty() || skip.iter().any(|skipped| skipped == path) {
        return;
    }
    out.push((path.to_owned(), text.to_owned(), is_string));
}

/// Flattens the fields of a subtree, dropping the keys `skip` names.
///
/// A Describe block that already reads a subtree must not repeat it under raw JSON
/// paths, so the caller lists the keys it handles itself.
fn flatten_scalars_below(value: &Value, skip: &[&str], out: &mut Vec<FieldRow>) {
    let Value::Object(map) = value else {
        flatten_scalars(value, "", out);
        return;
    };
    for (key, child) in map {
        if skip.contains(&key.as_str()) {
            continue;
        }
        flatten_scalars(child, key, out);
    }
}

/// Status fields the block above already says in words, and the subtrees the Conditions and
/// Containers blocks already read.
///
/// `phase`, `reason` and `message` are here because [`status_headline`] *is* those three lines,
/// at a size a reader can act on: printing `Phase Running` under a coloured `Running` is the same
/// sentence twice, and the second copy is the one that takes a row.
const POD_STATUS_HANDLED: [&str; 7] = [
    "phase",
    "reason",
    "message",
    "conditions",
    "containerStatuses",
    "initContainerStatuses",
    "ephemeralContainerStatuses",
];

/// Status fields every kind's own blocks already say in words.
///
/// `conditions` is the Conditions block for all twelve kinds, not just Pods: a Deployment reports
/// `Available`, `Progressing` and `ReplicaFailure`, and flattening them into raw paths
/// (`conditions[0].lastTransitionTime`) puts a wall of timestamps in the middle of the block whose
/// only job is to say whether the object is fine.
const STATUS_HANDLED_ALL: [&str; 1] = ["conditions"];

/// Status rows for a known kind: everything in `status` that no other block in this panel already
/// says. What is left is genuinely the odd field — `configHash`, `podIPs`, `startTime`,
/// `nominatedNodeName` — and a block of odd fields is worth having; a block that repeats the three
/// lines above it is not.
fn describe_status_rows(object: &DynamicObject, kind: &str) -> Vec<FieldRow> {
    let Some(status) = object.data.get("status") else {
        return Vec::new();
    };
    let handled: &[&str] = match kind {
        "Pod" => &POD_STATUS_HANDLED,
        _ => &STATUS_HANDLED_ALL,
    };
    let mut rows = Vec::new();
    flatten_scalars_below(status, handled, &mut rows);
    drop_duplicate_addresses(rows)
}

/// `status.hostIP` and `status.hostIPs[0].ip` are one address, and `status.podIP` and
/// `status.podIPs[0].ip` are one address. The API server keeps both because it has to answer v1
/// and v1beta1 from one stored object; a client that only reads v1 has no use for the pair.
///
/// Two rows reading `172.18.0.3` and `172.18.0.3` is a row the reader has to stop and check, in
/// the one block whose whole job is to be believed without checking — and [`describe_status_rows`]
/// already says a block that repeats itself is not worth having.
///
/// Only index `0` goes. A dual-stack Pod's second address is real information and keeps its own
/// row, at its real path, so every row that survives can still be pasted into a `kubectl` command.
fn drop_duplicate_addresses(rows: Vec<FieldRow>) -> Vec<FieldRow> {
    let values: BTreeMap<String, String> = rows
        .iter()
        .map(|(path, value, _)| (path.clone(), value.clone()))
        .collect();
    rows.into_iter()
        .filter(|(path, value, _)| match path.strip_suffix("IPs[0].ip") {
            Some(singular) => values.get(&format!("{singular}IP")) != Some(value),
            None => true,
        })
        .collect()
}

/// The one-line answer to "is this thing healthy", read off the object.
///
/// `UI-REDESIGN.md` §3.4 puts Status first and always open because that is the question the panel
/// was opened to answer, and the mockup's block is three lines rather than the field list a
/// `describe` prints: the state word, the counts beside it, and the reason under them. A reader
/// who wanted `status.containerStatuses[0].lastState` can open Containers; a reader who wanted to
/// know whether to worry cannot afford to read it.
///
/// The severity is the product's central inversion (`UI-SPEC.md` §0's third rule): a healthy
/// resource is grey and only Pending / Failed / Error is coloured, so a table of ten thousand
/// pods stays quiet and the one that is broken is the only thing on the screen with a colour.
#[derive(Clone)]
struct StatusHeadline {
    /// The state word: `Running`, `CrashLoopBackOff`, `Pending`, `Unschedulable`.
    word: String,
    severity: Severity,
    /// The counts beside the word: `1/1 ready · 0 restarts`.
    summary: Option<String>,
    /// Why, in the cluster's own words. Clamped by the caller.
    reason: Option<String>,
}

/// The word a waiting or terminated container's state carries, which is more specific than the
/// Pod's phase and is what `kubectl get pod` shows in its STATUS column.
fn container_state_word(state: Option<&Value>) -> Option<String> {
    let state = state?;
    for key in ["waiting", "terminated", "running"] {
        let Some(inner) = state.get(key) else {
            continue;
        };
        if let Some(reason) = inner.get("reason").and_then(Value::as_str) {
            // `ContainerCreating` is progress, and `Completed` is a success, so both are stated
            // as their own words rather than folded into the phase.
            return Some(reason.to_owned());
        }
        // The JSON keys are lower case and the word is shown to a person, so the fallback is
        // capitalised here rather than at the call site: a headline that says `terminated` in
        // sentence case next to `CrashLoopBackOff` is a different typeface's opinion, not a fact
        // about the object.
        let mut word = key.to_owned();
        if let Some(first) = word.get_mut(0..1) {
            first.make_ascii_uppercase();
        }
        return Some(word);
    }
    None
}

/// How bad a container's own state is, which is not the same as how bad the Pod is.
fn container_state_severity(word: &str, state: Option<&Value>) -> Severity {
    // An exit code is the fact, and a `reason` alone is not: a container that terminated with
    // `reason: Error` and `exitCode: 0` is a container that finished, and one that terminated
    // with `reason: Completed` and `exitCode: 137` is a container that was killed. Reading only
    // the reason is how a Pod whose container was OOM-killed gets a green `Running` headline.
    if let Some(code) = state
        .and_then(|state| state.pointer("/terminated/exitCode"))
        .and_then(Value::as_i64)
    {
        return if code == 0 {
            Severity::Success
        } else {
            Severity::Error
        };
    }
    match word {
        // A crash loop and a missing image are the two states a human has to act on.
        "CrashLoopBackOff"
        | "ImagePullBackOff"
        | "ErrImagePull"
        | "CreateContainerConfigError"
        | "CreateContainerError"
        | "InvalidImageName"
        | "Error"
        | "OOMKilled" => Severity::Error,
        // Still converging. Warning, not Error: a Pod pulling a 2GB image is not broken.
        "ContainerCreating" | "PodInitializing" | "Terminated" => Severity::Warning,
        // `Running` and `Completed` are the quiet outcomes, and `Completed` is the good one.
        _ => Severity::Success,
    }
}

/// Seconds since an object was created, or `None` when it carries no timestamp.
fn object_age_seconds(object: &DynamicObject) -> Option<i64> {
    let created = object.metadata.creation_timestamp.as_ref()?;
    let seconds = created.0.as_second() - now_seconds();
    Some(seconds.max(0))
}

/// The current wall clock in seconds, which the age arithmetic needs.
fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or_default()
}

/// Ready and restart counts across a Pod's app containers, as `1/2 ready · 14 restarts`.
///
/// Init containers are excluded on purpose: they are supposed to have finished, so counting them
/// would report a healthy Pod as not ready for as long as its migration runs.
fn pod_readiness_summary(object: &DynamicObject) -> Option<String> {
    let statuses = object
        .data
        .pointer("/status/containerStatuses")
        .and_then(Value::as_array)?;
    if statuses.is_empty() {
        return None;
    }
    let ready = statuses
        .iter()
        .filter(|status| status.get("ready").and_then(Value::as_bool) == Some(true))
        .count();
    let restarts: i64 = statuses
        .iter()
        .filter_map(|status| status.get("restartCount").and_then(Value::as_i64))
        .sum();
    Some(format!(
        "{ready}/{} ready · {restarts} restarts",
        statuses.len()
    ))
}

/// Replica counts for a workload kind, as `2/3 ready`.
///
/// `desired` comes from `spec.replicas` when the object has one and from `status.replicas`
/// otherwise, so a Deployment that is still rolling out reports the target the reader is
/// reasoning about rather than the count it happens to have right now.
fn workload_readiness_summary(object: &DynamicObject, kind: &str) -> Option<String> {
    if !matches!(
        kind,
        "Deployment" | "StatefulSet" | "ReplicaSet" | "DaemonSet"
    ) {
        return None;
    }
    let status = object.data.get("status")?;
    let number = |path: &str| status.pointer(path).and_then(Value::as_i64);
    if kind == "DaemonSet" {
        let desired = number("/desiredNumberScheduled").unwrap_or_default();
        let ready = number("/numberReady").unwrap_or_default();
        return (desired > 0 || ready > 0).then(|| format!("{ready}/{desired} ready"));
    }
    let ready = number("/readyReplicas")
        .or_else(|| number("/availableReplicas"))
        .or_else(|| number("/numberReady"))
        .unwrap_or_default();
    let desired = object
        .data
        .pointer("/spec/replicas")
        .and_then(Value::as_i64)
        .or_else(|| number("/replicas"))
        .unwrap_or(ready);
    Some(format!("{ready}/{desired} ready"))
}

/// The headline for a known kind, or `None` when the object reports no status at all.
fn status_headline(object: &DynamicObject, kind: &str) -> Option<StatusHeadline> {
    if kind == "Pod" {
        return pod_status_headline(object);
    }
    if kind == "Node" {
        return node_status_headline(object);
    }
    workload_status_headline(object, kind)
}

fn pod_status_headline(object: &DynamicObject) -> Option<StatusHeadline> {
    let phase = object
        .data
        .pointer("/status/phase")
        .and_then(Value::as_str)
        .unwrap_or("Unknown")
        .to_owned();
    let reason = object
        .data
        .pointer("/status/reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
        .map(str::to_owned);
    let message = object
        .data
        .pointer("/status/message")
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .map(str::to_owned);

    // A container's own state is more specific than the phase, so it wins the word. The first one
    // that is not healthy does, because a Pod with one broken container is broken by that one and
    // the reader wants its name, not the third container's.
    let mut waiting: Option<(String, Severity, Option<String>)> = None;
    if let Some(statuses) = object
        .data
        .pointer("/status/containerStatuses")
        .and_then(Value::as_array)
    {
        for status in statuses {
            let state = status.get("state");
            let word = container_state_word(state);
            let Some(word) = word else { continue };
            let severity = container_state_severity(&word, state);
            if severity == Severity::Success {
                continue;
            }
            let detail = status
                .pointer("/state/waiting/message")
                .or_else(|| status.pointer("/state/terminated/message"))
                .and_then(Value::as_str)
                .filter(|message| !message.is_empty())
                .map(str::to_owned);
            waiting = Some((word, severity, detail));
            break;
        }
    }

    let (word, severity) = match &waiting {
        Some((word, severity, _)) => (word.clone(), *severity),
        None => {
            // No container is complaining, so the phase decides - and `Pending` is graded by age,
            // because nine thousand Pending pods that are three seconds old are normal and the
            // four hundred that are nine minutes old are the incident (`UI-SPEC.md` §0).
            let severity = match phase.as_str() {
                "Running" => Severity::Success,
                "Succeeded" => Severity::Success,
                "Failed" => Severity::Error,
                "Pending" => match object_age_seconds(object) {
                    Some(age) if age > 300 => Severity::Error,
                    Some(age) if age > 30 => Severity::Warning,
                    _ => Severity::Muted,
                },
                _ => Severity::Muted,
            };
            // `Unschedulable` is the reason a Pending Pod is Pending, and it is the answer to
            // "why has this not started", so it replaces the phase as the word and the phase
            // becomes the second half of the summary.
            let word = reason.clone().unwrap_or_else(|| phase.clone());
            (word, severity)
        }
    };
    Some(StatusHeadline {
        word,
        severity,
        summary: pod_readiness_summary(object),
        // The cluster's own message is the last word, because it names the node, the image or the
        // quota that a container's waiting reason does not. `status.reason` is deliberately not
        // the reason line: when it is set it *is* the word, and printing it twice is a row that
        // says the same thing in two sizes.
        reason: message.or_else(|| waiting.and_then(|(_, _, detail)| detail)),
    })
}

fn node_status_headline(object: &DynamicObject) -> Option<StatusHeadline> {
    let conditions = object
        .data
        .pointer("/status/conditions")
        .and_then(Value::as_array)?;
    // `Ready=Unknown` is a node that has stopped reporting, and `Ready=False` is a node whose
    // kubelet says it cannot run pods. They are the same verdict and the same word, because a
    // reader who is about to move a workload off a node does not care which of the two the
    // kubelet used.
    let ready = conditions.iter().any(|condition| {
        condition.get("type").and_then(Value::as_str) == Some("Ready")
            && condition.get("status").and_then(Value::as_str) == Some("True")
    });
    // `NoSchedule` on the unschedulable taint is a different thing entirely: the node is fine
    // and is refusing work on purpose. It is the one node fact that is worth a word of its own,
    // because a Pod stuck `Pending` on an unschedulable node has an answer the Ready condition
    // will never give.
    let unschedulable = object
        .data
        .pointer("/spec/taints")
        .and_then(Value::as_array)
        .is_some_and(|taints| {
            taints.iter().any(|taint| {
                taint.get("key").and_then(Value::as_str) == Some("node.kubernetes.io/unschedulable")
                    && taint.get("effect").and_then(Value::as_str) != Some("PreferNoSchedule")
            })
        });
    let (word, severity) = if unschedulable {
        ("Unschedulable", Severity::Warning)
    } else if ready {
        ("Ready", Severity::Success)
    } else {
        ("NotReady", Severity::Error)
    };
    let reason = conditions
        .iter()
        .find(|condition| {
            condition.get("type").and_then(Value::as_str) == Some("Ready")
                && condition.get("status").and_then(Value::as_str) != Some("True")
        })
        .and_then(|condition| condition.get("message").and_then(Value::as_str))
        .filter(|message| !message.is_empty())
        .map(str::to_owned);
    Some(StatusHeadline {
        word: word.to_owned(),
        severity,
        summary: node_capacity_summary(object),
        reason,
    })
}

/// What a node can take, as `CPU 8 · Memory 30Gi`.
///
/// Allocatable rather than capacity, because allocatable is the number a scheduler admits
/// against: a node with 8 cores of which 200m are reserved cannot run eight pods that each ask
/// for a core. It is the *used* figures the reader usually wants too, and this object does not
/// carry them - they live in metrics-server - so the honest answer is the capacity and the
/// reason line above it, rather than a zero the reader would take for an idle machine.
fn node_capacity_summary(object: &DynamicObject) -> Option<String> {
    let allocatable = object.data.pointer("/status/allocatable")?;
    let mut parts = Vec::new();
    for resource in ["cpu", "memory"] {
        let capacity = allocatable.get(resource)?.as_str()?;
        if capacity.is_empty() || capacity == "0" {
            continue;
        }
        parts.push(format!("{resource} {capacity}"));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn workload_status_headline(object: &DynamicObject, kind: &str) -> Option<StatusHeadline> {
    // Only the kinds that report a state this can read honestly. A Service has no useful
    // `status`, and printing `Active` for one would be a word invented by this function rather
    // than a fact the cluster sent — which is the one thing a status line may not be.
    if !matches!(
        kind,
        "Deployment" | "StatefulSet" | "ReplicaSet" | "DaemonSet" | "Job" | "CronJob"
    ) {
        return None;
    }
    let status = object.data.get("status")?;
    let summary = workload_readiness_summary(object, kind);
    // The condition that is failing, in the order a reader would check them. `Available` and
    // `Progressing` are the two a Deployment reports; `Ready` is what a StatefulSet reports.
    let failing = status
        .get("conditions")
        .and_then(Value::as_array)
        .and_then(|conditions| {
            conditions.iter().find(|condition| {
                let polarity =
                    condition_polarity(condition.get("type").and_then(Value::as_str).unwrap_or(""));
                let status = condition
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                matches!((status, polarity), ("False", ConditionPolarity::Healthy))
            })
        });
    let (word, severity, reason) = match failing {
        Some(condition) => {
            let reason = condition
                .get("reason")
                .and_then(Value::as_str)
                .filter(|reason| !reason.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| "Degraded".to_owned());
            (
                reason.clone(),
                Severity::Warning,
                condition
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|message| !message.is_empty())
                    .map(str::to_owned),
            )
        }
        None => {
            let ready = status
                .pointer("/readyReplicas")
                .or_else(|| status.pointer("/numberReady"))
                .or_else(|| status.pointer("/succeeded"))
                .and_then(Value::as_i64);
            let word = match kind {
                // A Job is either working or finished. `active` is the count of running pods,
                // so a Job with no active pods has either completed or failed, and only the
                // failed conditions below say which — which is why the failure arm comes first.
                "Job" | "CronJob" => {
                    let active = status.get("active").and_then(Value::as_i64).unwrap_or(0);
                    let failed = status.get("failed").and_then(Value::as_i64).unwrap_or(0);
                    if failed > 0 {
                        "Failed"
                    } else if active > 0 {
                        "Running"
                    } else {
                        "Complete"
                    }
                }
                _ => match ready {
                    Some(0) => "Unavailable",
                    _ => "Healthy",
                },
            };
            let severity = match word {
                "Healthy" | "Complete" | "Running" => Severity::Success,
                "Failed" => Severity::Error,
                _ => Severity::Warning,
            };
            (word.to_owned(), severity, None)
        }
    };
    Some(StatusHeadline {
        word,
        severity,
        summary,
        reason,
    })
}

/// The block the Status section leads with, at the sizes the mockup draws them.
///
/// The word is `SUBTITLE` in status ink, the counts beside it are `LABEL` in the tertiary ink,
/// and the reason is `LABEL` in the secondary ink under both. A healthy object therefore reads as
/// three quiet lines and a broken one as a coloured word with its reason directly under it, which
/// is the whole point: the reason line is what turns a colour into a diagnosis.
///
/// It is one block rather than two rows because the two lines are one sentence. A section whose
/// rows are 32px apart would put the word and its reason on either side of a field row and turn a
/// diagnosis into a list, so the block carries its own 4px leading and a 4px foot — the same
/// amount of air `UI-SPEC.md` §2.1 puts between a heading and what it introduces.
fn status_headline_block(headline: StatusHeadline, cx: &App) -> AnyElement {
    let ink = role::status_word_for(headline.severity, cx);
    let mut block = v_flex()
        .id("inspector-status-headline")
        .debug_selector(|| "inspector-status-headline".to_owned())
        .w_full()
        .min_w(px(0.))
        .gap(space::XS)
        .pb(space::XS)
        .child(
            h_flex()
                .w_full()
                .min_w(px(0.))
                .gap(space::SM)
                .items_center()
                .child(
                    div()
                        .flex_none()
                        .text_size(text::SUBTITLE)
                        .line_height(text::SUBTITLE_LINE_HEIGHT)
                        .text_color(ink)
                        .font_weight(text::MEDIUM)
                        .child(SharedString::from(headline.word.clone())),
                )
                .when_some(headline.summary, |this, summary| {
                    this.child(
                        div()
                            .id("inspector-status-summary")
                            .debug_selector(|| "inspector-status-summary".to_owned())
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .tooltip(common::hover_hint(summary.clone()))
                            .text_size(text::LABEL)
                            .line_height(text::LABEL_LINE_HEIGHT)
                            .text_color(role::fg_tertiary(cx))
                            .child(SharedString::from(summary)),
                    )
                }),
        );
    if let Some(reason) = headline.reason {
        block = block.child(
            div()
                .id("inspector-status-reason")
                .debug_selector(|| "inspector-status-reason".to_owned())
                .w_full()
                .min_w(px(0.))
                .whitespace_normal()
                .tooltip(common::hover_hint(reason.clone()))
                .text_size(text::LABEL)
                .line_height(text::LABEL_LINE_HEIGHT)
                .text_color(role::fg_secondary(cx))
                .child(SharedString::from(reason)),
        );
    }
    block.into_any_element()
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Number(number) => Some(number.to_string()),
        // `null` was the source format's word for the same thing `{}` and `[]` used to print, so
        // it gets the same one word as they do. A JSON null in a Kubernetes object is a field the
        // server set to nothing, which is what a reader has to be told.
        Value::Null => Some(EMPTY_VALUE.to_owned()),
        _ => None,
    }
}

/// Whether a condition type is healthy when its status is `True` or `False`.
///
/// Most conditions are healthy when `True` (`Ready`, `Available`, `Initialized`).
/// Failure and pressure conditions are the opposite: `Failed=False` and
/// `MemoryPressure=False` mean the workload is fine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConditionPolarity {
    Healthy,
    Unhealthy,
}

fn condition_polarity(condition_type: &str) -> ConditionPolarity {
    match condition_type {
        "Failed" | "OOMKilled" | "Evicted" | "MemoryPressure" | "DiskPressure" | "PIDPressure"
        | "NetworkUnavailable" | "KernelDeadlock" => ConditionPolarity::Unhealthy,
        _ => ConditionPolarity::Healthy,
    }
}

fn condition_rows(
    object: &DynamicObject,
    stacked: bool,
    values: &ValueRows,
    cx: &App,
) -> Vec<AnyElement> {
    let Some(conditions) = object
        .data
        .pointer("/status/conditions")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    conditions
        .iter()
        .flat_map(|condition| {
            let kind = condition
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("Condition")
                .to_owned();
            let status = condition
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("Unknown")
                .to_owned();
            let reason = condition
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let message = condition
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let severity =
                match (status.as_str(), condition_polarity(&kind)) {
                    ("True", ConditionPolarity::Healthy)
                    | ("False", ConditionPolarity::Unhealthy) => Severity::Success,
                    ("True", ConditionPolarity::Unhealthy)
                    | ("False", ConditionPolarity::Healthy) => Severity::Warning,
                    _ if status.eq_ignore_ascii_case("critical") => Severity::Error,
                    _ => Severity::Muted,
                };
            let summary = if reason.is_empty() {
                status
            } else {
                format!("{status} · {reason}")
            };
            let mut rows = vec![status_field_row(
                &kind,
                &summary,
                ValueStyle::prose(),
                stacked,
                severity,
                format!("inspector-describe-condition-{kind}"),
                values,
                cx,
            )];
            if !message.is_empty() {
                rows.push(field_row_with_icon(
                    "Message",
                    &message,
                    ValueStyle::prose(),
                    stacked,
                    None,
                    format!("inspector-describe-condition-message-{kind}"),
                    values,
                    cx,
                ));
            }
            rows
        })
        .collect()
}

/// One container group of a Pod: app containers, init containers, and ephemeral debuggers.
struct ContainerGroup {
    label: &'static str,
    spec_path: &'static str,
    status_path: &'static str,
    /// Init containers must finish before the app containers start.
    must_succeed: bool,
}

const CONTAINER_GROUPS: [ContainerGroup; 3] = [
    ContainerGroup {
        label: "Containers",
        spec_path: "/spec/containers",
        status_path: "/status/containerStatuses",
        must_succeed: false,
    },
    ContainerGroup {
        label: "Init Containers",
        spec_path: "/spec/initContainers",
        status_path: "/status/initContainerStatuses",
        must_succeed: true,
    },
    ContainerGroup {
        label: "Ephemeral Containers",
        spec_path: "/spec/ephemeralContainers",
        status_path: "/status/ephemeralContainerStatuses",
        must_succeed: false,
    },
];

fn container_rows(
    object: &DynamicObject,
    stacked: bool,
    values: &ValueRows,
    cx: &App,
) -> Vec<AnyElement> {
    let mut rows = Vec::new();
    for group in CONTAINER_GROUPS {
        let containers = object
            .data
            .pointer(group.spec_path)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if containers.is_empty() {
            continue;
        }
        let statuses = object
            .data
            .pointer(group.status_path)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut section_rows = container_group_rows(
            group.must_succeed,
            &containers,
            &statuses,
            stacked,
            values,
            cx,
        );
        if section_rows.is_empty() {
            continue;
        }
        if rows.is_empty() && group.label == "Containers" {
            rows.append(&mut section_rows);
            continue;
        }
        rows.push(
            h_flex()
                .w_full()
                .min_w(px(0.))
                .h(design::size::ROW)
                .pl(DESCRIBE_MARKER_SLOT)
                .items_center()
                .child(label_body(group.label).text_color(role::fg_tertiary(cx)))
                .into_any_element(),
        );
        rows.append(&mut section_rows);
    }
    rows
}

fn container_group_rows(
    must_succeed: bool,
    containers: &[Value],
    statuses: &[Value],
    stacked: bool,
    values: &ValueRows,
    cx: &App,
) -> Vec<AnyElement> {
    containers
        .iter()
        .flat_map(|container| {
            let name = container
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("container")
                .to_owned();
            let image = container
                .get("image")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let status = statuses
                .iter()
                .find(|status| status.get("name").and_then(Value::as_str) == Some(name.as_str()));
            let ready = status
                .and_then(|status| status.get("ready"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let restarts = status
                .and_then(|status| status.get("restartCount"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let state = status
                .and_then(|status| status.get("state"))
                .map(container_state)
                .unwrap_or_default();
            let succeeded = status
                .and_then(|status| status.pointer("/state/terminated/exitCode"))
                .and_then(Value::as_i64)
                .is_some_and(|code| code == 0);
            // A container that terminated on a failure will not come back on its own, so it is
            // the one state that is an error rather than a caution.
            let failed = status
                .and_then(|status| status.pointer("/state/terminated/exitCode"))
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0);
            // An init container is expected to finish, not to stay ready. Only its exit
            // code decides whether the Pod can start.
            let severity = if must_succeed {
                match status {
                    Some(_) if succeeded => Severity::Success,
                    Some(_) => Severity::Warning,
                    None => Severity::Muted,
                }
            } else if failed {
                Severity::Error
            } else if !ready {
                // The row reads "Not ready", so the marker cannot stay neutral. A container the
                // API has not reported a state for yet is still not ready, and colour, icon,
                // text and shape have to agree on the state.
                Severity::Warning
            } else if restarts > 0 {
                // A container that came back is serving, but the restarts are the reason to
                // look at this row.
                Severity::Warning
            } else {
                Severity::Success
            };
            let readiness = if must_succeed {
                if succeeded { "Completed" } else { "Incomplete" }
            } else if ready {
                "Ready"
            } else {
                "Not ready"
            };
            let summary = if state.is_empty() {
                format!("{readiness} · Restart Count: {restarts}")
            } else {
                format!("{state} · {readiness} · Restart Count: {restarts}")
            };
            let selector = format!("inspector-describe-container-{name}");
            let mut rows = vec![status_field_row(
                &name,
                &summary,
                ValueStyle::prose(),
                stacked,
                severity,
                if must_succeed {
                    format!("{selector}-init")
                } else {
                    selector
                },
                values,
                cx,
            )];
            if !image.is_empty() {
                rows.push(field_row_with_icon(
                    "Image",
                    &image,
                    // An image reference is a string, so it takes the buffer font.
                    ValueStyle::data(true),
                    stacked,
                    None,
                    format!("inspector-container-image-{name}"),
                    values,
                    cx,
                ));
            }
            rows
        })
        .collect()
}

fn container_state(state: &Value) -> String {
    for key in ["running", "waiting", "terminated"] {
        if let Some(inner) = state.get(key) {
            let reason = inner.get("reason").and_then(Value::as_str);
            return match reason {
                Some(reason) => format!("{key} ({reason})"),
                None => key.to_owned(),
            };
        }
    }
    String::new()
}

fn events_newest_first(events: Vec<DynamicObject>) -> Vec<DynamicObject> {
    let mut dated = Vec::with_capacity(events.len());
    let mut undated = Vec::new();
    for event in events {
        match event_timestamp(&event) {
            Some(timestamp) => dated.push((timestamp, event)),
            None => undated.push(event),
        }
    }
    dated.sort_by(|(left, _), (right, _)| right.cmp(left));
    dated
        .into_iter()
        .map(|(_, event)| event)
        .chain(undated)
        .collect()
}

/// Sorts events and rejects a payload for a different object.
///
/// The request resolves by name, so an object recreated under that name answers with a
/// new UID. That is a reloadable failure, not content to show.
fn prepare_describe_data(data: DescribeData, expected_uid: &str) -> Result<DescribeData, String> {
    if !expected_uid.is_empty() && data.object.metadata.uid.as_deref() != Some(expected_uid) {
        return Err(OBJECT_REPLACED_REASON.to_owned());
    }
    Ok(DescribeData {
        events: events_newest_first(data.events),
        ..data
    })
}

fn event_timestamp(event: &DynamicObject) -> Option<i128> {
    [
        "/lastTimestamp",
        "/eventTime",
        "/series/lastObservedTime",
        "/firstTimestamp",
    ]
    .iter()
    .find_map(|pointer| {
        event
            .data
            .pointer(pointer)
            .and_then(Value::as_str)
            .and_then(|text| text.parse::<jiff::Timestamp>().ok())
            .map(|time| time.as_nanosecond())
    })
    .or_else(|| {
        event
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|created| created.0.as_nanosecond())
    })
}

fn event_seconds(event: &DynamicObject) -> Option<i64> {
    event_timestamp(event)
        .map(|timestamp| timestamp.div_euclid(1_000_000_000))
        .and_then(|seconds| i64::try_from(seconds).ok())
}

fn event_type(event: &DynamicObject) -> String {
    event
        .data
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("Normal")
        .to_owned()
}

fn event_reason(event: &DynamicObject) -> String {
    event
        .data
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn event_identity(event: &DynamicObject) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    event_timestamp(event).hash(&mut hasher);
    event.metadata.uid.hash(&mut hasher);
    event.metadata.name.hash(&mut hasher);
    event_reason(event).hash(&mut hasher);
    event_message(event).hash(&mut hasher);
    hasher.finish()
}

fn event_message(event: &DynamicObject) -> String {
    event
        .data
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn event_age(event: &DynamicObject) -> String {
    event_seconds(event).map_or_else(|| "—".to_owned(), format_age)
}

fn format_age(seconds: i64) -> String {
    let now = jiff::Timestamp::now().as_second();
    let age = now.saturating_sub(seconds).max(0);
    if age >= 86_400 {
        format!("{}d", age / 86_400)
    } else if age >= 3_600 {
        format!("{}h", age / 3_600)
    } else if age >= 60 {
        format!("{}m", age / 60)
    } else {
        format!("{age}s")
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gpui_kit::{Entity, TestAppContext};
    use k8s_core::cluster_data::{ClusterDataPort, DataFuture};
    use k8s_core::metrics::{MetricsError, NodeMetric, PodMetric};
    use k8s_core::overview::Overview;
    use serde_json::json;

    use super::*;
    use crate::panels::inspector_data::{DescribeData, InspectorSource, ObjectRef};

    /// gpui-kit installs its own theme and global state, so a test only has to bring up the
    /// app's keymap: the panel's hints read their chords from it.
    fn init_app(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        cx.update(|cx| {
            crate::keymap::install_target_default(cx).expect("the built-in keymap loads");
        });
    }

    /// A panel with one document loaded and the editor on screen, focused.
    ///
    /// The tab is set here rather than left to the panel's own default because the fixture's
    /// subject is the editor: it focuses `yaml_view`, and a fixture that focuses a view the panel
    /// is not showing is a fixture that only works by accident. Tests about which tab a reader
    /// lands on use `source_setup`, which leaves the panel in its shipped state.
    fn setup<'a>(
        cx: &'a mut TestAppContext,
        yaml: &str,
    ) -> (Entity<InspectorPanel>, &'a mut gpui_kit::VisualTestContext) {
        init_app(cx);
        let (panel, cx) = cx.add_window_view(|_, cx| InspectorPanel::new(cx));
        panel.update(cx, |panel, cx| {
            panel.show_tab(InspectorTab::Yaml, cx);
            panel.set_yaml(Some(yaml.to_owned()), cx);
        });
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel
                    .yaml_view
                    .update(cx, |view, cx| view.focus(window, cx));
            });
        });
        (panel, cx)
    }

    fn assert_toolbar_actions(
        cx: &mut gpui_kit::VisualTestContext,
        toolbar_selector: &'static str,
        action_selectors: &[&'static str],
    ) {
        let toolbar = cx
            .debug_bounds(toolbar_selector)
            .unwrap_or_else(|| panic!("{toolbar_selector}"));
        assert_eq!(
            f32::from(toolbar.size.height),
            f32::from(design::size::TOOLBAR)
        );
        let toolbar_left = f32::from(toolbar.origin.x);
        let toolbar_right = f32::from(toolbar.origin.x + toolbar.size.width);
        let toolbar_top = f32::from(toolbar.origin.y);
        let toolbar_bottom = f32::from(toolbar.origin.y + toolbar.size.height);
        for selector in action_selectors {
            let action = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector}"));
            let action_left = f32::from(action.origin.x);
            let action_right = f32::from(action.origin.x + action.size.width);
            let action_top = f32::from(action.origin.y);
            let action_bottom = f32::from(action.origin.y + action.size.height);
            assert!(
                action_left >= toolbar_left
                    && action_right <= toolbar_right
                    && action_top >= toolbar_top
                    && action_bottom <= toolbar_bottom,
                "{selector} is outside {toolbar_selector}"
            );
            assert!(
                ((action_top - toolbar_top) - (toolbar_bottom - action_bottom)).abs() <= 1.0,
                "{selector} is not vertically centered in {toolbar_selector}"
            );
        }
    }

    fn yaml_text(panel: &Entity<InspectorPanel>, cx: &mut gpui_kit::VisualTestContext) -> String {
        cx.update(|_, cx| panel.read(cx).yaml_view.read(cx).text().unwrap_or_default())
    }

    fn dirty(panel: &Entity<InspectorPanel>, cx: &mut gpui_kit::VisualTestContext) -> bool {
        cx.update(|_, cx| panel.read(cx).is_dirty(cx))
    }

    /// How far a tab-order walk goes before it gives up. The tab order wraps, so one pass over
    /// the panel reaches everything the value focus pool and the scroll regions register.
    const TAB_ORDER_WALK_LIMIT: usize = VALUE_FOCUS_POOL_SIZE + 64;

    /// Typing pause before the YAML editor parses the document, mirroring
    /// `yaml_editor::VALIDATE_DEBOUNCE`, which is private to that module.
    ///
    /// A copy of a private constant is a test that goes quiet instead of going red: raise the
    /// real debounce and the test still waits on this number, so the coverage is gone and
    /// nothing says so. `yaml_editor` has to publish the constant for this to be read instead of
    /// mirrored, and until it does, the test that uses it brackets the value from both sides -
    /// the editor is silent one millisecond before this number and has reported by it - so a
    /// change to the real debounce fails here loudly.
    const VALIDATE_DEBOUNCE_TEST: Duration = Duration::from_millis(300);

    /// Walks the rendered tab order and reports whether the keyboard reaches `target`.
    ///
    /// `Window::focus` accepts any handle, mounted or not, so reaching a handle proves less than
    /// Tab does. This is the check that a scroll region or a value row is a real tab stop.
    fn reaches_in_tab_order(
        cx: &mut gpui_kit::VisualTestContext,
        entry: &FocusHandle,
        target: &FocusHandle,
    ) -> bool {
        cx.update(|window, cx| window.focus(entry, cx));
        for _ in 0..TAB_ORDER_WALK_LIMIT {
            cx.update(|window, cx| window.focus_next(cx));
            if cx.update(|window, _| target.is_focused(window)) {
                return true;
            }
        }
        false
    }

    fn pod_object(name: &str, uid: &str) -> Arc<DynamicObject> {
        Arc::new(
            serde_json::from_value(json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {
                    "name": name,
                    "namespace": "default",
                    "uid": uid,
                    "creationTimestamp": "2026-09-22T00:00:00Z",
                    "labels": { "app": "web", "tier": "frontend" },
                    "ownerReferences": [{
                        "apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "web-rs",
                        "uid": "uid-web-rs", "controller": true,
                    }],
                },
                "spec": { "containers": [{ "name": "app", "image": "web:1" }] },
                "status": {
                    "phase": "Running",
                    "podIP": "10.244.0.5",
                    "conditions": [{
                        "type": "Ready", "status": "True", "reason": "ContainersReady",
                    }],
                    "containerStatuses": [{
                        "name": "app", "ready": true, "restartCount": 2,
                        "state": { "running": {} },
                    }],
                },
            }))
            .expect("pod"),
        )
    }

    /// UID shared by the layout fixture object and the selection that loads it.
    const LAYOUT_UID: &str = "layout-uid";

    /// A scheduler message past the inline limit, so it must wrap under its key.
    const LAYOUT_LONG_MESSAGE: &str = "CRITICAL: readiness dependency has been unavailable for a very long time and must not wrap";
    /// An image digest past the inline limit.
    const LAYOUT_LONG_IMAGE: &str = "registry.example.com/platform/api@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// Wide values for the layout assertions. The UID must match the selection that
    /// asks for it, otherwise the load is rejected as a replaced object.
    fn layout_pod_object(uid: &str) -> Arc<DynamicObject> {
        Arc::new(
            serde_json::from_value(json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {
                    "name": "web-frontend-7d9f8c6b5d4-abcde",
                    "namespace": "production-platform",
                    "uid": uid,
                    "creationTimestamp": "2026-09-22T00:00:00Z",
                    "labels": {
                        "app": "web",
                        "company.example.com/team-platform/extremely-long-label-name": "a very long label value that must remain on one line"
                    }
                },
                "spec": {
                    "containers": [{
                        "name": "api",
                        "image": LAYOUT_LONG_IMAGE
                    }]
                },
                "status": {
                    "phase": "Running",
                    "configHash": "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
                    "conditions": [{
                        "type": "Ready",
                        "status": "CRITICAL",
                        "reason": "DependencyUnavailable",
                        "message": LAYOUT_LONG_MESSAGE
                    }],
                    "containerStatuses": [{
                        "name": "api",
                        "ready": false,
                        "restartCount": 17,
                        "state": { "waiting": { "reason": "CrashLoopBackOff" } }
                    }]
                }
            }))
            .expect("layout pod"),
        )
    }

    fn pod_ref(uid: &str) -> ObjectRef {
        ObjectRef {
            resource: kube_core::ApiResource::from_gvk_with_plural(
                &kube_core::GroupVersionKind::gvk("", "v1", "Pod"),
                "pods",
            ),
            namespace: Some("default".to_owned()),
            name: "web-0".to_owned(),
            uid: uid.to_owned(),
        }
    }

    /// A Pod that names exactly one ConfigMap, so a test can count the resolves.
    fn referencing_pod_ref(uid: &str) -> ObjectRef {
        ObjectRef {
            name: "web".to_owned(),
            ..pod_ref(uid)
        }
    }

    /// A source that counts the `resolve` calls, so the caching claim is measured rather than
    /// assumed. It answers `Unknown` — the answer an offline session gets — so the panel under
    /// test draws the fallback rather than a resolved row.
    struct CountingResolveSource {
        describes: AtomicUsize,
        events: AtomicUsize,
        /// Resolves of a *referenced* object — a ConfigMap the Pod names.
        references: AtomicUsize,
        /// Resolves of the selected object itself, which is the panel's deleted-object check.
        presence: AtomicUsize,
    }

    impl CountingResolveSource {
        fn referencing_pod(name: &str, uid: &str) -> Arc<DynamicObject> {
            Arc::new(
                serde_json::from_value(json!({
                    "apiVersion": "v1",
                    "kind": "Pod",
                    "metadata": { "name": name, "namespace": "default", "uid": uid },
                    "spec": {
                        "volumes": [
                            { "name": "config", "configMap": { "name": "api-config" } },
                        ],
                    },
                }))
                .expect("the fixture is a Pod"),
            )
        }
    }

    impl InspectorSource for CountingResolveSource {
        fn describe(&self, object: &ObjectRef) -> OpsFuture<DescribeData> {
            self.describes.fetch_add(1, Ordering::Relaxed);
            let object = Self::referencing_pod(&object.name, &object.uid);
            Box::pin(async move {
                Ok(DescribeData {
                    object,
                    events: Vec::new(),
                    owners: Vec::new(),
                    controller: None,
                })
            })
        }

        fn events(&self, _object: &ObjectRef) -> OpsFuture<Vec<DynamicObject>> {
            self.events.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(Vec::new()) })
        }

        fn resolve(&self, object: &ObjectRef) -> OpsFuture<ResolveOutcome> {
            // Counted apart, because the two calls answer different questions and a single
            // counter would let one of them hide a regression in the other.
            if object.resource.kind == "Pod" {
                self.presence.fetch_add(1, Ordering::Relaxed);
            } else {
                self.references.fetch_add(1, Ordering::Relaxed);
            }
            Box::pin(async { Ok(ResolveOutcome::Unknown) })
        }
    }

    fn matching_apply_yaml(uid: &str) -> String {
        matching_apply_yaml_for(uid, "web-0")
    }

    /// YAML that identifies the object it claims to describe.
    fn matching_apply_yaml_for(uid: &str, name: &str) -> String {
        format!(
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: {name}\n  namespace: default\n  uid: {uid}\n"
        )
    }

    struct FakeSource {
        describes: AtomicUsize,
        events: AtomicUsize,
        object: Option<Arc<DynamicObject>>,
        event_count: usize,
    }

    impl InspectorSource for FakeSource {
        fn describe(&self, object: &ObjectRef) -> OpsFuture<DescribeData> {
            self.describes.fetch_add(1, Ordering::Relaxed);
            let name = object.name.clone();
            let uid = object.uid.clone();
            let object = self.object.clone();
            Box::pin(async move {
                Ok(DescribeData {
                    object: object.unwrap_or_else(|| pod_object(&name, &uid)),
                    events: Vec::new(),
                    owners: vec![("ReplicaSet".to_owned(), "web-rs".to_owned())],
                    controller: Some(k8s_core::ops::Controller {
                        kind: "ReplicaSet".to_owned(),
                        name: "web-rs".to_owned(),
                        uid: "uid-web-rs".to_owned(),
                    }),
                })
            })
        }

        fn events(&self, _object: &ObjectRef) -> OpsFuture<Vec<DynamicObject>> {
            self.events.fetch_add(1, Ordering::Relaxed);
            let events = (0..self.event_count)
                .map(|index| {
                    serde_json::from_value(json!({
                        "apiVersion": "v1",
                        "kind": "Event",
                        "metadata": { "name": format!("event-{index}") },
                        "type": "Normal",
                        "reason": "Synthetic",
                        "message": format!("Event {index}"),
                        "lastTimestamp": "2026-09-23T09:00:00Z"
                    }))
                    .expect("event")
                })
                .collect();
            Box::pin(async move { Ok(events) })
        }
    }

    /// A source whose events say what a real `FailedScheduling` says: several lines of it.
    struct LongMessageSource;

    const LONG_EVENT_MESSAGE: &str = "0/3 nodes are available: 1 Insufficient memory, 2 node(s) \
        had untolerated taint(s). preemption: 0/3 nodes are available: 3 Preemption is not \
        helpful for scheduling.";

    impl InspectorSource for LongMessageSource {
        fn describe(&self, _object: &ObjectRef) -> OpsFuture<DescribeData> {
            Box::pin(async { Err("unused".to_owned()) })
        }

        fn events(&self, _object: &ObjectRef) -> OpsFuture<Vec<DynamicObject>> {
            let events = ["FailedScheduling", "FailedMount"]
                .into_iter()
                .enumerate()
                .map(|(index, reason)| {
                    serde_json::from_value(json!({
                        "apiVersion": "v1",
                        "kind": "Event",
                        "metadata": { "name": format!("long-{index}") },
                        "type": "Warning",
                        "reason": reason,
                        "message": LONG_EVENT_MESSAGE,
                        "lastTimestamp": "2026-09-23T09:00:00Z",
                    }))
                    .expect("event")
                })
                .collect();
            Box::pin(async move { Ok(events) })
        }
    }

    /// Source that never answers, so a `Loading` entry can be observed.
    struct HangingSource {
        describes: AtomicUsize,
        events: AtomicUsize,
        /// Events fail instead of hanging, because events are best effort.
        events_fail: bool,
    }

    impl HangingSource {
        fn new(events_fail: bool) -> Self {
            Self {
                describes: AtomicUsize::new(0),
                events: AtomicUsize::new(0),
                events_fail,
            }
        }
    }

    impl InspectorSource for HangingSource {
        fn describe(&self, _object: &ObjectRef) -> OpsFuture<DescribeData> {
            self.describes.fetch_add(1, Ordering::Relaxed);
            Box::pin(std::future::pending())
        }

        fn events(&self, _object: &ObjectRef) -> OpsFuture<Vec<DynamicObject>> {
            self.events.fetch_add(1, Ordering::Relaxed);
            if self.events_fail {
                return Box::pin(async { Err("events are forbidden by the cluster".to_owned()) });
            }
            Box::pin(std::future::pending())
        }
    }

    struct EmptyMetricsPort;
    impl ClusterDataPort for EmptyMetricsPort {
        fn overview(&self, _metrics: bool) -> DataFuture<Overview, String> {
            Box::pin(async { Ok(Overview::default()) })
        }

        fn metrics_probe(&self) -> DataFuture<(), MetricsError> {
            Box::pin(async { Ok(()) })
        }

        fn metrics_nodes(&self) -> DataFuture<Vec<NodeMetric>, MetricsError> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn metrics_pods(
            &self,
            _namespace: Option<String>,
        ) -> DataFuture<Vec<PodMetric>, MetricsError> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn namespaces(&self) -> DataFuture<Vec<String>, String> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn cluster_uid(&self) -> DataFuture<String, String> {
            Box::pin(async { Ok(String::new()) })
        }

        fn server_version(&self) -> DataFuture<String, String> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    // The monospace decision follows the value's JSON type, so a column cannot change size
    // halfway down the way a string-matching heuristic made it do.

    // The review compares the two documents locally. A cluster-side dry run would need a request
    // this panel does not own, so the copy says what the comparison is.

    #[gpui_kit::test]
    fn inspector_tabs_begin_at_the_content_edge(cx: &mut TestAppContext) {
        let (_panel, cx) = setup(cx, "name: app");
        cx.run_until_parked();
        let strip = cx.debug_bounds("inspector-tabs").expect("Inspector tabs");
        let first = cx.debug_bounds("inspector-tab-0").expect("YAML tab");
        assert!((f32::from(first.origin.x - strip.origin.x)).abs() <= 8.0);
    }

    /// A fetch that failed and an empty selection are different facts. One is an anomaly with a
    /// severity, a reason, and a way to try again; the other is an instruction. The YAML tab
    /// rendered both as `Select a row to inspect its YAML.`, which during an incident reads as
    /// "you have not clicked anything yet" and sends the reader back to the table.
    #[gpui_kit::test]
    fn a_failed_yaml_fetch_is_not_an_empty_selection(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        cx.run_until_parked();
        panel.update(cx, |panel, cx| {
            panel.set_yaml_error(
                Some(pod_ref("uid-1")),
                "pods is forbidden: User cannot get resource pods".to_owned(),
                cx,
            );
        });
        cx.run_until_parked();

        let error = cx
            .debug_bounds("inspector-load-error")
            .expect("the YAML failure is reported as a failure");
        assert!(f32::from(error.size.height) > 0.0);
        assert!(
            cx.debug_bounds("inspector-retry").is_some(),
            "the failure offers a Retry, not an instruction to select a row"
        );
        assert!(
            cx.debug_bounds("empty-state").is_none(),
            "the empty selection state stays reserved for an empty selection"
        );
        let yaml_error = panel.read_with(cx, |panel, _| panel.yaml_error().map(str::to_owned));
        assert_eq!(
            yaml_error.as_deref(),
            Some("pods is forbidden: User cannot get resource pods")
        );
    }

    /// The two states are exclusive: a document that arrives resolves the last failure, so a stale
    /// reason cannot sit over a document that is on screen.
    #[gpui_kit::test]
    fn a_document_that_arrives_clears_the_previous_failure(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        panel.update(cx, |panel, cx| {
            panel.set_yaml_error(None, "the watch closed".to_owned(), cx);
        });
        assert!(panel.read_with(cx, |panel, _| panel.yaml_error().is_some()));
        panel.update(cx, |panel, cx| {
            panel.set_yaml(Some("name: app".to_owned()), cx)
        });
        assert!(
            panel.read_with(cx, |panel, _| panel.yaml_error().is_none()),
            "a document resolves the failure that preceded it"
        );
    }

    /// Retry has to leave the panel, because the Inspector does not read YAML itself: the table
    /// hands it the text it already read. A Retry that only cleared the error would put the reader
    /// back where they started, which is the failure this state exists to fix.
    #[gpui_kit::test]
    fn retry_asks_for_the_document_again(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        let asked = Rc::new(RefCell::new(0usize));
        let sink = Rc::clone(&asked);
        panel.update(cx, |panel, _| {
            panel.set_yaml_reload(move |_cx| {
                *sink.borrow_mut() += 1;
            });
        });
        panel.update(cx, |panel, cx| {
            panel.set_yaml_error(None, "the watch closed".to_owned(), cx);
        });
        cx.run_until_parked();
        let retry = cx
            .debug_bounds("inspector-retry")
            .expect("the failure offers a Retry");
        cx.simulate_click(retry.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(asked.borrow().clone(), 1, "Retry re-requests the document");
        assert!(
            panel.read_with(cx, |panel, _| panel.yaml_error().is_none()),
            "the error clears either way, so Retry is never a no-op"
        );
    }

    /// The Reload control on the YAML toolbar used to do nothing: `reload_active_tab` had no arm
    /// for tab 0, because there was nothing to report.
    #[gpui_kit::test]
    fn reloading_the_yaml_tab_asks_for_the_document(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        let asked = Rc::new(RefCell::new(0usize));
        let sink = Rc::clone(&asked);
        panel.update(cx, |panel, cx| {
            panel.set_yaml_reload(move |_cx| {
                *sink.borrow_mut() += 1;
            });
            panel.set_yaml_error(None, "the watch closed".to_owned(), cx);
        });
        panel.update(cx, |panel, cx| panel.reload_active_tab(cx));
        assert_eq!(asked.borrow().clone(), 1);
    }

    /// The YAML band names its document, not the object. At the 336px default width the old
    /// `YAML · {kind} {name} in namespace {ns}` title kept about 27 of its 53 characters, and the
    /// identity bar forty pixels above already said the same thing at full contrast.
    #[gpui_kit::test]
    fn the_yaml_band_does_not_repeat_the_object_identity(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        // A source that can apply, because the band under test is the clean one: with no handler
        // the toolbar spends its left half on the "Apply unavailable" status instead.
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(|_request, _| {});
        });
        select(&panel, "uid-1", cx);
        cx.simulate_resize(gpui_kit::size(px(336.), px(640.)));
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("yaml-clean-metadata").is_some(),
            "the clean band still shows the document glyph"
        );
        assert!(
            cx.debug_bounds("inspector-identity").is_some(),
            "the identity bar is where the object is named"
        );
        let band = cx
            .debug_bounds("yaml-clean-metadata")
            .expect("the YAML band");
        // The band's own width cannot carry this: its slot is a block box, so the band is as wide
        // as the slot whether it holds one glyph or a clipped sentence. Its height can, because
        // every line of text the app prints is at least `metadata` tall and the glyph is not.
        assert!(
            f32::from(band.size.height) < f32::from(design::text::CAPTION_LINE_HEIGHT),
            "the band holds the document glyph, not a line of the object identity: {band:?}"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.yaml_error().map(str::to_owned)),
            None,
            "a clean load has no failure to report"
        );
    }

    #[gpui_kit::test]
    fn narrow_inspector_bounds_reveal_roving_focus(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        cx.simulate_resize(gpui_kit::size(px(120.), px(640.)));
        cx.run_until_parked();

        let focus = panel.read_with(cx, |panel, _| panel.tab_focus.clone());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_keystrokes("right");
        cx.run_until_parked();

        assert_eq!(panel.read_with(cx, |panel, _| panel.focused_tab), 1);
        let viewport = panel.read_with(cx, |panel, _| panel.tabs_scroll.bounds());
        let describe = cx.debug_bounds("inspector-tab-1").expect("Describe tab");
        assert!(describe.left() >= viewport.left());
        assert!(describe.right() <= viewport.right());

        cx.simulate_keystrokes("right");
        cx.run_until_parked();

        assert_eq!(panel.read_with(cx, |panel, _| panel.focused_tab), 2);
        let viewport = panel.read_with(cx, |panel, _| panel.tabs_scroll.bounds());
        let events = cx.debug_bounds("inspector-tab-2").expect("Events tab");
        assert!(events.left() >= viewport.left());
        assert!(events.right() <= viewport.right());
    }

    #[gpui_kit::test]
    fn tab_arrows_roving_focus_and_enter_activates(cx: &mut TestAppContext) {
        // `source_setup` rather than `setup`, because the walk starts from whichever tab the
        // panel ships on and that is the thing under test here.
        let (panel, _source, cx) = source_setup(cx);
        let focus = panel.read_with(cx, |panel, _| panel.tab_focus.clone());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_keystrokes("right");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.active_tab(), InspectorTab::Describe);
            assert_eq!(panel.focused_tab, 2);
        });
        assert!(cx.update(|window, _| focus.is_focused(window)));
        cx.simulate_keystrokes("enter");
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.active_tab()),
            InspectorTab::Events
        );
    }

    /// The regression this default is: selecting an object used to open its document, so the
    /// first thing on the panel was line 221 of a 400-line manifest and the reason the object is
    /// in trouble was one tab away. `UI-REDESIGN.md` §3.4 (Status first) and D27 / `UI-SPEC.md`
    /// §13.1 (the document is the fallback, not the entrance) are what this asserts.
    /// A Related row that says `NotFound` is usually the root cause, so the verdict has to be a
    /// fact about the cluster rather than about a controller's choice of words. These are the
    /// three ways the old sentence matcher got that wrong, and each one is silent.
    #[test]
    fn a_missing_reference_is_named_by_the_object_and_confirmed_by_name() {
        let object: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": "web", "namespace": "team-a" },
            "spec": {
                "volumes": [
                    { "name": "config", "configMap": { "name": "api-config" } },
                    { "name": "creds", "secret": { "secretName": "db-creds" } },
                    { "name": "tuning", "configMap": { "name": "tuning", "optional": true } },
                ],
                "containers": [{
                    "name": "app",
                    "envFrom": [{ "configMapRef": { "name": "api-config" } }],
                }],
            },
        }))
        .expect("the fixture is a Pod");
        let warning = |reason: &str, message: &str| {
            let event: DynamicObject = serde_json::from_value(json!({
                "type": "Warning",
                "reason": reason,
                "message": message,
            }))
            .expect("the fixture is an Event");
            LoadState::Ready(Arc::new(vec![event]))
        };

        // The wording is the cluster's, not the panel's: `FailedMount` says the same thing as
        // `not found`, and the row cannot depend on which one arrived.
        assert_eq!(
            missing_references(
                &object,
                Some(&warning(
                    "FailedMount",
                    "Unable to attach or mount volumes: configmap \"api-config\" not found"
                ))
            ),
            vec![MissingReference {
                kind: "ConfigMap".to_owned(),
                name: "api-config".to_owned(),
            }],
        );

        // A name that is only a substring of another name is a different object: a configmap
        // called `api` is not the `api` inside `api-gateway`, and `config` is not the `config`
        // inside `configmap`. Both boundaries have to hold, or the panel invents a root cause.
        for message in [
            "configmap \"my-api-config-backup\" not found",
            "configmap \"not-api-config\" not found",
        ] {
            assert!(
                missing_references(&object, Some(&warning("FailedMount", message))).is_empty(),
                "a substring is not a reference: {message}"
            );
        }

        // An optional reference that is absent is a choice, not a fault, and a panel that paints
        // it `danger` teaches the reader to stop believing the row.
        assert!(
            missing_references(
                &object,
                Some(&warning("FailedMount", "configmap \"tuning\" not found"))
            )
            .is_empty(),
            "an optional reference is not a broken one"
        );

        // Nothing the cluster named means nothing is claimed. The old matcher read the words
        // "failed to pull image" and invented an `Image` relationship out of a log line, which has
        // no uid, no namespace, and nothing to open.
        assert!(
            missing_references(
                &object,
                Some(&warning(
                    "Failed",
                    "Failed to pull image \"registry.example.com/api:1\""
                ))
            )
            .is_empty(),
            "an image reference is not an object relationship"
        );
    }

    /// `UI-REDESIGN.md` L3 makes the Related block the way a dead end becomes a chain, so what
    /// `InspectorSource::resolve` answers has to decide three visibly different things — and the
    /// third is the one whose failure mode is a whole panel of red.
    ///
    /// `Missing` is the only answer that may paint `danger`, and the only one that may not draw an
    /// arrow: there is nothing on the other end of a link to an object the cluster has denied.
    /// `Found { uid }` is a live hop — and the uid is the whole point, because a name alone can be
    /// freed and taken by a different object. `Unknown` changes nothing at all, which is what an
    /// offline session and a source that never implemented the query both produce, and what a
    /// two-valued answer would have had to call `Missing`.
    #[test]
    fn a_reference_is_drawn_three_ways_and_an_unknown_one_keeps_the_existing_appearance() {
        let object: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": "web", "namespace": "team-a" },
            "spec": {
                "volumes": [
                    { "name": "gone", "configMap": { "name": "absent-config" } },
                    { "name": "live", "configMap": { "name": "present-config" } },
                    { "name": "quiet", "configMap": { "name": "unresolved-config" } },
                ],
            },
        }))
        .expect("the fixture is a Pod");

        let verdicts: ReferenceVerdicts = HashMap::from([
            (
                ("ConfigMap".to_owned(), "absent-config".to_owned()),
                ResolveOutcome::Missing,
            ),
            (
                ("ConfigMap".to_owned(), "present-config".to_owned()),
                ResolveOutcome::Found {
                    uid: "uid-present".to_owned(),
                },
            ),
            (
                ("ConfigMap".to_owned(), "unresolved-config".to_owned()),
                ResolveOutcome::Unknown,
            ),
        ]);

        let rows = reference_rows(&object, None, &verdicts, Some("team-a"));
        let row = |name: &str| {
            rows.iter()
                .find(|row| row.name == name)
                .unwrap_or_else(|| panic!("{name} is one of the three references"))
        };

        // Missing: danger, and no way out.
        assert_eq!(row("absent-config").health, RelatedHealth::NotFound);
        assert_eq!(
            row("absent-config").follow,
            None,
            "a link to an object the cluster has denied is a dead control"
        );

        // Found: the live hop, carrying the uid that makes following it safe.
        assert_eq!(row("present-config").health, RelatedHealth::Ok);
        let follow = row("present-config")
            .follow
            .as_ref()
            .expect("a confirmed ConfigMap is somewhere to go");
        assert_eq!(follow.uid, "uid-present");
        assert_eq!(follow.namespace.as_deref(), Some("team-a"));
        assert!(
            row("present-config").health != row("absent-config").health,
            "`§14 L3`: the failure and the healthy row must not wear the same ink, or the reader \
             cannot tell which one is the problem"
        );

        // Unknown: unchanged. With no Warning naming it, the row is not drawn at all — which is
        // what the panel did before `resolve` existed, and what an offline session still does.
        assert!(
            !rows.iter().any(|row| row.name == "unresolved-config"),
            "an unresolved reference must add nothing: no row, so no danger, and no arrow. \
             Mapping `Unknown` to `Missing` instead turns every reference in the panel red the \
             moment the connection drops: {rows:?}"
        );

        // Unknown with a Warning naming it falls back to the event evidence rather than going
        // quiet, so the one case the panel has always reported still gets reported.
        let event: DynamicObject = serde_json::from_value(json!({
            "type": "Warning",
            "reason": "FailedMount",
            "message": "configmap \"unresolved-config\" not found",
        }))
        .expect("the fixture is an Event");
        let events = LoadState::Ready(Arc::new(vec![event]));
        let rows = reference_rows(&object, Some(&events), &verdicts, Some("team-a"));
        let unresolved = rows
            .iter()
            .find(|row| row.name == "unresolved-config")
            .expect("the event evidence is the fallback for an unresolved reference");
        assert_eq!(unresolved.health, RelatedHealth::NotFound);
        assert_eq!(unresolved.follow, None);
    }

    /// The Related block must not ask the same question twice.
    ///
    /// A `GET` per reference per render would be the most expensive thing in the panel, and the
    /// render happens on every scroll, hover and value expansion. The cache is keyed by the
    /// selection's `cache_key`, so it also has to stop answering for an object the reader has left.
    #[gpui_kit::test]
    fn each_reference_is_resolved_once_per_selection(cx: &mut TestAppContext) {
        init_app(cx);
        let (panel, cx) = cx.add_window_view(|_, cx| InspectorPanel::new(cx));
        let source = Arc::new(CountingResolveSource {
            describes: AtomicUsize::new(0),
            events: AtomicUsize::new(0),
            references: AtomicUsize::new(0),
            presence: AtomicUsize::new(0),
        });
        panel.update(cx, |panel, _| {
            panel.set_source(source.clone() as Arc<dyn InspectorSource>);
        });
        panel.update(cx, |panel, cx| {
            panel.set_selection(
                Some(InspectorSelection {
                    object: referencing_pod_ref("uid-1"),
                    yaml: "kind: Pod".to_owned(),
                }),
                cx,
            );
        });
        cx.run_until_parked();
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        // Rendering again is what would re-ask if the cache were keyed per render.
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert_eq!(
            source.references.load(Ordering::Relaxed),
            1,
            "one ConfigMap, one `GET` — a second render must not ask again"
        );

        // A different object is a different question, and the previous answer must not be reused.
        panel.update(cx, |panel, cx| {
            let mut object = referencing_pod_ref("uid-2");
            object.name = "web-2".to_owned();
            panel.set_selection(
                Some(InspectorSelection {
                    object,
                    yaml: "kind: Pod".to_owned(),
                }),
                cx,
            );
        });
        cx.run_until_parked();
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        assert_eq!(
            source.references.load(Ordering::Relaxed),
            2,
            "a new object is a new reference to resolve"
        );
        panel.update(cx, |panel, _| {
            assert_eq!(
                panel.reference_verdicts.len(),
                2,
                "both objects keep their own verdicts, so going back is free"
            );
        });
    }

    /// Nothing in the panel runs on a timer while the reader just sits on a healthy object, which
    /// is why the deleted-object check needed a driver of its own.
    ///
    /// Without one the banner worked exactly once -- on the read that followed a tab switch or a
    /// Reload -- and then never again, so a reader who deleted a Pod out of the band and kept
    /// reading was left looking at a confident, complete, wrong copy. That is the R3 failure this
    /// whole mechanism exists to prevent, and it was reintroducible by deleting a `load_selection`
    /// call. So the cadence is pinned here: idle time alone has to produce the second question.
    #[gpui_kit::test]
    fn a_deleted_object_is_caught_while_the_reader_does_nothing(cx: &mut TestAppContext) {
        init_app(cx);
        let (panel, cx) = cx.add_window_view(|_, cx| InspectorPanel::new(cx));
        let source = Arc::new(CountingResolveSource {
            describes: AtomicUsize::new(0),
            events: AtomicUsize::new(0),
            references: AtomicUsize::new(0),
            presence: AtomicUsize::new(0),
        });
        panel.update(cx, |panel, _| {
            panel.set_source(source.clone() as Arc<dyn InspectorSource>);
        });
        panel.update(cx, |panel, cx| {
            panel.set_selection(
                Some(InspectorSelection {
                    object: referencing_pod_ref("uid-1"),
                    yaml: "kind: Pod".to_owned(),
                }),
                cx,
            );
        });
        cx.run_until_parked();
        let asked_once = source.presence.load(Ordering::Relaxed);
        assert_eq!(asked_once, 1, "the selection is checked once up front");

        // Idle. No selection change, no tab switch, no Reload, no render.
        // A hair past the deadline, because a timer armed for `t` is due *after* `t`.
        cx.executor()
            .advance_clock(PRESENCE_TTL + std::time::Duration::from_millis(1));
        cx.run_until_parked();
        assert_eq!(
            source.presence.load(Ordering::Relaxed),
            asked_once + 1,
            "an idle reader is re-checked on the cadence"
        );

        // And the cadence is a cadence, not a poll that never stops.
        cx.executor()
            .advance_clock(PRESENCE_TTL * 3 + std::time::Duration::from_millis(1));
        cx.run_until_parked();
        assert_eq!(
            source.presence.load(Ordering::Relaxed),
            asked_once + 4,
            "one question per interval, and no faster"
        );

        // Leaving the object stops the questions; otherwise a closed tab would keep polling the
        // cluster forever.
        panel.update(cx, |panel, cx| panel.set_selection(None, cx));
        cx.run_until_parked();
        cx.executor()
            .advance_clock(PRESENCE_TTL * 3 + std::time::Duration::from_millis(1));
        cx.run_until_parked();
        assert_eq!(
            source.presence.load(Ordering::Relaxed),
            asked_once + 4,
            "no selection, nothing to ask about"
        );
    }

    /// The status of a selection cannot be inferred from the table it came from, so the panel has
    /// to be told — and the copy has to say which object it is talking about, because a reader who
    /// just deleted one Pod out of forty needs to know it was that one.
    #[test]
    fn a_deleted_object_banner_says_which_object_it_is_about() {
        let reason = object_gone_reason("Pod", "web-7d2f");
        assert!(
            reason.contains("web-7d2f"),
            "the banner names the object: {reason}"
        );
        assert!(reason.contains("Pod"), "and its kind: {reason}");
        assert!(
            !reason.to_ascii_lowercase().contains("not found"),
            "`§4.15`: the state is stated, not the API server's HTTP sentence — {reason}"
        );
        // The namespace is one line above the strip and is deliberately not repeated: inside a
        // 420px panel a 47-character namespace is what turned this into a five-line paragraph.
        assert!(
            !reason.contains("namespace"),
            "the identity line above already states it — {reason}"
        );
        // One short line of headline, and the staleness caveat as its own caption, is the whole
        // strip. A strip that grows a third sentence is a strip nobody reads.
        assert_eq!(
            reason.matches('.').count(),
            1,
            "the headline is one sentence; the caveat is separate: {reason}"
        );
        assert!(
            GONE_STALE_NOTE.contains("last known state"),
            "and it has to admit the values are stale: {GONE_STALE_NOTE}"
        );
        // A cluster-scoped object is named the same way; there is no dangling "in namespace ".
        assert!(object_gone_reason("Node", "k8s-gpui-dev").contains("k8s-gpui-dev"));
    }

    /// `PROMPT.md` §6.7 R3: a `kubectl delete pod` makes the row vanish from the table and used to
    /// leave the Inspector holding a complete, confident, wrong copy of it — no banner, until the
    /// reader pressed Reload. The panel now asks the cluster, and the three answers have to read
    /// three different ways: only a confirmed absence may raise the strip.
    #[gpui_kit::test]
    fn only_a_confirmed_absence_raises_the_deleted_object_banner(cx: &mut TestAppContext) {
        for (verdict, expect_banner) in [
            (ResolveOutcome::Missing, true),
            (
                ResolveOutcome::Found {
                    uid: "uid-1".to_owned(),
                },
                false,
            ),
            (ResolveOutcome::Unknown, false),
        ] {
            init_app(cx);
            let (panel, cx) = cx.add_window_view(|_, cx| InspectorPanel::new(cx));
            panel.update(cx, |panel, _| {
                panel.set_source(Arc::new(VerdictSource {
                    verdict: verdict.clone(),
                }) as Arc<dyn InspectorSource>);
            });
            select(&panel, "uid-1", cx);
            panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
            cx.run_until_parked();
            cx.simulate_resize(gpui_kit::size(px(352.), px(640.)));
            cx.run_until_parked();
            assert_eq!(
                cx.debug_bounds("inspector-object-gone").is_some(),
                expect_banner,
                "{verdict:?} must {} raise the deleted-object banner",
                if expect_banner { "" } else { "not" }
            );
            panel.update(cx, |_, _| {});
        }
    }

    /// The Inspector showed the anchor of a multi-row selection and said nothing about it, so a
    /// reader with eight rows selected got three answers from three surfaces: the table counted
    /// eight, the actions that hit one object refused by name, and this panel read as though it
    /// were showing all of them. The banner is this panel's half of that agreement, and the only
    /// half — with one row selected the header already describes everything on screen, and it has
    /// to stay silent there or the reader pays for the caveat on every object they look at.
    #[gpui_kit::test]
    fn a_multi_row_selection_says_which_one_the_panel_is_showing(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        cx.simulate_resize(gpui_kit::size(px(336.), px(640.)));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("inspector-selection-banner").is_none(),
            "one selected row is what the panel is for, so the header says it already"
        );

        panel.update(cx, |panel, cx| panel.set_selected_rows(8, cx));
        cx.run_until_parked();
        let banner = cx
            .debug_bounds("inspector-selection-banner")
            .expect("a multi-row selection names the count and the one object on screen");
        let name = cx
            .debug_bounds("inspector-identity-name")
            .expect("the object's name");
        assert!(
            f32::from(banner.origin.y) < f32::from(name.origin.y),
            "the banner qualifies the name, so it is read before the name rather than after"
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.selection_banner(),
                Some(("8 rows selected".to_owned(), "web-0".to_owned())),
                "the banner names the count and the object on screen, so the reader can tell \
                 which of the eight this panel is about"
            );
        });

        panel.update(cx, |panel, cx| panel.set_selected_rows(1, cx));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("inspector-selection-banner").is_none(),
            "narrowing the selection back to one row takes the banner away with it"
        );

        // A followed object is not a row in that selection, and coming back has to carry the
        // count with the object rather than quietly forget it.
        panel.update(cx, |panel, cx| panel.set_selected_rows(8, cx));
        panel.update(cx, |panel, cx| {
            let target = followable("ReplicaSet", "web-rs", Some("default"), "uid-web-rs")
                .expect("a ReplicaSet in the table names its resource");
            panel.follow_related(target, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("inspector-selection-banner").is_none(),
            "the count is about the table's selection, and a followed object is not in it"
        );
        panel.update(cx, |panel, cx| panel.go_back(cx));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("inspector-selection-banner").is_some(),
            "going back to the selected row brings its count back with it"
        );
    }

    /// §2.4 / `§4.13`: the panel used to say the same thing on two faces — `No resource selected` in
    /// the identity band and `Select a row to see its fields` in the Describe body — at the same
    /// 15/600, so the reader could not tell which was the panel's title. The content region owns the
    /// sentence; the band keeps its geometry and its reserved rail so the empty state lands on the
    /// same edge as the content does.
    ///
    /// gpui-kit's test harness reads geometry and accessibility, not rendered text, so the
    /// sentence's *absence* from the band is enforced by the structure of
    /// [`InspectorPanel::render_identity`] rather than by an assertion here. What this pins down is
    /// the part a careless re-add would break: the rail and the height. An empty band that dropped
    /// its `border_l_2` would put every empty-state icon 2px right of the section headings under
    /// it, and an empty band shorter than the loaded one moved the tab strip, the three toolbars
    /// and the body 18px up the panel every time a selection was cleared.
    #[gpui_kit::test]
    fn nothing_selected_is_said_once_in_the_content_region_and_the_rail_still_lines_up(
        cx: &mut TestAppContext,
    ) {
        let mut empty_edges: Vec<(f32, f32)> = Vec::new();
        let mut loaded_edges: Vec<(f32, f32)> = Vec::new();
        let mut empty_band: Option<(f32, f32)> = None;
        let mut loaded_band: Option<(f32, f32)> = None;
        for selected in [false, true] {
            let (panel, cx) = setup(cx, "name: app");
            cx.simulate_resize(gpui_kit::size(px(352.), px(640.)));
            if selected {
                select(&panel, "uid-1", cx);
            }
            panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
            cx.run_until_parked();
            let band = cx
                .debug_bounds("inspector-identity")
                .expect("the band is still there — it is what holds the tab strip in place");
            let tabs = cx.debug_bounds("inspector-tabs").expect("the tab strip");
            let edges = (
                f32::from(band.origin.x) + f32::from(band.size.width),
                f32::from(tabs.origin.x) + f32::from(tabs.size.width),
            );
            let geometry = (f32::from(band.origin.y), f32::from(band.size.height));
            if selected {
                loaded_edges.push(edges);
                loaded_band = Some(geometry);
            } else {
                empty_edges.push(edges);
                empty_band = Some(geometry);
                assert!(
                    cx.debug_bounds("inspector-empty").is_some(),
                    "the content region carries the empty state, with its icon and one line"
                );
            }
            panel.update(cx, |_, _| {});
        }
        assert_eq!(
            empty_edges[0].1 - empty_edges[0].0,
            loaded_edges[0].1 - loaded_edges[0].0,
            "the empty band reserves the same focus rail the loaded one does: without it the \
             empty state's icon sits 2px right of the section headings underneath"
        );
        let (empty, loaded) = (
            empty_band.expect("the empty band"),
            loaded_band.expect("the loaded band"),
        );
        // A FLOOR, not an equality.
        //
        // The band reserves `identity_band_height()` so an EMPTY panel is exactly as
        // tall as a loaded one and clearing a selection moves nothing. A loaded band
        // is allowed to be TALLER - the multi-row selection banner and the wired
        // header actions are real states that need the room, and clipping them to
        // hold an equality would be trading a bug for a worse one.
        //
        // What is guaranteed, and what this pins, is that the band never shrinks
        // below the floor: the empty state sits exactly on it, and a loaded band is
        // never under it.
        assert!(
            empty.1 >= loaded.0,
            "the empty band reserves the height the loaded one occupies, so the band below it \
             does not move: empty ends at {:?} but the loaded band's content starts at {:?}",
            empty.1,
            loaded.0,
        );
        assert!(
            loaded.1 >= empty.1,
            "a loaded band is at least the reserved floor: loaded ends at {:?}, the floor is {:?}",
            loaded.1,
            empty.1,
        );
        assert_eq!(
            empty.0, loaded.0,
            "the band is the top band of the panel, so its top edge is the panel's top edge in \
             both states"
        );
    }

    /// A source whose `resolve` answers one scripted verdict, so the three states can be rendered
    /// side by side rather than reasoned about.
    struct VerdictSource {
        verdict: ResolveOutcome,
    }

    impl InspectorSource for VerdictSource {
        fn describe(&self, object: &ObjectRef) -> OpsFuture<DescribeData> {
            let name = object.name.clone();
            let uid = object.uid.clone();
            Box::pin(async move {
                Ok(DescribeData {
                    object: pod_object(&name, &uid),
                    events: Vec::new(),
                    owners: Vec::new(),
                    controller: None,
                })
            })
        }

        fn events(&self, _object: &ObjectRef) -> OpsFuture<Vec<DynamicObject>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn resolve(&self, _object: &ObjectRef) -> OpsFuture<ResolveOutcome> {
            let verdict = self.verdict.clone();
            Box::pin(async move { Ok(verdict) })
        }
    }

    /// A virtualised list gives every row the height of one row it measured at `MaxContent`
    /// width, so a message that wraps on screen is one line tall when measured and the two rows
    /// print on top of each other. Nothing caught it: the panel had no test with a message long
    /// enough to wrap, and a screenshot of a healthy object has no events at all.
    #[gpui_kit::test]
    fn a_wrapped_event_message_does_not_print_over_the_next_event(cx: &mut TestAppContext) {
        assert!(
            LONG_EVENT_MESSAGE.chars().count() > EVENT_MESSAGE_LINES * 40,
            "the fixture has to be long enough to wrap more than the clamp allows, or this test \
             stops covering the case it exists for"
        );
        init_app(cx);
        let (panel, cx) = cx.add_window_view(|_, cx| InspectorPanel::new(cx));
        panel.update(cx, |panel, _| {
            panel.set_source(Arc::new(LongMessageSource) as Arc<dyn InspectorSource>);
        });
        cx.simulate_resize(gpui_kit::size(gpui_kit::px(336.), gpui_kit::px(640.)));
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Events, cx));
        cx.run_until_parked();

        // The row's element id is derived from the event rather than from its position, so the
        // test asks the panel which events arrived instead of guessing at a name.
        let identities: Vec<u64> = panel.read_with(cx, |panel, _| {
            match panel.events_states.get("uid-1").map(|entry| &entry.state) {
                Some(LoadState::Ready(events)) => events.iter().map(event_identity).collect(),
                _ => Vec::new(),
            }
        });
        assert_eq!(identities.len(), 2, "both events arrived: {identities:?}");
        let rows: Vec<_> = identities
            .iter()
            .filter_map(|id| cx.debug_bounds(format!("event-timeline-{id}").leak() as &str))
            .collect();
        assert_eq!(rows.len(), 2, "both events are on screen: {rows:?}");
        assert!(
            rows[1].origin.y >= rows[0].bottom(),
            "the second event starts below the first: {:?} then {:?}",
            rows[0],
            rows[1]
        );
        for row in &rows {
            assert_eq!(
                f32::from(row.size.height),
                f32::from(
                    text::LABEL_LINE_HEIGHT * EVENT_MESSAGE_LINES as f32
                        + text::LABEL_LINE_HEIGHT
                        + space::XXS
                ),
                "the row states the height the list measures: {row:?}"
            );
        }
    }

    #[gpui_kit::test]
    fn selecting_an_object_lands_on_the_summary_not_the_document(cx: &mut TestAppContext) {
        let (panel, _source, cx) = source_setup(cx);
        select(&panel, "uid-1", cx);
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.active_tab()),
            InspectorTab::Describe
        );
    }

    #[gpui_kit::test]
    fn yaml_tab_is_editable_by_default(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        assert!(panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
        cx.simulate_input("x");
        assert_eq!(yaml_text(&panel, cx), "xname: app");
        assert!(dirty(&panel, cx));
    }

    #[gpui_kit::test]
    fn yaml_toolbar_is_fixed_and_cancel_replaces_edit(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        cx.run_until_parked();
        assert_toolbar_actions(
            cx,
            "yaml-action-toolbar",
            &["yaml-action-apply", "yaml-action-copy"],
        );
        assert!(cx.debug_bounds("yaml-action-cancel").is_none());
        assert!(cx.debug_bounds("yaml-action-edit").is_none());

        cx.simulate_input("x");
        cx.run_until_parked();
        assert!(cx.debug_bounds("yaml-action-cancel").is_some());
        assert!(panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
    }

    #[gpui_kit::test]
    fn inspector_context_toolbars_are_fixed_and_actions_stay_inside(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let metrics_runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("metrics runtime");
        let metrics = MetricsHandle::new(
            metrics_runtime.handle().clone(),
            Arc::new(EmptyMetricsPort) as Arc<dyn ClusterDataPort>,
        );
        panel.update(cx, |panel, cx| {
            panel.set_metrics_source(Some(metrics), MetricsProbeState::Missing, cx);
        });

        let cases: &[(InspectorTab, &'static str, &'static [&'static str])] = &[
            (
                InspectorTab::Describe,
                "inspector-context-toolbar",
                &["inspector-action-reload"],
            ),
            (
                InspectorTab::Events,
                "inspector-context-toolbar",
                &["inspector-action-reload"],
            ),
            (
                InspectorTab::Metrics,
                "metrics-context-toolbar",
                // `UI-SPEC` §16.5 fixes the six ranges, and the control claims to
                // offer all of them. The assertion is that every one of them is
                // inside the toolbar, so it has to name all six.
                &[
                    "metrics-range-action-1m",
                    "metrics-range-action-15m",
                    "metrics-range-action-1h",
                    "metrics-range-action-6h",
                    "metrics-range-action-24h",
                    "metrics-range-action-7d",
                ],
            ),
        ];
        for &(tab, toolbar_selector, action_selectors) in cases {
            panel.update(cx, |panel, cx| panel.show_tab(tab, cx));
            cx.run_until_parked();
            assert_toolbar_actions(cx, toolbar_selector, action_selectors);
            let tabs = cx.debug_bounds("inspector-tabs").expect("Inspector tabs");
            assert_eq!(
                f32::from(tabs.size.height),
                f32::from(design::size::TAB_BAR)
            );
        }
    }

    #[gpui_kit::test]
    fn apply_reports_invalid_yaml_and_keeps_dirty(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("{oops");
        panel.update(cx, |panel, cx| panel.apply(cx));
        assert!(panel.read_with(cx, |panel, _| panel.validation_error.is_some()));
        assert!(panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
        assert!(dirty(&panel, cx));
    }

    fn inline_diagnostics(
        panel: &Entity<InspectorPanel>,
        cx: &mut gpui_kit::VisualTestContext,
    ) -> Vec<Diagnostic> {
        cx.update(|_, cx| panel.read(cx).yaml_view.read(cx).diagnostics().to_vec())
    }

    #[gpui_kit::test]
    fn invalid_apply_sets_inline_diagnostics_and_edit_or_success_clears(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, _| panel.set_on_apply(|_| {}));
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("@oops");
        panel.update(cx, |panel, cx| panel.apply(cx));
        let diagnostics = inline_diagnostics(&panel, cx);
        assert_eq!(
            diagnostics.len(),
            1,
            "Validation failure must show an inline error"
        );
        assert_eq!(
            diagnostics[0].line, 0,
            "The @ symbol is on line 1: {diagnostics:?}"
        );
        assert!(
            diagnostics[0].message.contains("Line 1, column 1"),
            "{diagnostics:?}"
        );

        cx.simulate_input("x");
        assert!(
            inline_diagnostics(&panel, cx).is_empty(),
            "Editing clears diagnostics"
        );

        cx.simulate_keystrokes("ctrl-a");
        let yaml = matching_apply_yaml("uid-1");
        cx.simulate_input(&yaml);
        panel.update(cx, |panel, cx| panel.apply(cx));
        assert!(
            inline_diagnostics(&panel, cx).is_empty(),
            "Apply clears diagnostics"
        );
    }

    #[gpui_kit::test]
    fn apply_calls_callback_and_marks_saved(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let applied: Rc<RefCell<Option<ApplyRequest>>> = Rc::new(RefCell::new(None));
        let sink = applied.clone();
        panel.update(cx, |panel, _| {
            panel.set_on_apply(move |request: ApplyRequest| {
                *sink.borrow_mut() = Some(request);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        let yaml = matching_apply_yaml("uid-1");
        cx.simulate_input(&yaml);
        panel.update(cx, |panel, cx| panel.apply(cx));
        assert!(
            applied.borrow().is_none(),
            "Apply must not write before the review is confirmed"
        );
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        assert_eq!(
            applied
                .borrow()
                .as_ref()
                .map(|request| request.yaml.as_str()),
            Some(yaml.as_str())
        );
        assert_eq!(
            applied
                .borrow()
                .as_ref()
                .map(|request| request.target.uid.as_str()),
            Some("uid-1")
        );
        assert!(!dirty(&panel, cx));
        assert!(panel.read_with(cx, |panel, _| panel.validation_error.is_none()));
        assert!(panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
    }

    #[gpui_kit::test]
    fn clean_apply_does_not_call_a_write_handler(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let calls = Rc::new(AtomicUsize::new(0));
        let sink = calls.clone();
        panel.update(cx, |panel, _| {
            panel.set_on_apply(move |_request| {
                sink.fetch_add(1, Ordering::Relaxed);
            });
        });
        panel.update(cx, |panel, cx| panel.apply(cx));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(!dirty(&panel, cx));
        assert!(panel.read_with(cx, |panel, _| panel.applied_at.is_none()));
    }

    #[gpui_kit::test]
    fn missing_apply_handler_never_marks_yaml_saved(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        cx.simulate_keystrokes("ctrl-a");
        let yaml = matching_apply_yaml("uid-1");
        cx.simulate_input(&yaml);
        panel.update(cx, |panel, cx| panel.apply(cx));
        assert!(dirty(&panel, cx));
        panel.read_with(cx, |panel, _| {
            assert!(!panel.applying);
            assert_eq!(panel.apply_error.as_deref(), Some(APPLY_UNAVAILABLE_REASON));
            assert!(panel.applied_at.is_none());
        });
    }

    #[gpui_kit::test]
    fn editing_clears_stale_apply_feedback(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        panel.update(cx, |panel, _| {
            panel.validation_error = Some("stale validation".to_owned());
            panel.apply_error = Some("stale apply".to_owned());
            panel.conflict_owners = Some(vec!["kubectl".to_owned()]);
            panel.applied_at = Some(Instant::now());
        });
        cx.simulate_input("x");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(panel.validation_error.is_none());
            assert!(panel.apply_error.is_none());
            assert!(panel.conflict_owners.is_none());
            assert!(panel.applied_at.is_none());
        });
    }

    #[gpui_kit::test]
    fn async_apply_waits_for_result_and_keeps_content_on_failure(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let requests: Rc<RefCell<Vec<ApplyRequest>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = requests.clone();
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(move |request: ApplyRequest, _| {
                sink.borrow_mut().push(request);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        let yaml = matching_apply_yaml("uid-1");
        cx.simulate_input(&yaml);
        panel.update(cx, |panel, cx| panel.apply(cx));
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        let request = requests.borrow()[0].clone();
        assert_eq!(request.yaml, yaml.as_str());
        assert_eq!(request.target.uid, "uid-1");
        assert!(panel.read_with(cx, |panel, _| panel.is_applying()));
        assert!(!panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
        cx.simulate_input("x");
        assert_eq!(yaml_text(&panel, cx), yaml.as_str());
        assert!(
            dirty(&panel, cx),
            "Keep the YAML dirty until the result arrives"
        );
        cx.run_until_parked();
        assert!(cx.debug_bounds("yaml-action-copy").is_some());

        panel.update(cx, |panel, cx| {
            panel.apply_finished_for(
                request.clone(),
                Err("Kubernetes request failed".to_owned()),
                cx,
            );
        });
        assert!(panel.read_with(cx, |panel, _| panel.apply_error.is_some()));
        assert!(panel.read_with(cx, |panel, _| !panel.applying));
        assert!(panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
        assert!(dirty(&panel, cx), "A failed apply keeps the edited content");
        assert_eq!(yaml_text(&panel, cx), yaml.as_str());

        panel.update(cx, |panel, cx| panel.apply(cx));
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        let conflict_request = requests.borrow()[1].clone();
        panel.update(cx, |panel, cx| {
            panel.apply_finished_for(
                conflict_request,
                Ok(ApplyOutcome::Conflict {
                    owners: vec!["kubectl".to_owned(), "helm".to_owned()],
                }),
                cx,
            );
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.conflict_owners.as_deref(),
                Some(["kubectl".to_owned(), "helm".to_owned()].as_slice())
            );
        });
        assert!(panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
        assert!(dirty(&panel, cx), "A conflict keeps the edited content");

        panel.update(cx, |panel, cx| panel.apply(cx));
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        let success_request = requests.borrow()[2].clone();
        panel.update(cx, |panel, cx| {
            panel.apply_finished_for(
                success_request,
                Ok(ApplyOutcome::Applied(pod_object("web-0", "uid-1"))),
                cx,
            );
        });
        assert!(!dirty(&panel, cx));
        assert!(panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
    }

    #[gpui_kit::test]
    fn apply_unknown_result_asks_for_refresh_without_claiming_failure(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let requests: Rc<RefCell<Vec<ApplyRequest>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = requests.clone();
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(move |request: ApplyRequest, _| {
                sink.borrow_mut().push(request);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        let yaml = matching_apply_yaml("uid-1");
        cx.simulate_input(&yaml);
        panel.update(cx, |panel, cx| panel.apply(cx));
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        let request = requests.borrow()[0].clone();
        panel.update(cx, |panel, cx| {
            panel.apply_finished_for(
                request,
                Ok(ApplyOutcome::Unknown {
                    reason: "connection reset".to_owned(),
                }),
                cx,
            );
        });
        assert!(panel.read_with(cx, |panel, _| {
            panel.apply_error.as_deref() == Some(APPLY_UNKNOWN_REASON)
        }));
        assert!(dirty(&panel, cx));
        assert!(panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
        assert!(panel.read_with(cx, |panel, _| panel.applied_at.is_none()));
    }

    #[gpui_kit::test]
    fn apply_transport_error_uses_unknown_result_copy(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let requests: Rc<RefCell<Vec<ApplyRequest>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = requests.clone();
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(move |request: ApplyRequest, _| {
                sink.borrow_mut().push(request);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        let yaml = matching_apply_yaml("uid-1");
        cx.simulate_input(&yaml);
        panel.update(cx, |panel, cx| panel.apply(cx));
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        let request = requests.borrow()[0].clone();
        panel.update(cx, |panel, cx| {
            panel.apply_finished_for(
                request,
                Err("Apply request failed: HyperError: connection reset".to_owned()),
                cx,
            );
        });
        assert!(panel.read_with(cx, |panel, _| {
            panel.apply_error.as_deref() == Some(APPLY_UNKNOWN_REASON)
        }));
    }

    #[gpui_kit::test]
    fn ctrl_enter_opens_the_review_and_the_confirmation_writes(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let calls = Rc::new(AtomicUsize::new(0));
        let sink = calls.clone();
        panel.update(cx, |panel, _| {
            panel.set_on_apply(move |_request| {
                sink.fetch_add(1, Ordering::Relaxed);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        let yaml = matching_apply_yaml("uid-1");
        cx.simulate_input(&yaml);
        cx.simulate_keystrokes("ctrl-enter");
        cx.run_until_parked();
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "Ctrl+Enter must not write before the review is confirmed"
        );
        assert!(
            dirty(&panel, cx),
            "An unconfirmed change stays dirty, so the way back is still visible"
        );
        assert!(
            cx.debug_bounds("yaml-apply-review").is_some(),
            "Ctrl+Enter opens the review"
        );
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(!dirty(&panel, cx), "A confirmed apply marks the YAML saved");
        assert!(panel.read_with(cx, |panel, _| panel.validation_error.is_none()));
    }

    #[gpui_kit::test]
    fn dirty_selection_switch_is_deferred_until_cancel(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("a: 2");
        assert!(dirty(&panel, cx));
        panel.update(cx, |panel, cx| {
            panel.set_yaml(Some("b: 3".to_owned()), cx);
        });
        assert_eq!(
            yaml_text(&panel, cx),
            "a: 2",
            "A new selection must not replace dirty YAML"
        );
        assert!(panel.read_with(cx, |panel, _| panel.has_pending()));
        panel.update(cx, |panel, cx| panel.revert(cx));
        assert_eq!(
            yaml_text(&panel, cx),
            "b: 3",
            "Cancel loads the pending selection"
        );
        assert!(!dirty(&panel, cx));
        assert!(panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
    }

    #[gpui_kit::test]
    fn dirty_selection_keeps_the_original_target_until_discard(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        select(&panel, "uid-1", cx);
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("a: 2");
        panel.update(cx, |panel, cx| {
            panel.set_selection(
                Some(InspectorSelection {
                    object: ObjectRef {
                        name: "web-1".to_owned(),
                        ..pod_ref("uid-2")
                    },
                    yaml: "b: 3".to_owned(),
                }),
                cx,
            );
        });
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.apply_target().unwrap().uid),
            "uid-1"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.pending_selection().unwrap().object.uid),
            "uid-2"
        );
        panel.update(cx, |panel, cx| panel.discard_dirty(cx));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.selection().unwrap().uid.clone()),
            "uid-2"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.apply_target().unwrap().uid),
            "uid-2"
        );
        assert!(!dirty(&panel, cx));
    }

    #[gpui_kit::test]
    fn apply_captures_the_exact_target_and_ignores_a_stale_completion(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        let cluster_id = k8s_core::cluster::ClusterId::derive("ctx", "https://cluster.example");
        panel.update(cx, |panel, _| panel.set_cluster_id(Some(cluster_id)));
        select(&panel, "uid-1", cx);
        let requests: Rc<RefCell<Option<ApplyRequest>>> = Rc::new(RefCell::new(None));
        let sink = requests.clone();
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(move |request: ApplyRequest, _| {
                *sink.borrow_mut() = Some(request);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        let yaml = matching_apply_yaml("uid-1");
        cx.simulate_input(&yaml);
        panel.update(cx, |panel, cx| panel.apply(cx));
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        let request = requests.borrow().clone().expect("request");
        assert_eq!(request.yaml, yaml);
        assert_eq!(request.target.uid, "uid-1");
        assert_eq!(request.target.object_ref().uid, "uid-1");
        assert_eq!(request.target.name, "web-0");
        assert_eq!(request.target.resource.kind, "Pod");
        assert!(request.target.is_complete());
        assert_eq!(request.target.cluster_id(), Some(cluster_id));

        let mut wrong_request = request.clone();
        wrong_request.id += 1;
        panel.update(cx, |panel, cx| {
            panel.apply_finished_for(wrong_request, Err("stale".to_owned()), cx);
        });
        assert!(panel.read_with(cx, |panel, _| panel.is_applying()));
        assert!(dirty(&panel, cx));

        panel.update(cx, |panel, cx| panel.reset(cx));
        select(&panel, "uid-2", cx);
        panel.update(cx, |panel, cx| {
            panel.apply_finished_for(
                request,
                Ok(ApplyOutcome::Applied(pod_object("web-0", "uid-1"))),
                cx,
            );
        });
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.apply_target().unwrap().uid),
            "uid-2"
        );
        assert!(!panel.read_with(cx, |panel, _| panel.is_applying()));
    }

    #[gpui_kit::test]
    fn session_change_invalidates_an_in_flight_target(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        select(&panel, "uid-1", cx);
        let requests: Rc<RefCell<Option<ApplyRequest>>> = Rc::new(RefCell::new(None));
        let sink = requests.clone();
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(move |request: ApplyRequest, _| {
                *sink.borrow_mut() = Some(request);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        let yaml = matching_apply_yaml("uid-1");
        cx.simulate_input(&yaml);
        panel.update(cx, |panel, cx| panel.apply(cx));
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        let request = requests.borrow().clone().expect("request");
        panel.update(cx, |panel, _| panel.set_session_epoch(99));
        assert!(panel.read_with(cx, |panel, _| panel.apply_target().is_none()));
        panel.update(cx, |panel, cx| {
            panel.apply_finished_for(
                request,
                Ok(ApplyOutcome::Applied(pod_object("web-0", "uid-1"))),
                cx,
            );
        });
        assert!(!panel.read_with(cx, |panel, _| panel.is_applying()));
        assert!(dirty(&panel, cx));
    }

    #[gpui_kit::test]
    fn apply_rejects_yaml_without_an_exact_target(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        let calls = Rc::new(AtomicUsize::new(0));
        let sink = calls.clone();
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(move |_request: ApplyRequest, _| {
                sink.fetch_add(1, Ordering::Relaxed);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("a: 2");
        panel.update(cx, |panel, cx| panel.apply(cx));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(dirty(&panel, cx));
        assert!(panel.read_with(cx, |panel, _| panel.apply_error.is_some()));
        assert!(!panel.read_with(cx, |panel, _| panel.is_applying()));
    }

    #[gpui_kit::test]
    fn apply_requires_selected_uid_before_dispatch(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        select(&panel, "uid-1", cx);
        let calls = Rc::new(AtomicUsize::new(0));
        let sink = calls.clone();
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(move |_request: ApplyRequest, _| {
                sink.fetch_add(1, Ordering::Relaxed);
            });
        });
        for yaml in [
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: web-0\n  namespace: default\n",
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: web-0\n  namespace: default\n  uid: uid-2\n",
        ] {
            cx.simulate_keystrokes("ctrl-a");
            cx.simulate_input(yaml);
            panel.update(cx, |panel, cx| panel.apply(cx));
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            assert!(dirty(&panel, cx));
            assert!(panel.read_with(cx, |panel, _| {
                !panel.applying && panel.current_apply_request().is_none()
            }));
        }
        assert!(panel.read_with(cx, |panel, _| {
            panel
                .apply_error
                .as_deref()
                .is_some_and(|error| error.contains("UID"))
        }));
    }

    #[gpui_kit::test]
    fn apply_rejects_yaml_for_a_different_object(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        select(&panel, "uid-1", cx);
        let calls = Rc::new(AtomicUsize::new(0));
        let sink = calls.clone();
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(move |_request: ApplyRequest, _| {
                sink.fetch_add(1, Ordering::Relaxed);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("metadata:\n  name: other\n  uid: uid-1");
        panel.update(cx, |panel, cx| panel.apply(cx));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(dirty(&panel, cx));
        assert!(panel.read_with(cx, |panel, _| panel.apply_error.is_some()));
    }

    #[gpui_kit::test]
    fn reset_discards_dirty_content_and_pending_selection(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        select(&panel, "uid-1", cx);
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("a: 2");
        panel.update(cx, |panel, cx| {
            panel.set_selection(
                Some(InspectorSelection {
                    object: pod_ref("uid-2"),
                    yaml: "b: 3".to_owned(),
                }),
                cx,
            );
        });
        panel.update(cx, |panel, cx| panel.reset(cx));
        assert!(!panel.read_with(cx, |panel, _| panel.has_yaml()));
        assert!(!panel.read_with(cx, |panel, _| panel.has_pending()));
        assert!(panel.read_with(cx, |panel, _| panel.selection().is_none()));
        assert!(panel.read_with(cx, |panel, _| panel.apply_target().is_none()));
        assert!(!dirty(&panel, cx));
    }

    fn source_setup(
        cx: &mut TestAppContext,
    ) -> (
        Entity<InspectorPanel>,
        Arc<FakeSource>,
        &mut gpui_kit::VisualTestContext,
    ) {
        init_app(cx);
        let (panel, cx) = cx.add_window_view(|_, cx| InspectorPanel::new(cx));
        let source = Arc::new(FakeSource {
            describes: AtomicUsize::new(0),
            events: AtomicUsize::new(0),
            object: None,
            event_count: 0,
        });
        panel.update(cx, |panel, _| {
            panel.set_source(source.clone() as Arc<dyn InspectorSource>);
        });
        (panel, source, cx)
    }

    fn layout_source_setup(
        cx: &mut TestAppContext,
    ) -> (
        Entity<InspectorPanel>,
        Arc<FakeSource>,
        &mut gpui_kit::VisualTestContext,
    ) {
        init_app(cx);
        let (panel, cx) = cx.add_window_view(|_, cx| InspectorPanel::new(cx));
        let source = Arc::new(FakeSource {
            describes: AtomicUsize::new(0),
            events: AtomicUsize::new(0),
            object: Some(layout_pod_object(LAYOUT_UID)),
            event_count: 0,
        });
        panel.update(cx, |panel, _| {
            panel.set_source(source.clone() as Arc<dyn InspectorSource>);
        });
        (panel, source, cx)
    }

    fn events_source_setup(
        cx: &mut TestAppContext,
        event_count: usize,
    ) -> (
        Entity<InspectorPanel>,
        Arc<FakeSource>,
        &mut gpui_kit::VisualTestContext,
    ) {
        init_app(cx);
        let (panel, cx) = cx.add_window_view(|_, cx| InspectorPanel::new(cx));
        let source = Arc::new(FakeSource {
            describes: AtomicUsize::new(0),
            events: AtomicUsize::new(0),
            object: None,
            event_count,
        });
        panel.update(cx, |panel, _| {
            panel.set_source(source.clone() as Arc<dyn InspectorSource>);
        });
        (panel, source, cx)
    }

    /// Panel with a source that never answers Describe.
    fn hanging_setup(
        cx: &mut TestAppContext,
        events_fail: bool,
    ) -> (
        Entity<InspectorPanel>,
        Arc<HangingSource>,
        &mut gpui_kit::VisualTestContext,
    ) {
        init_app(cx);
        let (panel, cx) = cx.add_window_view(|_, cx| InspectorPanel::new(cx));
        let source = Arc::new(HangingSource::new(events_fail));
        panel.update(cx, |panel, _| {
            panel.set_source(source.clone() as Arc<dyn InspectorSource>);
        });
        (panel, source, cx)
    }

    /// Opens every Describe section, so a test that measures a row is not measuring a
    /// collapsed heading.
    ///
    /// `UI-REDESIGN.md` §3.4 made every section below Status a disclosure, and Status is the one
    /// that starts open. A layout test is about the rows, not about the default disclosure
    /// state, so it says which state it wants rather than relying on the default.
    fn open_all_sections(panel: &Entity<InspectorPanel>, cx: &mut gpui_kit::VisualTestContext) {
        panel.update(cx, |panel, cx| {
            panel.open_sections = BTreeSet::from([
                DetailSection::Status,
                DetailSection::Labels,
                DetailSection::Conditions,
                DetailSection::Containers,
                DetailSection::Spec,
                DetailSection::Events,
                DetailSection::Related,
                DetailSection::Identity,
            ]);
            cx.notify();
        });
        cx.run_until_parked();
    }

    fn select(panel: &Entity<InspectorPanel>, uid: &str, cx: &mut gpui_kit::VisualTestContext) {
        panel.update(cx, |panel, cx| {
            panel.set_selection(
                Some(InspectorSelection {
                    object: pod_ref(uid),
                    yaml: "kind: Pod".to_owned(),
                }),
                cx,
            );
        });
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn describe_loads_lazily_and_is_cached_by_uid(cx: &mut TestAppContext) {
        let (panel, source, cx) = source_setup(cx);
        // Selecting lands on the summary (`DEFAULT_TAB`), so the details are asked for once, and
        // the fact that opening the tab asks for nothing more is the caching this test is about.
        select(&panel, "uid-1", cx);
        assert_eq!(source.describes.load(Ordering::Relaxed), 1);

        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        assert_eq!(source.describes.load(Ordering::Relaxed), 1);
        panel.read_with(cx, |panel, _| {
            assert!(matches!(
                panel.describe_states.get("uid-1"),
                Some(LoadState::Ready(_))
            ));
        });

        // Return to the cached tab without another request.
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Yaml, cx));
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        assert_eq!(source.describes.load(Ordering::Relaxed), 1, "Cache by UID");

        // A new selection misses the cache and loads again.
        select(&panel, "uid-2", cx);
        cx.run_until_parked();
        assert_eq!(source.describes.load(Ordering::Relaxed), 2);
    }

    #[gpui_kit::test]
    fn describe_rows_use_measured_width_and_keep_long_values_bounded(cx: &mut TestAppContext) {
        // The marker slot is reserved on every row so a marked row and an unmarked one share one
        // key column and one value start. The glyph in it grew to `design::size::STATUS_MARKER`,
        // so the slot has to have grown with it or the two would not be the same shape.
        assert_eq!(
            DESCRIBE_MARKER_SLOT,
            design::size::STATUS_MARKER,
            "the marker slot and the marker are one size: a glyph wider than its slot is a \
             different key column for a marked row"
        );
        let (panel, _source, cx) = layout_source_setup(cx);
        select(&panel, LAYOUT_UID, cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        open_all_sections(&panel, cx);
        // The fixture must keep its long values long, or this test stops covering the
        // wrapping branch.
        assert!(LAYOUT_LONG_MESSAGE.chars().count() > DESCRIBE_INLINE_VALUE_LIMIT);
        assert!(LAYOUT_LONG_IMAGE.chars().count() > DESCRIBE_INLINE_VALUE_LIMIT);

        for width in [240.0, 288.0, 336.0, 480.0] {
            cx.simulate_resize(gpui_kit::size(gpui_kit::px(width), gpui_kit::px(640.0)));
            cx.run_until_parked();
            cx.run_until_parked();

            let body = cx
                .debug_bounds("inspector-describe-body")
                .expect("Describe body");
            assert!(
                f32::from(body.size.width) <= f32::from(design::size::INSPECTOR_MAX),
                "the Describe body tracks the Inspector, which DESIGN.md §3.3 caps at \
                 design::size::INSPECTOR_MAX"
            );
            // The last flag marks a value longer than the inline limit, which must wrap
            // under its key instead of running along one line.
            let selectors = [
                (
                    "inspector-describe-field-company.example.com/team-platform/extremely-long-label-name",
                    "inspector-describe-field-company.example.com/team-platform/extremely-long-label-name-key",
                    "inspector-describe-field-company.example.com/team-platform/extremely-long-label-name-value",
                    false,
                ),
                (
                    "inspector-describe-field-configHash",
                    "inspector-describe-field-configHash-key",
                    "inspector-describe-field-configHash-value",
                    false,
                ),
                (
                    "inspector-describe-condition-Ready",
                    "inspector-describe-condition-Ready-key",
                    "inspector-describe-condition-Ready-value",
                    false,
                ),
                (
                    "inspector-describe-condition-message-Ready",
                    "inspector-describe-condition-message-Ready-key",
                    "inspector-describe-condition-message-Ready-value",
                    true,
                ),
                (
                    "inspector-container-image-api",
                    "inspector-container-image-api-key",
                    "inspector-container-image-api-value",
                    true,
                ),
            ];
            // The panel decides the branch from the width it measured, so the
            // expectation follows the same state instead of guessing from the window.
            let two_column = panel.read_with(cx, |panel, _| !panel.describe_stacked());
            for (selector, key_selector, value_selector, long_value) in selectors {
                let row = cx
                    .debug_bounds(selector)
                    .unwrap_or_else(|| panic!("{selector}"));
                let key = cx
                    .debug_bounds(key_selector)
                    .unwrap_or_else(|| panic!("{key_selector}"));
                let value = cx
                    .debug_bounds(value_selector)
                    .unwrap_or_else(|| panic!("{value_selector}"));
                assert!(row.origin.x >= body.origin.x);
                assert!(
                    f32::from(row.origin.x + row.size.width)
                        <= f32::from(body.origin.x + body.size.width) + 1.0
                );
                assert!(f32::from(key.size.width) > 0.0);
                assert!(f32::from(value.size.width) > 0.0);
                assert!(
                    f32::from(value.origin.x + value.size.width)
                        <= f32::from(row.origin.x + row.size.width) + 1.0
                );
                assert!(f32::from(key.size.height) <= f32::from(design::size::ROW));
                if two_column && !long_value {
                    assert_eq!(f32::from(row.size.height), f32::from(design::size::ROW));
                    assert!(f32::from(key.size.width) >= 120.0);
                    // The marker sits in a fixed slot, so a marked row and an unmarked
                    // row start their value at the same edge.
                    let marked = cx
                        .debug_bounds("inspector-describe-condition-Ready-value")
                        .expect("marked value");
                    let value_start = f32::from(value.origin.x);
                    assert!((f32::from(marked.origin.x) - value_start).abs() <= 0.5);
                } else {
                    // Key above, value below and wrapping: two lines or more.
                    assert!(f32::from(row.size.height) >= f32::from(design::size::ROW) * 2.);
                    assert!(f32::from(value.size.height) >= f32::from(design::size::ROW));
                    assert!(f32::from(key.origin.y) < f32::from(value.origin.y));
                }
            }
            let labels = cx
                .debug_bounds("inspector-describe-section-Labels")
                .expect("Labels section");
            let conditions = cx
                .debug_bounds("inspector-describe-section-Conditions")
                .expect("Conditions section");
            let gap =
                f32::from(conditions.origin.y) - f32::from(labels.origin.y + labels.size.height);
            assert!(
                (gap - f32::from(space::MD)).abs() <= 1.0,
                "section gap: {gap}"
            );
            // The block title is a section role, not another metadata label.
            let title = cx
                .debug_bounds("inspector-describe-section-title-Conditions")
                .expect("Conditions title");
            assert!(
                f32::from(title.size.height) > f32::from(design::text::CAPTION_LINE_HEIGHT),
                "block title must outrank a field key"
            );
        }
    }

    /// The two-column layout is a promise about width, and the promise is arithmetic.
    ///
    /// `DESIGN.md` §3.2 lets a component keep a size that is not a token, but only if it says
    /// why that size is independent. The key column and the value column are one decision: a
    /// wider key beside the same minimum would promise a two-column layout its own numbers say
    /// does not fit, so the minimum is derived from the two rather than written out beside them.
    /// A Describe value the cluster sent follows the data font, and its row grows with it.
    ///
    /// The value is set in the data role, so reading the default `design::text::MONO_SM` would
    /// leave this surface at 12px while the resource table grows. The row is the other half of
    /// the promise: a taller glyph in a 28px box is cropped, and the setting's own help text says
    /// the row grows so text is never cropped. The prose rows beside it must not move, or the
    /// setting would be scaling the whole panel rather than the data in it.
    #[gpui_kit::test]
    #[gpui_kit::test]
    fn a_describe_data_value_follows_the_configured_data_font(cx: &mut TestAppContext) {
        let (panel, _source, cx) = layout_source_setup(cx);
        select(&panel, LAYOUT_UID, cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        open_all_sections(&panel, cx);
        // The widest Inspector, so the rows under test are the one-line two-column layout.
        cx.simulate_resize(gpui_kit::size(gpui_kit::px(480.0), gpui_kit::px(640.0)));
        cx.run_until_parked();
        assert!(
            !panel.read_with(cx, |panel, _| panel.describe_stacked()),
            "this test is about the one-line row, and 480px is the width that keeps it"
        );
        // A Status field is a data column; a label is prose.
        const DATA_ROW: &str = "inspector-describe-field-configHash";
        const PROSE_ROW: &str =
            "inspector-describe-field-company.example.com/team-platform/extremely-long-label-name";

        let before = f32::from(
            cx.debug_bounds(DATA_ROW)
                .expect("the data row is laid out")
                .size
                .height,
        );
        assert!((before - f32::from(design::size::ROW)).abs() <= 1.0);

        cx.update(|_, cx| {
            crate::settings::SettingsStore::update(cx, |store, cx| {
                store
                    // 24px is a 36px line box, which clears the 32px default row.
                    // 20px was a 30px line and the default row now absorbs it, so
                    // the fixture would not have raised anything.
                    .set_user_settings(r#"{ "buffer_font_size": 24 }"#, cx)
                    .expect("the data size applies");
            });
        });
        panel.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();

        let expected = cx.update(|_, cx| crate::settings::data_typography(cx).row_height());
        assert!(
            f32::from(expected) > f32::from(design::size::ROW),
            "the fixture has to actually raise the data font, or this test proves nothing"
        );
        let data_height = f32::from(
            cx.debug_bounds(DATA_ROW)
                .expect("the data row is still laid out")
                .size
                .height,
        );
        assert!(
            (data_height - f32::from(expected)).abs() <= 1.0,
            "a data value is cropped in a {before}px row once the reader raises the data font: \
             the row drew {data_height}px and the configured line needs {expected}px"
        );
        let prose_height = f32::from(
            cx.debug_bounds(PROSE_ROW)
                .expect("the prose row is laid out")
                .size
                .height,
        );
        assert!(
            (prose_height - f32::from(design::size::ROW)).abs() <= 1.0,
            "a label is prose, not data: the data font size must not restyle the whole panel"
        );
    }

    // A truncated value is a display decision, so the panel has to offer the whole text to the
    // keyboard. This asserts the affordances exist, not that a value stays clipped.
    #[gpui_kit::test]
    /// A shortcut nobody can see is not a shortcut. The field rows answer to two chords, so
    /// the rows have to actually show them once the reader is on the row.
    #[gpui_kit::test]
    fn a_value_row_shows_the_chords_it_answers_to(cx: &mut TestAppContext) {
        let (panel, _source, cx) = layout_source_setup(cx);
        select(&panel, LAYOUT_UID, cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        open_all_sections(&panel, cx);

        let chords = cx
            .debug_bounds("inspector-describe-field-configHash-chords")
            .expect("a field row renders its chords");
        // The chords must not take the room the value needs. The value column is the flexible
        // one, so the pair has to fit inside the row rather than pushing the text out.
        assert!(
            chords.size.width > px(0.),
            "the chord pair occupies space, so it cannot have collapsed to nothing"
        );
        assert!(
            chords.size.height <= design::size::ROW,
            "the chords stay inside the row rhythm instead of growing it"
        );
    }

    #[gpui_kit::test]
    fn every_value_row_is_a_tab_stop_with_an_expand_and_a_copy_path(cx: &mut TestAppContext) {
        let (panel, _source, cx) = layout_source_setup(cx);
        select(&panel, LAYOUT_UID, cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        open_all_sections(&panel, cx);
        for selector in [
            "inspector-describe-field-configHash",
            "inspector-container-image-api",
            "inspector-describe-condition-message-Ready",
        ] {
            assert!(
                panel
                    .read_with(cx, |panel, _| panel.value_focus_handle(selector))
                    .is_some(),
                "{selector} must be reachable with the keyboard"
            );
        }
        // A handle the panel holds is not yet a tab stop, so walk the rendered order for one of
        // them. The pool is handed out in render order, so the rows keep the reading order.
        let entry = panel.read_with(cx, |panel, _| panel.tab_focus.clone());
        let row_focus = panel
            .read_with(cx, |panel, _| {
                panel.value_focus_handle("inspector-describe-field-configHash")
            })
            .expect("the row keeps its handle");
        assert!(
            reaches_in_tab_order(cx, &entry, &row_focus),
            "a Describe value row is reached by Tab"
        );

        // A row expands to its full value and copies the text the ellipsis hides.
        let selector = "inspector-describe-field-configHash";
        let bounds = |cx: &mut gpui_kit::VisualTestContext| {
            let row = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector}"));
            let key = cx
                .debug_bounds("inspector-describe-field-configHash-key")
                .expect("key");
            let value = cx
                .debug_bounds("inspector-describe-field-configHash-value")
                .expect("value");
            (row, key, value)
        };
        let (collapsed_row, collapsed_key, collapsed_value) = bounds(cx);
        if !panel.read_with(cx, |panel, _| panel.describe_stacked()) {
            assert!(
                (f32::from(collapsed_key.origin.y) - f32::from(collapsed_value.origin.y)).abs()
                    < 1.0,
                "a collapsed row keeps its value beside the key"
            );
        }
        panel.update(cx, |panel, cx| panel.toggle_value_expansion(selector, cx));
        cx.run_until_parked();
        assert!(
            panel.read_with(cx, |panel, _| panel.value_is_expanded(selector)),
            "Enter expands the focused row"
        );
        let (expanded_row, expanded_key, expanded_value) = bounds(cx);
        assert!(
            f32::from(expanded_key.origin.y) < f32::from(expanded_value.origin.y),
            "an expanded row moves the key above the value, so the value gets the whole width"
        );
        assert!(
            f32::from(expanded_row.size.height) > f32::from(collapsed_row.size.height),
            "an expanded row grows instead of clipping: {:?} then {:?}",
            collapsed_row.size.height,
            expanded_row.size.height
        );

        panel.update(cx, |panel, cx| panel.copy_value(selector, cx));
        let clipboard = cx
            .read_from_clipboard()
            .expect("clipboard text")
            .text()
            .expect("clipboard string");
        assert!(
            clipboard.contains("sha256:abcdef"),
            "the copy takes the whole value: {clipboard:?}"
        );
        assert!(panel.read_with(cx, |panel, _| panel.value_copied(selector)));
    }

    // Every scrollable region in the panel is focusable, so the keyboard can reach the content
    // that the pointer can only drag.
    #[gpui_kit::test]
    fn every_scrollable_region_has_a_focus_handle_and_scrolls(cx: &mut TestAppContext) {
        let (panel, _source, cx) = layout_source_setup(cx);
        select(&panel, LAYOUT_UID, cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        open_all_sections(&panel, cx);
        // A short viewport guarantees the content overflows, so a scroll key has somewhere to go.
        cx.simulate_resize(gpui_kit::size(gpui_kit::px(240.), gpui_kit::px(200.)));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("describe-scroll").is_some(),
            "the Describe scroll region renders"
        );
        let describe = panel.read_with(cx, |panel, _| panel.describe_focus.clone());
        let entry = panel.read_with(cx, |panel, _| panel.tab_focus.clone());
        assert!(
            reaches_in_tab_order(cx, &entry, &describe),
            "the Describe scroll region is reached by Tab"
        );
        cx.update(|window, cx| window.focus(&describe, cx));
        assert!(cx.update(|window, _| describe.is_focused(window)));
        // A GPUI scroll offset is the distance from the top of the content to the top of the
        // viewport, so moving the view down makes it more negative. That is the direction the
        // mouse wheel uses too.
        let before = panel.read_with(cx, |panel, _| panel.describe_scroll.offset().y);
        cx.simulate_keystrokes("pagedown");
        cx.run_until_parked();
        let after = panel.read_with(cx, |panel, _| panel.describe_scroll.offset().y);
        assert!(
            after < before,
            "Page Down moves the Describe view: {before} then {after}"
        );
        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        assert_eq!(
            f32::from(panel.read_with(cx, |panel, _| panel.describe_scroll.offset().y)),
            0.0,
            "Home returns to the first row"
        );

        // The Events list scrolls the same way. The Describe fixture holds no events, and a list
        // with nothing to scroll proves nothing, so the panel gets a source that has them.
        panel.update(cx, |panel, _| {
            panel.set_source(Arc::new(FakeSource {
                describes: AtomicUsize::new(0),
                events: AtomicUsize::new(0),
                object: Some(layout_pod_object(LAYOUT_UID)),
                event_count: 200,
            }) as Arc<dyn InspectorSource>);
        });
        // A new source is a new session, so it drops the selection. Events render for the
        // selected object, and an empty selection renders no list to scroll at all.
        select(&panel, LAYOUT_UID, cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Events, cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("events-scroll").is_some());
        let events = panel.read_with(cx, |panel, _| panel.events_focus.clone());
        assert!(
            reaches_in_tab_order(cx, &entry, &events),
            "the Events scroll region is reached by Tab"
        );
        cx.update(|window, cx| window.focus(&events, cx));
        assert!(cx.update(|window, _| events.is_focused(window)));
        let events_offset =
            |panel: &InspectorPanel| panel.events_scroll.0.borrow().base_handle.offset().y;
        let before = panel.read_with(cx, |panel, _| events_offset(panel));
        cx.simulate_keystrokes("pagedown");
        cx.run_until_parked();
        let after = panel.read_with(cx, |panel, _| events_offset(panel));
        assert!(
            after < before,
            "Page Down moves the event list: {before} then {after}"
        );

        // The Metrics charts scroll through the same contract.
        let metrics_runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("metrics runtime");
        let metrics = MetricsHandle::new(
            metrics_runtime.handle().clone(),
            Arc::new(EmptyMetricsPort) as Arc<dyn ClusterDataPort>,
        );
        panel.update(cx, |panel, cx| {
            panel.set_metrics_source(Some(metrics), MetricsProbeState::Available, cx);
            panel.set_metrics_visible(true, cx);
            panel.show_tab(InspectorTab::Metrics, cx);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("metrics-scroll").is_some());
        let charts = panel.read_with(cx, |panel, _| panel.metrics_focus.clone());
        assert!(
            reaches_in_tab_order(cx, &entry, &charts),
            "the Metrics scroll region is reached by Tab"
        );
        cx.update(|window, cx| window.focus(&charts, cx));
        assert!(cx.update(|window, _| charts.is_focused(window)));
    }

    // Apply is gated on validity, and the problems list is a list the keyboard can walk.
    #[gpui_kit::test]
    fn a_parse_problem_blocks_apply_and_the_list_can_be_walked(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, _| panel.set_on_apply(|_| {}));
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("@oops");
        // The editor parses after a typing pause, so the test waits out the same debounce the
        // user waits out. `run_until_parked` does not move the clock. The two steps bracket
        // `VALIDATE_DEBOUNCE_TEST` from both sides, so a change to the real debounce fails here
        // instead of leaving the test waiting on a stale copy of the number.
        cx.executor()
            .advance_clock(VALIDATE_DEBOUNCE_TEST - Duration::from_millis(1));
        cx.run_until_parked();
        assert!(
            inline_diagnostics(&panel, cx).is_empty(),
            "validation waits for the typing to pause"
        );
        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        assert!(
            !inline_diagnostics(&panel, cx).is_empty(),
            "the editor reports the problem while typing"
        );
        assert!(
            cx.debug_bounds("yaml-problems").is_some(),
            "the problems list replaces the silent disabled button"
        );
        // The gate is the point: broken YAML never reaches a review, let alone a request.
        panel.update(cx, |panel, cx| panel.apply(cx));
        assert!(
            panel.read_with(cx, |panel, _| panel.reviewed_change().is_none()),
            "a document with a diagnostic must not open a review"
        );
        let cursor = panel.read_with(cx, |panel, _| panel.problem_cursor);
        assert_eq!(cursor, 0);
        panel.update(cx, |panel, _| panel.move_problem_cursor(1));
        panel.update(cx, |panel, _| panel.move_problem_cursor(1));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.problem_cursor),
            0,
            "the cursor stops at the last problem"
        );
        assert!(cx.debug_bounds("yaml-problem-0").is_some());
    }

    // A list longer than the cap is capped, and the cap drops nothing.
    #[gpui_kit::test]
    fn a_long_problem_list_is_capped_and_keeps_every_problem(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        // The parser stops at the first syntax error, so a long list arrives the way a server
        // check would deliver it: as a batch the panel has to render.
        let count = 16;
        assert!(
            count > PROBLEMS_VISIBLE_ROWS,
            "the fixture has to be longer than the cap"
        );
        let diagnostics = (0..count)
            .map(|index| Diagnostic {
                line: index * 2,
                column: 0,
                message: format!("problem {index}"),
            })
            .collect::<Vec<_>>();
        let editor = panel.read_with(cx, |panel, _| panel.yaml_view.clone());
        editor.update(cx, |view, cx| view.set_diagnostics(diagnostics, cx));
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.problem_count),
            count,
            "the panel counts every problem the editor reported"
        );
        let list = cx
            .debug_bounds("yaml-problem-list")
            .expect("the problems list");
        let budget = cx.update(|_, cx| f32::from(problems_list_max_height(cx)));
        assert!(
            f32::from(list.size.height) <= budget,
            "a long list is capped instead of pushing the editor out of the panel: {} against \
             a budget of {budget}",
            f32::from(list.size.height)
        );
        assert!(
            cx.debug_bounds("yaml-problem-0").is_some(),
            "the first problem stays in the list"
        );
        // The cap is a viewport, not a truncation: the row past the cap is still rendered, and
        // the keyboard walk reaches it.
        assert!(
            cx.debug_bounds("yaml-problem-15").is_some(),
            "a capped list drops nothing"
        );
        let last = count - 1;
        panel.update(cx, |panel, _| panel.move_problem_cursor(last as isize));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.problem_cursor),
            last,
            "the cursor reaches the last problem"
        );
    }

    /// The parser's sentence is the whole of the feature, so it is shown rather than trimmed.
    ///
    /// A diagnostic was drawn on one line with `Line 448, column 10` beside it and an ellipsis at
    /// the end, which at the 336px this panel ships at left about twenty-three characters of an
    /// eleven-pixel face for a message that is thirty-four. `found unexpected end of stream` —
    /// which is what a half-typed manifest reports, and the single most common thing a reader
    /// will meet here — arrived as `found unexpected end of stream…` with the rest behind a
    /// tooltip nobody opens while looking at a red underline. The address is now its own line and
    /// the message wraps under it.
    #[gpui_kit::test]
    fn a_problem_message_is_readable_rather_than_trimmed(cx: &mut TestAppContext) {
        for width in [260., 336., 352.] {
            let (panel, cx) = setup(cx, "name: app");
            select(&panel, "uid-1", cx);
            let editor = panel.read_with(cx, |panel, _| panel.yaml_view.clone());
            editor.update(cx, |view, cx| {
                view.set_diagnostics(
                    vec![Diagnostic {
                        line: 447,
                        column: 9,
                        message: "found unexpected end of stream while scanning a quoted scalar"
                            .to_owned(),
                    }],
                    cx,
                )
            });
            cx.simulate_resize(gpui_kit::size(gpui_kit::px(width), gpui_kit::px(640.)));
            cx.run_until_parked();
            let message = cx
                .debug_bounds("yaml-problem-message")
                .expect("the message is on screen");
            let address_line = cx
                .debug_bounds("yaml-problem-0")
                .expect("the row is on screen");
            let one_line = f32::from(design::text::CAPTION_LINE_HEIGHT);
            assert!(
                f32::from(message.size.height) >= one_line * 2. - 1.,
                "{width}px: the message takes {} for one line of {one_line} — it was trimmed \\
                 again",
                f32::from(message.size.height)
            );
            assert!(
                f32::from(address_line.size.height) > f32::from(design::size::ROW),
                "{width}px: the row grew to hold the address above the message, so the message \\
                 is not competing with it for the same line"
            );
            // And the row still fits inside the panel it is drawn in, which is the other half of
            // wrapping: a taller row that overhangs is worse than a trimmed one.
            let frame = cx.debug_bounds("inspector-frame").expect("the panel");
            assert!(
                f32::from(message.origin.x + message.size.width)
                    <= f32::from(frame.origin.x + frame.size.width) + 0.5,
                "{width}px: the wrapped message does not run past the panel edge"
            );
        }
    }

    /// The review asks the server itself. A local diff cannot see a schema violation or an
    /// immutable field, and the apply path does not validate strictly, so an answer the reader
    /// has to go and ask a button for is an answer they do not have on the last screen before a
    /// write.
    #[gpui_kit::test]
    fn the_review_asks_the_server_about_the_document_without_writing_it(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, _| panel.set_on_apply(|_| {}));
        let asked: Rc<RefCell<Vec<ApplyRequest>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = asked.clone();
        panel.update(cx, |panel, _| {
            panel.set_targeted_check_handler(move |request: ApplyRequest, _| {
                sink.borrow_mut().push(request);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input(&matching_apply_yaml("uid-1"));
        panel.update(cx, |panel, cx| panel.apply(cx));
        cx.run_until_parked();

        let status = cx
            .debug_bounds("yaml-review-check-status")
            .expect("the review states what has been verified");
        assert!(status.size.height > px(0.), "the status line is rendered");
        assert_eq!(asked.borrow().len(), 1, "opening a review asks the server");
        assert_eq!(
            asked.borrow()[0].yaml,
            matching_apply_yaml("uid-1"),
            "the document the reader is looking at is the document that was checked"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.apply_check.clone()),
            ApplyCheckState::Running,
            "no verdict yet, and the review says so rather than implying one"
        );
        assert!(
            cx.debug_bounds("yaml-review-check").is_none(),
            "the check is not a step the reader has to know about"
        );
        assert!(
            !panel.read_with(cx, |panel, _| panel.is_applying()),
            "a server check must not apply the change"
        );

        // The shell drops any reply that is not the request it is holding, so a verdict has to
        // be answerable while the review is open and no write is in flight.
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.current_apply_request()),
            Some(asked.borrow()[0].clone()),
            "a check belongs to a review that has not been confirmed"
        );
    }

    /// A verdict belongs to the document that was checked, so it must not survive the review.
    #[gpui_kit::test]
    fn a_verdict_does_not_outlive_the_review_it_checked(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, _| panel.set_on_apply(|_| {}));
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input(&matching_apply_yaml("uid-1"));
        panel.update(cx, |panel, cx| panel.apply(cx));
        cx.run_until_parked();

        panel.update(cx, |panel, cx| {
            panel.apply_check_finished(Ok(ApplyVerdict::Valid), cx)
        });
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.apply_check.clone()),
            ApplyCheckState::Valid
        );

        panel.update(cx, |panel, cx| panel.cancel_pending_apply(cx));
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.apply_check.clone()),
            ApplyCheckState::Running,
            "a stale verdict must not greet the next document"
        );
        assert!(
            cx.debug_bounds("yaml-apply-review").is_none(),
            "the review is gone"
        );
    }

    /// Every verdict has to read as words, because a person has to act on it.
    #[gpui_kit::test]
    fn every_verdict_has_a_readable_outcome(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, _| panel.set_on_apply(|_| {}));
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input(&matching_apply_yaml("uid-1"));
        panel.update(cx, |panel, cx| panel.apply(cx));
        cx.run_until_parked();

        for (result, expected) in [
            (Ok(ApplyVerdict::Valid), ApplyCheckState::Valid),
            (
                Ok(ApplyVerdict::Conflict {
                    owners: vec!["kube-controller-manager".to_owned()],
                }),
                ApplyCheckState::Conflict {
                    owners: vec!["kube-controller-manager".to_owned()],
                },
            ),
            (
                Ok(ApplyVerdict::Conflict { owners: Vec::new() }),
                ApplyCheckState::Conflict { owners: Vec::new() },
            ),
            (
                Err("The server could not be reached.".to_owned()),
                ApplyCheckState::Failed {
                    reason: "The server could not be reached.".to_owned(),
                },
            ),
        ] {
            panel.update(cx, |panel, cx| panel.apply_check_finished(result, cx));
            cx.run_until_parked();
            assert_eq!(
                panel.read_with(cx, |panel, _| panel.apply_check.clone()),
                expected
            );
            assert!(
                cx.debug_bounds("yaml-review-check-status").is_some(),
                "every verdict renders a line the reader can act on"
            );
        }
    }

    #[gpui_kit::test]
    fn the_review_names_the_target_and_shows_the_change(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, _| panel.set_on_apply(|_| {}));
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input(&matching_apply_yaml("uid-1"));
        panel.update(cx, |panel, cx| panel.apply(cx));
        cx.run_until_parked();
        let review = panel.read_with(cx, |panel, _| panel.reviewed_change().cloned());
        let review = review.expect("a reviewed change");
        assert_eq!(review.target.uid, "uid-1");
        assert_eq!(review.target.name, "web-0");
        // Opening the review is not the write. Only `confirm_pending_apply` starts a request, so
        // this is the half of "the write needs an explicit action" that a selector cannot prove.
        assert!(
            !panel.read_with(cx, |panel, _| panel.is_applying()),
            "Apply only opens a review; it must not start a write"
        );
        assert!(cx.debug_bounds("yaml-apply-review").is_some());
        assert!(
            cx.debug_bounds("yaml-review-apply").is_some(),
            "the write needs an explicit action"
        );
        assert!(
            cx.debug_bounds("yaml-review-keep-editing").is_some(),
            "the safe action is in the same strip"
        );
        // The claim is about the order the user walks, and a walk follows the tab index. Both
        // halves of that order are checked here: the strip reads left to right, and each control
        // declares where it sits in the walk. A lower index to the right of a higher one is the
        // control a reader meets last, so the destructive action has to be the rightmost.
        let keep_editing_bounds = cx
            .debug_bounds("yaml-review-keep-editing")
            .expect("the safe action keeps its place in the strip");
        let write_bounds = cx
            .debug_bounds("yaml-review-apply")
            .expect("the write keeps its place in the strip");
        assert!(
            keep_editing_bounds.origin.x < write_bounds.origin.x,
            "the safe action has to read before the write"
        );

        // `WRITE-OPS.md` §3.1: a kind the product knows the shape of gets a field diff, not a
        // diff of the text. The reviewed change here is a different document, so a line diff
        // would be a screenful of `+`; the semantic diff is the two fields that moved.
        assert!(
            cx.debug_bounds("yaml-diff-path").is_some(),
            "the review names the field that changed, not the lines that moved"
        );
        let diff_frame = cx
            .debug_bounds("yaml-apply-review-diff")
            .expect("the diff frame");
        assert!(
            f32::from(diff_frame.size.height) < f32::from(design::size::ROW) * 8.,
            "a two-field change does not fill the panel: {:?}",
            diff_frame.size.height
        );

        // Cancelling writes nothing and leaves the change on screen.
        panel.update(cx, |panel, cx| panel.cancel_pending_apply(cx));
        assert!(panel.read_with(cx, |panel, _| panel.reviewed_change().is_none()));
        assert!(dirty(&panel, cx));
    }

    /// Every control, on every tab, at every width the panel is allowed to be.
    ///
    /// `UI-SPEC.md` §11.1 gives the Inspector a range — 260 to 480 — and the shell that owns the
    /// docking decision is currently docking it at 336 while the spec says 352, so neither number
    /// is the width to test and the whole range is. A control that runs past the panel's own right
    /// edge is clipped by the frame, and a clipped control is one the reader cannot read or aim
    /// at; the toolbars are the place it happens, because they are the only rows with fixed-width
    /// children.
    ///
    /// The four widths are the spec's two candidates (232/336 and 236/352), its floor and its
    /// ceiling, so a regression at any of them is caught rather than waiting to be seen.
    #[gpui_kit::test]
    fn no_tab_loses_a_control_at_any_width_the_panel_allows(cx: &mut TestAppContext) {
        for width in [260., 336., 352., 480.] {
            let (panel, cx) = setup(cx, "name: app");
            select(&panel, "uid-1", cx);
            panel.update(cx, |panel, _cx| panel.set_on_apply(|_| {}));
            cx.simulate_resize(gpui_kit::size(gpui_kit::px(width), gpui_kit::px(640.)));
            open_all_sections(&panel, cx);
            for tab in [
                InspectorTab::Yaml,
                InspectorTab::Describe,
                InspectorTab::Events,
            ] {
                panel.update(cx, |panel, cx| panel.show_tab(tab, cx));
                cx.run_until_parked();
                assert_controls_inside(cx, width, &format!("{tab:?}"));
            }
            // And the review, which is the one mode that replaces the tab body rather than sitting
            // inside it.
            panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Yaml, cx));
            cx.simulate_keystrokes("ctrl-a");
            cx.simulate_input(&matching_apply_yaml("uid-1"));
            panel.update(cx, |panel, cx| panel.apply(cx));
            cx.run_until_parked();
            assert_controls_inside(cx, width, "Review");
        }
    }

    /// Every control on screen ends inside the panel, and none of them is a sliver.
    fn assert_controls_inside(cx: &mut gpui_kit::VisualTestContext, width: f32, tab: &str) {
        let frame = cx
            .debug_bounds("inspector-frame")
            .unwrap_or_else(|| panic!("{tab}: the panel frame"));
        let right = f32::from(frame.origin.x + frame.size.width);
        for selector in [
            "inspector-copy-link",
            "inspector-tabs",
            "yaml-action-apply",
            "yaml-action-copy",
            "yaml-review-apply",
            "yaml-review-keep-editing",
            "metrics-action-reload",
            "metrics-range-action-1m",
            "metrics-range-action-15m",
            "metrics-range-action-1h",
            "metrics-range-action-6h",
            "metrics-range-action-24h",
            "metrics-range-action-7d",
        ] {
            let Some(bounds) = cx.debug_bounds(selector) else {
                continue;
            };
            let left = f32::from(bounds.origin.x);
            let end = left + f32::from(bounds.size.width);
            assert!(
                left >= f32::from(frame.origin.x) - 0.5 && end <= right + 0.5,
                "{tab} at {width}px: `{selector}` runs from {left} to {end} and the panel ends \
                 at {right} — it is clipped, and a clipped control cannot be read"
            );
        }
    }

    /// Nothing in the review is allowed to run off the edge of the panel.
    ///
    /// The write row used to hold three controls - keep editing, the optional server check, and
    /// the write - and at the 336px default width they do not fit, so the row overflowed and
    /// clipped its leftmost label in half. The check is not a control any more: the review asks
    /// the server itself, so what is left in the row is the two decisions it exists to put to
    /// the reader, and the verdict is a line of words that wraps.
    #[gpui_kit::test]
    fn the_review_keeps_every_control_inside_the_panel(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, _| panel.set_on_apply(|_| {}));
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input(&matching_apply_yaml("uid-1"));
        panel.update(cx, |panel, cx| panel.apply(cx));
        cx.run_until_parked();

        let frame = cx.debug_bounds("yaml-apply-review").expect("the review");
        let right = frame.origin.x + frame.size.width;
        for control in ["yaml-review-keep-editing", "yaml-review-apply"] {
            let bounds = cx
                .debug_bounds(control)
                .unwrap_or_else(|| panic!("{control} is in the review"));
            assert!(
                bounds.origin.x >= frame.origin.x,
                "{control} starts inside the panel: {:?}",
                bounds.origin
            );
            assert!(
                bounds.origin.x + bounds.size.width <= right,
                "{control} ends inside the panel: {:?} of {:?}",
                bounds.origin.x + bounds.size.width,
                right
            );
        }

        // The verdict is the line a reader reads before deciding, and it has to sit where it is
        // read: above the diff, and clear of the write it qualifies.
        let verdict = cx
            .debug_bounds("yaml-review-check-status")
            .expect("the verdict");
        let write = cx.debug_bounds("yaml-review-apply").expect("the write");
        assert!(
            verdict.origin.y + verdict.size.height <= write.origin.y,
            "the verdict is not in the write row: {verdict:?} against {write:?}"
        );
        assert!(
            verdict.origin.x + verdict.size.width <= right,
            "the verdict wraps inside the panel: {:?} of {:?}",
            verdict.origin.x + verdict.size.width,
            right
        );
    }

    /// `WRITE-OPS.md` §3.1: the semantic diff reports the field, and leaves the noise out.
    ///
    /// Three claims, each of which a line diff gets wrong in a different way: the path is a field
    /// path, the fields the server rewrites are not in it, and a Secret's values never reach the
    /// screen.
    #[gpui_kit::test]
    fn the_review_diff_is_a_field_diff_without_the_servers_own_writes(_cx: &mut TestAppContext) {
        let before = concat!(
            "apiVersion: apps/v1\n",
            "kind: Deployment\n",
            "metadata:\n",
            "  name: api\n",
            "  namespace: prod\n",
            "  resourceVersion: '9001'\n",
            "  managedFields:\n",
            "  - manager: kube-controller-manager\n",
            "spec:\n",
            "  replicas: 3\n",
            "  template:\n",
            "    spec:\n",
            "      containers:\n",
            "      - name: api\n",
            "        image: registry.example.com/api:1.24.3\n",
            "status:\n",
            "  readyReplicas: 3\n"
        );
        let after = before
            .replace("replicas: 3", "replicas: 5")
            .replace("1.24.3", "1.24.4")
            .replace("readyReplicas: 3", "readyReplicas: 5");
        let diff = semantic_diff(before, &after, false).expect("both documents parse");
        let paths = diff
            .changes
            .iter()
            .map(|change| change.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                "spec.replicas",
                "spec.template.spec.containers[name=api].image",
            ],
            "only the two fields a reader cares about, in document order"
        );
        assert!(
            diff.changes[1].path.contains("name=api"),
            "a list of named objects is keyed by its name, not its position: {:?}",
            paths
        );
        assert!(
            !paths.iter().any(|path| path.contains("resourceVersion")),
            "the server's own write is not a change the reader made"
        );
        assert!(
            !paths.iter().any(|path| path.starts_with("status")),
            "status is the server's, not the reader's"
        );
        assert!(
            !paths.iter().any(|path| path.contains("managedFields")),
            "the field-ownership table is not part of the object a reader edits"
        );

        // A Secret's values are masked, and its keys are not: which key changed is the point.
        let secret_before = "apiVersion: v1\nkind: Secret\ndata:\n  DB_PASSWORD: b2xk\n";
        let secret_after = "apiVersion: v1\nkind: Secret\ndata:\n  DB_PASSWORD: bmV3\n";
        let masked = semantic_diff(secret_before, secret_after, true).expect("both parse");
        assert_eq!(masked.changes[0].path, "data.DB_PASSWORD");
        assert_eq!(
            masked.changes[0].after.as_deref(),
            Some("••••••"),
            "a Secret's value never reaches the screen"
        );
    }

    /// `UI-REDESIGN.md` L3: a relationship the panel can name is a way out, and a way out with
    /// no way back is a trap.
    ///
    /// The claim is the whole loop, because each half can be there without the other: the row
    /// activates, the panel shows the object it points at, `Esc` returns, and a selection from
    /// outside ends the trail rather than leaving a "back" that goes somewhere unrelated.
    #[gpui_kit::test]
    fn a_followable_relationship_opens_the_object_and_escape_comes_back(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("inspector-related-back").is_none(),
            "there is nowhere to go back to from the selected row"
        );

        // The Pod fixture's owner carries a uid, so the row is a control rather than a name.
        panel.update(cx, |panel, cx| {
            let target = followable("ReplicaSet", "web-rs", Some("default"), "uid-web-rs")
                .expect("a ReplicaSet in the table names its resource");
            panel.follow_related(target, cx);
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.selection.as_ref().map(|object| object.name.as_str()),
                Some("web-rs"),
                "following a relationship shows the object it points at"
            );
            assert_eq!(panel.related_trail.len(), 1);
        });
        assert!(
            cx.debug_bounds("inspector-related-back").is_some(),
            "a followed object offers its way back"
        );
        // The document belongs to the selected row, so the tab has to say so rather than keep the
        // previous object's text under this object's name.
        assert!(
            cx.debug_bounds("inspector-empty").is_some(),
            "the YAML tab refuses to show another object's document"
        );

        cx.update(|window, cx| {
            let focus = panel.read(cx).focus_handle();
            window.focus(&focus, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            let focus = panel.read(cx).focus_handle();
            window.focus(&focus, cx);
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.selection.as_ref().map(|object| object.uid.as_str()),
                Some("uid-1"),
                "Escape goes back one level"
            );
            assert!(panel.related_trail.is_empty());
        });

        // A row the panel cannot name wears no arrow, so nothing on screen promises a hop.
        assert!(
            followable("ConfigMap", "api-config", Some("default"), "").is_none(),
            "an object with no uid is a name, not a link: following it could land on a different \
             object with the same name"
        );
    }

    /// `UI-SPEC.md` §4.19: the header's chain copies the object's address, in the documented
    /// spelling and with the documented kind alias.
    #[gpui_kit::test]
    fn the_header_copies_a_deep_link_with_the_documented_alias(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("inspector-copy-link").is_some(),
            "the header carries a link control on every object"
        );
        panel.update(cx, |panel, cx| panel.copy_link(cx));
        cx.run_until_parked();
        let link = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("a link on the clipboard");
        let expected = panel.read_with(cx, |panel, _| {
            let selection = panel.selection.clone().expect("a selection");
            let cluster = panel
                .session
                .cluster_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "cluster".to_owned());
            deep_link(&cluster, &selection)
        });
        assert_eq!(link, expected);
        assert!(
            link.starts_with("k8s-gpui://") && link.contains("/po/"),
            "the link is the documented form and the kind is the documented alias: {link}"
        );
        assert_eq!(
            kind_link_alias("Deployment"),
            "deploy",
            "§4.19 lists the alias, and it is the one a person types"
        );
    }

    // The text the cluster reported stays recoverable after a write.
    #[gpui_kit::test]
    fn revert_after_an_apply_restores_the_text_the_cluster_reported(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, _| panel.set_on_apply(|_| {}));
        let server_text = panel.read_with(cx, |panel, _| panel.original.clone());
        let server_text = server_text.expect("the selection text");
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input(&matching_apply_yaml("uid-1"));
        panel.update(cx, |panel, cx| panel.apply(cx));
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        assert!(!dirty(&panel, cx));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.original.clone()),
            Some(server_text.clone()),
            "an apply must not overwrite the text the cluster reported"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.applied_text().map(str::to_owned)),
            Some(matching_apply_yaml("uid-1")),
            "the applied text is kept beside it"
        );
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("yaml-action-cancel").is_some(),
            "the way back stays available after the editor is marked saved"
        );
        panel.update(cx, |panel, cx| panel.revert(cx));
        assert_eq!(yaml_text(&panel, cx), server_text);
    }

    // The Metrics tab is hidden for a reason, and the fallback says which one.
    #[gpui_kit::test]
    fn a_hidden_metrics_tab_explains_itself(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        assert!(
            panel.read_with(cx, |panel, _| !panel.metrics_tab_visible()),
            "the fixture has no metrics source"
        );
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Metrics, cx));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.active_tab()),
            DEFAULT_TAB_ENUM,
            "a tab that cannot exist falls back to the reading order's first tab"
        );
        let notice = panel
            .read_with(cx, |panel, _| panel.metrics_notice.clone())
            .expect("a reason for the missing tab");
        assert!(
            notice.contains("metrics API"),
            "the fallback must name the cause: {notice}"
        );
    }

    #[gpui_kit::test]
    fn describe_and_events_read_one_cache_so_the_tabs_cannot_disagree(cx: &mut TestAppContext) {
        let (panel, source, cx) = source_setup(cx);
        // The Describe body ends in the newest events, and RELATED's event row counts the same
        // list, so the tab a reader lands on needs the events too. One request answers both: it
        // is the Describe request that pulls them, and the Events tab below must not ask again.
        select(&panel, "uid-1", cx);
        assert_eq!(source.describes.load(Ordering::Relaxed), 1);
        assert_eq!(source.events.load(Ordering::Relaxed), 1);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Events, cx));
        cx.run_until_parked();
        assert_eq!(
            source.events.load(Ordering::Relaxed),
            1,
            "the Events tab reads the cache Describe filled"
        );
        panel.read_with(cx, |panel, _| {
            assert!(matches!(
                panel.events_states.get("uid-1").map(|entry| &entry.state),
                Some(LoadState::Ready(_))
            ));
        });
    }

    #[gpui_kit::test]
    fn large_event_result_uses_the_uniform_list_handle(cx: &mut TestAppContext) {
        let (panel, _source, cx) = events_source_setup(cx, 2_000);
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Events, cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("inspector-events-list").is_some());
        panel.read_with(cx, |panel, _| {
            assert!(panel.events_scroll.0.borrow().last_item_size.is_some());
            assert!(matches!(
                panel.events_states.get("uid-1").map(|entry| &entry.state),
                Some(LoadState::Ready(events)) if events.len() == 2_000
            ));
        });
    }

    #[gpui_kit::test]
    fn metrics_retry_state_resets_when_hidden_shown_or_target_changes(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let metrics_runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("metrics runtime");
        let metrics = MetricsHandle::new(
            metrics_runtime.handle().clone(),
            Arc::new(EmptyMetricsPort) as Arc<dyn ClusterDataPort>,
        );
        panel.update(cx, |panel, cx| {
            panel.set_metrics_source(Some(metrics), MetricsProbeState::Available, cx);
            panel.set_metrics_visible(true, cx);
            panel.show_tab(InspectorTab::Metrics, cx);
            assert!(panel.metrics_should_sample());
            // Becoming visible queues one immediate sample. This test owns the clock, so it
            // has to collect that decision before the backoff can be observed.
            assert_eq!(
                panel.metrics_scheduler.decide(Duration::ZERO),
                SampleDecision::SampleNow
            );

            panel.record_metrics_sample(Err("cluster unavailable".to_owned()), cx);
            assert_eq!(
                retry_delay_text(&panel.metrics_scheduler).as_deref(),
                Some("Retrying in 11 seconds")
            );
            assert_eq!(
                panel.metrics_scheduler.decide(Duration::ZERO),
                SampleDecision::Wait(Duration::from_secs(11))
            );

            panel.set_metrics_visible(false, cx);
            assert_eq!(retry_delay_text(&panel.metrics_scheduler), None);
            panel.set_metrics_visible(true, cx);
            assert_eq!(retry_delay_text(&panel.metrics_scheduler), None);
            assert_eq!(
                panel.metrics_scheduler.decide(Duration::ZERO),
                SampleDecision::SampleNow
            );

            panel.record_metrics_sample(Err("cluster unavailable".to_owned()), cx);
            assert!(retry_delay_text(&panel.metrics_scheduler).is_some());
            let mut object = pod_ref("uid-2");
            object.name = "web-1".to_owned();
            panel.set_selection(
                Some(InspectorSelection {
                    object,
                    yaml: "kind: Pod".to_owned(),
                }),
                cx,
            );
            assert_eq!(retry_delay_text(&panel.metrics_scheduler), None);
            assert_eq!(
                panel.metrics_scheduler.decide(Duration::ZERO),
                SampleDecision::SampleNow
            );
        });
    }

    #[gpui_kit::test]
    fn apply_is_blocked_while_a_selection_is_pending(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        select(&panel, "uid-1", cx);
        let requests: Rc<RefCell<Vec<ApplyRequest>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = requests.clone();
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(move |request: ApplyRequest, _| {
                sink.borrow_mut().push(request);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        let yaml = format!("{}\n", matching_apply_yaml("uid-1"));
        cx.simulate_input(&yaml);
        let pending = ObjectRef {
            name: "web-1".to_owned(),
            ..pod_ref("uid-2")
        };
        // The deferred selection carries the YAML of the object it navigates to, so the
        // object and its document must agree on the name.
        let pending_yaml = format!("{}\n", matching_apply_yaml_for("uid-2", &pending.name));
        panel.update(cx, |panel, cx| {
            panel.set_selection(
                Some(InspectorSelection {
                    object: pending,
                    yaml: pending_yaml.clone(),
                }),
                cx,
            );
        });
        cx.run_until_parked();

        panel.update(cx, |panel, cx| panel.apply(cx));
        assert!(
            requests.borrow().is_empty(),
            "Apply must not write to the object the user navigated away from"
        );
        assert!(
            panel.read_with(cx, |panel, _| panel.reviewed_change().is_none()),
            "A blocked change never reaches a review"
        );
        panel.read_with(cx, |panel, _| {
            let (title, reason) = panel
                .apply_blocked_by_pending()
                .expect("a pending selection blocks Apply");
            assert_eq!(title, "Apply paused");
            assert!(
                reason.contains("web-0"),
                "The message must name the object the YAML belongs to: {reason}"
            );
        });

        // Cancel resolves the state: the pending object loads and Apply targets it again.
        panel.update(cx, |panel, cx| panel.revert(cx));
        cx.run_until_parked();
        assert!(!panel.read_with(cx, |panel, _| panel.has_pending()));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.apply_target().unwrap().uid),
            "uid-2"
        );
        // Dirty means the document differs from the text the cluster reported, so retyping the
        // same bytes is not a change and Apply has nothing to send. The comment is one.
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input(&format!("# edited\n{pending_yaml}"));
        assert!(dirty(&panel, cx));
        panel.update(cx, |panel, cx| panel.apply(cx));
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        assert_eq!(requests.borrow().len(), 1);
        assert_eq!(requests.borrow()[0].target.uid, "uid-2");
        assert_eq!(requests.borrow()[0].target.name, "web-1");
    }

    /// A disabled Apply has to name the state the reader is actually in. An apply in flight used
    /// to fall through to the "review the change" copy, which was wrong twice over: no review is
    /// open, and the document is read-only until the request comes back.
    ///
    /// The ordering is the part worth pinning. The document here is dirty and there is no parse
    /// problem, so every other reason is false at once and only the precedence can answer; and
    /// a selection that arrives mid-flight is the one state that outlives the request, so it
    /// takes over from the in-flight copy.
    #[gpui_kit::test]
    fn an_apply_in_flight_says_so_instead_of_asking_for_a_review(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        select(&panel, "uid-1", cx);
        // A handler that never reports back is an apply in flight.
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(|_request: ApplyRequest, _| {});
        });
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input(&matching_apply_yaml("uid-1"));
        panel.update(cx, |panel, cx| panel.apply(cx));
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        assert!(panel.read_with(cx, |panel, _| panel.is_applying()));
        assert!(dirty(&panel, cx));
        panel.read_with(cx, |panel, cx| {
            assert_eq!(
                panel.apply_block_reason(panel.diagnostics(cx).len(), cx),
                Some(APPLY_IN_FLIGHT_REASON.to_owned()),
                "a request is on the wire, so that is what the button explains"
            );
        });

        panel.update(cx, |panel, cx| {
            panel.set_selection(
                Some(InspectorSelection {
                    object: ObjectRef {
                        name: "web-1".to_owned(),
                        ..pod_ref("uid-2")
                    },
                    yaml: "b: 3".to_owned(),
                }),
                cx,
            );
        });
        panel.read_with(cx, |panel, cx| {
            let (_, deferred) = panel
                .apply_blocked_by_pending()
                .expect("the unapplied edit defers the switch");
            assert_eq!(
                panel.apply_block_reason(panel.diagnostics(cx).len(), cx),
                Some(deferred),
                "the object the reader has to come back to outranks the request in flight"
            );
        });
    }

    #[gpui_kit::test]
    fn pending_apply_button_is_disabled(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        select(&panel, "uid-1", cx);
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input(&matching_apply_yaml("uid-1"));
        cx.run_until_parked();
        let enabled = cx.debug_bounds("yaml-action-apply").expect("Apply button");
        assert!(f32::from(enabled.size.width) > 0.0);

        panel.update(cx, |panel, cx| {
            panel.set_selection(
                Some(InspectorSelection {
                    object: ObjectRef {
                        name: "web-1".to_owned(),
                        ..pod_ref("uid-2")
                    },
                    yaml: matching_apply_yaml("uid-2"),
                }),
                cx,
            );
        });
        cx.run_until_parked();
        let disabled = cx
            .debug_bounds("yaml-action-apply")
            .expect("Apply button stays in place");
        assert_eq!(
            disabled.size.width, enabled.size.width,
            "A disabled Apply must not resize the toolbar"
        );
        assert!(
            cx.debug_bounds("yaml-clean-metadata").is_none(),
            "The pending state replaces the clean metadata with a status"
        );
    }

    #[gpui_kit::test]
    fn undo_to_the_saved_text_loads_the_pending_selection(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        select(&panel, "uid-1", cx);
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("a: 2");
        assert!(dirty(&panel, cx));
        panel.update(cx, |panel, cx| {
            panel.set_selection(
                Some(InspectorSelection {
                    object: ObjectRef {
                        name: "web-1".to_owned(),
                        ..pod_ref("uid-2")
                    },
                    yaml: "b: 3".to_owned(),
                }),
                cx,
            );
        });
        cx.run_until_parked();
        assert!(panel.read_with(cx, |panel, _| panel.has_pending()));

        // gpui-base records a keystroke that replaced a selection as its own atomic
        // transaction, so undoing a select-all and replace takes two presses.
        cx.simulate_keystrokes("ctrl-z ctrl-z");
        cx.run_until_parked();
        assert!(
            !panel.read_with(cx, |panel, _| panel.has_pending()),
            "Undoing back to the saved text resolves the pending selection"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.selection().unwrap().uid.clone()),
            "uid-2"
        );
        assert_eq!(yaml_text(&panel, cx), "b: 3");
    }

    #[gpui_kit::test]
    fn a_repeated_selection_update_keeps_the_caret(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "alpha: 1\nbeta: 2");
        select(&panel, "uid-1", cx);
        assert!(!dirty(&panel, cx));
        panel.update(cx, |panel, cx| {
            panel
                .yaml_view
                .update(cx, |view, cx| view.place_caret(4, 8, cx));
        });
        let before = panel.read_with(cx, |panel, cx| panel.yaml_view.read(cx).caret(cx));
        assert_eq!(before, (8, 4));

        // Same object, same content: the caret and selection stay put.
        select(&panel, "uid-1", cx);
        assert_eq!(
            panel.read_with(cx, |panel, cx| panel.yaml_view.read(cx).caret(cx)),
            before,
            "An identical selection update must not reset the selection"
        );

        // Same object, new content: a live update keeps the caret instead of jumping home.
        let updated = "kind: Pod\nmetadata: {}\nstatus: {}".to_owned();
        panel.update(cx, |panel, cx| {
            panel.set_selection(
                Some(InspectorSelection {
                    object: pod_ref("uid-1"),
                    yaml: updated.clone(),
                }),
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, cx| panel.yaml_view.read(cx).caret(cx)),
            before,
            "A live update must not move the caret"
        );
        assert_eq!(yaml_text(&panel, cx), updated);
        assert!(!dirty(&panel, cx));
    }

    #[gpui_kit::test]
    fn returning_to_a_previous_object_reloads_it(cx: &mut TestAppContext) {
        let (panel, source, cx) = source_setup(cx);
        let selection = |uid: &str| {
            Some(InspectorSelection {
                object: pod_ref(uid),
                yaml: "kind: Pod".to_owned(),
            })
        };
        // A is handed over and nothing is allowed to answer yet, so its request is the
        // outstanding one this test is about. `select` would park and let it land.
        panel.update(cx, |panel, cx| panel.set_selection(selection("uid-1"), cx));
        assert_eq!(source.describes.load(Ordering::Relaxed), 1);

        // Leave A while its request is still in flight, then come back.
        panel.update(cx, |panel, cx| panel.set_selection(selection("uid-2"), cx));
        panel.update(cx, |panel, cx| panel.set_selection(selection("uid-1"), cx));
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        assert_eq!(
            source.describes.load(Ordering::Relaxed),
            3,
            "A must load again instead of waiting for a request that was dropped"
        );
        panel.read_with(cx, |panel, _| {
            assert!(matches!(
                panel.describe_states.get("uid-1"),
                Some(LoadState::Ready(_))
            ));
        });
    }

    #[gpui_kit::test]
    fn pod_conditions_and_all_container_groups_are_listed(cx: &mut TestAppContext) {
        init_app(cx);
        let object = Arc::new(
            serde_json::from_value(json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": { "name": "web-0", "namespace": "default", "uid": "uid-1" },
                "spec": {
                    "initContainers": [{
                        "name": "migrate", "image": "app:1",
                    }],
                    "containers": [{ "name": "app", "image": "app:1" }],
                    "ephemeralContainers": [{ "name": "debugger", "image": "busybox:1" }],
                },
                "status": {
                    "conditions": [
                        { "type": "Ready", "status": "True", "reason": "ContainersReady" },
                        { "type": "PodReadyToStartContainers", "status": "False" },
                    ],
                    "initContainerStatuses": [{
                        "name": "migrate", "ready": true, "restartCount": 0,
                        "state": { "terminated": { "exitCode": 0, "reason": "Completed" } },
                    }],
                    "containerStatuses": [{
                        "name": "app", "ready": false, "restartCount": 3,
                        "state": { "waiting": { "reason": "CrashLoopBackOff" } },
                    }],
                    "ephemeralContainerStatuses": [{
                        "name": "debugger", "ready": true, "restartCount": 0,
                        "state": { "running": {} },
                    }],
                },
            }))
            .expect("pod"),
        );
        let (panel, cx) = cx.add_window_view(|_, cx| InspectorPanel::new(cx));
        let source = Arc::new(FakeSource {
            describes: AtomicUsize::new(0),
            events: AtomicUsize::new(0),
            object: Some(object),
            event_count: 0,
        });
        panel.update(cx, |panel, _| {
            panel.set_source(source.clone() as Arc<dyn InspectorSource>);
        });
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        open_all_sections(&panel, cx);

        for selector in [
            "inspector-describe-container-app",
            "inspector-describe-container-migrate-init",
            "inspector-describe-container-debugger",
            "inspector-describe-condition-Ready",
            "inspector-describe-condition-PodReadyToStartContainers",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "Describe must list {selector}"
            );
        }
    }

    #[gpui_kit::test]
    fn metrics_toolbar_actions_stay_inside_a_narrow_inspector(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let metrics_runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("metrics runtime");
        let metrics = MetricsHandle::new(
            metrics_runtime.handle().clone(),
            Arc::new(EmptyMetricsPort) as Arc<dyn ClusterDataPort>,
        );
        panel.update(cx, |panel, cx| {
            panel.set_metrics_source(Some(metrics), MetricsProbeState::Missing, cx);
            panel.set_metrics_visible(true, cx);
            panel.show_tab(InspectorTab::Metrics, cx);
        });
        cx.run_until_parked();
        cx.simulate_resize(gpui_kit::size(gpui_kit::px(240.), gpui_kit::px(640.)));
        cx.run_until_parked();
        cx.run_until_parked();
        assert_toolbar_actions(
            cx,
            "metrics-context-toolbar",
            &[
                "metrics-action-reload",
                "metrics-range-action-1m",
                "metrics-range-action-15m",
                "metrics-range-action-1h",
            ],
        );
    }

    #[gpui_kit::test]
    fn metrics_range_and_retry_commands_change_the_panel(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let metrics_runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("metrics runtime");
        let metrics = MetricsHandle::new(
            metrics_runtime.handle().clone(),
            Arc::new(EmptyMetricsPort) as Arc<dyn ClusterDataPort>,
        );
        panel.update(cx, |panel, cx| {
            panel.set_metrics_source(Some(metrics), MetricsProbeState::Missing, cx);
            panel.set_metrics_visible(true, cx);
            panel.show_tab(InspectorTab::Metrics, cx);
        });
        cx.run_until_parked();
        // The Metrics toolbar binds these actions to the panel handle. The YAML editor
        // owned the focus in `setup`, and it is unmounted on this tab, so a window
        // dispatch would find no node to route the action from.
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.focus_handle().focus(window, cx));
        });
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.metrics_range_ms),
            DEFAULT_RANGE_MS
        );
        cx.update(|window, cx| window.dispatch_action(Box::new(MetricsRange1h), cx));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.metrics_range_ms),
            60 * 60 * 1000,
            "The 1h command must reach the panel"
        );
        cx.update(|window, cx| window.dispatch_action(Box::new(MetricsRange15m), cx));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.metrics_range_ms),
            15 * 60 * 1000
        );
        cx.update(|window, cx| window.dispatch_action(Box::new(ReloadActiveTab), cx));
        cx.run_until_parked();

        // Retry without a probe owner must not leave the panel stuck in Checking.
        panel.update(cx, |panel, cx| {
            panel.set_metrics_probe_retry_handler(|_| {});
            panel.set_metrics_probe_state(MetricsProbeState::Missing, cx);
        });
        cx.run_until_parked();
        panel.update(cx, |panel, cx| {
            panel.metrics_probe_retry = None;
            panel.set_metrics_source(
                Some(MetricsHandle::new(
                    metrics_runtime.handle().clone(),
                    Arc::new(EmptyMetricsPort) as Arc<dyn ClusterDataPort>,
                )),
                MetricsProbeState::Missing,
                cx,
            );
            panel.retry_metrics(cx);
        });
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.metrics_probe_task.is_some(),
                "Retry must run its own probe instead of waiting for an owner"
            );
        });
    }

    #[gpui_kit::test]
    fn metrics_probe_state_keeps_error_reason_when_unavailable_arrives(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        panel.update(cx, |panel, cx| {
            panel.set_metrics_probe_state(
                MetricsProbeState::Error {
                    reason: "connection refused".to_owned(),
                },
                cx,
            );
        });
        panel.read_with(cx, |panel, _| {
            assert!(matches!(
                panel.metrics_probe,
                MetricsProbeState::Error { ref reason } if reason == "connection refused"
            ));
            assert_eq!(
                panel.metrics.last_error.as_deref(),
                Some("connection refused")
            );
        });
        panel.update(cx, |panel, cx| panel.set_metrics_available(false, cx));
        panel.read_with(cx, |panel, _| {
            assert!(matches!(
                panel.metrics_probe,
                MetricsProbeState::Error { .. }
            ));
        });
        panel.update(cx, |panel, cx| {
            panel.set_metrics_probe_state(MetricsProbeState::Missing, cx);
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.metrics_probe, MetricsProbeState::Missing);
        });
    }

    // A `Loading` entry is cached so a second visit does not start a duplicate request, so
    // a request that never resolves must not pin the tab in a spinner.
    #[gpui_kit::test]
    fn a_describe_that_never_answers_offers_retry(cx: &mut TestAppContext) {
        let (panel, source, cx) = hanging_setup(cx, false);
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        assert_eq!(source.describes.load(Ordering::Relaxed), 1);
        panel.read_with(cx, |panel, _| {
            assert!(matches!(
                panel.describe_states.get("uid-1"),
                Some(LoadState::Loading { .. })
            ));
        });

        cx.executor().advance_clock(LOAD_DEADLINE);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                matches!(
                    panel.describe_states.get("uid-1"),
                    Some(LoadState::Failed(reason)) if reason == LOAD_TIMEOUT_REASON
                ),
                "a stuck load becomes a retryable failure"
            );
            assert_eq!(
                load_failure_hint(LOAD_TIMEOUT_REASON),
                "The cluster is slow to answer. Retry, or check the cluster connection."
            );
            assert!(
                panel.describe_task.is_none(),
                "the stuck request is dropped so Retry starts a new one"
            );
        });
        assert!(
            cx.debug_bounds("inspector-load-error").is_some(),
            "the tab shows the failure with a next step"
        );

        // Retry asks the source again.
        panel.update(cx, |panel, cx| panel.ensure_describe(true, cx));
        cx.run_until_parked();
        assert_eq!(source.describes.load(Ordering::Relaxed), 2);
    }

    // The deadline must not expire data that already arrived.
    #[gpui_kit::test]
    fn a_loaded_describe_survives_the_load_deadline(cx: &mut TestAppContext) {
        let (panel, _source, cx) = source_setup(cx);
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        cx.executor().advance_clock(LOAD_DEADLINE * 3);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(matches!(
                panel.describe_states.get("uid-1"),
                Some(LoadState::Ready(_))
            ));
        });
    }

    // Events are secondary: a failure is recoverable and leaves the rest of the panel
    // working.
    #[gpui_kit::test]
    fn a_failing_events_load_is_best_effort_and_retryable(cx: &mut TestAppContext) {
        let (panel, source, cx) = hanging_setup(cx, true);
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Events, cx));
        cx.run_until_parked();
        assert_eq!(source.events.load(Ordering::Relaxed), 1);
        panel.read_with(cx, |panel, _| {
            assert!(matches!(
                panel.events_states.get("uid-1"),
                Some(EventsEntry {
                    state: LoadState::Failed(_),
                    ..
                })
            ));
        });
        assert!(
            cx.debug_bounds("inspector-load-error").is_some(),
            "the Events tab explains the failure"
        );

        // The object data is unaffected, so the other tabs keep working.
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Yaml, cx));
        cx.run_until_parked();
        assert_eq!(yaml_text(&panel, cx), "kind: Pod");
        assert!(panel.read_with(cx, |panel, _| panel.selection().is_some()));

        // Retry asks the source again.
        panel.update(cx, |panel, cx| panel.ensure_events(true, cx));
        cx.run_until_parked();
        assert_eq!(source.events.load(Ordering::Relaxed), 2);
    }

    #[gpui_kit::test]
    fn a_hanging_events_load_offers_retry(cx: &mut TestAppContext) {
        let (panel, _source, cx) = hanging_setup(cx, false);
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Events, cx));
        cx.run_until_parked();
        cx.executor().advance_clock(LOAD_DEADLINE);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                matches!(
                    panel.events_states.get("uid-1"),
                    Some(EventsEntry { state: LoadState::Failed(reason), .. }) if reason == LOAD_TIMEOUT_REASON
                ),
                "a stuck event load becomes a retryable failure"
            );
        });
    }

    // The Reload command must reach the toolbar of the tab that is open, and it has to
    // bypass the cache.
    #[gpui_kit::test]
    fn the_reload_command_bypasses_the_describe_and_events_cache(cx: &mut TestAppContext) {
        let (panel, source, cx) = source_setup(cx);
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        assert_eq!(source.describes.load(Ordering::Relaxed), 1);
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.focus_handle().focus(window, cx));
        });
        cx.update(|window, cx| window.dispatch_action(Box::new(ReloadActiveTab), cx));
        cx.run_until_parked();
        assert_eq!(
            source.describes.load(Ordering::Relaxed),
            2,
            "Reload asks for the object again"
        );

        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Events, cx));
        cx.run_until_parked();
        assert_eq!(source.events.load(Ordering::Relaxed), 1);
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.focus_handle().focus(window, cx));
        });
        cx.update(|window, cx| window.dispatch_action(Box::new(ReloadActiveTab), cx));
        cx.run_until_parked();
        assert_eq!(
            source.events.load(Ordering::Relaxed),
            2,
            "Reload asks for the events again"
        );
    }

    // The Metrics target carries the UID, so a pod recreated under the same name starts
    // an empty series instead of charting its predecessor.
    #[gpui_kit::test]
    fn a_recreated_object_restarts_the_metrics_series(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let metrics_runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("metrics runtime");
        let metrics = MetricsHandle::new(
            metrics_runtime.handle().clone(),
            Arc::new(EmptyMetricsPort) as Arc<dyn ClusterDataPort>,
        );
        panel.update(cx, |panel, cx| {
            panel.set_metrics_source(Some(metrics), MetricsProbeState::Available, cx);
            panel.set_metrics_visible(true, cx);
            panel.show_tab(InspectorTab::Metrics, cx);
            panel.record_metrics_sample(
                Ok(SamplePayload {
                    containers: vec![super::super::metrics::ContainerSample {
                        name: "app".to_owned(),
                        cpu_millicores: Some(100.0),
                        memory_bytes: Some(1024.0),
                    }],
                    window: "10s".to_owned(),
                    at_ms: 1_000,
                }),
                cx,
            );
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.metrics.series.len(), 1);
            assert_eq!(
                panel.metrics_target.as_ref().map(|target| target.uid()),
                Some("uid-1")
            );
        });

        // Same name, new UID: the samples of the old pod must not survive.
        panel.update(cx, |panel, cx| {
            panel.set_selection(
                Some(InspectorSelection {
                    object: pod_ref("uid-2"),
                    yaml: "kind: Pod".to_owned(),
                }),
                cx,
            );
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.metrics_target.as_ref().map(|target| target.uid()),
                Some("uid-2")
            );
            assert!(
                panel.metrics.is_empty(),
                "a recreated pod starts without the old samples"
            );
        });
    }

    // ── The structure `UI-REDESIGN.md` §3.4 fixes ──────────────────────────────────
    //
    // The three claims below are the ones a reader would notice immediately if they broke, and
    // none of them is visible in a compile: Status being first and always open, the values
    // lining up in a column, and the section heading carrying a chevron and a count instead of a
    // rule. Each is a thing a later edit can undo without any test going red.

    /// `§3.4`: Status is first, and it is the one section that cannot be closed.
    #[gpui_kit::test]
    fn status_is_the_first_section_and_cannot_be_collapsed(cx: &mut TestAppContext) {
        let (panel, _source, cx) = layout_source_setup(cx);
        select(&panel, LAYOUT_UID, cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("inspector-describe-section-Status")
                .is_some(),
            "an object with a status renders it, and it renders before anything else"
        );
        assert!(
            cx.debug_bounds("inspector-describe-section-Labels")
                .is_some(),
            "a closed section still shows its heading: the count is what tells the reader what \
             they would get by opening it"
        );
        assert!(
            cx.debug_bounds("inspector-describe-field-app-value")
                .is_none(),
            "the sections below Status are on request, so they start closed"
        );
        assert!(
            cx.debug_bounds("inspector-describe-field-configHash-value")
                .is_some(),
            "Status is open without being asked, because it is the block the panel exists for"
        );
        // Status's heading is a block, not a button, so there is nothing to press.
        assert!(
            !panel.read_with(cx, |panel, _| panel
                .open_sections
                .contains(&DetailSection::Status)
                && panel.section_is_collapsible(DetailSection::Status)),
            "Status is the answer the panel exists for; a section you can close is one you can lose"
        );
    }

    /// `§3.4`: the key column is a grid, not a flex that floats. Two values in different
    /// sections have to start at the same x, which is the entire reason the column exists.
    #[gpui_kit::test]
    fn values_line_up_in_a_column_across_sections(cx: &mut TestAppContext) {
        let (panel, _source, cx) = layout_source_setup(cx);
        select(&panel, LAYOUT_UID, cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        open_all_sections(&panel, cx);
        cx.simulate_resize(gpui_kit::size(gpui_kit::px(480.0), gpui_kit::px(640.0)));
        cx.run_until_parked();
        assert!(
            !panel.read_with(cx, |panel, _| panel.describe_stacked()),
            "480px is the width the two-column grid is for"
        );

        // One value from Status, one from Spec, and one long enough to wrap: three sections and
        // two layouts, one column. If the key ever goes back to a flex, or the wrapped layout
        // stops being indented to the grid, these three drift apart.
        let inline = f32::from(
            cx.debug_bounds("inspector-describe-field-configHash-value-text")
                .expect("the Status value's text")
                .origin
                .x,
        );
        let other_section = f32::from(
            cx.debug_bounds("inspector-describe-field-app-value-text")
                .expect("the Spec value's text")
                .origin
                .x,
        );
        let wrapped = f32::from(
            cx.debug_bounds("inspector-container-image-api-value-wrapped-text")
                .expect("the wrapped value's text")
                .origin
                .x,
        );
        assert!(
            (inline - other_section).abs() <= 0.5,
            "two sections' values start at the same x: {inline} against {other_section}"
        );
        assert!(
            (inline - wrapped).abs() <= 0.5,
            "a wrapped value starts at the same x as a one-line value: {inline} against {wrapped}. \
             The wrapped layout indents to the value column precisely so the column survives a \
             value that has to break across lines."
        );
    }

    /// `§3.4`: the heading carries a chevron and a count, and there is no full-width rule under
    /// it. The rule is the part worth pinning — a `Separator` here is one line of code and reads
    /// as a bug at every width.
    #[gpui_kit::test]
    fn a_section_heading_is_a_chevron_a_caption_and_a_count(cx: &mut TestAppContext) {
        let (panel, _source, cx) = layout_source_setup(cx);
        select(&panel, LAYOUT_UID, cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        open_all_sections(&panel, cx);

        let head = cx
            .debug_bounds("inspector-section-head-Labels")
            .expect("a section heading");
        assert_eq!(
            f32::from(head.size.height),
            f32::from(design::size::ROW_DENSE),
            "the heading is the 24px band, not a line of text"
        );
        let dash = cx
            .debug_bounds("inspector-section-dash")
            .expect("the short dash a heading wears");
        assert_eq!(
            f32::from(dash.size.width),
            f32::from(SECTION_DASH_WIDTH),
            "`§3.4` fixes the dash at 48px: short enough not to read as a divider"
        );
        // A full-width rule would reach the panel edge. The dash stops well short of it, and
        // that difference is the whole point of the change.
        assert!(
            f32::from(dash.size.width) < f32::from(head.size.width) / 2.,
            "the dash is a mark after the title, not a rule under it: {:?} against {:?}",
            dash.size.width,
            head.size.width
        );
    }

    /// `§13.3`: a field the controller owns is drawn quieter, with a lock, because editing it is
    /// undone. This is the one place a managed-field value is content rather than a control state,
    /// so it is worth a test that the plumbing actually reaches a row. `managed_ink` says which
    /// role it uses and why.
    #[gpui_kit::test]
    fn a_managed_field_is_marked_as_one(_cx: &mut TestAppContext) {
        let managed = ManagedFields {
            paths: BTreeSet::from(["spec".to_owned(), "status.phase".to_owned()]),
            known: true,
        };
        assert!(
            managed.owns("spec"),
            "a manager that owns `spec` owns it whole"
        );
        assert!(
            managed.owns("spec.containers[0].image"),
            "a prefix counts: the row is under a field the manager owns"
        );
        assert!(managed.owns("status.phase"));
        assert!(
            !managed.owns("metadata.name"),
            "an unmanaged path stays editable"
        );

        // An object with no managers at all is not an object whose every field is managed. The
        // cluster simply has server-side apply off, and locking everything would be a worse lie
        // than locking nothing.
        let none = ManagedFields::from_object(&pod_object("web-0", "uid-1"));
        assert!(!none.known && !none.owns("spec.containers[0].image"));
    }

    /// `WRITE-OPS.md` §3.3: the diff is a mode, not a strip. The claim that matters is the one a
    /// reader would notice going wrong — the tab strip is *gone* while the review is open, so the
    /// panel is not showing two different things at once.
    #[gpui_kit::test]
    fn a_review_takes_over_the_panel_rather_than_squeezing_the_editor(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, _| panel.set_on_apply(|_| {}));
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input(&matching_apply_yaml("uid-1"));
        panel.update(cx, |panel, cx| panel.apply(cx));
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("inspector-diff-header").is_some(),
            "the review replaces the tab strip with a Diff header"
        );
        assert!(
            cx.debug_bounds("inspector-tabs").is_none(),
            "a tab strip still showing YAML while the panel shows a diff is two answers at once"
        );
        let review = cx
            .debug_bounds("yaml-apply-review")
            .expect("the review fills the content region");
        assert!(
            review.size.height > px(120.),
            "the review is the panel, not a strip: {:?}",
            review.size.height
        );

        // Esc is the way back, and it must not cost the reader their edits.
        panel.update(cx, |panel, cx| panel.cancel_pending_apply(cx));
        cx.run_until_parked();
        assert!(
            dirty(&panel, cx),
            "Esc returns to Detail and keeps the unsaved edits"
        );
        assert!(
            cx.debug_bounds("inspector-tabs").is_some(),
            "and the tab strip comes back with it"
        );
    }

    /// The overlay mode is a claim about width, and a claim about width is only worth anything
    /// if it is measured rather than assumed.
    #[gpui_kit::test]
    fn a_narrow_inspector_draws_as_an_overlay(cx: &mut TestAppContext) {
        let (panel, _source, cx) = layout_source_setup(cx);
        select(&panel, LAYOUT_UID, cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.simulate_resize(gpui_kit::size(gpui_kit::px(352.0), gpui_kit::px(640.0)));
        cx.run_until_parked();
        assert!(
            !panel.read_with(cx, |panel, _| panel.overlay_frame()),
            "the 352px default is a docked column, not an overlay"
        );

        cx.simulate_resize(gpui_kit::size(gpui_kit::px(280.0), gpui_kit::px(640.0)));
        cx.run_until_parked();
        assert!(
            panel.read_with(cx, |panel, _| panel.overlay_frame()),
            "`§3.4`: below 320px the Inspector floats instead of docking"
        );
    }

    /// A header action with no handler behind it is the worst thing a tool can draw, so the row
    /// draws one button per wired action and nothing at all when none are wired.
    #[gpui_kit::test]
    fn a_header_action_is_only_drawn_when_something_can_run_it(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("inspector-actions").is_none(),
            "no handler, no row: a dead button is worse than no button"
        );

        let ran = Rc::new(RefCell::new(0usize));
        let sink = ran.clone();
        panel.update(cx, |panel, cx| {
            panel.set_object_actions(
                vec![ObjectAction::new(
                    "logs",
                    "Logs",
                    IconName::FileText,
                    "Open the logs for this pod",
                    None,
                    false,
                    move |_| *sink.borrow_mut() += 1,
                )],
                cx,
            );
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("inspector-action-logs").is_some());
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.object_action_ids()),
            vec!["logs"]
        );
        let button = cx
            .debug_bounds("inspector-action-logs")
            .expect("the action");
        cx.simulate_click(button.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(ran.borrow().clone(), 1, "the wired action runs");
    }

    /// `UI-SPEC.md` §8's "both appearances" checklist, for the roles this panel actually draws.
    ///
    /// Light is where a restrained palette fails: a tertiary ink that reads as a deliberate
    /// quiet step in dark is a light grey on white in light, and nothing in a headless test
    /// notices until a person squints at it. The panel leans on `fg_tertiary` for every key,
    /// every count and every section heading, so the floors it has to clear are the ones a
    /// secondary role is held to, and §8 gives `tertiary` the weaker 3:1 floor rather than the
    /// 4.5:1 body-text floor.
    ///
    /// The readings this test enforces are measured, not quoted: the theme's derivation layer solves
    /// every role against the floor the design gives it, so the check here is that the panel's own
    /// surface and the panel's own set of roles agree with that promise. Two of them are the ones
    /// the code reasons about in prose, and both are printed on the way past so a change to either
    /// the theme or the floor leaves a number in the log next to the failure rather than a comment
    /// that has quietly gone untrue: `fg_tertiary` reads 4.14:1 light and 4.04:1 dark, and
    /// `fg_disabled` — a role this panel deliberately does not draw, see `managed_ink` — reads
    /// 3.61:1 and 3.52:1, i.e. it now clears `DISABLED_TEXT_MIN_CONTRAST` too. Light is the
    /// tighter appearance for both, which is why the loop starts there.
    #[gpui_kit::test]
    fn every_role_the_inspector_draws_clears_its_floor_in_both_appearances(
        cx: &mut TestAppContext,
    ) {
        for appearance in [design::Appearance::Light, design::Appearance::Dark] {
            init_app(cx);
            let (panel, cx) = cx.add_window_view(|_, cx| InspectorPanel::new(cx));
            cx.update(|_, cx| design::set_appearance(cx, appearance));
            cx.simulate_resize(gpui_kit::size(gpui_kit::px(352.0), gpui_kit::px(640.0)));
            cx.run_until_parked();

            // `fg_disabled` is deliberately not drawn here, and the panel's reason for leaving it
            // out is measured rather than remembered: see `managed_ink`. It is included below so
            // that reading cannot go stale — the number quoted in that doc is this one.
            let roles: [(&str, f32, f32); 6] = cx.update(|_, cx| {
                let surface = role::surface_content(cx);
                let measured = |ink: Hsla| {
                    design::calculate_contrast_ratio(
                        design::composite_surface(surface, ink),
                        surface,
                    )
                };
                [
                    (
                        "fg_primary",
                        role::fg_primary(cx),
                        design::TEXT_MIN_CONTRAST,
                    ),
                    (
                        "fg_secondary",
                        role::fg_secondary(cx),
                        design::TEXT_MIN_CONTRAST,
                    ),
                    (
                        "fg_tertiary",
                        role::fg_tertiary(cx),
                        design::MARKER_MIN_CONTRAST,
                    ),
                    ("status.danger", role::danger(cx), design::TEXT_MIN_CONTRAST),
                    (
                        "status.warning",
                        role::warning(cx),
                        design::TEXT_MIN_CONTRAST,
                    ),
                    (
                        "fg_disabled",
                        role::fg_disabled(cx),
                        design::DISABLED_TEXT_MIN_CONTRAST,
                    ),
                ]
                .map(|(name, ink, floor)| (name, measured(ink), floor))
            });
            for (name, ratio, floor) in roles {
                assert!(
                    ratio >= floor,
                    "{appearance:?}: `{name}` measures {ratio:.2}:1 on the Inspector's surface \
                     and the floor is {floor}:1"
                );
                if name == "fg_disabled" || name == "fg_tertiary" {
                    std::println!("MEASURED {appearance:?} {name} {ratio:.4}");
                }
            }
            panel.update(cx, |_, _| {});
        }
    }

    /// The three states of the region, measured rather than described.
    ///
    /// `PROMPT.md` §4.5.3 is blunt that the three states carry most of the weight in a k8s tool
    /// and that almost nobody builds them. The measurable claims are: the empty state is a 24px
    /// icon over one line and nothing else, the waiting state says what it is waiting for, and
    /// the failure state offers a verb-phrase action rather than an acknowledgement.
    #[gpui_kit::test]
    fn empty_loading_and_error_each_say_one_thing(cx: &mut TestAppContext) {
        // Empty: nothing selected.
        let (panel, cx) = setup(cx, "name: app");
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.simulate_resize(gpui_kit::size(gpui_kit::px(352.0), gpui_kit::px(640.0)));
        cx.run_until_parked();
        let empty = cx.debug_bounds("inspector-empty").expect("the empty state");
        assert!(
            f32::from(empty.size.height) > 240.,
            "the empty state fills the panel it is the state of, rather than sitting in a corner"
        );
        assert!(
            cx.debug_bounds("inspector-retry").is_none(),
            "`§4.13`: at most one action, and here the action is in the table, not here"
        );

        // Loading: a source that never answers.
        let (panel, _source, cx) = hanging_setup(cx, false);
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("inspector-loading").is_some(),
            "a request in flight says so, in the region the data would land in"
        );
        // A skeleton over data that could still arrive is the failure `§4.14` names by name.
        assert!(
            cx.debug_bounds("inspector-describe-body").is_none(),
            "nothing is drawn under the wait, so nothing has to be taken away when it lands"
        );

        // Error: a permission failure.
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, cx| {
            panel.set_yaml_error(
                Some(pod_ref("uid-1")),
                "pods is forbidden: User \"system:serviceaccount:default:app\" cannot get \
                 resource pods in API group \"\" in the namespace \"default\""
                    .to_owned(),
                cx,
            );
        });
        cx.run_until_parked();
        let failure = cx
            .debug_bounds("inspector-load-error")
            .expect("a failed read is a failure, not an empty selection");
        assert!(f32::from(failure.size.height) > 0.);
        let retry = cx.debug_bounds("inspector-retry").expect("Retry");
        assert!(
            f32::from(retry.size.width) > 40.,
            "`§4.15`: the action is a 28px control, not a word in a sentence"
        );
    }

    /// The layout invariant at every width the panel allows, for every content state it holds.
    ///
    /// The invariant is a *width* invariant: nothing this panel draws may leave the frame's left
    /// or right edge, at any width between `design::size::INSPECTOR_MIN` and
    /// `design::size::INSPECTOR_MAX`. The content a state holds is not a function of width, so
    /// the state is built once and the width is swept across it.
    ///
    /// It used to be built the other way round — a fresh panel per width, the sections opened
    /// per width, and a whole document typed and parsed per width to reach the apply states —
    /// which cost 150 seconds, the entire runtime of the other 1001 tests put together. Measured
    /// on this tree: one `setup` is 288ms and one resize-and-measure is **10.8ms**, so the setup
    /// and the typing were 99% of the cost and the layout the test actually exists to check was
    /// 0.4% of it. The states are now derived three times in total instead of 112, and the width
    /// grid is unchanged at 4px, so the coverage is the same and the apply states are now also
    /// measured at widths they were never *reached* at before — a settled panel that is then
    /// resized is a stronger check than a panel built at that width, because the wrap points it
    /// was laid out at are still in the tree.
    #[gpui_kit::test]
    fn scratch_width_sweep(cx: &mut TestAppContext) {
        let widths: Vec<f32> = (260..=480).step_by(4).map(|width| width as f32).collect();
        let mut report: Vec<String> = Vec::new();

        // The three docked tabs share one panel: a tab switch is a content change, and the
        // content is what this sweep holds still.
        {
            let (panel, cx) = setup(cx, "name: app");
            select(&panel, "uid-1", cx);
            panel.update(cx, |panel, _| panel.set_on_apply(|_| {}));
            // Opened at the narrowest width the panel allows, which is the worst case for
            // anything that wraps or stacks, and then swept outwards from there.
            cx.simulate_resize(gpui_kit::size(gpui_kit::px(260.), gpui_kit::px(900.)));
            open_all_sections(&panel, cx);
            for tab in [
                InspectorTab::Yaml,
                InspectorTab::Describe,
                InspectorTab::Events,
            ] {
                panel.update(cx, |panel, cx| panel.show_tab(tab, cx));
                cx.run_until_parked();
                for width in widths.iter().copied() {
                    cx.simulate_resize(gpui_kit::size(gpui_kit::px(width), gpui_kit::px(900.)));
                    report.extend(sweep(cx, width, &format!("{tab:?}")));
                }
            }
        }

        // The apply review, reached once by typing the document.
        {
            let (panel, cx) = setup(cx, "name: app");
            select(&panel, "uid-1", cx);
            panel.update(cx, |panel, _| panel.set_on_apply(|_| {}));
            cx.simulate_resize(gpui_kit::size(gpui_kit::px(260.), gpui_kit::px(900.)));
            panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Yaml, cx));
            cx.simulate_keystrokes("ctrl-a");
            cx.simulate_input(&matching_apply_yaml("uid-1"));
            panel.update(cx, |panel, cx| panel.apply(cx));
            cx.run_until_parked();
            for width in widths.iter().copied() {
                cx.simulate_resize(gpui_kit::size(gpui_kit::px(width), gpui_kit::px(900.)));
                report.extend(sweep(cx, width, "Review"));
            }
        }

        // The rejected apply, which is the other state with its own controls.
        {
            let (panel, cx) = setup(cx, "name: app");
            select(&panel, "uid-1", cx);
            let requests: Rc<RefCell<Vec<ApplyRequest>>> = Rc::new(RefCell::new(Vec::new()));
            let sink = requests.clone();
            panel.update(cx, |panel, _| {
                panel.set_targeted_apply_handler(move |request: ApplyRequest, _| {
                    sink.borrow_mut().push(request);
                });
            });
            cx.simulate_resize(gpui_kit::size(gpui_kit::px(260.), gpui_kit::px(900.)));
            panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Yaml, cx));
            cx.simulate_keystrokes("ctrl-a");
            cx.simulate_input(&matching_apply_yaml("uid-1"));
            panel.update(cx, |panel, cx| panel.apply(cx));
            panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
            let request = requests.borrow()[0].clone();
            panel.update(cx, |panel, cx| {
                panel.apply_finished_for(
                    request,
                    Err(
                        "Deployment.apps \"web\" is invalid: spec.replicas: Invalid value: \
                         -1: must be greater than or equal to 0"
                            .to_owned(),
                    ),
                    cx,
                );
            });
            cx.run_until_parked();
            for width in widths.iter().copied() {
                cx.simulate_resize(gpui_kit::size(gpui_kit::px(width), gpui_kit::px(900.)));
                report.extend(sweep(cx, width, "ApplyFailed"));
            }
        }

        let mut report: Vec<String> = report.into_iter().collect();
        report.sort();
        report.dedup();
        // It printed this and asserted nothing, so for as long as it existed it could not fail —
        // a 150-second no-op in the middle of a suite whose other 1001 tests take 141 seconds
        // between them. The assertion is the test; the printing was how it was being read.
        assert!(
            report.is_empty(),
            "{} element(s) drawn outside the Inspector's frame. First: {}\n\
             re-run with --nocapture; the full list is the same assertion, one line per breach.",
            report.len(),
            report.first().map(String::as_str).unwrap_or(""),
        );
    }

    fn sweep(cx: &mut gpui_kit::VisualTestContext, width: f32, mode: &str) -> Vec<String> {
        let mut out = Vec::new();
        let Some(frame) = cx.debug_bounds("inspector-frame") else {
            out.push(format!("{mode} {width}: no frame"));
            return out;
        };
        let left = f32::from(frame.origin.x);
        let right = left + f32::from(frame.size.width);
        let selectors = [
            "inspector-identity",
            "inspector-copy-link",
            "inspector-tabs",
            "inspector-tab-0",
            "inspector-tab-1",
            "inspector-tab-2",
            "inspector-actions",
            "inspector-action-reload",
            "inspector-empty",
            "inspector-empty-action",
            "inspector-loading",
            "inspector-load-error",
            "inspector-retry",
            "inspector-copy-reason",
            "inspector-describe-body",
            "inspector-describe-section-Status",
            "inspector-describe-section-title-Status",
            "inspector-describe-section-Labels",
            "inspector-describe-section-Conditions",
            "inspector-describe-section-Containers",
            "inspector-describe-section-Spec",
            "inspector-describe-section-Related",
            "inspector-section-dash",
            "inspector-events-list",
            "describe-scroll",
            "events-scroll",
            "yaml-action-apply",
            "yaml-action-cancel",
            "yaml-action-copy",
            "yaml-clean-metadata",
            "yaml-problems",
            "yaml-problem-list",
            "yaml-problem-0",
            "yaml-problem-message",
            "yaml-apply-failed",
            "yaml-apply-failed-copy",
            "yaml-apply-failed-revert",
            "yaml-apply-review",
            "yaml-apply-review-diff",
            "yaml-review-apply",
            "yaml-review-keep-editing",
            "yaml-review-check-status",
            "yaml-diff-path",
            "inspector-diff-header",
            "inspector-related-back",
            "metrics-action-reload",
            "metrics-range-action-1m",
            "metrics-range-action-7d",
            "metrics-scroll",
        ];
        for selector in selectors {
            let Some(b) = cx.debug_bounds(selector) else {
                continue;
            };
            let l = f32::from(b.origin.x);
            let r = l + f32::from(b.size.width);
            if l < left - 0.5 || r > right + 0.5 {
                out.push(format!(
                    "{mode} {width}: `{selector}` {l:.1}..{r:.1} vs frame {left:.1}..{right:.1}"
                ));
            }
        }
        out
    }
}
