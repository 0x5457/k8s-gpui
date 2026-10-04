//! Shared panel primitives: typography roles, loading, empty state, and the
//! status row.
//!
//! Every primitive here is a thin call into a gpui-kit component. The app keeps
//! only the mapping from its own design tokens onto those components.

use gpui_kit::assets::IconName;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::empty::{
    Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle,
};
use gpui_kit::component::label::Label;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme, Icon, Sizable, Size, h_flex, v_flex};
// `gpui_kit` re-exports GPUI's `test` attribute macro, so a bare `gpui_kit::*`
// glob in a module that also carries `#[gpui_kit::test]` would shadow the
// built-in `#[test]` the macro emits and expansion would never terminate.
use gpui_kit::base::StyledExt as _;
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, AnyView, App, BoxShadow, ClipboardItem, Context, Div, ElementId, Entity, Font,
    FontWeight, Hsla, IntoElement, Pixels, Role, SharedString, StyleRefinement, Styled, Window,
    div, font, px,
};

use crate::design::{self, Severity, radius, space};

// ---------------------------------------------------------------------------
// Ink roles
// ---------------------------------------------------------------------------

/// One of the three foreground roles `UI-SPEC` §1.4 gives text.
///
/// A shared primitive names the role and never the value (`PROMPT.md` §2.1 #3),
/// and resolves the role in [`RenderOnce`], because `App` is the only thing that
/// can answer it and a call site does not have one.
#[derive(Clone, Copy)]
enum Ink {
    /// `fg.primary` — object names, titles, the selected row.
    Primary,
    /// `fg.secondary` — body copy, healthy state, secondary fields.
    Secondary,
    /// `fg.tertiary` — placeholders, counts, group heads, health dots.
    Tertiary,
}

impl Ink {
    fn resolve(self, cx: &App) -> Hsla {
        match self {
            Self::Primary => design::role::fg_primary(cx),
            Self::Secondary => design::role::fg_secondary(cx),
            Self::Tertiary => design::role::fg_tertiary(cx),
        }
    }
}

/// A label that already wears its ink role, so a call site cannot forget.
///
/// gpui-kit's `Label` renders as
/// `div().line_height(rems(1.25)).text_color(cx.theme().foreground).refine_style(&self.style)`.
/// The `text_color` is hard-coded *after* the div inherits, so a `text_color` the
/// caller puts on the returned value wins and one put on any ancestor never
/// arrives at all. Every shared label therefore reached the screen as
/// `fg.primary` unless its own call site remembered to say otherwise — and
/// `fg.primary` is *heavier* than the `secondary` and `tertiary` ink that most
/// chrome is meant to wear, so the failure is loud rather than quiet. Agent P
/// found two places in the Dock where it had already happened (a tab's active
/// and inactive ink drawn identically, and a three-state toggle drawn all in
/// `fg.primary`); the same shape was reachable from every panel.
///
/// This wrapper keeps the role. The caller's style is refined on last and still
/// wins, so a call site that *does* name an ink renders exactly as it did, and a
/// call site that does not now gets the ink its size and weight belong to.
/// Making `color` a required argument stays available as the stricter follow-up
/// — see the delivery note — but the failure mode is gone now, and gone for the
/// 18 call sites that had none, without touching a line outside this file.
#[derive(IntoElement)]
pub(crate) struct RoleLabel {
    label: Label,
    style: StyleRefinement,
    ink: Ink,
}

impl RoleLabel {
    fn new(
        text: impl Into<SharedString>,
        size: Pixels,
        line_height: Pixels,
        weight: Option<FontWeight>,
        ink: Ink,
    ) -> Self {
        let mut label = Label::new(text).text_size(size).line_height(line_height);
        if let Some(weight) = weight {
            label = label.font_weight(weight);
        }
        Self {
            label,
            style: StyleRefinement::default(),
            ink,
        }
    }
}

/// The style memory is this wrapper's, so every `Styled` method a caller reaches
/// for — `text_color`, `truncate`, `flex_none`, `max_w`, `whitespace_normal` —
/// lands in one place and is refined onto the label at render, which is exactly
/// what chaining them onto the `Label` itself used to do.
impl Styled for RoleLabel {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for RoleLabel {
    fn render(mut self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        // `Styled::text_color` writes `style.text.color`, so that is where a
        // call site's own ink lands and where the role is filled in if it did not.
        if self.style.text.color.is_none() {
            self.style.text.color = Some(self.ink.resolve(cx));
        }
        self.label.refine_style(&self.style)
    }
}

// ---------------------------------------------------------------------------
// Typography
// ---------------------------------------------------------------------------

/// The readable size for a panel's own copy.
///
/// `body 13/400` is `fg.secondary`, not `fg.primary`: §1.4 gives `fg.secondary` to
/// 正文 and reserves `fg.primary` for the name of the thing the copy is about.
pub(super) fn label_body(text: impl Into<SharedString>) -> RoleLabel {
    RoleLabel::new(
        text,
        design::text::BODY,
        design::text::BODY_LINE_HEIGHT,
        None,
        Ink::Secondary,
    )
}

/// The name of a panel or a surface, at the size `DESIGN.md` §3.1 gives it.
///
/// This is the product's panel-title baseline and every panel title in the app
/// is this one call, so its contract is the whole hierarchy: `text::TITLE` +
/// `text::SEMIBOLD` in `role::fg_primary`, and nothing else.
///
/// Two things it deliberately does **not** do, because both were the way a title
/// went wrong in practice:
///
/// - It does not take a subtitle, a count or a status. A panel that has one puts
///   it in a `text::CAPTION` slot beside the title (`label_metadata`), so the
///   title stays one size and one weight whether or not anything is next to it.
///   A helper that accepted the metadata would have had to decide the size
///   itself, and a title that shrinks to fit a count is not a title.
/// - It does not take an ink. A call site that reaches for a different colour is
///   not writing a panel title — it is writing a *status* (and wants
///   `*_word`, the ink solved for a word, not the mark ink a 6px dot wears) or a
///   *dialog* title on a coloured surface (and wants `fg_primary` on that
///   surface). Nine call sites name an ink today; every one of them is a title
///   that had been given another job, and the fix is at the call site.
pub(crate) fn label_panel_title(text: impl Into<SharedString>) -> RoleLabel {
    RoleLabel::new(
        text,
        design::text::TITLE,
        design::text::TITLE_LINE_HEIGHT,
        Some(FontWeight::SEMIBOLD),
        Ink::Primary,
    )
}

/// `metadata` is a caption: §1.4's `fg.tertiary` covers 占位、计数、分组头, and a
/// caption is a count or a label wherever this one is used.
pub(super) fn label_metadata(text: impl Into<SharedString>) -> RoleLabel {
    RoleLabel::new(
        text,
        design::text::CAPTION,
        design::text::CAPTION_LINE_HEIGHT,
        None,
        Ink::Tertiary,
    )
}

pub(super) fn label_text(text: impl Into<SharedString>) -> RoleLabel {
    // The same role as `label_body`, written out rather than delegated, so the ink
    // a helper names is readable in the helper — see the contract in the tests.
    RoleLabel::new(
        text,
        design::text::BODY,
        design::text::BODY_LINE_HEIGHT,
        None,
        Ink::Secondary,
    )
}

/// The product's secondary-metadata baseline: a count, a hint, a placeholder, a
/// unit, a group head.
///
/// `text::CAPTION` in `role::fg_tertiary`, and both halves are load-bearing. The
/// size is a *token*, never a raw `px(11.)`, because the whole reason this
/// helper exists is that the two used to be written out at each call site — and
/// `text::CAPTION` is the one rung of the scale `Design guides > Typography` says
/// is not for a button, a heading or a dense list, which is exactly what a
/// secondary metadata label is not.
///
/// The ink is `fg_tertiary` and never `fg_secondary`, because this is the
/// *quietest* tier and a metadata label drawn a rung up is a label competing
/// with the body copy it annotates. A call site that wants `fg_secondary` wants
/// `label_text`, which is one call away.
///
/// It is a *label*, so it does not truncate itself: a count that does not fit is
/// a layout problem at the call site, and silently clipping one is how a "1,234"
/// becomes a "1,2…".
pub(super) fn label_small(text: impl Into<SharedString>) -> RoleLabel {
    RoleLabel::new(
        text,
        design::text::CAPTION,
        design::text::CAPTION_LINE_HEIGHT,
        None,
        Ink::Tertiary,
    )
}

/// The monospace face every data column and buffer shares.
pub(super) fn buffer_font(cx: &App) -> Font {
    font(&cx.theme().mono_font_family)
}

// ---------------------------------------------------------------------------
// Controls
// ---------------------------------------------------------------------------

/// The shared control rhythm: a ghost button on the 28px row, in the tab order.
pub(super) fn reusable_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    Button::new(id)
        .label(label)
        .ghost()
        // The 28px row is the shared control rhythm, so a change to the button
        // size cannot quietly move the app off the toolbar contract.
        .with_size(Size::Size(design::size::CONTROL))
        .tab_index(0isize)
}

/// Puts a chrome control's LABEL on the type scale, for a caller who cannot.
///
/// gpui-kit resolves a `Button`'s label through `button_text_size`, and the
/// `Size::Size(_)` arm returns `text_base()` — 16px, three steps above
/// `text::BODY`. A button on a chrome band that names its own `Size` therefore
/// paints its label louder than the tab titles above it, and
/// `Button::content_style(_, icon_size)` is the only other lever and it is
/// `pub(crate)`.
///
/// So the label rides in as a `CHILD` at the token the band wants, which is what
/// the two title-bar switchers already do, and the button's own accessible label
/// carries the words for a screen reader — otherwise the control would have a
/// visible name and no announced one.
///
/// One helper rather than twenty inline `div`s, because twenty inline copies is
/// how a second size turns up: this is the only place in the product that says
/// what a chrome button's label is.
pub(crate) fn labelled(button: Button, text: impl Into<SharedString>) -> Button {
    let text = text.into();
    button
        .child(
            div()
                .text_size(design::text::BODY)
                .line_height(design::text::BODY_LINE_HEIGHT)
                .child(text.clone()),
        )
        .accessibility_label(text)
}

/// The shared control rhythm for an icon-only control, on the same 28px target.
pub(super) fn reusable_icon_button(
    id: impl Into<ElementId>,
    icon: IconName,
    label: impl Into<SharedString>,
) -> Button {
    Button::new(id)
        .icon(icon)
        .ghost()
        // gpui-kit derives a Button's glyph from its BOX at 0.75, and overwrites
        // whatever size the caller asked for, so the box is the only lever a caller
        // has on the glyph. At `size::CONTROL` (28px) that is a 21px glyph, which is
        // in no lane: the toolbar lane is 16, the navigation lane is 14, and a 21px
        // mark beside a 16px one is the ragged icon column this product spent a wave
        // removing. `icon::IN_TOOLBAR / 0.75` is the box that derives exactly the
        // toolbar lane, and it is still wider than `size::HIT_MIN`, so the target a
        // pointer has to hit does not shrink to accommodate a glyph.
        .with_size(Size::Size(icon_button_box()))
        .w(icon_button_box())
        .accessibility_label(label)
        .tab_index(0isize)
}

/// The box an icon-only button takes so that gpui-kit derives
/// [`design::icon::IN_TOOLBAR`] for its glyph.
///
/// A function because `Pixels` division is not a const operation, and a function
/// because the 0.75 belongs to gpui-kit rather than to this product: it is the
/// inverse of the rule the lane is written in, and a constant that said 0.75
/// would be a second place to be wrong when that rule changes.
fn icon_button_box() -> Pixels {
    design::icon::IN_TOOLBAR / 0.75
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// A waiting glyph. `Spinner` owns the sweep and stops it when the user asked
/// for less motion, so no reduce-motion branch is needed here.
pub(crate) fn spinner(icon: IconName, color: Hsla, size: Size) -> AnyElement {
    Spinner::new()
        .icon(Icon::new(icon).text_color(color))
        .with_size(size)
        .into_any_element()
}

/// The bar that leads a loading empty state. gpui-kit's `Progress` owns the
/// sweep, so the app only supplies the accent and the shared width.
#[derive(IntoElement)]
struct LoadingBar;

impl RenderOnce for LoadingBar {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        Progress::new("loading-bar")
            .loading(true)
            // The role layer's accent, contrast-solved on every surface — the
            // theme's seed would read as a second blue beside it.
            .color(design::role::accent(cx))
            .w(design::size::UPDATE_PROGRESS)
            .max_w_full()
            .h(space::XS)
    }
}

// ---------------------------------------------------------------------------
// Empty state
// ---------------------------------------------------------------------------

/// The reason a panel shows when there is no cluster to talk to.
///
/// One string, because the overview routes on it by value - a failed load whose
/// reason is this is the no-cluster empty state rather than a failure - so two
/// definitions would quietly stop recognising each other.
pub(crate) const NOT_CONNECTED_REASON: &str = "Not connected to a cluster.";

/// The size the glyph that leads an empty state is drawn at.
///
/// `design::icon::LEAD`, named rather than restated: every empty state in this
/// product goes through [`empty_state`], so the lane is stated once here and the
/// surfaces inherit it.
const EMPTY_ICON: Pixels = design::icon::LEAD;

/// Shows an icon, title, and next step.
pub(crate) fn empty_state(
    icon: IconName,
    title: &'static str,
    hint: impl Into<SharedString>,
) -> AnyElement {
    empty_state_with_action(icon, title, hint, None)
}

pub(crate) fn empty_state_with_action(
    icon: IconName,
    title: &'static str,
    hint: impl Into<SharedString>,
    action: Option<AnyElement>,
) -> AnyElement {
    EmptyState {
        icon,
        title,
        hint: hint.into(),
        action,
        loading: icon == IconName::LoaderCircle,
    }
    .into_any_element()
}

/// The app's one empty state, drawn to `UI-SPEC` §4.13.
///
/// gpui-kit's `Empty` is a *page-level* empty state and every one of its defaults
/// is a decision this product does not make, so each is undone here rather than
/// inherited:
///
/// | `Empty` default | what §4.13 asks for |
/// |---|---|
/// | `border_dashed()` 1px | no border — `PROMPT.md` §2.1 #5 leaves strokes to inputs, overlays and the one panel divider |
/// | `rounded(radius.xl)` 12px | `r-lg` 8px is the only card radius in §2.2 |
/// | `p_6()` 24px | §4.13 asks for 48px of air and says nothing about a card inset; a 24px pad inside a padded panel is a second inset |
/// | `EmptyMediaVariant::Icon` = `size_8()` `bg(muted)` frame | §4.13: a 24px `fg.tertiary` glyph and **不要用彩色大图标** |
/// | `EmptyTitle` `text_sm().font_medium()` = 14/500 | `title 15/600` — and 14 is not one of the nine tokens in §2.3 |
/// | `EmptyDescription` `text_sm()` `relative(1.625)` + gpui-kit's `muted_foreground` | `body 13/400` at `fg.secondary`, on the line box §2.3 gives `body` |
/// | `flex_1()` | `flex_none()` — see below |
///
/// The `flex_1()` is the one that costs a reader something. `Empty` grows to fill
/// whatever height it is given and centres its own children, so the action — its
/// *sibling*, not its child — is pushed to the bottom of the viewport: the title
/// sat in the middle of the panel and the button 450px below it against the
/// status bar. §4.13 asks for icon + one line + action as one centred group, and
/// this wrapper's own centring is what puts the group there.
///
/// **And it is what makes the wrapper's height honest.** `size_full()` is
/// `h_full()`, a *percentage* height, and a percentage has nothing definite to
/// resolve against when the parent is a scroll container — it came out **0px**, so
/// every caller that puts an empty state inside a scrolling column (the dock,
/// forwards, helm, the table view) drew nothing at all under a toolbar that said
/// there was nothing. `flex_1` fills a definite-height flex parent and `min_h(0)`
/// lets the box shrink inside a scroller instead of vanishing.
///
/// The ink is resolved here rather than at the call site for the reason
/// [`RoleLabel`] gives: a call site has no `App`. The glyph is `fg.tertiary` in
/// every state including the failures, because a coloured 24px icon on a panel
/// with nothing else on it is the loudest thing a failure state can be.
#[derive(IntoElement)]
struct EmptyState {
    icon: IconName,
    title: &'static str,
    hint: SharedString,
    action: Option<AnyElement>,
    loading: bool,
}

impl RenderOnce for EmptyState {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            icon,
            title,
            hint,
            action,
            loading,
        } = self;
        // A blank explanation adds nothing, and `EmptyDescription` is a
        // `line_height(relative(1.625))` box: leaving it in place for an empty
        // string put 22px of nothing between the title and the action. §4.13 says
        // the description is absent by default.
        let described = !hint.is_empty();
        let glyph: AnyElement = if loading {
            // The shared spinner stops when the reader asked for less motion, and
            // the unframed media slot is the only one it fits: the `Icon` frame is
            // sized for a static glyph and would crop the sweep.
            spinner(icon, design::role::fg_tertiary(cx), Size::Size(EMPTY_ICON))
        } else {
            Icon::new(icon)
                .with_size(Size::Size(EMPTY_ICON))
                .text_color(design::role::fg_tertiary(cx))
                .into_any_element()
        };
        let header = EmptyHeader::new()
            // Icon ↔ text is `space::SM` (§2.1's loose value). `EmptyMedia` adds
            // `mb_2()` of its own, which on top of the header gap put 16px
            // between the glyph and the title — neither step legal as a sum.
            .gap(space::SM)
            .max_w(design::size::EMPTY_MEASURE)
            .media(
                EmptyMedia::new()
                    .with_variant(EmptyMediaVariant::Default)
                    .mb_0()
                    .child(
                        div()
                            .id("empty-icon")
                            .debug_selector(|| "empty-icon".to_owned())
                            .child(glyph),
                    ),
            )
            .title(
                EmptyTitle::new()
                    .text_size(design::text::TITLE)
                    .line_height(design::text::TITLE_LINE_HEIGHT)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(design::role::fg_primary(cx))
                    .child(
                        div()
                            .id("empty-title")
                            .debug_selector(|| "empty-title".to_owned())
                            .child(title),
                    ),
            );
        let header = if described {
            header.description(
                EmptyDescription::new()
                    .line_height(design::text::BODY_LINE_HEIGHT)
                    .text_color(design::role::fg_secondary(cx))
                    .child(
                        div()
                            .id("empty-description")
                            .debug_selector(|| "empty-description".to_owned())
                            .child(label_body(hint.clone())),
                    ),
            )
        } else {
            header
        };
        let empty = Empty::new()
            .border_0()
            .p_0()
            .rounded(radius::LG)
            .flex_none()
            .gap(space::SM)
            // `Empty` hard-codes gpui-kit's `foreground` after inheriting, so the
            // role is stated here rather than left to the component.
            .text_color(design::role::fg_primary(cx))
            .header(header)
            .when(loading, |this| this.child(LoadingBar));
        let empty = match action {
            Some(action) => empty.child(action),
            None => empty,
        };

        // `Empty` is presentational, so the role and the accessible name sit on
        // the wrapper that owns the surface -- and so does the name every caller
        // that lays this out measures under. gpui-kit's component brings no
        // selector of its own, so the one the shell, the dock and the editor all
        // ask for is registered here.
        div()
            .id(title)
            .debug_selector(|| "empty-state".to_owned())
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            // The group is centred by the wrapper rather than by the component,
            // which is what makes `.flex_none()` on `Empty` safe.
            .items_center()
            .justify_center()
            .role(if loading { Role::Status } else { Role::Region })
            .aria_label(title)
            .when(described, |this| this.aria_description(hint))
            .child(empty)
    }
}

// ---------------------------------------------------------------------------
// Status message
// ---------------------------------------------------------------------------

struct StatusMessageState {
    expanded: bool,
}

impl StatusMessageState {
    fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { expanded: false }
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        self.expanded = !self.expanded;
        cx.notify();
    }
}

/// The status row, with the reason behind it one keystroke away.
///
/// gpui-kit's `Alert` draws the bar; the disclosure is app behavior, because the
/// reason behind a failure is only ever seen by someone who asks for it.
#[derive(IntoElement)]
struct StatusMessage {
    id: ElementId,
    severity: Severity,
    message: SharedString,
    detail: Option<String>,
}

impl RenderOnce for StatusMessage {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(self.id.clone(), cx, StatusMessageState::new);
        let expanded = state.read(cx).expanded;
        let detail = self.detail.clone();
        let mut bar = div().w_full().min_w(px(0.)).child(alert_for(
            self.severity,
            self.id.clone(),
            self.message.clone(),
            cx,
        ));
        if self.detail.is_some() {
            let click = state;
            bar = bar.child(
                reusable_button(
                    self.id.clone(),
                    if expanded {
                        "Hide details"
                    } else {
                        "Show details"
                    },
                )
                .accessibility_label(if expanded {
                    "Hide the reason for this status"
                } else {
                    "Show the reason for this status"
                })
                .on_click(move |_, _, cx| {
                    click.update(cx, |state, cx| state.toggle(cx));
                }),
            );
        }
        let mut column = v_flex().w_full().min_w(px(0.)).gap(space::XS).child(bar);
        if let (true, Some(detail)) = (expanded, detail) {
            // Named after the message rather than the detail, so the id survives
            // a live reason changing under a reader who is looking at it.
            let reason = ElementId::from(format!("status-message-reason-{}", self.message));
            column = column.child(detail_block(detail, reason, cx));
        }
        column
    }
}

/// The mark and the two inks one status band is drawn from, plus its plate.
///
/// gpui-kit's `Alert` paints its own band, and every one of these was a second
/// value rather than the product's: the message text and the icon both took the
/// *mark* ink, the fill was `theme.info` mixed 4% toward white, the hairline was
/// the same channel at 30%, and the radius was whatever the theme's own `radius`
/// happened to be.
///
/// The product separates the two inks for a reason that is written down in
/// `design::role`: a 6px mark and a 12px word do not read at the same contrast,
/// and asking one colour to do both buys the mark's legibility at the word's
/// expense. A band painted in the mark ink is that failure on every status row
/// in the app — the message is a *word*, and it has to be the word ink.
struct AlertRoles {
    /// The glyph, from the product's one health vocabulary.
    ///
    /// [`design::health_icon`] and not a second map written here. The status bar
    /// had a private one, one of its entries was the same glyph a spinner wore,
    /// and one shape was then saying "nothing is running" and "connecting" in the
    /// same strip. A mark the product already names is a mark that cannot drift.
    mark: IconName,
    /// The mark ink, held to the graphic floor.
    mark_ink: Hsla,
    /// The word ink, held to the body-text floor, on the wash below it.
    word_ink: Hsla,
    /// The plate.
    wash: Hsla,
    /// The one hairline.
    border: Hsla,
}

impl AlertRoles {
    /// The band roles for a severity.
    ///
    /// `Neutral` and `Muted` have no channel, and that is the point: they are
    /// ordinary content, so the band is a neutral plate with a neutral mark rather
    /// than an information alert. They used to arrive as `Alert::info`, which put
    /// a blue glyph and a blue wash in front of a message nobody had to act on —
    /// a status colour used as decoration, and the loudest thing on a panel whose
    /// only other content was the message itself.
    ///
    /// `Success` keeps the `success` channel. `role::status_for` maps success to
    /// grey, and that mapping is scoped to a *table status cell* — a 6px dot and a
    /// word in a column, where a green dot on every healthy row is noise. A band
    /// is a different surface: a caller that asked for `Success` is saying
    /// something happened, and saying it in the channel it happened in is what
    /// "the `*` role for the mark, the `*_word` role for the word" asks for.
    fn for_severity(severity: Severity, cx: &App) -> Self {
        let (mark_ink, word_ink, wash, border) = match severity {
            Severity::Success => (
                design::role::success(cx),
                design::role::success_word(cx),
                design::role::success_wash(cx),
                design::role::success_border(cx),
            ),
            Severity::Warning => (
                design::role::warning(cx),
                design::role::warning_word(cx),
                design::role::warning_wash(cx),
                design::role::warning_border(cx),
            ),
            Severity::Error => (
                design::role::danger(cx),
                design::role::danger_word(cx),
                design::role::danger_wash(cx),
                design::role::danger_border(cx),
            ),
            Severity::Info => (
                design::role::info(cx),
                design::role::info_word(cx),
                design::role::info_wash(cx),
                design::role::info_border(cx),
            ),
            // A plate of a channel this band does not have. `surface_inset` is the
            // ladder's own quiet step and a fraction of a contrast from the content
            // plane, so an ordinary message reads as a plate and not as a notice.
            Severity::Neutral | Severity::Muted => (
                design::role::fg_tertiary(cx),
                design::role::fg_secondary(cx),
                design::role::surface_inset(cx),
                design::role::border_subtle(cx),
            ),
        };
        Self {
            mark: design::health_icon(severity),
            mark_ink,
            word_ink,
            wash,
            border,
        }
    }
}

/// One status band, in the product's roles.
///
/// The band is a `radius::MD` plate with a single 1px hairline in the channel's
/// own `_border` role, `space::SM` of vertical and `space::MD` of horizontal
/// padding, and a `space::ICON` gap between the mark and the text. Those are the
/// values `PROMPT.md` §2.1 sets, and gpui-kit's `Size::XSmall` preset is 12/6/6
/// — every one of them a different number from the product's, on every status
/// row in the app.
///
/// `refine_style` is applied last by the component, so restating the padding,
/// the radius, the fill and the stroke here is not fighting it: it *is* the
/// override. The one thing that cannot be reached from here is the fill's own
/// compositing, because `AlertVariant::bg` mixes the channel toward white rather
/// than laying the product's wash down — which is exactly why the wash is stated
/// on the element instead of left to the variant. With every colour stated, the
/// variant has no job left, so it stays `Default`: choosing one would re-introduce
/// a second source for a colour this function has already decided.
///
/// The mark is stated explicitly because the component's icon inherits the
/// band's text colour, and inheriting it would put the *word* ink on the mark —
/// the one thing the two-ink rule exists to prevent.
///
/// One hairline, drawn once, by the element that owns the boundary. There is no
/// second border anywhere in the band and no inset: an alert is a plate, not a
/// card in a card.
fn alert_for(severity: Severity, id: ElementId, message: SharedString, cx: &App) -> Alert {
    let roles = AlertRoles::for_severity(severity, cx);
    Alert::new(id, message)
        .icon(
            Icon::new(roles.mark)
                .with_size(Size::Size(design::size::STATUS_MARKER))
                .text_color(roles.mark_ink)
                .flex_none(),
        )
        .text_color(roles.word_ink)
        .text_size(design::text::LABEL)
        .line_height(design::text::LABEL_LINE_HEIGHT)
        .bg(roles.wash)
        .border_1()
        .border_color(roles.border)
        .rounded(radius::MD)
        .px(space::MD)
        .py(space::SM)
        .gap(space::ICON)
}

/// The measure a reason is read at, in characters of the UI face.
///
/// `UI-SPEC` §2.3's 40ch, the same measure the empty state's description uses
/// and for the same reason: a failure reason is a sentence or three, and a
/// sentence that runs the full width of a 1400px window makes the reader's eye
/// travel the whole panel to reach its second line. The block is also allowed to
/// be *narrower* than that — it is `min_w(0)` and `max_w`, not a fixed width — so
/// the same block is a readable column inside a 320px inspector and a readable
/// column in a 1400px one.
const DETAIL_MEASURE_CH: f32 = 40.0;
/// Advance of `0` on the UI face, as a fraction of the size.
const UI_ADVANCE_EM: f32 = 0.6;
/// The word that names the value in the key/value spine.
const DETAIL_KEY: &str = "Reason";

/// The open half of a status row: the whole reason, and a way to take it away.
///
/// A key/value spine, a measure, and a truncation. All three because the reason
/// is the one string in the app with no length bound: an API server's error body
/// runs to a paragraph, and it used to be laid out as a wrapping column inside a
/// flex row, so a long reason grew the status row by an unbounded number of
/// lines and pushed everything under it off the panel. The row is a status row,
/// and a status row is one line.
///
/// The truncation is not a loss. The value carries the whole reason in a
/// `Tooltip` and the whole reason in the clipboard, and GPUI draws no text
/// selection outside an input, so the copy control is what makes a long reason
/// reachable for a reader who cannot hover.
///
/// The value is `text::LABEL` in `fg_secondary`, not `text::CAPTION` in
/// `fg_tertiary`: the reason is the one sentence a reader opened the status row
/// to read, and a disclosure whose payload is a caption is a caption the reader
/// cannot read.
fn detail_block(detail: String, id: ElementId, _cx: &App) -> Div {
    // The click handler is an `Fn`, so it copies rather than moves: a second copy
    // is cheaper than a control that only works once.
    let copy = detail.clone();
    let measure = px(f32::from(design::text::LABEL) * UI_ADVANCE_EM * DETAIL_MEASURE_CH);
    let value = RoleLabel::new(
        detail.clone(),
        design::text::LABEL,
        design::text::LABEL_LINE_HEIGHT,
        Some(design::text::MEDIUM),
        Ink::Secondary,
    );
    h_flex()
        .flex_1()
        .min_w(px(0.))
        .max_w(measure)
        .gap(space::SM)
        .items_center()
        .child(div().flex_none().child(RoleLabel::new(
            DETAIL_KEY,
            design::text::LABEL,
            design::text::LABEL_LINE_HEIGHT,
            Some(design::text::MEDIUM),
            Ink::Tertiary,
        )))
        // The tooltip is on the wrapper rather than on the label, because
        // `RoleLabel` is a label and only an element with an id can carry one —
        // and the id is the hitbox the tooltip is anchored to, so it has to be
        // the whole value and not the words inside it.
        .child(
            div()
                .id(id)
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .tooltip(text_tooltip(detail))
                .child(value),
        )
        .child(
            div().flex_none().child(
                reusable_icon_button("status-message-copy", IconName::Copy, "Copy the reason")
                    .tooltip("Copy the reason")
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                    }),
            ),
        )
}

/// A plain text tooltip for the surfaces gpui-kit has no tooltip-aware control
/// for. The tooltip is the app's own; the popup and its dismissal are gpui-kit's.
fn text_tooltip(
    text: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    move |window, cx| Tooltip::new(text.clone()).build(window, cx)
}

pub(super) fn status_message(
    severity: Severity,
    message: impl Into<SharedString>,
    detail: Option<String>,
    _cx: &App,
) -> AnyElement {
    let message = message.into();
    // A blank reason has nothing to disclose, so it never grows a disclosure.
    let detail = detail.filter(|detail| !detail.trim().is_empty());
    StatusMessage {
        id: ElementId::from(format!("status-message-{message}")),
        severity,
        message,
        detail,
    }
    .into_any_element()
}

// ---------------------------------------------------------------------------
// Menus
// ---------------------------------------------------------------------------

/// A context menu built from `PopupMenu` rows. gpui-kit owns the popup surface
/// and its dismissal, so the app only describes the rows.
pub(super) fn build_menu(
    window: &mut Window,
    cx: &mut App,
    build: impl FnOnce(PopupMenu, &mut Window, &mut App) -> PopupMenu + 'static,
) -> Entity<PopupMenu> {
    PopupMenu::build(window, cx, move |menu, window, cx| build(menu, window, cx))
}

/// One row of a context menu.
pub(super) fn menu_item(label: impl Into<SharedString>) -> PopupMenuItem {
    PopupMenuItem::new(label)
}

#[derive(Clone, Copy)]
pub(super) struct TabSpec {
    pub label: &'static str,
    pub icon: IconName,
}

const FOCUS_GLOW_BLUR: Pixels = px(4.);

/// The keyboard-focus treatment every panel control uses.
///
/// One accent rail on the control's own edge plus the soft outer halo the product's focus token
/// already describes; four pixels of blur is what makes that band soft rather than a second
/// border. gpui-kit's own ring is a 3px stroke at half alpha painted *outside* the border, which
/// on a text field reads as the browser default rather than as a focus state, so controls turn it
/// off and paint this instead - the window should have one ring, not two languages of ring.
///
/// It hangs on the wrapper that carries the app's focus handle, which is also what makes it appear
/// at all: GPUI only resolves `focus_visible` on an element that tracks a focus handle
/// (`gpui::Div::compute_style_internal`), so a ring drawn anywhere else is never shown.
pub(crate) fn focus_ring(cx: &App) -> impl Fn(StyleRefinement) -> StyleRefinement {
    let (ring, glow) = design::state::focus_ring(cx);
    move |style| {
        style
            .border(design::border::FOCUS_RAIL)
            .border_color(ring)
            .shadow(vec![
                BoxShadow::new(px(0.), px(0.), glow).blur_radius(FOCUS_GLOW_BLUR),
            ])
    }
}

/// A plain hover hint for a value that is merely clipped.
///
/// gpui-kit's `Tooltip` is the element for an explanation on a non-interactive readout, and GPUI
/// only attaches it to an element that owns an identity, so a clipped value needs to be a
/// stateful `div` before this hint will show at all. Footers that read as a chord build their own
/// hints out of a keycap instead; this is the plain one.
pub(crate) fn hover_hint(
    text: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> AnyView {
    let text = text.into();
    move |window, cx| Tooltip::new(text.clone()).build(window, cx)
}

/// A tooltip as a builder closure over the window, so a caller writes that closure once instead of
/// at every cell.
pub(crate) fn with_tooltip<E: StatefulInteractiveElement>(
    mut element: E,
    text: impl Into<SharedString>,
) -> E {
    let text = text.into();
    element
        .interactivity()
        .tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx));
    element
}

#[cfg(test)]
pub(super) fn selection_background(cx: &App) -> Hsla {
    design::text_selection::background(cx)
}

#[cfg(test)]
mod tests {
    use gpui_kit::{TestAppContext, size as gpui_size};

    use super::*;

    /// This module's own text, read by the ink contract below. A source scan is
    /// the only instrument that answers "does *every* helper here name an ink?",
    /// because `RoleLabel` deliberately resolves the colour at render: nothing
    /// observable from outside says whether the fallback was written down.
    const SOURCE: &str = include_str!("common.rs");

    /// Every typography helper the crate shares, and the one it must not grow.
    const TYPOGRAPHY_HELPERS: [&str; 5] = [
        "label_body",
        "label_panel_title",
        "label_metadata",
        "label_text",
        "label_small",
    ];

    struct TypographyHarness;

    impl Render for TypographyHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                v_flex()
                    .id("test-labels")
                    .debug_selector(|| "test-labels".to_owned())
                    .child(label_panel_title("Section"))
                    .child(label_text("Body"))
                    .child(label_small("Metadata")),
            )
        }
    }

    /// The empty state as its callers mount it: inside a scrolling column, which
    /// is where the `size_full()` → 0px bug lived and where a `flex_none` block
    /// that grows instead would put the action 450px from the title.
    struct EmptyHarness(&'static str);

    impl Render for EmptyHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .id("empty-harness-scroll")
                .overflow_y_scroll()
                .child(empty_state(
                    design::glyph::state::no_cluster(),
                    "No clusters",
                    self.0,
                ))
        }
    }

    /// Each shared label names the ink it falls back to.
    ///
    /// gpui-kit's `Label` hard-codes `theme().foreground` after inheriting, so an
    /// ancestor's colour can never reach the text and a helper that names no role
    /// puts `fg.primary` on every one of its call sites — heavier than the
    /// `secondary` and `tertiary` ink chrome is meant to wear, and a failure that
    /// looks like a design choice. `RoleLabel` is the guard, and the guard is only
    /// worth something if every helper opts into it, so this scans the module.
    ///
    /// There is no rendered-pixels equivalent: `RenderOnce` resolves the colour
    /// from `cx.theme()` at paint time and the test surface exposes bounds, not
    /// ink.
    #[test]
    fn every_shared_label_names_its_default_ink() {
        let code = SOURCE
            .split("mod tests {")
            .next()
            .expect("this module before its tests");
        for helper in TYPOGRAPHY_HELPERS {
            let body = code
                .split(&format!("fn {helper}("))
                .nth(1)
                .unwrap_or_else(|| panic!("{helper} is not defined in this module"));
            let body = body
                .split("\n}")
                .next()
                .expect("a helper body ends at the first closing brace");
            assert!(
                body.contains("Ink::"),
                "{helper} names no Ink, so every call site silently renders at \
                 gpui-kit's hard-coded foreground"
            );
        }
        // A helper added without being listed here would not be scanned.
        assert!(
            !code
                .lines()
                .filter(|line| line.contains("fn label_"))
                .any(|line| !TYPOGRAPHY_HELPERS
                    .iter()
                    .any(|helper| line.contains(&format!("fn {helper}(")))),
            "a new label_* helper has to join TYPOGRAPHY_HELPERS so its ink is scanned"
        );
    }

    #[gpui_kit::test]
    fn shared_labels_bind_project_line_heights(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (_harness, cx) = cx.add_window_view(|_, _| TypographyHarness);
        cx.simulate_resize(gpui_size(px(320.), px(80.)));
        cx.run_until_parked();

        let labels = cx.debug_bounds("test-labels").expect("shared labels");
        // 20 + 18 + 14 on the type scale `UI-SPEC` §2.3 sets (section/title 15/20,
        // body 13/18, metadata = caption 11/14).
        assert_eq!(f32::from(labels.size.height), 52.);
    }

    #[gpui_kit::test]
    fn empty_state_renders_through_the_gpui_kit_component(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (_harness, cx) = cx.add_window_view(|_, _| EmptyHarness("Connect one to begin"));
        cx.simulate_resize(gpui_size(px(320.), px(200.)));
        cx.run_until_parked();

        let state = cx
            .debug_bounds("empty-state")
            .expect("the shared empty state");
        // **Not** merely present, which is all this used to assert: a zero-height
        // box is present. `size_full()` is `h_full()`, and a percentage height
        // inside a scrolling column has nothing definite to resolve against — so
        // every caller that puts an empty state in one (the dock, forwards, helm,
        // the table view) drew a toolbar that said there was nothing and then
        // nothing at all under it.
        assert!(
            f32::from(state.size.height) > 0.,
            "the empty state has no height: measured {}",
            f32::from(state.size.height)
        );
        // And not merely tall enough: the line the reader came for is inside it.
        let title = cx
            .debug_bounds("empty-title")
            .expect("the empty state's title");
        assert!(
            title.bottom() <= state.bottom() && title.top() >= state.top(),
            "the title is clipped by its own wrapper: title {:?}, wrapper {:?}",
            title,
            state
        );
    }

    /// §4.13's geometry: a 24px muted glyph, a `title 15/600` line, and no
    /// description at all when the reason has nothing to add.
    ///
    /// gpui-kit's `Empty` defaults are a page-level empty state, and each of these
    /// three was wrong for this product: a `size_8()` `bg(muted)` frame around the
    /// glyph, an `EmptyTitle` at `text_sm().font_medium()` — 14/500, and 14 is not
    /// one of the nine tokens in §2.3 — and an `EmptyDescription` that renders a
    /// `line_height(relative(1.625))` box even when it is handed an empty string,
    /// which put 22px of nothing between a title and its action.
    #[gpui_kit::test]
    fn the_empty_state_is_a_24px_glyph_over_a_title(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (_harness, cx) = cx.add_window_view(|_, _| EmptyHarness("Connect one to begin"));
        cx.simulate_resize(gpui_size(px(320.), px(200.)));
        cx.run_until_parked();

        let glyph = cx
            .debug_bounds("empty-icon")
            .expect("the empty state's glyph");
        assert_eq!(
            gpui_size(glyph.size.width, glyph.size.height),
            gpui_size(px(24.), px(24.)),
            "§4.13 gives an empty state a 24px glyph, unframed"
        );
        let title = cx
            .debug_bounds("empty-title")
            .expect("the empty state's title");
        assert_eq!(
            f32::from(title.size.height),
            f32::from(design::text::TITLE_LINE_HEIGHT),
            "§4.13 gives the title `title 15/600`, and 20 is its line box"
        );

        // §4.13: 说明 默认**没有**. helm's "No releases" and settings' two
        // no-matches states all pass a blank sentence on purpose.
        let (_harness, cx) = cx.add_window_view(|_, _| EmptyHarness(""));
        cx.simulate_resize(gpui_size(px(320.), px(200.)));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("empty-description").is_none(),
            "an empty sentence draws no description box at all"
        );
    }

    #[gpui_kit::test]
    fn selection_token_stays_on_the_theme_role(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        cx.update(|cx| {
            assert_eq!(
                selection_background(cx),
                design::colors(cx).element_selection_background
            );
        });
    }
}
