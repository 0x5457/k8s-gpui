//! Static command definitions and fuzzy filtering for the command palette.
//!
//! Each command uses [`CommandRun`] to define how it runs. Action commands dispatch after the
//! palette closes. Unavailable commands remain searchable and show why they cannot run.
//!
//! Icons follow one rule: the icon names the object a command acts on, and no two commands
//! share one. A command that changes meaning must change icon, or two rows read as the same
//! action. Glyphs come from the shared `gpui_kit::assets::IconName` set.

use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::{Action, Context, Entity, SharedString, Window};

use super::{
    ApplyYaml, CheckForUpdates, CloseAllTabs, CloseOtherTabs, CloseTab, CopySelectedPodName, Cut,
    DescribeSelection, ExecSelection, FocusYaml, MoveTabLeft, MoveTabRight, NextTab,
    OpenContextSwitcher, OpenEvents, OpenForwards, OpenLogs, OpenNamespaceSwitcher, OpenOverview,
    OpenResourceKindSwitcher, OpenServiceAccount, PaletteScope, Paste, PauseUpdates,
    PortForwardSelection, PreviousTab, Redo, RefreshView, ReloadKeymap, ReloadKubeconfigs,
    RestartSelection, RestartToUpdate, ResumeUpdates, ScaleSelection, SearchResources, SelectAll,
    Shell, TabView, ToggleCommandPalette, ToggleDock, ToggleHotbar, ToggleLeftPanel,
    ToggleNotifications, TogglePinTab, ToggleRightPanel, ToggleTheme, Undo, UseDarkTheme,
    UseKeymapPreset, UseLightTheme, UseSystemTheme,
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
    "This build does not support application updates. Use a build with updater support.";
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
        self.id.starts_with("helm.uninstall") || self.id.starts_with("hotbar.remove")
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
    ("Toggle Light/Dark Theme", "k8s_shell::ToggleTheme"),
    ("Use Light Theme", "k8s_shell::UseLightTheme"),
    ("Use Dark Theme", "k8s_shell::UseDarkTheme"),
    // File menu.
    ("Refresh View", "k8s_shell::RefreshView"),
    ("Reload Kubeconfigs", "k8s_shell::ReloadKubeconfigs"),
    ("Reload Keymap", "k8s_shell::ReloadKeymap"),
    ("Close Tab", "k8s_shell::CloseTab"),
    ("Close Other Tabs", "k8s_shell::CloseOtherTabs"),
    ("Close All Tabs", "k8s_shell::CloseAllTabs"),
    // Edit menu. The standard editing commands are menu entries, so they are
    // palette rows too.
    ("Undo", "k8s_shell::Undo"),
    ("Redo", "k8s_shell::Redo"),
    ("Cut", "k8s_shell::Cut"),
    ("Copy", "k8s_shell::Copy"),
    ("Paste", "k8s_shell::Paste"),
    ("Select All", "k8s_shell::SelectAll"),
    ("Focus YAML", "k8s_shell::FocusYaml"),
    ("Apply YAML Changes", "k8s_shell::ApplyYaml"),
    ("Describe Selected Resource", "k8s_shell::DescribeSelection"),
    (
        "Open Service Account for Selected Pod",
        "k8s_shell::OpenServiceAccount",
    ),
    ("Show Logs for Selected Pod", "k8s_shell::OpenLogs"),
    ("Show Events for Selected Pod", "k8s_shell::OpenEvents"),
    ("Exec in Selected Pod", "k8s_shell::ExecSelection"),
    (
        "Start Port Forward for Selected Pod",
        "k8s_shell::PortForwardSelection",
    ),
    ("Restart Selected Resource", "k8s_shell::RestartSelection"),
    ("Scale Selected Resource", "k8s_shell::ScaleSelection"),
    // The menu title is corrected to name every resource view, not only Pods.
    (
        "Copy Selected Resource Name",
        "k8s_shell::CopySelectedPodName",
    ),
    // View menu.
    ("Command Palette", "k8s_shell::ToggleCommandPalette"),
    ("Switch Context", "k8s_shell::OpenContextSwitcher"),
    ("Switch Namespace", "k8s_shell::OpenNamespaceSwitcher"),
    (
        "Choose Resource Kind",
        "k8s_shell::OpenResourceKindSwitcher",
    ),
    ("Open Cluster Overview", "k8s_shell::OpenOverview"),
    ("Open Port Forwards", "k8s_shell::OpenForwards"),
    ("Search Cluster Resources", "k8s_shell::SearchResources"),
    ("Toggle Sidebar", "k8s_shell::ToggleLeftPanel"),
    ("Toggle Inspector", "k8s_shell::ToggleRightPanel"),
    ("Toggle Dock", "k8s_shell::ToggleDock"),
    ("Toggle Notifications", "k8s_shell::ToggleNotifications"),
    // Window menu.
    ("Previous Tab", "k8s_shell::PreviousTab"),
    ("Next Tab", "k8s_shell::NextTab"),
    ("Move Tab Left", "k8s_shell::MoveTabLeft"),
    ("Move Tab Right", "k8s_shell::MoveTabRight"),
    ("Toggle Pin Tab", "k8s_shell::TogglePinTab"),
];

/// Menu entries with no palette row, and why.
///
/// * The keymap preset is one action with one entry per preset, so the menu has
///   two titles for it and the palette keeps the matching two.
/// * `Hide Others`, `Show All` and `Enter or Exit Full Screen` are dispatched by
///   the macOS-only menu bootstrap. On Linux and Windows a palette row would be a
///   no-op, so they stay menu-only.
pub const MENU_ONLY_ACTIONS: &[&str] = &[
    "k8s_shell::UseKeymapPreset",
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
    // The Hotbar rail owns its own toggle.
    "k8s_hotbar::ToggleHotbar",
    // The resource table owns refresh, its row menu, and its column sort: each acts on the
    // table the reader is looking at, and the table already offers all three as toolbar
    // controls with their chords beside them.
    "k8s_ops::Refresh",
    "k8s_table::OpenRowActions",
    "k8s_table::SortSelectedColumn",
    // prints a chord beside "Only problems" in the column-header popover.
    // An action that prints a key is a command, so it is a palette row too — otherwise the
    // key is discoverable only by finding the popover.
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

fn close_other_tabs_action() -> Box<dyn Action> {
    Box::new(CloseOtherTabs)
}

fn close_all_tabs_action() -> Box<dyn Action> {
    Box::new(CloseAllTabs)
}

fn toggle_pin_tab_action() -> Box<dyn Action> {
    Box::new(TogglePinTab)
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

fn refresh_action() -> Box<dyn Action> {
    Box::new(crate::table_view::Refresh)
}

fn open_row_actions_action() -> Box<dyn Action> {
    Box::new(crate::table_view::OpenRowActions)
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

fn toggle_command_palette_action() -> Box<dyn Action> {
    Box::new(ToggleCommandPalette)
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

fn toggle_hotbar_action() -> Box<dyn Action> {
    Box::new(ToggleHotbar)
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

/// Build the Hotbar commands.
fn hotbar_commands() -> Vec<Command> {
    use CommandRun as Run;
    [
        (
            "hotbar.add_cluster",
            "Add Current Context to Hotbar",
            IconName::Plus,
            Run::Shell(Shell::command_hotbar_add_cluster),
        ),
        (
            "hotbar.create_bank",
            "Create Hotbar Bank",
            IconName::SquarePlus,
            Run::Shell(Shell::command_hotbar_create_bank),
        ),
        (
            "hotbar.rename_bank",
            "Rename Hotbar Bank",
            IconName::Pencil,
            Run::Shell(Shell::command_hotbar_rename_bank),
        ),
        (
            "hotbar.remove_bank",
            "Remove Hotbar Bank",
            IconName::Trash,
            Run::Shell(Shell::command_hotbar_remove_bank),
        ),
        (
            "hotbar.toggle",
            "Toggle Hotbar",
            IconName::PanelLeftClose,
            Run::Action(toggle_hotbar_action as fn() -> Box<dyn Action>),
        ),
    ]
    .into_iter()
    .map(|(id, label, icon, run)| {
        with_action_binding(Command {
            id: SharedString::from(id),
            label: SharedString::from(label),
            group: SharedString::from("Hotbar"),
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

pub fn demo_commands_with_capabilities(
    helm: HelmCapability,
    updater_available: bool,
) -> Vec<Command> {
    use CommandRun as Run;
    let helm_run = if helm == HelmCapability::Available {
        Run::Shell(Shell::command_open_helm)
    } else {
        Run::Unavailable {
            badge: helm.label(),
            reason: helm.reason().unwrap_or(HELM_COMMAND_FAILED),
        }
    };
    let mut helm_commands = vec![(
        "helm.open",
        "Open Helm Releases",
        "Helm",
        IconName::Archive,
        helm_run,
    )];
    for (id, action, icon) in [
        (
            "helm.upgrade",
            HelmReleaseAction::Upgrade,
            IconName::ArrowUp,
        ),
        (
            "helm.rollback",
            HelmReleaseAction::Rollback,
            IconName::Clock,
        ),
        (
            "helm.uninstall",
            HelmReleaseAction::Uninstall,
            IconName::FileX,
        ),
    ] {
        // A destructive action never owns a key: it is reachable from the palette,
        // from the detail action bar, and from the release menu, and every one of
        // them opens the confirmation dialog first.
        helm_commands.push((
            id,
            action.command_label(),
            "Helm",
            icon,
            if helm == HelmCapability::Available {
                Run::ShellFn(helm_release_action_handler(action))
            } else {
                Run::Unavailable {
                    badge: helm.label(),
                    reason: helm.reason().unwrap_or(HELM_COMMAND_FAILED),
                }
            },
        ));
    }
    let mut commands: Vec<Command> = [
        (
            "navigation.context",
            "Switch Context",
            "Navigation",
            IconName::Server,
            Run::Action(open_context_switcher_action as fn() -> Box<dyn Action>),
        ),
        (
            "navigation.namespace",
            "Switch Namespace",
            "Navigation",
            IconName::Folder,
            Run::Action(open_namespace_switcher_action as fn() -> Box<dyn Action>),
        ),
        (
            "navigation.kind",
            "Choose Resource Kind",
            "Navigation",
            IconName::ListTree,
            Run::Action(open_resource_kind_switcher_action as fn() -> Box<dyn Action>),
        ),
        (
            "tab.close",
            "Close Tab",
            "Tabs",
            IconName::Close,
            Run::Action(close_tab_action as fn() -> Box<dyn Action>),
        ),
        (
            "tab.close_others",
            "Close Other Tabs",
            "Tabs",
            IconName::ListCollapse,
            Run::Action(close_other_tabs_action as fn() -> Box<dyn Action>),
        ),
        (
            "tab.close_all",
            "Close All Tabs",
            "Tabs",
            IconName::ListX,
            Run::Action(close_all_tabs_action as fn() -> Box<dyn Action>),
        ),
        (
            "tab.toggle_pin",
            // The native menu title, so the palette row and the menu entry name one action once.
            "Toggle Pin Tab",
            "Tabs",
            IconName::Pin,
            Run::Action(toggle_pin_tab_action as fn() -> Box<dyn Action>),
        ),
        (
            "tab.move_left",
            "Move Tab Left",
            "Tabs",
            IconName::ArrowLeft,
            Run::Action(move_tab_left_action as fn() -> Box<dyn Action>),
        ),
        (
            "tab.move_right",
            "Move Tab Right",
            "Tabs",
            IconName::ArrowRight,
            Run::Action(move_tab_right_action as fn() -> Box<dyn Action>),
        ),
        (
            "tab.previous",
            "Previous Tab",
            "Tabs",
            IconName::ChevronLeft,
            Run::Action(previous_tab_action as fn() -> Box<dyn Action>),
        ),
        (
            "tab.next",
            "Next Tab",
            "Tabs",
            IconName::ChevronRight,
            Run::Action(next_tab_action as fn() -> Box<dyn Action>),
        ),
        (
            "view.refresh",
            "Refresh View",
            "View",
            IconName::RefreshCw,
            Run::Action(refresh_view_action as fn() -> Box<dyn Action>),
        ),
        (
            "view.overview",
            "Open Cluster Overview",
            "View",
            IconName::Monitor,
            Run::Action(open_overview_action as fn() -> Box<dyn Action>),
        ),
        (
            "view.forwards",
            "Open Port Forwards",
            "View",
            IconName::Table,
            Run::Action(open_forwards_action as fn() -> Box<dyn Action>),
        ),
        (
            "view.search_resources",
            "Search Cluster Resources",
            "View",
            IconName::Search,
            Run::Action(search_resources_action as fn() -> Box<dyn Action>),
        ),
        (
            "view.command_palette",
            "Command Palette",
            "View",
            IconName::Command,
            Run::Action(toggle_command_palette_action as fn() -> Box<dyn Action>),
        ),
        (
            "view.notifications",
            "Toggle Notifications",
            "View",
            IconName::BellRing,
            Run::Action(toggle_notifications_action as fn() -> Box<dyn Action>),
        ),
        (
            "pod.describe",
            "Describe Selected Resource",
            "View",
            IconName::Info,
            Run::Action(describe_action),
        ),
        (
            "yaml.focus",
            "Focus YAML",
            "View",
            IconName::FileCode,
            Run::Action(focus_yaml_action),
        ),
        (
            "yaml.apply",
            "Apply YAML Changes",
            "View",
            IconName::Check,
            Run::Action(apply_yaml_action as fn() -> Box<dyn Action>),
        ),
        (
            "pod.logs",
            "Show Logs for Selected Pod",
            "View",
            IconName::BookOpen,
            Run::Action(open_logs_action as fn() -> Box<dyn Action>),
        ),
        (
            "pod.events",
            "Show Events for Selected Pod",
            "View",
            IconName::Bell,
            Run::Action(open_events_action as fn() -> Box<dyn Action>),
        ),
        (
            "pod.pause",
            "Pause Updates",
            "View",
            IconName::Pause,
            Run::Action(pause_updates_action),
        ),
        (
            "pod.resume",
            "Resume Updates",
            "View",
            IconName::Play,
            Run::Action(resume_updates_action),
        ),
        (
            "pod.copy_name",
            "Copy Selected Resource Name",
            "View",
            IconName::Copy,
            Run::Action(copy_selected_name_action),
        ),
        // These four answer to a chord inside the table, but a chord is not a command list.
        // Someone who learns the app from the palette, or who wants to see what a key does,
        // needs them here too; requires every product command to be reachable.
        (
            "table.refresh",
            "Refresh Resources",
            "View",
            IconName::CircleArrowRight,
            Run::Action(refresh_action),
        ),
        (
            "table.row_actions",
            "Open Row Actions",
            "View",
            IconName::Ellipsis,
            Run::Action(open_row_actions_action),
        ),
        (
            "table.sort_column",
            "Sort Selected Column",
            "View",
            IconName::ListTodo,
            Run::Action(sort_selected_column_action),
        ),
        (
            "table.only_problems",
            "Show Only Problems",
            "View",
            // The funnel, not the warning triangle. This command narrows the table, and
            // The design's D5 gives the amber channel to rows that are actually in
            // trouble — a warning glyph on a control that is merely on or off spends
            // the reader's attention before they have read the label. The triangle
            // stays on `inspector.next_problem`, which does point at a real defect.
            IconName::ListFilter,
            Run::Action(toggle_problems_only_action),
        ),
        (
            "edit.undo",
            "Undo",
            "Edit",
            IconName::Undo,
            Run::Action(undo_action as fn() -> Box<dyn Action>),
        ),
        (
            "edit.redo",
            "Redo",
            "Edit",
            IconName::Redo,
            Run::Action(redo_action as fn() -> Box<dyn Action>),
        ),
        (
            "edit.cut",
            "Cut",
            "Edit",
            IconName::Scissors,
            Run::Action(cut_action as fn() -> Box<dyn Action>),
        ),
        (
            "edit.copy",
            "Copy",
            "Edit",
            IconName::ClipboardCopy,
            Run::Action(copy_action as fn() -> Box<dyn Action>),
        ),
        (
            "edit.paste",
            "Paste",
            "Edit",
            IconName::NotepadText,
            Run::Action(paste_action as fn() -> Box<dyn Action>),
        ),
        (
            "edit.select_all",
            "Select All",
            "Edit",
            IconName::SquareDashedMousePointer,
            Run::Action(select_all_action as fn() -> Box<dyn Action>),
        ),
        (
            "pod.exec",
            "Exec in Selected Pod",
            "Actions",
            IconName::Terminal,
            Run::Action(exec_selection_action as fn() -> Box<dyn Action>),
        ),
        (
            "pod.forward_port",
            "Start Port Forward for Selected Pod",
            "Actions",
            IconName::Link,
            Run::Action(port_forward_selection_action as fn() -> Box<dyn Action>),
        ),
        (
            "pod.service_account",
            "Open Service Account for Selected Pod",
            "Actions",
            IconName::UserCheck,
            Run::Action(open_service_account_action as fn() -> Box<dyn Action>),
        ),
        (
            "resource.restart",
            "Restart Selected Resource",
            "Actions",
            IconName::RotateCw,
            Run::Action(restart_selection_action as fn() -> Box<dyn Action>),
        ),
        (
            "resource.scale",
            "Scale Selected Resource",
            "Actions",
            IconName::Replace,
            Run::Action(scale_selection_action as fn() -> Box<dyn Action>),
        ),
        // The Inspector group. Every row here dispatches an action the panel answers to, and every
        // one of them is also a control inside the panel with its chord drawn beside it, so the
        // palette is the searchable way in and the panel keeps the direct route. The keymap binds
        // them in the `Inspector` context, which the palette is not, so a row draws no keycap
        // instead of advertising a chord the palette cannot fire.
        //
        // A label here is also the label the Settings keyboard list shows, because that list reads
        // these rows, so a rename touches one name and both surfaces keep it.
        //
        // Two neighbours of this group are deliberately absent. `CancelApplyReview` is Escape
        // inside the review strip, where the strip's own button says "Keep editing": a global row
        // named after cancelling a review would be ambiguous in a list that has no review open.
        // The port forward panel's Copy URL and Open in Browser are not actions at all: that panel
        // answers Control+c and Control+o on its selected row itself, so it has no action to
        // dispatch and no row to advertise.
        (
            "inspector.reload_tab",
            "Reload Active Tab",
            "Inspector",
            IconName::PanelTop,
            Run::Action(reload_active_tab_action as fn() -> Box<dyn Action>),
        ),
        (
            "inspector.retry_metrics",
            "Retry Metrics",
            "Inspector",
            IconName::Timer,
            Run::Action(retry_metrics_action as fn() -> Box<dyn Action>),
        ),
        (
            "inspector.metrics_1m",
            "Metrics: Last Minute",
            "Inspector",
            IconName::SignalLow,
            Run::Action(metrics_range_1m_action as fn() -> Box<dyn Action>),
        ),
        (
            "inspector.metrics_6h",
            "Metrics: Last 6 Hours",
            "Inspector",
            IconName::Signal,
            Run::Action(metrics_range_6h_action as fn() -> Box<dyn Action>),
        ),
        (
            "inspector.metrics_24h",
            "Metrics: Last 24 Hours",
            "Inspector",
            IconName::CalendarClock,
            Run::Action(metrics_range_24h_action as fn() -> Box<dyn Action>),
        ),
        (
            "inspector.metrics_7d",
            "Metrics: Last 7 Days",
            "Inspector",
            IconName::CalendarDays,
            Run::Action(metrics_range_7d_action as fn() -> Box<dyn Action>),
        ),
        (
            "inspector.metrics_15m",
            "Metrics: Last 15 Minutes",
            "Inspector",
            IconName::SignalMedium,
            Run::Action(metrics_range_15m_action as fn() -> Box<dyn Action>),
        ),
        (
            "inspector.metrics_1h",
            "Metrics: Last Hour",
            "Inspector",
            IconName::SignalHigh,
            Run::Action(metrics_range_1h_action as fn() -> Box<dyn Action>),
        ),
        (
            "inspector.confirm_apply",
            "Confirm and Apply Changes",
            "Inspector",
            IconName::CheckCheck,
            Run::Action(confirm_apply_action as fn() -> Box<dyn Action>),
        ),
        (
            "inspector.revert_yaml",
            "Revert YAML",
            "Inspector",
            IconName::ReplaceAll,
            Run::Action(revert_yaml_action as fn() -> Box<dyn Action>),
        ),
        (
            "inspector.copy_yaml",
            "Copy YAML",
            "Inspector",
            IconName::FileText,
            Run::Action(copy_yaml_action as fn() -> Box<dyn Action>),
        ),
        (
            "inspector.toggle_value",
            "Expand or Collapse Value",
            "Inspector",
            IconName::TextWrap,
            Run::Action(toggle_value_expansion_action as fn() -> Box<dyn Action>),
        ),
        (
            "inspector.copy_value",
            "Copy Value",
            "Inspector",
            IconName::Quote,
            Run::Action(copy_value_action as fn() -> Box<dyn Action>),
        ),
        (
            "inspector.next_problem",
            "Next YAML Problem",
            "Inspector",
            IconName::TriangleAlert,
            Run::Action(next_problem_action as fn() -> Box<dyn Action>),
        ),
        (
            "keymap.create_or_show",
            "Create or Open Keymap File",
            "Keymap",
            IconName::Braces,
            Run::Shell(Shell::command_create_or_show_keymap),
        ),
        (
            "keymap.reload",
            "Reload Keymap",
            "Keymap",
            IconName::Keyboard,
            Run::Action(reload_keymap_action as fn() -> Box<dyn Action>),
        ),
        (
            "keymap.preset.lens",
            "Use Lens Keymap",
            "Keymap",
            IconName::Star,
            Run::Action(use_lens_keymap_action as fn() -> Box<dyn Action>),
        ),
        (
            "keymap.preset.vscode",
            "Use VS Code Keymap",
            "Keymap",
            IconName::Code,
            Run::Action(use_vscode_keymap_action as fn() -> Box<dyn Action>),
        ),
        (
            "sidebar.toggle",
            "Toggle Sidebar",
            "Panels",
            IconName::PanelLeftOpen,
            Run::Action(toggle_left_panel_action as fn() -> Box<dyn Action>),
        ),
        (
            "inspector.toggle",
            "Toggle Inspector",
            "Panels",
            IconName::PanelRightOpen,
            Run::Action(toggle_right_panel_action),
        ),
        (
            "dock.toggle",
            "Toggle Dock",
            "Panels",
            IconName::ChevronUp,
            Run::Action(toggle_dock_action),
        ),
        (
            "theme.toggle",
            "Toggle Light/Dark Theme",
            "Application",
            IconName::ArrowRightLeft,
            Run::Action(toggle_theme_action),
        ),
        (
            "theme.light",
            "Use Light Theme",
            "Application",
            IconName::Eye,
            Run::Action(light_theme_action),
        ),
        (
            "theme.dark",
            "Use Dark Theme",
            "Application",
            IconName::EyeOff,
            Run::Action(dark_theme_action),
        ),
        (
            "theme.system",
            "Use System Theme",
            "Application",
            IconName::AppWindow,
            Run::Action(use_system_theme_action as fn() -> Box<dyn Action>),
        ),
        (
            "cluster.reload_kubeconfigs",
            "Reload Kubeconfigs",
            "Application",
            IconName::FolderSync,
            Run::Action(reload_kubeconfigs_action as fn() -> Box<dyn Action>),
        ),
        (
            CHECK_FOR_UPDATES_COMMAND_ID,
            "Check for Updates",
            "Application",
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
            "Restart to Update",
            "Application",
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
            "Application",
            IconName::Settings,
            Run::Action(open_settings_action as fn() -> Box<dyn Action>),
        ),
    ]
    .into_iter()
    .map(|(id, label, group, icon, run)| {
        with_action_binding(Command {
            id: SharedString::from(id),
            label: SharedString::from(label),
            group: SharedString::from(group),
            icon,
            run,
            binding: None,
        })
    })
    .collect::<Vec<_>>();
    // The Helm group is built apart from the static list: its run depends on the
    // detected client, and it must stay contiguous next to `helm.open`.
    let helm_at = commands
        .iter()
        .position(|command| command.group.as_ref() == "Actions")
        .unwrap_or(commands.len());
    let helm_block = helm_commands
        .into_iter()
        .map(|(id, label, group, icon, run)| Command {
            id: SharedString::from(id),
            label: SharedString::from(label),
            group: SharedString::from(group),
            icon,
            run,
            binding: None,
        })
        .collect::<Vec<_>>();
    for (offset, command) in helm_block.into_iter().enumerate() {
        commands.insert(helm_at + offset, command);
    }
    commands.extend(hotbar_commands());
    commands
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

    /// Close Tab and Close All Tabs used the same `x`; the whole-Tabs command needs its own glyph.
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
        let one = icon_for("tab.close");
        assert_ne!(one, icon_for("tab.close_all"));
        assert_ne!(icon_for("tab.close_others"), one);
        // Reordering tabs and forwarding a port are different actions.
        assert_ne!(icon_for("pod.forward_port"), icon_for("tab.move_right"));
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
        // The query is uppercase, and the Inspector group and its rows match it too, so the two
        // rows that prove case-insensitive matching are named instead of listed in rank order.
        let labels: Vec<&str> = filter_commands(&commands, "INSPECT")
            .iter()
            .map(|command| command.label.as_ref())
            .collect();
        for label in [
            "Toggle Inspector",
            "Open Service Account for Selected Pod",
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
        assert_eq!(labels, vec!["Toggle Dock"]);
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
        assert!(labels.contains(&"Use Light Theme"));
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
        assert!(labels.contains(&"Toggle Light/Dark Theme"));
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
            ("view.refresh", "Refresh View", "k8s_shell::RefreshView"),
            (
                "view.forwards",
                "Open Port Forwards",
                "k8s_shell::OpenForwards",
            ),
            (
                "cluster.reload_kubeconfigs",
                "Reload Kubeconfigs",
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
        let keymap = commands
            .iter()
            .find(|command| command.id == "keymap.create_or_show")
            .expect("keymap command exists");
        assert_eq!(keymap.label, "Create or Open Keymap File");
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
            ("tab.close", "Close Tab", "k8s_shell::CloseTab"),
            (
                "tab.close_others",
                "Close Other Tabs",
                "k8s_shell::CloseOtherTabs",
            ),
            ("tab.close_all", "Close All Tabs", "k8s_shell::CloseAllTabs"),
            (
                "tab.toggle_pin",
                "Toggle Pin Tab",
                "k8s_shell::TogglePinTab",
            ),
            ("tab.move_left", "Move Tab Left", "k8s_shell::MoveTabLeft"),
            (
                "tab.move_right",
                "Move Tab Right",
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
        assert_eq!(command.label, "Start Port Forward for Selected Pod");
    }

    /// Labels must name what the command does in every resource view, not only Pods.
    #[test]
    fn labels_match_the_native_menu_and_the_resource_view() {
        let commands = demo_commands(true);
        for (id, label) in [
            ("navigation.kind", "Choose Resource Kind"),
            ("pod.copy_name", "Copy Selected Resource Name"),
            (
                "pod.service_account",
                "Open Service Account for Selected Pod",
            ),
            ("hotbar.add_cluster", "Add Current Context to Hotbar"),
            ("settings.open", "Settings\u{2026}"),
            ("theme.toggle", "Toggle Light/Dark Theme"),
            ("keymap.preset.lens", "Use Lens Keymap"),
            ("tab.next", "Next Tab"),
            ("tab.previous", "Previous Tab"),
            ("view.notifications", "Toggle Notifications"),
            ("theme.system", "Use System Theme"),
            ("view.command_palette", "Command Palette"),
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
            for title in ["Use Lens Keymap", "Use VS Code Keymap"] {
                assert!(
                    commands.iter().any(|command| {
                        command.label.as_ref() == title
                            && command.action_name().as_deref()
                                == Some("k8s_shell::UseKeymapPreset")
                    }),
                    "{title} needs a palette row with the same title"
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

    /// The Inspector group: one label per action, one row per action, and the Settings
    /// keyboard list reads the same rows.
    #[test]
    fn inspector_commands_are_grouped_and_dispatch_inspector_actions() {
        let commands = demo_commands(true);
        for (id, label) in [
            ("inspector.reload_tab", "Reload Active Tab"),
            ("inspector.retry_metrics", "Retry Metrics"),
            ("inspector.metrics_1m", "Metrics: Last Minute"),
            ("inspector.metrics_15m", "Metrics: Last 15 Minutes"),
            ("inspector.metrics_1h", "Metrics: Last Hour"),
            ("inspector.confirm_apply", "Confirm and Apply Changes"),
            ("inspector.revert_yaml", "Revert YAML"),
            ("inspector.copy_yaml", "Copy YAML"),
            ("inspector.toggle_value", "Expand or Collapse Value"),
            ("inspector.copy_value", "Copy Value"),
            ("inspector.next_problem", "Next YAML Problem"),
        ] {
            let command = commands
                .iter()
                .find(|command| command.id == id)
                .unwrap_or_else(|| panic!("{id} is in the command palette"));
            assert_eq!(command.label, label);
            assert_eq!(command.group, "Inspector");
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
                "Upgrade Selected Release",
                IconName::ArrowUp,
            ),
            (
                "helm.rollback",
                "Roll Back Selected Release",
                IconName::Clock,
            ),
            (
                "helm.uninstall",
                "Uninstall Selected Release",
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
        assert_eq!(check.label, "Check for Updates");
        let CommandRun::Action(make_check) = check.run else {
            panic!("Check for Updates must dispatch the shell action");
        };
        assert_eq!(make_check().name(), "k8s_app::CheckForUpdates");

        let restart = commands
            .iter()
            .find(|command| command.id == RESTART_TO_UPDATE_COMMAND_ID)
            .expect("Restart to Update command exists");
        assert_eq!(restart.label, "Restart to Update");
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
        assert_eq!(command.label, "Open Port Forwards");
        let CommandRun::Action(make_action) = command.run else {
            panic!("Open Port Forwards must dispatch the shell action");
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
            panic!("Refresh View must dispatch the shell action");
        };
        assert_eq!(make_action().name(), "k8s_shell::RefreshView");
    }

    #[test]
    fn reload_kubeconfigs_command_dispatches_shell_action() {
        let command = demo_commands(true)
            .into_iter()
            .find(|command| command.id == "cluster.reload_kubeconfigs")
            .expect("reload kubeconfigs command exists");
        assert_eq!(command.label, "Reload Kubeconfigs");
        let CommandRun::Action(make_action) = command.run else {
            panic!("Reload Kubeconfigs must dispatch the shell action");
        };
        assert_eq!(make_action().name(), "k8s_shell::ReloadKubeconfigs");
    }
}
