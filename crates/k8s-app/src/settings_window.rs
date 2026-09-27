//! The Settings window.
//!
//! The design lists "Settings opens its own window" among the eight things a
//! person notices without being told, and §15.2 makes it the first of the four
//! rules this surface is built around. So the settings surface is not a centre tab
//! here: it is a window, hosted the way the main window is hosted, with gpui-kit's
//! `Root` between it and the desktop so it still gets the overlay layers, the
//! tooltip layer and the native menu fallback every window needs.
//!
//! Two decisions are worth naming, because the rest of this file follows from them.
//!
//! **There is one settings store, and it is not here.** The values a reader changes
//! live in `k8s_ui::settings`, which is a set of `App` globals — `SettingsStore`,
//! `DiskCache`, `SettingsSaveStatus`, `ThemeChoice`. A `Global` belongs to the `App`,
//! not to a window, so a second window reads and writes the same copy the first one
//! does and the file is written once. This module adds no store, no cache and no
//! channel: it constructs a `SettingsView` and hands the window the handful of
//! session facts the shell owns, through `SettingsEnvironment`.
//!
//! **One window, not one per press.** A menu item and a keyboard shortcut both land
//! here, and a second Settings window would be a second place to change a setting
//! from. The open window is remembered in a `Global`; asking again focuses it and
//! routes the request to the view already in it.

use std::rc::Rc;

use gpui_kit::component::notification::{Notification, NotificationType};
use gpui_kit::component::theme::ThemeRegistry;
use gpui_kit::component::{Root, WindowExt as _};
use gpui_kit::{
    Action, AnyWindowHandle, App, AppContext as _, Bounds, Entity, Global, TitlebarOptions,
    WeakEntity, Window, WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowOptions,
    point, px, size,
};
use k8s_ui::design::Severity;
use k8s_ui::keymap;
use k8s_ui::panels::settings_view::{SETTINGS_WINDOW_MIN, SETTINGS_WINDOW_SIZE, SettingsView};
use k8s_ui::settings::ThemeChoice;
use k8s_ui::shell::{UseDarkTheme, UseLightTheme, UseSystemTheme, UseTheme};

/// The window's own title, before the surface names the pane inside it.
const WINDOW_TITLE: &str = "Settings";

/// How a window tells the reader that something happened.
///
/// The same shape the surface's notice handler takes, named once here because the
/// window builds the channel and then hands it to two of the surface's handlers.
type Notice = Rc<dyn Fn(String, Severity, &mut App)>;

/// What the Settings window should be showing when it opens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SettingsTarget {
    /// The page the reader left on, which the surface remembers itself.
    #[default]
    Settings,
    /// The shortcut reference, the destination `?` names from anywhere.
    Shortcuts,
}

/// The open Settings window, and the view inside it.
///
/// A `WeakEntity` rather than an `Entity` because the window's `Root` owns the view:
/// holding it strongly here would keep a closed window's view, its keymap poll and
/// its global observers alive until the next press. A weak handle and a live window
/// handle answer the same question — "is it still there?" — without either of them
/// keeping the answer true.
#[derive(Clone, Default)]
struct SettingsWindow {
    handle: Option<AnyWindowHandle>,
    view: Option<WeakEntity<SettingsView>>,
}

impl Global for SettingsWindow {}

/// Opens the Settings window, or routes the request to the one already open.
///
/// Returns the window that answered, so a caller can name the one that failed. The
/// shortcut reference is opened rather than toggled: `?` promises that the reference
/// is reachable from anywhere, and a promise that closes the destination when the
/// destination is already showing is not that promise.
pub fn open_settings_window(
    cx: &mut App,
    target: SettingsTarget,
) -> Result<AnyWindowHandle, String> {
    if let Some(handle) = live_window(cx) {
        route(cx, handle, target)?;
        return Ok(handle);
    }
    let view = build_settings_view(cx);
    let weak = view.downgrade();
    let handle = cx
        .open_window(settings_window_options(), move |window, cx| {
            crate::watch_system_appearance(window);
            cx.new(|cx| Root::new(view, window, cx).bordered(false))
        })
        .map_err(|error| format!("The Settings window could not be opened: {error}"))?;
    cx.set_global(SettingsWindow {
        handle: Some(handle.into()),
        view: Some(weak),
    });
    route(cx, handle.into(), target)?;
    Ok(handle.into())
}

/// The Settings window, if one is still open.
///
/// A closed window's handle answers every `update` with an error, and that is the
/// only way to ask the question: nothing in the API is told when a window goes away.
fn live_window(cx: &mut App) -> Option<AnyWindowHandle> {
    let state = cx.try_global::<SettingsWindow>()?.clone();
    let handle = state.handle?;
    let live = handle.update(cx, |_, _, _| ()).is_ok()
        && state
            .view
            .as_ref()
            .is_some_and(|view| view.update(cx, |_, _| ()).is_ok());
    if !live {
        cx.set_global(SettingsWindow::default());
        return None;
    }
    Some(handle)
}

/// Sends a request to the window that is already open.
fn route(cx: &mut App, handle: AnyWindowHandle, target: SettingsTarget) -> Result<(), String> {
    let Some(view) = cx
        .try_global::<SettingsWindow>()
        .and_then(|state| state.view.clone())
    else {
        return Err("The Settings window is no longer available.".to_owned());
    };
    handle
        .update(cx, |_, window, cx| {
            // A weak handle that has expired means the window went away between
            // the liveness check and this update, which is a race no amount of
            // checking removes. There is nothing to route to and nothing to
            // report: the caller asked for a window that no longer exists.
            let Some(view) = view.upgrade() else {
                return;
            };
            window.activate_window();
            if target == SettingsTarget::Shortcuts && !view.read(cx).shortcut_reference_open() {
                view.update(cx, |view, cx| view.toggle_shortcut_reference(window, cx));
            }
            // Focus lands in the search box either way. It is the first control on
            // the surface and the reason a list of sixty-eight commands is usable
            // at all, and a window that opens with nothing focused makes a person
            // reach for the mouse before they can type.
            let focus = view.read(cx).focus_handle();
            window.focus(&focus, cx);
        })
        .map_err(|error| format!("The Settings window could not be reached: {error}"))
}

/// Builds the view and wires the session facts and the host's own commands into it.
///
/// Everything the surface cannot answer for itself arrives here: the theme command,
/// the keymap file, the notice channel, the way out, and the session facts the shell
/// publishes. The view reads the facts from `SettingsEnvironment` itself, so a
/// window opened before the shell has probed anything is already correct the moment
/// the first probe reports.
fn build_settings_view(cx: &mut App) -> Entity<SettingsView> {
    let view = cx.new(SettingsView::new);
    let notice = notice_channel();
    view.update(cx, |view, _| {
        view.set_owns_window();
        view.set_theme_handler(apply_theme_choice);
        view.set_notice_handler({
            let notice = notice.clone();
            move |message, severity, cx| notice(message, severity, cx)
        });
        view.set_close_handler(|window, _| window.remove_window());
        view.set_keymap_handlers(
            {
                let notice = notice.clone();
                move |_window, cx| open_user_keymap(&notice, cx)
            },
            |_window, cx| {
                if let Err(error) = keymap::reload(cx) {
                    eprintln!("k8s-gpui: keymap reload failed: {error}");
                }
            },
        );
    });
    view
}

/// The one place this window raises feedback.
///
/// A settings window has no shell behind it, so the shell's toast is out of reach and
/// the surface's own error banner is the wrong home for a confirmation. gpui-kit's
/// `Root` owns the notification list for a window, so a notification raised here lands
/// in the window the reader is looking at: the active window is the one with focus,
/// which is the one the press came from.
fn notice_channel() -> Notice {
    Rc::new(|message, severity, cx| {
        let Some(window) = cx.active_window() else {
            return;
        };
        // Only a warning and a failure are coloured. The design rule is that a
        // healthy outcome is neutral grey and only an exception earns colour, so a
        // confirmation rides the same neutral treatment as `info` rather than
        // reaching for `success`.
        let kind = match severity {
            Severity::Warning => NotificationType::Warning,
            Severity::Error => NotificationType::Error,
            Severity::Success | Severity::Info | Severity::Neutral | Severity::Muted => {
                NotificationType::Info
            }
        };
        let _ = window.update(cx, |_, window, cx| {
            window.push_notification(Notification::new().message(message).with_type(kind), cx);
        });
    })
}

/// Applies a theme choice through the app's own theme command.
///
/// The app installs one `on_action` handler per theme action, and that handler both
/// applies the choice and writes it to `settings.json`. Dispatching the action rather
/// than reimplementing it is the whole reason this is a bridge and not a second theme
/// path: there is one write, and it is the one every other surface already uses. The
/// one check kept here is the one the caller needs an answer for, because the app's
/// handler declines a name no installed theme carries silently.
fn apply_theme_choice(
    choice: ThemeChoice,
    window: &mut Window,
    cx: &mut App,
) -> Result<(), String> {
    let action: Box<dyn Action> = match choice {
        ThemeChoice::Light => Box::new(UseLightTheme),
        ThemeChoice::Dark => Box::new(UseDarkTheme),
        ThemeChoice::System => Box::new(UseSystemTheme),
        ThemeChoice::Named(name) => {
            if !ThemeRegistry::global(cx)
                .themes()
                .contains_key(name.as_str())
            {
                return Err(format!(
                    "Theme not found: {name}. Choose an installed theme."
                ));
            }
            Box::new(UseTheme { name })
        }
    };
    // A focused element answers first, because a theme action dispatched to a window
    // with nothing focused would skip the surface that asked for the change.
    if let Some(focus) = window.focused(cx) {
        focus.dispatch_action(action.as_ref(), window, cx);
    } else {
        window.dispatch_action(action, cx);
    }
    Ok(())
}

/// Creates the user keymap file if it is missing, opens it, and says which it did.
fn open_user_keymap(notice: &Notice, cx: &mut App) {
    let Some(path) = keymap::user_keymap_path() else {
        notice(
            "The configuration directory was not found. Check the user configuration, then try again."
                .to_owned(),
            Severity::Error,
            cx,
        );
        return;
    };
    let created = match keymap::ensure_user_keymap_file(&path) {
        Ok(created) => created,
        Err(error) => {
            eprintln!("k8s-gpui: keymap file was not created: {error}");
            notice(
                "The app did not create the keymap file. Check the configuration directory, then try again."
                    .to_owned(),
                Severity::Error,
                cx,
            );
            return;
        }
    };
    if let Err(error) = keymap::open_user_keymap_file(&path) {
        eprintln!("k8s-gpui: keymap file was not opened: {error}");
        notice(
            "The system did not open the keymap file with the default program. Check the file association, then try again."
                .to_owned(),
            Severity::Error,
            cx,
        );
        return;
    }
    let message = if created {
        format!("Keymap file created and opened: {}", path.display())
    } else {
        format!("Keymap file opened: {}", path.display())
    };
    notice(message, Severity::Success, cx);
}

/// The window's geometry and chrome.
///
/// Server-side decorations, like the main window: a client-side frame drawn by
/// gpui-kit's `Root` would be a second frame around a window the window manager has
/// already framed. The minimum is the settings surface's own row floor rather than
/// the shell's, because this window has no sidebar or inspector to give width away.
fn settings_window_options() -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(48.0), px(48.0)),
            size(px(SETTINGS_WINDOW_SIZE.0), px(SETTINGS_WINDOW_SIZE.1)),
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some(WINDOW_TITLE.into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        app_id: Some("k8s-gpui".to_owned()),
        window_background: WindowBackgroundAppearance::Opaque,
        window_decorations: Some(WindowDecorations::Server),
        window_min_size: Some(size(px(SETTINGS_WINDOW_MIN.0), px(SETTINGS_WINDOW_MIN.1))),
        ..Default::default()
    }
}
