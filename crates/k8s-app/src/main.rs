#![cfg_attr(windows, windows_subsystem = "windows")]
#![deny(unsafe_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use assets::Assets;
use gpui::{
    App, AppContext, Bounds, Global, PromptLevel, TitlebarOptions, Window, WindowAppearance,
    WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowOptions, actions, point, px,
    size,
};
use k8s_app::updater::{GlobalUpdater, UpdaterRuntime, UpdaterStatus, UpdaterStatusCallback};
use k8s_core::cluster::ClusterRegistry;
use k8s_ui::design;
use k8s_ui::settings::{self as ui_settings, ThemeChoice};
use k8s_ui::shell::commands::UPDATER_UNAVAILABLE_REASON;
use k8s_ui::shell::{
    STARTUP_LOADING_REASON, Shell, ToggleTheme, UseDarkTheme, UseLightTheme, UseSystemTheme,
    UseTheme,
};
use k8s_ui::table_view::ClusterSession;
use k8s_ui::{UpdateActions, UpdatePhase, UpdateUiState};
use settings::{Settings as _, SettingsStore};
use theme::{ActiveTheme, Appearance, SystemAppearance, ThemeRegistry};
use theme_settings::{ThemeAppearanceMode, ThemeName, ThemeSelection, ThemeSettings};

mod diagnostics;
#[cfg_attr(windows, allow(unsafe_code))]
mod instance;
mod menus;
mod terminal;

// App-level actions.
actions!(
    k8s_app,
    [About, MinimizeWindow, ZoomWindow, Hide, CloseWindow, Quit]
);

const PRODUCT_THEME_LIGHT: &str = "K8s Studio Light";
const PRODUCT_THEME_DARK: &str = "K8s Studio Dark";
const PRODUCT_THEME: &str = include_str!("../assets/themes/k8s-studio.json");

fn install_product_theme(cx: &mut App) {
    if let Err(error) =
        theme_settings::load_user_theme(&ThemeRegistry::global(cx), PRODUCT_THEME.as_bytes())
    {
        eprintln!("k8s-gpui: product theme load failed: {error:#}");
    }
}

fn refine_active_theme(cx: &mut App) {
    let mut refined = cx.theme().as_ref().clone();
    k8s_ui::design::refine_theme_with_contrast(
        &mut refined,
        ui_settings::increase_contrast_enabled(cx),
    );
    theme::GlobalTheme::update_theme(cx, Arc::new(refined));
}

/// Current theme preference.
struct ThemePreference(ThemeSelection);

impl Global for ThemePreference {}

fn user_settings_path() -> Option<PathBuf> {
    k8s_core::paths::config_file("settings.json")
}

/// Loads user settings into memory. JSONC and the full settings schema are supported.
/// A parse error is logged and does not block startup.
fn load_user_settings(cx: &mut App) {
    let Some(path) = user_settings_path() else {
        ui_settings::sync_increase_contrast(cx);
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        ui_settings::sync_increase_contrast(cx);
        return;
    };
    ui_settings::sync_increase_contrast(cx);
    let result = SettingsStore::update(cx, |store, cx| store.set_user_settings(&text, cx));
    if matches!(result.parse_status, settings::ParseStatus::Failed { .. }) {
        eprintln!("k8s-gpui: settings.json parse failed. Using built-in defaults: {result:?}");
    } else {
        eprintln!("k8s-gpui: settings.json loaded: {}", path.display());
    }
}

fn reduce_motion_enabled(cx: &App) -> bool {
    ui_settings::reduce_motion_enabled(cx)
}

fn install_reduce_motion(cx: &mut App) {
    let apply = |cx: &mut App| cx.set_reduce_motion(reduce_motion_enabled(cx));
    apply(cx);
    cx.observe_global::<SettingsStore>(apply).detach();
}

/// Returns the theme set in settings.json, or None to follow the system theme.
fn user_theme_selection(cx: &App) -> Option<ThemeSelection> {
    let selection = cx
        .try_global::<SettingsStore>()
        .and_then(|store| store.raw_user_settings())
        .and_then(|settings| settings.content.theme.theme.as_ref())?;
    Some(match selection {
        settings::ThemeSelection::Static(theme) => {
            ThemeSelection::Static(ThemeName(theme.0.clone()))
        }
        settings::ThemeSelection::Dynamic { mode, light, dark } => ThemeSelection::Dynamic {
            mode: *mode,
            light: ThemeName(light.0.clone()),
            dark: ThemeName(dark.0.clone()),
        },
    })
}

fn system_theme_selection() -> ThemeSelection {
    ThemeSelection::Dynamic {
        mode: ThemeAppearanceMode::System,
        light: ThemeName(PRODUCT_THEME_LIGHT.into()),
        dark: ThemeName(PRODUCT_THEME_DARK.into()),
    }
}

fn theme_selection(choice: ThemeChoice) -> ThemeSelection {
    match choice {
        ThemeChoice::System => system_theme_selection(),
        ThemeChoice::Light => ThemeSelection::Static(ThemeName(PRODUCT_THEME_LIGHT.into())),
        ThemeChoice::Dark => ThemeSelection::Static(ThemeName(PRODUCT_THEME_DARK.into())),
        ThemeChoice::Named(name) => ThemeSelection::Static(ThemeName(name.into())),
    }
}

fn registered_theme_selection(cx: &mut App, selection: &ThemeSelection) -> bool {
    let Some(registry) = ThemeRegistry::try_global(cx) else {
        return true;
    };
    match selection {
        ThemeSelection::Static(theme) => registry.get(&theme.0).is_ok(),
        ThemeSelection::Dynamic { light, dark, .. } => {
            registry.get(&light.0).is_ok() && registry.get(&dark.0).is_ok()
        }
    }
}

fn normalize_theme_selection(cx: &mut App, selection: ThemeSelection) -> ThemeSelection {
    if registered_theme_selection(cx, &selection) {
        selection
    } else {
        eprintln!("k8s-gpui: unknown configured theme. Using system themes.");
        system_theme_selection()
    }
}

fn theme_choice_for_selection(selection: &ThemeSelection, appearance: Appearance) -> ThemeChoice {
    match selection.mode() {
        Some(ThemeAppearanceMode::System) => ThemeChoice::System,
        _ => ThemeChoice::Named(selection.name(appearance).0.to_string()),
    }
}

/// Applies a theme through the in-memory settings store and reloads it.
fn apply_theme(cx: &mut App, selection: ThemeSelection) {
    let selection = normalize_theme_selection(cx, selection);
    if cx
        .try_global::<ThemePreference>()
        .is_some_and(|preference| preference.0 == selection)
    {
        refine_active_theme(cx);
        sync_window_appearance(cx);
        let choice = theme_choice_for_selection(&selection, cx.theme().appearance());
        ui_settings::set_theme_choice(cx, choice);
        return;
    }
    cx.set_global(ThemePreference(selection.clone()));
    let mut settings = ThemeSettings::get_global(cx).clone();
    settings.theme = selection.clone();
    SettingsStore::update(cx, |store, _| store.override_global(settings));
    theme_settings::reload_theme(cx);
    refine_active_theme(cx);
    sync_window_appearance(cx);
    // Read the selection after reload. The system appearance can change during reload.
    let choice = theme_choice_for_selection(&selection, cx.theme().appearance());
    ui_settings::set_theme_choice(cx, choice);
}

fn forced_window_appearance(
    selection: &ThemeSelection,
    appearance: Appearance,
) -> Option<WindowAppearance> {
    let appearance = || match appearance {
        Appearance::Light => WindowAppearance::Light,
        Appearance::Dark => WindowAppearance::Dark,
    };
    match selection {
        ThemeSelection::Static(_) => Some(appearance()),
        ThemeSelection::Dynamic {
            mode: ThemeAppearanceMode::System,
            ..
        } => None,
        ThemeSelection::Dynamic { .. } => Some(appearance()),
    }
}

fn sync_window_appearance(cx: &App) {
    let selection = cx
        .try_global::<ThemePreference>()
        .map(|preference| preference.0.clone())
        .unwrap_or_else(system_theme_selection);
    cx.set_window_appearance(forced_window_appearance(
        &selection,
        cx.theme().appearance(),
    ));
}

fn set_theme_appearance(cx: &mut App, appearance: Appearance) {
    let choice = match appearance {
        Appearance::Light => ThemeChoice::Light,
        Appearance::Dark => ThemeChoice::Dark,
    };
    // Apply the appearance first. A save error does not change this session.
    let persist = ui_settings::theme_choice(cx) != choice;
    apply_theme(cx, theme_selection(choice.clone()));
    if persist
        && let Err(error) = ui_settings::update(cx, |settings| {
            settings.theme = Some(choice.value());
        })
    {
        eprintln!("k8s-gpui: failed to save theme: {error}");
    }
}

/// Applies the configured theme at startup. Missing settings follow the system theme.
fn install_theme(cx: &mut App, window: &Window) {
    let appearance: Appearance = window.appearance().into();
    *SystemAppearance::global_mut(cx) = SystemAppearance(appearance);
    let selection = user_theme_selection(cx).unwrap_or_else(system_theme_selection);
    eprintln!("k8s-gpui: appearance = {appearance:?}, selection = {selection:?}");
    apply_theme(cx, selection);
}

fn sync_theme_from_settings(cx: &mut App) {
    ui_settings::sync_increase_contrast(cx);
    let selection = user_theme_selection(cx).unwrap_or_else(system_theme_selection);
    apply_theme(cx, selection);
}

fn install_theme_controller(cx: &mut App) {
    cx.observe_global::<SettingsStore>(sync_theme_from_settings)
        .detach();
}

/// Reloads the theme when the system appearance changes in system mode.
fn watch_system_appearance(window: &Window) {
    window
        .observe_window_appearance(|window, cx| {
            let appearance: Appearance = window.appearance().into();
            if *SystemAppearance::global(cx) == appearance {
                return;
            }
            *SystemAppearance::global_mut(cx) = SystemAppearance(appearance);
            if cx.global::<ThemePreference>().0.mode() == Some(ThemeAppearanceMode::System) {
                theme_settings::reload_theme(cx);
                refine_active_theme(cx);
                sync_window_appearance(cx);
            }
        })
        .detach();
}

/// Installs app-level handlers for theme actions.
fn install_theme_handlers(cx: &mut App) {
    cx.on_action(|_: &ToggleTheme, cx: &mut App| {
        let target = match cx.theme().appearance() {
            Appearance::Light => Appearance::Dark,
            Appearance::Dark => Appearance::Light,
        };
        set_theme_appearance(cx, target);
    });
    cx.on_action(|_: &UseLightTheme, cx: &mut App| set_theme_appearance(cx, Appearance::Light));
    cx.on_action(|_: &UseDarkTheme, cx: &mut App| set_theme_appearance(cx, Appearance::Dark));
    cx.on_action(|_: &UseSystemTheme, cx: &mut App| {
        apply_theme(cx, system_theme_selection());
    });
    cx.on_action(|action: &UseTheme, cx: &mut App| {
        if ThemeRegistry::global(cx).get(&action.name).is_ok() {
            apply_theme(
                cx,
                ThemeSelection::Static(ThemeName(action.name.clone().into())),
            );
        }
    });
}

fn window_options() -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(24.0), px(24.0)),
            size(px(1280.0), px(720.0)),
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some("k8s-gpui".into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        app_id: Some("k8s-gpui".to_owned()),
        window_background: WindowBackgroundAppearance::Opaque,
        window_decorations: Some(WindowDecorations::Server),
        // The supported floor comes from the design token, so the window manager and the shell
        // layout cannot end up enforcing two different numbers. `DESIGN.md` §6 promises 960x640.
        window_min_size: Some(size(
            px(design::size::WINDOW_MIN.0),
            px(design::size::WINDOW_MIN.1),
        )),
        ..Default::default()
    }
}

fn show_about(cx: &mut App) {
    let Some(window) = cx.active_window() else {
        return;
    };
    let _ = window.update(cx, |_, window, cx| {
        let detail = format!(
            "Version {}\n\nManage Kubernetes clusters and workloads.",
            env!("CARGO_PKG_VERSION")
        );
        drop(window.prompt(PromptLevel::Info, "K8s GPUI", Some(&detail), &["OK"], cx));
    });
}

fn minimize_active_window(cx: &mut App) {
    let Some(window) = cx.active_window() else {
        return;
    };
    let _ = window.update(cx, |_, window, _| window.minimize_window());
}

fn zoom_active_window(cx: &mut App) {
    let Some(window) = cx.active_window() else {
        return;
    };
    let _ = window.update(cx, |_, window, _| window.zoom_window());
}

fn close_active_window(cx: &mut App) {
    let Some(window) = cx.active_window() else {
        return;
    };
    let _ = window.update(cx, |_, window, _| window.remove_window());
}

fn install_menu_handlers(cx: &mut App) {
    cx.on_action(|_: &About, cx: &mut App| show_about(cx));
    cx.on_action(|_: &MinimizeWindow, cx: &mut App| minimize_active_window(cx));
    cx.on_action(|_: &ZoomWindow, cx: &mut App| zoom_active_window(cx));
    cx.on_action(|_: &Hide, cx: &mut App| cx.hide());
    cx.on_action(|_: &CloseWindow, cx: &mut App| close_active_window(cx));
    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
}

fn install_zoom_handlers(_cx: &mut App) {}

/// Watches the keymap directory and applies each saved change.
/// Directory watching also detects a newly created keymap file.
fn install_keymap_watch(cx: &mut App) {
    let Some(path) = k8s_ui::keymap::user_keymap_path() else {
        return;
    };
    let Some(dir) = path.parent().map(Path::to_path_buf) else {
        return;
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "k8s-gpui: failed to create config directory {}: {error}",
            dir.display()
        );
        return;
    }
    let fs: Arc<dyn fs::Fs> = fs::RealFs::new(None, cx.background_executor().clone());
    // The watcher requires Zed's HashSet hasher. The call signature infers it.
    let mut config_paths = HashSet::with_hasher(Default::default());
    config_paths.insert(path);
    let mut keymap_contents =
        settings::watch_config_dir(cx.background_executor(), fs, dir, config_paths);
    cx.spawn(async move |cx| {
        while let Ok(contents) = keymap_contents.recv().await {
            cx.update(|cx| {
                if k8s_ui::keymap::reload_from_source(cx, &contents) {
                    eprintln!("k8s-gpui: keymap reloaded");
                }
            });
        }
    })
    .detach();
}

fn install_settings_watch(cx: &mut App) {
    let Some(path) = user_settings_path() else {
        return;
    };
    let Some(dir) = path.parent().map(Path::to_path_buf) else {
        return;
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "k8s-gpui: failed to create config directory {}: {error}",
            dir.display()
        );
        return;
    }
    let fs: Arc<dyn fs::Fs> = fs::RealFs::new(None, cx.background_executor().clone());
    let mut config_paths = HashSet::with_hasher(Default::default());
    config_paths.insert(path);
    let mut settings_contents =
        settings::watch_config_dir(cx.background_executor(), fs, dir, config_paths);
    cx.spawn(async move |cx| {
        while let Ok(contents) = settings_contents.recv().await {
            cx.update(|cx| {
                let result =
                    SettingsStore::update(cx, |store, cx| store.set_user_settings(&contents, cx));
                if matches!(result.parse_status, settings::ParseStatus::Failed { .. }) {
                    eprintln!("k8s-gpui: settings.json reload failed: {result:?}");
                } else {
                    ui_settings::sync_increase_contrast(cx);
                    sync_theme_from_settings(cx);
                    cx.refresh_windows();
                }
            });
        }
    })
    .detach();
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum StartupState {
    Loading,
    Ready,
    Unavailable(String),
}

impl StartupState {
    fn reason(&self) -> &str {
        match self {
            Self::Loading => STARTUP_LOADING_REASON,
            Self::Ready => "",
            Self::Unavailable(reason) => reason,
        }
    }

    fn finish(
        &mut self,
        handle: &instance::RegistryHandle,
        result: &Result<Arc<ClusterRegistry>, String>,
    ) {
        match result {
            Ok(registry) => {
                handle.replace(Arc::clone(registry));
                *self = Self::Ready;
            }
            Err(error) => {
                *self = Self::Unavailable(format!(
                    "Kubeconfig load failed: {error}. Check the kubeconfig and try again."
                ));
            }
        }
    }
}

struct LoadedCluster {
    registry: Arc<ClusterRegistry>,
    session: ClusterSession,
}

fn spawn_registry_load(
    handle: &tokio::runtime::Handle,
) -> tokio::task::JoinHandle<Result<LoadedCluster, String>> {
    let runtime = handle.clone();
    handle.spawn(async move {
        let registry = Arc::new(
            ClusterRegistry::load_default()
                .await
                .map_err(|error| format!("{error:#}"))?,
        );
        let session = ClusterSession::from_registry(Arc::clone(&registry), runtime);
        if let ClusterSession::Unavailable { reason, .. } = &session {
            return Err(reason.clone());
        }
        Ok(LoadedCluster { registry, session })
    })
}

fn should_open_window(ipc_result: instance::InstallResult) -> bool {
    !matches!(ipc_result, instance::InstallResult::AlreadyRunning)
}

fn should_start_updater(ipc_result: instance::InstallResult) -> bool {
    matches!(ipc_result, instance::InstallResult::Started)
}

fn install_updater(
    cx: &mut App,
) -> Option<(
    UpdaterRuntime,
    tokio::sync::mpsc::UnboundedReceiver<UpdaterStatus>,
)> {
    if let Some(reason) = k8s_app::updater::initial_unavailable_reason() {
        eprintln!("k8s-gpui: updater unavailable: {reason}");
        return None;
    }
    let (status_tx, status_rx) = tokio::sync::mpsc::unbounded_channel();
    let callback: UpdaterStatusCallback = Arc::new(move |status| {
        let _ = status_tx.send(status);
    });
    let runtime = match UpdaterRuntime::new(Arc::clone(&callback)) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("k8s-gpui: updater initialization failed: {error:#}");
            return None;
        }
    };
    cx.set_global(GlobalUpdater(runtime.clone()));
    let handle = gpui_tokio::Tokio::handle(cx);
    let poll_runtime = runtime.clone();
    handle.spawn(async move {
        let _poll_task = poll_runtime.spawn_auto_poll();
    });
    Some((runtime, status_rx))
}

fn update_actions(runtime: UpdaterRuntime) -> UpdateActions {
    let check_runtime = runtime.clone();
    let retry_runtime = runtime.clone();
    let restart_runtime = runtime;
    UpdateActions::new(
        move |cx| {
            let runtime = check_runtime.clone();
            let handle = gpui_tokio::Tokio::handle(cx);
            handle.spawn(async move {
                let _ = runtime.check().await;
            });
        },
        move |cx| {
            let runtime = retry_runtime.clone();
            let handle = gpui_tokio::Tokio::handle(cx);
            handle.spawn(async move {
                let _ = runtime.check().await;
            });
        },
        move |_cx| {
            if let Err(error) = restart_runtime.request_restart() {
                eprintln!("k8s-gpui: updater restart failed: {error:#}");
            }
        },
    )
}

fn run_version_subcommand() -> bool {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--version") || args.next().is_some() {
        return false;
    }
    println!("k8s-app {}", env!("CARGO_PKG_VERSION"));
    true
}

fn run_install_user_subcommand() -> bool {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("install-user") || args.next().is_some() {
        return false;
    }
    match k8s_app::updater::install_user() {
        Ok(path) => println!("Managed k8s-gpui executable: {}", path.display()),
        Err(error) => {
            eprintln!("k8s-gpui: user installation failed: {error:#}");
            std::process::exit(1);
        }
    }
    true
}

/// When the app is allowed to end on its own.
///
/// Linux gets `Explicit`. The default there is `LastWindowClosed`, so a window that goes away,
/// or an entity that is dropped with its window, ends the process with no exit code and nothing
/// in the log: a crash on the way out looks exactly like a clean shutdown. With `Explicit` the
/// only exit is one this process asked for, and
/// [`install_quit_observer`] turns the last window closing into that ask.
fn quit_mode() -> gpui::QuitMode {
    if cfg!(target_os = "linux") {
        gpui::QuitMode::Explicit
    } else {
        gpui::QuitMode::Default
    }
}

/// The exit path `QuitMode::Explicit` asks for: closing the last window quits, and says so.
///
/// The window is already gone by the time this runs, so the reason is the only record of why the
/// process ended. A window that disappears while others are still open is reported and nothing
/// else happens.
fn install_quit_observer(cx: &mut App) {
    cx.on_window_closed(|cx, window_id| {
        if should_quit_after_window_close(cx.windows().len()) {
            eprintln!("k8s-gpui: last window {window_id:?} closed, quitting");
            cx.quit();
        } else {
            eprintln!(
                "k8s-gpui: window {window_id:?} closed, {} still open",
                cx.windows().len()
            );
        }
    })
    .detach();
}

/// Quits once no window is left. The count is taken after the closed window is gone, so zero means
/// this one was the last.
fn should_quit_after_window_close(remaining_windows: usize) -> bool {
    remaining_windows == 0
}

fn main() {
    if run_version_subcommand() || run_install_user_subcommand() {
        return;
    }

    diagnostics::crash::install();
    let app = gpui_platform::application()
        .with_assets(Assets)
        .with_quit_mode(quit_mode());

    app.run(|cx: &mut App| {
        gpui_tokio::init(cx);
        install_quit_observer(cx);
        settings::init(cx);
        ui_settings::install_product_typography_defaults(cx);
        // Load settings before opening the window so install_theme can read the user theme.
        load_user_settings(cx);
        install_reduce_motion(cx);
        theme_settings::init(theme::LoadThemes::All(Box::new(Assets)), cx);
        install_product_theme(cx);
        install_theme_controller(cx);
        install_settings_watch(cx);
        install_theme_handlers(cx);
        install_menu_handlers(cx);

        // Install the keymap first. It clears existing bindings. Diagnostics adds F1 after this.
        if let Err(error) = k8s_ui::keymap::install(cx) {
            eprintln!("k8s-gpui: keymap load failed. The valid part was loaded:\n{error:#}");
        }
        install_keymap_watch(cx);
        install_zoom_handlers(cx);
        menus::install(cx);
        cx.observe_global::<k8s_ui::keymap::KeymapStatus>(menus::refresh)
            .detach();
        diagnostics::init(cx);

        let registry = ThemeRegistry::global(cx);
        eprintln!(
            "k8s-gpui: loaded {} themes. Active theme: {:?}",
            registry.list_names().len(),
            cx.theme().name
        );

        let ipc_registry = instance::RegistryHandle::new(None);
        let ipc_result = instance::install(cx);
        if !should_open_window(ipc_result) {
            eprintln!(
                "k8s-gpui: another instance is already running. Exiting without opening a window"
            );
            cx.quit();
            return;
        }
        let updater = if should_start_updater(ipc_result) {
            install_updater(cx)
        } else {
            None
        };
        let update_actions = if k8s_app::updater::updater_configured() {
            updater
                .as_ref()
                .map(|(runtime, _)| update_actions(runtime.clone()))
        } else {
            None
        };
        let mut startup = StartupState::Loading;
        let session = ClusterSession::unavailable(STARTUP_LOADING_REASON);
        let terminals = terminal::services(&session);
        let terminals = terminals.is_available().then_some(terminals);
        let shell_registry = ipc_registry.clone();

        match cx.open_window(window_options(), move |window, cx| {
            install_theme(cx, window);
            watch_system_appearance(window);
            eprintln!(
                "k8s-gpui: active theme = {:?}, background = {}, panel = {}",
                cx.theme().name,
                cx.theme().colors().background,
                cx.theme().colors().panel_background
            );
            cx.new(|cx| {
                let mut shell = Shell::with_cluster(session, cx);
                shell.set_registry_reload_callback(Rc::new(move |registry| {
                    shell_registry.replace(registry);
                }));
                shell.set_terminal_services(terminals, cx);
                if let Some(actions) = update_actions {
                    shell.set_update_actions(actions, cx);
                } else {
                    shell.set_update_state(
                        UpdateUiState::new(UpdatePhase::Unsupported)
                            .with_error(UPDATER_UNAVAILABLE_REASON),
                        cx,
                    );
                }
                shell
            })
        }) {
            Ok(window) => {
                let Ok(shell) = window.entity(cx) else {
                    eprintln!("k8s-gpui: failed to resolve the Shell entity");
                    cx.quit();
                    return;
                };
                let registry_load = spawn_registry_load(&gpui_tokio::Tokio::handle(cx));
                if let Some((_, mut status_rx)) = updater {
                    let update_shell = shell.clone();
                    cx.spawn(async move |cx| {
                        while let Some(state) = status_rx.recv().await {
                            let executable = if state.phase == UpdatePhase::Restarting {
                                state.executable().map(Path::to_owned)
                            } else {
                                None
                            };
                            update_shell.update(cx, |shell, cx| {
                                shell.set_update_state(state, cx);
                            });
                            if let Some(executable) = executable {
                                cx.update(|cx| {
                                    cx.set_restart_path(executable);
                                    cx.restart();
                                });
                                break;
                            }
                        }
                    })
                    .detach();
                }
                let startup_registry = ipc_registry;
                cx.spawn(async move |cx| {
                    let result = match registry_load.await {
                        Ok(result) => result,
                        Err(error) => Err(format!("Kubeconfig load task failed: {error}")),
                    };
                    if startup_registry.current().is_some() {
                        eprintln!("k8s-gpui: discarded initial registry load after Shell reload");
                        return;
                    }
                    match result {
                        Ok(LoadedCluster { registry, session }) => {
                            let terminals = terminal::services(&session);
                            let terminals = terminals.is_available().then_some(terminals);
                            let applied = shell.update(cx, |shell, cx| {
                                if shell.replace_session(session, cx) {
                                    shell.set_terminal_services(terminals, cx);
                                    true
                                } else {
                                    false
                                }
                            });
                            if applied {
                                startup.finish(&startup_registry, &Ok(registry));
                            } else {
                                let keep_session = startup_registry.current().is_some();
                                startup.finish(
                                    &startup_registry,
                                    &Err("Shell kept the current session.".to_owned()),
                                );
                                if !keep_session {
                                    let reason = startup.reason().to_owned();
                                    shell.update(cx, |shell, cx| {
                                        shell.replace_session(
                                            ClusterSession::unavailable(reason),
                                            cx,
                                        );
                                        shell.set_terminal_services(None, cx);
                                    });
                                }
                            }
                        }
                        Err(error) => {
                            let keep_session = startup_registry.current().is_some();
                            startup.finish(&startup_registry, &Err(error.clone()));
                            if !keep_session {
                                let reason = startup.reason().to_owned();
                                shell.update(cx, |shell, cx| {
                                    shell.replace_session(ClusterSession::unavailable(reason), cx);
                                    shell.set_terminal_services(None, cx);
                                });
                            }
                            eprintln!(
                                "k8s-gpui: Kubeconfig load failed: {error}. Check the kubeconfig and try again."
                            );
                        }
                    }
                })
                .detach();
                cx.activate(true);
            }
            Err(error) => {
                eprintln!("k8s-gpui: failed to open window: {error:#}");
                cx.quit();
            }
        }
    });
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;

    #[test]
    fn theme_refinement_preserves_identity() {
        let themes = theme_settings::refine_theme_family(
            theme_settings::deserialize_user_theme(PRODUCT_THEME.as_bytes())
                .expect("product theme"),
        )
        .themes;
        for name in [PRODUCT_THEME_LIGHT, PRODUCT_THEME_DARK] {
            let current = Arc::new(
                themes
                    .iter()
                    .find(|theme| theme.name.as_ref() == name)
                    .expect("product theme variant")
                    .clone(),
            );
            let mut refined = current.as_ref().clone();
            k8s_ui::design::refine_theme(&mut refined);
            assert_eq!(refined.id, current.id);
            assert_eq!(refined.name, current.name);
            assert_eq!(refined.appearance, current.appearance);
        }
    }

    #[test]
    fn only_already_running_prevents_opening_a_window() {
        assert!(should_open_window(instance::InstallResult::Started));
        assert!(should_open_window(instance::InstallResult::Failed));
        assert!(!should_open_window(instance::InstallResult::AlreadyRunning));
    }

    /// Linux must not end the process on its own. The default there quits with the last window,
    /// so a window or entity that disappears takes the exit code and the reason with it.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_quits_only_on_request() {
        assert_eq!(quit_mode(), gpui::QuitMode::Explicit);
        assert_ne!(
            quit_mode(),
            gpui::QuitMode::LastWindowClosed,
            "the default mode is what this change exists to avoid"
        );
    }

    /// Closing the last window is the one exit `QuitMode::Explicit` still has to allow, or the app
    /// would never close. Any other window closing is not a reason to quit.
    #[test]
    fn the_last_window_close_still_quits() {
        assert!(should_quit_after_window_close(0));
        assert!(!should_quit_after_window_close(1));
        assert!(!should_quit_after_window_close(2));
    }

    #[test]
    fn updater_requires_a_successful_instance_lock() {
        assert!(should_start_updater(instance::InstallResult::Started));
        assert!(!should_start_updater(instance::InstallResult::Failed));
        assert!(!should_start_updater(
            instance::InstallResult::AlreadyRunning
        ));
    }

    #[test]
    fn startup_commit_keeps_last_good_registry_on_failure() {
        let handle = instance::RegistryHandle::default();
        let good = Arc::new(ClusterRegistry::default());
        handle.replace(Arc::clone(&good));

        let mut state = StartupState::Loading;
        state.finish(&handle, &Err("exec timed out".to_owned()));
        assert_eq!(
            state,
            StartupState::Unavailable(
                "Kubeconfig load failed: exec timed out. Check the kubeconfig and try again."
                    .to_owned(),
            )
        );
        assert!(Arc::ptr_eq(&handle.current().expect("registry"), &good));

        let replacement = Arc::new(ClusterRegistry::default());
        state.finish(&handle, &Ok(Arc::clone(&replacement)));
        assert_eq!(state, StartupState::Ready);
        assert!(Arc::ptr_eq(
            &handle.current().expect("registry"),
            &replacement
        ));
    }

    #[cfg(target_os = "macos")]
    const CONFIG_HOME_ENV: &str = "HOME";
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    const CONFIG_HOME_ENV: &str = "XDG_CONFIG_HOME";

    #[cfg(not(windows))]
    static ENVIRONMENT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[cfg(not(windows))]
    #[tokio::test(flavor = "current_thread")]
    async fn startup_uses_hotbar_with_partial_kubeconfig_sources() {
        let _guard = ENVIRONMENT_LOCK.lock().await;
        let root = tempfile::tempdir().expect("temp directory");
        let valid = root.path().join("valid.yaml");
        let invalid = root.path().join("invalid.yaml");
        std::fs::write(
            &valid,
            r#"
apiVersion: v1
kind: Config
clusters:
- name: cluster-one
  cluster:
    server: http://127.0.0.1:6443
- name: cluster-two
  cluster:
    server: http://127.0.0.1:6444
contexts:
- name: valid-ctx
  context: { cluster: cluster-one, user: user }
- name: second-ctx
  context: { cluster: cluster-two, user: user }
users:
- name: user
  user: {}
current-context: valid-ctx
"#,
        )
        .expect("write valid kubeconfig");
        std::fs::write(&invalid, "contexts: [").expect("write invalid kubeconfig");
        let value = std::env::join_paths([&valid, &invalid]).expect("join kubeconfig paths");
        let selected = k8s_core::cluster::ClusterId::derive("second-ctx", "http://127.0.0.1:6444/");
        let mut hotbar = k8s_core::hotbar::Hotbar::default();
        let bank = hotbar.create_bank("default").expect("create startup bank");
        hotbar
            .add_slot(bank, selected, "second-ctx")
            .expect("create startup slot");
        let previous_kubeconfig = std::env::var_os("KUBECONFIG");
        let previous_config_home = std::env::var_os(CONFIG_HOME_ENV);
        unsafe {
            std::env::set_var("KUBECONFIG", &value);
            std::env::set_var(CONFIG_HOME_ENV, root.path());
        }
        let config_dir = k8s_core::paths::config_dir().expect("test config directory");
        hotbar
            .save(config_dir.join("hotbar.json"))
            .expect("save startup hotbar");
        let result = spawn_registry_load(&tokio::runtime::Handle::current()).await;
        unsafe {
            match previous_kubeconfig {
                Some(value) => std::env::set_var("KUBECONFIG", value),
                None => std::env::remove_var("KUBECONFIG"),
            }
            match previous_config_home {
                Some(value) => std::env::set_var(CONFIG_HOME_ENV, value),
                None => std::env::remove_var(CONFIG_HOME_ENV),
            }
        }
        let loaded = result.expect("load task").expect("valid source must load");
        assert!(matches!(&loaded.session, ClusterSession::Ready { .. }));
        assert_eq!(loaded.registry.current_context(), Some("valid-ctx"));
        assert_eq!(loaded.session.cluster_name(), Some("second-ctx"));
        assert_eq!(
            loaded
                .registry
                .clusters()
                .iter()
                .map(|cluster| cluster.name())
                .collect::<Vec<_>>(),
            ["valid-ctx", "second-ctx"]
        );
        assert_eq!(loaded.registry.source_errors().len(), 1);
        assert!(
            loaded.registry.source_errors()[0]
                .to_string()
                .contains("invalid.yaml")
        );
    }

    #[test]
    fn startup_state_exposes_immediate_loading_reason() {
        assert_eq!(StartupState::Loading.reason(), "Loading kubeconfig…");
    }

    #[test]
    fn system_theme_leaves_native_appearance_untouched() {
        let selection = ThemeSelection::Dynamic {
            mode: ThemeAppearanceMode::System,
            light: ThemeName(PRODUCT_THEME_LIGHT.into()),
            dark: ThemeName(PRODUCT_THEME_DARK.into()),
        };
        assert_eq!(
            forced_window_appearance(&selection, Appearance::Light),
            None
        );
    }

    #[test]
    fn explicit_theme_forces_matching_native_appearance() {
        let selection = ThemeSelection::Static(ThemeName(PRODUCT_THEME_DARK.into()));
        assert_eq!(
            forced_window_appearance(&selection, Appearance::Dark),
            Some(WindowAppearance::Dark)
        );
    }

    #[test]
    fn theme_choices_share_one_persistent_shape() {
        assert_eq!(
            ThemeChoice::System.value(),
            serde_json::json!({
                "mode": "system",
                "light": PRODUCT_THEME_LIGHT,
                "dark": PRODUCT_THEME_DARK,
            })
        );
        let ayu_dark = ThemeSelection::Static(ThemeName("Ayu Dark".into()));
        assert_eq!(
            theme_selection(ThemeChoice::Named("Ayu Dark".into())),
            ayu_dark
        );
        assert_eq!(
            theme_choice_for_selection(&ayu_dark, Appearance::Dark),
            ThemeChoice::Named("Ayu Dark".into())
        );
        assert_eq!(
            theme_choice_for_selection(&system_theme_selection(), Appearance::Light),
            ThemeChoice::System
        );
    }
}
