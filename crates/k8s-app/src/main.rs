#![cfg_attr(windows, windows_subsystem = "windows")]
#![deny(unsafe_code)]

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crate::settings_window::{SettingsTarget, open_settings_window};
use gpui_kit::component::theme::{ThemeMode, ThemeRegistry};
use gpui_kit::component::{ActiveTheme, Root, WindowExt as _};
use gpui_kit::{
    App, AppContext as _, Bounds, Entity, Global, Pixels, PlatformDisplay, QuitMode,
    TitlebarOptions, Window, WindowAppearance, WindowBackgroundAppearance, WindowBounds,
    WindowDecorations, WindowOptions, actions, application, point, px, size,
};
use k8s_app::runtime as app_runtime;
use k8s_app::theme as product_theme;
use k8s_app::updater::{GlobalUpdater, UpdaterRuntime, UpdaterStatus, UpdaterStatusCallback};
use k8s_core::cluster::ClusterRegistry;
use k8s_ui::design;
use k8s_ui::panels::settings_view::OpenShortcutReference;
use k8s_ui::settings::{self as ui_settings, OpenSettings, SettingsStore, ThemeChoice};
use k8s_ui::shell::commands::UPDATER_UNAVAILABLE_REASON;
use k8s_ui::shell::{
    STARTUP_LOADING_REASON, Shell, ToggleTheme, UseDarkTheme, UseLightTheme, UseSystemTheme,
    UseTheme,
};
use k8s_ui::table_view::ClusterSession;
use k8s_ui::{UpdateActions, UpdatePhase, UpdateUiState};

mod diagnostics;

mod instance;
mod menus;
mod settings_window;
mod terminal;

// App-level actions.
actions!(
    k8s_app,
    [About, MinimizeWindow, ZoomWindow, Hide, CloseWindow, Quit]
);

/// How often a watched configuration file is checked for a change.
const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------
//
// gpui-kit owns the theme: it initializes itself, holds the registry, and changes
// appearance. The app's half of the split is the choice — System, Light, Dark, or
// a name from the registry — and `k8s_app::theme` applies it to both systems at
// once, so a window never reads an app role from one appearance and a component
// role from the other. `k8s_ui::design` keeps the product's own semantic roles;
// nothing here reads or writes a theme value.

/// The choice that produced the active theme, so a settings change knows whether
/// re-applying it would change anything.
struct ThemePreference(ThemeChoice);

impl Global for ThemePreference {}

/// The theme `settings.json` asks for, or `None` when the file says nothing the
/// app understands.
///
/// [`ThemeChoice::from_value`] owns the two spellings and the migration between
/// them, so this is the one place that decides what a `theme` key means. Reading it
/// here rather than matching the shape inline is what makes an explicit `Light` or
/// `Dark` survive a restart: the old format wrote both as the bare name of a
/// product theme, which came back as `Named` and let the registry decide the
/// appearance instead of the choice.
fn configured_theme(cx: &App) -> Option<ThemeChoice> {
    let text = cx
        .try_global::<SettingsStore>()?
        .raw_user_settings()?
        .to_owned();
    ui_settings::parse(&text)
        .theme
        .as_ref()
        .and_then(ThemeChoice::from_value)
}

/// The mode a choice resolves to. Only `System` reads the window, because it is
/// the only choice that defers to the desktop.
fn mode_for(cx: &App, window: Option<&Window>, choice: &ThemeChoice) -> ThemeMode {
    match choice {
        ThemeChoice::System => ThemeMode::from(
            window.map_or_else(|| cx.window_appearance(), |window| window.appearance()),
        ),
        ThemeChoice::Light => ThemeMode::Light,
        ThemeChoice::Dark => ThemeMode::Dark,
        ThemeChoice::Named(name) => ThemeRegistry::global(cx)
            .themes()
            .get(name.as_str())
            .map_or(ThemeMode::Light, |config| config.mode),
    }
}

/// Falls back to the system themes for a name nothing has registered, so a
/// settings file naming a theme this build does not have still starts.
fn normalize_theme_choice(cx: &App, choice: ThemeChoice) -> ThemeChoice {
    if let ThemeChoice::Named(name) = &choice
        && !ThemeRegistry::global(cx)
            .themes()
            .contains_key(name.as_str())
    {
        eprintln!("k8s-gpui: unknown configured theme. Using the system themes.");
        return ThemeChoice::System;
    }
    choice
}

/// Applies a theme choice to both theme systems.
fn apply_theme(cx: &mut App, window: Option<&mut Window>, choice: ThemeChoice) {
    let choice = normalize_theme_choice(cx, choice);
    cx.set_global(ThemePreference(choice.clone()));
    ui_settings::set_theme_choice(cx, choice.clone());
    match &choice {
        // A named theme carries its own appearance and its own role values, so it
        // is applied whole rather than switched between.
        ThemeChoice::Named(name) => {
            product_theme::select(cx, name);
        }
        // The other three differ only in which mode they land on.
        choice => {
            let mode = mode_for(cx, window.as_deref(), choice);
            product_theme::set_mode(cx, mode, window);
        }
    }
    sync_window_appearance(cx);
    cx.refresh_windows();
}

/// Applies a theme and records it in `settings.json`.
fn set_theme(cx: &mut App, choice: ThemeChoice) {
    // Apply the appearance first. A save error does not change this session.
    let persist = ui_settings::theme_choice(cx) != choice;
    apply_theme(cx, None, choice.clone());
    if persist
        && let Err(error) = ui_settings::update(cx, |settings| {
            settings.theme = Some(choice.value());
        })
    {
        eprintln!("k8s-gpui: failed to save theme: {error}");
    }
}

/// Keeps the window manager's light and dark in step with a forced theme.
///
/// A forced theme also has to claim the window, or the desktop keeps drawing its
/// chrome for the appearance the reader just turned off. `System` gives the
/// window back to the desktop.
fn sync_window_appearance(cx: &App) {
    let choice = cx
        .try_global::<ThemePreference>()
        .map(|preference| preference.0.clone())
        .unwrap_or(ThemeChoice::System);
    let forced = match choice {
        ThemeChoice::System => None,
        _ => Some(if cx.theme().is_dark() {
            WindowAppearance::Dark
        } else {
            WindowAppearance::Light
        }),
    };
    cx.set_window_appearance(forced);
}

/// Applies the configured theme at startup. Missing settings follow the system theme.
///
/// The window is needed because the system appearance is a property of the window,
/// so this runs inside the window rather than beside it.
fn install_theme(cx: &mut App, window: &mut Window) {
    let choice = configured_theme(cx).unwrap_or(ThemeChoice::System);
    eprintln!("k8s-gpui: theme = {choice:?}");
    apply_theme(cx, Some(window), choice);
}

fn sync_theme_from_settings(cx: &mut App) {
    ui_settings::sync_increase_contrast(cx);
    let choice = configured_theme(cx).unwrap_or(ThemeChoice::System);
    apply_theme(cx, None, choice);
}

fn install_theme_controller(cx: &mut App) {
    cx.observe_global::<SettingsStore>(sync_theme_from_settings)
        .detach();
}

/// Reloads the theme when the system appearance changes, while the theme follows
/// the system. A forced theme ignores it, or the desktop would undo the choice.
pub(crate) fn watch_system_appearance(window: &Window) {
    window
        .observe_window_appearance(|window, cx| {
            let follows_system = cx
                .try_global::<ThemePreference>()
                .is_none_or(|preference| preference.0 == ThemeChoice::System);
            if !follows_system {
                return;
            }
            product_theme::set_mode(cx, ThemeMode::from(window.appearance()), Some(window));
            cx.refresh_windows();
        })
        .detach();
}

/// Installs app-level handlers for theme actions.
fn install_theme_handlers(cx: &mut App) {
    cx.on_action(|_: &ToggleTheme, cx: &mut App| {
        let target = if cx.theme().is_dark() {
            ThemeChoice::Light
        } else {
            ThemeChoice::Dark
        };
        set_theme(cx, target);
    });
    cx.on_action(|_: &UseLightTheme, cx: &mut App| set_theme(cx, ThemeChoice::Light));
    cx.on_action(|_: &UseDarkTheme, cx: &mut App| set_theme(cx, ThemeChoice::Dark));
    cx.on_action(|_: &UseSystemTheme, cx: &mut App| set_theme(cx, ThemeChoice::System));
    cx.on_action(|action: &UseTheme, cx: &mut App| {
        if ThemeRegistry::global(cx)
            .themes()
            .contains_key(action.name.as_str())
        {
            set_theme(cx, ThemeChoice::named(action.name.clone()));
        }
    });
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Replaces the settings store's text, and reports whether it parsed.
///
/// The store's own `update` hands back nothing, and the caller has to know whether
/// the text it was handed is the text now in memory.
fn store_user_settings(cx: &mut App, text: &str) -> Result<(), String> {
    let mut outcome = Ok(());
    SettingsStore::update(cx, |store, cx| outcome = store.set_user_settings(text, cx));
    outcome
}

/// Loads user settings into memory. JSONC and the full settings schema are supported.
/// A parse error is logged and does not block startup.
fn load_user_settings(cx: &mut App) {
    let Some(path) = k8s_core::paths::config_file("settings.json") else {
        ui_settings::sync_increase_contrast(cx);
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        ui_settings::sync_increase_contrast(cx);
        return;
    };
    ui_settings::sync_increase_contrast(cx);
    match store_user_settings(cx, &text) {
        Ok(()) => eprintln!("k8s-gpui: settings.json loaded: {}", path.display()),
        Err(error) => {
            eprintln!("k8s-gpui: settings.json parse failed. Using built-in defaults: {error}")
        }
    }
}

fn install_reduce_motion(cx: &mut App) {
    let apply = |cx: &mut App| cx.set_reduce_motion(ui_settings::reduce_motion_enabled(cx));
    apply(cx);
    cx.observe_global::<SettingsStore>(apply).detach();
}

/// Applies each change to a configuration file, without a filesystem watcher.
///
/// The `fs` crate's recursive directory watcher is gone with the settings crate,
/// so the file is polled on the background executor and compared by content.
/// Comparing content rather than the modification time means a touched but
/// unchanged file is not a change, and a change is applied once however many
/// writes produced it.
fn watch_config_file(cx: &mut App, path: PathBuf, apply: impl Fn(&mut App, &str) + 'static) {
    let read = move || std::fs::read_to_string(&path).ok();
    let mut previous = read();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(CONFIG_POLL_INTERVAL).await;
            let current = read();
            if current == previous {
                continue;
            }
            // A file that cannot be read counts as absent rather than as empty, so
            // a delete never writes defaults over the settings it removed.
            let Some(contents) = current else {
                previous = None;
                continue;
            };
            previous = Some(contents.clone());
            cx.update(|cx| apply(cx, &contents));
        }
    })
    .detach();
}

/// Returns a config file to watch, creating the directory that holds it.
///
/// A file the app may write has to have somewhere to live before it is watched,
/// or the first save lands in a directory that was never created.
fn config_file_to_watch(path: Option<PathBuf>) -> Option<PathBuf> {
    let path = path?;
    let dir = path.parent()?.to_path_buf();
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "k8s-gpui: failed to create config directory {}: {error}",
            dir.display()
        );
        return None;
    }
    Some(path)
}

/// Watches the keymap file and applies each saved change. Watching the file also
/// detects a keymap that did not exist when the app started.
fn install_keymap_watch(cx: &mut App) {
    let Some(path) = config_file_to_watch(k8s_ui::keymap::user_keymap_path()) else {
        return;
    };
    watch_config_file(cx, path, |cx, contents| {
        if k8s_ui::keymap::reload_from_source(cx, contents) {
            eprintln!("k8s-gpui: keymap reloaded");
        }
    });
}

fn install_settings_watch(cx: &mut App) {
    let Some(path) = config_file_to_watch(k8s_core::paths::config_file("settings.json")) else {
        return;
    };
    watch_config_file(cx, path, |cx, contents| {
        if let Err(error) = store_user_settings(cx, contents) {
            eprintln!("k8s-gpui: settings.json reload failed: {error}. Using built-in defaults.");
            return;
        }
        ui_settings::sync_increase_contrast(cx);
        sync_theme_from_settings(cx);
        cx.refresh_windows();
    });
}

// ---------------------------------------------------------------------------
// Window
// ---------------------------------------------------------------------------

fn window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(initial_window_bounds(cx))),
        titlebar: Some(TitlebarOptions {
            title: Some("k8s-gpui".into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        app_id: Some("k8s-gpui".to_owned()),
        window_background: WindowBackgroundAppearance::Opaque,
        window_decorations: Some(WindowDecorations::Server),
        // The supported floor comes from the design token, so the window manager and the shell
        // layout cannot end up enforcing two different numbers. The design promises 960x640.
        window_min_size: Some(size(
            px(design::size::WINDOW_MIN.0),
            px(design::size::WINDOW_MIN.1),
        )),
        ..Default::default()
    }
}

/// The box the first window opens with: what this display was left at, or the default.
///
/// The design asks for window geometry remembered per display, and a single global box
/// cannot be that — a window sized for the 4K the reader unplugged last night is a window
/// that covers a third of the laptop they are looking at now. So the lookup is by display,
/// and the remembered box is fitted to the display that is actually there before it is
/// used: an unplugged monitor's coordinates are not a smaller window, they are a window
/// nothing on screen contains.
fn initial_window_bounds(cx: &App) -> Bounds<Pixels> {
    let (layout, complaint) = ui_settings::layout::load();
    if let Some(complaint) = complaint {
        eprintln!("k8s-gpui: {complaint}");
    }
    let fallback = Bounds::new(point(px(24.0), px(24.0)), size(px(1280.0), px(720.0)));
    let Some(display) = cx.primary_display() else {
        return fallback;
    };
    let Some(remembered) = layout.displays.get(&display_key(&display)) else {
        return fallback;
    };
    let visible = display.visible_bounds();
    let origin = (f32::from(visible.origin.x), f32::from(visible.origin.y));
    let extent = (
        f32::from(visible.size.width),
        f32::from(visible.size.height),
    );
    match ui_settings::layout::fitted_geometry(*remembered, origin, extent) {
        Some((x, y, width, height)) => {
            Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
        }
        None => fallback,
    }
}

/// The key a display's remembered box is stored under.
///
/// The UUID is what survives a reboot; the runtime id is the only handle some platforms
/// offer, and a key that changed on every launch would make remembering anything by
/// display impossible rather than merely imprecise.
fn display_key(display: &Rc<dyn PlatformDisplay>) -> String {
    let uuid = display.uuid().ok().map(|uuid| uuid.to_string());
    ui_settings::layout::display_key(uuid.as_deref(), u64::from(display.id()))
}

fn show_about(cx: &mut App) {
    let Some(window) = cx.active_window() else {
        return;
    };
    let _ = window.update(cx, |_, window, cx| {
        window.open_alert_dialog(cx, |alert, _, _| {
            alert.title("K8s GPUI").description(format!(
                "Version {}. Manage Kubernetes clusters and workloads.",
                env!("CARGO_PKG_VERSION")
            ))
        });
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

    // The design wants Settings in its own window, so both the menu item and
    // `secondary-,` land in `settings_window` rather than in a centre tab. The
    // window remembers itself, so a second press focuses the one already open.
    cx.on_action(|_: &OpenSettings, cx: &mut App| {
        report_open_failure(open_settings_window(cx, SettingsTarget::Settings));
    });
    // `?` from anywhere. It is a reference, not a request, so it carries no
    // payload, and it is bound in every key context that is not a text surface.
    cx.on_action(|_: &OpenShortcutReference, cx: &mut App| {
        report_open_failure(open_settings_window(cx, SettingsTarget::Shortcuts));
    });
}

/// A window that will not open is a bug the reader cannot see, so it says so on
/// stderr rather than doing nothing and looking like a key that does nothing.
fn report_open_failure(outcome: Result<gpui_kit::AnyWindowHandle, String>) {
    if let Err(error) = outcome {
        eprintln!("k8s-gpui: {error}");
    }
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

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
    let handle = app_runtime::handle(cx);
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
            let handle = app_runtime::handle(cx);
            handle.spawn(async move {
                let _ = runtime.check().await;
            });
        },
        move |cx| {
            let runtime = retry_runtime.clone();
            let handle = app_runtime::handle(cx);
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
fn quit_mode() -> QuitMode {
    if cfg!(target_os = "linux") {
        QuitMode::Explicit
    } else {
        QuitMode::Default
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
    // The whole Lucide catalog, not gpui-kit's default component bundle. The app's design tokens
    // name icons from across the catalog, and any name outside the bundle renders as a missing
    // asset at runtime rather than as a compile error.
    let app = application()
        // The product's own kind set, then the whole shared catalog behind it.
        // The product's twelve win on a name collision, and a missing one falls
        // through to a Lucide glyph rather than to nothing.
        .with_assets(k8s_app::assets::ProductAssets)
        .with_quit_mode(quit_mode());

    app.run(|cx: &mut App| {
        app_runtime::install(cx);
        // gpui-kit first: it installs the component theme, the window extensions, and the
        // component root that dialogs, sheets, notifications, and tooltips need somewhere to
        // render. Nothing component-backed may be built before it.
        gpui_kit::init(cx);
        install_quit_observer(cx);
        // The bundled typeface, before the theme names it and before any window
        // can ask for a family. A font requested before it is registered falls
        // back to the platform's silently, and the symptom would surface much
        // later as table columns that measure differently on each OS.
        k8s_app::fonts::install(cx);
        // The app's own settings store. It holds the raw settings text every reader
        // parses, so it is created before anything reads it and before the window
        // opens.
        ui_settings::init(cx);
        // The product theme on top of gpui-kit's own defaults, so every component wears the
        // product palette and the app's semantic roles come from the same file.
        product_theme::install(cx);
        // Load settings before the window opens, so the theme installer can read the choice.
        load_user_settings(cx);
        install_reduce_motion(cx);
        install_theme_controller(cx);
        install_settings_watch(cx);
        install_theme_handlers(cx);
        install_menu_handlers(cx);

        // Install the keymap first. It clears existing bindings. Diagnostics adds F1 after this.
        if let Err(error) = k8s_ui::keymap::install(cx) {
            eprintln!("k8s-gpui: keymap load failed. The valid part was loaded:\n{error:#}");
        }
        install_keymap_watch(cx);
        menus::install(cx);
        cx.observe_global::<k8s_ui::keymap::KeymapStatus>(menus::refresh)
            .detach();
        diagnostics::init(cx);

        eprintln!(
            "k8s-gpui: active theme = {:?}, background = {}, panel = {}",
            cx.theme().theme_name(),
            design::colors(cx).background,
            design::colors(cx).panel_background
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
        // The component root is the window's view, so the shell entity is handed out through a
        // slot the tasks below read once the window exists.
        let shell_slot: Rc<RefCell<Option<Entity<Shell>>>> = Rc::new(RefCell::new(None));
        let opened_slot = shell_slot.clone();

        match cx.open_window(window_options(cx), move |window, cx| {
            install_theme(cx, window);
            watch_system_appearance(window);
            let shell = cx.new(|cx| {
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
            });
            *opened_slot.borrow_mut() = Some(shell.clone());
            // gpui-kit's root owns the window's overlay layers. The border is off because this
            // window asks the window manager for its decorations: a client-side frame and its
            // shadow would be a second frame around a window that already has one.
            //
            // gpui-kit 0.7.0 removes `bordered` and draws the frame from a root plugin with no
            // way to opt out, so the upgrade waits on that: the two versions are otherwise
            // compatible, and this line is the whole of the difference.
            cx.new(|cx| Root::new(shell, window, cx).bordered(false))
        }) {
            Ok(_window) => {
                let Some(shell) = shell_slot.borrow_mut().take() else {
                    eprintln!("k8s-gpui: failed to resolve the Shell entity");
                    cx.quit();
                    return;
                };
                let registry_load = spawn_registry_load(&app_runtime::handle(cx));
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

    /// Closing the last window is the one exit `QuitMode::Explicit` still has to allow,
    /// and every other window is no reason at all.
    ///
    /// The count is read after the closed window is gone, so zero means this one was the
    /// last. Without the zero the app cannot be closed at all on Linux, and with any
    /// other rule a Settings window closing takes the whole session with it.
    #[test]
    fn only_the_last_window_closing_is_a_reason_to_quit() {
        assert!(should_quit_after_window_close(0));
        assert!(!should_quit_after_window_close(1));
        assert!(!should_quit_after_window_close(7));
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
    fn linux_is_the_only_platform_that_asks_before_quitting() {
        let explicit = if cfg!(target_os = "linux") {
            QuitMode::Explicit
        } else {
            QuitMode::Default
        };
        assert_eq!(quit_mode(), explicit);
    }
}
