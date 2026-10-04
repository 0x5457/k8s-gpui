use gpui_kit::App;
#[cfg(target_os = "macos")]
use gpui_kit::Global;
#[cfg(any(target_os = "macos", test))]
use gpui_kit::{Menu, MenuItem, OsAction, SystemMenuType};
#[cfg(any(target_os = "macos", test))]
use k8s_actions::{HideOthers, ShowAll, ToggleFullScreen};
#[cfg(any(target_os = "macos", test))]
use k8s_ui::settings::OpenSettings;
#[cfg(any(target_os = "macos", test))]
use k8s_ui::shell::{
    ApplyYaml, CloseAllTabs, CloseOtherTabs, CloseTab, Copy, CopySelectedPodName, Cut,
    DescribeSelection, ExecSelection, FocusYaml, MoveTabLeft, MoveTabRight, NextTab,
    OpenContextSwitcher, OpenEvents, OpenForwards, OpenLogs, OpenNamespaceSwitcher, OpenOverview,
    OpenResourceKindSwitcher, OpenServiceAccount, Paste, PortForwardSelection, PreviousTab, Redo,
    RefreshView, ReloadKeymap, ReloadKubeconfigs, RestartSelection, ScaleSelection,
    SearchResources, SelectAll, ToggleCommandPalette, ToggleDock, ToggleLeftPanel,
    ToggleNotifications, TogglePinTab, ToggleRightPanel, ToggleTheme, Undo, UseDarkTheme,
    UseLightTheme,
};

/// The product's own name, which is the name the menu bar, the themes and the
/// updater all use. The binary and the bundle id stay `k8s-gpui`: those identify
/// the build, and a person is never asked to read either of them.
const PRODUCT_NAME: &str = "K8s Studio";

/// The one spelling of the product's name, for a surface that cannot reach the
/// constant directly.
pub fn product_name() -> &'static str {
    PRODUCT_NAME
}

pub fn install(cx: &mut App) {
    cx.set_app_identity("dev.k8s-gpui.app", PRODUCT_NAME);
    #[cfg(target_os = "macos")]
    install_actions_once(cx);
    refresh(cx);
}

pub fn refresh(_cx: &mut App) {
    #[cfg(target_os = "macos")]
    _cx.set_menus(app_menus());
}

#[cfg(target_os = "macos")]
struct MenuActionsInstalled;

#[cfg(target_os = "macos")]
impl Global for MenuActionsInstalled {}

#[cfg(target_os = "macos")]
fn install_actions_once(cx: &mut App) {
    if cx.has_global::<MenuActionsInstalled>() {
        return;
    }
    cx.on_action(|_: &HideOthers, cx: &mut App| cx.hide_other_apps())
        .on_action(|_: &ShowAll, cx: &mut App| cx.unhide_other_apps())
        .on_action(|_: &ToggleFullScreen, cx: &mut App| {
            let Some(window) = cx.active_window() else {
                return;
            };
            let _ = window.update(cx, |_, window, _| window.toggle_fullscreen());
        });
    cx.set_global(MenuActionsInstalled);
}

/// Menu structure. It is built on every platform so the contract is testable without a Mac,
/// and installed as the native menu bar only on macOS.
#[cfg(any(target_os = "macos", test))]
fn app_menus() -> Vec<Menu> {
    vec![
        Menu::new(PRODUCT_NAME).items([
            MenuItem::action("About K8s Studio", crate::About),
            MenuItem::separator(),
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::action("Toggle light/dark theme", ToggleTheme),
            MenuItem::action("Use light theme", UseLightTheme),
            MenuItem::action("Use dark theme", UseDarkTheme),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide K8s Studio", crate::Hide),
            MenuItem::action("Hide others", HideOthers),
            MenuItem::action("Show all", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit K8s Studio", crate::Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("Refresh view", RefreshView),
            MenuItem::action("Reload kubeconfigs", ReloadKubeconfigs),
            MenuItem::action("Reload keymap", ReloadKeymap),
            MenuItem::separator(),
            // Every close command lives in File, so one action has one menu entry and one key.
            MenuItem::action("Close tab", CloseTab),
            MenuItem::action("Close other tabs", CloseOtherTabs),
            MenuItem::action("Close all tabs", CloseAllTabs),
            MenuItem::action("Close window", crate::CloseWindow),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", Undo, OsAction::Undo),
            MenuItem::os_action("Redo", Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::separator(),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            MenuItem::action("Focus YAML", FocusYaml),
            MenuItem::action("Apply YAML changes…", ApplyYaml),
            MenuItem::action("Describe selected resource", DescribeSelection),
            MenuItem::action("Open Service Account for selected Pod", OpenServiceAccount),
            MenuItem::action("Show logs for selected Pod", OpenLogs),
            MenuItem::action("Show events for selected Pod", OpenEvents),
            MenuItem::action("Exec in selected Pod", ExecSelection),
            MenuItem::action("Start port forward for selected Pod…", PortForwardSelection),
            MenuItem::action("Restart selected resource", RestartSelection),
            MenuItem::action("Scale selected resource…", ScaleSelection),
            MenuItem::action("Copy selected resource name", CopySelectedPodName),
        ]),
        Menu::new("View").items([
            MenuItem::action("Command palette…", ToggleCommandPalette),
            MenuItem::action("Switch context…", OpenContextSwitcher),
            MenuItem::action("Switch namespace…", OpenNamespaceSwitcher),
            MenuItem::action("Choose resource kind…", OpenResourceKindSwitcher),
            MenuItem::action("Open cluster overview", OpenOverview),
            MenuItem::action("Open port forwards", OpenForwards),
            MenuItem::action("Search cluster resources…", SearchResources),
            MenuItem::separator(),
            MenuItem::action("Toggle sidebar", ToggleLeftPanel),
            MenuItem::action("Toggle inspector", ToggleRightPanel),
            MenuItem::action("Toggle dock", ToggleDock),
            MenuItem::action("Toggle notifications", ToggleNotifications),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", crate::MinimizeWindow),
            MenuItem::action("Zoom", crate::ZoomWindow),
            MenuItem::action("Enter or exit full screen", ToggleFullScreen),
            MenuItem::separator(),
            MenuItem::action("Previous tab", PreviousTab),
            MenuItem::action("Next tab", NextTab),
            MenuItem::action("Move tab left", MoveTabLeft),
            MenuItem::action("Move tab right", MoveTabRight),
            MenuItem::action("Toggle pin tab", TogglePinTab),
        ]),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use k8s_ui::keymap::{UNBOUND_ACTIONS, built_in_action_names};
    use k8s_ui::shell::commands::{MENU_ONLY_ACTIONS, NATIVE_MENU_TITLES};

    use super::*;

    /// Chrome the binary owns: the action type lives in the app binary, so the palette in k8s-ui
    /// cannot dispatch it. Each one has a default key.
    const BINARY_OWNED: &[&str] = &[
        "k8s_app::About",
        "k8s_app::Hide",
        "k8s_app::Quit",
        "k8s_app::CloseWindow",
        "k8s_app::MinimizeWindow",
        "k8s_app::ZoomWindow",
    ];

    fn action_items(menu: &Menu) -> Vec<(&str, &str)> {
        menu.items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action { name, action, .. } => Some((name.as_ref(), action.name())),
                _ => None,
            })
            .collect()
    }

    fn find_menu<'a>(menus: &'a [Menu], name: &str) -> &'a Menu {
        menus
            .iter()
            .find(|menu| menu.name.as_ref() == name)
            .unwrap_or_else(|| panic!("menu {name} exists"))
    }

    fn labels(menu: &Menu) -> BTreeSet<&str> {
        action_items(menu)
            .into_iter()
            .map(|(label, _)| label)
            .collect()
    }

    /// One command, one menu entry: a repeated command gives keyboard users two places to look for
    /// the same shortcut and drifts from the keymap. A label also stays unique inside its menu so
    /// every entry can be named unambiguously.
    ///
    /// There is no exception list. About used to appear in the app menu and again in a Help menu
    /// that held nothing else, and the keymap preset used to appear once per preset beside two
    /// palette rows and a Settings row; both repeats are gone, so a new one has to be argued for
    /// where the product is changed rather than granted here.
    #[test]
    fn every_action_appears_once_in_the_menu_bar() {
        let menus = app_menus();
        let mut actions: Vec<&str> = Vec::new();
        for menu in &menus {
            let items = action_items(menu);
            assert_eq!(
                labels(menu).len(),
                items.len(),
                "duplicate label in menu {}",
                menu.name
            );
            actions.extend(items.into_iter().map(|(_, action)| action));
        }
        actions.sort_unstable();
        for pair in actions.windows(2) {
            assert_ne!(pair[0], pair[1], "{:?} must have one menu entry", pair[0]);
        }
    }

    /// Close commands belong to File. The Window menu keeps the window commands and tab
    /// navigation, and never repeats a close command under any label.
    #[test]
    fn close_commands_live_in_the_file_menu() {
        const FILE_CLOSE_ENTRIES: [(&str, &str); 4] = [
            ("Close tab", "k8s_shell::CloseTab"),
            ("Close other tabs", "k8s_shell::CloseOtherTabs"),
            ("Close all tabs", "k8s_shell::CloseAllTabs"),
            ("Close window", "k8s_app::CloseWindow"),
        ];
        let menus = app_menus();
        let file = find_menu(&menus, "File");
        let file_labels = labels(file);
        let file_actions: BTreeSet<&str> = action_items(file)
            .into_iter()
            .map(|(_, action)| action)
            .collect();
        for (label, action) in FILE_CLOSE_ENTRIES {
            assert!(file_labels.contains(label), "File must offer {label}");
            assert!(file_actions.contains(action), "File must dispatch {action}");
        }
        for name in ["Window", "View", "Edit", "K8s Studio"] {
            let repeated: Vec<&str> = action_items(find_menu(&menus, name))
                .into_iter()
                .filter(|(label, action)| {
                    label.starts_with("Close")
                        || FILE_CLOSE_ENTRIES
                            .iter()
                            .any(|(_, close)| *close == *action)
                })
                .map(|(label, _)| label)
                .collect();
            assert!(
                repeated.is_empty(),
                "{name} must not repeat a close command: {repeated:?}"
            );
        }
    }

    /// The Window menu is tab navigation and window chrome. Every tab command it offers has a
    /// default key, so the tab bar, this menu, and the Settings keyboard panel agree.
    #[test]
    fn window_menu_offers_keyed_tab_commands() {
        const TAB_ACTIONS: [&str; 5] = [
            "k8s_shell::PreviousTab",
            "k8s_shell::NextTab",
            "k8s_shell::MoveTabLeft",
            "k8s_shell::MoveTabRight",
            "k8s_shell::TogglePinTab",
        ];
        let bound = built_in_action_names();
        let menus = app_menus();
        let window = find_menu(&menus, "Window");
        let actions: BTreeSet<&str> = action_items(window)
            .into_iter()
            .map(|(_, action)| action)
            .collect();
        for action in TAB_ACTIONS {
            assert!(actions.contains(action), "Window must offer {action}");
            assert!(bound.contains(action), "{action} has no default key");
        }
    }

    /// Every menu entry is a product action that either has a default key or is explicitly listed
    /// as unbound, so no menu item is a dead end for keyboard and screen reader users.
    #[test]
    fn every_menu_action_has_a_key_or_is_explicitly_unbound() {
        let bound = built_in_action_names();
        for menu in app_menus() {
            for (label, action) in action_items(&menu) {
                assert!(
                    action.starts_with("k8s_"),
                    "{label} must use a product action, found {action}"
                );
                assert!(
                    bound.contains(action) || UNBOUND_ACTIONS.contains(&action),
                    "{label} ({action}) has no default key and no unbound entry"
                );
            }
        }
    }

    /// Title the palette shows for a menu action, or `None` when the menu offers a title the
    /// palette never uses.
    ///
    /// Every action the menu offers is named in `NATIVE_MENU_TITLES`, so the menu and the
    /// palette read one title for it.
    fn palette_title(label: &str, action: &str) -> Option<String> {
        NATIVE_MENU_TITLES
            .iter()
            .find(|(title, menu_action)| *title == label && *menu_action == action)
            .map(|(title, _)| (*title).to_owned())
    }

    /// k8s-ui cannot see this menu, so the palette sweep can only prove the palette is a superset.
    /// This closes the other direction here, in the binary that owns the menu: a title the menu
    /// bar shows is also a palette row for the same action.
    #[test]
    fn every_menu_action_has_one_palette_row_with_the_same_title() {
        for menu in app_menus() {
            for (label, action) in action_items(&menu) {
                if BINARY_OWNED.contains(&action) || MENU_ONLY_ACTIONS.contains(&action) {
                    continue;
                }
                assert_eq!(
                    palette_title(label, action).as_deref(),
                    Some(label),
                    "{label} ({action}) has no palette row with the same title"
                );
            }
        }
    }

    /// Menu coverage for the commands the palette gained. Tab steps and the palette and
    /// notification toggles are menu commands, so the menu bar keeps offering them. The system
    /// theme choice belongs to Settings, and the three release operations have no action for a
    /// menu item to dispatch, so both stay palette-only.
    #[test]
    fn new_palette_commands_have_a_decided_menu_coverage() {
        let menus = app_menus();
        let mut offered: Vec<(&str, &str)> = Vec::new();
        for menu in &menus {
            offered.extend(action_items(menu));
        }
        for (title, action) in [
            ("Next tab", "k8s_shell::NextTab"),
            ("Previous tab", "k8s_shell::PreviousTab"),
            ("Command palette…", "k8s_shell::ToggleCommandPalette"),
            ("Toggle notifications", "k8s_shell::ToggleNotifications"),
        ] {
            assert!(
                offered.contains(&(title, action)),
                "{title} must stay a menu command"
            );
        }
        // A release operation owns no key either: the palette, the detail action bar, and the
        // release menu each open the same confirmation, and the destructive one must never be one
        // keystroke away.
        for title in [
            "Use system theme",
            "Upgrade selected release…",
            "Roll back selected release…",
            "Uninstall selected release…",
        ] {
            assert!(
                !menus.iter().any(|menu| labels(menu).contains(title)),
                "{title} must stay off the menu bar"
            );
        }
    }
}
