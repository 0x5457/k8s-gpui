//! Shared panel primitives for headings, tabs, empty states, and monospace columns.

use gpui::{
    Animation, AnimationExt, AnyElement, App, ClipboardItem, Context, Div, ElementId, Font, Hsla,
    IntoElement, ParentElement, Pixels, Role, SharedString, Styled, Window, div, px, relative,
};
use ui::CommonAnimationExt;
use ui::Tooltip;
use ui::prelude::*;

use crate::design::{self, Severity, space};
use crate::settings::DataTypography;

#[derive(IntoElement)]
pub(crate) struct ProjectLabel {
    label: Label,
    line_height: Pixels,
    flex_grow: Option<f32>,
    flex_shrink: Option<f32>,
    flex_basis_zero: bool,
}

impl ProjectLabel {
    #[allow(dead_code)]
    pub fn flex_1(mut self) -> Self {
        self.flex_grow = Some(1.);
        self.flex_shrink = Some(1.);
        self.flex_basis_zero = true;
        self
    }

    pub fn flex_none(mut self) -> Self {
        self.flex_grow = Some(0.);
        self.flex_shrink = Some(0.);
        self
    }

    #[allow(dead_code)]
    pub fn flex_grow(mut self) -> Self {
        self.flex_grow = Some(1.);
        self
    }

    #[allow(dead_code)]
    pub fn flex_shrink(mut self) -> Self {
        self.flex_shrink = Some(1.);
        self
    }

    #[allow(dead_code)]
    pub fn flex_shrink_0(mut self) -> Self {
        self.flex_shrink = Some(0.);
        self
    }
}

impl RenderOnce for ProjectLabel {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let mut element = div().line_height(rems_from_px(f32::from(self.line_height)));
        if self.flex_basis_zero {
            element = element.flex_basis(gpui::relative(0.));
        }
        if let Some(flex_grow) = self.flex_grow {
            element = element.flex_grow(flex_grow);
        }
        if let Some(flex_shrink) = self.flex_shrink {
            element = element.flex_shrink(flex_shrink);
        }
        element.child(self.label)
    }
}

impl LabelCommon for ProjectLabel {
    fn size(mut self, size: LabelSize) -> Self {
        self.label = self.label.size(size);
        self
    }

    fn weight(mut self, weight: gpui::FontWeight) -> Self {
        self.label = self.label.weight(weight);
        self
    }

    fn line_height_style(self, _line_height_style: LineHeightStyle) -> Self {
        self
    }

    fn color(mut self, color: Color) -> Self {
        self.label = self.label.color(color);
        self
    }

    fn strikethrough(mut self) -> Self {
        self.label = self.label.strikethrough();
        self
    }

    fn italic(mut self) -> Self {
        self.label = self.label.italic();
        self
    }

    fn underline(mut self) -> Self {
        self.label = self.label.underline();
        self
    }

    fn alpha(mut self, alpha: f32) -> Self {
        self.label = self.label.alpha(alpha);
        self
    }

    fn truncate(mut self) -> Self {
        self.label = self.label.truncate();
        self
    }

    fn single_line(mut self) -> Self {
        self.label = self.label.single_line();
        self
    }

    fn buffer_font(mut self, cx: &App) -> Self {
        self.label = self.label.buffer_font(cx);
        self
    }

    fn inline_code(mut self, cx: &App) -> Self {
        self.label = self.label.inline_code(cx);
        self
    }
}

fn project_label(text: impl Into<SharedString>, size: Pixels, line_height: Pixels) -> ProjectLabel {
    ProjectLabel {
        label: Label::new(text)
            .size(LabelSize::Custom(rems_from_px(f32::from(size))))
            .line_height_style(LineHeightStyle::TextLabel),
        line_height,
        flex_grow: None,
        flex_shrink: None,
        flex_basis_zero: false,
    }
}

#[allow(dead_code)]
pub(super) fn label_section(text: impl Into<SharedString>) -> ProjectLabel {
    project_label(
        text,
        design::text::SECTION,
        design::text::SECTION_LINE_HEIGHT,
    )
}

pub(super) fn label_body(text: impl Into<SharedString>) -> ProjectLabel {
    project_label(text, design::text::BODY, design::text::BODY_LINE_HEIGHT)
}

/// The name of a panel or a surface, at the size `DESIGN.md` §3.1 gives it.
///
/// Toolbar titles, the Inspector title and the modal titles all name the surface
/// a reader is looking at, so they are one role. Five call sites were building
/// that role themselves, which is how a toolbar ended up naming itself in body
/// text and the modals ended up sharing a token with their own subtitles.
pub(crate) fn label_panel_title(text: impl Into<SharedString>) -> ProjectLabel {
    project_label(
        text,
        design::text::PANEL_TITLE,
        design::text::PANEL_TITLE_LINE_HEIGHT,
    )
}

pub(super) fn label_metadata(text: impl Into<SharedString>) -> ProjectLabel {
    project_label(
        text,
        design::text::METADATA,
        design::text::METADATA_LINE_HEIGHT,
    )
}

pub(super) fn label_text(text: impl Into<SharedString>) -> ProjectLabel {
    label_body(text)
}

pub(super) fn label_small(text: impl Into<SharedString>) -> ProjectLabel {
    label_metadata(text)
}

pub(super) fn buffer_font(cx: &App) -> Font {
    theme::theme_settings(cx).buffer_font(cx).clone()
}

#[allow(dead_code)]
pub(super) fn data_text(
    text: impl Into<SharedString>,
    typography: &DataTypography,
    color: Hsla,
) -> Div {
    typography.apply(div().text_color(color).child(text.into()))
}

#[allow(dead_code)]
pub(super) fn data_row_height(typography: &DataTypography) -> Pixels {
    typography.row_height()
}

/// The shared control rhythm: a ghost button on the 28px row, in the tab order.
pub(super) fn reusable_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    Button::new(id, label)
        .style(ButtonStyle::OutlinedGhost)
        // ButtonSize::Medium is the shared control rhythm, so a change to the
        // button size cannot quietly move the app off the 28px row and toolbar
        // contract.
        .size(ButtonSize::Medium)
        .tab_index(0isize)
}

/// The shared control rhythm for an icon-only control, on the same 28px target.
pub(super) fn reusable_icon_button(
    id: impl Into<ElementId>,
    icon: IconName,
    label: impl Into<SharedString>,
) -> IconButton {
    IconButton::new(id, icon)
        .style(ButtonStyle::OutlinedGhost)
        .size(ButtonSize::Medium)
        .icon_size(IconSize::Custom(rems_from_px(f32::from(
            design::size::ICON,
        ))))
        .width(design::size::CONTROL)
        .aria_label(label)
        .tab_index(0isize)
}

#[cfg(test)]
pub(super) fn selection_background(cx: &App) -> Hsla {
    design::text_selection::background(cx)
}

/// Smallest fill, so the bar is still visible at the start of a sweep.
const LOADING_FILL_MIN: f32 = 0.04;
/// Alpha of the unfilled part of the bar.
const LOADING_TRACK_ALPHA: f32 = 0.22;

/// Progress bar for the loading state.
///
/// A rotating glyph at `design::size::ICON_LARGE` needs a 32px box and two text
/// lines before a panel can show it, so a short container clips the animation
/// down to a sliver. A bar is a hairline tall, keeps the same job, and a fill
/// that advances says the fetch is still running. `Animation` holds its phase in
/// the element state, so the fill keeps advancing across re-renders.
///
/// One sweep is `design::motion::LOADING`: long enough to read as progress, short
/// enough that a slow fetch still looks alive. The bar never parks at full,
/// because full would claim the work is done.
///
/// The bar is a component because it needs two things `empty_state` cannot hand
/// it: the accent colour, which only the theme can resolve, and the reduced
/// motion setting.
#[derive(IntoElement)]
struct LoadingBar {
    title: &'static str,
}

impl RenderOnce for LoadingBar {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        // `Color` is a semantic role and `bg` paints a value, so the shared
        // accessor is what turns the accent role into a paintable colour.
        let accent = Color::Accent.color(cx);
        // The sweep is automatic, repeated motion, so it stops when the user asked
        // for less motion. The static frame is the one `AnimationElement` paints
        // under `App::reduce_motion`, so the bar keeps its shape and its start.
        let fill = div()
            .absolute()
            .top_0()
            .bottom_0()
            .left_0()
            .rounded_full()
            .bg(accent)
            .w(relative(LOADING_FILL_MIN));
        let fill: AnyElement = if crate::settings::reduce_motion_enabled(cx) {
            fill.into_any_element()
        } else {
            fill.with_animation(
                ElementId::Name(SharedString::from(format!(
                    "empty-state-fill-{}",
                    self.title
                ))),
                Animation::new(design::motion::LOADING).repeat(),
                move |fill, phase| fill.w(relative(phase.clamp(LOADING_FILL_MIN, 1.))),
            )
            .into_any_element()
        };
        div()
            .id("empty-state-progress")
            .debug_selector(|| "empty-state-progress".to_owned())
            .role(Role::ProgressIndicator)
            .flex_none()
            .relative()
            // The same width as the update progress bar, so every progress bar in
            // the app reads at one size, and never wider than the panel it sits in.
            .w(design::size::UPDATE_PROGRESS)
            .max_w_full()
            .h(space::XS)
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded_full()
                    .bg(accent)
                    .opacity(LOADING_TRACK_ALPHA),
            )
            .child(fill)
    }
}

/// A waiting glyph that stops rotating when the user asked for less motion.
///
/// A spinner sits inside a row, a popover, and a status line at once, so the
/// reduce-motion branch has to live in one place. Otherwise each call site
/// decides, and thirteen sites already decided differently: twelve rotated
/// unconditionally and only this logic existed at all.
///
/// `LoadingBar` is the right answer for a panel-sized wait. This is for the
/// inline case where a 32px bar would not fit.
pub(crate) fn spinner(icon: IconName, color: Color, size: IconSize, cx: &App) -> AnyElement {
    let icon = Icon::new(icon).size(size).color(color);
    if cx.reduce_motion() {
        icon.into_any_element()
    } else {
        icon.with_rotate_animation(design::motion::SPINNER_PERIOD_SECONDS)
            .into_any_element()
    }
}

/// Shows an icon, title, and next step.
pub(crate) fn empty_state(
    icon: IconName,
    title: &'static str,
    hint: impl Into<SharedString>,
) -> AnyElement {
    empty_state_with_action(icon, title, hint, None)
}

pub(super) fn empty_state_with_action(
    icon: IconName,
    title: &'static str,
    hint: impl Into<SharedString>,
    action: Option<AnyElement>,
) -> AnyElement {
    let hint = hint.into();
    let loading = icon == IconName::LoadCircle;
    let empty_state_icon = design::size::ICON_LARGE;
    let indicator = if loading {
        LoadingBar { title }.into_any_element()
    } else {
        Icon::new(icon)
            .size(IconSize::Custom(rems_from_px(f32::from(empty_state_icon))))
            .color(Color::Muted)
            .into_any_element()
    };
    let title_copy = div()
        .w_full()
        .min_w(px(0.))
        .text_center()
        .text_size(rems_from_px(f32::from(design::text::BODY)))
        .line_height(rems_from_px(f32::from(design::text::BODY_LINE_HEIGHT)))
        // The title keeps its line in a short panel. The hint is what collapses.
        .when(loading, |this| this.flex_shrink_0())
        .debug_selector(|| "empty-state-title".to_owned())
        .child(label_text(title));
    let hint_copy = div()
        .w_full()
        .min_w(px(0.))
        .max_w_full()
        .flex_shrink_1()
        .when(loading, |this| this.min_h(px(0.)).overflow_hidden())
        .text_center()
        .whitespace_normal()
        .text_size(rems_from_px(f32::from(design::text::METADATA)))
        .line_height(rems_from_px(f32::from(design::text::METADATA_LINE_HEIGHT)))
        .debug_selector(|| "empty-state-hint".to_owned())
        .child(label_small(hint.clone()).color(Color::Muted));
    v_flex()
        .id(title)
        .size_full()
        .min_w(px(0.))
        .min_h(px(0.))
        .items_center()
        .justify_center()
        .gap(space::SM)
        .px(space::XL)
        .role(if loading { Role::Status } else { Role::Region })
        .aria_label(title)
        .aria_description(hint)
        .debug_selector(|| "empty-state".to_owned())
        .child(
            v_flex()
                .id(format!("empty-state-block-{title}"))
                .debug_selector(|| "empty-state-block".to_owned())
                .w_full()
                .items_center()
                .gap(space::SM)
                // Room for the indicator and the title, so a short panel keeps
                // the animation and loses the hint instead.
                .min_h(space::XXL)
                .when(loading, |this| this.max_h_full())
                .child(indicator)
                .child(title_copy)
                .child(hint_copy),
        )
        .children(action)
        .into_any_element()
}

/// The status bar, with the reason behind it one keystroke away.
///
/// A tooltip is not enough: the reason behind a failure is only ever seen by
/// someone holding a mouse, and it is the one line that says what went wrong.
/// `status_message` is a free function, so the open and closed state lives in
/// keyed element state the way a component would hold it.
#[derive(IntoElement)]
struct StatusMessage {
    severity: Severity,
    message: SharedString,
    detail: Option<String>,
    notification_surface: Hsla,
    marker: Hsla,
}

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

/// The open half of a status bar: the whole reason, and a way to take it away.
///
/// GPUI draws no text selection outside a text input, so a reader who cannot
/// hover a tooltip still needs a way to get the reason out of the window. The
/// copy control is keyboard reachable on the same 28px target as every other
/// icon control.
fn detail_block(detail: String) -> AnyElement {
    // The click handler is an `Fn`, so it copies rather than moves: a second copy
    // is cheaper than a control that only works once.
    let copy = detail.clone();
    h_flex()
        .flex_1()
        .min_w(px(0.))
        .gap(space::XS)
        .items_start()
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .whitespace_normal()
                .child(label_small(detail).color(Color::Muted)),
        )
        .child(
            reusable_icon_button("status-message-copy", IconName::Copy, "Copy the reason")
                .tooltip(Tooltip::text("Copy the reason"))
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                }),
        )
        .into_any_element()
}

impl RenderOnce for StatusMessage {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(
            (ElementId::from("status-message"), self.message.clone()),
            cx,
            StatusMessageState::new,
        );
        let expanded = state.read(cx).expanded;
        let detail = self.detail.clone();
        // Closed, the message keeps its one line. Open, it wraps with the reason
        // under it, because a half-cut sentence explains nothing.
        let message_copy: AnyElement = if expanded {
            div()
                .w_full()
                .min_w(px(0.))
                .whitespace_normal()
                .child(label_small(self.message.clone()).color(Color::Default))
                .into_any_element()
        } else {
            label_small(self.message.clone())
                .color(Color::Default)
                .truncate()
                .into_any_element()
        };
        let mut body = v_flex()
            .flex_1()
            .min_w(px(0.))
            .gap(space::XS)
            .child(message_copy);
        if let (true, Some(detail)) = (expanded, detail.clone()) {
            body = body.child(detail_block(detail));
        }
        // The bar changes shape instead of clipping: a one-line row while it is
        // closed, a growing column while it is open.
        let bar = if expanded {
            v_flex()
                .min_h(design::size::CONTROL)
                .py(space::XS)
                .gap(space::XS)
        } else {
            h_flex().h(design::size::CONTROL).items_center()
        };
        let mut status = bar
            .id(self.message.clone())
            .flex_none()
            .min_w(px(0.))
            .px(space::SM)
            .debug_selector(|| "status-message".to_owned())
            .gap(space::XS)
            .overflow_hidden()
            .bg(self.notification_surface.alpha(1.0))
            .role(Role::Status)
            .aria_label(self.message.clone())
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(space::XS)
                    .items_center()
                    .child(
                        Icon::new(design::severity_icon(self.severity))
                            .size(IconSize::XSmall)
                            .color(Color::Custom(self.marker)),
                    )
                    .child(body),
            );
        if let Some(detail) = detail.clone() {
            status = status.aria_description(detail.clone());
            status.interactivity().tooltip(Tooltip::text(detail));
        }
        // The disclosure is a real control in the tab order, so a keyboard or a
        // screen reader opens the reason the same way a click does.
        if detail.is_some() {
            let click = state.clone();
            let expand = state.clone();
            let collapse = state.clone();
            status = status.child(
                reusable_button(
                    "status-message-details",
                    if expanded {
                        "Hide details"
                    } else {
                        "Show details"
                    },
                )
                .end_icon(
                    Icon::new(if expanded {
                        IconName::ChevronUp
                    } else {
                        IconName::ChevronDown
                    })
                    .size(IconSize::XSmall),
                )
                .aria_label(if expanded {
                    "Hide the reason for this status"
                } else {
                    "Show the reason for this status"
                })
                .aria_expanded(expanded)
                .on_a11y_action(gpui::accesskit::Action::Expand, move |_, _, cx| {
                    expand.update(cx, |state, cx| state.toggle(cx));
                })
                .on_a11y_action(gpui::accesskit::Action::Collapse, move |_, _, cx| {
                    collapse.update(cx, |state, cx| state.toggle(cx));
                })
                .on_click(move |_, _, cx| {
                    click.update(cx, |state, cx| state.toggle(cx));
                }),
            );
        }
        status.into_any_element()
    }
}

pub(super) fn status_message(
    severity: Severity,
    message: impl Into<SharedString>,
    detail: Option<String>,
    cx: &App,
) -> AnyElement {
    let message = message.into();
    // The surface and the marker read from the theme here, where the `App` the
    // caller already holds is in hand.
    let notification_surface = design::surface::notification(cx);
    StatusMessage {
        severity,
        message,
        // A blank reason has nothing to disclose, so it never grows a disclosure.
        detail: detail.filter(|detail| !detail.trim().is_empty()),
        notification_surface,
        marker: severity.marker_on(cx, notification_surface),
    }
    .into_any_element()
}

#[derive(Clone, Copy)]
pub(super) struct TabSpec {
    pub label: &'static str,
    pub icon: IconName,
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::{Context, Render, TestAppContext, div, px};

    use super::*;

    struct TypographyHarness;

    impl Render for TypographyHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                v_flex()
                    .id("test-labels")
                    .debug_selector(|| "test-labels".to_owned())
                    .child(label_section("Section"))
                    .child(label_text("Body"))
                    .child(label_small("Metadata")),
            )
        }
    }

    struct StatusHarness;

    impl Render for StatusHarness {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(status_message(Severity::Info, "Ready", None, cx))
        }
    }

    struct ButtonHarness {
        clicks: Rc<Cell<usize>>,
        focused: Rc<Cell<usize>>,
    }

    impl ButtonHarness {
        fn new(_cx: &mut Context<Self>) -> Self {
            Self {
                clicks: Rc::new(Cell::new(0)),
                focused: Rc::new(Cell::new(0)),
            }
        }
    }

    impl Render for ButtonHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let first_clicks = self.clicks.clone();
            let disabled_clicks = self.clicks.clone();
            let third_clicks = self.clicks.clone();
            let icon_clicks = self.clicks.clone();
            let first_focus = self.focused.clone();
            let disabled_focus = self.focused.clone();
            let third_focus = self.focused.clone();
            let icon_focus = self.focused.clone();
            h_flex()
                .size_full()
                .child(
                    div()
                        .debug_selector(|| "test-button".to_owned())
                        .on_key_down(move |_, _, _| first_focus.set(1))
                        .child(
                            reusable_button("test-button", "Run")
                                .on_click(move |_, _, _| first_clicks.set(1)),
                        ),
                )
                .child(
                    div()
                        .debug_selector(|| "test-disabled-button".to_owned())
                        .on_key_down(move |_, _, _| disabled_focus.set(2))
                        .child(
                            reusable_button("test-disabled-button", "Disabled")
                                .disabled(true)
                                .on_click(move |_, _, _| disabled_clicks.set(2)),
                        ),
                )
                .child(
                    div()
                        .debug_selector(|| "test-third-button".to_owned())
                        .on_key_down(move |_, _, _| third_focus.set(3))
                        .child(
                            reusable_button("test-third-button", "Next")
                                .on_click(move |_, _, _| third_clicks.set(3)),
                        ),
                )
                .child(
                    div()
                        .debug_selector(|| "test-icon-button".to_owned())
                        .on_key_down(move |_, _, _| icon_focus.set(4))
                        .child(
                            reusable_icon_button("test-icon-button", IconName::Check, "Check")
                                .on_click(move |_, _, _| icon_clicks.set(4)),
                        ),
                )
        }
    }

    #[gpui::test]
    fn shared_labels_bind_project_line_heights(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let (_harness, cx) = cx.add_window_view(|_, _| TypographyHarness);
        cx.simulate_resize(gpui::size(px(320.), px(80.)));
        cx.run_until_parked();

        let labels = cx.debug_bounds("test-labels").expect("shared labels");
        assert_eq!(f32::from(labels.size.height), 50.);
    }

    #[gpui::test]
    fn status_messages_use_the_control_surface_and_height(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let (_harness, cx) = cx.add_window_view(|_, _| StatusHarness);
        cx.simulate_resize(gpui::size(px(160.), px(48.)));
        cx.run_until_parked();

        let status = cx.debug_bounds("status-message").expect("status message");
        assert_eq!(f32::from(status.size.height), 28.);
    }

    #[gpui::test]
    fn reusable_controls_have_fixed_targets_and_skip_disabled_tabs(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let (harness, cx) = cx.add_window_view(|_, cx| ButtonHarness::new(cx));
        cx.simulate_resize(gpui::size(px(320.), px(80.)));
        cx.run_until_parked();

        let button = cx.debug_bounds("test-button").expect("reusable button");
        let icon = cx
            .debug_bounds("test-icon-button")
            .expect("reusable icon button");
        assert_eq!(f32::from(button.size.height), 28.);
        assert_eq!(f32::from(icon.size.width), 28.);
        assert_eq!(f32::from(icon.size.height), 28.);

        let clicks = harness.read_with(cx, |harness, _| harness.clicks.clone());
        let focused = harness.read_with(cx, |harness, _| harness.focused.clone());
        cx.update(|window, cx| {
            window.blur(cx);
            window.focus_next(cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("shift-f");
        assert_eq!(focused.get(), 1);
        cx.update(|window, cx| window.focus_next(cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("shift-f");
        assert_eq!(focused.get(), 3);
        cx.update(|window, cx| window.focus_next(cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("shift-f");
        assert_eq!(focused.get(), 4);

        let disabled = cx
            .debug_bounds("test-disabled-button")
            .expect("disabled button");
        let third = cx.debug_bounds("test-third-button").expect("third button");
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        assert_eq!(clicks.get(), 1);
        cx.simulate_click(disabled.center(), gpui::Modifiers::none());
        assert_eq!(clicks.get(), 1);
        cx.simulate_click(third.center(), gpui::Modifiers::none());
        assert_eq!(clicks.get(), 3);
        cx.simulate_click(icon.center(), gpui::Modifiers::none());
        assert_eq!(clicks.get(), 4);
    }

    #[gpui::test]
    fn selection_token_stays_on_the_theme_role(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        cx.update(|cx| {
            assert_eq!(
                selection_background(cx),
                cx.theme().colors().element_selection_background
            );
        });
    }

    /// The tab surfaces and the focus border stay named theme roles, so no tab
    /// treatment can drift onto a component-local color.
    #[gpui::test]
    fn tab_and_focus_roles_stay_on_the_theme_roles(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        cx.update(|cx| {
            let colors = cx.theme().colors();
            assert_eq!(design::surface::tab_bar(cx), colors.tab_bar_background);
            assert_eq!(
                design::surface::tab_active(cx),
                colors.tab_active_background
            );
            assert_eq!(design::focus::border(cx), colors.border_focused);
            assert_ne!(
                design::surface::tab_active(cx),
                design::surface::tab_bar(cx),
                "the active tab has to separate from the strip behind it"
            );
        });
    }
}
