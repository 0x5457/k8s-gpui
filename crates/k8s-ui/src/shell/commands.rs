//! Static command definitions and fuzzy filtering for the command palette.
//!
//! Each command uses [`CommandRun`] to define how it runs. Action commands dispatch after the
//! palette closes. Unavailable commands remain searchable and show why they cannot run.
//!
//! # What earns a row
//!
//! A row is for a command a person has a reason to reach for through the palette: a jump somewhere,
//! an action on the object they picked, or a state they think in. A control that is already on
//! screen under its own name does not get a second name here. The tab strip's menu says "Close All
//! Tabs", the row's own menu is a button on the row, and the settings window has the button that
//! opens the keymap file. Where such a
//! control also owns a chord, the chord stays in the keymap and the surface keeps the name; a
//! second label for it in the palette is how one command ends up under two names in two places.
//!
//! # Grouping
//!
//! Every row is grouped by the thing it acts on — the cluster, the resource the reader picked, the
//! tab strip, the panels around them, a Helm release, the text being edited, the keymap
//! file, the application — and the group is named after that thing, so a person who knows what they
//! want to act on can predict where it is before they type a word. The blocks run in browse order
//! and each is written once, so a row cannot end up outside its block.
//!
//! Ids are the internal namespace and are not part of this: they predate the grouping and several
//! of them still say `pod.` for a command that acts on the whole resource view. They stay as they
//! are because the shell's own tests name them, and renaming them is a mechanical follow-up that
//! needs those tests updated with them.
//!
//! Icons follow one rule: the icon names the object a command acts on, and no two commands
//! share one. A command that changes meaning must change icon, or two rows read as the same
//! action. Glyphs come from the shared `gpui_kit::assets::IconName` set.

use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::{Action, Context, Entity, SharedString, Window};

use super::{
    ApplyYaml, CheckForUpdates, CloseTab, CopySelectedPodName, Cut, DescribeSelection,
    ExecSelection, FocusYaml, MoveTabLeft, MoveTabRight, NextTab, OpenContextSwitcher, OpenEvents,
    OpenForwards, OpenLogs, OpenNamespaceSwitcher, OpenOverview, OpenResourceKindSwitcher,
    OpenServiceAccount, PaletteScope, Paste, PauseUpdates, PortForwardSelection, PreviousTab, Redo,
    RefreshView, ReloadKeymap, ReloadKubeconfigs, RestartSelection, RestartToUpdate, ResumeUpdates,
    ScaleSelection, SearchResources, SelectAll, Shell, TabView, ToggleDock, ToggleLeftPanel,
    ToggleNotifications, ToggleRightPanel, ToggleTheme, Undo, UseDarkTheme, UseKeymapPreset,
    UseLightTheme, UseSystemTheme,
};
use crate::design::Severity;
use crate::panels::helm::{HELM_COMMAND_FAILED, HelmCapability, HelmReleaseAction, HelmView};
use crate::panels::inspector::{
    ConfirmApply, CopyValue, CopyYaml, MetricsRange1h, MetricsRange1m, MetricsRange6h,
    MetricsRange7d, MetricsRange15m, MetricsRange24h, NextProblem, ReloadActiveTab, RetryMetrics,
    RevertYaml, ToggleValueExpansion,
};
use crate::settings::OpenSettings;

/// Reason shown when no updater capability is available.
pub const UPDATER_UNAVAILABLE_REASON: &str =
    "This build cannot update itself. Download a newer k8s-gpui release and replace it.";
pub const CHECK_FOR_UPDATES_COMMAND_ID: &str = "update.check";
pub const RESTART_TO_UPDATE_COMMAND_ID: &str = "update.restart";

/// Shell command handler with captured state.
pub type ShellHandler = Rc<dyn Fn(&mut Shell, &mut Window, &mut Context<Shell>)>;

#[derive(Clone)]
pub enum CommandRun {
    /// Dispatch the action after the palette closes and focus returns.
    Action(fn() -> Box<dyn Action>),
    /// Show a badge and explain why the command cannot run.
    Unavailable {
        badge: &'static str,
        reason: &'static str,
    },
    /// Run an inline Shell command after the palette closes.
    Shell(fn(&mut Shell, &mut Window, &mut Context<Shell>)),
    /// Run a Shell command with captured state.
    ShellFn(ShellHandler),
}

impl std::fmt::Debug for CommandRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Action(_) => f.write_str("Action(..)"),
            Self::Unavailable { badge, .. } => {
                f.debug_struct("Unavailable").field("badge", badge).finish()
            }
            Self::Shell(_) => f.write_str("Shell(..)"),
            Self::ShellFn(_) => f.write_str("ShellFn(..)"),
        }
    }
}

pub type ActionFactory = fn() -> Box<dyn Action>;

#[derive(Clone, Copy, Debug)]
pub struct CommandBinding {
    pub make_action: ActionFactory,
}

#[derive(Clone, Debug)]
pub struct Command {
    pub id: SharedString,
    pub label: SharedString,
    pub group: SharedString,
    pub icon: IconName,
    pub run: CommandRun,
    pub binding: Option<CommandBinding>,
}

impl Command {
    /// Name of the action this command dispatches, or `None` when it runs a Shell
    /// command instead. The menu parity sweep compares action names, so a row
    /// without one is a palette-only command.
    pub fn action_name(&self) -> Option<String> {
        match &self.run {
            CommandRun::Action(make_action) => Some(make_action().name().to_string()),
            _ => None,
        }
    }

    /// True when the command starts something that removes state, so it must not
    /// own a bare key.
    pub fn is_destructive(&self) -> bool {
        self.id.starts_with("helm.uninstall")
    }
}

fn with_action_binding(mut command: Command) -> Command {
    if let CommandRun::Action(make_action) = &command.run {
        command.binding = Some(CommandBinding {
            make_action: *make_action,
        });
    }
    command
}

/// Every menu title the native menu bar offers for a shell-owned action.
///
/// The native menu is built in the app binary, which depends on this crate and
/// not the other way round, so this table is the palette side of the
/// `Menu / Command parity` contract: one title per action, and the sweep below
/// fails here when a menu entry has no palette row with the same title.
pub const NATIVE_MENU_TITLES: &[(&str, &str)] = &[
    // App menu.
    ("Settings\u{2026}", "k8s_app::OpenSettings"),
    ("Toggle light/dark theme", "k8s_shell::ToggleTheme"),
    ("Use light theme", "k8s_shell::UseLightTheme"),
    ("Use dark theme", "k8s_shell::UseDarkTheme"),
    ("Use Lens keymap", "k8s_shell::UseKeymapPreset"),
    ("Use VS Code keymap", "k8s_shell::UseKeymapPreset"),
    // File menu.
    ("Refresh view", "k8s_shell::RefreshView"),
    ("Reload kubeconfigs", "k8s_shell::ReloadKubeconfigs"),
    ("Reload keymap", "k8s_shell::ReloadKeymap"),
    ("Close tab", "k8s_shell::CloseTab"),
    // Edit menu. The standard editing commands are menu entries, so they are
    // palette rows too.
    ("Undo", "k8s_shell::Undo"),
    ("Redo", "k8s_shell::Redo"),
    ("Cut", "k8s_shell::Cut"),
    ("Copy", "k8s_shell::Copy"),
    ("Paste", "k8s_shell::Paste"),
    ("Select All", "k8s_shell::SelectAll"),
    ("Focus YAML", "k8s_shell::FocusYaml"),
    ("Apply YAML changes…", "k8s_shell::ApplyYaml"),
    ("Describe selected resource", "k8s_shell::DescribeSelection"),
    (
        "Open Service Account for selected Pod",
        "k8s_shell::OpenServiceAccount",
    ),
    ("Show logs for selected Pod", "k8s_shell::OpenLogs"),
    ("Show events for selected Pod", "k8s_shell::OpenEvents"),
    ("Exec in selected Pod", "k8s_shell::ExecSelection"),
    (
        "Start port forward for selected Pod…",
        "k8s_shell::PortForwardSelection",
    ),
    ("Restart selected resource", "k8s_shell::RestartSelection"),
    ("Scale selected resource…", "k8s_shell::ScaleSelection"),
    // The menu title is corrected to name every resource view, not only Pods.
    (
        "Copy selected resource name",
        "k8s_shell::CopySelectedPodName",
    ),
    // View menu. The command that opens the palette is a menu entry on macOS and nowhere else:
    // a row for it inside the palette can only close the list the reader is looking at.
    ("Switch context…", "k8s_shell::OpenContextSwitcher"),
    ("Switch namespace…", "k8s_shell::OpenNamespaceSwitcher"),
    (
        "Choose resource kind…",
        "k8s_shell::OpenResourceKindSwitcher",
    ),
    ("Open cluster overview", "k8s_shell::OpenOverview"),
    ("Open port forwards", "k8s_shell::OpenForwards"),
    ("Search cluster resources…", "k8s_shell::SearchResources"),
    ("Toggle sidebar", "k8s_shell::ToggleLeftPanel"),
    ("Toggle inspector", "k8s_shell::ToggleRightPanel"),
    ("Toggle dock", "k8s_shell::ToggleDock"),
    ("Toggle notifications", "k8s_shell::ToggleNotifications"),
    // Window menu.
    ("Previous tab", "k8s_shell::PreviousTab"),
    ("Next tab", "k8s_shell::NextTab"),
    ("Move tab left", "k8s_shell::MoveTabLeft"),
    ("Move tab right", "k8s_shell::MoveTabRight"),
];

/// Menu entries with no palette row, and why.
///
/// * The tab strip owns three of the commands the menu bar offers: its own menu says "Close Other
///   Tabs", "Close All Tabs" and "Pin" under the words the reader is already choosing between, and
///   Shift+F10 opens that menu from the keyboard, so a palette row repeated the same words in a
///   second list a person has to search.
/// * `ToggleCommandPalette` is the command that opens the palette. A row for it inside the palette
///   can only close the list the reader is looking at, which is what Escape already does.
/// * `Hide Others`, `Show All` and `Enter or Exit Full Screen` are dispatched by
///   the macOS-only menu bootstrap. On Linux and Windows a palette row would be a
///   no-op, so they stay menu-only.
pub const MENU_ONLY_ACTIONS: &[&str] = &[
    "k8s_shell::CloseOtherTabs",
    "k8s_shell::CloseAllTabs",
    "k8s_shell::TogglePinTab",
    "k8s_shell::ToggleCommandPalette",
    "k8s_app::HideOthers",
    "k8s_app::ShowAll",
    "k8s_app::ToggleFullScreen",
];

/// Palette rows with no menu entry, and the surface that owns each one.
///
/// An entry is either a gap to close or a deliberate decision, and it always names the owner. A
/// command that acts on one panel row or one focused value can only be named by that panel,
/// because a menu item would offer the same action with nothing selected. Linux and Windows have
/// no native menu bar, so the palette and the keymap carry reachability there.
pub const PALETTE_ONLY_ACTIONS: &[&str] = &[
    // The update strip owns these two.
    "k8s_app::CheckForUpdates",
    "k8s_app::RestartToUpdate",
    // The resource table toolbar owns pause and resume.
    "k8s_shell::PauseUpdates",
    "k8s_shell::ResumeUpdates",
    // The column header popover owns its sort and its "only problems" filter, and prints the
    // chord beside both. An action that prints a key is a command, so it is a palette row too —
    // otherwise the key is discoverable only by finding the popover.
    "k8s_table::SortSelectedColumn",
    "k8s_table::ToggleProblemsOnly",
    // Settings owns the theme choice, and the keymap owns the system theme.
    "k8s_shell::UseSystemTheme",
    // The Inspector owns these. Each one names the active tab or the focused value, so a menu
    // item could only name the panel, and the panel already offers each as a control with its
    // chord beside it. The review keeps Escape to itself, so `CancelApplyReview` is not here and
    // has no palette row.
    "k8s_inspector::ConfirmApply",
    "k8s_inspector::CopyValue",
    "k8s_inspector::CopyYaml",
    "k8s_inspector::MetricsRange1m",
    "k8s_inspector::MetricsRange15m",
    "k8s_inspector::MetricsRange1h",
    "k8s_inspector::MetricsRange6h",
    "k8s_inspector::MetricsRange24h",
    "k8s_inspector::MetricsRange7d",
    "k8s_inspector::NextProblem",
    "k8s_inspector::ReloadActiveTab",
    "k8s_inspector::RevertYaml",
    "k8s_inspector::RetryMetrics",
    "k8s_inspector::ToggleValueExpansion",
];

fn toggle_left_panel_action() -> Box<dyn Action> {
    Box::new(ToggleLeftPanel)
}

fn toggle_right_panel_action() -> Box<dyn Action> {
    Box::new(ToggleRightPanel)
}

fn toggle_dock_action() -> Box<dyn Action> {
    Box::new(ToggleDock)
}

fn describe_action() -> Box<dyn Action> {
    Box::new(DescribeSelection)
}

fn close_tab_action() -> Box<dyn Action> {
    Box::new(CloseTab)
}

fn move_tab_left_action() -> Box<dyn Action> {
    Box::new(MoveTabLeft)
}

fn move_tab_right_action() -> Box<dyn Action> {
    Box::new(MoveTabRight)
}

fn focus_yaml_action() -> Box<dyn Action> {
    Box::new(FocusYaml)
}

fn refresh_view_action() -> Box<dyn Action> {
    Box::new(RefreshView)
}

fn search_resources_action() -> Box<dyn Action> {
    Box::new(SearchResources)
}

fn open_settings_action() -> Box<dyn Action> {
    Box::new(OpenSettings)
}

fn reload_kubeconfigs_action() -> Box<dyn Action> {
    Box::new(ReloadKubeconfigs)
}

fn open_context_switcher_action() -> Box<dyn Action> {
    Box::new(OpenContextSwitcher)
}

fn open_namespace_switcher_action() -> Box<dyn Action> {
    Box::new(OpenNamespaceSwitcher)
}

fn open_resource_kind_switcher_action() -> Box<dyn Action> {
    Box::new(OpenResourceKindSwitcher)
}

fn open_overview_action() -> Box<dyn Action> {
    Box::new(OpenOverview)
}

fn open_forwards_action() -> Box<dyn Action> {
    Box::new(OpenForwards)
}

fn apply_yaml_action() -> Box<dyn Action> {
    Box::new(ApplyYaml)
}

fn open_logs_action() -> Box<dyn Action> {
    Box::new(OpenLogs)
}

fn open_events_action() -> Box<dyn Action> {
    Box::new(OpenEvents)
}

fn exec_selection_action() -> Box<dyn Action> {
    Box::new(ExecSelection)
}

fn port_forward_selection_action() -> Box<dyn Action> {
    Box::new(PortForwardSelection)
}

fn open_service_account_action() -> Box<dyn Action> {
    Box::new(OpenServiceAccount)
}

fn restart_selection_action() -> Box<dyn Action> {
    Box::new(RestartSelection)
}

fn scale_selection_action() -> Box<dyn Action> {
    Box::new(ScaleSelection)
}

fn reload_keymap_action() -> Box<dyn Action> {
    Box::new(ReloadKeymap)
}

fn use_lens_keymap_action() -> Box<dyn Action> {
    Box::new(UseKeymapPreset {
        preset: "lens".to_owned(),
    })
}

fn use_vscode_keymap_action() -> Box<dyn Action> {
    Box::new(UseKeymapPreset {
        preset: "vscode".to_owned(),
    })
}

fn check_for_updates_action() -> Box<dyn Action> {
    Box::new(CheckForUpdates)
}

fn restart_to_update_action() -> Box<dyn Action> {
    Box::new(RestartToUpdate)
}

fn pause_updates_action() -> Box<dyn Action> {
    Box::new(PauseUpdates)
}

fn resume_updates_action() -> Box<dyn Action> {
    Box::new(ResumeUpdates)
}

fn copy_selected_name_action() -> Box<dyn Action> {
    Box::new(CopySelectedPodName)
}

fn sort_selected_column_action() -> Box<dyn Action> {
    Box::new(crate::table_view::SortSelectedColumn)
}

fn toggle_problems_only_action() -> Box<dyn Action> {
    Box::new(crate::table_view::ToggleProblemsOnly)
}

fn toggle_theme_action() -> Box<dyn Action> {
    Box::new(ToggleTheme)
}

fn light_theme_action() -> Box<dyn Action> {
    Box::new(UseLightTheme)
}

fn dark_theme_action() -> Box<dyn Action> {
    Box::new(UseDarkTheme)
}

fn next_tab_action() -> Box<dyn Action> {
    Box::new(NextTab)
}

fn previous_tab_action() -> Box<dyn Action> {
    Box::new(PreviousTab)
}

fn toggle_notifications_action() -> Box<dyn Action> {
    Box::new(ToggleNotifications)
}

fn use_system_theme_action() -> Box<dyn Action> {
    Box::new(UseSystemTheme)
}

fn undo_action() -> Box<dyn Action> {
    Box::new(Undo)
}

fn redo_action() -> Box<dyn Action> {
    Box::new(Redo)
}

fn cut_action() -> Box<dyn Action> {
    Box::new(Cut)
}

fn copy_action() -> Box<dyn Action> {
    Box::new(super::Copy)
}

fn paste_action() -> Box<dyn Action> {
    Box::new(Paste)
}

fn select_all_action() -> Box<dyn Action> {
    Box::new(SelectAll)
}

fn reload_active_tab_action() -> Box<dyn Action> {
    Box::new(ReloadActiveTab)
}

fn retry_metrics_action() -> Box<dyn Action> {
    Box::new(RetryMetrics)
}

fn metrics_range_1m_action() -> Box<dyn Action> {
    Box::new(MetricsRange1m)
}

fn metrics_range_6h_action() -> Box<dyn Action> {
    Box::new(MetricsRange6h)
}

fn metrics_range_24h_action() -> Box<dyn Action> {
    Box::new(MetricsRange24h)
}

fn metrics_range_7d_action() -> Box<dyn Action> {
    Box::new(MetricsRange7d)
}

fn metrics_range_15m_action() -> Box<dyn Action> {
    Box::new(MetricsRange15m)
}

fn metrics_range_1h_action() -> Box<dyn Action> {
    Box::new(MetricsRange1h)
}

fn confirm_apply_action() -> Box<dyn Action> {
    Box::new(ConfirmApply)
}

fn revert_yaml_action() -> Box<dyn Action> {
    Box::new(RevertYaml)
}

fn copy_yaml_action() -> Box<dyn Action> {
    Box::new(CopyYaml)
}

fn toggle_value_expansion_action() -> Box<dyn Action> {
    Box::new(ToggleValueExpansion)
}

fn copy_value_action() -> Box<dyn Action> {
    Box::new(CopyValue)
}

fn next_problem_action() -> Box<dyn Action> {
    Box::new(NextProblem)
}

/// One command row as a group writes it: the id, the label a person reads, the icon, and how it
/// runs.
///
/// The group is deliberately not part of the row. [`group`] names it once per block, so a
/// command's category is decided where the block starts instead of being restated on every row,
/// and a row cannot drift out of its block because it was pasted in the wrong place.
type Row = (&'static str, &'static str, IconName, CommandRun);

/// The commands that act on one target, under the name of that target.
fn group(name: &'static str, rows: impl IntoIterator<Item = Row>) -> Vec<Command> {
    rows.into_iter()
        .map(|(id, label, icon, run)| {
            with_action_binding(Command {
                id: SharedString::from(id),
                label: SharedString::from(label),
                group: SharedString::from(name),
                icon,
                run,
                binding: None,
            })
        })
        .collect()
}

/// Build the command list from the available capabilities.
pub fn demo_commands(helm_available: bool) -> Vec<Command> {
    demo_commands_with_updater(helm_available, true)
}

pub fn demo_commands_with_updater(helm_available: bool, updater_available: bool) -> Vec<Command> {
    demo_commands_with_capabilities(
        if helm_available {
            HelmCapability::Available
        } else {
            HelmCapability::NotInstalled
        },
        updater_available,
    )
}

/// Every product command, in the order a reader browses them.
///
/// The blocks are the information architecture: each one is a target a person already has in mind
/// — the cluster they are pointed at, the resource they picked, the tabs they opened, the panels
/// around them — and the block's name is that target. The palette is the app's only labelled
/// surface, so where a command sits in this list is most of how a person learns that the command
/// exists.
pub fn demo_commands_with_capabilities(
    helm: HelmCapability,
    updater_available: bool,
) -> Vec<Command> {
    use CommandRun as Run;
    let mut commands: Vec<Command> = Vec::new();

    commands.extend(group(
        "Cluster",
        [
            (
                "navigation.context",
                "Switch context…",
                IconName::Server,
                Run::Action(open_context_switcher_action as fn() -> Box<dyn Action>),
            ),
            (
                "navigation.namespace",
                "Switch namespace…",
                IconName::Folder,
                Run::Action(open_namespace_switcher_action as fn() -> Box<dyn Action>),
            ),
            (
                "navigation.kind",
                "Choose resource kind…",
                IconName::ListTree,
                Run::Action(open_resource_kind_switcher_action as fn() -> Box<dyn Action>),
            ),
            (
                "view.overview",
                "Open cluster overview",
                IconName::Monitor,
                Run::Action(open_overview_action as fn() -> Box<dyn Action>),
            ),
            (
                "view.search_resources",
                "Search cluster resources…",
                IconName::Search,
                Run::Action(search_resources_action as fn() -> Box<dyn Action>),
            ),
            // Reloads the active view and the catalog behind the sidebar, so it is the cluster's
            // reload and not the table's: F5 already refreshes the table the reader is looking at,
            // and the palette used to offer a second row for that under the name "Refresh
            // Resources".
            (
                "view.refresh",
                "Refresh view",
                IconName::RefreshCw,
                Run::Action(refresh_view_action as fn() -> Box<dyn Action>),
            ),
            (
                "cluster.reload_kubeconfigs",
                "Reload kubeconfigs",
                IconName::FolderSync,
                Run::Action(reload_kubeconfigs_action as fn() -> Box<dyn Action>),
            ),
        ],
    ));

    // Everything a person does to the row they picked, and to the document that row opens. It is
    // one block because it is one question — what can I do to this thing — and because the rows
    // that used to answer it were spread across three blocks named after surfaces (`View`,
    // `Actions`, `Inspector`), so "restart", "logs" and "copy yaml" never appeared together.
    //
    // Every row here dispatches an action that is also a control inside the panel, which is the
    // point: the palette is the only way to run it when the reader is looking somewhere else, and
    // the panel keeps the direct route. Two neighbours of the block are deliberately absent.
    // `CancelApplyReview` is Escape inside the review strip, where the strip's own button reads
    // "Keep editing", so a row named after cancelling a review would be ambiguous in a list with
    // no review open. The port forward panel's Copy URL and Open in Browser are not actions at
    // all: that panel answers Control+c and Control+o on its own selected row.
    commands.extend(group(
        "Resources",
        [
            (
                "pod.describe",
                "Describe selected resource",
                IconName::Info,
                Run::Action(describe_action),
            ),
            (
                "pod.logs",
                "Show logs for selected Pod",
                IconName::BookOpen,
                Run::Action(open_logs_action as fn() -> Box<dyn Action>),
            ),
            (
                "pod.events",
                "Show events for selected Pod",
                IconName::Bell,
                Run::Action(open_events_action as fn() -> Box<dyn Action>),
            ),
            (
                "pod.exec",
                "Exec in selected Pod",
                IconName::Terminal,
                Run::Action(exec_selection_action as fn() -> Box<dyn Action>),
            ),
            (
                "pod.forward_port",
                "Start port forward for selected Pod…",
                IconName::Link,
                Run::Action(port_forward_selection_action as fn() -> Box<dyn Action>),
            ),
            // Next to the command that starts one: a forward is only useful if the list of them is
            // one keystroke away, and two rows in different blocks is how a person searches for
            // "forward" twice.
            (
                "view.forwards",
                "Open port forwards",
                IconName::Table,
                Run::Action(open_forwards_action as fn() -> Box<dyn Action>),
            ),
            (
                "pod.service_account",
                "Open Service Account for selected Pod",
                IconName::UserCheck,
                Run::Action(open_service_account_action as fn() -> Box<dyn Action>),
            ),
            (
                "resource.restart",
                "Restart selected resource",
                IconName::RotateCw,
                Run::Action(restart_selection_action as fn() -> Box<dyn Action>),
            ),
            (
                "resource.scale",
                "Scale selected resource…",
                IconName::Replace,
                Run::Action(scale_selection_action as fn() -> Box<dyn Action>),
            ),
            (
                "pod.copy_name",
                "Copy selected resource name",
                IconName::Copy,
                Run::Action(copy_selected_name_action),
            ),
            (
                "yaml.focus",
                "Focus YAML",
                IconName::FileCode,
                Run::Action(focus_yaml_action),
            ),
            (
                "yaml.apply",
                "Apply YAML changes…",
                IconName::Check,
                Run::Action(apply_yaml_action as fn() -> Box<dyn Action>),
            ),
            // One verb for one result: the Inspector's own commit says "Apply to
            // cluster", and a palette row that asks the user to "Confirm" something
            // they already staged is a ritual word the copy lexicon dropped.
            (
                "inspector.confirm_apply",
                "Apply to cluster",
                IconName::CheckCheck,
                Run::Action(confirm_apply_action as fn() -> Box<dyn Action>),
            ),
            (
                "inspector.revert_yaml",
                "Revert YAML",
                IconName::ReplaceAll,
                Run::Action(revert_yaml_action as fn() -> Box<dyn Action>),
            ),
            (
                "inspector.copy_yaml",
                "Copy YAML",
                IconName::FileText,
                Run::Action(copy_yaml_action as fn() -> Box<dyn Action>),
            ),
            (
                "inspector.next_problem",
                "Next YAML problem",
                IconName::TriangleAlert,
                Run::Action(next_problem_action as fn() -> Box<dyn Action>),
            ),
            // "Reload Active Tab" read as the centre tab strip, which has its own tabs and its own
            // F5 meaning. The Inspector's reload is the panel's, and says so.
            (
                "inspector.reload_tab",
                "Reload inspector tab",
                IconName::PanelTop,
                Run::Action(reload_active_tab_action as fn() -> Box<dyn Action>),
            ),
            (
                "inspector.retry_metrics",
                "Retry metrics",
                IconName::Timer,
                Run::Action(retry_metrics_action as fn() -> Box<dyn Action>),
            ),
            (
                "inspector.metrics_1m",
                "Metrics: last minute",
                IconName::SignalLow,
                Run::Action(metrics_range_1m_action as fn() -> Box<dyn Action>),
            ),
            (
                "inspector.metrics_15m",
                "Metrics: last 15 minutes",
                IconName::SignalMedium,
                Run::Action(metrics_range_15m_action as fn() -> Box<dyn Action>),
            ),
            (
                "inspector.metrics_1h",
                "Metrics: last hour",
                IconName::SignalHigh,
                Run::Action(metrics_range_1h_action as fn() -> Box<dyn Action>),
            ),
            (
                "inspector.metrics_6h",
                "Metrics: last 6 hours",
                IconName::Signal,
                Run::Action(metrics_range_6h_action as fn() -> Box<dyn Action>),
            ),
            (
                "inspector.metrics_24h",
                "Metrics: last 24 hours",
                IconName::CalendarClock,
                Run::Action(metrics_range_24h_action as fn() -> Box<dyn Action>),
            ),
            (
                "inspector.metrics_7d",
                "Metrics: last 7 days",
                IconName::CalendarDays,
                Run::Action(metrics_range_7d_action as fn() -> Box<dyn Action>),
            ),
            (
                "inspector.toggle_value",
                "Expand or collapse value",
                IconName::TextWrap,
                Run::Action(toggle_value_expansion_action as fn() -> Box<dyn Action>),
            ),
            (
                "inspector.copy_value",
                "Copy value",
                IconName::Quote,
                Run::Action(copy_value_action as fn() -> Box<dyn Action>),
            ),
            // These two answer to a chord inside the table, and a chord is not a command list.
            // Someone who learns the app from the palette, or who wants to see what a key does,
            // needs them here too. What did not need a row was the row's own `⋯` menu, which is a
            // button on the row carrying Shift+F10 in its accessible name.
            (
                "table.sort_column",
                "Sort selected column",
                IconName::ListTodo,
                Run::Action(sort_selected_column_action),
            ),
            (
                "table.only_problems",
                "Show only problems",
                // The funnel, not the warning triangle. This command narrows the table, and
                // The design's D5 gives the amber channel to rows that are actually in
                // trouble — a warning glyph on a control that is merely on or off spends
                // the reader's attention before they have read the label. The triangle
                // stays on `inspector.next_problem`, which does point at a real defect.
                IconName::ListFilter,
                Run::Action(toggle_problems_only_action),
            ),
            // One row per state, not a toggle: a person who wants to stop a streaming table and a
            // person who wants it streaming again are in the same place, and neither of them wants
            // to discover which way round the state currently is before they can ask.
            (
                "pod.pause",
                "Pause updates",
                IconName::Pause,
                Run::Action(pause_updates_action),
            ),
            (
                "pod.resume",
                "Resume updates",
                IconName::Play,
                Run::Action(resume_updates_action),
            ),
        ],
    ));

    commands.extend(group(
        "Tabs",
        [
            (
                "tab.close",
                "Close tab",
                IconName::Close,
                Run::Action(close_tab_action as fn() -> Box<dyn Action>),
            ),
            (
                "tab.previous",
                "Previous tab",
                IconName::ChevronLeft,
                Run::Action(previous_tab_action as fn() -> Box<dyn Action>),
            ),
            (
                "tab.next",
                "Next tab",
                IconName::ChevronRight,
                Run::Action(next_tab_action as fn() -> Box<dyn Action>),
            ),
            (
                "tab.move_left",
                "Move tab left",
                IconName::ArrowLeft,
                Run::Action(move_tab_left_action as fn() -> Box<dyn Action>),
            ),
            (
                "tab.move_right",
                "Move tab right",
                IconName::ArrowRight,
                Run::Action(move_tab_right_action as fn() -> Box<dyn Action>),
            ),
        ],
    ));

    commands.extend(group(
        "Layout",
        [
            (
                "sidebar.toggle",
                "Toggle sidebar",
                IconName::PanelLeftOpen,
                Run::Action(toggle_left_panel_action as fn() -> Box<dyn Action>),
            ),
            (
                "inspector.toggle",
                "Toggle inspector",
                IconName::PanelRightOpen,
                Run::Action(toggle_right_panel_action),
            ),
            (
                // The dock is the third panel of one group, and the other two name their edge
                // in the `Panel*Open` family. A bare disclosure chevron is the language of a
                // section header, so beside two panel glyphs it read as a fourth kind of row
                // rather than as the bottom edge of the window.
                "dock.toggle",
                "Toggle dock",
                IconName::PanelBottomOpen,
                Run::Action(toggle_dock_action),
            ),
            (
                "view.notifications",
                "Toggle notifications",
                IconName::BellRing,
                Run::Action(toggle_notifications_action as fn() -> Box<dyn Action>),
            ),
        ],
    ));

    commands.extend(group(
        "Helm",
        vec![
            (
                "helm.open",
                "Open Helm releases",
                IconName::Archive,
                if helm == HelmCapability::Available {
                    Run::Shell(Shell::command_open_helm)
                } else {
                    helm_missing_run(helm)
                },
            ),
            // A release operation never owns a key: it opens the same confirmation dialog as the
            // detail buttons and the release menu, and the destructive one must never be one
            // keystroke away.
            (
                "helm.upgrade",
                HelmReleaseAction::Upgrade.command_label(),
                IconName::ArrowUp,
                helm_release_run(helm, HelmReleaseAction::Upgrade),
            ),
            (
                "helm.rollback",
                HelmReleaseAction::Rollback.command_label(),
                IconName::Clock,
                helm_release_run(helm, HelmReleaseAction::Rollback),
            ),
            (
                "helm.uninstall",
                HelmReleaseAction::Uninstall.command_label(),
                IconName::FileX,
                helm_release_run(helm, HelmReleaseAction::Uninstall),
            ),
        ],
    ));

    commands.extend(group(
        "Edit",
        [
            (
                "edit.undo",
                "Undo",
                IconName::Undo,
                Run::Action(undo_action as fn() -> Box<dyn Action>),
            ),
            (
                "edit.redo",
                "Redo",
                IconName::Redo,
                Run::Action(redo_action as fn() -> Box<dyn Action>),
            ),
            (
                "edit.cut",
                "Cut",
                IconName::Scissors,
                Run::Action(cut_action as fn() -> Box<dyn Action>),
            ),
            (
                "edit.copy",
                "Copy",
                IconName::ClipboardCopy,
                Run::Action(copy_action as fn() -> Box<dyn Action>),
            ),
            (
                "edit.paste",
                "Paste",
                IconName::NotepadText,
                Run::Action(paste_action as fn() -> Box<dyn Action>),
            ),
            (
                "edit.select_all",
                "Select All",
                IconName::SquareDashedMousePointer,
                Run::Action(select_all_action as fn() -> Box<dyn Action>),
            ),
        ],
    ));

    commands.extend(group(
        "Keymap",
        [
            (
                "keymap.reload",
                "Reload keymap",
                IconName::Keyboard,
                Run::Action(reload_keymap_action as fn() -> Box<dyn Action>),
            ),
            (
                "keymap.preset.lens",
                "Use Lens keymap",
                IconName::Star,
                Run::Action(use_lens_keymap_action as fn() -> Box<dyn Action>),
            ),
            (
                "keymap.preset.vscode",
                "Use VS Code keymap",
                IconName::Code,
                Run::Action(use_vscode_keymap_action as fn() -> Box<dyn Action>),
            ),
        ],
    ));

    commands.extend(group(
        "Application",
        [
            (
                "theme.toggle",
                "Toggle light/dark theme",
                IconName::ArrowRightLeft,
                Run::Action(toggle_theme_action),
            ),
            // The theme is three values and the shell draws no control for any of them, so each
            // value is a row: a keyboard user must be able to reach the one they want without
            // opening Settings to find an Appearance page. Settings owns the setting; these are
            // the fast path to it, not a second copy of it.
            (
                "theme.light",
                "Use light theme",
                IconName::Eye,
                Run::Action(light_theme_action),
            ),
            (
                "theme.dark",
                "Use dark theme",
                IconName::EyeOff,
                Run::Action(dark_theme_action),
            ),
            (
                "theme.system",
                "Use system theme",
                IconName::AppWindow,
                Run::Action(use_system_theme_action as fn() -> Box<dyn Action>),
            ),
            (
                CHECK_FOR_UPDATES_COMMAND_ID,
                "Check for updates",
                IconName::CloudDownload,
                if updater_available {
                    Run::Action(check_for_updates_action as fn() -> Box<dyn Action>)
                } else {
                    Run::Unavailable {
                        badge: "Unavailable",
                        reason: UPDATER_UNAVAILABLE_REASON,
                    }
                },
            ),
            (
                RESTART_TO_UPDATE_COMMAND_ID,
                "Restart to update",
                IconName::Power,
                if updater_available {
                    Run::Action(restart_to_update_action as fn() -> Box<dyn Action>)
                } else {
                    Run::Unavailable {
                        badge: "Unavailable",
                        reason: UPDATER_UNAVAILABLE_REASON,
                    }
                },
            ),
            (
                "settings.open",
                "Settings\u{2026}",
                IconName::Settings,
                Run::Action(open_settings_action as fn() -> Box<dyn Action>),
            ),
        ],
    ));

    commands
}

/// How a Helm release command runs.
///
/// It goes through the panel's own request path, so it opens the same confirmation dialog as the
/// detail buttons and is refused for a release Helm is already working on.
fn helm_release_run(helm: HelmCapability, action: HelmReleaseAction) -> CommandRun {
    if helm == HelmCapability::Available {
        CommandRun::ShellFn(helm_release_action_handler(action))
    } else {
        helm_missing_run(helm)
    }
}

/// The run a Helm command gets when there is no client to run it with. The row stays searchable
/// and says what the probe found and what to do about it.
fn helm_missing_run(helm: HelmCapability) -> CommandRun {
    CommandRun::Unavailable {
        badge: helm.label(),
        reason: helm.reason().unwrap_or(HELM_COMMAND_FAILED),
    }
}

/// Starts a Helm release operation from the palette.
///
/// The operation goes through the panel's own request path, so it opens the same
/// confirmation dialog as the detail buttons and is refused for a release Helm
/// is already working on.
fn helm_release_action_handler(action: HelmReleaseAction) -> ShellHandler {
    Rc::new(
        move |shell: &mut Shell, window: &mut Window, cx: &mut Context<Shell>| {
            let Some(view) = active_helm_view(shell) else {
                shell.notify(
                "No Helm release is selected. Open the Helm tab, select a release, then try again."
                    .to_owned(),
                Severity::Info,
                None,
                cx,
            );
                return;
            };
            let started = view.update(cx, |view, cx| {
                view.request_release_action(action, window, cx)
            });
            if !started && let Some(reason) = view.read(cx).blocked_reason(action) {
                shell.notify(reason.to_owned(), Severity::Warning, None, cx);
            }
        },
    )
}

/// The Helm view the palette acts on: the active tab when it is a Helm tab, and
/// otherwise the first open one.
fn active_helm_view(shell: &Shell) -> Option<Entity<HelmView>> {
    shell
        .views
        .get(shell.active_tab)
        .and_then(Option::as_ref)
        .and_then(|view| match view {
            TabView::Helm(view) => Some(view.clone()),
            _ => None,
        })
        .or_else(|| {
            shell.views.iter().find_map(|slot| match slot {
                Some(TabView::Helm(view)) => Some(view.clone()),
                _ => None,
            })
        })
}

fn query_terms(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_owned)
        .collect()
}

fn command_search_terms(command: &Command) -> String {
    let id_terms = command
        .id
        .to_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{} {} {}",
        command.label.to_lowercase(),
        command.group.to_lowercase(),
        id_terms
    )
}

fn command_keyword_terms(command: &Command) -> String {
    command.label.to_string()
}

fn command_matches_literal_query(command: &Command, query: &str) -> bool {
    command.label.to_lowercase().contains(query) || command.id.to_lowercase().contains(query)
}

fn query_has_separator(query: &str) -> bool {
    query
        .chars()
        .any(|character| !character.is_alphanumeric() && !character.is_whitespace())
}

#[cfg(test)]
pub(super) fn command_matches_query(command: &Command, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    if query_has_separator(&query) && command_matches_literal_query(command, &query) {
        return true;
    }
    let terms = query_terms(&query);
    if terms.is_empty() || (query_has_separator(&query) && terms.len() == 1) {
        return false;
    }
    let searchable = command_search_terms(command);
    terms
        .iter()
        .all(|term| k8s_core::fuzzy::score(term, &searchable).is_some())
}

fn append_ranked(
    output: &mut Vec<usize>,
    seen: &mut [bool],
    eligible: &[bool],
    ranked: Vec<k8s_core::fuzzy::Ranked>,
    allow: impl Fn(k8s_core::fuzzy::MatchTier) -> bool,
) {
    for ranked in ranked {
        if !eligible[ranked.index] || seen[ranked.index] || !allow(ranked.tier) {
            continue;
        }
        seen[ranked.index] = true;
        output.push(ranked.index);
    }
}

pub fn filter_commands<'a>(commands: &'a [Command], query: &str) -> Vec<&'a Command> {
    filter_commands_with(commands, query, |_| true)
}

pub fn filter_commands_for_scope<'a>(
    commands: &'a [Command],
    query: &str,
    scope: PaletteScope,
) -> Vec<&'a Command> {
    filter_commands_with(commands, query, |command| match scope {
        PaletteScope::Commands => true,
        PaletteScope::Context => command.id.starts_with("context."),
        PaletteScope::Namespace => command.id.starts_with("namespace."),
        PaletteScope::Kind => command.id.starts_with("kind."),
    })
}

fn filter_commands_with<'a, F>(
    commands: &'a [Command],
    query: &str,
    mut include: F,
) -> Vec<&'a Command>
where
    F: FnMut(&Command) -> bool,
{
    let query = query.trim().to_lowercase();
    let mut eligible = vec![true; commands.len()];
    for (index, command) in commands.iter().enumerate() {
        eligible[index] = include(command);
    }
    if query.is_empty() {
        return k8s_core::fuzzy::rank("", commands.iter().map(|command| command.label.as_ref()))
            .into_iter()
            .filter(|ranked| eligible[ranked.index])
            .map(|ranked| &commands[ranked.index])
            .collect();
    }
    if query_has_separator(&query) {
        let literal_matches: Vec<_> = commands
            .iter()
            .enumerate()
            .filter(|(index, command)| {
                eligible[*index] && command_matches_literal_query(command, &query)
            })
            .map(|(_, command)| command)
            .collect();
        if !literal_matches.is_empty() {
            return literal_matches;
        }
    }
    let terms = query_terms(&query);
    if terms.is_empty() || (query_has_separator(&query) && terms.len() == 1) {
        return Vec::new();
    }

    let keyword_texts = commands
        .iter()
        .map(command_keyword_terms)
        .collect::<Vec<_>>();
    let id_texts = commands
        .iter()
        .map(|command| command.id.to_string())
        .collect::<Vec<_>>();
    let category_texts = commands
        .iter()
        .map(|command| command.group.to_string())
        .collect::<Vec<_>>();
    let all_texts = commands
        .iter()
        .map(command_search_terms)
        .collect::<Vec<_>>();
    for term in &terms {
        let mut term_eligible = vec![false; commands.len()];
        for ranked in k8s_core::fuzzy::rank(term.as_str(), keyword_texts.iter().map(String::as_str))
        {
            term_eligible[ranked.index] = true;
        }
        for ranked in k8s_core::fuzzy::rank(term.as_str(), id_texts.iter().map(String::as_str)) {
            term_eligible[ranked.index] = true;
        }
        for ranked in
            k8s_core::fuzzy::rank(term.as_str(), category_texts.iter().map(String::as_str))
        {
            term_eligible[ranked.index] = true;
        }
        for ranked in k8s_core::fuzzy::rank(term.as_str(), all_texts.iter().map(String::as_str)) {
            term_eligible[ranked.index] = true;
        }
        for (is_eligible, matched) in eligible.iter_mut().zip(term_eligible) {
            *is_eligible &= matched;
        }
    }

    let first_term = terms[0].as_str();
    let keyword_ranked =
        k8s_core::fuzzy::rank(first_term, keyword_texts.iter().map(String::as_str));
    let id_ranked = k8s_core::fuzzy::rank(first_term, id_texts.iter().map(String::as_str));
    let category_ranked =
        k8s_core::fuzzy::rank(first_term, category_texts.iter().map(String::as_str));
    let all_ranked = k8s_core::fuzzy::rank(first_term, all_texts.iter().map(String::as_str));
    let mut output = Vec::new();
    let mut seen = vec![false; commands.len()];
    append_ranked(&mut output, &mut seen, &eligible, keyword_ranked, |tier| {
        tier != k8s_core::fuzzy::MatchTier::Fuzzy
    });
    append_ranked(&mut output, &mut seen, &eligible, id_ranked, |tier| {
        tier != k8s_core::fuzzy::MatchTier::Fuzzy
    });
    append_ranked(&mut output, &mut seen, &eligible, category_ranked, |tier| {
        tier != k8s_core::fuzzy::MatchTier::Fuzzy
    });
    append_ranked(&mut output, &mut seen, &eligible, all_ranked, |_| true);
    output.into_iter().map(|index| &commands[index]).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::panels::helm::{HELM_NOT_INSTALLED, HELM_PROBE_FAILED, HELM_PROBE_TIMED_OUT};

    /// The shipped keymap as the loader reads it: one row per binding, carrying the
    /// context it holds under, the chord that fires it, and the action it runs.
    ///
    /// The loader's own file type is private to `keymap`, and these tests are about
    /// what the asset says rather than about what this machine ends up bound, so the
    /// asset is read the way the loader reads it: JSONC, a list of sections, each an
    /// optional `context` and a `bindings` map from keystrokes to an action name or a
    /// two-element `[action, input]` pair.
    fn keymap_bindings() -> Vec<(String, String, String)> {
        let sections: Vec<serde_json::Value> =
            crate::settings::parse_jsonc(crate::keymap::default_keymap_source())
                .expect("the default keymap must parse");
        sections
            .iter()
            .flat_map(|section| {
                let context = section["context"].as_str().unwrap_or_default().to_owned();
                section["bindings"]
                    .as_object()
                    .into_iter()
                    .flat_map(|bindings| {
                        // Each row carries the section's context, and the iterator below runs
                        // once per binding, so the context is copied into the row closure
                        // rather than moved into it.
                        let context = context.clone();
                        bindings.iter().filter_map(move |(keystrokes, action)| {
                            let name = match action {
                                serde_json::Value::String(name) => Some(name.clone()),
                                serde_json::Value::Array(items) if items.len() == 2 => {
                                    items[0].as_str().map(str::to_owned)
                                }
                                _ => None,
                            }?;
                            Some((context.clone(), keystrokes.clone(), name))
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Two rows with one glyph read as the same action, so every command needs its own icon.
    #[test]
    fn every_command_has_its_own_icon() {
        // `IconName` is a unit-variant enum with no `Ord`, so the variant name is the
        // identity here: it is what the glyph is, and it is what the message prints.
        let mut owners: BTreeMap<String, SharedString> = BTreeMap::new();
        for command in demo_commands(true) {
            let icon = format!("{:?}", command.icon);
            let label = command.label.clone();
            if let Some(other) = owners.insert(icon.clone(), label.clone()) {
                panic!("{other} and {label} both use {icon}");
            }
        }
    }

    /// Two rows with one glyph read as the same action, and the tab rows are the easiest to
    /// confuse: close, walk and reorder all sit in one strip.
    #[test]
    fn tab_close_commands_use_distinct_icons() {
        let commands = demo_commands(true);
        let icon_for = |id: &str| {
            commands
                .iter()
                .find(|command| command.id == id)
                .expect("command exists")
                .icon
        };
        // Closing the tab the reader is on, walking the strip, and reordering are four different
        // actions, and reordering a tab is not forwarding a port.
        assert_ne!(icon_for("tab.close"), icon_for("tab.move_right"));
        assert_ne!(icon_for("tab.move_right"), icon_for("tab.move_left"));
        assert_ne!(icon_for("tab.next"), icon_for("tab.previous"));
        assert_ne!(icon_for("pod.forward_port"), icon_for("tab.move_right"));
    }

    /// A control that is already on screen under its own name does not get a palette row.
    ///
    /// These eight were each a second name for something else: three tab commands the tab strip's
    /// own menu already offers, the table's row menu and the table's own refresh, the keymap file
    /// button in Settings, and the palette opener itself. Every one of them
    /// still runs from its chord or its control; what is gone is the second name.
    #[test]
    fn a_surface_that_already_names_the_command_owns_it() {
        let commands = demo_commands(true);
        for id in [
            "tab.close_others",
            "tab.close_all",
            "tab.toggle_pin",
            "table.refresh",
            "table.row_actions",
            "view.command_palette",
            "keymap.create_or_show",
        ] {
            assert!(
                !commands.iter().any(|command| command.id == id),
                "{id} is named by the surface that owns it"
            );
        }
    }

    #[test]
    fn empty_query_keeps_every_command() {
        let commands = demo_commands(true);
        assert_eq!(filter_commands(&commands, "").len(), commands.len());
        assert_eq!(filter_commands(&commands, "   ").len(), commands.len());
    }

    #[test]
    fn filtering_is_case_insensitive_and_matches_id() {
        let commands = demo_commands(true);
        // The query is uppercase, and the rows it matches span three groups, so the rows that
        // prove case-insensitive matching are named instead of listed in rank order.
        let labels: Vec<&str> = filter_commands(&commands, "INSPECT")
            .iter()
            .map(|command| command.label.as_ref())
            .collect();
        for label in [
            "Toggle inspector",
            "Open Service Account for selected Pod",
            "Copy YAML",
        ] {
            assert!(
                labels.contains(&label),
                "{label} must match a query in any case"
            );
        }

        let labels: Vec<&str> = filter_commands(&commands, "dock.")
            .iter()
            .map(|command| command.label.as_ref())
            .collect();
        assert_eq!(labels, vec!["Toggle dock"]);
    }

    fn ranked_test_command(id: &str, label: &str, group: &str) -> Command {
        Command {
            id: SharedString::from(id),
            label: SharedString::from(label),
            group: SharedString::from(group),
            icon: IconName::Server,
            run: CommandRun::Shell(|_, _, _| {}),
            binding: None,
        }
    }

    #[test]
    fn ranking_prefers_keyword_then_category_before_fuzzy() {
        let commands = vec![
            ranked_test_command("keyword", "View", "Other"),
            ranked_test_command("category", "Overview", "View"),
            ranked_test_command("fuzzy", "Overview", "Other"),
        ];
        let ids: Vec<String> = filter_commands(&commands, "view")
            .into_iter()
            .map(|command| command.id.to_string())
            .collect();
        assert_eq!(ids, vec!["keyword", "category", "fuzzy"]);
    }

    #[test]
    fn filtering_matches_multi_word_command_terms() {
        let commands = demo_commands(true);
        let labels: Vec<&str> = filter_commands(&commands, "theme light")
            .iter()
            .map(|command| command.label.as_ref())
            .collect();
        assert!(labels.contains(&"Use light theme"));
        let reversed: Vec<&str> = filter_commands(&commands, "light theme")
            .iter()
            .map(|command| command.label.as_ref())
            .collect();
        assert_eq!(reversed, labels);
    }

    #[test]
    fn filtering_matches_a_subsequence_across_command_fields() {
        let commands = demo_commands(true);
        let labels: Vec<&str> = filter_commands(&commands, "gtp")
            .iter()
            .map(|command| command.label.as_ref())
            .collect();
        assert!(labels.contains(&"Toggle light/dark theme"));
    }

    #[test]
    fn fuzzy_filtering_keeps_repeated_queries_stable() {
        let commands = demo_commands(true);
        let first: Vec<String> = filter_commands(&commands, "theme")
            .into_iter()
            .map(|command| command.id.to_string())
            .collect();
        let second: Vec<String> = filter_commands(&commands, "theme")
            .into_iter()
            .map(|command| command.id.to_string())
            .collect();
        assert_eq!(first, second);
    }

    #[test]
    fn no_match_yields_empty() {
        let commands = demo_commands(true);
        assert!(filter_commands(&commands, "zzzz").is_empty());
        assert!(filter_commands(&commands, "---").is_empty());
    }

    #[test]
    fn commands_are_ordered_by_group() {
        let commands = demo_commands(true);
        let mut seen = Vec::new();
        for command in &commands {
            if seen.last() != Some(&command.group) {
                assert!(
                    !seen.contains(&command.group),
                    "command groups must stay contiguous"
                );
                seen.push(command.group.clone());
            }
        }
    }

    /// The Helm command explains when the client is unavailable.
    #[test]
    fn helm_command_reflects_availability() {
        let unavailable = demo_commands(false)
            .into_iter()
            .find(|command| command.id == "helm.open")
            .expect("Helm command exists");
        assert!(matches!(
            unavailable.run,
            CommandRun::Unavailable { reason, .. } if reason == HELM_NOT_INSTALLED
        ));

        let available = demo_commands(true)
            .into_iter()
            .find(|command| command.id == "helm.open")
            .expect("Helm command exists");
        assert!(matches!(available.run, CommandRun::Shell(_)));
    }

    #[test]
    fn helm_command_preserves_probe_failure_copy() {
        for (capability, badge, reason) in [
            (
                HelmCapability::NotInstalled,
                "Not installed",
                HELM_NOT_INSTALLED,
            ),
            (HelmCapability::Timeout, "Timed out", HELM_PROBE_TIMED_OUT),
            (HelmCapability::Error, "Error", HELM_PROBE_FAILED),
        ] {
            let command = demo_commands_with_capabilities(capability, true)
                .into_iter()
                .find(|command| command.id == "helm.open")
                .expect("helm.open command exists");
            assert!(matches!(
                command.run,
                CommandRun::Unavailable {
                    badge: actual_badge,
                    reason: actual_reason,
                } if actual_badge == badge && actual_reason == reason
            ));
        }
    }

    /// Every unavailable command includes a badge and a reason.
    #[test]
    fn unavailable_commands_carry_a_reason() {
        for command in demo_commands(false) {
            if let CommandRun::Unavailable { badge, reason } = command.run {
                assert!(!badge.is_empty(), "{} has no badge", command.id);
                assert!(!reason.is_empty(), "{} has no reason", command.id);
            }
        }
    }

    /// A command whose work is a shell action must dispatch exactly that action.
    ///
    /// Three commands were three near-identical bodies differing only in the id,
    /// the label and the action name; a table says the rule once.
    #[test]
    fn these_commands_dispatch_their_shell_action() {
        let commands = demo_commands(true);
        for (id, label, action) in [
            ("view.refresh", "Refresh view", "k8s_shell::RefreshView"),
            (
                "view.forwards",
                "Open port forwards",
                "k8s_shell::OpenForwards",
            ),
            (
                "cluster.reload_kubeconfigs",
                "Reload kubeconfigs",
                "k8s_shell::ReloadKubeconfigs",
            ),
        ] {
            let command = commands
                .iter()
                .find(|command| command.id == id)
                .unwrap_or_else(|| panic!("{id} command exists"));
            assert_eq!(command.label, label, "{id}");
            let CommandRun::Action(make_action) = &command.run else {
                panic!("{id} must dispatch the shell action");
            };
            assert_eq!(make_action().name(), action, "{id}");
        }
    }

    #[test]
    fn settings_commands_use_truthful_names() {
        let commands = demo_commands(true);
        let settings = commands
            .iter()
            .find(|command| command.id == "settings.open")
            .expect("settings command exists");
        assert_eq!(
            settings.label, "Settings\u{2026}",
            "the palette uses the native menu title"
        );
    }

    #[test]
    fn tab_and_yaml_commands_use_action_paths() {
        let commands = demo_commands(true);
        for (id, label, action_name) in [
            ("tab.close", "Close tab", "k8s_shell::CloseTab"),
            ("tab.previous", "Previous tab", "k8s_shell::PreviousTab"),
            ("tab.next", "Next tab", "k8s_shell::NextTab"),
            ("tab.move_left", "Move tab left", "k8s_shell::MoveTabLeft"),
            (
                "tab.move_right",
                "Move tab right",
                "k8s_shell::MoveTabRight",
            ),
            ("yaml.focus", "Focus YAML", "k8s_shell::FocusYaml"),
        ] {
            let command = commands
                .iter()
                .find(|command| command.id == id)
                .expect("command exists");
            assert_eq!(command.label, label);
            let CommandRun::Action(make_action) = command.run else {
                panic!("{id} must dispatch an action");
            };
            assert_eq!(make_action().name(), action_name);
        }
    }

    #[test]
    fn unavailable_updater_commands_explain_why_they_cannot_run() {
        let commands = demo_commands_with_updater(true, false);
        for id in [CHECK_FOR_UPDATES_COMMAND_ID, RESTART_TO_UPDATE_COMMAND_ID] {
            let command = commands
                .iter()
                .find(|command| command.id == id)
                .expect("update command exists");
            assert!(matches!(
                command.run,
                CommandRun::Unavailable { badge, reason }
                    if badge == "Unavailable" && reason == UPDATER_UNAVAILABLE_REASON
            ));
        }
    }

    #[test]
    fn port_forward_command_uses_sentence_case() {
        let command = demo_commands(true)
            .into_iter()
            .find(|command| command.id == "pod.forward_port")
            .expect("port forward command exists");
        assert_eq!(command.label, "Start port forward for selected Pod…");
    }

    /// Labels must name what the command does in every resource view, not only Pods.
    #[test]
    fn labels_match_the_native_menu_and_the_resource_view() {
        let commands = demo_commands(true);
        for (id, label) in [
            ("navigation.kind", "Choose resource kind…"),
            ("pod.copy_name", "Copy selected resource name"),
            (
                "pod.service_account",
                "Open Service Account for selected Pod",
            ),
            ("settings.open", "Settings\u{2026}"),
            ("theme.toggle", "Toggle light/dark theme"),
            ("keymap.preset.lens", "Use Lens keymap"),
            ("tab.next", "Next tab"),
            ("tab.previous", "Previous tab"),
            ("view.notifications", "Toggle notifications"),
            ("theme.system", "Use system theme"),
            // The panel's reload read as the centre tab strip's F5; it is the Inspector's.
            ("inspector.reload_tab", "Reload inspector tab"),
        ] {
            let command = commands
                .iter()
                .find(|command| command.id == id)
                .expect("command exists");
            assert_eq!(command.label, label);
        }
    }

    /// Every action the native menu offers has exactly one palette row under the
    /// same title, and every palette row that dispatches an action is a menu entry.
    /// One action, one name, one place to look for it.
    ///
    #[test]
    fn menu_and_palette_agree_on_every_action() {
        {
            let commands = demo_commands(true);
            for (title, action) in NATIVE_MENU_TITLES {
                let title = (*title).to_owned();
                let matching: Vec<&Command> = commands
                    .iter()
                    .filter(|command| {
                        command.action_name().as_deref() == Some(*action)
                            && command.label.as_ref() == title
                    })
                    .collect();
                assert_eq!(
                    matching.len(),
                    1,
                    "{title} ({action}) needs exactly one palette row with the same title"
                );
            }
            for command in &commands {
                let Some(action) = command.action_name() else {
                    continue;
                };
                assert!(
                    NATIVE_MENU_TITLES
                        .iter()
                        .any(|(_, menu_action)| *menu_action == action)
                        || MENU_ONLY_ACTIONS.contains(&action.as_str())
                        || PALETTE_ONLY_ACTIONS.contains(&action.as_str()),
                    "{} ({action}) is a palette row with no menu entry",
                    command.id
                );
            }
        }
    }

    /// The panel's own commands: one label per action, one row per action, under the target they
    /// act on rather than under the surface they happen to be drawn in, and the Settings keyboard
    /// list reads the same rows.
    #[test]
    fn inspector_commands_dispatch_inspector_actions_and_are_grouped_by_target() {
        let commands = demo_commands(true);
        for (id, label) in [
            ("inspector.reload_tab", "Reload inspector tab"),
            ("inspector.retry_metrics", "Retry metrics"),
            ("inspector.metrics_1m", "Metrics: last minute"),
            ("inspector.metrics_15m", "Metrics: last 15 minutes"),
            ("inspector.metrics_1h", "Metrics: last hour"),
            ("inspector.confirm_apply", "Apply to cluster"),
            ("inspector.revert_yaml", "Revert YAML"),
            ("inspector.copy_yaml", "Copy YAML"),
            ("inspector.toggle_value", "Expand or collapse value"),
            ("inspector.copy_value", "Copy value"),
            ("inspector.next_problem", "Next YAML problem"),
        ] {
            let command = commands
                .iter()
                .find(|command| command.id == id)
                .unwrap_or_else(|| panic!("{id} is in the command palette"));
            assert_eq!(command.label, label);
            assert_eq!(command.group, "Resources");
            let CommandRun::Action(make_action) = command.run else {
                panic!("{id} dispatches the Inspector action");
            };
            assert!(
                make_action().name().starts_with("k8s_inspector::"),
                "{id} must dispatch the action the panel answers to"
            );
            assert!(
                PALETTE_ONLY_ACTIONS.contains(&make_action().name()),
                "{id} has no menu entry, so the parity list must name the Inspector as its owner"
            );
        }
    }

    /// Every Inspector action the keymap binds is a palette row, so an action only the panel
    /// can run is still discoverable by name.
    #[test]
    fn every_bound_inspector_action_is_a_palette_row() {
        let commands = demo_commands(true);
        let mut bound: BTreeSet<String> = BTreeSet::new();
        for (_, _, action) in keymap_bindings() {
            if action.starts_with("k8s_inspector::") {
                bound.insert(action);
            }
        }
        assert!(!bound.is_empty(), "the keymap binds the Inspector actions");
        for name in &bound {
            // The one exception is Escape inside the review strip, which the next test covers.
            if name == "k8s_inspector::CancelApplyReview" {
                continue;
            }
            assert!(
                commands
                    .iter()
                    .any(|command| command.action_name().as_deref() == Some(name.as_str())),
                "{name} is bound and has no palette row"
            );
        }
    }

    /// Escape belongs to the review strip, and the strip's own button reads "Keep editing", so a
    /// row named after cancelling a review would be ambiguous in a list with no review open. The
    /// key is the reachability, so the panel keeps it and the palette stays out.
    #[test]
    fn the_review_cancel_stays_in_the_panel() {
        let commands = demo_commands(true);
        assert!(
            !commands.iter().any(|command| {
                command.action_name().as_deref() == Some("k8s_inspector::CancelApplyReview")
            }),
            "the review's Escape must not become a global palette row"
        );
        assert!(
            keymap_bindings().iter().any(|(context, _, action)| {
                context.contains("Inspector") && action == "k8s_inspector::CancelApplyReview"
            }),
            "the review must keep Escape, which is how the command stays reachable"
        );
    }

    /// A palette row looks its chord up in the `Shell` context, so an Inspector binding would
    /// draw a keycap the palette cannot fire. Every Inspector binding stays in the Inspector
    /// context, where the panel is the only surface that can act on it, so no Inspector row
    /// draws one.
    #[test]
    fn inspector_rows_advertise_no_shell_chord() {
        for (context, keystrokes, action) in keymap_bindings() {
            if !action.starts_with("k8s_inspector::") {
                continue;
            }
            assert!(
                context.contains("Inspector"),
                "{keystrokes} binds {action} outside the Inspector, so a palette row would \
                 advertise a chord the palette cannot fire"
            );
        }
    }

    /// The three high-risk release operations are searchable, and the destructive
    /// one is reachable without owning a key.
    #[test]
    fn helm_release_operations_are_in_the_palette() {
        let commands = demo_commands(true);
        for (id, label, icon) in [
            (
                "helm.upgrade",
                "Upgrade selected release…",
                IconName::ArrowUp,
            ),
            (
                "helm.rollback",
                "Roll back selected release…",
                IconName::Clock,
            ),
            (
                "helm.uninstall",
                "Uninstall selected release…",
                IconName::FileX,
            ),
        ] {
            let command = commands
                .iter()
                .find(|command| command.id == id)
                .unwrap_or_else(|| panic!("{id} is in the command palette"));
            assert_eq!(command.label, label);
            assert_eq!(command.group, "Helm");
            assert_eq!(command.icon, icon);
            assert!(
                matches!(command.run, CommandRun::ShellFn(_)),
                "{id} runs a Shell command, so it can check the release before it runs"
            );
            let labels: Vec<&str> = filter_commands(&commands, label)
                .iter()
                .map(|command| command.label.as_ref())
                .collect();
            assert_eq!(labels, [label], "{id} must be searchable by its title");
        }
    }

    /// A destructive action must not own a bare key: no keymap binding and no
    /// advertised shortcut, only the palette, the panel, and the menu.
    #[test]
    fn no_destructive_command_owns_a_key() {
        for command in demo_commands(true)
            .iter()
            .filter(|command| command.is_destructive())
        {
            assert!(
                command.binding.is_none(),
                "{} must not advertise a key",
                command.id
            );
            assert_eq!(
                command.action_name(),
                None,
                "{} must not dispatch a key-bound action",
                command.id
            );
        }
        assert!(
            demo_commands(true)
                .iter()
                .any(|command| command.is_destructive()),
            "the palette must offer a destructive command at all"
        );

        // No built-in key runs a Helm operation, and the Helm table's own key
        // context binds nothing, so no chord can reach a release operation.
        for (context, keystrokes, action) in keymap_bindings() {
            assert!(
                !action.contains("Helm"),
                "{keystrokes} must not run a Helm action ({action})"
            );
            if context.contains("Helm releases") {
                panic!("the Helm context must bind no bare key, found {keystrokes} -> {action}");
            }
        }
    }

    #[test]
    fn helm_commands_explain_a_missing_client() {
        for (id, badge, reason) in [
            ("helm.upgrade", "Not installed", HELM_NOT_INSTALLED),
            ("helm.rollback", "Timed out", HELM_PROBE_TIMED_OUT),
            ("helm.uninstall", "Error", HELM_PROBE_FAILED),
        ] {
            let capability = match reason {
                HELM_NOT_INSTALLED => HelmCapability::NotInstalled,
                HELM_PROBE_TIMED_OUT => HelmCapability::Timeout,
                _ => HelmCapability::Error,
            };
            let command = demo_commands_with_capabilities(capability, true)
                .into_iter()
                .find(|command| command.id == id)
                .expect("helm command exists");
            assert!(
                matches!(
                    command.run,
                    CommandRun::Unavailable {
                        badge: actual_badge,
                        reason: actual_reason,
                    } if actual_badge == badge && actual_reason == reason
                ),
                "{id} must explain that the client is unavailable"
            );
        }
    }

    #[test]
    fn update_commands_are_searchable_and_dispatch_actions() {
        let commands = demo_commands(true);
        let check = commands
            .iter()
            .find(|command| command.id == CHECK_FOR_UPDATES_COMMAND_ID)
            .expect("Check for Updates command exists");
        assert_eq!(check.label, "Check for updates");
        let CommandRun::Action(make_check) = check.run else {
            panic!("Check for Updates must dispatch the shell action");
        };
        assert_eq!(make_check().name(), "k8s_app::CheckForUpdates");

        let restart = commands
            .iter()
            .find(|command| command.id == RESTART_TO_UPDATE_COMMAND_ID)
            .expect("Restart to Update command exists");
        assert_eq!(restart.label, "Restart to update");
        let CommandRun::Action(make_restart) = restart.run else {
            panic!("Restart to Update must dispatch the shell action");
        };
        assert_eq!(make_restart().name(), "k8s_app::RestartToUpdate");
    }
    #[test]
    fn open_forwards_command_dispatches_shell_action() {
        let command = demo_commands(true)
            .into_iter()
            .find(|command| command.id == "view.forwards")
            .expect("view.forwards command exists");
        assert_eq!(command.label, "Open port forwards");
        let CommandRun::Action(make_action) = command.run else {
            panic!("Open port forwards must dispatch the shell action");
        };
        assert_eq!(make_action().name(), "k8s_shell::OpenForwards");
    }

    #[test]
    fn refresh_view_uses_the_shell_action() {
        let command = demo_commands(true)
            .into_iter()
            .find(|command| command.id == "view.refresh")
            .expect("view.refresh command exists");
        let CommandRun::Action(make_action) = command.run else {
            panic!("Refresh view must dispatch the shell action");
        };
        assert_eq!(make_action().name(), "k8s_shell::RefreshView");
    }

    #[test]
    fn reload_kubeconfigs_command_dispatches_shell_action() {
        let command = demo_commands(true)
            .into_iter()
            .find(|command| command.id == "cluster.reload_kubeconfigs")
            .expect("reload kubeconfigs command exists");
        assert_eq!(command.label, "Reload kubeconfigs");
        let CommandRun::Action(make_action) = command.run else {
            panic!("Reload kubeconfigs must dispatch the shell action");
        };
        assert_eq!(make_action().name(), "k8s_shell::ReloadKubeconfigs");
    }
}
