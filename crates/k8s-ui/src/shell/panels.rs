//! Rendering for the shell toolbar, resource tree, center tabs, panels, and command palette.

#[path = "searchable_picker.rs"]
mod searchable_picker;

pub(super) use searchable_picker::PickerKind;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, ClickEvent, ClipboardItem, Context, DismissEvent, DragMoveEvent,
    Entity, Focusable, InteractiveElement, IntoElement, KeybindingKeystroke, Keystroke,
    MouseButton, MouseDownEvent, ParentElement, Pixels, Render, Role, SharedString,
    StatefulInteractiveElement, Styled, WeakEntity, Window, div, point, px, size,
};
use k8s_core::cluster::Health;
use ui::prelude::*;
use ui::{
    ContextMenu, ContextMenuEntry, ElevationIndex, Indicator, KeyBinding as UiKeyBinding,
    KeybindingHint, ListItem, ListItemSpacing, PopoverMenu, PopoverMenuHandle, ProgressBar,
    TintColor, Tooltip,
};

use super::commands::CommandRun;
use super::tree::{TreeRow, TreeRowKind};
use super::{
    CatalogState, CenterTab, CenterTabDragPayload, CenterTabDragState, ConnectionState,
    DIALOG_WIDTH, Dialog, NamespaceState, Shell, TabContent, TabView, Toast, ToggleCommandPalette,
    ToggleLeftPanel, ToggleRightPanel,
};
use crate::panels::common::label_panel_title;
use crate::panels::helm::HelmView;
use crate::{
    design, keymap,
    table_view::{ClusterSession, TableStatus, TextInput},
    update::UpdatePhase,
};
use searchable_picker::{
    PickerConfig, PickerOption, PickerSelectHandler, PickerTrigger, SearchablePicker,
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

fn toast_bottom_inset(dock_open: bool, dock_height: f32) -> f32 {
    super::status_bar_height()
        + f32::from(design::space::LG)
        + if dock_open {
            f32::from(design::border::HIT) + dock_height
        } else {
            0.0
        }
}

/// The top of the update card: below the toolbar, the center tab strip, the table header, and
/// the first table row, plus one gap.
///
/// The card used to hang off the toolbar, which put it over the first row of the main table,
/// the one thing the person opened the app to read. Apple HIG `popovers.md` › Best practices:
/// a popover should not cover essential content. `design::size::ROW * 2` covers the header
/// band and the first row, and the header band is shorter than a row.
fn update_overlay_top() -> Pixels {
    design::size::TOOLBAR + design::size::TAB_BAR + design::size::ROW * 2 + design::space::SM
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
}

impl Render for CenterTabDragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
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
            .child(
                Icon::new(self.icon)
                    .size(IconSize::XSmall)
                    .color(Color::Default),
            )
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
/// the last result was cut off with no fade and no scrollbar, because `render_palette_list_edges`
/// derived "no overflow" from the same wrong number. `Shell::palette_chrome` replaces this with
/// the measured chrome from the second frame on, so the prediction only has to survive one frame.
///
/// It errs tall on purpose. The title and the scope line are labels, so their line box is the
/// font's rather than `text::METADATA_LINE_HEIGHT`, and this takes `BODY_LINE_HEIGHT` for them
/// instead: a first frame a few pixels too tall shows a little empty space under the list, while a
/// few pixels too short clips a row the reader is about to choose.
fn palette_chrome_height() -> f32 {
    // The scope block: `space::SM` of padding above, the title row, a `space::XS` gap, and the
    // scope line under it.
    f32::from(design::space::SM)
        + f32::from(design::text::BODY_LINE_HEIGHT) * 2.0
        + f32::from(design::space::XS)
    // The field: its own `size::CONTROL` frame inside a `space::SM` margin on each side.
        + f32::from(design::space::SM) * 2.0
        + f32::from(design::size::CONTROL)
    // The footer. Its rule sits inside its `size::ROW` box, so the row covers the line.
        + f32::from(design::size::ROW)
}

/// The three vertical numbers the palette needs before anything is measured: how tall the
/// card is, how much of that the scrolling list gets, and how tall the list's content is.
/// The end fades and the scroll indicator read all three, so they stay true on the first
/// frame instead of trailing the scroll handle by one.
struct PaletteMetrics {
    card_height: f32,
    list_viewport: f32,
    list_content: f32,
}

/// `chrome` is what the card spends on everything but the list. The caller passes the measured
/// chrome when there is one, so a wrong constant cannot quietly become the layout.
fn palette_metrics(matches: &[&super::commands::Command], chrome: f32) -> PaletteMetrics {
    let row = f32::from(design::size::ROW);
    let mut content = f32::from(design::space::XS) * 2.0;
    let mut group: Option<&str> = None;
    for command in matches {
        // A group header carries the space that separates it from the group above.
        if group != Some(command.group.as_ref()) {
            group = Some(command.group.as_ref());
            content += row + f32::from(design::space::SM);
        }
        content += row;
    }
    if matches.is_empty() {
        // The empty branch is a sentence *and* the `Clear search` control, and this used to
        // budget the sentence alone. The card is `overflow_hidden` with a `flex_1` list, so the
        // missing control was cut off at the bottom edge — the one control the empty state
        // exists for, unreachable by mouse and by keyboard, on a card that reported itself
        // large enough to hold everything it listed.
        content += f32::from(design::text::METADATA_LINE_HEIGHT)
            + f32::from(design::size::CONTROL)
            + f32::from(design::space::SM) * 3.0;
    }
    let card_height = (content + chrome).min(PALETTE_MAX_HEIGHT);
    PaletteMetrics {
        card_height,
        list_viewport: (card_height - chrome).max(0.0),
        list_content: content,
    }
}

#[cfg(test)]
pub(super) fn palette_height(matches: &[&super::commands::Command]) -> f32 {
    palette_metrics(matches, palette_chrome_height()).card_height
}

/// The height the palette's list needs for these matches, at the given chrome. The card has to
/// cover this plus the chrome, or the last row is cut off.
#[cfg(test)]
pub(super) fn palette_list_content(matches: &[&super::commands::Command], chrome: f32) -> f32 {
    palette_metrics(matches, chrome).list_content
}

pub(super) fn palette_result_label(count: usize) -> String {
    // The palette holds more rows than anywhere else in the app, so it is where a missing
    // thousands separator shows up first. `DESIGN.md` §9 claims the count format is one format.
    design::format::count_with_noun(count, "result", "results")
}

fn topbar_palette_visible(_width: f32) -> bool {
    true
}

fn label_size(size: Pixels) -> LabelSize {
    LabelSize::Custom(rems_from_px(f32::from(size)))
}

/// Alpha of the accent wash the active center tab paints on hover and on press.
///
/// `DESIGN.md` §3.4 asks for a large accent fill to mean one clear primary action, and the tab
/// strip is a navigation surface, not a content area. A full `element_selected` slab measured
/// 1.462:1 in dark and 1.229:1 in light there, which is louder than the selected row it sits
/// above. These two steps keep both states visible without repainting the tab's own surface.
///
/// Composited onto `tab.active_background` they measure 1.134:1 and 1.211:1 in light and 1.167:1
/// and 1.267:1 in dark, against the 1.229:1 and 1.462:1 of the slab they replace, and against the
/// 1.018:1 at which a wash stops being a state at all.
const TAB_HOVER_WASH_ALPHA: f32 = 0.08;
const TAB_PRESSED_WASH_ALPHA: f32 = 0.12;

/// The wash the active center tab adds for hover, or for press when `pressed`.
fn tab_accent_wash(colors: &theme::ThemeColors, pressed: bool) -> gpui::Hsla {
    let alpha = if pressed {
        TAB_PRESSED_WASH_ALPHA
    } else {
        TAB_HOVER_WASH_ALPHA
    };
    colors.text_accent.opacity(alpha)
}

fn text(text: impl Into<SharedString>) -> Label {
    Label::new(text).size(label_size(design::text::BODY))
}

fn text_small(text: impl Into<SharedString>) -> Label {
    Label::new(text).size(label_size(design::text::METADATA))
}

fn section_text(text: impl Into<SharedString>) -> Label {
    Label::new(text).size(label_size(design::text::SECTION))
}

/// One footer hint: the verb in the metadata ink, then the keycap.
///
/// `KeybindingHint::with_prefix` cannot be used for this. It sets `FontStyle::Italic` and
/// `text_disabled` on its own base element, and the keycap resets its face inside that base, so
/// the verb arrived italic and dim while the key beside it did not: two appearances for one hint,
/// and the verb in the ink the contract reserves for a control the reader cannot use.
/// `DESIGN.md` §4 and `DESIGN-PROPOSAL` §4.2 both ask for the italic to go, and a parent cannot
/// undo it because the shared component sets the style on itself. So the prefix is not asked for
/// at all, and the verb is a label like every other one.
fn footer_hint(verb: &'static str, binding: UiKeyBinding, cx: &Context<Shell>) -> AnyElement {
    h_flex()
        .gap(design::space::XS)
        .items_center()
        .child(text_small(verb).color(Color::Muted))
        .child(KeybindingHint::new(
            binding,
            cx.theme().colors().elevated_surface_background,
        ))
        .into_any_element()
}

fn keybinding(specs: &[&str]) -> Option<UiKeyBinding> {
    let keystrokes = specs
        .iter()
        .map(|spec| {
            Keystroke::parse(spec)
                .ok()
                .map(KeybindingKeystroke::from_keystroke)
        })
        .collect::<Option<Vec<_>>>()?;
    Some(UiKeyBinding::from_keystrokes(keystrokes.into(), false))
}

fn toolbar_separator(cx: &Context<Shell>) -> impl IntoElement {
    div()
        .flex_none()
        .w(design::border::LINE)
        .h(design::space::LG)
        .bg(cx.theme().colors().border)
}

fn namespace_picker_options(state: &NamespaceState, current: &SharedString) -> Vec<PickerOption> {
    let names = match state {
        NamespaceState::Ready(names) => names.as_slice(),
        NamespaceState::Loading | NamespaceState::Failed(_) => &[],
    };
    let mut all = PickerOption::new(super::ALL_NAMESPACES, super::ALL_NAMESPACES);
    all.icon = Some(IconName::Folder);
    all.current = current.as_ref() == super::ALL_NAMESPACES;
    all.debug_selector = Some(format!("MENU_ITEM-{}", super::ALL_NAMESPACES).into());
    match state {
        NamespaceState::Loading => {
            all.status = Some(design::Severity::Muted);
            all.status_label = Some("Loading…".into());
        }
        NamespaceState::Ready(names) if names.is_empty() => {
            all.status = Some(design::Severity::Muted);
            all.status_label = Some("No Namespaces Found".into());
        }
        NamespaceState::Failed(_) => {
            all.status = Some(design::Severity::Error);
            all.status_label = Some("Load Failed".into());
        }
        NamespaceState::Ready(_) => {}
    }
    let mut options = vec![all];
    options.extend(names.iter().map(|name| {
        let mut option = PickerOption::new(name.clone(), name.clone());
        option.icon = Some(IconName::Folder);
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

fn tree_status(
    id: &'static str,
    icon: IconName,
    message: impl Into<SharedString>,
    cx: &App,
) -> AnyElement {
    let message = message.into();
    let icon = if icon == IconName::LoadCircle {
        // The shared spinner, so this surface stops rotating with Reduce motion like every
        // other one.
        crate::panels::common::spinner(icon, Color::Accent, IconSize::XLarge, cx)
    } else {
        Icon::new(icon)
            .size(IconSize::XLarge)
            .color(Color::Muted)
            .into_any_element()
    };
    v_flex()
        .id(id)
        .w_full()
        .role(Role::Status)
        .aria_label(message.clone())
        .items_center()
        .gap(design::space::SM)
        .px(design::space::MD)
        .py(design::space::LG)
        .child(icon)
        .child(text_small(message).color(Color::Muted))
        .into_any_element()
}

fn update_phase_icon(phase: UpdatePhase) -> IconName {
    match phase {
        UpdatePhase::Idle => IconName::Info,
        UpdatePhase::UpToDate => IconName::Check,
        UpdatePhase::Checking | UpdatePhase::Downloading => IconName::LoadCircle,
        UpdatePhase::Ready => IconName::Check,
        UpdatePhase::Restarting => IconName::RotateCw,
        UpdatePhase::Failed => IconName::Warning,
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
            ConnectionState::Failed(_) => "Connection Unavailable",
            ConnectionState::Reconnecting(_) => "Reconnecting…",
            ConnectionState::Connecting | ConnectionState::Live => "Connected",
        }
    }

    fn connection_failure_icon(&self) -> IconName {
        match self.connection {
            ConnectionState::Reconnecting(_) => IconName::LoadCircle,
            ConnectionState::Failed(_) => IconName::Warning,
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
        let reason = self.connection.detail().unwrap_or_default().to_owned();
        let title = self.connection_failure_message();
        let severity = self.connection.severity();
        let title_color = if matches!(self.connection, ConnectionState::Failed(_)) {
            Color::Error
        } else {
            Color::Warning
        };
        let role = if matches!(self.connection, ConnectionState::Failed(_)) {
            Role::Alert
        } else {
            Role::Status
        };
        let icon = Icon::new(self.connection_failure_icon())
            .size(IconSize::XLarge)
            .color(Color::Custom(severity.marker(cx)));
        // A reconnect is a real wait, so it gets the shared spinner: it rotates unless the
        // reader asked for less motion, which the inline rotation here never did.
        let icon = if matches!(self.connection, ConnectionState::Reconnecting(_)) {
            crate::panels::common::spinner(
                self.connection_failure_icon(),
                Color::Custom(severity.marker(cx)),
                IconSize::XLarge,
                cx,
            )
        } else {
            icon.into_any_element()
        };
        let reload = div()
            .debug_selector(|| "connection-reload-kubeconfigs".to_owned())
            .child(
                Button::new("connection-reload-kubeconfigs", "Reload Kubeconfigs")
                    .style(ButtonStyle::Outlined)
                    .size(ButtonSize::Medium)
                    .tab_index(0isize)
                    .tooltip(Tooltip::text("Reload Kubeconfigs"))
                    .aria_label("Reload Kubeconfigs")
                    .on_click(cx.listener(|shell, _, window, cx| {
                        shell.dispatch(super::ReloadKubeconfigs, window, cx);
                    })),
            );
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
                .child(section_text(title).color(title_color))
                .child(
                    text("Reconnect to the context. If the connection fails, check access to the context.")
                        .color(Color::Muted),
                )
                .child(
                    h_flex()
                        .id("connection-failure-actions")
                        .gap(design::space::SM)
                        .items_center()
                        .child(
                            div()
                                .debug_selector(|| "connection-retry".to_owned())
                                .child(
                                    Button::new("connection-retry", "Retry")
                                        .style(ButtonStyle::Tinted(TintColor::Accent))
                                        .size(ButtonSize::Medium)
                                        .width(px(ACTION_BUTTON_WIDTH))
                                        .tab_index(0isize)
                                        .tooltip(Tooltip::text("Retry Connection"))
                                        .aria_label("Retry Connection")
                                        .on_click(cx.listener(|shell, _, _, cx| {
                                            shell.retry_connection(cx)
                                        })),
                                ),
                        )
                        .child(reload),
                )
                .into_any_element(),
        )
    }

    fn render_update_progress(&self, progress: Option<f32>, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
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
                        ProgressBar::new("update-progress-bar", progress, 1.0, cx)
                            .bg_color(colors.border_variant)
                            .fg_color(colors.text_accent),
                    )
                    .child(text_small(format!("{percent}%")).color(Color::Muted))
                    .into_any_element()
            }
            None => h_flex()
                .id("update-progress-indeterminate")
                .gap(design::space::XS)
                .items_center()
                .role(Role::Status)
                .aria_label("Downloading Update")
                .child(crate::panels::common::spinner(
                    IconName::LoadCircle,
                    Color::Custom(design::Severity::Info.marker(cx)),
                    IconSize::XSmall,
                    cx,
                ))
                .child(text_small("Downloading…").color(Color::Muted))
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
        let colors = cx.theme().colors();
        let severity = update_phase_severity(phase);
        let status = state.status_text();
        let icon = Icon::new(update_phase_icon(phase))
            .size(IconSize::XSmall)
            .color(Color::Custom(severity.marker(cx)));
        let icon = if matches!(
            phase,
            UpdatePhase::Checking | UpdatePhase::Downloading | UpdatePhase::Restarting
        ) {
            crate::panels::common::spinner(
                update_phase_icon(phase),
                Color::Custom(severity.marker(cx)),
                IconSize::XSmall,
                cx,
            )
        } else {
            icon.into_any_element()
        };
        let progress = (phase == UpdatePhase::Downloading)
            .then(|| self.render_update_progress(state.progress, cx));
        let action: Option<AnyElement> = match phase {
            UpdatePhase::Ready => Some(
                Button::new("update-restart", "Restart to Update")
                    .style(ButtonStyle::Tinted(TintColor::Accent))
                    .size(ButtonSize::Medium)
                    .width(px(ACTION_BUTTON_WIDTH))
                    .tab_index(1isize)
                    .disabled(self.update_actions.is_none())
                    .tooltip(Tooltip::text("Restart to Update"))
                    .aria_label("Restart to Update")
                    .on_click(cx.listener(|shell, _, _, cx| shell.run_update_restart(cx)))
                    .into_any_element(),
            ),
            UpdatePhase::Failed => Some(
                Button::new("update-retry", "Retry")
                    .style(ButtonStyle::Tinted(TintColor::Accent))
                    .size(ButtonSize::Medium)
                    .width(px(ACTION_BUTTON_WIDTH))
                    .tab_index(0isize)
                    .disabled(self.update_actions.is_none())
                    .tooltip(Tooltip::text("Retry Update"))
                    .aria_label("Retry Update")
                    .on_click(cx.listener(|shell, _, _, cx| shell.run_update_retry(cx)))
                    .into_any_element(),
            ),
            _ => None,
        };
        let role = if phase == UpdatePhase::Failed {
            Role::Alert
        } else {
            Role::Status
        };
        let mut strip = h_flex()
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
                    .child(
                        text(status)
                            .truncate()
                            .color(if phase == UpdatePhase::Failed {
                                Color::Error
                            } else {
                                Color::Default
                            }),
                    )
                    .when_some(progress, |this, progress| this.child(progress)),
            )
            .when_some(action, |this, action| this.child(action));
        if let Some(error) = state.error.clone() {
            strip.interactivity().tooltip(Tooltip::text(error));
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
        let colors = cx.theme().colors();
        let severity = update_phase_severity(phase);
        let icon = Icon::new(update_phase_icon(phase))
            .size(IconSize::XSmall)
            .color(Color::Custom(severity.marker(cx)));
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
            .child(text(status.clone()).truncate().color(Color::Default));
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
                        Button::new("update-action-check", "Check for Updates")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Medium)
                            .width(px(width))
                            .start_icon(Icon::new(IconName::RefreshTitle).size(IconSize::XSmall))
                            .tooltip(Tooltip::text("Check for Updates"))
                            .aria_label("Check for Updates")
                            .disabled(self.update_actions.is_none())
                            .on_click(cx.listener(|shell, _, _, cx| shell.run_update_check(cx))),
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
                        Button::new("update-action-retry", "Retry Update")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Medium)
                            .width(px(width))
                            .start_icon(Icon::new(IconName::RefreshTitle).size(IconSize::XSmall))
                            .tooltip(Tooltip::text("Retry Update"))
                            .aria_label("Retry Update")
                            .disabled(self.update_actions.is_none())
                            .on_click(cx.listener(|shell, _, _, cx| shell.run_update_retry(cx))),
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
                        Button::new("update-action-restart", "Restart to Update")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Medium)
                            .width(px(width))
                            .start_icon(Icon::new(IconName::RotateCw).size(IconSize::XSmall))
                            .tooltip(Tooltip::text("Restart to Update"))
                            .aria_label("Restart to Update")
                            .disabled(self.update_actions.is_none())
                            .on_click(cx.listener(|shell, _, _, cx| shell.run_update_restart(cx))),
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
            .shadow(ElevationIndex::ModalSurface.shadow(cx))
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
            overlay.interactivity().tooltip(Tooltip::text(error));
        }
        Some(overlay.into_any_element())
    }

    pub(super) fn render_kubeconfig_warning(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let detail = self.kubeconfig_warning.clone()?;
        let message = kubeconfig_warning_message(&detail);
        let colors = cx.theme().colors();
        let mut strip = h_flex()
            .id("kubeconfig-source-warning")
            .debug_selector(|| "kubeconfig-source-warning".to_owned())
            .w_full()
            .flex_none()
            .min_h(design::size::ROW)
            .px(design::space::SM)
            .py(design::space::XS)
            .gap(design::space::SM)
            .items_center()
            .tab_group()
            .tab_index(-3isize)
            .bg(colors.panel_background.alpha(1.0))
            .border_b_1()
            .border_color(colors.border)
            .role(Role::Alert)
            .aria_label(message.clone())
            .aria_description(detail.clone())
            .child(
                Icon::new(IconName::Warning)
                    .size(IconSize::XSmall)
                    .color(Color::Custom(design::Severity::Warning.marker(cx))),
            )
            .child(text(message).truncate())
            .child(div().flex_1())
            .child(
                IconButton::new("dismiss-kubeconfig-warning", IconName::Close)
                    .size(ButtonSize::Default)
                    .icon_size(IconSize::XSmall)
                    .tab_index(0isize)
                    .tooltip(Tooltip::text("Dismiss Kubeconfig Warning"))
                    .aria_label("Dismiss Kubeconfig Warning")
                    .on_click(cx.listener(|shell, _, _, cx| {
                        shell.kubeconfig_warning = None;
                        cx.notify();
                    })),
            );
        strip.interactivity().tooltip(Tooltip::text(detail));
        Some(strip.into_any_element())
    }

    /// Render the top toolbar.
    pub(super) fn render_top_bar(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let width = f32::from(window.viewport_size().width);
        let compact = !super::inspector_available(width);
        let show_palette = topbar_palette_visible(width);
        let colors = cx.theme().colors();
        div()
            .relative()
            .flex_none()
            .w_full()
            .h(design::size::TOOLBAR)
            .px(design::space::SM)
            .overflow_hidden()
            .bg(colors.title_bar_background.alpha(1.0))
            .border_b_1()
            .border_color(colors.border)
            .child(
                h_flex()
                    .size_full()
                    .min_w(px(0.0))
                    .items_center()
                    .gap(design::space::XS)
                    .tab_group()
                    .tab_index(-4isize)
                    .overflow_hidden()
                    .child(self.render_sidebar_toggle(cx))
                    .child(self.render_cluster_selector(cx))
                    .child(div().flex_1().min_w(px(0.0)))
                    .child(self.render_namespace_selector(window, cx))
                    .when(show_palette, |this| {
                        this.child(toolbar_separator(cx))
                            .child(self.render_palette_button(cx))
                    })
                    .child(self.render_settings_button(cx))
                    .child(self.render_inspector_toggle(compact, cx))
                    .child(self.render_top_bar_notifications(cx)),
            )
    }

    /// Error count and notification bell for the top bar's right-hand group.
    ///
    /// `windows.md` asks for critical information and actions to stay off a window's bottom edge,
    /// because a window gets moved. Failures and the notification center therefore live up here,
    /// where the title bar keeps them on screen.
    ///
    /// Two numbers, because they answer two different questions. Errors are what the cluster did
    /// wrong; notifications are everything the app has to say, and four of the five Settings
    /// switches add an Info confirmation each time they succeed, so the raw total is padded with
    /// the reader's own actions. The bell shows the active incidents, which is the same figure the
    /// notification centre puts first and the only one that means something on its own.
    fn render_top_bar_notifications(&self, cx: &Context<Self>) -> AnyElement {
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
        let label = format!("{action}: {notifications}, {errors}");
        let count = h_flex()
            .id("top-bar-errors")
            // A selector so a test can prove the two numbers are two groups.
            .debug_selector(|| "top-bar-errors".to_owned())
            .flex_none()
            .gap(design::space::XS)
            .items_center()
            .role(Role::Status)
            .aria_label(errors.clone())
            // The error shape is the health channel's, not the chrome's: the old
            // `IconName::Close` was the same glyph as this row's own dismiss button, so
            // "3 errors" and "close this" were one shape.
            .child(
                Icon::new(design::health_icon(design::Severity::Error))
                    .size(IconSize::XSmall)
                    .color(if summary.errors > 0 {
                        Color::Error
                    } else {
                        Color::Muted
                    }),
            )
            .child(text_small(errors).color(Color::Muted));
        let mut bell = IconButton::new("top-bar-notifications", IconName::Bell)
            .size(ButtonSize::Medium)
            .icon_size(IconSize::Small)
            .tab_index(7isize)
            .track_focus(&self.top_bar_notifications_focus)
            // The bell and its number are one readout, so they take one ink. The bell used to
            // take the error count's colour, which made it an unlabelled second copy of a number
            // printed 20px away, and the alarm belongs to the error chip on its left.
            .icon_color(Color::Muted)
            .aria_label(label.clone())
            .aria_expanded(open)
            .on_click(cx.listener(|shell, _: &ClickEvent, window, cx| {
                if shell.status_panel == super::StatusPanel::Notifications {
                    shell.close_notifications(window, cx);
                } else {
                    shell.open_notifications(window, cx);
                }
            }));
        if !open {
            bell = bell.tooltip(Tooltip::text(format!("{label}. {total} in total")));
        }
        h_flex()
            .id("top-bar-notifications-group")
            .flex_none()
            .gap(design::space::XS)
            .items_center()
            .child(count)
            .child(
                h_flex()
                    .id("top-bar-notification-count")
                    .debug_selector(|| "top-bar-notification-count".to_owned())
                    .flex_none()
                    .gap(design::space::XS)
                    .items_center()
                    .child(bell)
                    // The number is the live region and the bell is the control, so the count
                    // chip is its own element rather than a wrapper around a button.
                    .child(
                        div()
                            .id("top-bar-notification-count-value")
                            .flex_none()
                            .role(Role::Status)
                            .aria_label(notifications.clone())
                            .child(text_small(notifications).color(Color::Muted)),
                    ),
            )
            .into_any_element()
    }

    fn render_sidebar_toggle(&self, cx: &Context<Self>) -> impl IntoElement {
        // The switch follows the panel, not the tab. Settings is a center tab, so the resource
        // tree is still there to be hidden and shown, and the toggle used to be disabled with a
        // label naming the way out instead.
        let open = self.sidebar_open;
        IconButton::new(
            "toggle-sidebar",
            if open {
                IconName::ThreadsSidebarLeftOpen
            } else {
                IconName::ThreadsSidebarLeftClosed
            },
        )
        .size(ButtonSize::Medium)
        .icon_size(IconSize::Small)
        .track_focus(&self.top_bar_focus)
        .tab_index(0isize)
        .toggle_state(open)
        // `DESIGN.md` §5 gives a selected button the primary style, and the button styles in
        // use have no primary. Without this the selected state falls through to an upstream
        // default this repo has never looked at, so the panel that is open is not the panel
        // that reads as open.
        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
        .aria_expanded(open)
        .tooltip(Tooltip::element(move |_window, cx| {
            h_flex()
                .gap_2()
                .items_center()
                .child(if open { "Hide Sidebar" } else { "Show Sidebar" })
                .child(UiKeyBinding::for_action(&ToggleLeftPanel, cx))
                .into_any_element()
        }))
        .aria_label("Toggle Sidebar")
        .on_click(cx.listener(|this, _, window, cx| {
            this.dispatch(ToggleLeftPanel, window, cx);
        }))
    }

    /// The Settings trigger.
    ///
    /// `toolbars.md` › Actions asks for a symbol over a text label when the symbol is well
    /// recognized, and a gear is. Keeping the label and its shortcut chip would also cost more
    /// width than the cluster name, which is the one item that must never be ambiguous, so the
    /// shortcut stays in the tooltip and the accessible name is unchanged.
    fn render_settings_button(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .id("open-settings-control")
            .debug_selector(|| "open-settings".to_owned())
            .flex_none()
            .child(
                IconButton::new("open-settings", IconName::Settings)
                    .size(ButtonSize::Medium)
                    .icon_size(IconSize::Small)
                    .tab_index(5isize)
                    .track_focus(&self.top_bar_settings_focus)
                    .tooltip(Tooltip::element(move |_window, cx| {
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child("Open Settings")
                            .child(UiKeyBinding::for_action(&crate::settings::OpenSettings, cx))
                            .into_any_element()
                    }))
                    .aria_label("Open Settings")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.dispatch(crate::settings::OpenSettings, window, cx);
                    })),
            )
            .into_any_element()
    }

    fn render_inspector_toggle(&self, compact: bool, cx: &Context<Self>) -> impl IntoElement {
        // Only a window too narrow for the Inspector disables this, which is a fact about the
        // window. Settings is a tab, and the Inspector belongs to the resource views, so it keeps
        // its own switch while Settings is open.
        let open = self.inspector_open;
        let shown = open && !compact;
        IconButton::new(
            "toggle-inspector",
            if shown {
                IconName::ThreadsSidebarRightOpen
            } else {
                IconName::ThreadsSidebarRightClosed
            },
        )
        .size(ButtonSize::Medium)
        .icon_size(IconSize::Small)
        .tab_index(6isize)
        .track_focus(&self.top_bar_inspector_focus)
        .toggle_state(shown)
        // Same reason as the sidebar toggle: a selected state with no declared style renders
        // whatever the shared component defaults to.
        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
        .aria_expanded(open)
        .disabled(compact)
        .tooltip(Tooltip::element(move |_window, cx| {
            h_flex()
                .gap_2()
                .items_center()
                .child(if compact {
                    super::INSPECTOR_COMPACT_LABEL
                } else if open {
                    "Hide Inspector"
                } else {
                    "Show Inspector"
                })
                .when(compact, |this| {
                    this.child(text_small(super::INSPECTOR_WIDTH_HINT).color(Color::Muted))
                })
                .when(!compact, |this| {
                    this.child(UiKeyBinding::for_action(&ToggleRightPanel, cx))
                })
                .into_any_element()
        }))
        .aria_label(if compact {
            super::INSPECTOR_COMPACT_LABEL
        } else {
            "Toggle Inspector"
        })
        .on_click(cx.listener(|this, _, window, cx| {
            this.dispatch(ToggleRightPanel, window, cx);
        }))
    }

    /// The Command Palette trigger.
    ///
    /// `toolbars.md` › Actions asks for a symbol over a text label when the symbol is well
    /// recognized, and the magnifier is the symbol for searching every command. Leaving the
    /// shortcut in the tooltip rather than on the surface also leaves the leading items of the
    /// bar free for the context they describe.
    fn render_palette_button(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .id("command-palette")
            .debug_selector(|| "command-palette".to_owned())
            .flex_none()
            .child(
                IconButton::new("command-palette", IconName::MagnifyingGlass)
                    .size(ButtonSize::Medium)
                    .icon_size(IconSize::Small)
                    .tab_index(4isize)
                    .track_focus(&self.top_bar_palette_focus)
                    .tooltip(Tooltip::element(move |_window, cx| {
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child("Open Command Palette")
                            .child(UiKeyBinding::for_action(&ToggleCommandPalette, cx))
                            .into_any_element()
                    }))
                    .aria_label("Open Command Palette")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.dispatch(ToggleCommandPalette, window, cx);
                    })),
            )
            .into_any_element()
    }

    fn picker_menu(
        &self,
        id: &'static str,
        kind: PickerKind,
        options: Vec<PickerOption>,
        query: Option<String>,
        input: Option<Entity<TextInput>>,
    ) -> PopoverMenu<ContextMenu> {
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
        let picker_slot: Rc<RefCell<Option<Entity<SearchablePicker>>>> =
            Rc::new(RefCell::new(None));
        let menu_slot: Rc<RefCell<Option<WeakEntity<ContextMenu>>>> = Rc::new(RefCell::new(None));
        let input_for_dismiss = input.clone();
        let menu_for_dismiss = menu_slot.clone();
        let shell_for_dismiss = self.shell_weak.clone();
        let on_dismiss: searchable_picker::PickerDismissHandler = Rc::new(move |_window, cx| {
            if let Some(input) = input_for_dismiss.as_ref() {
                input.update(cx, |input, cx| input.clear(cx));
            }
            if let Some(menu) = menu_for_dismiss.borrow().as_ref() {
                menu.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
            }
        });
        let picker_for_open = picker_slot.clone();
        let shell_for_open = self.shell_weak.clone();
        PopoverMenu::new(id)
            .menu(move |window, cx| {
                let config = PickerConfig {
                    kind,
                    options: options.clone(),
                    query: query.clone(),
                    input: input.clone(),
                    on_select: on_select.clone(),
                    on_dismiss: on_dismiss.clone(),
                };
                let picker = cx.new(|cx| SearchablePicker::new(config, cx));
                *picker_slot.borrow_mut() = Some(picker.clone());
                let picker_for_row = picker.clone();
                let menu = ContextMenu::build_persistent(window, cx, move |menu, _, _| {
                    let picker_for_entry = picker_for_row.clone();
                    menu.custom_row(move |_, _| picker_for_entry.clone().into_any_element())
                });
                let input_for_menu_dismiss = input.clone();
                let shell_for_menu_dismiss = shell_for_dismiss.clone();
                window
                    .subscribe(&menu, cx, move |_, _: &DismissEvent, _, cx| {
                        if let Some(input) = input_for_menu_dismiss.as_ref() {
                            input.update(cx, |input, cx| input.clear(cx));
                        }
                        if let Some(shell) = shell_for_menu_dismiss.upgrade() {
                            shell.update(cx, |shell, cx| {
                                shell.set_picker_menu_open(kind, false, cx)
                            });
                        }
                    })
                    .detach();
                *menu_slot.borrow_mut() = Some(menu.downgrade());
                Some(menu)
            })
            .on_open(Rc::new(move |window, cx| {
                if let Some(shell) = shell_for_open.upgrade() {
                    shell.update(cx, |shell, cx| shell.set_picker_menu_open(kind, true, cx));
                }
                let picker_for_focus = picker_for_open.clone();
                window.on_next_frame(move |window, _cx| {
                    window.on_next_frame(move |window, cx| {
                        let focus = picker_for_focus
                            .borrow()
                            .as_ref()
                            .map(|picker| picker.read(cx).input_focus_handle(cx));
                        if let Some(focus) = focus {
                            window.focus(&focus, cx);
                        }
                    });
                });
            }))
    }

    fn render_cluster_selector(&self, cx: &Context<Self>) -> impl IntoElement {
        let clusters = self.clusters.clone();
        let active = self.active_cluster;
        let current = clusters
            .get(active)
            .cloned()
            .unwrap_or_else(|| SharedString::from("No Context"));
        let status = self.connection.label();
        let severity = self.connection.severity();
        let label = format!("{current} · {status}");
        let tooltip = match self.connection.detail() {
            Some(reason) => format!("Switch Context: {current} · {status}. {reason}"),
            None => format!("Switch Context: {current} · {status}"),
        };
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
                            "Not Ready",
                            Some(SharedString::from(reason.clone())),
                        ),
                        Health::Unknown => (design::Severity::Muted, "Not Checked", None),
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
        let handle = PopoverMenuHandle::<ContextMenu>::default();
        let menu = self
            .picker_menu("cluster-menu", PickerKind::Cluster, options, None, None)
            .with_handle(handle.clone())
            .trigger(PickerTrigger::new(
                Button::new("cluster-selector", label)
                    .start_icon(
                        Icon::new(IconName::Server)
                            .size(IconSize::XSmall)
                            .color(Color::Custom(severity.marker(cx))),
                    )
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                    .style(ButtonStyle::Subtle)
                    // `PopoverMenu::trigger` calls `Toggleable::toggle_state` on this
                    // wrapper, and the wrapper forwards it to this button. With no
                    // `selected_style` the shared component keeps rendering the
                    // `Subtle` style it was given, so the open menu was announced by
                    // `aria_expanded` alone and looked identical to a closed one.
                    // `DESIGN.md` §5 gives a selected button the primary style; the
                    // styles in use have no primary, so it names `Tinted(Accent)`.
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                    .size(ButtonSize::Medium)
                    .tab_index(1isize)
                    .track_focus(&self.top_bar_cluster_focus)
                    .truncate(true)
                    .tooltip(Tooltip::text(tooltip.clone()))
                    .aria_role(Role::ComboBox)
                    .aria_value(current.clone())
                    .aria_expanded(self.cluster_menu_open)
                    .aria_label(format!("Context: {current}, {status}"))
                    .aria_description(tooltip),
                handle,
            ));
        div()
            .flex_none()
            .min_w(px(0.0))
            .max_w(px(max_width))
            .flex_shrink_1()
            .h_full()
            .flex()
            .items_center()
            .debug_selector(|| "cluster-selector".to_owned())
            .child(menu)
    }

    fn render_namespace_selector(&self, window: &Window, _cx: &Context<Self>) -> impl IntoElement {
        let current = self.namespace.clone();
        let options = namespace_picker_options(&self.namespace_state, &current);
        let state_label = match &self.namespace_state {
            NamespaceState::Loading => "Loading Namespaces",
            NamespaceState::Ready(names) if names.is_empty() => "No Namespaces Found",
            NamespaceState::Ready(_) => "Loaded",
            NamespaceState::Failed(_) => "Namespace List Unavailable",
        };
        let tooltip = match &self.namespace_state {
            NamespaceState::Failed(_) => {
                "Namespace List Unavailable. Refresh the list, then try again.".to_owned()
            }
            _ => format!("Switch Namespace: {current}"),
        };
        let aria_label = format!("Namespace: {current}, {state_label}");
        let max_width =
            if f32::from(window.viewport_size().width) < super::INSPECTOR_LAYOUT_BREAKPOINT {
                f32::from(design::size::SIDEBAR_MIN) + f32::from(design::space::SM)
            } else {
                f32::from(design::size::SIDEBAR_MIN) + f32::from(design::space::XL)
            };
        let handle = PopoverMenuHandle::<ContextMenu>::default();
        let icon = if matches!(self.namespace_state, NamespaceState::Failed(_)) {
            Icon::new(IconName::Warning)
                .size(IconSize::XSmall)
                .color(Color::Warning)
        } else {
            Icon::new(IconName::Folder)
                .size(IconSize::XSmall)
                .color(Color::Muted)
        };
        let menu = self
            .picker_menu("namespace-menu", PickerKind::Namespace, options, None, None)
            .with_handle(handle.clone())
            .trigger(PickerTrigger::new(
                Button::new("namespace-selector", current.clone())
                    .start_icon(icon)
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                    .style(ButtonStyle::Subtle)
                    // Same reason as the context selector: the popover's open state
                    // reaches this button through the trigger wrapper's `toggle_state`.
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                    .size(ButtonSize::Medium)
                    .width(px(max_width))
                    .tab_index(3isize)
                    .track_focus(&self.top_bar_namespace_focus)
                    .truncate(true)
                    .tooltip(Tooltip::text(tooltip.clone()))
                    .aria_role(Role::ComboBox)
                    .aria_value(current)
                    .aria_expanded(self.namespace_menu_open)
                    .aria_label(aria_label)
                    .aria_description(tooltip),
                handle,
            ));
        div()
            .flex_none()
            .min_w(px(0.0))
            .max_w(px(max_width))
            .flex_shrink_1()
            .h_full()
            .flex()
            .items_center()
            .debug_selector(|| "namespace-selector".to_owned())
            .child(menu)
    }

    /// Render the resource tree and its load states.
    pub(super) fn render_tree(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let focused = self.tree_focus_handle.is_focused(window);
        let rows = self.visible_tree_rows();
        let kind_label = tree_kind_label(&self.tree);
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
                        // structure and the container row stays quiet.
                        children.push(
                            div()
                                .w_full()
                                .h(design::border::LINE)
                                .bg(colors.border_variant)
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
                IconName::LoadCircle,
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
            .w(px(self.left_width))
            .h_full()
            .min_w(px(0.0))
            .bg(colors.panel_background.alpha(1.0))
            .track_focus(&self.tree_focus_handle)
            .key_context("Tree")
            .tab_group()
            .tab_index(0isize)
            .border_1()
            .border_color(colors.border_transparent)
            .focus_visible(|style| style.border_color(colors.border_focused))
            .on_key_down(cx.listener(Self::on_tree_key_down))
            .child(
                h_flex()
                    .flex_none()
                    .h(design::size::ROW)
                    .px(design::space::SM)
                    .gap(design::space::XS)
                    .items_center()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(
                        Icon::new(IconName::ListTree)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(section_text("Resources").color(Color::Muted))
                    .child(div().flex_1())
                    .when_some(self.catalog_stale, |this, stale| {
                        // Mark an expired cache as stale.
                        let severity = if stale {
                            design::Severity::Warning
                        } else {
                            design::Severity::Muted
                        };
                        let label = if stale { "Stale Cache" } else { "Cached" };
                        let mut chip = h_flex()
                            .id("tree-cache")
                            .gap(design::space::XS)
                            .items_center();
                        chip.interactivity().tooltip(Tooltip::text(
                            "Showing cached resources while the list refreshes in the background.",
                        ));
                        this.child(
                            chip.child(Indicator::dot().color(Color::Custom(severity.marker(cx))))
                                .child(text_small(label).color(Color::Muted)),
                        )
                    })
                    .child(text_small(kind_label).color(Color::Muted)),
            )
            .child(
                // The scroll viewport owns the scrollbar and the bottom scroll
                // edge, so the rows keep scrolling under both.
                v_flex()
                    .relative()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(
                        div()
                            .id("resource-tree-scroll")
                            .flex_1()
                            .min_h(px(0.0))
                            .overflow_y_scroll()
                            .track_scroll(&self.tree_scroll)
                            .py(design::space::XS)
                            .child(body),
                    )
                    .child(self.render_tree_scroll_edge(cx)),
            )
    }

    /// Draw the tree's vertical scroll affordance: a track thumb and a bottom
    /// scroll edge.
    ///
    /// The shared `Scrollbars` component keeps its thumb state in keyed window
    /// state and needs a `&mut Window`, which this `&self` render path cannot
    /// hand it. Reading the same scroll handle keeps the thumb honest about the
    /// scroll position, and both overlays use the theme's scrollbar roles, so
    /// they follow Light and Dark instead of a fixed color.
    fn render_tree_scroll_edge(&self, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let viewport = f32::from(self.tree_scroll.bounds().size.height);
        let max_offset = f32::from(self.tree_scroll.max_offset().y);
        let offset = f32::from(self.tree_scroll.offset().y);
        // The first frame has no layout yet, so the affordances wait for it.
        let scrollable = viewport > 0.0 && max_offset > 0.0;
        // The overlays fill the scroll viewport without joining its layout, and
        // every gpui box is a positioning context, so this box is the one the
        // thumb and the edge are placed against. Neither takes a hitbox, so the
        // rows underneath still receive every click.
        let mut edge = div().absolute().inset_0();
        if scrollable && offset > -max_offset {
            // A soft bottom edge keeps a half-visible row reading as more
            // content below instead of as a row clipped by the panel. It is
            // painted first so the thumb stays crisp on top of it.
            let panel = colors.panel_background.alpha(1.0);
            edge = edge.child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .h(design::space::XL)
                    .bg(gpui::linear_gradient(
                        0.,
                        gpui::linear_color_stop(panel.opacity(0.0), 0.),
                        gpui::linear_color_stop(panel, 1.),
                    )),
            );
        }
        if scrollable {
            // The thumb keeps the theme's scrollbar width, so it reads as the
            // scrollbar it stands in for, and it never thins below a row.
            let height = (viewport * viewport / (viewport + max_offset))
                .max(f32::from(design::space::XL))
                .min(viewport);
            let scrolled = (-offset / max_offset).clamp(0.0, 1.0);
            edge = edge.child(
                div()
                    .absolute()
                    .right_0()
                    .top(px((viewport - height) * scrolled))
                    .w(ui::ScrollbarStyle::Regular.to_pixels())
                    .h(px(height))
                    .rounded_full()
                    .bg(colors.scrollbar_thumb_background),
            );
        }
        edge.into_any_element()
    }

    /// Render the resource load failure and retry action.
    fn render_tree_failure(&self, reason: &str, cx: &Context<Self>) -> AnyElement {
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
            .tooltip(Tooltip::text(reason.to_owned()));
        panel
            .child(
                Icon::new(IconName::Warning)
                    .size(IconSize::Medium)
                    .color(Color::Custom(design::Severity::Error.marker(cx))),
            )
            .child(text("Resource Load Failed"))
            .child(
                text_small(
                    "Load the resource tree. If loading fails, check the context connection.",
                )
                .color(Color::Muted),
            )
            .child(
                Button::new("tree-retry", "Retry")
                    .style(ButtonStyle::Tinted(TintColor::Accent))
                    .size(ButtonSize::Medium)
                    .width(px(ACTION_BUTTON_WIDTH))
                    .tab_index(0isize)
                    .tooltip(Tooltip::text("Retry Loading Resources"))
                    .aria_label("Retry Loading Resources")
                    .on_click(cx.listener(|this, _, _, cx| this.retry_catalog(cx))),
            )
            .into_any_element()
    }

    /// Render one tree row.
    ///
    /// The ListItem component cannot carry level, set position, or the
    /// disclosure state, so the row is wrapped in the ARIA tree item itself.
    /// The wrapper also owns selection and focus: the list item draws a focus
    /// ring around its inner box, and that box is a full-width row pushed right
    /// by the indent, so the ring's right edge lands under the centre panel
    /// instead of at the sidebar edge. A leading rail in the row's own gutter
    /// cannot be clipped that way.
    fn render_tree_row(
        &self,
        index: usize,
        row: TreeRow,
        tree_focused: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        // A selected row that lost keyboard focus keeps a rail, dimmed the way
        // the resource table dims it. The rail is a graphic, so it stays above
        // 3:1 on the panel in both themes: 0.55 measured 2.4:1 on Light.
        const MUTED_RAIL_ALPHA: f32 = 0.75;
        let expandable = row.expandable();
        let click_row = row.clone();
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
        // while, so the sidebar uses the app's own glyph for the same task. The
        // cluster row answers through `kind_icon`, which no longer maps it to a
        // server either.
        let icon = match kind {
            // The toolbar names the context, so the cluster row itself is filtered out of the
            // sidebar. The arm still answers for the tree's own vocabulary, and a server is the
            // cluster: `kind_icon` covers resources, and its fallback is a file.
            TreeRowKind::Cluster => IconName::Server,
            TreeRowKind::Overview => IconName::Screen,
            TreeRowKind::Group if expanded => IconName::FolderOpen,
            TreeRowKind::Group => IconName::Folder,
            TreeRowKind::Kind => click_row
                .resource_kind
                .as_deref()
                .map_or(IconName::File, design::kind_icon),
        };
        let detail_color = if selected {
            Color::Default
        } else {
            Color::Muted
        };
        // The indent is padding on the row, not a margin on the list item: a
        // margin would push a full-width row past the sidebar by the indent.
        let indent = f32::from(design::space::MD) * f32::from(depth);
        // Every row reserves the disclosure column, and a leaf leaves it empty.
        // `ListItem::toggle` cannot be used for this: the shared component puts
        // the triangle at `left(rems(-1.))`, which is 16px left of the item box and
        // therefore outside the sidebar for a depth-0 row, and it hides the
        // expanded one behind `visible_on_hover` on top of that. A full-width scan
        // of the gutter found three device pixels of a collapsed triangle and no
        // pixel at all of an expanded one. A disclosure that cannot be seen is not
        // an affordance, so the row draws the column itself: same slot for every
        // row, so the icon column keeps one left edge and the depth step is still
        // `space::MD`.
        let disclosure_label = if expanded {
            format!("Collapse {label}")
        } else {
            format!("Expand {label}")
        };
        let disclosure_id = id.clone();
        let disclosure = h_flex()
            .flex_none()
            .w(design::size::HIT_MIN)
            // A definite height, not `h_full`: the slot's parent is an auto-height row, and a
            // percentage height against one resolves to the content, which is the 12px glyph.
            .h(design::size::TREE_ROW)
            .justify_center()
            .items_center()
            .when(expandable, |this| {
                this.child(
                    div()
                        .id(("tree-disclosure", index))
                        .debug_selector(move || format!("tree-disclosure-{index}"))
                        .size_full()
                        .flex()
                        .justify_center()
                        .items_center()
                        .cursor_pointer()
                        .role(Role::Button)
                        // The tree is one Tab stop with a roving cursor, so the triangle stays
                        // out of the Tab order; Arrow keys, Enter and Space already reach it
                        // through the row.
                        .tab_index(-1isize)
                        .aria_label(disclosure_label)
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
                        // `disclosure-controls.md` asks the control to point inward while the
                        // content is hidden and down while it is shown, so the shape carries the
                        // state as well as the ink.
                        .child(
                            Icon::new(if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                        ),
                )
            });
        // Selection and keyboard focus stay two states, as in the resource
        // table: the selected row keeps its rail when focus leaves the tree,
        // and the focused row gets the wider rail.
        let (rail_width, rail_color) = match (selected, cursor) {
            (true, true) => (design::border::TABLE_FOCUS_RAIL, colors.text_accent),
            (true, false) => (
                design::border::FOCUS_RAIL,
                colors.text_accent.opacity(MUTED_RAIL_ALPHA),
            ),
            (false, _) => (design::border::FOCUS_RAIL, colors.border_focused),
        };
        div()
            .id(("tree-row", index))
            .w_full()
            .pl(px(indent))
            .relative()
            .h(design::size::TREE_ROW)
            .role(Role::TreeItem)
            .accessibility_id(format!("tree-row-{id}"))
            .aria_level(usize::from(depth) + 1)
            .aria_position_in_set(pos_in_set)
            .aria_size_of_set(set_size)
            .aria_selected(selected)
            .when(expandable, |this| this.aria_expanded(expanded))
            // Hover and selection live here too: the list item's own box is
            // shorter than the row, so a wash painted inside it leaves a strip
            // of the row above and below it.
            .hover(|this| this.bg(design::row_hover_bg(cx)))
            .active(|this| this.bg(colors.element_active))
            .when(selected, |this| this.bg(design::row_selected_bg(cx)))
            .when(selected || cursor, |this| {
                this.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(rail_width)
                        .bg(rail_color),
                )
            })
            .child(
                ListItem::new(id)
                    .aria_label(label.clone())
                    .height(design::size::ROW)
                    .spacing(ListItemSpacing::ExtraDense)
                    .inset(true)
                    // The row above owns hover, selection and focus.
                    .selectable(false)
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        if event.click_count() > 1 {
                            return;
                        }
                        this.tree_cursor = Some(index);
                        window.focus(&this.tree_focus_handle, cx);
                        this.on_tree_click(click_row.clone(), cx);
                    }))
                    .on_secondary_mouse_down(cx.listener(
                        move |this, event: &MouseDownEvent, window, cx| {
                            this.tree_cursor = Some(index);
                            window.focus(&this.tree_focus_handle, cx);
                            this.open_tree_context_menu(index, Some(event.position), window, cx);
                            cx.stop_propagation();
                        },
                    ))
                    .start_slot(
                        h_flex()
                            .flex_none()
                            .gap(design::space::XS)
                            .child(disclosure)
                            .child(Icon::new(icon).size(IconSize::XSmall).color(if selected {
                                Color::Default
                            } else {
                                Color::Muted
                            })),
                    )
                    // The group/version suffix is the only thing that tells two same-named
                    // Kinds apart, and it is the end of the label that a 232px sidebar runs out
                    // of room for. A hard cut through the last glyph reads as a rendering fault
                    // rather than as "there is more", so the label ellipsises and the whole of it
                    // is on the tooltip.
                    .tooltip(Tooltip::text(label.clone()))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(text(label)),
                    )
                    .child(div().flex_1())
                    .when_some(detail, |this, detail| {
                        this.end_slot(text_small(detail).color(detail_color))
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
        let colors = cx.theme().colors();
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
                        IconName::LoadCircle,
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
                        IconName::Warning,
                        resource_catalog_missing_message(&tab.title),
                        cx,
                    );
                }
                CatalogState::Ready => {}
            }
        }
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(design::space::SM)
            .px(design::space::XL)
            .child(
                // The empty-state icon token, like the shell's other empty state. `IconSize::XLarge`
                // is 48px and `design::size::ICON_LARGE` is 32, and two empty states on the same
                // surface at 1.5x apart is the whole of finding F36.
                Icon::new(tab.icon)
                    .size(IconSize::Custom(rems_from_px(f32::from(
                        design::size::ICON_LARGE,
                    ))))
                    .color(Color::Muted),
            )
            .child(text(format!("Browse {}", tab.title)))
            .child(text_small(resource_catalog_missing_message(&tab.title)).color(Color::Muted))
            .child(
                Button::new("placeholder-show-pods", "Show Pods")
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Medium)
                    .width(px(ACTION_BUTTON_WIDTH))
                    .tab_index(0isize)
                    .tooltip(Tooltip::text("Show Pods"))
                    .aria_label("Show Pods")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.activate_tab(0, cx);
                        cx.notify();
                    })),
            )
            .into_any_element()
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
        } = slot;
        let colors = cx.theme().colors();
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
            .h_full()
            .flex_none()
            .pl(design::space::MD)
            .pr(design::space::SM)
            .gap(design::space::XS)
            .items_center()
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
            .border_1()
            .border_color(if focused && index == cursor {
                colors.border_focused
            } else {
                colors.border_transparent
            })
            .opacity(if dragging { 0.55 } else { 1.0 })
            .when(is_active, |this| {
                this.bg(design::surface::tab_active(cx).alpha(1.0))
                    // The active tab has no hover of its own, so a pointer resting on the open
                    // tab looked like a pointer resting on nothing. The wash is a low alpha
                    // accent over the tab's own surface: the pressed state used to be a full
                    // `element_selected` slab, which measured 1.462:1 dark and 1.229:1 light over
                    // a navigation strip, and the same slab was just removed from the table
                    // header for exactly that reason.
                    .hover(|this| this.bg(tab_accent_wash(colors, false)))
                    .active(|this| this.bg(tab_accent_wash(colors, true)))
            })
            .when(!is_active, |this| {
                this.hover(|this| this.bg(colors.element_hover))
                    .active(|this| this.bg(colors.element_active))
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
            .child(Icon::new(icon).size(IconSize::XSmall).color(if is_active {
                Color::Default
            } else {
                Color::Muted
            }))
            .child(
                Icon::new(IconName::Pin)
                    .size(IconSize::XSmall)
                    .color(if pinned {
                        Color::Default
                    } else {
                        Color::Custom(colors.background.alpha(0.0))
                    }),
            )
            .child(
                div()
                    .debug_selector(move || format!("center-tab-label-{index}"))
                    .flex_none()
                    .child(text(title).color(if is_active {
                        Color::Default
                    } else {
                        Color::Muted
                    })),
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
                        IconButton::new(("center-tab-close-button", index), IconName::Close)
                            .size(ButtonSize::Default)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text(close_label.clone()))
                            .aria_label(close_label)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                window.focus(&this.center_tabs_focus, cx);
                                cx.stop_propagation();
                                this.close_tab_index(index, window, cx);
                            })),
                    ),
            )
            .when(drop_before, |this| {
                this.child(
                    div()
                        .id(("center-tab-drop-before", index))
                        .debug_selector(move || format!("center-tab-drop-before-{index}"))
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(design::border::FOCUS_RAIL)
                        .bg(colors.text_accent),
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
                        .bg(colors.text_accent),
                )
            })
            .when(is_active, |this| {
                this.child(
                    div()
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .right_0()
                        .h(design::border::FOCUS_RAIL)
                        .bg(colors.text_accent),
                )
            })
            .into_any_element()
    }

    fn render_center_tab_bar(
        &self,
        window: &Window,
        tabs: &[(usize, SharedString, IconName)],
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
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
            .overflow_x_scroll()
            .track_scroll(&self.tabs_scroll)
            .restrict_scroll_to_axis()
            .when(has_pinned, |this| {
                this.border_l_1().border_color(colors.border_variant)
            })
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
            .overflow_hidden()
            .bg(colors.tab_bar_background.alpha(1.0))
            .role(Role::TabList)
            .accessibility_id("center-tab-list")
            .aria_label("Open Tabs")
            // The tab menu is reachable without a pointer.
            .aria_keyshortcuts("Shift+F10 ContextMenu")
            .tab_group()
            .track_focus(&self.center_tabs_focus)
            .on_key_down(cx.listener(Self::on_center_tabs_key_down))
            .border_b_1()
            .border_color(colors.border)
            .when(has_pinned, |this| this.child(pinned_section))
            .child(ordinary_section)
    }

    pub(super) fn open_tab_context_menu(
        &mut self,
        index: usize,
        position: Option<gpui::Point<Pixels>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.open_tabs.contains(&index) || self.tab_context_menu.is_some() {
            return;
        }
        let shell = self.shell_weak.clone();
        let pinned = self.center_tab_is_pinned(index);
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let close_shell = shell.clone();
            let others_shell = shell.clone();
            let all_shell = shell.clone();
            let pin_shell = shell.clone();
            menu.item(
                ContextMenuEntry::new("Close")
                    .icon(IconName::Close)
                    .handler(move |window, cx| {
                        if let Some(shell) = close_shell.upgrade() {
                            shell.update(cx, |shell, cx| shell.close_tab_index(index, window, cx));
                        }
                    }),
            )
            .item(
                ContextMenuEntry::new("Close Other Tabs")
                    .icon(IconName::ListCollapse)
                    .handler(move |window, cx| {
                        if let Some(shell) = others_shell.upgrade() {
                            shell.update(cx, |shell, cx| {
                                shell.close_other_center_tabs(index, window, cx)
                            });
                        }
                    }),
            )
            .item(
                ContextMenuEntry::new("Close All Tabs")
                    .icon(IconName::GenericClose)
                    .handler(move |window, cx| {
                        if let Some(all_shell) = all_shell.upgrade() {
                            all_shell.update(cx, |shell, cx| {
                                shell.close_all_center_tabs(window, cx);
                            });
                        }
                    }),
            )
            .separator()
            .item(
                ContextMenuEntry::new(if pinned { "Unpin" } else { "Pin" })
                    .icon(if pinned {
                        IconName::Unpin
                    } else {
                        IconName::Pin
                    })
                    .handler(move |_window, cx| {
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
        window.on_next_frame(move |window, _cx| {
            window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        });
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
            gpui::deferred(
                gpui::anchored()
                    .position(position)
                    .snap_to_window_with_margin(design::space::SM)
                    .child(div().id("center-tab-context-menu").occlude().child(menu)),
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
        position: Option<gpui::Point<Pixels>>,
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
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let toggle_shell = shell.clone();
            let open_shell = shell.clone();
            let copy_shell = shell.clone();
            let copy_label = label.clone();
            let mut menu = menu;
            if expandable {
                menu = menu.item(
                    ContextMenuEntry::new(if expanded { "Collapse" } else { "Expand" })
                        .icon(if expanded {
                            IconName::ChevronUp
                        } else {
                            IconName::ChevronDown
                        })
                        .handler(move |_window, cx| {
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
                    ContextMenuEntry::new("Open")
                        .icon(IconName::ArrowRight)
                        .handler(move |window, cx| {
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
                ContextMenuEntry::new("Copy Name")
                    .icon(IconName::Copy)
                    .handler(move |_, cx| {
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
        window.on_next_frame(move |window, _cx| {
            window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        });
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
            gpui::deferred(
                gpui::anchored()
                    .position(position)
                    .snap_to_window_with_margin(design::space::SM)
                    .child(div().id("tree-context-menu").occlude().child(menu)),
            )
            .with_priority(3)
            .into_any_element(),
        )
    }

    /// Render the Inspector panel container.
    pub(super) fn render_inspector(&self, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        v_flex()
            .flex_none()
            .w(px(self.right_width))
            .h_full()
            .min_w(px(0.0))
            .bg(colors.panel_background.alpha(1.0))
            // Inspector focus follows the resource table.
            .tab_group()
            .tab_index(4)
            .child(self.inspector.clone())
    }

    /// Render the Dock panel container.
    pub(super) fn render_dock(&self, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        div()
            .id("shell-dock")
            .debug_selector(|| "shell-dock".to_owned())
            .flex_none()
            .w_full()
            .overflow_hidden()
            // Dock focus follows the Inspector.
            .tab_group()
            .tab_index(5)
            .when(self.dock_open, |this| this.h(px(self.dock_height)))
            .bg(colors.panel_background.alpha(1.0))
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
        let colors = cx.theme().colors();
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
            design::surface::raised(cx),
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
                    .line_height(rems_from_px(f32::from(design::text::BODY_LINE_HEIGHT)))
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
                .child(
                    Button::new("toast-action-button", label)
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Medium)
                        .tab_index(0isize)
                        .aria_label(label)
                        .on_click(cx.listener(|shell, _, window, cx| {
                            // The toast goes first, so the recovery runs against a shell with no
                            // toast on it.
                            let action = shell.toast.take().and_then(|toast| toast.action);
                            if let Some(action) = action {
                                (action.run)(shell, window, cx);
                            }
                            cx.notify();
                        })),
                )
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
            .shadow(ElevationIndex::ModalSurface.shadow(cx))
            .child(
                div()
                    .id("toast-icon")
                    .debug_selector(|| "shell-toast-icon".to_owned())
                    .flex_none()
                    .child(
                        Icon::new(design::severity_icon(toast.severity))
                            .size(IconSize::XSmall)
                            .color(Color::Custom(toast.severity.marker(cx))),
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
                        IconButton::new("toast-dismiss-button", IconName::Close)
                            .size(ButtonSize::Medium)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Dismiss Message (Esc)"))
                            .aria_label("Dismiss Message")
                            .on_click(cx.listener(|shell, _, _, cx| {
                                shell.toast = None;
                                cx.notify();
                            })),
                    ),
            );
        card.interactivity().tooltip(Tooltip::text(full_message));
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
        let colors = cx.theme().colors();
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
        let metrics = palette_metrics(&matches, chrome);
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
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            // The scrim states the modality. `modality.md` > Best practices warns that a modal
            // which obscures its previous context makes people lose track of the task they
            // suspended, and it stops a bright status wash behind the card from out-shouting the
            // card itself.
            .bg(design::surface::backdrop(cx))
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
                    .h(px(metrics.card_height))
                    .max_h(DefiniteLength::Fraction(PALETTE_VIEWPORT_SHARE))
                    .flex_none()
                    .rounded_lg()
                    // One border only: the field below draws its own, and the surface already
                    // separates the card from the dimmed window behind it.
                    .bg(colors.elevated_surface_background.alpha(1.0))
                    .shadow(ElevationIndex::ModalSurface.shadow(cx))
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
                            .px(design::space::MD)
                            .pt(design::space::SM)
                            .gap(design::space::XS)
                            .child(
                                h_flex()
                                    .flex_none()
                                    // The count sits in the title row, at least a `space::MD`
                                    // from the scope it qualifies.
                                    .gap(design::space::MD)
                                    .items_center()
                                    // A modal title is a title, not a count: `DESIGN.md` §3.1
                                    // gives `panel_title` to the names of panels and surfaces
                                    // and `metadata` to counts and notes. Two full-screen
                                    // modals whose titles differed by 30% because they shared a
                                    // token with their own subtitles is the same collapse the
                                    // Describe panel had.
                                    .child(label_panel_title(title).color(Color::Default))
                                    .child(
                                        div()
                                            .id("command-palette-result-count")
                                            .debug_selector(|| {
                                                "command-palette-result-count".to_owned()
                                            })
                                            .flex_none()
                                            .aria_label(result_label.clone())
                                            .child(
                                                text_small(result_label).color(Color::Muted),
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
                            // them and takes the whole width between them.
                            .mx(design::space::MD)
                            .items_center()
                            .child(self.palette_input.clone()),
                    )
                    // The scope gets its own line under the field, which is the shape
                    // `DESIGN.md` §4 asks for and the shape the resource search already has.
                    // Between the title and the field it read as part of the title, and for the
                    // context switcher it named the answer to the question the card asks.
                    .child(
                        div()
                            .id("command-palette-scope-line")
                            .debug_selector(|| "command-palette-scope-line".to_owned())
                            .flex_none()
                            .w_full()
                            .min_w(px(0.0))
                            // The same box the resource search gives its scope line, so the two
                            // modals put the same sentence in the same place.
                            .px(design::space::MD)
                            .pb(design::space::SM)
                            .aria_label(format!("Current scope: {scope}"))
                            .child(text_small(scope).color(Color::Muted)),
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
                                    .flex_1()
                                    .min_h(px(0.0))
                                    .overflow_y_scroll()
                                    .track_scroll(&self.palette_scroll)
                                    .py(design::space::XS)
                                    .when(empty, |this| {
                                        this.child(
                                            v_flex()
                                                .px(design::space::MD)
                                                .py(design::space::SM)
                                                .gap(design::space::SM)
                                                .child(
                                                    text_small(
                                                        "No matching commands. Clear the search or try another term.",
                                                    )
                                                    .color(Color::Muted),
                                                )
                                                // `DESIGN.md` §9 promises a control here, and
                                                // the sibling picker has had one all along: a
                                                // sentence that tells the reader to clear the
                                                // search is not a way to clear it. The wrapper
                                                // carries the selector because the shared
                                                // `Button` does not expose one.
                                                .child(
                                                    div()
                                                        .id("command-palette-clear-search")
                                                        .debug_selector(|| {
                                                            "command-palette-clear-button"
                                                                .to_owned()
                                                        })
                                                        .child(
                                                            Button::new(
                                                                "command-palette-clear-button",
                                                                "Clear search",
                                                            )
                                                            .style(ButtonStyle::OutlinedGhost)
                                                            .size(ButtonSize::Medium)
                                                            .tab_index(0isize)
                                                            .aria_label("Clear search")
                                                            .on_click(cx.listener(
                                                                |shell, _, _, cx| {
                                                                    shell.palette_query.clear();
                                                                    shell.palette_input.update(
                                                                        cx,
                                                                        |input, cx| {
                                                                            input.set_text("", cx)
                                                                        },
                                                                    );
                                                                    shell.palette_note = None;
                                                                    shell.sync_palette_selection();
                                                                    shell.reveal_palette_selection();
                                                                    cx.notify();
                                                                },
                                                            )),
                                                        ),
                                                ),
                                        )
                                    })
                                    .children(self.render_command_rows(&matches, selected, cx)),
                            )
                            .children(self.render_palette_list_edges(&metrics, cx)),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .h(design::size::ROW)
                            .px(design::space::MD)
                            .gap(design::space::LG)
                            .items_center()
                            .border_t_1()
                            .border_color(colors.border_variant)
                            // Show why an unavailable command cannot run.
                            .when_some(self.palette_note, |this, note| {
                                this.child(
                                    h_flex()
                                        .gap(design::space::XS)
                                        .items_center()
                                        // `design::health_icon` gives every severity one shape,
                                        // and the note is an explanation, not a failure, so it
                                        // takes the info shape in the muted ink. Painting an info
                                        // glyph in the warning colour says two things at once.
                                        .child(
                                            Icon::new(design::health_icon(design::Severity::Info))
                                                .size(IconSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                        .child(text_small(note).color(Color::Muted)),
                                )
                            })
                            .when(self.palette_note.is_none(), |this| {
                                this.when_some(keybinding(&["up", "down"]), |this, kb| {
                                    this.child(footer_hint("Move Selection", kb, cx))
                                })
                                .when_some(keybinding(&["enter"]), |this, kb| {
                                    this.child(footer_hint("Run Command", kb, cx))
                                })
                                .when_some(keybinding(&["escape"]), |this, kb| {
                                    this.child(footer_hint("Dismiss", kb, cx))
                                })
                            }),
                    ),
            )
    }

    /// Render a dialog button with a visible focus state.
    fn dialog_button(
        &self,
        id: &'static str,
        label: &'static str,
        focus_index: usize,
        style: ButtonStyle,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let focus = self.dialog_button_focus(focus_index);
        let focus_for_click = focus.clone();
        let button = Button::new(id, label)
            .style(style)
            .size(ButtonSize::Medium)
            .width(px(ACTION_BUTTON_WIDTH))
            .tab_index(focus_index as isize)
            .track_focus(&focus)
            .tooltip(Tooltip::text(label))
            .aria_label(label)
            .on_click(on_click);
        div()
            .rounded_md()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    this.dialog_focus = focus_index;
                    window.focus(&focus_for_click, cx);
                }),
            )
            .border_1()
            .border_color(if self.dialog_focus == focus_index {
                colors.border_focused
            } else {
                colors.border_transparent
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
            ButtonStyle::Outlined,
            cx.listener(|this, _, window, cx| this.cancel_dialog(window, cx)),
            cx,
        )
    }

    fn dialog_actions(cancel: AnyElement, confirm: AnyElement) -> gpui::Div {
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
    ) -> gpui::Stateful<gpui::Div> {
        let colors = cx.theme().colors();
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
            .rounded_lg()
            .border_1()
            // The same resolved boundary the toast and the popovers use. `colors.border` on
            // `raised` is 1.37:1 in dark and 2.67:1 in light, and the dialog carried it as its
            // focus ring, so a dialog that had never been focused looked focused and one that
            // had lost focus looked like a sheet of paper.
            .border_color(design::graphic_on_with_minimum(
                design::surface::raised(cx),
                colors.border,
                design::border::INTERACTIVE_MIN_CONTRAST,
            ))
            .bg(colors.elevated_surface_background.alpha(1.0))
            .shadow(ElevationIndex::ModalSurface.shadow(cx))
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
            .child(section_text(title).color(Color::Default))
            .child(detail.color(Color::Muted))
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
        let colors = cx.theme().colors();
        let Some(dialog) = self.dialog.as_ref() else {
            return div().into_any_element();
        };
        let mut card;

        match dialog {
            Dialog::ConfirmTabClose { request } => {
                let (title, detail) = match request {
                    super::TabCloseRequest::One(_) => (
                        "Close Tab?",
                        "Unsaved YAML changes in this tab will be permanently discarded.",
                    ),
                    super::TabCloseRequest::Others(_) => (
                        "Close Other Tabs?",
                        "Unsaved YAML changes in these tabs will be permanently discarded. You cannot undo this.",
                    ),
                    super::TabCloseRequest::All => (
                        "Close All Tabs?",
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
                            ButtonStyle::Tinted(TintColor::Error),
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
                    ButtonStyle::Tinted(TintColor::Error)
                } else {
                    ButtonStyle::Tinted(TintColor::Accent)
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
                    let confirm_focus_handle = self.dialog_button_focus(confirm_focus);
                    let confirm_focus_index = confirm_focus;
                    let confirm_focus_for_click = confirm_focus_handle.clone();
                    let button = Button::new("dialog-helm-confirm", confirm_label)
                        .style(confirm_style)
                        .size(ButtonSize::Medium)
                        .width(px(ACTION_BUTTON_WIDTH))
                        .tab_index(confirm_focus as isize)
                        .track_focus(&confirm_focus_handle)
                        .tooltip(Tooltip::text(confirm_label))
                        .aria_label(confirm_label)
                        .disabled(true)
                        .on_click(cx.listener(|this, _, window, cx| this.confirm_helm(window, cx)));
                    div()
                        .rounded_md()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                this.dialog_focus = confirm_focus_index;
                                window.focus(&confirm_focus_for_click, cx);
                            }),
                        )
                        .border_1()
                        .border_color(if self.dialog_focus == confirm_focus {
                            colors.border_focused
                        } else {
                            colors.border_transparent
                        })
                        .child(button)
                        .into_any_element()
                };
                card = self.dialog_shell(Role::AlertDialog, title, text(detail), window, cx);
                if let Some(field) = field {
                    card = card
                        .child(text_small("Chart Reference").color(Color::Muted))
                        .child(field)
                        .when_some(error.clone(), |this, message| {
                            this.child(text_small(message).color(Color::Error))
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
                            ButtonStyle::Tinted(TintColor::Error),
                            cx.listener(|this, _, window, cx| this.confirm_delete(window, cx)),
                            cx,
                        ),
                    ));
            }
            Dialog::Exec { target, selected } => {
                let title = format!("Open Shell in {}", target.name);
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
                            .cursor_pointer()
                            .rounded_sm()
                            .role(Role::Button)
                            .aria_label(container.clone())
                            .aria_selected(active)
                            .when(active && focused, |this| this.aria_active_descendant())
                            .when(active, |this| this.bg(colors.element_selected))
                            .hover(|this| this.bg(colors.element_hover))
                            .active(|this| this.bg(colors.element_active))
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
                                    IconName::Terminal
                                })
                                .size(IconSize::XSmall)
                                .color(if active {
                                    Color::Default
                                } else {
                                    Color::Muted
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
                            ButtonStyle::Tinted(TintColor::Accent),
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
                let title = format!("Port Forward to {}", target.name);
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
                            .cursor_pointer()
                            .rounded_sm()
                            .border_1()
                            .border_color(if active {
                                colors.border_focused
                            } else {
                                colors.border_variant
                            })
                            .role(Role::Button)
                            .aria_label(format!("Remote port {label}"))
                            .aria_selected(active)
                            .when(active, |this| this.bg(colors.element_selected))
                            .hover(|this| this.bg(colors.element_hover))
                            .active(|this| this.bg(colors.element_active))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_remote_port(port_number, cx);
                                this.focus_dialog_index(0, window, cx);
                            }))
                            .child(
                                Icon::new(if active {
                                    IconName::Check
                                } else {
                                    IconName::ArrowRightLeft
                                })
                                .size(IconSize::XSmall)
                                .color(if active {
                                    Color::Default
                                } else {
                                    Color::Muted
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
                    .child(text_small("Remote Port").color(Color::Muted))
                    .child(field)
                    .when_some(error.clone(), |this, message| {
                        this.child(text_small(message).color(Color::Error))
                    })
                    .child(text_small("Local Port").color(Color::Muted))
                    .child(local_field)
                    .when_some(local_error, |this, message| {
                        this.child(text_small(message).color(Color::Error))
                    })
                    .child(
                        text_small("Leave Local Port empty to assign a free local port.")
                            .color(Color::Muted),
                    )
                    .child(Self::dialog_actions(
                        self.dialog_button(
                            "dialog-forward-cancel",
                            "Cancel",
                            super::PORT_FORWARD_CANCEL_FOCUS,
                            ButtonStyle::Outlined,
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
                            ButtonStyle::Tinted(TintColor::Accent),
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
                    .child(text_small("Replica Count").color(Color::Muted))
                    .child(field)
                    .when_some(*error, |this, message| {
                        this.child(text_small(message).color(Color::Error))
                    })
                    .child(Self::dialog_actions(
                        self.dialog_button(
                            "dialog-scale-cancel",
                            "Cancel",
                            1,
                            ButtonStyle::Outlined,
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
                            ButtonStyle::Tinted(TintColor::Accent),
                            cx.listener(|this, _, window, cx| {
                                this.focus_dialog_index(2, window, cx);
                                this.confirm_scale(window, cx)
                            }),
                            cx,
                        ),
                    ));
            }
            Dialog::HotbarBankName {
                index,
                input,
                error,
            } => {
                let editing = index.is_some();
                let title = if editing {
                    "Rename Bank"
                } else {
                    "Create Bank"
                };
                let confirm = if editing { "Rename" } else { "Create" };
                let field = self.render_dialog_input("dialog-bank-input", input, 0, cx);
                card = self
                    .dialog_shell(
                        Role::Dialog,
                        title,
                        text_small("Use Alt+1 through Alt+9 to switch contexts in this bank."),
                        window,
                        cx,
                    )
                    .child(field)
                    .when_some(error.clone(), |this, message| {
                        this.child(text_small(message).color(Color::Error))
                    })
                    .child(Self::dialog_actions(
                        self.dialog_cancel_button("dialog-bank-cancel", 1, cx),
                        self.dialog_button(
                            "dialog-bank-confirm",
                            confirm,
                            2,
                            ButtonStyle::Tinted(TintColor::Accent),
                            cx.listener(|this, _, window, cx| {
                                this.confirm_hotbar_bank_name(window, cx)
                            }),
                            cx,
                        ),
                    ));
            }
            Dialog::HotbarRemove { name, .. } => {
                let title = format!("Remove Bank \"{name}\"?");
                card = self
                    .dialog_shell(
                        Role::AlertDialog,
                        title,
                        text("This removes the bank and its slots. It does not remove contexts from your kubeconfig."),
                        window,
                        cx,
                    )
                    .aria_label(format!("Remove Bank {name}?"))
                    .child(Self::dialog_actions(
                        self.dialog_cancel_button("dialog-bank-remove-cancel", 0, cx),
                        self.dialog_button(
                            "dialog-bank-remove-confirm",
                            "Remove",
                            1,
                            ButtonStyle::Tinted(TintColor::Error),
                            cx.listener(|this, _, window, cx| {
                                this.confirm_hotbar_remove(window, cx)
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
            .bg(design::surface::backdrop(cx))
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

    /// The edges of the scrolling list: a scroll affordance and the fades that say the
    /// results continue past the window.
    ///
    /// The shared `Scrollbars` component keeps its thumb state in keyed window state and needs
    /// a `&mut Window`, which this `&self` render path cannot hand it. The list's own numbers
    /// and the handle it already tracks keep the thumb honest about the scroll position, and
    /// both overlays use the theme's scrollbar roles, so they follow Light and Dark instead of
    /// a fixed color.
    fn render_palette_list_edges(
        &self,
        metrics: &PaletteMetrics,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let colors = cx.theme().colors();
        // The list's own numbers answer before the first layout; the handle answers exactly
        // from the second frame on, and it is what the thumb has to agree with.
        let measured = self.palette_scroll.bounds().size.height;
        let viewport = if measured > px(0.0) {
            f32::from(measured)
        } else {
            metrics.list_viewport
        };
        let measured_overflow = self.palette_scroll.max_offset().y;
        let overflow = if measured_overflow > px(0.0) {
            f32::from(measured_overflow)
        } else {
            metrics.list_content - viewport
        };
        if viewport <= 0.0 || overflow <= 0.0 {
            return Vec::new();
        }
        let scrolled = (-f32::from(self.palette_scroll.offset().y) / overflow).clamp(0.0, 1.0);
        let surface = colors.elevated_surface_background;
        let mut edges = Vec::new();
        // A soft edge keeps a half-visible row reading as more content instead of as a row
        // clipped by the window.
        if scrolled > 0.0 {
            edges.push(Self::palette_edge_fade(surface, true));
        }
        if scrolled < 1.0 {
            edges.push(Self::palette_edge_fade(surface, false));
        }
        // The thumb keeps the theme's scrollbar width, so it reads as the scrollbar it stands
        // in for, and it never thins below a row.
        let height = (viewport * viewport / (viewport + overflow))
            .max(f32::from(design::size::ROW))
            .min(viewport);
        edges.push(
            div()
                .absolute()
                .right_0()
                .top(px((viewport - height) * scrolled))
                .w(ui::ScrollbarStyle::Regular.to_pixels())
                .h(px(height))
                .rounded_full()
                .bg(colors.scrollbar_thumb_background)
                .into_any_element(),
        );
        edges
    }

    /// One edge fade: bands of the card surface that thin out away from the window edge, so a
    /// row the window cuts in half reads as more content instead of as a broken row. The bands
    /// are stacked in element order rather than angled, so the direction of the fade is stated
    /// in the layout instead of in a gradient angle.
    fn palette_edge_fade(surface: gpui::Hsla, at_top: bool) -> AnyElement {
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
        let colors = cx.theme().colors();
        let mut rows = Vec::new();
        let mut group: Option<&str> = None;
        for (index, command) in matches.iter().enumerate() {
            if group != Some(command.group.as_ref()) {
                group = Some(command.group.as_ref());
                rows.push(
                    h_flex()
                        .id(("command-group", index))
                        .w_full()
                        .h(design::size::ROW)
                        .flex_none()
                        // Space above each header, so the groups read as groups.
                        .mt(design::space::SM)
                        .px(design::space::MD)
                        .items_center()
                        .child(
                            // A group header introduces a set, so it takes the section role
                            // rather than the body role its rows use. It was body text in a
                            // lighter weight, which measured 21 device pixels of ink against
                            // the row labels' 20: the label that names a group was quieter than
                            // the items it names.
                            section_text(command.group.clone())
                                .weight(gpui::FontWeight::SEMIBOLD)
                                .color(Color::Muted),
                        )
                        .into_any_element(),
                );
            }
            // The hint has to name a key the shell surface owns. Every business binding is
            // released inside the palette, so a palette-scoped lookup answers with nothing and
            // the row would draw an empty keycap shell where the shortcut belongs. A command
            // with no key in the shell gets an empty right column instead.
            let trailing: Option<AnyElement> = command.binding.and_then(|binding| {
                let action = (binding.make_action)();
                keymap::binding_for_context(action.as_ref().name(), "Shell", cx)
                    .and_then(|chord| keybinding(&[chord.as_str()]))
                    .map(|binding| {
                        KeybindingHint::new(binding, colors.elevated_surface_background)
                            .into_any_element()
                    })
            });
            // The current row carries its check and nothing else. The badge slot stays for
            // commands that genuinely cannot run, where the reason is the point.
            let current = super::palette_command_is_current(command);
            let trailing = trailing.or_else(|| match &command.run {
                CommandRun::Unavailable { badge, .. } if !current => {
                    Some(text_small(*badge).color(Color::Muted).into_any_element())
                }
                _ => None,
            });
            let command_id = command.id.clone();
            let label = if current {
                format!("{}, current", command.label)
            } else {
                command.label.to_string()
            };
            rows.push(
                h_flex()
                    .id(("command", index))
                    .w_full()
                    .h(design::size::ROW)
                    .flex_none()
                    // The selected slab is inset by a `space::XS` and carries the palette's own
                    // radius, so it stays one continuous row and never paints outside a rounded
                    // corner of the card.
                    .px(design::space::XS)
                    .rounded_lg()
                    .cursor_pointer()
                    .role(Role::ListBoxOption)
                    .aria_label(label)
                    .aria_selected(index == selected)
                    .aria_position_in_set(index + 1)
                    .aria_size_of_set(matches.len())
                    .when(index == selected, |this| this.bg(colors.element_selected))
                    .hover(|this| this.bg(colors.element_hover))
                    .active(|this| this.bg(colors.element_active))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.run_command(command_id.clone(), window, cx)
                    }))
                    .child(
                        // The inset above plus this padding puts the label, the check, and the
                        // shortcut exactly where the card padding already put them.
                        h_flex()
                            .flex_1()
                            .min_w(px(0.0))
                            .px(design::space::SM)
                            .gap(design::space::SM)
                            .items_center()
                            // The check follows the row that is current, not the row the
                            // keyboard is on. Bound to the selection it ticked whatever the
                            // cursor happened to rest on, so the card could tick Pods while its
                            // own scope line said the view was Overview.
                            .child(
                                div()
                                    .w(design::space::MD)
                                    .flex_none()
                                    .children(current.then(|| {
                                        Icon::new(IconName::Check)
                                            .size(IconSize::XSmall)
                                            .color(Color::Default)
                                    })),
                            )
                            .child(
                                Icon::new(command.icon)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(text(command.label.clone()))
                            .child(div().flex_1())
                            .children(trailing),
                    )
                    .into_any_element(),
            );
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use theme::LoadThemes;

    use crate::update::{UpdateActions, UpdateUiState};

    use super::*;

    /// The file under test, for the checks that have nothing else to read.
    ///
    /// A token swap has no bounds to measure and no event to simulate, so the only thing left is
    /// to read the line back. The comparisons below ignore whitespace, so a reformat cannot fail
    /// them, and a change of role fails them.
    const SOURCE: &str = include_str!("panels.rs");

    #[gpui::test]
    fn toast_bounds_avoid_status_bar_and_open_dock_at_supported_sizes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
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
            cx.simulate_resize(gpui::size(width, height));
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
    #[gpui::test]
    fn resource_search_is_centred_on_the_window_not_the_centre_column(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        let width = px(1440.0);
        cx.simulate_resize(gpui::size(width, px(900.0)));
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
        cx.simulate_click(tree.center(), gpui::Modifiers::none());
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
    /// `popovers.md` says not to show another view over a popover. The toast and the two status
    /// panels were both anchored to the trailing bottom corner with the same 48px inset, so the
    /// toast covered the notification centre's header row and the port-forward panel's
    /// `New Port Forward…` button, and the toast's own dismiss button with them.
    #[gpui::test]
    fn the_toast_never_lands_on_an_open_status_panel(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        let width = px(960.0);
        cx.simulate_resize(gpui::size(width, px(640.0)));
        shell.update(cx, |shell, cx| {
            shell.toast(
                "The app could not start the port forward. Try again, then check the pod."
                    .to_owned(),
                design::Severity::Error,
                cx,
            );
        });
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| shell.open_port_forward_panel(window, cx));
        });
        cx.run_until_parked();

        let toast = cx
            .debug_bounds("shell-toast")
            .expect("the toast must be laid out");
        let panel = cx
            .debug_bounds("port-forward-panel")
            .expect("the port forward panel must be laid out");
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
    #[gpui::test]
    fn a_toast_with_a_recovery_offers_it(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
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
    #[gpui::test]
    fn the_top_bar_counts_the_notifications_it_shows(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
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
    #[gpui::test]
    fn the_tree_disclosure_is_inside_the_sidebar(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (_shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.simulate_resize(gpui::size(px(1440.0), px(900.0)));
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
            IconName::Screen,
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

    #[gpui::test]
    fn overflowing_center_tabs_keep_first_and_last_reachable(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.simulate_resize(gpui::size(px(960.0), px(640.0)));
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
    #[gpui::test]
    fn the_cluster_name_outranks_the_toolbar_chrome(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        shell.update(cx, |shell, cx| {
            shell.clusters = vec!["kind-k8s-gpui-development-cluster-long-name".into()];
            shell.active_cluster = 0;
            cx.notify();
        });

        for width in [px(960.0), px(1440.0), px(1920.0)] {
            cx.simulate_resize(gpui::size(width, px(900.0)));
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
    #[gpui::test]
    fn connection_failure_offers_retry_and_reload_kubeconfigs(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
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
    #[gpui::test]
    fn kubeconfig_warning_agrees_with_the_source_count(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
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
    #[gpui::test]
    fn a_build_that_cannot_update_itself_keeps_the_notice_off_the_table(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.simulate_resize(gpui::size(px(960.0), px(640.0)));
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
    #[gpui::test]
    fn the_update_notice_opens_once_and_clears_the_first_table_row(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        shell.update(cx, |shell, cx| {
            shell.set_update_actions(UpdateActions::new(|_| {}, |_| {}, |_| {}), cx);
            shell.set_update_state(UpdateUiState::new(UpdatePhase::Unsupported), cx);
        });
        cx.simulate_resize(gpui::size(px(1440.0), px(800.0)));
        cx.run_until_parked();

        let card = cx
            .debug_bounds("update-strip")
            .expect("update card is laid out");
        let panel = cx
            .debug_bounds("center-tab-panel")
            .expect("center tab panel is laid out");
        // The header band and the first row together are two row heights.
        assert!(
            card.top() >= panel.top() + design::size::ROW * 2,
            "the notice must not cover the first table row"
        );
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
    #[gpui::test]
    fn the_active_tab_states_are_three_washes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (_shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.update(|_, cx| {
            let colors = cx.theme().colors();
            let rest = design::surface::tab_active(cx);
            let hover = tab_accent_wash(colors, false);
            let pressed = tab_accent_wash(colors, true);
            assert_ne!(hover, pressed, "hover and press must be tellable apart");
            assert_ne!(rest, hover);
            assert_ne!(rest, pressed);
            // The wash is quieter than the slab it replaced and louder than nothing, measured
            // against the surface the active tab actually paints on.
            for (name, surface) in [
                ("tab bar", design::surface::tab_bar(cx)),
                ("tab active", design::surface::tab_active(cx)),
            ] {
                for (state, wash) in [("hover", hover), ("press", pressed)] {
                    let ratio = ui::utils::calculate_contrast_ratio(
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
        assert!(design::text::SECTION > design::text::BODY);
        let rows = SOURCE
            .split("fn render_command_rows")
            .nth(1)
            .expect("the command row renderer");
        assert!(
            rows.contains("section_text(command.group.clone())"),
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
        assert!(design::text::PANEL_TITLE > design::text::METADATA);
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
    #[gpui::test]
    fn an_empty_palette_offers_the_clear_search_control(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.simulate_resize(gpui::size(px(1440.0), px(900.0)));
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
        cx.simulate_click(button.center(), gpui::Modifiers::none());
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
    #[test]
    fn a_toggled_button_declares_the_style_its_selected_state_renders_in() {
        let toggle = [".toggle_", "state("].concat();
        let selected = ["selected_", "style("].concat();
        let mut declared = 0;
        let mut found = 0;
        let mut offset = 0;
        while let Some(at) = SOURCE[offset..].find(&toggle) {
            let at = offset + at;
            found += 1;
            // The style is set on the same builder chain, so it has to appear before the next
            // toggle; a window that ran past it would credit this call with the next one's fix.
            let end = SOURCE[at + toggle.len()..]
                .find(&toggle)
                .map_or(SOURCE.len(), |next| at + toggle.len() + next)
                .min(at + 500);
            if SOURCE[at..end].contains(&selected) {
                declared += 1;
            }
            offset = at + toggle.len();
        }
        assert!(
            found > 0,
            "the toolbar and the switchers are toggles, so this must find one"
        );
        assert_eq!(
            declared, found,
            "every `toggle_state` needs a `selected_style`; the rest fall through to a style this \
             repo has never looked at"
        );
    }

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
